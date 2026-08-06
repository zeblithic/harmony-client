# ZEB-862 Restart-Durable First-Observation Clock — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist each subsystem's LOCAL `first_observed_ms` TTL clock across restart via a per-subsystem sidecar CBOR file, so never-covered relay-hold blobs and DM-inbox entries age out on their intended TTL instead of getting a fresh TTL each reboot.

**Architecture:** Add a getter + restore-setter to `RelayHoldDoc` / `DmInboxDoc` (the `first_observed_ms` field stays `#[serde(skip)]`, so the CRDT wire bytes are unchanged). Add sidecar save/load/recover functions to `relay_hold_persist.rs` / `dm_inbox_persist.rs`, cloned verbatim from the existing replay-tracker functions in the same files, and write the sidecar in `FleetPersist::persist`. Wire boot to load the sidecar and `restore_first_observed` into the doc.

**Tech Stack:** Rust, `ciborium` (CBOR), `serde`, tokio; `owner_state_persist::save_atomically` for crash-durable atomic writes.

## Global Constraints

- Persist the LOCAL clock only — NO peer stamp (`held_at` / `deposited_at`) enters any TTL decision (ZEB-831 mandate). The sidecar stores `BTreeMap<String, u64>` (entry-key → local first-observation ms), nothing else.
- `first_observed_ms` field stays `#[serde(skip)]` on both docs; CRDT canonical wire bytes and entries-only `PartialEq` are unchanged.
- Sidecar mirrors the existing replay-tracker sibling exactly: 1-byte schema-version prefix, plaintext CBOR, strict trailing-byte rejection, quarantine-on-`CborDecode`-corruption (`.corrupt-<ms>`, bytes preserved), transient `Persist` I/O propagated untouched (ZEB-460).
- Missing sidecar → `Ok(BTreeMap::new())` → today's re-stamp behavior (no doc-file migration; file presence IS the migration).
- Filenames: `relay_hold_first_observed.cbor`, `dm_inbox_first_observed.cbor` (alongside the existing `*.cbor` / `*_replay.cbor`).
- CI gates (from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend (repo root): `npx tsc --noEmit` (no FE change expected).
- Iterative per-task gates may use scoped runs (`cargo nextest run -p harmony-app -E 'test(...)'`); the FINAL pre-PR gate (Task 3) is the full `--workspace --all-targets` sweep. Paste any `scripts/test-select` `round=… bucket=…` summary line into task notes if used.

---

### Task 1: Doc accessors + core regression tests (both CRDTs)

**Files:**
- Modify: `src-tauri/src/community_relay_hold_crdt.rs` (add two methods to the `impl RelayHoldDoc` block containing `gc`, ~after line 210; tests into the existing `#[cfg(test)] mod tests`)
- Modify: `src-tauri/src/dm_inbox_crdt.rs` (add two methods to the `impl DmInboxDoc` block containing `gc_expired`, ~after line 145; tests into the existing `#[cfg(test)] mod tests` at line 272)

**Interfaces:**
- Produces (both docs): `pub fn first_observed_ms(&self) -> &std::collections::BTreeMap<String, u64>` and `pub fn restore_first_observed(&mut self, map: std::collections::BTreeMap<String, u64>)`.
- Consumes: existing `RelayHoldDoc::gc(now_ms)`, `DmInboxDoc::gc_expired(now_ms, covered)`; test helpers `hlc`, `space`, `entry`, `key_rr` (relay-hold tests) and `entry(at, by, ig)` (dm-inbox tests); consts `crate::community_relay::RELAY_HOLD_TTL_MS`, `crate::butler_deposit::INBOX_TTL_MS`.

- [ ] **Step 1: Write the failing tests (relay-hold)**

Add to `community_relay_hold_crdt.rs` `mod tests`:

```rust
#[test]
fn restored_old_first_observed_expires_across_restart() {
    // ZEB-862: a durable OLD stamp (as if reloaded from the sidecar) ages the
    // entry out on the next sweep — the whole point of the fix.
    let mut doc = RelayHoldDoc::default();
    let k = key_rr(1, 1);
    doc.entries
        .insert(k.clone(), entry([1; 16], [2; 16], space(3), hlc(1, "a"), "relay", &[]));
    let now = crate::community_relay::RELAY_HOLD_TTL_MS + 10_000;
    doc.restore_first_observed([(k.clone(), 1u64)].into_iter().collect());
    assert!(doc.gc(now), "old restored stamp → entry ages out");
    assert!(doc.entries.is_empty());
}

#[test]
fn empty_first_observed_survives_first_sweep_after_restart() {
    // ZEB-862 negative: without the sidecar the empty clock re-stamps at `now`,
    // so the entry survives — this is today's bug, pinned to show the contrast.
    let mut doc = RelayHoldDoc::default();
    let k = key_rr(1, 1);
    doc.entries
        .insert(k.clone(), entry([1; 16], [2; 16], space(3), hlc(1, "a"), "relay", &[]));
    let now = crate::community_relay::RELAY_HOLD_TTL_MS + 10_000;
    assert!(!doc.gc(now), "empty clock re-stamps at now → survives");
    assert_eq!(doc.entries.len(), 1);
}

#[test]
fn restore_and_read_first_observed_round_trips() {
    let mut doc = RelayHoldDoc::default();
    let k = key_rr(1, 1);
    doc.entries
        .insert(k.clone(), entry([1; 16], [2; 16], space(3), hlc(1, "a"), "relay", &[]));
    let m: std::collections::BTreeMap<String, u64> =
        [(k.clone(), 12_345u64)].into_iter().collect();
    doc.restore_first_observed(m.clone());
    assert_eq!(doc.first_observed_ms(), &m);
}
```

- [ ] **Step 2: Run them; verify they fail to compile (methods undefined)**

Run: `cd src-tauri && cargo nextest run -p harmony-app --features test-fixtures -E 'test(restored_old_first_observed_expires_across_restart) + test(empty_first_observed_survives_first_sweep_after_restart) + test(restore_and_read_first_observed_round_trips)'`
Expected: compile error — no method `restore_first_observed` / `first_observed_ms` on `RelayHoldDoc`.

- [ ] **Step 3: Implement the accessors (relay-hold)**

In `community_relay_hold_crdt.rs`, inside the `impl RelayHoldDoc` block that holds `gc` (add right after the `gc` method, before its closing `}`):

```rust
    /// ZEB-862: read the LOCAL first-observation clock for durable sidecar
    /// persistence. Never leaves this replica and never enters the wire.
    pub fn first_observed_ms(&self) -> &BTreeMap<String, u64> {
        &self.first_observed_ms
    }

    /// ZEB-862: restore the LOCAL first-observation clock on boot from the
    /// sidecar file, so TTL GC survives restart instead of re-stamping `now`.
    pub fn restore_first_observed(&mut self, map: BTreeMap<String, u64>) {
        self.first_observed_ms = map;
    }
```

- [ ] **Step 4: Write the failing tests (dm-inbox)**

Add to `dm_inbox_crdt.rs` `mod tests` (uses that module's `entry(at, by, ig)` helper; `gc_expired` takes an explicit `covered` set):

```rust
#[test]
fn restored_old_first_observed_expires_across_restart() {
    let mut doc = DmInboxDoc::default();
    let k = DmInboxDoc::key(&[1u8; 16], &[2u8; 32]);
    doc.entries.insert(k.clone(), entry(hlc(1, "a"), "butler", &[]));
    let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
    doc.restore_first_observed([(k.clone(), 1u64)].into_iter().collect());
    assert!(
        doc.gc_expired(now, &std::collections::BTreeSet::new()),
        "old restored stamp → entry ages out"
    );
    assert!(doc.entries.is_empty());
}

#[test]
fn empty_first_observed_survives_first_sweep_after_restart() {
    let mut doc = DmInboxDoc::default();
    let k = DmInboxDoc::key(&[1u8; 16], &[2u8; 32]);
    doc.entries.insert(k.clone(), entry(hlc(1, "a"), "butler", &[]));
    let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
    assert!(
        !doc.gc_expired(now, &std::collections::BTreeSet::new()),
        "empty clock re-stamps at now → survives"
    );
    assert_eq!(doc.entries.len(), 1);
}

#[test]
fn restore_and_read_first_observed_round_trips() {
    let mut doc = DmInboxDoc::default();
    let k = DmInboxDoc::key(&[1u8; 16], &[2u8; 32]);
    doc.entries.insert(k.clone(), entry(hlc(1, "a"), "butler", &[]));
    let m: std::collections::BTreeMap<String, u64> =
        [(k.clone(), 12_345u64)].into_iter().collect();
    doc.restore_first_observed(m.clone());
    assert_eq!(doc.first_observed_ms(), &m);
}
```

Note: confirm a `hlc(w, d)` helper exists in `dm_inbox_crdt.rs` `mod tests`; if not, inline `Hlc { wall_ms: 1, logical: 0, device_id: "a".into() }` in place of `hlc(1, "a")`.

- [ ] **Step 5: Run them; verify they fail (methods undefined)**

Run: `cd src-tauri && cargo nextest run -p harmony-app --features test-fixtures -E 'test(dm_inbox_crdt)'`
Expected: compile error — methods undefined on `DmInboxDoc`.

- [ ] **Step 6: Implement the accessors (dm-inbox)**

In `dm_inbox_crdt.rs`, inside the `impl DmInboxDoc` block that holds `gc_expired` (after `gc_expired`, before its closing `}`):

```rust
    /// ZEB-862: read the LOCAL first-observation clock for durable sidecar
    /// persistence. Never leaves this replica and never enters the wire.
    pub fn first_observed_ms(&self) -> &BTreeMap<String, u64> {
        &self.first_observed_ms
    }

    /// ZEB-862: restore the LOCAL first-observation clock on boot from the
    /// sidecar file, so TTL GC survives restart instead of re-stamping `now`.
    pub fn restore_first_observed(&mut self, map: BTreeMap<String, u64>) {
        self.first_observed_ms = map;
    }
```

- [ ] **Step 7: Run all six tests; verify PASS**

Run: `cd src-tauri && cargo nextest run -p harmony-app --features test-fixtures -E 'test(restored_old_first_observed_expires_across_restart) + test(empty_first_observed_survives_first_sweep_after_restart) + test(restore_and_read_first_observed_round_trips)'`
Expected: 6 passed (3 per module).

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/community_relay_hold_crdt.rs src-tauri/src/dm_inbox_crdt.rs
git commit -m "ZEB-862: first_observed_ms accessors + restart-durability regression tests"
```

---

### Task 2: Sidecar persist functions + FleetPersist wiring (both persist modules)

**Files:**
- Modify: `src-tauri/src/relay_hold_persist.rs` (add filename const, schema const, `FirstObservedFileV1` newtype, `save_first_observed` / `load_first_observed` / `load_first_observed_or_recover`; add `first_observed_path` to `RelayHoldPersist` (line ~213) and write it in `persist` (line ~219); tests into the existing `#[cfg(test)] mod tests`)
- Modify: `src-tauri/src/dm_inbox_persist.rs` (symmetric)

**Interfaces:**
- Consumes: `RelayHoldDoc::first_observed_ms()` / `DmInboxDoc::first_observed_ms()` (Task 1); existing in-file `atomic_write`, `quarantine`, `from_reader`, `into_writer`, `Cursor`.
- Produces: `relay_hold_persist::{RELAY_HOLD_FIRST_OBSERVED_FILENAME, save_first_observed, load_first_observed, load_first_observed_or_recover}` and `RelayHoldPersist.first_observed_path: PathBuf`; `dm_inbox_persist::{DM_INBOX_FIRST_OBSERVED_FILENAME, save_first_observed, load_first_observed, load_first_observed_or_recover}` and `DmInboxPersist.first_observed_path: PathBuf`.

- [ ] **Step 1: Write the failing tests (relay-hold persist)**

Add to `relay_hold_persist.rs` `mod tests`:

```rust
#[test]
fn first_observed_round_trips_and_missing_is_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay_hold_first_observed.cbor");
    assert!(load_first_observed(&path).unwrap().is_empty());
    let mut m = std::collections::BTreeMap::new();
    m.insert("k1".to_string(), 111u64);
    m.insert("k2".to_string(), 222u64);
    save_first_observed(&path, &m).unwrap();
    assert_eq!(load_first_observed(&path).unwrap(), m);
}

#[test]
fn load_first_observed_rejects_trailing_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay_hold_first_observed.cbor");
    let mut m = std::collections::BTreeMap::new();
    m.insert("k".to_string(), 5u64);
    save_first_observed(&path, &m).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.push(0xFF);
    std::fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        load_first_observed(&path).unwrap_err(),
        SyncError::CborDecode(_)
    ));
    // recover quarantines and returns empty
    assert!(load_first_observed_or_recover(&path).unwrap().is_empty());
    assert!(!path.exists(), "corrupt sidecar was quarantined");
}

#[test]
fn load_first_observed_or_recover_propagates_transient_io_without_quarantine() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fo.cbor");
    std::fs::create_dir(&path).unwrap();
    assert!(matches!(
        load_first_observed_or_recover(&path).unwrap_err(),
        SyncError::Persist(_)
    ));
    assert!(path.is_dir(), "transient error leaves the path untouched");
}
```

Also extend the existing `relay_hold_persist_writes_both_files` test: add `first_observed_path: dir.path().join("relay_hold_first_observed.cbor")` to the `RelayHoldPersist` literal, seed `doc.restore_first_observed([("x".to_string(), 9u64)].into_iter().collect())` before `persist`, and after `persist` assert `load_first_observed(&p.first_observed_path).unwrap()` equals that map. Rename the test to `relay_hold_persist_writes_all_files`.

- [ ] **Step 2: Add the boot-path integration test (relay-hold persist)**

```rust
#[test]
fn persist_then_reload_first_observed_drives_expiry() {
    // Full sidecar round-trip: an OLD stamp persisted, reloaded via the boot
    // shape, restored into a fresh doc, ages the entry out on gc(now).
    use crate::community_relay_hold_crdt::RelayHoldDoc;
    let dir = tempfile::tempdir().unwrap();
    let fo_path = dir.path().join("relay_hold_first_observed.cbor");
    let key = RelayHoldDoc::key(&[1u8; 16], &[2u8; 32]);
    save_first_observed(&fo_path, &[(key.clone(), 1u64)].into_iter().collect()).unwrap();

    let mut doc = sample_doc(); // has one never-covered entry under a different key
    doc.entries
        .insert(key.clone(), sample_entry_uncovered(&key));
    doc.restore_first_observed(load_first_observed_or_recover(&fo_path).unwrap());
    let now = crate::community_relay::RELAY_HOLD_TTL_MS + 10_000;
    doc.gc(now);
    assert!(
        !doc.entries.contains_key(&key),
        "reloaded old stamp aged the entry out"
    );
}
```

Add a helper next to `sample_entry` in the test module:

```rust
fn sample_entry_uncovered(_key: &str) -> RelayHoldEntry {
    let mut e = sample_entry();
    e.pulled_by.clear(); // never covered → only TTL can remove it
    e
}
```

- [ ] **Step 3: Run; verify failure**

Run: `cd src-tauri && cargo nextest run -p harmony-app --features test-fixtures -E 'test(first_observed) + test(relay_hold_persist_writes_all_files) + test(persist_then_reload_first_observed_drives_expiry)'`
Expected: compile error — `save_first_observed` / `load_first_observed` / `first_observed_path` undefined.

- [ ] **Step 4: Implement the sidecar functions + FleetPersist field (relay-hold)**

In `relay_hold_persist.rs`, after the replay-tracker section, add:

```rust
// ── first-observed sidecar (ZEB-862) ───────────────────────────────────────────

/// File name for the persisted LOCAL first-observation clock. Lives alongside
/// `relay_hold.cbor`.
pub const RELAY_HOLD_FIRST_OBSERVED_FILENAME: &str = "relay_hold_first_observed.cbor";

const RELAY_HOLD_FIRST_OBSERVED_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct RelayHoldFirstObservedFileV1(BTreeMap<String, u64>);

/// Load the LOCAL first-observation clock from `path`. Missing file → empty map
/// (→ today's re-stamp behavior; no doc-file migration needed).
pub fn load_first_observed(path: &Path) -> Result<BTreeMap<String, u64>, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "relay-hold first-observed file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        RELAY_HOLD_FIRST_OBSERVED_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: RelayHoldFirstObservedFileV1 = from_reader(&mut cursor).map_err(|e| {
                SyncError::CborDecode(format!("load_first_observed {}: {e}", path.display()))
            })?;
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after relay-hold first-observed value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown relay-hold first-observed schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Same recovery contract as [`load_doc_or_recover`]: `CborDecode` corruption is
/// quarantined and an empty map returned; a transient `Persist` error is left
/// untouched and propagated (ZEB-460).
pub fn load_first_observed_or_recover(path: &Path) -> Result<BTreeMap<String, u64>, SyncError> {
    match load_first_observed(path) {
        Ok(m) => Ok(m),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(BTreeMap::new())
        }
        Err(e) => Err(e),
    }
}

/// Save the LOCAL first-observation clock to `path` atomically.
pub fn save_first_observed(path: &Path, map: &BTreeMap<String, u64>) -> Result<(), SyncError> {
    let mut bytes = vec![RELAY_HOLD_FIRST_OBSERVED_SCHEMA_V1];
    into_writer(&RelayHoldFirstObservedFileV1(map.clone()), &mut bytes).map_err(|e| {
        SyncError::CborEncode(format!("encode first-observed {}: {e}", path.display()))
    })?;
    atomic_write(path, &bytes)
}
```

Then update `RelayHoldPersist` and its `persist`:

```rust
pub struct RelayHoldPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
    pub first_observed_path: std::path::PathBuf,
}

impl crate::fleet_sync::FleetPersist<RelayHoldDoc> for RelayHoldPersist {
    fn persist(
        &self,
        state: &RelayHoldDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        save(&self.doc_path, state)?;
        save_replay(&self.replay_path, tracker)?;
        save_first_observed(&self.first_observed_path, state.first_observed_ms())?;
        Ok(())
    }
}
```

- [ ] **Step 5: Run relay-hold persist tests; verify PASS**

Run: `cd src-tauri && cargo nextest run -p harmony-app --features test-fixtures -E 'test(first_observed) + test(relay_hold_persist_writes_all_files) + test(persist_then_reload_first_observed_drives_expiry)'`
Expected: all pass.

- [ ] **Step 6: Repeat symmetrically for dm-inbox persist**

In `dm_inbox_persist.rs`, add the same three tests (`first_observed_round_trips_and_missing_is_default`, `load_first_observed_rejects_trailing_bytes`, `load_first_observed_or_recover_propagates_transient_io_without_quarantine`) and the `persist_then_reload_first_observed_drives_expiry` integration test — using `DmInboxDoc`, `DmInboxDoc::gc_expired(now, &BTreeSet::new())`, `crate::butler_deposit::INBOX_TTL_MS`, filename `dm_inbox_first_observed.cbor`, and the module's `sample_doc`/`sample_entry`. Extend `dm_inbox_persist_writes_both_files` → `_writes_all_files`. Then implement the symmetric sidecar block with `DM_INBOX_FIRST_OBSERVED_FILENAME = "dm_inbox_first_observed.cbor"`, `DM_INBOX_FIRST_OBSERVED_SCHEMA_V1`, `DmInboxFirstObservedFileV1(BTreeMap<String, u64>)`, the three functions (identical bodies with "dm-inbox" wording), and add `first_observed_path` to `DmInboxPersist` + `save_first_observed(&self.first_observed_path, state.first_observed_ms())?` in `persist`.

For the dm-inbox uncovered helper, if `sample_entry`'s `ingested_by` is non-empty, add:

```rust
fn sample_entry_uncovered() -> crate::dm_inbox_crdt::DmInboxEntry {
    let mut e = sample_entry();
    e.ingested_by.clear();
    e
}
```

- [ ] **Step 7: Run dm-inbox persist tests; verify PASS**

Run: `cd src-tauri && cargo nextest run -p harmony-app --features test-fixtures -E 'test(dm_inbox_persist) + test(first_observed)'`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/relay_hold_persist.rs src-tauri/src/dm_inbox_persist.rs
git commit -m "ZEB-862: sidecar persist for first_observed_ms (relay-hold + DM-inbox)"
```

---

### Task 3: Boot wiring + test-site touch-ups + full gate

**Files:**
- Modify: `src-tauri/src/lib.rs` (DM-inbox block ~6002-6051; relay-hold block ~6262-6303)
- Modify: `src-tauri/src/dm_inbox_ingest.rs:2181` (`#[cfg(test)]` `DmInboxPersist` literal)

**Interfaces:**
- Consumes: everything from Tasks 1-2 — `load_first_observed_or_recover`, `*_FIRST_OBSERVED_FILENAME`, `restore_first_observed`, the `first_observed_path` struct field.

- [ ] **Step 1: Wire the DM-inbox boot block**

In `lib.rs` (~6002), after the `dm_inbox_replay_path` line add:

```rust
                    let dm_inbox_first_observed_path = identity_dir
                        .join(crate::dm_inbox_persist::DM_INBOX_FIRST_OBSERVED_FILENAME);
```

Replace the `dm_inbox_doc` construction (~6006-6009) with a form that restores the sidecar into the loaded doc:

```rust
                    let dm_inbox_doc = std::sync::Arc::new(tokio::sync::Mutex::new({
                        let mut doc = crate::dm_inbox_persist::load_doc_or_recover(&dm_inbox_path)
                            .map_err(|e| format!("load dm-inbox doc: {e}"))?;
                        doc.restore_first_observed(
                            crate::dm_inbox_persist::load_first_observed_or_recover(
                                &dm_inbox_first_observed_path,
                            )
                            .map_err(|e| format!("load dm-inbox first-observed: {e}"))?,
                        );
                        doc
                    }));
```

Add `first_observed_path: dm_inbox_first_observed_path,` to the `DmInboxPersist { doc_path, replay_path }` literal (~6047).

- [ ] **Step 2: Wire the relay-hold boot block**

In `lib.rs` (~6262), after the `relay_hold_replay_path` line add:

```rust
                    let relay_hold_first_observed_path = identity_dir
                        .join(crate::relay_hold_persist::RELAY_HOLD_FIRST_OBSERVED_FILENAME);
```

Replace the `relay_hold_doc` construction (~6266-6269) with:

```rust
                    let relay_hold_doc = std::sync::Arc::new(tokio::sync::Mutex::new({
                        let mut doc = crate::relay_hold_persist::load_doc_or_recover(&relay_hold_path)
                            .map_err(|e| format!("load relay-hold doc: {e}"))?;
                        doc.restore_first_observed(
                            crate::relay_hold_persist::load_first_observed_or_recover(
                                &relay_hold_first_observed_path,
                            )
                            .map_err(|e| format!("load relay-hold first-observed: {e}"))?,
                        );
                        doc
                    }));
```

Add `first_observed_path: relay_hold_first_observed_path,` to the `RelayHoldPersist { doc_path, replay_path }` literal (~6299).

- [ ] **Step 3: Update the `#[cfg(test)]` DmInboxPersist literal**

In `dm_inbox_ingest.rs:2181`, add to the literal:

```rust
                first_observed_path: dir.path().join("dm_inbox_first_observed.cbor"),
```

- [ ] **Step 4: Compile-check the whole workspace**

Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: clean. If the compiler flags any other `RelayHoldPersist`/`DmInboxPersist` literal missing `first_observed_path`, add it there too (a tempdir path in tests; the sibling `identity_dir.join(...)` path in any prod site).

- [ ] **Step 5: Full local gate (CI parity)**

Run (from `src-tauri/`):
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Then from repo root: `npx tsc --noEmit`
Expected: fmt clean, clippy clean, full nextest green, tsc exit 0.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/dm_inbox_ingest.rs
git commit -m "ZEB-862: boot-load first_observed sidecar into relay-hold + DM-inbox docs"
```

---

## Notes for the implementer

- The sidecar functions are near-verbatim clones of the `*_replay` functions already in each persist file — read `load_replay` / `save_replay` / `load_replay_or_recover` / `quarantine` in the same file and copy their structure; only the type (`BTreeMap<String, u64>` vs `BTreeMap<String, Hlc>`), the wording, and the const names change.
- Do NOT remove `#[serde(skip)]` from `first_observed_ms` — that attribute is what keeps the local clock off the CRDT wire. Durability comes from the sidecar, not from serializing the field.
- The three on-disk files (`*.cbor`, `*_replay.cbor`, `*_first_observed.cbor`) are written non-atomically relative to each other; this is intentional and safe (see spec "Error handling"). Do not add cross-file locking.
