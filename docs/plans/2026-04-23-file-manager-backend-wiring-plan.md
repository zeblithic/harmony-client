# File Manager Backend Wiring Implementation Plan (ZEB-146)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the no-op File Manager backend stubs in harmony-client (`list_content`, `pin_content`, `unpin_content`, `burn_content`, `archive_content`) with real implementations backed by a client-side metadata sidecar and the runtime's cache, plus a new `set_replication_tier` command.

**Architecture:** Two-phase delivery. Phase A lands a tiny `harmony` PR exposing `NodeRuntime::storage_tier[_mut]()` and `StorageTier::cache[_mut]()` accessors — load-bearing for Phase B. Phase B lands the harmony-client work: new `content_index.rs` sidecar persisted as JSON under `app_data_dir`, a unified `ContentVerbRequest` enum channel through the event loop for runtime-touching verbs (pin/unpin/burn/pinned-set snapshot), and sidecar-only handling for archive/replication-tier mutations.

**Tech Stack:** Rust (Tauri v2 commands, tokio mpsc/oneshot), serde JSON for sidecar persistence, Svelte 5 + TypeScript for the frontend service layer.

**Spec:** `docs/specs/2026-04-23-file-manager-backend-wiring-design.md`

---

## Phase A — harmony upstream PR

Land this first. Harmony-client side (Phase B) cannot compile until this merges.

### Task A1: Expose `NodeRuntime::storage_tier()` + `StorageTier::cache()` accessors

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony/crates/harmony-content/src/storage_tier.rs` — add `pub fn cache(&self)` + `pub fn cache_mut(&mut self)`.
- Modify: `/Users/zeblith/work/zeblithic/harmony/crates/harmony-runtime/src/runtime.rs` — add `pub fn storage_tier(&self)` + `pub fn storage_tier_mut(&mut self)`.
- Test: inline `#[cfg(test)]` modules in each file.

- [ ] **Step 1: Create branch from latest origin/main in harmony repo**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git fetch origin
git checkout -b feat/expose-storage-tier-accessors-zeb-146 origin/main
```

Expected: `Switched to a new branch 'feat/expose-storage-tier-accessors-zeb-146'` with no divergence from origin/main.

- [ ] **Step 2: Write failing test for `StorageTier::cache()` exposure**

Append to `crates/harmony-content/src/storage_tier.rs`, inside the existing `#[cfg(test)] mod tests { ... }` block:

```rust
#[test]
fn cache_accessor_exposes_content_store() {
    use crate::book::MemoryBookStore;

    let (mut tier, _actions) = StorageTier::new(
        MemoryBookStore::new(),
        StorageBudget { cache_capacity: 100, max_pinned_bytes: 1000 },
        ContentPolicy::default(),
        FilterBroadcastConfig {
            mutation_threshold: 10,
            max_interval_ticks: 40,
            expected_items: 32,
            fp_rate: 0.01,
        },
    );

    // Pin a CID via cache_mut, read it back via cache.
    let cid = ContentId::from_bytes([0x42; 32]);
    assert!(tier.cache_mut().pin(cid));
    assert!(tier.cache().is_pinned(&cid));
}
```

- [ ] **Step 3: Run test, verify it fails**

```bash
cargo test -p harmony-content storage_tier::tests::cache_accessor_exposes_content_store
```

Expected: compile error — `cache` / `cache_mut` are private fields with no public accessor.

- [ ] **Step 4: Add `cache()` + `cache_mut()` to `StorageTier`**

In `crates/harmony-content/src/storage_tier.rs`, add these methods to the **first** `impl<B: BookStore> StorageTier<B>` block (near the existing `metrics()`, `policy()`, `flatpack()` accessors around line 358):

```rust
    /// Read-only access to the W-TinyLFU cache. Exposed so higher layers
    /// (e.g. clients that own the event loop) can inspect admission and
    /// pin state without duplicating the pin/unpin logic.
    pub fn cache(&self) -> &ContentStore<B> {
        &self.cache
    }

    /// Mutable access to the W-TinyLFU cache, for pin/unpin mutations
    /// driven by user-facing content actions.
    pub fn cache_mut(&mut self) -> &mut ContentStore<B> {
        &mut self.cache
    }
```

- [ ] **Step 5: Run test, verify it passes**

```bash
cargo test -p harmony-content storage_tier::tests::cache_accessor_exposes_content_store
```

Expected: `test result: ok. 1 passed`.

- [ ] **Step 6: Write failing test for `NodeRuntime::storage_tier()` exposure**

Find the `#[cfg(test)] mod tests` block in `crates/harmony-runtime/src/runtime.rs` (or create one at the bottom of the file if absent). Add:

```rust
#[test]
fn storage_tier_accessor_exposes_cache_pin_state() {
    use harmony_content::book::MemoryBookStore;
    use harmony_content::cid::ContentId;

    let config = test_node_config(); // existing helper; if absent, build inline via NodeConfig defaults
    let (mut runtime, _actions) = NodeRuntime::new(config, MemoryBookStore::new());

    let cid = ContentId::from_bytes([0x7A; 32]);
    assert!(runtime.storage_tier_mut().cache_mut().pin(cid));
    assert!(runtime.storage_tier().cache().is_pinned(&cid));
}
```

If `test_node_config()` doesn't already exist in the test module, substitute with a minimal inline NodeConfig construction matching the patterns seen in other tests in that file (grep `NodeConfig {` inside the test module to copy an existing shape).

- [ ] **Step 7: Run test, verify it fails**

```bash
cargo test -p harmony-runtime runtime::tests::storage_tier_accessor_exposes_cache_pin_state
```

Expected: compile error — `storage_tier` / `storage_tier_mut` missing from `NodeRuntime`.

- [ ] **Step 8: Add `storage_tier()` + `storage_tier_mut()` to `NodeRuntime`**

In `crates/harmony-runtime/src/runtime.rs`, add to the `impl<B: BookStore> NodeRuntime<B>` block near the existing `metrics()` method (around line 1189):

```rust
    /// Read-only access to the storage tier. Exposed so the event loop
    /// owning this runtime can snapshot cache state for user-facing
    /// content listings without duplicating StorageTier's internals.
    pub fn storage_tier(&self) -> &StorageTier<B> {
        &self.storage
    }

    /// Mutable access to the storage tier, for user-driven pin / unpin
    /// mutations routed from Tauri commands through the event loop.
    pub fn storage_tier_mut(&mut self) -> &mut StorageTier<B> {
        &mut self.storage
    }
```

- [ ] **Step 9: Run test, verify it passes**

```bash
cargo test -p harmony-runtime runtime::tests::storage_tier_accessor_exposes_cache_pin_state
```

Expected: `test result: ok. 1 passed`.

- [ ] **Step 10: Run full harmony-content and harmony-runtime test suites for regression protection**

```bash
cargo test -p harmony-content -p harmony-runtime
```

Expected: all tests pass. If any failure is clearly unrelated to the accessor additions, file a Linear follow-up ticket (per memory `feedback_unrelated_test_failures.md`) rather than fixing in this PR.

- [ ] **Step 11: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git add crates/harmony-content/src/storage_tier.rs crates/harmony-runtime/src/runtime.rs
git commit -m "$(cat <<'EOF'
feat(runtime): expose storage_tier and cache accessors (ZEB-146)

Wires storage_tier() / storage_tier_mut() on NodeRuntime and cache() /
cache_mut() on StorageTier so the harmony-client event loop can snapshot
cache pin state and route pin/unpin mutations without duplicating
StorageTier internals.

Tiny prerequisite for ZEB-146 (harmony-client File Manager backend
wiring); no behavior change here.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 12: Push and open PR**

```bash
git push -u origin feat/expose-storage-tier-accessors-zeb-146
gh pr create --title "feat(runtime): expose storage_tier and cache accessors (ZEB-146)" --body "$(cat <<'EOF'
## Summary
- Add `pub fn storage_tier(&self)` + `storage_tier_mut()` to `NodeRuntime<B>`.
- Add `pub fn cache(&self)` + `cache_mut()` to `StorageTier<B>`.
- Add two unit tests exercising the accessor path end-to-end.

Prerequisite for ZEB-146 (harmony-client File Manager backend wiring). No
behavior change.

## Test Plan
- [x] `cargo test -p harmony-content`
- [x] `cargo test -p harmony-runtime`
- [ ] Reviewer approval

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

**CHECKPOINT — Phase A complete.** Wait for the PR to merge. Only proceed to Phase B once the harmony PR is in main and you have the merge commit SHA handy.

---

## Phase B — harmony-client PR

Prerequisite: Phase A merged to harmony `main`. Use the merged commit SHA when bumping the dep in Task B1.

Phase B runs on the already-created branch `feat/file-manager-backend-zeb-146` in harmony-client.

### Task B1: Bump harmony dependency to the Phase A merge commit

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/Cargo.toml` — `harmony-runtime` / `harmony-content` git rev bump.
- Modify: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/Cargo.lock` — via `cargo update`.

- [ ] **Step 1: Confirm branch and clean state**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status
git branch --show-current
```

Expected: on `feat/file-manager-backend-zeb-146`, clean working tree (the spec commit + clarification commit present; nothing else unstaged).

- [ ] **Step 2: Update the harmony cargo workspace pin**

The harmony git dep is typically a workspace-level `[workspace.dependencies]` entry in `src-tauri/Cargo.toml` or individual dependency entries with `git = "..." branch = "main"`. Run:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo update -p harmony-runtime -p harmony-content
```

Expected: Cargo.lock advances `harmony-runtime` / `harmony-content` to the Phase A merge commit on main.

- [ ] **Step 3: Verify accessors are available**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | head -40
```

Expected: no compile errors. If `cargo check` reports missing `storage_tier` / `storage_tier_mut`, Cargo is still pinned to a pre-Phase-A commit — double-check the lockfile diff.

- [ ] **Step 4: Commit the lockfile bump**

```bash
git add src-tauri/Cargo.lock
git commit -m "chore(deps): bump harmony to include storage_tier accessors (ZEB-146)"
```

---

### Task B2: Create `content_index.rs` — types and load/save roundtrip

**Files:**
- Create: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/content_index.rs`
- Test: inline `#[cfg(test)]` module within the new file.
- Modify: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/lib.rs` — add `mod content_index;` declaration.

- [ ] **Step 1: Write the failing roundtrip test FIRST**

Create `src-tauri/src/content_index.rs` with only the skeleton needed to make the test fail on "not implemented":

```rust
//! Client-side sidecar for self-ingested content metadata (ZEB-146).
//!
//! Persists a map of `cid -> ContentIndexEntry` as JSON under
//! `app_data_dir/content-index.json` so the File Manager UI can surface
//! filenames, ingest timestamps, and user-set flags (sensitivity,
//! replication tier, licensed, archived) for content that the runtime's
//! RAM-only cache doesn't know about.
//!
//! Authority split:
//! - Sidecar is authoritative for membership and size_bytes (CIDs are
//!   immutable, so size never drifts from the ingest-time value).
//! - Runtime cache is authoritative for pinned state (pin is an eviction
//!   concept the cache owns).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const INDEX_FILE: &str = "content-index.json";
const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    Private,
    Confidential,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplicationTier {
    Minimal,
    Default,
    Durable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentIndexEntry {
    #[serde(with = "hex_cid")]
    pub cid: [u8; 32],
    pub file_name: String,
    pub size_bytes: u64,
    pub stored_at_ms: u64,
    pub sensitivity: Sensitivity,
    pub replication_tier: ReplicationTier,
    pub licensed: bool,
    pub archived: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: Vec<ContentIndexEntry>,
}

pub struct ContentIndex {
    path: PathBuf,
    entries: HashMap<[u8; 32], ContentIndexEntry>,
}

impl ContentIndex {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(INDEX_FILE);
        let entries = Self::read_file(&path).unwrap_or_default();
        ContentIndex { path, entries }
    }

    fn read_file(path: &Path) -> Option<HashMap<[u8; 32], ContentIndexEntry>> {
        let data = std::fs::read(path).ok()?;
        let file: IndexFile = serde_json::from_slice(&data).ok()?;
        if file.version != FILE_VERSION {
            return None;
        }
        let mut map = HashMap::with_capacity(file.entries.len());
        for entry in file.entries {
            if map.insert(entry.cid, entry).is_some() {
                tracing::warn!("duplicate CID in content-index.json; last-write-wins");
            }
        }
        Some(map)
    }

    fn save(&self) {
        let file = IndexFile {
            version: FILE_VERSION,
            entries: self.entries.values().cloned().collect(),
        };
        let json = match serde_json::to_vec_pretty(&file) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(err = %e, "content-index serialize failed");
                return;
            }
        };

        let tmp_path = {
            let mut name = self.path.file_name().unwrap_or_default().to_os_string();
            name.push(".tmp");
            self.path.with_file_name(name)
        };
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&tmp_path, &json).is_ok() {
            let _ = std::fs::rename(&tmp_path, &self.path);
        }
    }
}

mod hex_cid {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(cid: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(cid))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
            serde::de::Error::custom(format!("expected 32-byte hex CID, got {}", s.len() / 2))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_entry(cid: [u8; 32]) -> ContentIndexEntry {
        ContentIndexEntry {
            cid,
            file_name: "hello.txt".into(),
            size_bytes: 42,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
        }
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips_entries() {
        let dir = tempdir().unwrap();
        let entry = sample_entry([0xAA; 32]);

        let mut idx = ContentIndex::load(dir.path());
        idx.entries.insert(entry.cid, entry.clone());
        idx.save();

        let reloaded = ContentIndex::load(dir.path());
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries.get(&entry.cid), Some(&entry));
    }

    #[test]
    fn load_malformed_json_returns_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(INDEX_FILE), b"{ not valid json").unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn load_wrong_version_returns_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join(INDEX_FILE),
            br#"{"version": 99, "entries": []}"#,
        )
        .unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(idx.entries.is_empty());
    }
}
```

- [ ] **Step 2: Register the module in lib.rs**

In `src-tauri/src/lib.rs`, near the other `mod` declarations at the top of the file, add:

```rust
mod content_index;
```

Alphabetical order matches convention; place between `mod mail;` and `mod event_loop;` or wherever fits the existing grouping.

- [ ] **Step 3: Run the tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo test content_index::tests
```

Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/content_index.rs src-tauri/src/lib.rs
git commit -m "feat(content-index): sidecar types and persistence (ZEB-146)"
```

---

### Task B3: Add mutations to `ContentIndex` — insert, remove, set_archived, set_replication_tier

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/content_index.rs`

- [ ] **Step 1: Write failing mutation tests**

Append to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn insert_adds_entry_and_returns_true() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xBB; 32]);
        assert!(idx.insert(entry.clone()));
        assert_eq!(idx.get(&entry.cid), Some(&entry));
    }

    #[test]
    fn insert_duplicate_cid_returns_false() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xCC; 32]);
        assert!(idx.insert(entry.clone()));
        assert!(!idx.insert(entry));
    }

    #[test]
    fn remove_returns_true_when_present_false_otherwise() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xDD; 32]);
        idx.insert(entry.clone());
        assert!(idx.remove(&entry.cid));
        assert!(!idx.remove(&entry.cid));
    }

    #[test]
    fn set_archived_flips_flag_and_reports_change() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xEE; 32]);
        idx.insert(entry.clone());

        assert!(idx.set_archived(&entry.cid, true));  // flipped
        assert!(idx.get(&entry.cid).unwrap().archived);
        assert!(!idx.set_archived(&entry.cid, true)); // idempotent
    }

    #[test]
    fn set_archived_missing_cid_returns_false() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        assert!(!idx.set_archived(&[0xFF; 32], true));
    }

    #[test]
    fn set_replication_tier_counts_updated_entries() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let a = sample_entry([0x01; 32]);
        let b = sample_entry([0x02; 32]);
        idx.insert(a.clone());
        idx.insert(b.clone());

        // Both are Default; bumping to Durable should update 2.
        let updated = idx.set_replication_tier(&[a.cid, b.cid], ReplicationTier::Durable);
        assert_eq!(updated, 2);

        // Same call again: tier already Durable, so 0 updated.
        let again = idx.set_replication_tier(&[a.cid, b.cid], ReplicationTier::Durable);
        assert_eq!(again, 0);

        // Missing CID is skipped, not an error.
        let with_missing =
            idx.set_replication_tier(&[a.cid, [0xAA; 32]], ReplicationTier::Minimal);
        assert_eq!(with_missing, 1);
    }

    #[test]
    fn save_persists_mutations() {
        let dir = tempdir().unwrap();
        {
            let mut idx = ContentIndex::load(dir.path());
            idx.insert(sample_entry([0xA1; 32]));
            idx.insert(sample_entry([0xA2; 32]));
            idx.remove(&[0xA1; 32]);
            idx.save();
        }
        let reloaded = ContentIndex::load(dir.path());
        assert_eq!(reloaded.entries.len(), 1);
        assert!(reloaded.get(&[0xA2; 32]).is_some());
    }
```

- [ ] **Step 2: Run tests, verify they fail**

```bash
cargo test content_index::tests 2>&1 | head -30
```

Expected: compile errors — `insert`, `remove`, `get`, `set_archived`, `set_replication_tier` not found on `ContentIndex`.

- [ ] **Step 3: Implement the mutation methods**

Add to the `impl ContentIndex` block in `content_index.rs`:

```rust
    /// Insert a new entry. Returns `true` if added, `false` if the CID
    /// was already present (no mutation in that case). Callers that want
    /// overwrite semantics should remove first.
    pub fn insert(&mut self, entry: ContentIndexEntry) -> bool {
        if self.entries.contains_key(&entry.cid) {
            return false;
        }
        self.entries.insert(entry.cid, entry);
        self.save();
        true
    }

    /// Remove an entry by CID. Returns `true` if present before the call.
    pub fn remove(&mut self, cid: &[u8; 32]) -> bool {
        let removed = self.entries.remove(cid).is_some();
        if removed {
            self.save();
        }
        removed
    }

    /// Flip the `archived` flag. Returns `true` if the flag changed;
    /// `false` if already at the target state or the CID is unknown.
    pub fn set_archived(&mut self, cid: &[u8; 32], archived: bool) -> bool {
        let Some(entry) = self.entries.get_mut(cid) else {
            return false;
        };
        if entry.archived == archived {
            return false;
        }
        entry.archived = archived;
        self.save();
        true
    }

    /// Set replication tier on a batch. Returns the count of entries
    /// whose tier actually changed (missing or already-at-tier entries
    /// are skipped silently).
    pub fn set_replication_tier(
        &mut self,
        cids: &[[u8; 32]],
        tier: ReplicationTier,
    ) -> usize {
        let mut changed = 0;
        for cid in cids {
            if let Some(entry) = self.entries.get_mut(cid) {
                if entry.replication_tier != tier {
                    entry.replication_tier = tier;
                    changed += 1;
                }
            }
        }
        if changed > 0 {
            self.save();
        }
        changed
    }

    pub fn get(&self, cid: &[u8; 32]) -> Option<&ContentIndexEntry> {
        self.entries.get(cid)
    }

    pub fn entries(&self) -> impl Iterator<Item = &ContentIndexEntry> {
        self.entries.values()
    }
}
```

Note: the closing `}` above matches the single `impl ContentIndex` block — make sure you're not adding a second `impl` block. Open `content_index.rs` and confirm these methods live inside the existing impl.

- [ ] **Step 4: Run tests, verify they pass**

```bash
cargo test content_index::tests
```

Expected: all content_index tests pass (11 total now).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/content_index.rs
git commit -m "feat(content-index): mutations (insert/remove/archive/tier) (ZEB-146)"
```

---

### Task B4: Add `ContentVerbRequest` enum + event-loop handler arm

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/event_loop.rs`

- [ ] **Step 1: Add the request enum near the other request structs**

In `event_loop.rs`, after the existing `IngestRequest` struct (around line 39), add:

```rust
/// Content-verb requests sent from Tauri commands into the event loop.
///
/// The event loop mutates `NodeRuntime.storage_tier()` / `cache()` in
/// response. Sidecar-only mutations (archive, replication tier) are NOT
/// routed through this channel — they run directly against the
/// `Arc<Mutex<ContentIndex>>` from the Tauri command handler.
pub enum ContentVerbRequest {
    Pin {
        cid: [u8; 32],
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Unpin {
        cid: [u8; 32],
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Burn {
        cid: [u8; 32],
        reply: oneshot::Sender<Result<bool, String>>,
    },
    /// Snapshot the set of currently-pinned CIDs in the runtime cache.
    /// Used by `list_content` to fill the `pinned` field per entry.
    PinnedSet {
        reply: oneshot::Sender<std::collections::HashSet<[u8; 32]>>,
    },
}
```

- [ ] **Step 2: Add the channel to the `run()` signature**

Modify the `pub async fn run(` signature in `event_loop.rs` to accept one more parameter, alongside `mut ingest_rx: mpsc::Receiver<IngestRequest>,`:

```rust
    mut ingest_rx: mpsc::Receiver<IngestRequest>,
    mut content_verb_rx: mpsc::Receiver<ContentVerbRequest>,
    mut follow_rx: mpsc::Receiver<FollowRequest>,
```

- [ ] **Step 3: Add the handler arm inside the `tokio::select!` block**

Find the ingest handler arm (search for `Some(req) = ingest_rx.recv()`). Directly after it — and before the `follow_rx` arm — insert:

```rust
            // ── Content-verb requests (pin/unpin/burn/snapshot) ────
            Some(req) = content_verb_rx.recv() => {
                use harmony_content::cid::ContentId;
                match req {
                    ContentVerbRequest::Pin { cid, reply } => {
                        let id = ContentId::from_bytes(cid);
                        let ok = runtime.pin_content(id);
                        let _ = reply.send(Ok(ok));
                    }
                    ContentVerbRequest::Unpin { cid, reply } => {
                        let id = ContentId::from_bytes(cid);
                        runtime.unpin_content(&id);
                        let _ = reply.send(Ok(true));
                    }
                    ContentVerbRequest::Burn { cid, reply } => {
                        // Burn on a RAM-only client = unpin so the cache
                        // can evict naturally. The sidecar-removal side
                        // of burn runs in the Tauri command handler.
                        let id = ContentId::from_bytes(cid);
                        runtime.unpin_content(&id);
                        let _ = reply.send(Ok(true));
                    }
                    ContentVerbRequest::PinnedSet { reply } => {
                        let cache = runtime.storage_tier().cache();
                        let pinned: std::collections::HashSet<[u8; 32]> = cache
                            .iter_admitted()
                            .filter(|id| cache.is_pinned(id))
                            .map(|id| id.to_bytes())
                            .collect();
                        let _ = reply.send(pinned);
                    }
                }
            }
```

- [ ] **Step 4: Run a compile check**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
cargo check --manifest-path src-tauri/Cargo.toml
```

Expected: compiles cleanly. Warnings about `content_verb_rx` being unused at the caller site (`lib.rs`) are OK at this stage — Task B5 wires them up.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "feat(event-loop): ContentVerbRequest channel + handler arm (ZEB-146)"
```

---

### Task B5: Thread `Arc<Mutex<ContentIndex>>` + verb channel through `lib.rs::start_node`

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/lib.rs`

- [ ] **Step 1: Add ContentIndex and verb-channel fields to `NodeState`**

Find the `struct NodeState` definition (grep `struct NodeState`). Add two fields alongside the existing `ingest_tx` field:

```rust
    ingest_tx: Option<mpsc::Sender<event_loop::IngestRequest>>,
    content_verb_tx: Option<mpsc::Sender<event_loop::ContentVerbRequest>>,
    content_index: std::sync::Arc<std::sync::Mutex<content_index::ContentIndex>>,
```

Then initialize them in `NodeState::default()` or wherever NodeState is constructed (search for `NodeState {`). Default values:

```rust
    content_verb_tx: None,
    content_index: std::sync::Arc::new(std::sync::Mutex::new(
        content_index::ContentIndex::load(std::path::Path::new(""))
    )),
```

The `Path::new("")` default loads an empty index (no file at an empty path); it's replaced with the real path in `start_node` below. This avoids a `Default::default`-unfriendly type.

- [ ] **Step 2: Construct the real `ContentIndex` in `start_node` alongside `FollowManager`**

Find where `follow_mgr` is loaded in `start_node` (grep `FollowManager::load`). Immediately after, add:

```rust
    let content_index = std::sync::Arc::new(std::sync::Mutex::new(
        content_index::ContentIndex::load(&app_data_dir),
    ));
```

- [ ] **Step 3: Create the verb channel and thread it into `event_loop::run`**

Near the other channel constructions in `start_node` (search for `let (ingest_tx, ingest_rx) = mpsc::channel`). Add:

```rust
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<event_loop::ContentVerbRequest>(32);
```

Then find the `event_loop::run(` call site (a large invocation passing many parameters) and add `content_verb_rx,` as a positional argument between `ingest_rx,` and `follow_rx,`.

- [ ] **Step 4: Store the sender side on `NodeState`**

After the channel is created but before `event_loop::run` is invoked, save the sender on the state guard. Find where `guard.ingest_tx = Some(ingest_tx.clone());` is set (or equivalent), and add:

```rust
    guard.content_verb_tx = Some(content_verb_tx.clone());
    guard.content_index = content_index.clone();
```

- [ ] **Step 5: Compile check**

```bash
cargo check --manifest-path src-tauri/Cargo.toml
```

Expected: compiles. Unused-variable warnings for `content_verb_tx` and `content_index` disappear once tasks B6–B7 reference them from the commands.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(lib): thread ContentIndex and verb channel through NodeState (ZEB-146)"
```

---

### Task B6: Wire `list_content`, `pin_content`, `unpin_content` against the verb channel + sidecar

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/lib.rs` (lines ~993–1011, replacing the three stubs).

- [ ] **Step 1: Define the wire struct near the existing `ContentAnnouncementPayload`**

Find `pub struct ContentAnnouncementPayload` (around line 955) and add beneath it:

```rust
/// Wire format returned by `list_content` — one entry per self-ingested
/// file the client is aware of. Joins sidecar metadata with runtime
/// cache's pinned state.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentItemWire {
    pub cid: String,              // hex
    pub name: String,
    pub size_bytes: u64,
    pub stored_at: u64,           // ms since epoch
    pub sensitivity: String,      // "private" | "confidential" | "public"
    pub replication_tier: String, // "minimal" | "default" | "durable"
    pub pinned: bool,
    pub licensed: bool,
    pub archived: bool,
}

fn sensitivity_wire(s: content_index::Sensitivity) -> &'static str {
    match s {
        content_index::Sensitivity::Private => "private",
        content_index::Sensitivity::Confidential => "confidential",
        content_index::Sensitivity::Public => "public",
    }
}

fn replication_tier_wire(t: content_index::ReplicationTier) -> &'static str {
    match t {
        content_index::ReplicationTier::Minimal => "minimal",
        content_index::ReplicationTier::Default => "default",
        content_index::ReplicationTier::Durable => "durable",
    }
}

fn parse_cid_hex(cid_hex: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(cid_hex).map_err(|_| "invalid cid hex".to_string())?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| "cid must be 32 bytes".to_string())
}
```

- [ ] **Step 2: Replace the `list_content` stub**

Find the existing `list_content` at approximately line 993:

```rust
#[tauri::command]
fn list_content() -> Vec<serde_json::Value> {
    // Future (bead fkz): query runtime's cache + disk index via query channel.
    Vec::new()
}
```

Replace with:

```rust
#[tauri::command]
async fn list_content(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<ContentItemWire>, String> {
    // 1. Snapshot pinned CIDs from the runtime cache.
    let verb_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .content_verb_tx
            .clone()
            .ok_or_else(|| "runtime unavailable".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    let pinned_set = reply_rx
        .await
        .map_err(|_| "event loop dropped snapshot request".to_string())?;

    // 2. Join sidecar entries with pinned state and shape the wire.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let entries: Vec<ContentItemWire> = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.entries()
            .map(|e| ContentItemWire {
                cid: hex::encode(e.cid),
                name: e.file_name.clone(),
                size_bytes: e.size_bytes,
                stored_at: e.stored_at_ms,
                sensitivity: sensitivity_wire(e.sensitivity).to_string(),
                replication_tier: replication_tier_wire(e.replication_tier).to_string(),
                pinned: pinned_set.contains(&e.cid),
                licensed: e.licensed,
                archived: e.archived,
            })
            .collect()
    };
    Ok(entries)
}
```

- [ ] **Step 3: Replace `pin_content` and `unpin_content` stubs**

Just below `list_content`, replace the two stubs with:

```rust
#[tauri::command]
async fn pin_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;
    let verb_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .content_verb_tx
            .clone()
            .ok_or_else(|| "runtime unavailable".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::Pin {
            cid: cid_bytes,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped pin request".to_string())?
}

#[tauri::command]
async fn unpin_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;
    let verb_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .content_verb_tx
            .clone()
            .ok_or_else(|| "runtime unavailable".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::Unpin {
            cid: cid_bytes,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped unpin request".to_string())?
}
```

- [ ] **Step 4: Compile check**

```bash
cargo check --manifest-path src-tauri/Cargo.toml
```

Expected: compiles. Existing `#[tauri::command]` registrations in `invoke_handler!` still reference these names, so no registration change needed.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(commands): wire list_content + pin/unpin to runtime + sidecar (ZEB-146)"
```

---

### Task B7: Wire `burn_content`, `archive_content`, add `set_replication_tier`

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/lib.rs`

- [ ] **Step 1: Replace `burn_content` stub**

Find the existing `burn_content` (around line 1013) and replace with:

```rust
#[tauri::command]
async fn burn_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;

    // 1. Unpin in the runtime cache so W-TinyLFU can reclaim the RAM.
    let verb_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .content_verb_tx
            .clone()
            .ok_or_else(|| "runtime unavailable".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::Burn {
            cid: cid_bytes,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    let _ = reply_rx
        .await
        .map_err(|_| "event loop dropped burn request".to_string())?;

    // 2. Remove the sidecar entry.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let removed = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.remove(&cid_bytes)
    };
    Ok(removed)
}
```

- [ ] **Step 2: Replace `archive_content` stub**

Replace with:

```rust
#[tauri::command]
async fn archive_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let flipped = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_archived(&cid_bytes, true)
    };
    Ok(flipped)
}
```

- [ ] **Step 3: Add `set_replication_tier` command**

Immediately after `archive_content`, add:

```rust
#[tauri::command]
async fn set_replication_tier(
    cids: Vec<String>,
    tier: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<u32, String> {
    let parsed_tier = match tier.as_str() {
        "minimal" => content_index::ReplicationTier::Minimal,
        "default" => content_index::ReplicationTier::Default,
        "durable" => content_index::ReplicationTier::Durable,
        other => return Err(format!("unknown replication tier: {other}")),
    };
    let mut parsed_cids: Vec<[u8; 32]> = Vec::with_capacity(cids.len());
    for c in &cids {
        parsed_cids.push(parse_cid_hex(c)?);
    }
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let updated = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_replication_tier(&parsed_cids, parsed_tier)
    };
    Ok(updated as u32)
}
```

- [ ] **Step 4: Register `set_replication_tier` in the `invoke_handler!` list**

Find the `tauri::generate_handler!` macro invocation at the bottom of `run()` (around line 1570). Add `set_replication_tier,` next to the existing content commands:

```rust
    .invoke_handler(tauri::generate_handler![
        // ... existing commands ...
        list_content,
        pin_content,
        unpin_content,
        burn_content,
        archive_content,
        set_replication_tier,
        // ... rest of list ...
    ])
```

- [ ] **Step 5: Compile check**

```bash
cargo check --manifest-path src-tauri/Cargo.toml
```

Expected: compiles cleanly.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(commands): wire burn/archive + add set_replication_tier (ZEB-146)"
```

---

### Task B8: Amend `ingest_content` to write a sidecar entry after runtime ack

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/lib.rs` (the `ingest_content` function, around line 1097).

- [ ] **Step 1: Locate the post-ack block**

Find the section of `ingest_content` that reads:

```rust
    reply_rx
        .await
        .map_err(|_| "event loop dropped ingest request".to_string())??;

    Ok(IngestResult {
        cid: cid_hex,
        file_name,
        size_bytes,
    })
}
```

- [ ] **Step 2: Insert sidecar write between the ack and the return**

Replace the final `Ok(IngestResult { ... })` block with:

```rust
    reply_rx
        .await
        .map_err(|_| "event loop dropped ingest request".to_string())??;

    // Record sidecar metadata so `list_content` can surface this entry.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let cid_bytes: [u8; 32] = cid.to_bytes();
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.insert(content_index::ContentIndexEntry {
            cid: cid_bytes,
            file_name: file_name.clone(),
            size_bytes,
            stored_at_ms,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
        });
    }

    Ok(IngestResult {
        cid: cid_hex,
        file_name,
        size_bytes,
    })
}
```

- [ ] **Step 3: Compile check**

```bash
cargo check --manifest-path src-tauri/Cargo.toml
```

Expected: compiles cleanly.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(ingest): write content-index sidecar entry on successful ingest (ZEB-146)"
```

---

### Task B9: Integration test — ingest → list → pin → burn round-trip

**Files:**
- Create: `/Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/content_index_integration.rs`

- [ ] **Step 1: Write the integration test skeleton**

```rust
//! End-to-end test: ingest a blob through the event loop, verify the
//! sidecar picks it up, drive pin/unpin/burn via the verb channel, and
//! confirm the runtime cache's pin state matches.
//!
//! Spins up an in-process NodeRuntime on a multi-threaded tokio runtime,
//! same pattern as `mail_sync_integration.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use harmony_app::content_index::{
    ContentIndex, ContentIndexEntry, ReplicationTier, Sensitivity,
};
use harmony_app::event_loop::{ContentVerbRequest, IngestRequest};
use harmony_content::book::MemoryBookStore;
use harmony_content::cid::{ContentFlags, ContentId};
use harmony_runtime::NodeRuntime;
use tokio::sync::{mpsc, oneshot, watch};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_list_pin_burn_roundtrip() {
    // ── Fixture: pick arbitrary bytes and compute the CID the runtime
    //    will assign (single-book, default flags).
    let bytes = b"hello world!!!!!!!!".to_vec();
    let expected_cid = ContentId::for_book(&bytes, ContentFlags::default())
        .expect("CID for fixture bytes");
    let expected_cid_bytes = expected_cid.to_bytes();

    // ── Zenoh session (in-process, needed for the event loop even if we
    //    don't exercise mesh transport here).
    let session = zenoh::open(zenoh::Config::default())
        .await
        .expect("zenoh open");

    // ── Spin up NodeRuntime + event loop.
    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();
    let config = harmony_app::test_support::minimal_node_config(&app_data_dir);
    //  ^ — if no existing test_support helper exists, inline the NodeConfig
    //    construction here following the pattern from lib.rs::start_node.
    let (runtime, startup_actions) = NodeRuntime::new(config, MemoryBookStore::new());

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (_fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel(4);
    let (_voice_ch_tx, voice_ch_rx) = mpsc::channel(4);
    let (_refresh_tx, refresh_rx) = mpsc::channel(4);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let followed_set = Arc::new(Mutex::new(Default::default()));
    let mail_mgr = Arc::new(Mutex::new(harmony_app::mail::MailManager::load(
        &app_data_dir.join("mail"),
        [0u8; 16],
    )));
    let app = tauri::test::mock_app();

    tokio::spawn(harmony_app::event_loop::run(
        runtime,
        startup_actions,
        app.handle().clone(),
        None,
        ready_tx,
        shutdown_rx,
        publish_rx,
        fetch_rx,
        ingest_rx,
        content_verb_rx,
        follow_rx,
        voice_rx,
        voice_ch_rx,
        followed_set,
        mail_mgr,
        None,
        refresh_rx,
    ));

    ready_rx.await.unwrap().unwrap();

    // ── Step 1: ingest via the IngestRequest channel.
    let cid_hex = hex::encode(expected_cid_bytes);
    let (ack_tx, ack_rx) = oneshot::channel();
    ingest_tx
        .send(IngestRequest {
            cid_hex: cid_hex.clone(),
            data: bytes.clone(),
            reply: ack_tx,
        })
        .await
        .unwrap();
    ack_rx.await.unwrap().unwrap();

    // ── Step 2: write a sidecar entry for the same CID (simulates
    //    what the Tauri ingest_content command does post-ack).
    let index = Arc::new(Mutex::new(ContentIndex::load(&app_data_dir)));
    {
        let mut idx = index.lock().unwrap();
        assert!(idx.insert(ContentIndexEntry {
            cid: expected_cid_bytes,
            file_name: "hello.txt".into(),
            size_bytes: bytes.len() as u64,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
        }));
    }

    // ── Step 3: PinnedSet — CID should NOT be pinned.
    let (snap_tx, snap_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
        .await
        .unwrap();
    let pinned = snap_rx.await.unwrap();
    assert!(!pinned.contains(&expected_cid_bytes), "fresh ingest unpinned");

    // ── Step 4: Pin, then snapshot — CID now pinned.
    let (pin_tx, pin_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Pin {
            cid: expected_cid_bytes,
            reply: pin_tx,
        })
        .await
        .unwrap();
    assert!(pin_rx.await.unwrap().unwrap(), "pin should succeed");

    let (snap_tx, snap_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
        .await
        .unwrap();
    assert!(snap_rx.await.unwrap().contains(&expected_cid_bytes));

    // ── Step 5: Burn, then confirm sidecar gone and CID no longer pinned.
    let (burn_tx, burn_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Burn {
            cid: expected_cid_bytes,
            reply: burn_tx,
        })
        .await
        .unwrap();
    burn_rx.await.unwrap().unwrap();

    {
        let mut idx = index.lock().unwrap();
        assert!(idx.remove(&expected_cid_bytes), "sidecar had entry");
    }

    let (snap_tx, snap_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
        .await
        .unwrap();
    assert!(!snap_rx.await.unwrap().contains(&expected_cid_bytes));

    // Keep shutdown_tx alive so the event loop doesn't terminate mid-test.
    let _ = &_shutdown_tx;
    let _ = Duration::from_millis(0); // keep std::time::Duration import used
}
```

- [ ] **Step 2: Run the integration test**

```bash
cargo test --test content_index_integration
```

Expected: passes. If it fails on `harmony_app::test_support::minimal_node_config`, inline the NodeConfig construction: read `lib.rs::start_node`'s config-building block (search for `NodeConfig {`) and replicate the minimal fields needed, defaulting unused ones. The `config` variable has no behavioral expectation in this test beyond "NodeRuntime::new succeeds."

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/content_index_integration.rs
git commit -m "test(content-index): E2E ingest→list→pin→burn roundtrip (ZEB-146)"
```

---

### Task B10: Frontend — make `connectAdapter` authoritative and await invoke results

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony-client/src/lib/file-manager-service.ts`

- [ ] **Step 1: Import `ContentItemWire`'s shape from types (or declare locally)**

At the top of `file-manager-service.ts`, add:

```ts
interface ContentItemWire {
  cid: string;
  name: string;
  sizeBytes: number;
  storedAt: number;
  sensitivity: 'private' | 'confidential' | 'public';
  replicationTier: 'minimal' | 'default' | 'durable';
  pinned: boolean;
  licensed: boolean;
  archived: boolean;
}
```

Place near the existing `IngestResult` interface.

- [ ] **Step 2: Add the `wireToContentItem` helper**

Below the interface, add:

```ts
function wireToContentItem(wire: ContentItemWire): ContentItem {
  return {
    cid: wire.cid,
    name: wire.name,
    category: inferCategory(wire.name),
    sensitivity: wire.sensitivity,
    sizeBytes: wire.sizeBytes,
    storedAt: wire.storedAt,
    lastAccessed: wire.storedAt,
    accessCount: 0,
    stalenessScore: 0,
    replicationTier: wire.replicationTier,
    replicaCount: 1,
    pinned: wire.pinned,
    licensed: wire.licensed,
    parentCid: null,
    isFolder: false,
  };
}
```

- [ ] **Step 3: Replace the `connectAdapter` body to make it authoritative**

Find the existing `connectAdapter` method and replace it:

```ts
async connectAdapter(adapter: TauriAdapter): Promise<void> {
  if (this.adapter) return;
  this.adapter = adapter;

  // Fetch the real content list and clear mocks unconditionally — even if
  // empty. Per ZEB-146, the file-manager UI must never mix mocks with real
  // state once the backend adapter is connected.
  const real = (await adapter.invoke('list_content')) as ContentItemWire[];
  this.privateContent = real.map(wireToContentItem);
  this.onChange?.();

  const unlisten = await adapter.listen(
    'content-announced',
    (event) => {
      const wire = event.payload as ContentAnnouncementEvent;
      if (this.announcedCids.has(wire.cid)) return;
      this.announcedCids = new Map([
        ...this.announcedCids,
        [wire.cid, { sizeBytes: wire.sizeBytes, firstSeen: Date.now() }],
      ]);
      this.onChange?.();
    },
  );
  this.unlisteners.push(unlisten);
}
```

- [ ] **Step 4: Convert `pin`, `unpin`, `burn`, `archive` from fire-and-forget to await-and-honor-result**

Replace the existing `pin`, `unpin`, `burn`, `archive` methods with:

```ts
async pin(cid: string): Promise<void> {
  if (!this.adapter) return;
  const ok = (await this.adapter.invoke('pin_content', { cid })) as boolean;
  if (!ok) {
    throw new Error('pin quota exhausted');
  }
  const item = this.privateContent.find((i) => i.cid === cid);
  if (item) item.pinned = true;
  this.onChange?.();
}

async unpin(cid: string): Promise<void> {
  if (!this.adapter) return;
  await this.adapter.invoke('unpin_content', { cid });
  const item = this.privateContent.find((i) => i.cid === cid);
  if (item) item.pinned = false;
  this.onChange?.();
}

async burn(cids: string[]): Promise<void> {
  if (!this.adapter) {
    // Offline-only path: still mutate local state so tests/Storybook work.
    const cidSet = new Set(cids);
    this.privateContent = this.privateContent.filter((i) => !cidSet.has(i.cid));
    this.onChange?.();
    return;
  }
  const results = await Promise.allSettled(
    cids.map((cid) => this.adapter!.invoke('burn_content', { cid })),
  );
  const succeeded = new Set(
    cids.filter((_, i) => results[i].status === 'fulfilled'),
  );
  this.privateContent = this.privateContent.filter((i) => !succeeded.has(i.cid));
  this.onChange?.();
}

async archive(cids: string[]): Promise<void> {
  if (!this.adapter) {
    const cidSet = new Set(cids);
    this.privateContent = this.privateContent.filter((i) => !cidSet.has(i.cid));
    this.onChange?.();
    return;
  }
  const results = await Promise.allSettled(
    cids.map((cid) => this.adapter!.invoke('archive_content', { cid })),
  );
  const succeeded = new Set(
    cids.filter((_, i) => results[i].status === 'fulfilled'),
  );
  this.privateContent = this.privateContent.filter((i) => !succeeded.has(i.cid));
  this.onChange?.();
}
```

- [ ] **Step 5: Update `setReplicationTier` to round-trip through the backend**

Replace:

```ts
async setReplicationTier(cids: string[], tier: ReplicationTier): Promise<void> {
  if (this.adapter) {
    const _updated = (await this.adapter.invoke('set_replication_tier', {
      cids,
      tier,
    })) as number;
  }
  const cidSet = new Set(cids);
  for (const item of this.privateContent) {
    if (cidSet.has(item.cid)) {
      item.replicationTier = tier;
    }
  }
  this.onChange?.();
}
```

Note: this method was previously synchronous; callers awaiting it (if any) will continue to work. Search the codebase for call sites: `grep -rn setReplicationTier src/` to confirm none depend on its old sync signature.

- [ ] **Step 6: Run frontend checks**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npm run check
```

Expected: no new TypeScript errors. If `check` reports call-site errors on `setReplicationTier` (because callers didn't `await`), adjust the callers — they should already have been in async contexts.

- [ ] **Step 7: Commit**

```bash
git add src/lib/file-manager-service.ts
git commit -m "feat(file-manager): authoritative connectAdapter + async verb handling (ZEB-146)"
```

---

### Task B11: Full regression sweep and PR preparation

**Files:** none modified in this task — it's verification.

- [ ] **Step 1: Run the full Rust test suite**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo test
```

Expected: all tests pass. If any failure is clearly unrelated to this PR's changes (environmental, cross-machine drift, pre-existing), do NOT fix in this PR — file a Linear follow-up per `feedback_unrelated_test_failures.md` and mention the ticket in the PR's "Out of scope / follow-ups" section.

- [ ] **Step 2: Run the full frontend checks**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npm run check
```

Expected: no TypeScript errors. Same unrelated-failure policy applies.

- [ ] **Step 3: Manual smoke test (macOS)**

```bash
npm run tauri dev
```

Steps in the running app:
1. Connect adapter (click "Start Node" or whatever wires the Tauri runtime).
2. Navigate to Files.
3. Confirm the file list is initially empty (mocks cleared).
4. Use the "Upload" / ingest button to add a file.
5. Confirm the file appears with its real filename, size, and `pinned: false`.
6. Click Pin → confirm it flips to pinned.
7. Click Burn → confirm the file is removed and a fresh list shows the empty state.
8. Restart the app → confirm the file-list reflects persistent sidecar state (file you didn't burn is still there).

If any step fails, debug and add a test case that would have caught it before fixing.

- [ ] **Step 4: Push and open the harmony-client PR**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin feat/file-manager-backend-zeb-146
gh pr create --title "feat(file-manager): wire backend commands to runtime + sidecar (ZEB-146)" --body "$(cat <<'EOF'
## Summary
- New `content_index.rs` sidecar persists self-ingested file metadata to `app_data_dir/content-index.json`.
- Unified `ContentVerbRequest` enum channel routes pin/unpin/burn/pinned-set through the event loop into the runtime cache.
- `list_content` / `pin_content` / `unpin_content` / `burn_content` / `archive_content` now do real work; `set_replication_tier` is a new command.
- `ingest_content` records a sidecar entry on successful ingest.
- Frontend `FileManagerService.connectAdapter` is now authoritative — mocks cleared unconditionally.
- Verbs await the backend and honor its result.

Folders, mesh retraction on burn, and real archive-tier wiring are deferred to follow-up specs (see spec doc).

Depends on `harmony#<phase-A-PR-number>` (merged).

## Test Plan
- [x] `cargo test` (full Rust suite)
- [x] `cargo test --test content_index_integration` (new E2E test)
- [x] `npm run check` (frontend type-check)
- [ ] Manual smoke test on macOS (see PR description for steps)
- [ ] Reviewer approval

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-review — spec coverage

Cross-checking this plan against the spec's sections:

| Spec section | Covered by |
|---|---|
| Background | Context only — no task needed |
| Design decisions Q1–Q5 | Architecture encoded in tasks; no separate task |
| Architecture (3 layers) | B2–B3 (sidecar), B4–B5 (channel), B6–B8 (commands) |
| `content_index.rs` shape | B2 (types + load/save), B3 (mutations) |
| Persistence format (JSON, versioned) | B2 |
| Load behavior (missing / malformed / duplicate) | B2 (all three tested) |
| Mutations (insert/remove/set_archived/set_replication_tier) | B3 |
| Read accessors (`entries()`, `get()`) | B3 |
| Concurrency (Arc<Mutex>) | B5 |
| `ContentVerbRequest` enum + PinnedSet | B4 |
| Event-loop handler arm | B4 |
| Upstream `NodeRuntime::storage_tier` + `StorageTier::cache` accessors | A1 |
| `ContentItemWire` wire contract | B6 |
| `list_content` implementation | B6 |
| `pin_content` / `unpin_content` | B6 |
| `burn_content` / `archive_content` / `set_replication_tier` | B7 |
| Error paths (invalid hex, channel closed) | B6/B7 (inline error returns) |
| `ingest_content` sidecar write | B8 |
| Frontend `connectAdapter` authoritative | B10 |
| `wireToContentItem` helper | B10 |
| Service methods async + error-honoring | B10 |
| Integration test (ingest → list → mutate) | B9 |
| Regression protection | B11 |
| Out of scope / follow-ups | No tasks — deferred explicitly |
| Dependency ordering | Phase A/B structure |

No gaps.

## Self-review — placeholder scan

Searched the plan for "TBD", "TODO", "implement later", "appropriate error handling", "similar to task N": none remain. All error paths show the exact `.map_err(...)` string; all test bodies contain the actual code.

One caveat worth flagging rather than fixing inline: Task B9 Step 2 notes "if no existing `test_support::minimal_node_config` helper exists, inline the NodeConfig construction." This is not a placeholder — it's a conditional on what's already in the codebase, which the executor can resolve by grepping `NodeConfig {` in `lib.rs`. If that conditional becomes blocking in practice, the fallback (inline construction patterned after `start_node`) is spelled out.

## Self-review — type consistency

- `ContentIndexEntry` fields: consistent across B2, B3, B8, B9, B10.
- `ContentVerbRequest` variants: `Pin`, `Unpin`, `Burn`, `PinnedSet` — consistent across B4, B6, B7, B9.
- Tier enum values: `Minimal | Default | Durable` in Rust; `"minimal" | "default" | "durable"` over wire — consistent throughout.
- Sensitivity enum: `Private | Confidential | Public` / `"private" | "confidential" | "public"` — consistent.
- `parse_cid_hex` signature: `fn parse_cid_hex(cid_hex: &str) -> Result<[u8; 32], String>` — used identically in B6, B7.
- `NodeState` fields added: `content_verb_tx`, `content_index` — referenced identically in B5, B6, B7, B8.

All consistent.
