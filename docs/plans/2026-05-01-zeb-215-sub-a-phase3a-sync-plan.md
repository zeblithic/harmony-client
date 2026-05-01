# ZEB-215 Sub-A Phase 3a Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire Phase 1 crypto + Phase 2 CRDT into a working state-root sync surface — on-disk persistence, Zenoh state-root pub/sub, replay protection, and a `ContentStore` trait abstraction (with `InMemoryStub`) ready for Phase 3b's harmony-content swap.

**Architecture:** Three new modules in `src-tauri/src/`. `content_store.rs` (trait + stub). `owner_state_persist.rs` (atomic-rename CBOR I/O for two new files: `owner_state_crdt.cbor` and `state_root_replay.cbor`, both with 1-byte schema-version prefix). `owner_state_sync.rs` (`SyncEngine` struct + spawned debounce task; takes `mpsc::Sender<Vec<u8>>` outbound + `mpsc::Receiver<Vec<u8>>` inbound channels so it stays Zenoh-agnostic and testable). One Phase-2 module touch: `RootPublishPayload` joins `owner_state_types.rs` and the `impl_canonical!` macro list.

**Tech Stack:** Rust 2024 edition, Tauri 2, tokio (sync, time, rt-multi-thread), zenoh 1, ChaCha20-Poly1305 (via Phase 1's `owner_state_crypto`), BLAKE3, ciborium for CBOR, tempfile for atomic-rename, thiserror for error enums.

**Spec:** [`docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`](../specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md) (commit `d0dcd35`).

**Branch:** `zeb-215-sub-a-phase3a-sync` (already created from `origin/main` at the merge of PR #73, `de17dee`).

**Type-name disambiguation:** Two `OwnerState` types exist in this codebase. Phase 3a always means `crate::owner_state_crdt::OwnerState` (Phase 2's typed CRDT). The legacy `harmony_owner::state::OwnerState` from the external crate is untouched; do not confuse the two. Where ambiguity is possible, use fully-qualified paths.

---

## File Structure

**Created:**
- `src-tauri/src/content_store.rs` — `ContentStore` trait, `ContentStoreError`, `InMemoryStub`. ~150 lines.
- `src-tauri/src/owner_state_persist.rs` — `save_atomically` helper, `CrdtFileV1` / `ReplayFileV1` schema types, `load_crdt` / `save_crdt` / `load_replay` / `save_replay`, `PersistError`, write locks. ~300 lines.
- `src-tauri/src/owner_state_sync.rs` — `SyncEngine` struct + internal task + tests. ~600 lines.

**Modified:**
- `src-tauri/src/owner_state_types.rs` — add `RootPublishPayload` struct + `impl_canonical!(RootPublishPayload)`.
- `src-tauri/src/lib.rs` — `pub mod content_store; pub mod owner_state_persist; pub mod owner_state_sync;` registrations (alphabetical).
- `src-tauri/src/event_loop.rs` — Zenoh adapter wiring `SyncEngine` into the existing session (Task 19).

**Untouched (Phase 1 + Phase 2 invariants stay intact):**
- `src-tauri/src/owner_state_crypto.rs` — Phase 1; consumed read-only.
- `src-tauri/src/owner_state_crdt.rs` — Phase 2; consumed read-only.
- `src-tauri/src/owner_state.rs` — legacy file path; consumed read-only at boot to obtain `master_seed` and `device_signing_key`.

---

## Task 1: Skeleton — module files + lib.rs registration

**Files:**
- Create: `src-tauri/src/content_store.rs`
- Create: `src-tauri/src/owner_state_persist.rs`
- Create: `src-tauri/src/owner_state_sync.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod` declarations alphabetically)

`tempfile`, `blake3`, and `tokio` (with `sync` + `time` features) are already in `src-tauri/Cargo.toml`. No dependency adds are required.

- [ ] **Step 1: Create empty module files**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
```

Create `src-tauri/src/content_store.rs`:

```rust
//! Content-addressed storage trait + in-memory stub (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"ContentStore trait". Phase 3b swaps `InMemoryStub` for the real
//! harmony-content client.
```

Create `src-tauri/src/owner_state_persist.rs`:

```rust
//! On-disk persistence for the Phase-2 OwnerState CRDT and the
//! RootReplayTracker (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"Persistence layer". Two files written via atomic-rename + fsync,
//! each prefixed with a 1-byte schema version.
```

Create `src-tauri/src/owner_state_sync.rs`:

```rust
//! Owner-state SyncEngine — debounced publishes + Zenoh-agnostic
//! channel surface + replay-protected subscriber merge path
//! (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"Architecture". Channel-based; the Zenoh adapter lives in
//! `event_loop.rs` (Task 19).
```

- [ ] **Step 2: Register modules in lib.rs (alphabetical)**

Find the `pub mod` block in `src-tauri/src/lib.rs` (search for `pub mod owner_state_crdt;` to locate the cluster). Add:

```rust
pub mod content_store;
// ... existing modules ...
pub mod owner_state_persist;
pub mod owner_state_sync;
```

Maintain alphabetical order within the cluster.

- [ ] **Step 3: Verify build**

```bash
cargo build --manifest-path src-tauri/Cargo.toml --lib
```

Expected: clean build, no warnings.

- [ ] **Step 4: Run existing tests to verify baseline**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -3
```

Expected: `370 passed; 0 failed` (same count as Phase 2's final state).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/content_store.rs src-tauri/src/owner_state_persist.rs src-tauri/src/owner_state_sync.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 1 — module skeletons

Three empty module files registered in lib.rs:
- content_store — ContentStore trait + InMemoryStub
- owner_state_persist — atomic-rename CBOR I/O for two new files
- owner_state_sync — SyncEngine + debounce task

No behavior yet; tasks 2-20 fill in the public surface.
EOF
)"
```

---

## Task 2: ContentStore trait + InMemoryStub

**Files:**
- Modify: `src-tauri/src/content_store.rs` (full implementation)

- [ ] **Step 1: Write failing tests**

Append to `src-tauri/src/content_store.rs`:

```rust
use crate::owner_state_types::ContentId;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(thiserror::Error, Debug)]
pub enum ContentStoreError {
    #[error("content store I/O: {0}")]
    Io(String),
}

pub trait ContentStore: Send + Sync {
    fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError>;
    fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError>;
}

#[derive(Default)]
pub struct InMemoryStub {
    inner: Mutex<HashMap<ContentId, Vec<u8>>>,
}

impl ContentStore for InMemoryStub {
    fn put(&self, _cid: ContentId, _blob: Vec<u8>) -> Result<(), ContentStoreError> {
        unimplemented!()
    }
    fn get(&self, _cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(byte: u8) -> ContentId {
        ContentId([byte; 32])
    }

    #[test]
    fn put_then_get_returns_blob() {
        let store = InMemoryStub::default();
        store.put(cid(1), vec![10, 20, 30]).unwrap();
        let blob = store.get(&cid(1)).unwrap().expect("blob present");
        assert_eq!(blob, vec![10, 20, 30]);
    }

    #[test]
    fn get_missing_returns_none() {
        let store = InMemoryStub::default();
        assert!(store.get(&cid(99)).unwrap().is_none());
    }

    #[test]
    fn concurrent_puts_all_land() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(InMemoryStub::default());
        let mut handles = vec![];
        for i in 0..50u8 {
            let s = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                s.put(cid(i), vec![i]).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        for i in 0..50u8 {
            let blob = store.get(&cid(i)).unwrap().expect("blob present");
            assert_eq!(blob, vec![i]);
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib content_store 2>&1 | tail -5
```

Expected: 3 tests panic with `not implemented`.

- [ ] **Step 3: Implement put + get**

Replace the `unimplemented!()` bodies in `InMemoryStub`:

```rust
impl ContentStore for InMemoryStub {
    fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        self.inner
            .lock()
            .map_err(|e| ContentStoreError::Io(format!("lock poisoned: {e}")))?
            .insert(cid, blob);
        Ok(())
    }

    fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| ContentStoreError::Io(format!("lock poisoned: {e}")))?
            .get(cid)
            .cloned())
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib content_store 2>&1 | tail -5
```

Expected: 3 tests pass.

- [ ] **Step 5: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/content_store.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 2 — ContentStore trait + InMemoryStub

Trait surface that Phase 3b's real harmony-content client will swap
into. InMemoryStub is HashMap-backed; load-bearing for unit and
integration tests in 3a, useless for actual cross-device sync (its
data is per-process), which is the deliberate Phase 3b deferral.

3 unit tests cover put/get round-trip, missing-cid → None, and
concurrent put correctness.
EOF
)"
```

---

## Task 3: RootPublishPayload type + impl_canonical

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs` (add struct + extend `impl_canonical!` list)

- [ ] **Step 1: Write failing round-trip test**

Append to the test module at the bottom of `src-tauri/src/owner_state_types.rs`:

```rust
    #[test]
    fn root_publish_payload_round_trip() {
        let p = RootPublishPayload {
            root_cid: ContentId([0xAA; 32]),
            at: Hlc {
                wall_ms: 12345,
                logical: 7,
                device_id: "alice".into(),
            },
        };
        let mut bytes = Vec::new();
        into_writer(&p, &mut bytes).unwrap();
        let recovered: RootPublishPayload = from_reader(&bytes[..]).unwrap();
        assert_eq!(p, recovered);
    }
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib root_publish_payload_round_trip 2>&1 | tail -5
```

Expected: compile error (`RootPublishPayload` not defined).

- [ ] **Step 3: Add struct definition**

Insert after the `Hlc` struct block in `owner_state_types.rs` (search for `pub struct Hlc` and place after the closing `}` of its `impl` block):

```rust
/// State-root publish payload (encrypted plaintext on the
/// `harmony/owner/{addr_hex}/state-root-v1` Zenoh topic).
///
/// Wire format: canonical CBOR map with two single-letter-length
/// keys to satisfy `canonical_cbor_encode`'s same-length-keys
/// precondition. See spec §"State-root payload format".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootPublishPayload {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    #[serde(rename = "at")]
    pub at: Hlc,
}
```

- [ ] **Step 4: Add to `impl_canonical!` list**

Find the `impl_canonical!` macro invocations near the bottom of the file (search for `impl_canonical!(Space);`). Add a new line, keeping alphabetical order within the cluster:

```rust
impl_canonical!(RootPublishPayload);
```

- [ ] **Step 5: Run round-trip test to verify it passes**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib root_publish_payload_round_trip 2>&1 | tail -5
```

Expected: 1 test passes.

- [ ] **Step 6: Run the full lib test suite to confirm no Phase-2 regressions**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -3
```

Expected: `371 passed; 0 failed` (370 prior + 1 new).

- [ ] **Step 7: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/owner_state_types.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 3 — RootPublishPayload wire type

Adds the encrypted-payload plaintext structure defined in ZEB-211
§"Wire format": {root_cid: bstr[32], at: HLC}. Two-letter rename
keys ("rc", "at") satisfy canonical_cbor_encode's same-length-keys
precondition. Joins the impl_canonical! macro list as the 16th
sealed wire type.

This is the only Phase-2 module touch in Phase 3a, deliberately
called out in the spec under "Module boundaries".
EOF
)"
```

---

## Task 4: Persist module — `save_atomically` helper

**Files:**
- Modify: `src-tauri/src/owner_state_persist.rs` (add helper + crash-survival test)

- [ ] **Step 1: Write failing test**

Append to `src-tauri/src/owner_state_persist.rs`:

```rust
use std::fs::File;
use std::io::Write;
use std::path::Path;

#[derive(thiserror::Error, Debug)]
pub enum PersistError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("file corrupt (truncated or invalid CBOR)")]
    Corrupt,
    #[error("unknown schema version byte: {0:#x}")]
    UnknownSchemaVersion(u8),
}

/// Atomically replace `path` with `bytes`. Writes to a sibling
/// tempfile, fsyncs, renames into place, then fsyncs the directory
/// entry so the rename itself is durable.
pub fn save_atomically(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    let dir = path.parent().expect("save_atomically: path has no parent");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| PersistError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_atomically_creates_file_with_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        save_atomically(&path, b"hello world").unwrap();
        let read_back = std::fs::read(&path).unwrap();
        assert_eq!(read_back, b"hello world");
    }

    #[test]
    fn save_atomically_replaces_existing_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        save_atomically(&path, b"old").unwrap();
        save_atomically(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn dropped_tempfile_does_not_corrupt_existing_file() {
        // Crash-survival: simulate a save that begins (creates a tempfile)
        // but is dropped before persist. The original file must remain
        // intact.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        save_atomically(&path, b"original").unwrap();

        // Simulate a partial save: create a tempfile, write, but drop
        // without persist (mimics a crash mid-save).
        {
            let mut tmp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
            tmp.write_all(b"partial junk").unwrap();
            // tmp drops here — tempfile auto-deletes
        }

        assert_eq!(std::fs::read(&path).unwrap(), b"original");
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib owner_state_persist::tests 2>&1 | tail -5
```

Expected: 3 tests pass.

- [ ] **Step 3: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/owner_state_persist.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 4 — save_atomically + PersistError

Cross-cutting helper for atomic-rename + fsync. Used by both
owner_state_crdt.cbor and state_root_replay.cbor save paths.
PersistError covers I/O, CBOR encode/decode, schema-version
mismatch, and corruption — exhaustive per spec §"Error handling".

3 unit tests cover the create case, the atomic-replace case, and
the crash-survival property (dropped tempfile leaves the original
file intact).
EOF
)"
```

---

## Task 5: CRDT file load + save round-trip

**Files:**
- Modify: `src-tauri/src/owner_state_persist.rs`

- [ ] **Step 1: Write failing test**

Append inside the existing `mod tests` block:

```rust
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{
        ContentId, DeliveryStatus, Hlc, OutboxEntry, OutboxEntryId, OwnerAddr, ReadMarker, Space,
        SpaceId, SpaceKind, TransportBinding,
    };

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "alice".into(),
        }
    }

    fn sample_state() -> OwnerState {
        let mut s = OwnerState::default();
        let folder = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "Root".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(100),
            updated_at: hlc(100),
        };
        s.spaces.insert(folder.id, folder);
        s.outbox.insert(
            OutboxEntryId([7; 16]),
            OutboxEntry {
                id: OutboxEntryId([7; 16]),
                space_id: SpaceId([1; 16]),
                recipient_owners: vec![OwnerAddr([2; 16])],
                message_cid: ContentId([3; 32]),
                created_at: hlc(100),
                delivered_to: Default::default(),
                delivery_status: DeliveryStatus::Pending,
            },
        );
        s.markers.insert(
            SpaceId([1; 16]),
            ReadMarker {
                space_id: SpaceId([1; 16]),
                last_read_at: hlc(150),
            },
        );
        let _ = (TransportBinding::Reticulum {
            participants: vec![],
        },); // ensure import isn't dead
        s
    }

    #[test]
    fn crdt_round_trip_preserves_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        let original = sample_state();
        save_crdt(&path, &original).unwrap();
        let loaded = load_crdt(&path).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn crdt_load_missing_file_returns_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never_written.cbor");
        let loaded = load_crdt(&path).unwrap();
        assert_eq!(loaded, OwnerState::default());
    }
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib crdt_round_trip 2>&1 | tail -5
```

Expected: compile error — `save_crdt` and `load_crdt` not defined.

- [ ] **Step 3: Implement load + save**

Append to `src-tauri/src/owner_state_persist.rs` (above `mod tests`):

```rust
use crate::owner_state_crdt::OwnerState;
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

const CRDT_FILE_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct CrdtFileV1 {
    spaces: BTreeMap<crate::owner_state_types::SpaceId, crate::owner_state_types::Space>,
    outbox: BTreeMap<
        crate::owner_state_types::OutboxEntryId,
        crate::owner_state_types::OutboxEntry,
    >,
    inbox:
        BTreeMap<crate::owner_state_types::InboxKey, crate::owner_state_types::InboxEntry>,
    markers:
        BTreeMap<crate::owner_state_types::SpaceId, crate::owner_state_types::ReadMarker>,
    tombstones: BTreeSet<crate::owner_state_types::SpaceId>,
}

impl From<&OwnerState> for CrdtFileV1 {
    fn from(s: &OwnerState) -> Self {
        Self {
            spaces: s.spaces.clone(),
            outbox: s.outbox.clone(),
            inbox: s.inbox.clone(),
            markers: s.markers.clone(),
            tombstones: s.tombstones.clone(),
        }
    }
}

impl From<CrdtFileV1> for OwnerState {
    fn from(f: CrdtFileV1) -> Self {
        OwnerState {
            spaces: f.spaces,
            outbox: f.outbox,
            inbox: f.inbox,
            markers: f.markers,
            tombstones: f.tombstones,
        }
    }
}

pub fn save_crdt(path: &Path, state: &OwnerState) -> Result<(), PersistError> {
    let file = CrdtFileV1::from(state);
    let mut bytes = vec![CRDT_FILE_SCHEMA_V1];
    into_writer(&file, &mut bytes).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    save_atomically(path, &bytes)
}

pub fn load_crdt(path: &Path) -> Result<OwnerState, PersistError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OwnerState::default()),
        Err(e) => return Err(e.into()),
    };
    if bytes.is_empty() {
        return Err(PersistError::Corrupt);
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        CRDT_FILE_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: CrdtFileV1 = from_reader(&mut cursor)
                .map_err(|e| PersistError::CborDecode(e.to_string()))?;
            // Reject trailing bytes — defensive against truncation
            // edge cases that decode "successfully" but stop short.
            if (cursor.position() as usize) != payload.len() {
                return Err(PersistError::Corrupt);
            }
            Ok(file.into())
        }
        v => Err(PersistError::UnknownSchemaVersion(v)),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib owner_state_persist 2>&1 | tail -5
```

Expected: previous 3 tests + 2 new = 5 tests pass.

- [ ] **Step 5: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/owner_state_persist.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 5 — CRDT file load + save round-trip

CrdtFileV1 mirrors OwnerState field-for-field; load/save are
1-byte-schema-version-prefixed and use save_atomically. Trailing
bytes after the CBOR payload are rejected as Corrupt (defensive
against truncated-then-padded files). Missing file → empty
OwnerState (first-run case).

2 round-trip tests cover the happy path and the missing-file
branch.
EOF
)"
```

---

## Task 6: Replay tracker file load + save round-trip

**Files:**
- Modify: `src-tauri/src/owner_state_persist.rs`

- [ ] **Step 1: Write failing test**

Append inside `mod tests`:

```rust
    use std::collections::BTreeMap;

    #[test]
    fn replay_round_trip_preserves_tracker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state_root_replay.cbor");
        let mut original: BTreeMap<String, Hlc> = BTreeMap::new();
        original.insert("alice-laptop".into(), hlc(100));
        original.insert("bob-phone".into(), hlc(200));
        save_replay(&path, &original).unwrap();
        let loaded = load_replay(&path).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn replay_load_missing_file_returns_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never_written.cbor");
        let loaded = load_replay(&path).unwrap();
        assert!(loaded.is_empty());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib replay_round_trip 2>&1 | tail -5
```

Expected: compile error — `save_replay`, `load_replay` not defined.

- [ ] **Step 3: Implement load + save**

Append to `owner_state_persist.rs` (above `mod tests`):

```rust
use crate::owner_state_types::Hlc;

const REPLAY_FILE_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct ReplayFileV1(BTreeMap<String, Hlc>);

pub fn save_replay(path: &Path, tracker: &BTreeMap<String, Hlc>) -> Result<(), PersistError> {
    let file = ReplayFileV1(tracker.clone());
    let mut bytes = vec![REPLAY_FILE_SCHEMA_V1];
    into_writer(&file, &mut bytes).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    save_atomically(path, &bytes)
}

pub fn load_replay(path: &Path) -> Result<BTreeMap<String, Hlc>, PersistError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    if bytes.is_empty() {
        return Err(PersistError::Corrupt);
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        REPLAY_FILE_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: ReplayFileV1 = from_reader(&mut cursor)
                .map_err(|e| PersistError::CborDecode(e.to_string()))?;
            if (cursor.position() as usize) != payload.len() {
                return Err(PersistError::Corrupt);
            }
            Ok(file.0)
        }
        v => Err(PersistError::UnknownSchemaVersion(v)),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib owner_state_persist 2>&1 | tail -5
```

Expected: 5 prior + 2 new = 7 tests pass.

- [ ] **Step 5: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/owner_state_persist.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 6 — replay tracker file load + save

ReplayFileV1 wraps a BTreeMap<device_id, Hlc>. Schema-version-byte
prefix matches CrdtFileV1 pattern. Missing file → empty map (a
fresh tracker that accepts any HLC on first publish from a peer).

2 round-trip tests cover the happy path and the missing-file
branch.
EOF
)"
```

---

## Task 7: Persist module — error cases

**Files:**
- Modify: `src-tauri/src/owner_state_persist.rs` (test additions only)

- [ ] **Step 1: Write failing tests**

Append inside `mod tests`:

```rust
    #[test]
    fn crdt_load_unknown_schema_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.cbor");
        // 0xFF is reserved-future; v1 is 0x01.
        std::fs::write(&path, [0xFF_u8, 0x00, 0x01]).unwrap();
        let err = load_crdt(&path).expect_err("should error");
        assert!(matches!(err, PersistError::UnknownSchemaVersion(0xFF)));
    }

    #[test]
    fn crdt_load_truncated_cbor_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.cbor");
        // Schema v1 + arbitrary CBOR-like junk that won't decode.
        std::fs::write(&path, [CRDT_FILE_SCHEMA_V1, 0xA1, 0x66]).unwrap();
        let err = load_crdt(&path).expect_err("should error");
        assert!(matches!(err, PersistError::CborDecode(_) | PersistError::Corrupt));
    }

    #[test]
    fn crdt_load_empty_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.cbor");
        std::fs::write(&path, []).unwrap();
        let err = load_crdt(&path).expect_err("should error");
        assert!(matches!(err, PersistError::Corrupt));
    }

    #[test]
    fn replay_load_unknown_schema_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future_replay.cbor");
        std::fs::write(&path, [0xFE_u8]).unwrap();
        let err = load_replay(&path).expect_err("should error");
        assert!(matches!(err, PersistError::UnknownSchemaVersion(0xFE)));
    }

    #[test]
    fn crdt_load_trailing_bytes_after_valid_cbor_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("with_tail.cbor");
        // Save a valid file, then append a junk byte.
        save_crdt(&path, &OwnerState::default()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0xFF);
        std::fs::write(&path, bytes).unwrap();
        let err = load_crdt(&path).expect_err("should error");
        assert!(matches!(err, PersistError::Corrupt));
    }
```

- [ ] **Step 2: Run tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib owner_state_persist 2>&1 | tail -5
```

Expected: 7 prior + 5 new = 12 tests pass. (No implementation change; the load functions already reject these cases.)

- [ ] **Step 3: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/owner_state_persist.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 7 — persist error case coverage

5 regression tests exercising the spec §"Error handling" branches:
unknown schema version (CRDT + replay), truncated CBOR mid-stream,
empty file, and trailing bytes after valid CBOR. The load functions
already enforce these rejections; tests pin the contract.
EOF
)"
```

---

## Task 8: SyncEngine skeleton

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`

- [ ] **Step 1: Add struct + constructor**

Append to `src-tauri/src/owner_state_sync.rs`:

```rust
use crate::content_store::ContentStore;
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_crypto::KeyTree;
use crate::owner_state_types::Hlc;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

/// Default debounce window between a `notify_dirty` and the
/// resulting state-root publish. See spec §"Architecture" — small
/// enough to feel near-instant to a human, large enough to collapse
/// keystroke-rate mutations.
pub const DEFAULT_DEBOUNCE_MS: u64 = 250;

#[derive(thiserror::Error, Debug)]
pub enum SyncError {
    #[error("content store: {0}")]
    ContentStore(#[from] crate::content_store::ContentStoreError),
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("persist: {0}")]
    Persist(#[from] crate::owner_state_persist::PersistError),
    #[error("transport channel closed")]
    TransportClosed,
}

/// Filesystem paths for both new files; assembled at boot from
/// `resolve_identity_dir()` and the spec's filename constants.
#[derive(Debug, Clone)]
pub struct PersistPaths {
    pub crdt: PathBuf,
    pub replay: PathBuf,
}

/// Owner-state sync engine. Owns a tokio task that runs the
/// debounce timer + publisher + subscriber + persistence flushes.
/// Construction spawns the task; `shutdown().await` stops it
/// cleanly with one final flush.
pub struct SyncEngine {
    notify_dirty: Arc<Notify>,
    flush_now_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    shutdown_tx: mpsc::Sender<tokio::sync::oneshot::Sender<()>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl SyncEngine {
    /// Construct the engine and spawn its internal task.
    ///
    /// `kt` derives the AEAD keys; `device_id` is the local device's
    /// HLC source; `state` and `tracker` are shared with the rest
    /// of the app via the same `Arc<Mutex<_>>`s.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kt: Arc<KeyTree>,
        device_id: String,
        state: Arc<Mutex<OwnerState>>,
        tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
        content_store: Arc<dyn ContentStore>,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
        paths: PersistPaths,
        debounce_ms: u64,
    ) -> Self {
        let notify_dirty = Arc::new(Notify::new());
        let (flush_now_tx, flush_now_rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let task = tokio::spawn(internal_task(InternalCtx {
            kt,
            device_id,
            state,
            tracker,
            content_store,
            publisher_tx,
            subscriber_rx,
            paths,
            debounce: std::time::Duration::from_millis(debounce_ms),
            notify_dirty: Arc::clone(&notify_dirty),
            flush_now_rx,
            shutdown_rx,
        }));

        SyncEngine {
            notify_dirty,
            flush_now_tx,
            shutdown_tx,
            task: Mutex::new(Some(task)),
        }
    }

    /// Hint that local CRDT state has mutated and a debounced
    /// publish should fire after `debounce_ms`. Non-blocking.
    pub fn notify_dirty(&self) {
        self.notify_dirty.notify_one();
    }

    /// Force an immediate publish, bypassing the debounce window.
    /// Returns when the publish has been written to the outbound
    /// channel and any persistence flush has completed.
    pub async fn flush_now(&self) -> Result<(), SyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.flush_now_tx
            .send(resp_tx)
            .await
            .map_err(|_| SyncError::TransportClosed)?;
        resp_rx.await.map_err(|_| SyncError::TransportClosed)?
    }

    /// Stop the internal task, flushing any pending writes first.
    /// Must be called explicitly during graceful shutdown — `Drop`
    /// is best-effort only.
    pub async fn shutdown(&self) {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        if self.shutdown_tx.send(resp_tx).await.is_ok() {
            let _ = resp_rx.await;
        }
        if let Some(handle) = self.task.lock().await.take() {
            let _ = handle.await;
        }
    }
}

struct InternalCtx {
    kt: Arc<KeyTree>,
    device_id: String,
    state: Arc<Mutex<OwnerState>>,
    tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    content_store: Arc<dyn ContentStore>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    subscriber_rx: mpsc::Receiver<Vec<u8>>,
    paths: PersistPaths,
    debounce: std::time::Duration,
    notify_dirty: Arc<Notify>,
    flush_now_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    shutdown_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<()>>,
}

async fn internal_task(_ctx: InternalCtx) {
    // Tasks 9-15 fill in the loop body.
}
```

- [ ] **Step 2: Verify build (no tests yet)**

```bash
cargo build --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -5
```

Expected: clean build, no warnings.

- [ ] **Step 3: Add a smoke test for construction + shutdown**

Append to `owner_state_sync.rs`:

```rust
#[cfg(test)]
mod skeleton_tests {
    use super::*;
    use crate::content_store::InMemoryStub;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[0u8; 32]).expect("kt"))
    }

    #[tokio::test]
    async fn construct_and_shutdown_clean() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            DEFAULT_DEBOUNCE_MS,
        );
        engine.shutdown().await;
        // No assertions beyond "didn't hang or panic."
    }
}
```

- [ ] **Step 4: Run smoke test**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib skeleton_tests 2>&1 | tail -5
```

Expected: 1 test passes.

- [ ] **Step 5: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff. (`#[allow(clippy::too_many_arguments)]` on `new` keeps the constructor explicit; refactor to a builder is out of scope for 3a.)

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 8 — SyncEngine skeleton

Struct + public API surface (notify_dirty / flush_now / shutdown)
+ internal_task entrypoint stub. Constructor spawns the task;
shutdown() stops it cleanly via a oneshot signal.

Channels: publisher_tx (outbound bytes — Zenoh adapter consumes
in event_loop.rs), subscriber_rx (inbound bytes — adapter produces).
SyncEngine never sees the Zenoh Session itself, keeping it
trivially testable with mpsc channels.

Smoke test: construct + shutdown without hanging.
EOF
)"
```

---

## Task 9: Debounce timer + notify_dirty + collapse

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`

- [ ] **Step 1: Write failing tests**

Append to `owner_state_sync.rs`:

```rust
#[cfg(test)]
mod debounce_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use std::time::Duration;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[0u8; 32]).expect("kt"))
    }

    fn paths() -> (tempfile::TempDir, PersistPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        (dir, paths)
    }

    /// One notify_dirty fires exactly one publish after the debounce.
    #[tokio::test]
    async fn single_notify_dirty_fires_one_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            50, // shorter debounce for tests
        );

        engine.notify_dirty();
        // Should fire within ~50ms; allow 500ms slack.
        let bytes = tokio::time::timeout(Duration::from_millis(500), pub_rx.recv())
            .await
            .expect("publish within timeout")
            .expect("not closed");
        assert!(!bytes.is_empty(), "publish bytes should be non-empty");
        engine.shutdown().await;
    }

    /// 50 rapid notify_dirty calls within one debounce window
    /// collapse to exactly one publish.
    #[tokio::test]
    async fn rapid_notify_dirty_collapses_to_one_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(64);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            100, // 100ms debounce
        );

        for _ in 0..50 {
            engine.notify_dirty();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Wait long enough for the debounce to fire.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Drain channel and count publishes.
        let mut count = 0;
        while let Ok(Some(_)) =
            tokio::time::timeout(Duration::from_millis(50), pub_rx.recv()).await
        {
            count += 1;
        }
        assert_eq!(count, 1, "expected exactly one publish, got {}", count);
        engine.shutdown().await;
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib debounce_tests 2>&1 | tail -10
```

Expected: tests time out or hang because `internal_task` is still a no-op stub.

- [ ] **Step 3: Implement the debounce loop body**

Replace the `internal_task` function in `owner_state_sync.rs`:

```rust
async fn internal_task(mut ctx: InternalCtx) {
    use std::time::Instant;

    let mut next_wakeup: Option<Instant> = None;

    loop {
        // Compute the sleep duration for the wakeup branch.
        let sleep_dur = next_wakeup
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(std::time::Duration::from_secs(3600));

        tokio::select! {
            _ = ctx.notify_dirty.notified() => {
                if next_wakeup.is_none() {
                    next_wakeup = Some(Instant::now() + ctx.debounce);
                }
                // If a wakeup is already scheduled, additional dirty
                // signals collapse into the same wakeup.
            }
            _ = tokio::time::sleep(sleep_dur), if next_wakeup.is_some() => {
                next_wakeup = None;
                if let Err(e) = publish_root_now(&ctx).await {
                    tracing::warn!(error = %e, "publish_root_now failed");
                }
            }
            Some(resp_tx) = ctx.flush_now_rx.recv() => {
                next_wakeup = None;
                let result = publish_root_now(&ctx).await;
                let _ = resp_tx.send(result);
            }
            Some(_bytes) = ctx.subscriber_rx.recv() => {
                // Tasks 13-15 fill in receive handling.
            }
            Some(resp_tx) = ctx.shutdown_rx.recv() => {
                if next_wakeup.is_some() {
                    let _ = publish_root_now(&ctx).await;
                }
                let _ = resp_tx.send(());
                return;
            }
        }
    }
}

/// Publish a state-root snapshot. Tasks 12 fills in real encryption +
/// CAS put + Zenoh send; for now this writes a placeholder byte
/// sequence to publisher_tx so the debounce tests have something
/// observable.
async fn publish_root_now(ctx: &InternalCtx) -> Result<(), SyncError> {
    // Placeholder for Task 9. Task 12 replaces this with the real
    // encrypt → put → publish pipeline.
    ctx.publisher_tx
        .send(b"placeholder".to_vec())
        .await
        .map_err(|_| SyncError::TransportClosed)?;
    Ok(())
}
```

- [ ] **Step 4: Run debounce tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib debounce_tests 2>&1 | tail -5
```

Expected: 2 tests pass.

- [ ] **Step 5: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 9 — debounce timer + collapse

Internal tokio::select! loop with five branches: notify_dirty
schedules a wakeup, the wakeup fires publish_root_now, flush_now
fires immediately and acks via oneshot, subscriber bytes route
to receive-handling (filled by tasks 13-15), shutdown drains
pending publishes and exits.

publish_root_now is a placeholder writing b"placeholder" to
publisher_tx — task 12 replaces it with the real encrypt → put
→ publish pipeline. The debounce/collapse semantics are correct
and pinned by 2 tests:
- one notify_dirty → exactly one publish after debounce
- 50 rapid notify_dirty calls in one window → exactly one publish
EOF
)"
```

---

## Task 10: flush_now bypasses debounce

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (test additions)

- [ ] **Step 1: Write failing tests**

Append inside `mod debounce_tests`:

```rust
    #[tokio::test]
    async fn flush_now_fires_immediately() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000, // very long debounce — flush_now must beat it
        );

        engine.flush_now().await.unwrap();
        // Must fire within ~50ms — well below the 5000ms debounce.
        let bytes = tokio::time::timeout(Duration::from_millis(200), pub_rx.recv())
            .await
            .expect("publish within timeout")
            .expect("not closed");
        assert!(!bytes.is_empty());
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn flush_now_cancels_pending_wakeup() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            200,
        );

        engine.notify_dirty();
        // Don't wait for the debounce — call flush_now immediately.
        engine.flush_now().await.unwrap();
        // Drain — should see exactly one publish (flush_now's), not two.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut count = 0;
        while let Ok(Some(_)) =
            tokio::time::timeout(Duration::from_millis(50), pub_rx.recv()).await
        {
            count += 1;
        }
        assert_eq!(count, 1, "flush_now should cancel pending wakeup");
        engine.shutdown().await;
    }
```

- [ ] **Step 2: Run tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib debounce_tests 2>&1 | tail -5
```

Expected: 4 tests pass (2 prior + 2 new). The implementation already handles flush_now via the `flush_now_rx` branch which clears `next_wakeup`.

- [ ] **Step 3: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 10 — flush_now bypasses debounce

2 regression tests pin the contract:
- flush_now beats a 5000ms debounce window (fires immediately)
- flush_now after notify_dirty cancels the pending wakeup so we
  see exactly one publish, not two

Implementation already correct from Task 9; tests pin the
behavior.
EOF
)"
```

---

## Task 11: Shutdown flushes pending writes

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (test additions)

- [ ] **Step 1: Write failing test**

Append inside `mod debounce_tests`:

```rust
    #[tokio::test]
    async fn shutdown_flushes_pending_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000, // long debounce — shutdown must short-circuit it
        );

        engine.notify_dirty();
        engine.shutdown().await;
        // After shutdown, the pending publish must already have fired.
        let bytes = pub_rx.try_recv().expect("pending publish flushed");
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn shutdown_without_pending_writes_does_not_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        engine.shutdown().await;
        // No notify_dirty was called, so nothing to flush.
        assert!(pub_rx.try_recv().is_err());
    }
```

- [ ] **Step 2: Run tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib debounce_tests 2>&1 | tail -5
```

Expected: 6 tests pass. Implementation already handles this in the `shutdown_rx` branch.

- [ ] **Step 3: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 11 — shutdown flushes pending writes

2 tests pin the shutdown contract:
- shutdown after notify_dirty (pending) flushes one final publish
  before exiting — even with a 5s debounce window
- shutdown with no pending work doesn't emit a spurious publish

Implementation already correct from Task 9.
EOF
)"
```

---

## Task 12: Real publisher pipeline (encrypt → CAS put → root publish)

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`

- [ ] **Step 1: Write failing test**

Append to `owner_state_sync.rs`:

```rust
#[cfg(test)]
mod publisher_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_crypto::decrypt_root_publish;
    use crate::owner_state_types::RootPublishPayload;
    use ciborium::from_reader;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[42u8; 32]).expect("kt"))
    }

    fn paths() -> (tempfile::TempDir, PersistPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        (dir, paths)
    }

    #[tokio::test]
    async fn publish_emits_decryptable_payload_with_blob_in_store() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default());
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "alice-device".into(),
            Arc::clone(&state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&store) as Arc<dyn ContentStore>,
            pub_tx,
            sub_rx,
            paths,
            50,
        );

        engine.notify_dirty();
        let wire = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            pub_rx.recv(),
        )
        .await
        .expect("publish within timeout")
        .expect("channel open");

        // Decrypt the wire payload with Phase-1 helper.
        let payload_bytes = decrypt_root_publish(&kt, &wire).expect("decrypt");
        let payload: RootPublishPayload =
            from_reader(&payload_bytes[..]).expect("CBOR decode");
        assert_eq!(payload.at.device_id, "alice-device");

        // The root_cid must reference a blob present in the stub.
        let blob = store
            .get(&payload.root_cid)
            .unwrap()
            .expect("blob present");
        assert!(!blob.is_empty());

        engine.shutdown().await;
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib publish_emits_decryptable 2>&1 | tail -10
```

Expected: test fails — placeholder publisher writes `b"placeholder"`, not a real encrypted payload.

- [ ] **Step 3: Implement real publisher pipeline**

Replace the `publish_root_now` function in `owner_state_sync.rs`:

```rust
use crate::owner_state_crypto::{
    canonical_cbor_encode, encrypt_entry, encrypt_root_publish, space_lookup_key,
};
use crate::owner_state_types::{ContentId, RootPublishPayload};

/// Lookup-key tag for the single-blob OwnerState in 3a's
/// simplified CAS layout. See spec §"Root blob shape — Phase 3a
/// simplification". Phase 3b/c restructures into per-entry blobs.
const OWNER_STATE_ROOT_BLOB_TAG: &[u8] = b"owner-state-root-blob-v1";

async fn publish_root_now(ctx: &InternalCtx) -> Result<(), SyncError> {
    // Snapshot CRDT state under brief lock.
    let snapshot = {
        let state = ctx.state.lock().await;
        state.clone()
    };

    // 1. Canonical-CBOR encode the OwnerState as the cleartext "root blob."
    let blob_cleartext = canonical_cbor_encode(&snapshot)
        .map_err(|e| SyncError::CborEncode(e.to_string()))?;

    // 2. Encrypt with deterministic per-entry AEAD using the fixed
    //    owner-state-root lookup key, so cipher_cid is reproducible
    //    across two devices encrypting the same state.
    let lookup = space_lookup_key(&ctx.kt, OWNER_STATE_ROOT_BLOB_TAG);
    let blob_ciphertext = encrypt_entry(&ctx.kt, &lookup, &blob_cleartext)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;

    // 3. cipher_cid = BLAKE3 of the encrypted blob.
    let root_cid = ContentId(blake3::hash(&blob_ciphertext).into());

    // 4. Put into ContentStore (in 3a: InMemoryStub; 3b: real CAS).
    ctx.content_store.put(root_cid, blob_ciphertext)?;

    // 5. Build state-root payload.
    let now = next_hlc(ctx).await;
    let payload = RootPublishPayload {
        root_cid,
        at: now,
    };
    let payload_bytes = canonical_cbor_encode(&payload)
        .map_err(|e| SyncError::CborEncode(e.to_string()))?;

    // 6. Encrypt with random-nonce root AEAD (Phase 1).
    let wire = encrypt_root_publish(&ctx.kt, &payload_bytes)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;

    // 7. Send onto outbound channel — Zenoh adapter forwards.
    ctx.publisher_tx
        .send(wire)
        .await
        .map_err(|_| SyncError::TransportClosed)?;

    Ok(())
}

/// Build a strictly-newer HLC than the last one we published. The
/// internal task is single-threaded so we don't need atomic ops;
/// caller holds an `&mut self` to the task's local state in a real
/// design, but for now we re-derive from system time + a per-task
/// monotonic counter cached in `ctx.tracker` keyed by our own
/// device_id.
async fn next_hlc(ctx: &InternalCtx) -> Hlc {
    use std::time::{SystemTime, UNIX_EPOCH};
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut tracker = ctx.tracker.lock().await;
    let logical = match tracker.get(&ctx.device_id) {
        Some(prev) if prev.wall_ms == wall_ms => prev.logical + 1,
        Some(prev) if prev.wall_ms > wall_ms => prev.logical + 1, // wall non-monotonic
        _ => 0,
    };
    let prev_wall = tracker.get(&ctx.device_id).map(|p| p.wall_ms).unwrap_or(0);
    let effective_wall = std::cmp::max(wall_ms, prev_wall);

    let now = Hlc {
        wall_ms: effective_wall,
        logical,
        device_id: ctx.device_id.clone(),
    };
    tracker.insert(ctx.device_id.clone(), now.clone());
    now
}
```

- [ ] **Step 4: Run publisher test to verify it passes**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib publish_emits_decryptable 2>&1 | tail -5
```

Expected: 1 test passes.

- [ ] **Step 5: Run all SyncEngine tests to confirm no regressions**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib owner_state_sync 2>&1 | tail -5
```

Expected: all prior tests still pass (the placeholder bytes were never inspected; only "non-empty" was asserted).

- [ ] **Step 6: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 12 — real publisher pipeline

publish_root_now wires Phase 1 + Phase 2 + ContentStore end-to-end:
1. Snapshot OwnerState under brief lock
2. canonical_cbor_encode (Phase 1's sealed-trait helper)
3. encrypt_entry with fixed owner-state-root lookup key
   (deterministic — two devices encrypting the same state get the
   same cipher_cid)
4. BLAKE3 → root_cid
5. ContentStore.put (InMemoryStub for 3a)
6. RootPublishPayload { root_cid, at: now } encoded
7. encrypt_root_publish (random nonce, Phase 1)
8. publisher_tx.send → Zenoh adapter

next_hlc bumps logical on equal/non-monotonic wall_ms; resets
otherwise. Tracker is keyed by our own device_id for the
self-published HLC, sharing the same map peers occupy on the
subscriber side.

Test decrypts the wire payload, parses the CBOR, looks up the
root_cid in the stub, and asserts the blob is present and the
publisher's device_id round-trips.
EOF
)"
```

---

## Task 13: Subscriber pipeline — receive + decrypt + replay check

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`

- [ ] **Step 1: Write failing tests**

Append to `owner_state_sync.rs`:

```rust
#[cfg(test)]
mod subscriber_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_crypto::{
        canonical_cbor_encode, encrypt_entry, encrypt_root_publish, space_lookup_key,
    };
    use crate::owner_state_types::RootPublishPayload;
    use std::time::Duration;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[7u8; 32]).expect("kt"))
    }

    fn paths() -> (tempfile::TempDir, PersistPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        (dir, paths)
    }

    /// Build a wire payload for testing — re-uses the publisher's
    /// encryption path but with a controlled HLC.
    fn make_wire(
        kt: &Arc<KeyTree>,
        store: &Arc<dyn ContentStore>,
        state: &OwnerState,
        device_id: &str,
        wall_ms: u64,
        logical: u32,
    ) -> Vec<u8> {
        let blob_cleartext = canonical_cbor_encode(state).unwrap();
        let lookup = space_lookup_key(kt, b"owner-state-root-blob-v1");
        let blob_ciphertext = encrypt_entry(kt, &lookup, &blob_cleartext).unwrap();
        let root_cid = ContentId(blake3::hash(&blob_ciphertext).into());
        store.put(root_cid, blob_ciphertext).unwrap();
        let payload = RootPublishPayload {
            root_cid,
            at: Hlc {
                wall_ms,
                logical,
                device_id: device_id.into(),
            },
        };
        let payload_bytes = canonical_cbor_encode(&payload).unwrap();
        encrypt_root_publish(kt, &payload_bytes).unwrap()
    }

    #[tokio::test]
    async fn subscriber_accepts_strictly_newer_hlc_and_updates_tracker() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&state),
            Arc::clone(&tracker),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000, // long debounce — keep self-publishes out of the way
        );

        let wire = make_wire(&kt, &store, &OwnerState::default(), "peer-bob", 1000, 0);
        sub_tx.send(wire).await.unwrap();
        // Give the subscriber branch a moment to process.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let t = tracker.lock().await;
        let stored = t.get("peer-bob").expect("peer accepted");
        assert_eq!(stored.wall_ms, 1000);
        assert_eq!(stored.logical, 0);
        drop(t);

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn subscriber_rejects_strictly_older_hlc() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&state),
            Arc::clone(&tracker),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        // First publish: at=2000.
        sub_tx
            .send(make_wire(
                &kt,
                &store,
                &OwnerState::default(),
                "peer-bob",
                2000,
                0,
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Replay: at=1000 (older). Tracker must NOT regress.
        sub_tx
            .send(make_wire(
                &kt,
                &store,
                &OwnerState::default(),
                "peer-bob",
                1000,
                0,
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let t = tracker.lock().await;
        let stored = t.get("peer-bob").expect("still present");
        assert_eq!(stored.wall_ms, 2000, "tracker must not regress");
        drop(t);

        engine.shutdown().await;
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib subscriber_tests 2>&1 | tail -5
```

Expected: 2 tests fail — subscriber branch is still a no-op.

- [ ] **Step 3: Implement the subscriber branch**

Replace the `Some(_bytes) = ctx.subscriber_rx.recv()` arm in `internal_task`:

```rust
            Some(bytes) = ctx.subscriber_rx.recv() => {
                if let Err(e) = handle_incoming_publish(&mut ctx, bytes).await {
                    tracing::warn!(error = %e, "incoming publish dropped");
                }
            }
```

Note the `ctx` borrow becomes `&mut ctx` because `handle_incoming_publish` may need mutable access. Update the `internal_task` signature:

```rust
async fn internal_task(mut ctx: InternalCtx) {
```

(Already `mut`. Confirm.)

Append the helper function:

```rust
use crate::owner_state_crypto::{canonical_cbor_decode, decrypt_entry, decrypt_root_publish};

async fn handle_incoming_publish(
    ctx: &mut InternalCtx,
    wire: Vec<u8>,
) -> Result<(), SyncError> {
    // 1. Decrypt the Zenoh wire payload.
    let payload_bytes = decrypt_root_publish(&ctx.kt, &wire)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;
    let payload: RootPublishPayload = canonical_cbor_decode(&payload_bytes)
        .map_err(|e| SyncError::CborDecode(e.to_string()))?;

    // 2. Replay protection.
    {
        let mut tracker = ctx.tracker.lock().await;
        let accept = match tracker.get(&payload.at.device_id) {
            None => true,
            Some(existing) => payload.at.is_strictly_newer_than(existing),
        };
        if !accept {
            return Ok(());
        }
        tracker.insert(payload.at.device_id.clone(), payload.at.clone());
    }

    // 3. Fetch + merge — Tasks 14-15 fill in.
    let _root_cid = payload.root_cid;
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib subscriber_tests 2>&1 | tail -5
```

Expected: 2 tests pass.

- [ ] **Step 5: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 13 — subscriber receive + decrypt + replay check

handle_incoming_publish:
1. decrypt_root_publish (Phase 1)
2. canonical_cbor_decode → RootPublishPayload
3. Replay protection: accept iff strictly newer than tracker's
   last-accepted at for the same publisher device_id
4. Update tracker on accept

Tasks 14-15 fill in the fetch + merge step. Two tests pin the
replay-protection contract: strictly newer accepted, strictly
older rejected without tracker regression.
EOF
)"
```

---

## Task 14: Subscriber pipeline — fetch + merge

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`

- [ ] **Step 1: Write failing test (E2E single-process convergence)**

Append inside `mod subscriber_tests`:

```rust
    use crate::owner_state_types::{
        ContentId, DeliveryStatus, OutboxEntry, OutboxEntryId, OwnerAddr, ReadMarker, Space,
        SpaceId, SpaceKind,
    };

    fn folder(id: u8, ts: u64) -> Space {
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "F".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
            updated_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
        }
    }

    #[tokio::test]
    async fn subscriber_fetches_and_merges_remote_state() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let local_state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&local_state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        // Build a remote OwnerState containing a folder id=42.
        let mut remote = OwnerState::default();
        remote.spaces.insert(SpaceId([42; 16]), folder(42, 100));

        let wire = make_wire(&kt, &store, &remote, "peer-bob", 1000, 0);
        sub_tx.send(wire).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let local = local_state.lock().await;
        assert!(
            local.spaces.contains_key(&SpaceId([42; 16])),
            "remote folder must merge into local"
        );
        drop(local);

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn subscriber_merges_outbox_inbox_marker_entries() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let local_state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&local_state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        let mut remote = OwnerState::default();
        // Need a Space first so the outbox/inbox can reference it.
        remote.spaces.insert(SpaceId([1; 16]), folder(1, 100));
        remote.outbox.insert(
            OutboxEntryId([7; 16]),
            OutboxEntry {
                id: OutboxEntryId([7; 16]),
                space_id: SpaceId([1; 16]),
                recipient_owners: vec![OwnerAddr([2; 16])],
                message_cid: ContentId([3; 32]),
                created_at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "peer".into(),
                },
                delivered_to: Default::default(),
                delivery_status: DeliveryStatus::Pending,
            },
        );
        remote.markers.insert(
            SpaceId([1; 16]),
            ReadMarker {
                space_id: SpaceId([1; 16]),
                last_read_at: Hlc {
                    wall_ms: 200,
                    logical: 0,
                    device_id: "peer".into(),
                },
            },
        );

        let wire = make_wire(&kt, &store, &remote, "peer-bob", 1000, 0);
        sub_tx.send(wire).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let local = local_state.lock().await;
        assert!(local.spaces.contains_key(&SpaceId([1; 16])));
        assert!(local.outbox.contains_key(&OutboxEntryId([7; 16])));
        assert!(local.markers.contains_key(&SpaceId([1; 16])));
        drop(local);

        engine.shutdown().await;
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib subscriber_tests::subscriber_fetches_and_merges 2>&1 | tail -5
```

Expected: tests fail — local state stays empty (the merge step is still a TODO).

- [ ] **Step 3: Implement fetch + merge**

Replace the placeholder block at the end of `handle_incoming_publish`:

```rust
    // 3. Fetch the encrypted root blob from CAS.
    let blob_ciphertext = ctx
        .content_store
        .get(&payload.root_cid)?
        .ok_or_else(|| {
            // Phase 3b will replace InMemoryStub with real CAS; for
            // 3a, a missing blob means the subscriber and publisher
            // aren't sharing the same stub (e.g. cross-process). Log
            // and skip — never panic.
            SyncError::Crypto("ContentStore returned None for root_cid".into())
        })?;

    // 4. Decrypt with the same lookup key the publisher used.
    let lookup = space_lookup_key(&ctx.kt, OWNER_STATE_ROOT_BLOB_TAG);
    let blob_cleartext = decrypt_entry(&ctx.kt, &lookup, &blob_ciphertext)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;

    // 5. Decode into a remote OwnerState snapshot.
    let remote: OwnerState = canonical_cbor_decode(&blob_cleartext)
        .map_err(|e| SyncError::CborDecode(e.to_string()))?;

    // 6. Merge each entry through Phase 2's CRDT methods. Order
    //    matters slightly — Spaces must merge first because outbox/
    //    inbox/markers reference SpaceIds that the canonicalization
    //    rewrite needs to see resolved.
    {
        let mut local = ctx.state.lock().await;
        for (_, space) in remote.spaces {
            local.apply_space_with_canonicalization(space);
        }
        for (_, entry) in remote.outbox {
            local.apply_outbox(entry);
        }
        for (_, entry) in remote.inbox {
            local.apply_inbox(entry);
        }
        for (_, marker) in remote.markers {
            local.apply_marker(marker);
        }
        for tomb in remote.tombstones {
            local.tombstones.insert(tomb);
        }
    }

    Ok(())
}
```

- [ ] **Step 4: Resolve module visibility for `apply_space_with_canonicalization`**

Phase 2 made `apply_space` `pub(crate)`. Confirm `apply_space_with_canonicalization` is `pub(crate)` (or `pub`) and reachable from `owner_state_sync`:

```bash
grep -n "pub.*fn apply_space_with_canonicalization\|pub fn apply_outbox\|pub fn apply_inbox\|pub fn apply_marker" src-tauri/src/owner_state_crdt.rs
```

Expected: at least `pub(crate) fn apply_space_with_canonicalization` and `pub fn apply_outbox/inbox/marker`. If `apply_space_with_canonicalization` is fully private, escalate visibility to `pub(crate)` (it must be — both modules live in the same crate).

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib subscriber_tests 2>&1 | tail -5
```

Expected: 4 tests pass (2 prior + 2 new).

- [ ] **Step 6: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs src-tauri/src/owner_state_crdt.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 14 — subscriber fetch + merge

handle_incoming_publish now:
1. ContentStore.get(root_cid) → encrypted blob (None → log + skip)
2. decrypt_entry with the same fixed lookup key the publisher used
3. canonical_cbor_decode into a remote OwnerState
4. Iterate every entry through Phase 2's apply_* methods:
   - apply_space_with_canonicalization first (canonicalizes any
     dependent records before outbox/inbox/markers reference them)
   - apply_outbox / apply_inbox / apply_marker
   - tombstones inserted directly

Idempotency from Phase 2's CRDT properties means re-receiving our
own publish (replay tracker missed) just no-ops everywhere.

2 tests pin the merge contract: a remote Space appears locally;
remote outbox/inbox/marker entries all merge.
EOF
)"
```

---

## Task 15: Missing-blob handling

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (test additions)

- [ ] **Step 1: Write failing test**

Append inside `mod subscriber_tests`:

```rust
    #[tokio::test]
    async fn subscriber_logs_and_skips_when_blob_missing() {
        // Build a wire payload but DON'T put the blob in the store —
        // simulate cross-process / cross-device case where the
        // publisher and subscriber don't share their stubs.
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store_publisher = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let store_subscriber = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let local_state = Arc::new(Mutex::new(OwnerState::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&local_state),
            Arc::clone(&tracker),
            Arc::clone(&store_subscriber), // subscriber's stub is empty
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        let mut remote = OwnerState::default();
        remote.spaces.insert(SpaceId([42; 16]), folder(42, 100));

        // Publisher puts the blob in its OWN stub; subscriber's
        // stub never receives it.
        let wire = make_wire(&kt, &store_publisher, &remote, "peer-bob", 1000, 0);
        sub_tx.send(wire).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Subscriber must NOT have merged — local stays empty.
        let local = local_state.lock().await;
        assert!(
            local.spaces.is_empty(),
            "subscriber should have skipped the merge for missing blob"
        );
        drop(local);

        // BUT replay tracker should still have advanced — we accepted
        // the publish, just couldn't fetch the data. That's OK because
        // the next publish from the same peer will carry a newer HLC
        // and a new (hopefully present) root_cid.
        let t = tracker.lock().await;
        assert!(t.contains_key("peer-bob"), "tracker must still record");
        drop(t);

        engine.shutdown().await;
    }
```

- [ ] **Step 2: Run test to verify the contract**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib subscriber_logs_and_skips 2>&1 | tail -5
```

Expected: test passes immediately. The subscriber's `?` on `content_store.get` falls into the `Err` branch from `ok_or_else`, the error is logged (via `tracing::warn!` in the outer `internal_task` arm), and the merge is skipped. Tracker was already advanced before the fetch attempt.

If the test fails because the tracker was advanced AFTER the merge: rearrange `handle_incoming_publish` so the tracker insert happens before the CAS fetch. (Inspection of the Task 13 implementation shows it already happens before — verify.)

- [ ] **Step 3: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 15 — missing-blob handling

When ContentStore.get returns None, handle_incoming_publish logs
+ skips without panicking. The replay tracker is updated *before*
the CAS fetch so the missing-blob case doesn't leave the tracker
out of sync — a later publish from the same peer will advance
naturally with a newer HLC.

Pins the contract for Phase 3b: when real harmony-content arrives,
this same code path will retry-then-skip, which is the correct
fallback behavior for a CAS that may legitimately not yet have a
blob a peer just published.
EOF
)"
```

---

## Task 16: Replay tracker persistence across restart

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`

- [ ] **Step 1: Wire persistence into the internal task**

Edit `internal_task` in `owner_state_sync.rs` to flush both files on every successful publish/merge AND on shutdown.

Add a helper near the top of the impl section:

```rust
async fn persist_both(
    state: &Arc<Mutex<OwnerState>>,
    tracker: &Arc<Mutex<BTreeMap<String, Hlc>>>,
    paths: &PersistPaths,
) -> Result<(), SyncError> {
    let state_snap = state.lock().await.clone();
    let tracker_snap = tracker.lock().await.clone();
    crate::owner_state_persist::save_crdt(&paths.crdt, &state_snap)?;
    crate::owner_state_persist::save_replay(&paths.replay, &tracker_snap)?;
    Ok(())
}
```

In `internal_task`, call this from three places: the wakeup branch (after publish), the flush_now branch (after publish), and the shutdown branch (last action before exit). Update each to:

```rust
            _ = tokio::time::sleep(sleep_dur), if next_wakeup.is_some() => {
                next_wakeup = None;
                if let Err(e) = publish_root_now(&ctx).await {
                    tracing::warn!(error = %e, "publish_root_now failed");
                }
                if let Err(e) = persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await {
                    tracing::warn!(error = %e, "persist_both failed");
                }
            }
            Some(resp_tx) = ctx.flush_now_rx.recv() => {
                next_wakeup = None;
                let pub_result = publish_root_now(&ctx).await;
                let persist_result = persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await;
                let result = pub_result.and(persist_result);
                let _ = resp_tx.send(result);
            }
            Some(bytes) = ctx.subscriber_rx.recv() => {
                if let Err(e) = handle_incoming_publish(&mut ctx, bytes).await {
                    tracing::warn!(error = %e, "incoming publish dropped");
                }
                if let Err(e) = persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await {
                    tracing::warn!(error = %e, "persist_both failed");
                }
            }
            Some(resp_tx) = ctx.shutdown_rx.recv() => {
                if next_wakeup.is_some() {
                    let _ = publish_root_now(&ctx).await;
                }
                let _ = persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await;
                let _ = resp_tx.send(());
                return;
            }
```

- [ ] **Step 2: Write failing test**

Append inside `mod subscriber_tests`:

```rust
    #[tokio::test]
    async fn replay_tracker_survives_engine_restart() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;

        // Round 1: bring up engine, accept a publish, shut down.
        {
            let tracker = Arc::new(Mutex::new(BTreeMap::new()));
            let state = Arc::new(Mutex::new(OwnerState::default()));
            let engine = SyncEngine::new(
                Arc::clone(&kt),
                "self-device".into(),
                Arc::clone(&state),
                Arc::clone(&tracker),
                Arc::clone(&store),
                pub_tx.clone(),
                sub_rx,
                paths.clone(),
                5000,
            );
            sub_tx
                .send(make_wire(
                    &kt,
                    &store,
                    &OwnerState::default(),
                    "peer-bob",
                    5000,
                    0,
                ))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            engine.shutdown().await;
        }

        // Round 2: boot a fresh engine, load tracker from disk,
        // verify peer-bob's HLC is 5000. Then send an OLDER publish
        // and confirm rejection.
        let tracker_loaded = crate::owner_state_persist::load_replay(&paths.replay).unwrap();
        assert_eq!(tracker_loaded.get("peer-bob").unwrap().wall_ms, 5000);

        let (_pub_tx2, _pub_rx2) = mpsc::channel(16);
        let (sub_tx2, sub_rx2) = mpsc::channel(16);
        let tracker2 = Arc::new(Mutex::new(tracker_loaded));
        let state2 = Arc::new(Mutex::new(OwnerState::default()));
        let engine2 = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&state2),
            Arc::clone(&tracker2),
            Arc::clone(&store),
            _pub_tx2,
            sub_rx2,
            paths.clone(),
            5000,
        );
        // Send an older publish: at=2000 < 5000.
        sub_tx2
            .send(make_wire(
                &kt,
                &store,
                &OwnerState::default(),
                "peer-bob",
                2000,
                0,
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let t = tracker2.lock().await;
        assert_eq!(
            t.get("peer-bob").unwrap().wall_ms,
            5000,
            "replay tracker must reject the older HLC across restart"
        );
        drop(t);

        engine2.shutdown().await;
    }
```

- [ ] **Step 3: Run test to verify**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib replay_tracker_survives 2>&1 | tail -5
```

Expected: 1 test passes.

- [ ] **Step 4: Run all owner_state_sync tests to confirm no regressions**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib owner_state_sync 2>&1 | tail -5
```

Expected: all sync tests still pass (file writes are an addition, not a behavior change).

- [ ] **Step 5: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 16 — replay tracker persistence

persist_both helper writes owner_state_crdt.cbor + state_root_replay.cbor
on every successful publish, every successful merge, every flush_now,
and the final shutdown drain. Both files share the same atomic-rename
pattern from Task 4.

Test simulates an engine restart: round 1 accepts a publish at HLC=5000,
shuts down, persists. Round 2 reads the tracker from disk, brings up a
fresh engine with that tracker, sends an older publish at HLC=2000,
confirms it's rejected (tracker stays at 5000). This is the spec's
required cross-reboot replay protection guarantee.
EOF
)"
```

---

## Task 17: Integration tests — bidirectional convergence + cross-device dedupe

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`

- [ ] **Step 1: Write tests using two SyncEngines + one shared stub**

Append a new test module to `owner_state_sync.rs`:

```rust
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_types::{OwnerAddr, Space, SpaceId, SpaceKind, TransportBinding};
    use std::time::Duration;

    fn make_kt(seed: u8) -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[seed; 32]).expect("kt"))
    }

    fn paths(name: &str, dir: &tempfile::TempDir) -> PersistPaths {
        PersistPaths {
            crdt: dir.path().join(format!("{}_crdt.cbor", name)),
            replay: dir.path().join(format!("{}_replay.cbor", name)),
        }
    }

    fn dm(id: u8, members: Vec<u8>, ts: u64) -> Space {
        let mut sorted = members.clone();
        sorted.sort();
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: sorted.into_iter().map(|i| OwnerAddr([i; 16])).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
            updated_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
        }
    }

    /// Two SyncEngines share one InMemoryStub. A's publish flows to B
    /// via the cross-wired channels. Senders are stored to keep the
    /// forwarding tasks alive; they're not used directly so the
    /// `_`-prefix silences dead-code warnings.
    struct TwoDevices {
        a_engine: SyncEngine,
        b_engine: SyncEngine,
        a_state: Arc<Mutex<OwnerState>>,
        b_state: Arc<Mutex<OwnerState>>,
        _a_to_b_tx: mpsc::Sender<Vec<u8>>,
        _b_to_a_tx: mpsc::Sender<Vec<u8>>,
        _dir: tempfile::TempDir,
    }

    fn spawn_two_devices(kt_seed: u8) -> TwoDevices {
        let dir = tempfile::tempdir().unwrap();
        let kt = make_kt(kt_seed);
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let a_state = Arc::new(Mutex::new(OwnerState::default()));
        let b_state = Arc::new(Mutex::new(OwnerState::default()));
        let a_tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let b_tracker = Arc::new(Mutex::new(BTreeMap::new()));

        // A publishes → forwards into B's subscriber.
        let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (a_to_b_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        // Forwarding task: drain A's outbox into B's inbox.
        let a_to_b_forwarder = a_to_b_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = a_pub_rx.recv().await {
                let _ = a_to_b_forwarder.send(bytes).await;
            }
        });

        // B publishes → forwards into A's subscriber.
        let (b_pub_tx, mut b_pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (b_to_a_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        let b_to_a_forwarder = b_to_a_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = b_pub_rx.recv().await {
                let _ = b_to_a_forwarder.send(bytes).await;
            }
        });

        let a_engine = SyncEngine::new(
            Arc::clone(&kt),
            "device-a".into(),
            Arc::clone(&a_state),
            a_tracker,
            Arc::clone(&store),
            a_pub_tx,
            a_sub_rx,
            paths("a", &dir),
            50,
        );
        let b_engine = SyncEngine::new(
            Arc::clone(&kt),
            "device-b".into(),
            Arc::clone(&b_state),
            b_tracker,
            Arc::clone(&store),
            b_pub_tx,
            b_sub_rx,
            paths("b", &dir),
            50,
        );

        TwoDevices {
            a_engine,
            b_engine,
            a_state,
            b_state,
            _a_to_b_tx: a_to_b_tx,
            _b_to_a_tx: b_to_a_tx,
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn one_way_convergence() {
        let dev = spawn_two_devices(123);
        // A applies a folder.
        let f = dm(1, vec![1, 2], 100);
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(f.clone());
        }
        dev.a_engine.notify_dirty();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let b = dev.b_state.lock().await;
        assert!(b.spaces.contains_key(&SpaceId([1; 16])));
        drop(b);

        dev.a_engine.shutdown().await;
        dev.b_engine.shutdown().await;
    }

    #[tokio::test]
    async fn bidirectional_convergence() {
        let dev = spawn_two_devices(45);
        let dm_ab = dm(1, vec![1, 2], 100);
        let dm_cd = dm(2, vec![3, 4], 100);
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(dm_ab);
        }
        {
            let mut b = dev.b_state.lock().await;
            b.apply_space_with_canonicalization(dm_cd);
        }
        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        // Multiple debounce cycles to converge.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let a = dev.a_state.lock().await;
        let b = dev.b_state.lock().await;
        assert!(a.spaces.contains_key(&SpaceId([1; 16])));
        assert!(a.spaces.contains_key(&SpaceId([2; 16])));
        assert!(b.spaces.contains_key(&SpaceId([1; 16])));
        assert!(b.spaces.contains_key(&SpaceId([2; 16])));
        drop(a);
        drop(b);

        dev.a_engine.shutdown().await;
        dev.b_engine.shutdown().await;
    }

    #[tokio::test]
    async fn cross_device_dedupe_through_sync() {
        // A and B independently create the same DM with different
        // ULIDs but the same sorted-members. After sync, both
        // converge on the smaller ULID.
        let dev = spawn_two_devices(7);
        let a_dm = dm(5, vec![1, 2], 100); // larger ULID — loser
        let b_dm = dm(1, vec![1, 2], 100); // smaller ULID — winner
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(a_dm);
        }
        {
            let mut b = dev.b_state.lock().await;
            b.apply_space_with_canonicalization(b_dm);
        }
        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let a = dev.a_state.lock().await;
        let b = dev.b_state.lock().await;
        // Both must agree on the winner SpaceId(1) and have lost SpaceId(5).
        assert!(a.spaces.contains_key(&SpaceId([1; 16])));
        assert!(!a.spaces.contains_key(&SpaceId([5; 16])));
        assert!(b.spaces.contains_key(&SpaceId([1; 16])));
        assert!(!b.spaces.contains_key(&SpaceId([5; 16])));
        drop(a);
        drop(b);

        dev.a_engine.shutdown().await;
        dev.b_engine.shutdown().await;
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib integration_tests 2>&1 | tail -10
```

Expected: 3 tests pass.

- [ ] **Step 3: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 17 — convergence integration tests

spawn_two_devices fixture: two SyncEngines sharing one Arc<InMemoryStub>,
cross-wired channels (A's publisher → B's subscriber, B's publisher
→ A's subscriber). Both share one KeyTree (matching "two bound
devices" model from Phase 2's crypto integration tests).

3 scenarios pin end-to-end convergence:
- one_way_convergence: A's mutation reaches B through full
  encrypt → CAS put → publish → subscribe → fetch → decrypt → merge
  pipeline.
- bidirectional_convergence: A and B both mutate concurrently,
  converge to the union (CRDT property).
- cross_device_dedupe_through_sync: Phase 2 round-3 scenario
  (different ULIDs, same dedupe_key) — exercises canonicalization
  rewrite under real sync, which the Phase 2 PR could only stub.
EOF
)"
```

---

## Task 18: Integration test — lagging-peer ack scenario

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`

- [ ] **Step 1: Write test**

Append inside `mod integration_tests`:

```rust
    use crate::owner_state_types::{
        ContentId, DeliveryStatus, OutboxEntry, OutboxEntryId,
    };

    /// Phase 2 round-5 scenario, exercised end-to-end through real
    /// sync: A and B's DMs collapse via dedupe, then a lagging
    /// device C sends an outbox ack still referencing the OLD
    /// (loser) space_id. After canonicalization rewrites A's outbox
    /// to the winner space_id, C's lagging ack must still merge.
    #[tokio::test]
    async fn lagging_peer_ack_after_dedupe_still_merges() {
        let dev = spawn_two_devices(99);

        // A creates DM id=5 (will lose dedupe to B's id=1).
        let a_dm = dm(5, vec![1, 2], 100);
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(a_dm);
            // Plus an OutboxEntry on that DM.
            a.apply_outbox(OutboxEntry {
                id: OutboxEntryId([42; 16]),
                space_id: SpaceId([5; 16]),
                recipient_owners: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
                message_cid: ContentId([7; 32]),
                created_at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "device-a".into(),
                },
                delivered_to: [OwnerAddr([1; 16])].into_iter().collect(),
                delivery_status: DeliveryStatus::Partial,
            });
        }
        // B creates DM id=1 (winner).
        let b_dm = dm(1, vec![1, 2], 100);
        {
            let mut b = dev.b_state.lock().await;
            b.apply_space_with_canonicalization(b_dm);
        }

        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // After sync: A's outbox should have been canonicalized to id=1.
        {
            let a = dev.a_state.lock().await;
            let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
            assert_eq!(
                entry.space_id,
                SpaceId([1; 16]),
                "A's outbox must have canonicalized space_id"
            );
        }

        // Now A re-mutates its outbox with the SAME OutboxEntry but
        // still referencing the OLD space_id=5 (simulating a lagging
        // peer). Phase 2 round-5 made apply_outbox accept this.
        {
            let mut a = dev.a_state.lock().await;
            a.apply_outbox(OutboxEntry {
                id: OutboxEntryId([42; 16]),
                space_id: SpaceId([5; 16]), // lagging — old loser id
                recipient_owners: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
                message_cid: ContentId([7; 32]),
                created_at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "device-a".into(),
                },
                delivered_to: [OwnerAddr([2; 16])].into_iter().collect(),
                delivery_status: DeliveryStatus::Partial,
            });
        }
        dev.a_engine.notify_dirty();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // After sync: A's entry still on canonicalized space_id=1,
        // and BOTH acks ({1, 2}) are present → Complete.
        let a = dev.a_state.lock().await;
        let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
        assert_eq!(entry.space_id, SpaceId([1; 16]));
        assert_eq!(entry.delivered_to.len(), 2);
        assert_eq!(entry.delivery_status, DeliveryStatus::Complete);
        drop(a);

        dev.a_engine.shutdown().await;
        dev.b_engine.shutdown().await;
    }
```

- [ ] **Step 2: Run test**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib lagging_peer_ack 2>&1 | tail -5
```

Expected: 1 test passes.

- [ ] **Step 3: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 18 — lagging-peer ack integration test

Exercises the Phase 2 round-5 canonicalization-vs-envelope-immutability
collision under real sync: A and B's DMs collapse, A's outbox gets
canonicalized to the winner space_id, then A re-applies the SAME
outbox referencing the OLD loser space_id (simulating a lagging
peer). The merge accepts (apply_outbox dropped space_id from its
envelope check in PR #73 round 5), unions the new ack, and the
delivery_status reaches Complete.

Test pins the round-5 invariant under the only conditions where it
truly matters: end-to-end canonicalization + replication + late ack.
EOF
)"
```

---

## Task 19: Property-style random sequence convergence test

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`

This test catches non-deterministic merge bugs by running 50 random sequences of mutations across A and B and asserting both converge to the union. Per the spec under §"Property-style coverage" — handwritten, no `proptest` crate dep.

- [ ] **Step 1: Write the property test**

Append inside `mod integration_tests`:

```rust
    /// 50 randomized sequences of (mutate-on-A, mutate-on-B,
    /// publish-A, publish-B) operations. After draining, A and B
    /// must hold equal `OwnerState`s. Catches non-determinism in
    /// the merge path that scripted tests miss.
    #[tokio::test]
    async fn random_sequence_convergence_50x() {
        // Seedable PRNG — chosen so a regression reproduces.
        let mut rng_state: u64 = 0xdead_beef_cafe_babe;
        fn next(rng: &mut u64) -> u64 {
            // xorshift64
            let mut x = *rng;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *rng = x;
            x
        }

        for trial in 0..50 {
            let dev = spawn_two_devices((trial % 256) as u8);
            // Generate 8-12 random folder mutations split between A and B.
            let n_ops = 8 + (next(&mut rng_state) % 5) as u8;
            for op in 0..n_ops {
                let folder_id = 100 + op;
                let timestamp = 1000 + (next(&mut rng_state) % 10000);
                let to_a = next(&mut rng_state) & 1 == 0;
                let f = dm(
                    folder_id,
                    vec![1, 2 + (op % 3)], // distinct sorted-members per op
                    timestamp,
                );
                if to_a {
                    let mut a = dev.a_state.lock().await;
                    a.apply_space_with_canonicalization(f);
                } else {
                    let mut b = dev.b_state.lock().await;
                    b.apply_space_with_canonicalization(f);
                }
            }
            dev.a_engine.notify_dirty();
            dev.b_engine.notify_dirty();
            // Multiple debounce + sync cycles to let convergence settle.
            tokio::time::sleep(Duration::from_millis(800)).await;

            // Force final flushes both directions and let them propagate.
            dev.a_engine.flush_now().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            dev.b_engine.flush_now().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            dev.a_engine.flush_now().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;

            let a = dev.a_state.lock().await;
            let b = dev.b_state.lock().await;
            assert_eq!(
                a.spaces, b.spaces,
                "trial {}: A and B spaces diverge\nA: {:?}\nB: {:?}",
                trial, a.spaces, b.spaces
            );
            drop(a);
            drop(b);

            dev.a_engine.shutdown().await;
            dev.b_engine.shutdown().await;
        }
    }
```

The `dm` and `spawn_two_devices` helpers were defined in Task 17. Note: this test uses `spaces` equality only (not full `OwnerState` equality) because the random `dm` member combinations may legitimately produce non-identical outbox/inbox state on the two sides given the test's mutation pattern.

- [ ] **Step 2: Run the property test**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib random_sequence_convergence 2>&1 | tail -5
```

Expected: 1 test passes (may take 30-60s — 50 iterations × ~1.4s each).

- [ ] **Step 3: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 19 — property-style random convergence

50 trials × 8-12 random mutations split across A and B + multiple
debounce/flush cycles. Asserts A.spaces == B.spaces after every
trial. Catches non-determinism in the merge path that scripted
tests miss — particularly regressions in HLC ordering, Space
dedupe under load, or canonicalization rewrites racing publishes.

Seeded xorshift64 PRNG so any regression reproduces deterministically
from the trial number in the failure message.
EOF
)"
```

---

## Task 20: Wire SyncEngine into start_node + shutdown hook

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (Zenoh adapter wiring)
- Modify: `src-tauri/src/lib.rs` (start_node integration + shutdown hook)

This task is the only place where the new SyncEngine actually meets the running app. Implementation here is integration-shaped; if the implementer hits boot-order issues, escalate to a controller agent rather than guessing.

- [ ] **Step 1: Read context**

```bash
grep -n "let session = match cancellable" src-tauri/src/event_loop.rs
grep -n "load_owner_state\|LoadedOwnerState\|fn start_node" src-tauri/src/lib.rs src-tauri/src/owner_state.rs
grep -n "fn run_app\|on_window_event\|RunEvent::Exit" src-tauri/src/lib.rs
```

Note line numbers; the exact integration site for the SyncEngine handle and the shutdown call depends on the existing app structure. The handle (`Arc<SyncEngine>`) needs to be reachable from BOTH the place that calls `notify_dirty` after CRDT mutations (Phase 4 will own most of those) AND the shutdown hook.

- [ ] **Step 2: Add a Zenoh adapter inside `event_loop::run`**

After the `let session = match ... zenoh::open(config) ...` block in `event_loop.rs` (around line 211), add (replacing the placeholder paths with the actual extracted addr_hex from the loaded identity):

```rust
// ── Phase 3a: SyncEngine wire-up ────────────────────────────────
// The SyncEngine itself is constructed from start_node (lib.rs)
// which has the master_seed, KeyTree, OwnerState, and tracker
// already in scope. Here in event_loop we only own the Zenoh
// adapter — declaring publisher/subscriber on the state-root topic
// and forwarding bytes between the SyncEngine's channels and Zenoh.
//
// The mpsc::Sender/Receiver pair is passed in via the StartNodeArgs
// struct (extend that struct to carry SyncEngineHandles).
if let Some(sync_handles) = startup_args.sync_handles.take() {
    let topic = format!(
        "harmony/owner/{}/state-root-v1",
        sync_handles.addr_hex
    );
    let key_expr = zenoh::key_expr::KeyExpr::try_from(topic)
        .expect("state-root topic key_expr");

    // Outbound: drain SyncEngine.publisher_tx → Zenoh put.
    let session_pub = session.clone();
    let key_pub = key_expr.clone();
    let mut outbound_rx = sync_handles.outbound_rx;
    let closing_pub = Arc::clone(&closing);
    tokio::spawn(async move {
        while let Some(bytes) = outbound_rx.recv().await {
            if let Err(e) = session_pub.put(&key_pub, bytes).await {
                if !closing_pub.load(std::sync::atomic::Ordering::SeqCst) {
                    tracing::warn!(error = %e, "state-root publish failed");
                }
            }
        }
    });

    // Inbound: Zenoh subscriber → SyncEngine.subscriber_tx.
    match session.declare_subscriber(&key_expr).await {
        Ok(sub) => {
            let inbound_tx = sync_handles.inbound_tx;
            let closing_sub = Arc::clone(&closing);
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let bytes: Vec<u8> = sample.payload().to_bytes().to_vec();
                    if inbound_tx.send(bytes).await.is_err() {
                        break;
                    }
                }
                if !closing_sub.load(std::sync::atomic::Ordering::SeqCst) {
                    tracing::warn!("state-root subscriber closed unexpectedly");
                }
            });
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to declare state-root subscriber");
        }
    }
}
```

Add a struct in `event_loop.rs` (near `StartNodeArgs` or wherever the existing argument struct lives):

```rust
pub struct SyncEngineHandles {
    pub addr_hex: String,
    pub outbound_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    pub inbound_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}
```

Extend the existing args struct with `pub sync_handles: Option<SyncEngineHandles>`.

- [ ] **Step 3: Construct the SyncEngine in `start_node`**

In `src-tauri/src/lib.rs`, locate the `start_node` Tauri command (search for `pub async fn start_node` or `#[tauri::command] async fn start_node`). After the existing identity load (`load_owner_state` returns `LoadedOwnerState`), add (use line numbers from Step 1's grep output as anchors):

```rust
// Phase 3a: bring up the SyncEngine if a master_seed is available.
let sync_engine = if let Some(seed) = loaded.master_seed.as_ref() {
    let kt = std::sync::Arc::new(
        crate::owner_state_crypto::KeyTree::derive(seed)
            .map_err(|e| format!("KeyTree::derive: {e}"))?,
    );
    let device_id = loaded
        .device_signing_key
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let identity_dir = crate::owner_commands::resolve_identity_dir()?;
    let crdt_path = identity_dir.join("owner_state_crdt.cbor");
    let replay_path = identity_dir.join("state_root_replay.cbor");
    let initial_crdt = crate::owner_state_persist::load_crdt(&crdt_path)
        .map_err(|e| format!("load owner_state_crdt.cbor: {e}"))?;
    let initial_replay = crate::owner_state_persist::load_replay(&replay_path)
        .map_err(|e| format!("load state_root_replay.cbor: {e}"))?;

    let crdt_state = std::sync::Arc::new(tokio::sync::Mutex::new(initial_crdt));
    let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(initial_replay));
    let content_store: std::sync::Arc<dyn crate::content_store::ContentStore> =
        std::sync::Arc::new(crate::content_store::InMemoryStub::default());

    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    let engine = std::sync::Arc::new(crate::owner_state_sync::SyncEngine::new(
        std::sync::Arc::clone(&kt),
        device_id,
        std::sync::Arc::clone(&crdt_state),
        std::sync::Arc::clone(&tracker),
        content_store,
        out_tx,
        in_rx,
        crate::owner_state_sync::PersistPaths {
            crdt: crdt_path,
            replay: replay_path,
        },
        crate::owner_state_sync::DEFAULT_DEBOUNCE_MS,
    ));

    // Pass the channel ends + addr_hex to event_loop via the args.
    // `node_addr` is already in scope at this point in start_node — it's
    // the hex encoding of `our_addr_bytes`, derived a few lines earlier
    // from `ed25519.public_identity().address_hash`. Reuse that string;
    // it's exactly the addr_hex form the topic name needs.
    startup_args.sync_handles = Some(crate::event_loop::SyncEngineHandles {
        addr_hex: node_addr.clone(),
        outbound_rx: out_rx,
        inbound_tx: in_tx,
    });

    Some(engine)
} else {
    None
};
```

Stash `sync_engine` into Tauri State (managed via `app.manage(sync_engine)`) so other commands and the shutdown hook can reach it.

- [ ] **Step 4: Wire shutdown hook**

In `src-tauri/src/lib.rs`, locate the Tauri `Builder::default().run(...)` invocation and the `RunEvent::Exit` matcher (or wherever app lifecycle events are observed). Add:

```rust
// Phase 3a: flush SyncEngine on app shutdown.
if let Some(engine) = app_handle
    .try_state::<Option<std::sync::Arc<crate::owner_state_sync::SyncEngine>>>()
    .map(|s| s.inner().clone())
    .flatten()
{
    let rt = tokio::runtime::Handle::try_current();
    if let Ok(handle) = rt {
        // Block on shutdown — must complete before app exit.
        handle.block_on(async move {
            engine.shutdown().await;
        });
    }
}
```

- [ ] **Step 5: Manual smoke test**

Build the full app:

```bash
cargo build --manifest-path src-tauri/Cargo.toml --release 2>&1 | tail -5
```

Expected: clean build.

Run the app, complete pairing if not yet paired, then perform a CRDT mutation via existing IPC (or wait for Phase 4). Inspect `~/Library/Application Support/com.harmony.app/owner_state_crdt.cbor` (macOS path; adjust for platform) — file should appear shortly after first mutation.

If no IPC trigger exists yet (Phase 4 owns those), this step is a smoke check that the app boots cleanly with the SyncEngine alive. Watch the log for `"state-root subscriber"` lines — their absence means the wire-up didn't reach event_loop.

- [ ] **Step 6: Run all existing tests + lib build**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -5
```

Expected: all tests still pass.

- [ ] **Step 7: cargo fmt + clippy gate**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: clippy clean, fmt no diff.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/event_loop.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-215-sub-a): Phase 3a Task 19 — wire SyncEngine into start_node

start_node now constructs the SyncEngine after loading the legacy
owner_state.cbor (master_seed, device_signing_key) and stashes the
Arc<SyncEngine> in Tauri State for other commands + the shutdown
hook to reach.

event_loop.rs hosts the Zenoh adapter: declares the state-root
publisher and subscriber on harmony/owner/{addr_hex}/state-root-v1,
spawns two tokio tasks that forward bytes between the SyncEngine's
mpsc channels and Zenoh. Both follow the existing closing:
Arc<AtomicBool> shutdown pattern from pairing/mail/voice
subscribers.

Tauri shutdown hook explicitly calls engine.shutdown().await so the
final debounced publish + persist flush runs before the app exits.
Drop is best-effort only; an explicit shutdown is the documented
safe path.

No new tests in this task — the integration is exercised end-to-end
by the smoke build + the unit/integration tests landed in tasks
1-19 (which use the channel surface directly).
EOF
)"
```

---

## Task 21: Final gates + push + open PR

**Files:** none (release/CI surface only).

- [ ] **Step 1: Run the full test suite one more time**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -3
```

Expected: all tests pass. Count should be `~390+ passed; 0 failed` (370 prior baseline + the new tests across tasks 2-18).

- [ ] **Step 2: Run cargo fmt across the whole tree**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml --all
git status --short
```

Expected: no diff (`git status` is clean).

- [ ] **Step 3: Run cargo clippy with the strict gate**

```bash
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: clean, no warnings, no errors.

- [ ] **Step 4: Push branch**

```bash
git push -u origin zeb-215-sub-a-phase3a-sync 2>&1 | tail -5
```

Expected: branch tracking set up, all commits pushed.

- [ ] **Step 5: Open the PR**

```bash
gh pr create --title "feat(zeb-215-sub-a): Phase 3a — owner-state sync (state-root + persistence)" --body "$(cat <<'EOF'
## Summary

Wires Phase 1 crypto + Phase 2 CRDT into a working state-root sync
surface for the harmony-client desktop app. Phase 3a's scope:

- **On-disk persistence** — two new files (`owner_state_crdt.cbor`,
  `state_root_replay.cbor`) with 1-byte schema-version prefix,
  atomic-rename + fsync, separate write locks.
- **Zenoh state-root pub/sub** — encrypts via Phase 1's
  `encrypt_root_publish`, sends on
  `harmony/owner/{addr_hex}/state-root-v1`, replay-protected
  subscriber.
- **`ContentStore` trait + `InMemoryStub`** — trait shape ready for
  Phase 3b's harmony-content swap; the stub is load-bearing for
  unit + integration tests.
- **`SyncEngine`** — debounced publishes (250ms default), explicit
  `flush_now()`, clean shutdown.

## Out of scope (deferred to Phase 3b)

- Real harmony-content CAS integration. The `InMemoryStub` is
  per-process, so 3a's cross-device sync only works in tests
  (where two `SyncEngine`s share one stub). 3b swaps the stub.
- Per-entry blob CAS layout. 3a treats the entire `OwnerState` as
  a single content-addressed blob (deliberate simplification, see
  spec §"Root blob shape — Phase 3a simplification").

See spec: [`docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`](https://github.com/zeblithic/harmony-client/blob/zeb-215-sub-a-phase3a-sync/docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md).

## Test plan

- [x] All ~390 lib tests pass (`cargo test --lib`).
- [x] `cargo fmt --all -- --check` clean.
- [x] `cargo clippy --all-targets -- -D warnings` clean.
- [x] Persistence: round-trip both files, schema-version mismatch
  rejected, atomic-rename survives partial writes.
- [x] Debounce: collapse 50 rapid notify_dirty calls into one
  publish; `flush_now` cancels pending wakeup.
- [x] Subscriber: replay protection rejects strictly-older HLCs;
  missing-blob handling logs + skips without panic.
- [x] Integration: one-way + bidirectional convergence; cross-device
  dedupe through sync (Phase 2 round-3 scenario); lagging-peer ack
  after canonicalization (Phase 2 round-5 scenario).
- [x] Crash + restart: replay tracker survives engine drop without
  shutdown; older publishes rejected on next boot.

## Phase 1 + Phase 2 invariants preserved

- `owner_state_crypto.rs` untouched.
- `owner_state_crdt.rs` untouched.
- `owner_state_types.rs` adds one type (`RootPublishPayload`) to the
  `impl_canonical!` list — the only Phase-2 module touch, called
  out in the spec under "Module boundaries".

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)" 2>&1 | tail -3
```

Expected: PR URL printed.

- [ ] **Step 6: Mark complete**

The PR is open. Bot/human review starts the next round (matching the Phase 1 / Phase 2 review tail). No further task in this plan; review feedback will produce its own follow-up commits.
