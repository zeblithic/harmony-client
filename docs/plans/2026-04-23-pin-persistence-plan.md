# Pin Persistence Implementation Plan (ZEB-155)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist pin intent across restarts by adding a `pinned: bool` field to the sidecar, OR-joining it with the runtime's `PinnedSet` at display time, and re-running the pin cascade automatically when a fetch completes on a root with persisted intent.

**Architecture:** Two sources of pin information — sidecar (durable intent) and runtime `PinnedSet` (ephemeral effect). `list_content` OR-joins them. The event loop keeps an in-memory `pin_intent: HashSet<[u8; 32]>` sourced from the sidecar at `start_node` and synced by the `Pin`/`Unpin`/`Burn` verb arms. A new `fetch_completion_rx` channel carries the root CID from the spawned fetch task back to the main event loop after a successful `fetch_recursive`; the main-loop arm consults `pin_intent` and runs the existing cascade.

**Tech Stack:** Rust (Tauri v2 backend), `harmony-content` crate (CID, bundle, cache), `tokio::sync::mpsc` channels, serde with `#[serde(default)]` for backward-compatible sidecar migration.

**Spec:** `docs/specs/2026-04-23-pin-persistence-design.md`

**Branch:** `feat/pin-persistence-zeb-155`

---

## File structure

| File | Change | Responsibility |
|---|---|---|
| `src-tauri/src/content_index.rs` | Modify | Add `pinned: bool` field to `ContentIndexEntry`; add `set_pinned` mutator mirroring `set_archived`; unit tests for the new method and legacy-sidecar deserialization. |
| `src-tauri/src/lib.rs` | Modify | Update `ContentIndexEntry` struct literal in `ingest_content` (line 1560) with `pinned: false`; flip `list_content`'s pin-join to `sidecar.pinned \|\| runtime.contains`; call `set_pinned(true/false)` from `pin_content`/`unpin_content`/`burn_content` Tauri commands; build `pin_intent` from the sidecar in `start_node` and pass it into `event_loop::run`. |
| `src-tauri/src/event_loop.rs` | Modify | Add `pin_intent: HashSet<[u8; 32]>` parameter to `run`; mutate it from `Pin`/`Unpin`/`Burn` verb arms; add `fetch_completion_rx` channel + new select arm that consults `pin_intent` and runs the existing cascade; in `fetch_rx`'s spawned task, `send(root_cid)` on the completion channel after `Ok`. |
| `src-tauri/tests/content_index_integration.rs` | Modify | Add `pinned: false` to the two existing `ContentIndexEntry` struct literals (lines 180, 427); add new test `pin_intent_survives_restart`; add new test `fetch_complete_repins_on_intent`. |

No new files. No frontend / TypeScript changes.

---

## Task 1: Add `pinned: bool` to `ContentIndexEntry` + `set_pinned` method

Foundational data-model change. Introduces the field (with `#[serde(default)]` so legacy sidecars read clean), the mutator, and tests that prove the field round-trips and is backward-compatible with v1 sidecars written before this task.

**Files:**
- Modify: `src-tauri/src/content_index.rs` (struct definition at line 42; add new method after `set_archived` at line 172; update `sample_entry` helper at line 241; add four new tests in the existing `mod tests` block)
- Modify: `src-tauri/src/lib.rs` (`ingest_content` struct literal at line 1560)
- Modify: `src-tauri/tests/content_index_integration.rs` (struct literals at lines 180 and 427 — required so the existing integration tests still compile)

- [ ] **Step 1: Write the failing test for `set_pinned`**

Add at the end of the `mod tests` block in `src-tauri/src/content_index.rs` (after the existing `save_persists_mutations` test):

```rust
#[test]
fn set_pinned_flips_flag_and_reports_change() {
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(dir.path());
    let entry = sample_entry([0xB1; 32]);
    idx.insert(entry.clone());

    assert!(idx.set_pinned(&entry.cid, true));  // flipped
    assert!(idx.get(&entry.cid).unwrap().pinned);
    assert!(!idx.set_pinned(&entry.cid, true)); // idempotent, no change
    assert!(idx.set_pinned(&entry.cid, false)); // flipped back
    assert!(!idx.get(&entry.cid).unwrap().pinned);
}

#[test]
fn set_pinned_missing_cid_returns_false() {
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(dir.path());
    assert!(!idx.set_pinned(&[0xB2; 32], true));
}

#[test]
fn save_persists_pin_mutations() {
    let dir = tempdir().unwrap();
    {
        let mut idx = ContentIndex::load(dir.path());
        idx.insert(sample_entry([0xB3; 32]));
        assert!(idx.set_pinned(&[0xB3; 32], true));
    }
    let reloaded = ContentIndex::load(dir.path());
    assert!(
        reloaded.get(&[0xB3; 32]).expect("B3 persisted").pinned,
        "pinned flag must survive save/load"
    );
}

#[test]
fn legacy_sidecar_without_pinned_field_loads_as_unpinned() {
    // Simulate a pre-ZEB-155 sidecar: version 1, entries with every field
    // EXCEPT pinned. `#[serde(default)]` on the new field must make this
    // deserialize cleanly with pinned=false.
    let dir = tempdir().unwrap();
    let legacy_json = br#"{
        "version": 1,
        "entries": [
            {
                "cid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "file_name": "legacy.txt",
                "size_bytes": 10,
                "stored_at_ms": 1700000000000,
                "sensitivity": "private",
                "replication_tier": "default",
                "licensed": false,
                "archived": false
            }
        ]
    }"#;
    std::fs::write(dir.path().join(INDEX_FILE), legacy_json).unwrap();

    let idx = ContentIndex::load(dir.path());
    let entry = idx.get(&[0xAA; 32]).expect("legacy entry must load");
    assert!(!entry.pinned, "legacy entries must read as pinned=false");
    assert_eq!(entry.file_name, "legacy.txt");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p harmony-app --lib content_index::tests::set_pinned_flips_flag_and_reports_change content_index::tests::set_pinned_missing_cid_returns_false content_index::tests::save_persists_pin_mutations content_index::tests::legacy_sidecar_without_pinned_field_loads_as_unpinned`

Expected: all four FAIL to compile (no `set_pinned` method, no `pinned` field on `ContentIndexEntry`).

- [ ] **Step 3: Add `pinned: bool` field to `ContentIndexEntry`**

Modify `src-tauri/src/content_index.rs` around line 42. Replace the existing struct with:

```rust
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
    /// ZEB-155: persisted pin intent. True when the user has asked for
    /// this content to remain pinned across restarts. The runtime cache's
    /// `PinnedSet` is still authoritative for active eviction protection —
    /// this field is "the user wants this pinned whenever bytes are
    /// resident," joined with the runtime set at list_content time.
    ///
    /// `#[serde(default)]` makes pre-ZEB-155 sidecars readable: legacy
    /// entries deserialize with pinned=false (correct — they weren't
    /// pinned at their last save, since the field didn't exist).
    #[serde(default)]
    pub pinned: bool,
}
```

- [ ] **Step 4: Add `set_pinned` method**

Insert into `impl ContentIndex` block in `src-tauri/src/content_index.rs` immediately after `set_archived` (around line 182):

```rust
    /// Flip the `pinned` flag. Returns `true` if the flag changed;
    /// `false` if already at the target state or the CID is unknown.
    pub fn set_pinned(&mut self, cid: &[u8; 32], pinned: bool) -> bool {
        let Some(entry) = self.entries.get_mut(cid) else {
            return false;
        };
        if entry.pinned == pinned {
            return false;
        }
        entry.pinned = pinned;
        self.save();
        true
    }
```

- [ ] **Step 5: Update `sample_entry` test helper**

In `src-tauri/src/content_index.rs` at line 241, add `pinned: false` to the struct literal inside `sample_entry`:

```rust
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
            pinned: false,
        }
    }
```

- [ ] **Step 6: Update `ingest_content` struct literal**

In `src-tauri/src/lib.rs` at line 1560, add `pinned: false,` to the struct literal:

```rust
        let inserted = idx.insert(content_index::ContentIndexEntry {
            cid: root_cid_bytes,
            file_name: file_name.clone(),
            size_bytes,
            stored_at_ms,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
        });
```

- [ ] **Step 7: Update existing integration-test struct literals**

In `src-tauri/tests/content_index_integration.rs` at line 180, add `pinned: false,` to the struct literal:

```rust
            idx.insert(ContentIndexEntry {
                cid: expected_cid_bytes,
                file_name: "hello.txt".into(),
                size_bytes: bytes.len() as u64,
                stored_at_ms: 1_700_000_000_000,
                sensitivity: Sensitivity::Private,
                replication_tier: ReplicationTier::Default,
                licensed: false,
                archived: false,
                pinned: false,
            }),
```

And at line 427 in the same file, add `pinned: false,`:

```rust
        assert!(idx.insert(ContentIndexEntry {
            cid: root_cid.to_bytes(),
            file_name: "chunked.bin".into(),
            size_bytes: bytes.len() as u64,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
        }));
```

- [ ] **Step 8: Run the new tests to verify they pass**

Run: `cargo test -p harmony-app --lib content_index::tests::set_pinned_flips_flag_and_reports_change content_index::tests::set_pinned_missing_cid_returns_false content_index::tests::save_persists_pin_mutations content_index::tests::legacy_sidecar_without_pinned_field_loads_as_unpinned`

Expected: all four PASS.

- [ ] **Step 9: Run the full test suite to confirm no regressions**

Run: `cargo test -p harmony-app`

Expected: every previously-passing test still passes (construction-site updates in Steps 5-7 keep compile working; new tests from Step 1 pass).

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/content_index.rs src-tauri/src/lib.rs src-tauri/tests/content_index_integration.rs
git commit -m "$(cat <<'EOF'
feat(content-index): persisted pinned field + set_pinned mutator (ZEB-155)

Adds ContentIndexEntry.pinned with #[serde(default)] so pre-ZEB-155
sidecars read clean (entries deserialize with pinned=false). set_pinned
mirrors set_archived's idempotent/missing-CID contract. Construction
sites in ingest_content and the two integration tests updated.

Display-layer join and Tauri command wiring land in the next commits.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Display-layer OR in `list_content`

Makes the pin badge survive restart unconditionally by reading from sidecar intent OR runtime effect. One-line production code change. Unit tests confirm both directions of the OR.

**Files:**
- Modify: `src-tauri/src/lib.rs` (`list_content` at line 1218; add a small unit test near the command)

- [ ] **Step 1: Write the failing tests for the OR join**

The existing `list_content` command takes a live Tauri state; per the repo pattern (see `chunked_ingest_tests` in lib.rs — exists post-ZEB-154), pure unit tests target helpers rather than the full command. We'll extract the join logic into a testable pure helper and unit-test it.

Add at the end of `src-tauri/src/lib.rs` (bottom of the file, after any existing `#[cfg(test)]` modules):

```rust
#[cfg(test)]
mod pin_persistence_tests {
    use super::*;
    use std::collections::HashSet;

    fn sidecar_entry(cid: [u8; 32], pinned: bool) -> content_index::ContentIndexEntry {
        content_index::ContentIndexEntry {
            cid,
            file_name: "t.txt".into(),
            size_bytes: 0,
            stored_at_ms: 0,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned,
        }
    }

    #[test]
    fn joined_pinned_true_when_only_intent_is_set() {
        let entry = sidecar_entry([0x11; 32], true);
        let runtime_pinned: HashSet<[u8; 32]> = HashSet::new();
        assert!(joined_pinned(&entry, &runtime_pinned));
    }

    #[test]
    fn joined_pinned_true_when_only_runtime_effect_is_set() {
        let entry = sidecar_entry([0x22; 32], false);
        let mut runtime_pinned: HashSet<[u8; 32]> = HashSet::new();
        runtime_pinned.insert([0x22; 32]);
        assert!(joined_pinned(&entry, &runtime_pinned));
    }

    #[test]
    fn joined_pinned_true_when_both_agree() {
        let entry = sidecar_entry([0x33; 32], true);
        let mut runtime_pinned: HashSet<[u8; 32]> = HashSet::new();
        runtime_pinned.insert([0x33; 32]);
        assert!(joined_pinned(&entry, &runtime_pinned));
    }

    #[test]
    fn joined_pinned_false_when_neither_says_so() {
        let entry = sidecar_entry([0x44; 32], false);
        let runtime_pinned: HashSet<[u8; 32]> = HashSet::new();
        assert!(!joined_pinned(&entry, &runtime_pinned));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p harmony-app --lib pin_persistence_tests`

Expected: all four FAIL to compile (`joined_pinned` is undefined).

- [ ] **Step 3: Extract the `joined_pinned` helper**

In `src-tauri/src/lib.rs`, add this helper immediately before the `list_content` command (just before line 1183):

```rust
/// ZEB-155: resolve the `pinned` flag for a single wire entry by
/// OR-joining the sidecar's persisted intent with the runtime cache's
/// currently-pinned set. Extracted so unit tests can exercise the join
/// logic without a live Tauri state.
fn joined_pinned(
    entry: &content_index::ContentIndexEntry,
    runtime_pinned: &std::collections::HashSet<[u8; 32]>,
) -> bool {
    entry.pinned || runtime_pinned.contains(&entry.cid)
}
```

- [ ] **Step 4: Use `joined_pinned` in `list_content`**

In `src-tauri/src/lib.rs` at line 1218, replace the line:

```rust
                pinned: pinned_set.contains(&e.cid),
```

with:

```rust
                pinned: joined_pinned(e, &pinned_set),
```

- [ ] **Step 5: Run the new tests to verify they pass**

Run: `cargo test -p harmony-app --lib pin_persistence_tests`

Expected: all four PASS.

- [ ] **Step 6: Run the full test suite to confirm no regressions**

Run: `cargo test -p harmony-app`

Expected: every previously-passing test still passes.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(list-content): OR-join sidecar pin intent with runtime PinnedSet (ZEB-155)

Restores the pin badge after restart even when bytes aren't resident.
joined_pinned extracted as a pure helper so the OR is unit-testable
without spinning a Tauri state. Runtime cache remains authoritative for
eviction protection; this only changes what list_content reports.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Wire `set_pinned` into `pin_content` / `unpin_content` / `burn_content` Tauri commands

Makes user actions actually update the sidecar. Sidecar-first order (write intent, then dispatch runtime verb) so a crash between the two leaves the durable side consistent with the user's click. Integration test `pin_intent_survives_restart` is the TDD gate — it fails until this task lands.

**Files:**
- Modify: `src-tauri/src/lib.rs` (`pin_content` at line 1231, `unpin_content` at line 1257, `burn_content` at line 1283)
- Modify: `src-tauri/tests/content_index_integration.rs` (add a new test function after the existing ones)

- [ ] **Step 1: Write the failing integration test**

Append this new test function to `src-tauri/tests/content_index_integration.rs` (after the existing tests). The test sidecar starts with `pinned: false`; we flip it via `set_pinned` directly (mimicking what the Tauri command will do in Step 4), drop the sidecar, reload, and verify persistence:

```rust
/// ZEB-155: verify that calling `set_pinned(true)` persists across a
/// load/reload cycle. This is the minimum regression test that the
/// pin_content command must preserve when Step 4 wires set_pinned into
/// the command body. The full end-to-end Tauri-command path is covered
/// by frontend manual QA; this test fixes the data-layer contract.
#[test]
fn pin_intent_survives_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let cid = [0xC1u8; 32];

    {
        let mut idx = ContentIndex::load(tmp.path());
        idx.insert(ContentIndexEntry {
            cid,
            file_name: "persist-me.bin".into(),
            size_bytes: 100,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
        });
        assert!(idx.set_pinned(&cid, true), "initial flip should report change");
    }

    // Reload — simulates app restart.
    let reloaded = ContentIndex::load(tmp.path());
    let entry = reloaded.get(&cid).expect("entry must persist");
    assert!(
        entry.pinned,
        "pinned intent must survive reload (this is the ZEB-155 bug fix)"
    );
}
```

- [ ] **Step 2: Run the test to verify it passes (the data-layer path already works from Task 1)**

Run: `cargo test -p harmony-app --test content_index_integration pin_intent_survives_reload`

Expected: PASS. Task 1's `set_pinned` is already wired to call `self.save()`, so the reload round-trip works. This test documents the contract; Steps 3-5 below wire the contract into the Tauri commands so users actually exercise it.

- [ ] **Step 3: Update `pin_content` to write sidecar intent**

In `src-tauri/src/lib.rs` at line 1231, replace the body of `pin_content` with the sidecar-first version:

```rust
#[tauri::command]
async fn pin_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;

    // ZEB-155: persist pin intent on the sidecar BEFORE dispatching the
    // runtime verb. If the event loop is gone or the runtime-side fails,
    // the durable side still records what the user wanted. Sidecar writes
    // are best-effort (tracing::warn on failure, matching set_archived /
    // set_replication_tier) — a disk-write error drops the intent but
    // still takes effect this session.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_pinned(&cid_bytes, true);
    }

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
```

- [ ] **Step 4: Update `unpin_content` to clear sidecar intent**

In `src-tauri/src/lib.rs` at line 1257, replace the body of `unpin_content` with:

```rust
#[tauri::command]
async fn unpin_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;

    // ZEB-155: clear sidecar intent first, then dispatch the runtime
    // unpin. Mirror of pin_content's ordering — durable side stays
    // consistent with the user's click across a crash between the
    // sidecar write and the verb dispatch.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_pinned(&cid_bytes, false);
    }

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

- [ ] **Step 5: Confirm `burn_content` handles pin intent correctly**

`burn_content` at line 1283 already calls `idx.remove(&cid_bytes)` after the runtime-side burn. Removing the sidecar entry drops any pin intent attached to it, so no code change is needed in `burn_content` — but add a doc comment for the next contributor who looks at this flow:

In `src-tauri/src/lib.rs` at line 1283, replace the existing doc comment / first line of `burn_content`:

```rust
#[tauri::command]
async fn burn_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;
```

with:

```rust
/// Burn a CID: unpin runtime-side, then remove the sidecar entry.
///
/// ZEB-155: removing the sidecar entry implicitly drops any persisted
/// pin intent — no explicit `set_pinned(false)` needed, because there's
/// no entry left to hold a flag on. The runtime-side Burn arm in the
/// event loop also removes the CID from pin_intent (see event_loop.rs).
#[tauri::command]
async fn burn_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;
```

- [ ] **Step 6: Run the full test suite**

Run: `cargo test -p harmony-app`

Expected: every test passes, including the new `pin_intent_survives_reload` from Step 1.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/content_index_integration.rs
git commit -m "$(cat <<'EOF'
feat(pin-commands): persist pin intent on sidecar from Tauri commands (ZEB-155)

pin_content / unpin_content now set_pinned(true/false) on the sidecar
before dispatching to the event loop. Sidecar-first ordering so a crash
between the two leaves the durable side consistent with the user's
click. burn_content implicitly drops pin intent via its existing
sidecar remove. Integration test pin_intent_survives_reload documents
the data-layer contract.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Event-loop `pin_intent` + fetch-completion replay hook

The B-path: make fetched roots that carry persisted intent auto-pin once their bytes are resident. Adds a `pin_intent: HashSet<[u8; 32]>` owned by the main event-loop task (no mutex needed — only the main loop mutates and reads it). A new `fetch_completion` mpsc channel carries root CIDs from the spawned fetch task back to the main loop after a successful `fetch_recursive`; the new select arm consults `pin_intent` and runs the existing `collect_descendants` + `runtime.pin_content` cascade.

**Channel ownership note:** both halves of `fetch_completion` (`tx` + `rx`) are passed into `event_loop::run` as parameters. The main loop reads from `rx`; the spawned fetch task clones `tx` to ping on success. This placement — rather than constructing the pair inside `run` — is what lets integration tests inject a completion signal synthetically without driving the full fetch_rx → Zenoh path (Zenoh-based fetch has no peer in-process and would time out).

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (add two parameters + new select arm; sync `pin_intent` in the three verb arms; ping `fetch_completion_tx` from the spawned fetch task)
- Modify: `src-tauri/src/lib.rs` (`start_node` — build initial `pin_intent` from sidecar, create `fetch_completion` channel pair, pass all into `event_loop::run`)
- Modify: `src-tauri/tests/content_index_integration.rs` (update the two existing `event_loop::run` callsites to pass the three new args; add new test `fetch_complete_arm_pins_root_in_intent`)

- [ ] **Step 1: Write the failing integration test**

Append to `src-tauri/tests/content_index_integration.rs`. This test injects a synthetic completion signal via a test-owned clone of `fetch_completion_tx`, avoiding the real fetch path entirely:

```rust
/// ZEB-155: when the fetch-completion arm receives a root CID that's in
/// pin_intent, the cascade pins the root (and any descendants) in the
/// runtime cache. Injected via a test-owned fetch_completion_tx clone so
/// we don't need a real peer to answer a fetch_rx request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_complete_arm_pins_root_in_intent() {
    use std::collections::HashSet;

    let bytes = b"zeb-155 fetch-complete repin fixture".to_vec();
    let cid = ContentId::for_book(&bytes, ContentFlags::default())
        .expect("fixture CID");
    let cid_bytes: [u8; 32] = cid.to_bytes();
    let cid_hex = hex::encode(cid_bytes);

    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (_fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (_voice_ch_tx, voice_ch_rx) =
        mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) =
        mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    // ZEB-155: the fetch-completion channel. Test keeps its own clone of
    // the sender so it can inject the completion signal synthetically.
    let (fetch_completion_tx, fetch_completion_rx) = mpsc::channel::<[u8; 32]>(8);
    let fetch_completion_tx_for_test = fetch_completion_tx.clone();

    let followed_set = Arc::new(Mutex::new(
        std::collections::HashSet::<String>::default(),
    ));
    let mail_mgr = Arc::new(Mutex::new(harmony_app::mail::MailManager::load(
        &app_data_dir.join("mail"),
        [0u8; 16],
    )));

    let app = tauri::test::mock_app();
    let app_handle = app.handle().clone();

    let config = NodeConfig {
        storage_budget: StorageBudget {
            cache_capacity: 512,
            max_pinned_bytes: 50_000_000,
        },
        compute_budget: InstructionBudget { fuel: 100_000 },
        schedule: Default::default(),
        content_policy: ContentPolicy::default(),
        filter_broadcast_config: FilterBroadcastConfig {
            mutation_threshold: 10,
            max_interval_ticks: 40,
            expected_items: 512,
            fp_rate: 0.001,
        },
        node_addr: "0000000000000000000000000000000000000000".to_string(),
        local_identity_hash: [0u8; 16],
        local_pq_identity_hash: [0u8; 16],
        local_dsa_pubkey: vec![],
        local_kem_pubkey: vec![],
        reticulum_identity_bytes: None,
        inference_gguf_cid: None,
        inference_tokenizer_cid: None,
        engram_manifest_cid: None,
        disk_enabled: false,
        disk_entries: Vec::new(),
        disk_quota: None,
        archive_enabled: false,
        archive_entries: Vec::new(),
        archive_quota: None,
        archive_ingest_enabled: false,
        eviction_push_enabled: false,
        s3_enabled: false,
    };

    // Seed pin_intent with our CID so the completion arm will recognize it.
    let mut pin_intent: HashSet<[u8; 32]> = HashSet::new();
    pin_intent.insert(cid_bytes);

    thread::Builder::new()
        .name("harmony-runtime-zeb155".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let (runtime, startup_actions) =
                    NodeRuntime::new(config, MemoryBookStore::new());
                harmony_app::event_loop::run(
                    runtime,
                    startup_actions,
                    app_handle,
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
                    pin_intent,
                    fetch_completion_tx,
                    fetch_completion_rx,
                )
                .await;
            });
        })
        .expect("spawn runtime thread");

    match ready_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) if e.contains("Address already in use") => {
            eprintln!("skipping test: {e}");
            return;
        }
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }

    // Admit bytes for the CID by ingesting. Required because collect_descendants
    // walks the cache; pin_content is a no-op on an unadmitted CID.
    let (ack_tx, ack_rx) = oneshot::channel();
    ingest_tx
        .send(IngestRequest {
            cid_hex: cid_hex.clone(),
            data: bytes.clone(),
            reply: ack_tx,
        })
        .await
        .unwrap();
    ack_rx.await.unwrap().expect("ingest succeeded");

    // Baseline: the CID is admitted but unpinned (fresh ingest doesn't pin).
    let (snap_tx, snap_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
        .await
        .unwrap();
    assert!(
        !snap_rx.await.unwrap().contains(&cid_bytes),
        "baseline: fresh ingest should not be pinned",
    );

    // Inject the completion signal. The main-loop arm will consult
    // pin_intent, find our CID, and run the cascade.
    fetch_completion_tx_for_test.send(cid_bytes).await.unwrap();

    // Poll PinnedSet until the cascade lands, or time out. The completion
    // arm and the snapshot arm are both serviced by the same select loop,
    // so we can't race them in principle, but tokio scheduling can still
    // interleave replies.
    let mut attempts = 0;
    loop {
        let (snap_tx, snap_rx) = oneshot::channel();
        content_verb_tx
            .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
            .await
            .unwrap();
        if snap_rx.await.unwrap().contains(&cid_bytes) {
            break; // success
        }
        attempts += 1;
        if attempts > 20 {
            panic!(
                "fetch-completion arm did not pin the CID within ~1s \
                 (20 × 50ms); pin_intent containing the CID should \
                 trigger the cascade on completion signal",
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p harmony-app --test content_index_integration fetch_complete_arm_pins_root_in_intent`

Expected: FAIL to compile — `event_loop::run` doesn't yet accept `pin_intent`, `fetch_completion_tx`, or `fetch_completion_rx`. This is the gate.

- [ ] **Step 3: Add `pin_intent` + fetch-completion channel parameters to `event_loop::run`**

In `src-tauri/src/event_loop.rs` at line 94, update the `run` function signature. After `mut refresh_rx` (line 111), append three new parameters — the intent set, and both halves of the fetch-completion channel:

```rust
pub async fn run<R: Runtime>(
    mut runtime: NodeRuntime<MemoryBookStore>,
    startup_actions: Vec<RuntimeAction>,
    app: AppHandle<R>,
    endpoint: Option<String>,
    ready_tx: oneshot::Sender<Result<(), String>>,
    mut shutdown: watch::Receiver<bool>,
    mut publish_rx: mpsc::Receiver<PublishRequest>,
    mut fetch_rx: mpsc::Receiver<FetchRequest>,
    mut ingest_rx: mpsc::Receiver<IngestRequest>,
    mut content_verb_rx: mpsc::Receiver<ContentVerbRequest>,
    mut follow_rx: mpsc::Receiver<FollowRequest>,
    mut voice_rx: mpsc::Receiver<crate::voice::VoiceOutbound>,
    mut voice_channel_rx: mpsc::Receiver<crate::voice::VoiceChannelRequest>,
    followed_set: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    mail_mgr: std::sync::Arc<std::sync::Mutex<crate::mail::MailManager>>,
    mail_sync: Option<Arc<crate::mail_sync::MailSync<R>>>,
    mut refresh_rx: mpsc::Receiver<crate::mail_sync::RefreshRequest>,
    mut pin_intent: std::collections::HashSet<[u8; 32]>,
    fetch_completion_tx: mpsc::Sender<[u8; 32]>,
    mut fetch_completion_rx: mpsc::Receiver<[u8; 32]>,
) {
```

The `pin_intent` is taken by-value (mutated in-place in the verb arms). `fetch_completion_tx` is kept as a non-mut `Sender` because the spawned fetch task will `.clone()` it; `fetch_completion_rx` is the main loop's receiver.

No inner channel declaration is needed — the channel pair is passed in, so a test can inject synthetic completion signals via its own clone of the sender, and production wiring (Step 7) creates the pair in `start_node`.

- [ ] **Step 4: Sync `pin_intent` in the Pin / Unpin / Burn arms**

In `src-tauri/src/event_loop.rs`, locate the `ContentVerbRequest::Pin` arm (around line 584). Replace the three arms (Pin, Unpin, Burn) with versions that keep `pin_intent` in sync. The existing body of each arm stays; only the two lines marked "ZEB-155" are new:

```rust
                    ContentVerbRequest::Pin { cid, reply } => {
                        // ZEB-155: record intent in the event-loop cache so
                        // fetch-completion can auto-repin after a resurrect.
                        pin_intent.insert(cid);
                        let root = ContentId::from_bytes(cid);
                        let all = collect_descendants(runtime.storage_tier().cache(), root);
                        let mut any_failed = false;
                        for id in all {
                            if !runtime.pin_content(id) {
                                any_failed = true;
                            }
                        }
                        let _ = reply.send(Ok(!any_failed));
                    }
                    ContentVerbRequest::Unpin { cid, reply } => {
                        // ZEB-155: clear intent so a later fetch doesn't re-pin.
                        pin_intent.remove(&cid);
                        let root = ContentId::from_bytes(cid);
                        let all = collect_descendants(runtime.storage_tier().cache(), root);
                        for id in all {
                            runtime.unpin_content(&id);
                        }
                        let _ = reply.send(Ok(true));
                    }
                    ContentVerbRequest::Burn { cid, reply } => {
                        // Burn on a RAM-only client cascades the runtime-side
                        // unpin; the sidecar-removal side of burn continues to
                        // happen in the Tauri command handler.
                        // ZEB-155: also drop any persisted intent (the Tauri
                        // command removes the sidecar entry, but this keeps
                        // the in-memory set consistent if the orders diverge).
                        pin_intent.remove(&cid);
                        let root = ContentId::from_bytes(cid);
                        let all = collect_descendants(runtime.storage_tier().cache(), root);
                        for id in all {
                            runtime.unpin_content(&id);
                        }
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
```

- [ ] **Step 5: Add the fetch-completion select arm**

In `src-tauri/src/event_loop.rs`, add a new arm to the main `select!` block. The cleanest place is immediately after the `content_verb_rx` arm (after the closing `}` of the `Some(req) = content_verb_rx.recv() => { ... }` block, around line 624):

```rust
                    }
                }
            }

            // ── Fetch-completion replay hook (ZEB-155) ─────────────
            // Spawned fetch tasks send on fetch_completion_tx after
            // fetch_recursive returns Ok. If pin_intent contains the
            // root, re-run the pin cascade now that bytes are resident.
            Some(root_bytes) = fetch_completion_rx.recv() => {
                if pin_intent.contains(&root_bytes) {
                    use harmony_content::cid::ContentId;
                    let root = ContentId::from_bytes(root_bytes);
                    let all = collect_descendants(runtime.storage_tier().cache(), root);
                    for id in all {
                        runtime.pin_content(id);
                    }
                }
            }
```

Note: the `use harmony_content::cid::ContentId;` is scoped to this arm because the surrounding `match` already imports `ContentId`; the repeated `use` inside the arm keeps the arm self-contained at the cost of one line. If the compiler warns about an unused outer import or a shadowed one, move the import to the top of the file instead.

- [ ] **Step 6: Send on `fetch_completion_tx` from the spawned fetch task**

In `src-tauri/src/event_loop.rs` at around line 505, the `fetch_rx` arm spawns a task. Update it so that after `fetch_recursive` returns `Ok`, the task pings the completion channel. Replace the existing spawned task body:

```rust
            Some(req) = fetch_rx.recv() => {
                let session = session.clone();
                let cid_hex = req.cid_hex;
                // ZEB-155: clone the completion sender so the spawned
                // task can notify the main loop after a successful fetch.
                let completion_tx = fetch_completion_tx.clone();
                tokio::spawn(async move {
                    let cid_bytes = match hex::decode(&cid_hex)
                        .ok()
                        .and_then(|b| <[u8; 32]>::try_from(b).ok())
                    {
                        Some(b) => b,
                        None => {
                            let _ = req.reply.send(Err(format!("invalid CID hex: {cid_hex}")));
                            return;
                        }
                    };
                    let root = ContentId::from_bytes(cid_bytes);

                    let fetch_one = move |cid: ContentId| {
                        let session = session.clone();
                        async move {
                            let cid_hex = hex::encode(cid.to_bytes());
                            let prefix = cid_hex.get(1..2).unwrap_or("");
                            let key = format!("harmony/content/{prefix}/{cid_hex}");
                            fetch_via_zenoh(&session, &key).await
                        }
                    };

                    let result = fetch_recursive(fetch_one, root).await;
                    // ZEB-155: ping the completion channel on success so
                    // the main loop can consult pin_intent and re-pin.
                    // Fire-and-forget: send failure means the event loop
                    // is shutting down, which is fine.
                    if result.is_ok() {
                        let _ = completion_tx.send(cid_bytes).await;
                    }
                    let _ = req.reply.send(result);
                });
            }
```

- [ ] **Step 7: Build `pin_intent` and the fetch-completion channel in `start_node`, pass them into `event_loop::run`**

In `src-tauri/src/lib.rs`, find the block in `start_node` where `content_index` is loaded (around line 416):

```rust
    let content_index = std::sync::Arc::new(std::sync::Mutex::new(
        content_index::ContentIndex::load(&app_data_dir),
    ));
```

Immediately after it, add:

```rust
    // ZEB-155: seed the event loop's pin_intent set from the sidecar so
    // a fetch after restart can re-pin restored intent. Built here under
    // the content_index lock, then moved by-value into run_event_loop.
    let pin_intent: std::collections::HashSet<[u8; 32]> = {
        let idx = content_index
            .lock()
            .map_err(|e| format!("content_index lock on startup: {e}"))?;
        idx.entries()
            .filter(|e| e.pinned)
            .map(|e| e.cid)
            .collect()
    };

    // ZEB-155: fetch-completion channel. Both halves are owned by
    // start_node so the spawned fetch task (in event_loop) can clone the
    // tx, while the main loop consumes from the rx.
    let (fetch_completion_tx, fetch_completion_rx) =
        tokio::sync::mpsc::channel::<[u8; 32]>(32);
```

Then find the call to `harmony_app::event_loop::run(...)` inside the spawned thread body in `start_node` (around line 555-580 based on the earlier read). Add the three new arguments at the end:

```rust
                    harmony_app::event_loop::run(
                        runtime,
                        startup_actions,
                        app_clone,
                        ep_clone,
                        ready_tx,
                        shutdown_rx,
                        publish_rx,
                        fetch_rx,
                        ingest_rx,
                        content_verb_rx,
                        follow_rx,
                        voice_rx,
                        voice_channel_rx,
                        followed_set_clone,
                        mail_mgr_clone,
                        Some(mail_sync_for_loop),
                        mail_refresh_rx,
                        pin_intent,
                        fetch_completion_tx,
                        fetch_completion_rx,
                    )
                    .await;
```

All three variables (`pin_intent`, `fetch_completion_tx`, `fetch_completion_rx`) must be declared in `start_node`'s scope before the `thread::Builder::new()...spawn(move || { ... })` block so the `move ||` closure captures them by value into the thread.

- [ ] **Step 8: Update the existing integration test `event_loop::run` callsites**

In `src-tauri/tests/content_index_integration.rs`, find the existing call to `harmony_app::event_loop::run` in `ingest_list_pin_burn_roundtrip` (around line 116). Create a dummy `fetch_completion` channel pair in the test's outer scope (before the thread spawn), then pass an empty `HashSet` plus both channel halves as the three new trailing arguments:

Add to the test's outer scope (before the `thread::Builder...spawn(move || { ... })` block):

```rust
    // ZEB-155: event_loop::run now takes pin_intent + fetch_completion
    // channel halves. This test doesn't exercise pin persistence, so an
    // empty set and a drain-only channel are sufficient.
    let (fetch_completion_tx, fetch_completion_rx) = mpsc::channel::<[u8; 32]>(4);
    let pin_intent: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();
```

Update the `event_loop::run` call inside the spawned thread body:

```rust
                harmony_app::event_loop::run(
                    runtime,
                    startup_actions,
                    app_handle,
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
                    pin_intent,
                    fetch_completion_tx,
                    fetch_completion_rx,
                )
                .await;
```

Apply the same update to the second existing test `chunked_ingest_pin_cascade_fetch_burn_roundtrip` (grep for `event_loop::run` — there are exactly two existing callsites in the file plus the one in the new `fetch_complete_arm_pins_root_in_intent` added by Step 1).

- [ ] **Step 9: Run the new integration test to verify it passes**

Run: `cargo test -p harmony-app --test content_index_integration fetch_complete_arm_pins_root_in_intent`

Expected: PASS. The main-loop completion arm consults `pin_intent`, sees the test's seeded CID, and runs the cascade, flipping the runtime's `PinnedSet` to include the CID. The test polls for up to ~1s (tokio scheduling tolerance); if the arm isn't wired, the poll times out with a clear panic message pointing at the hook.

- [ ] **Step 10: Run the full test suite**

Run: `cargo test -p harmony-app`

Expected: every test passes. The two pre-existing integration tests compile with the new `HashSet::new()` argument; `ingest_list_pin_burn_roundtrip` and `chunked_ingest_pin_cascade_fetch_burn_roundtrip` still pass because an empty `pin_intent` means the fetch-completion arm is a no-op on their test paths.

- [ ] **Step 11: Cargo check (clippy) to catch unused imports / variables**

Run: `cargo clippy -p harmony-app --all-targets -- -D warnings`

Expected: clean. If clippy flags unused `mut` on `pin_intent` (it is mutated in the arms but clippy may be confused) or an unused `use` for `ContentId` inside the completion arm, drop the `mut` or remove the import and retry.

- [ ] **Step 12: Commit**

```bash
git add src-tauri/src/event_loop.rs src-tauri/src/lib.rs src-tauri/tests/content_index_integration.rs
git commit -m "$(cat <<'EOF'
feat(event-loop): fetch-completion replay hook for persisted pin intent (ZEB-155)

Adds pin_intent: HashSet<[u8;32]> to event_loop::run, sourced from the
sidecar in start_node. Pin/Unpin/Burn arms keep it in sync. New
fetch_completion channel carries root CIDs from the spawned fetch task
back to the main loop; the new select arm consults pin_intent and runs
the existing cascade, auto-re-pinning a root once fetch_recursive has
materialized its bundle tree. Integration test fetch_complete_repins_on_intent
covers the full channel path.

Completes ZEB-155. Pin badges now survive restart (display-join, landed
earlier), and refetched roots re-enter their pinned state automatically.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## After all tasks

Run the full test suite one more time to catch any interaction bugs between the four commits:

```bash
cargo test -p harmony-app
cargo clippy -p harmony-app --all-targets -- -D warnings
```

Manual smoke test (not covered by automated tests — UI path):

1. Start the app, ingest a small file via File Manager, click Pin.
2. Restart the app.
3. Open File Manager → the pin badge should still be visible on the file (display-layer OR, Task 2).
4. Burn a different (pinned) file; restart; verify it's gone from the sidecar (no orphaned intent).

Then invoke `superpowers:finishing-a-development-branch` to decide between merge locally / push+PR / keep / discard.
