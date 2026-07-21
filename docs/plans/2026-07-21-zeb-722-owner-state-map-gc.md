# ZEB-722 — owner-state map GC on burn Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** GC the grow-only `file_deks` / `file_grants` owner-state maps when a personal file is burned, converging across the owner's devices via a permanent `burned_content` tombstone.

**Architecture:** Add `OwnerState.burned_content: BTreeSet<[u8;32]>` — a permanent burn tombstone mirroring the existing `SpaceId` `tombstones` set. `burn_content`'s `RuntimeAction::Burn(cid)` arm (fires only when the last sidecar reference to a CID is gone) records the tombstone and drops the CID's DEK + grant list; `merge_remote_into_local` unions the tombstone then sweeps the maps against it, so a stale sibling device cannot resurrect a burned entry on the add-wins union. Permanent + HLC-free is safe because encrypted ingest mints a random DEK ⇒ a burned CID is cryptographically unreproducible (full rationale in the spec).

**Tech Stack:** Rust, Tauri 2, `serde`/`ciborium` (owner-state CBOR), `tokio`, `cargo-nextest`.

**Spec:** `docs/specs/2026-07-21-zeb-722-owner-state-map-gc-design.md`

## Global Constraints

- CI-exact gates run from `src-tauri/`:
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  - `cargo fmt --all -- --check`
- `--locked`, `--all-targets`, `--features test-fixtures` are all load-bearing (CLAUDE.md).
- Tests must be **keychain-free** (never `KeychainStore::new()`; use derived `KeyTree` / `grantable_state`) and **wall-clock-free** (no timing gates).
- Iterative dev may use `scripts/test-select --context task` — paste its printed `round=… bucket=…` summary line into the task report so the selection is auditable (Qodo rule 1601747); the final pre-PR sweep is the full `--workspace --all-targets` commands above.
- `burned_content` serde tag is `"bt"` (free — existing tags: sp, ob, ib, mk, tm, od, lb, ot, fg, rd, fd, fr, rg). Absent-when-empty (`skip_serializing_if` + `default`) so pre-ZEB-722 snapshots load empty — no schema-version bump.
- Do NOT touch the `INFO_FRIEND_AEAD` derivation string or the `friend_aead` field (out of scope — deferred; renaming breaks `FleetKeyMaterial` CBOR).

---

## File Structure

- `src-tauri/src/owner_state_crdt.rs` — the `OwnerState` struct: add the `burned_content` field + a `burn_gc` helper method.
- `src-tauri/src/owner_state_persist.rs` — the `CrdtFileV2` persisted mirror + its two `From` impls: add the field (compile-forced).
- `src-tauri/src/owner_state_sync.rs` — `merge_remote_into_local`: destructure the field (compile-forced) + union + sweep; add merge tests.
- `src-tauri/src/lib.rs` — `burn_content`: extract a `burn_content_impl(&Mutex<NodeState>)` seam and hook the GC in the `Burn(cid)` arm; add an end-to-end test using the `grantable_state` harness.
- `src-tauri/src/file_sharing.rs` — tighten the `sealed_blob_is_not_the_raw_dek` test (review nit).

---

### Task 1: `burned_content` field, persistence mirror, convergent merge, and `burn_gc` helper

This is the core mechanism. Adding the field to `OwnerState` compile-forces the `merge_remote_into_local` destructure and the `From<CrdtFileV2> for OwnerState` construction, so all three files change together and the crate compiles as one independently-testable deliverable.

**Files:**
- Modify: `src-tauri/src/owner_state_crdt.rs` (struct field after line 153; `burn_gc` method in the existing `impl OwnerState`)
- Modify: `src-tauri/src/owner_state_persist.rs` (`CrdtFileV2` field after line 147; both `From` impls, ~166 and ~186)
- Modify: `src-tauri/src/owner_state_sync.rs` (destructure ~264; union+sweep after the `file_grants` loop, ~481; tests in the existing `#[cfg(test)] mod`)

**Interfaces:**
- Produces: `OwnerState.burned_content: BTreeSet<[u8; 32]>`; `OwnerState::burn_gc(&mut self, cid: [u8; 32])`. Task 2 consumes `burn_gc`.

- [ ] **Step 1: Write the failing test — `burn_gc` helper + merge convergence + resurrection sweep**

Add to the `#[cfg(test)] mod tests` in `owner_state_sync.rs` (it already `use super::*;` and imports `OwnerState`, `Hlc`; `merge_remote_into_local` is `super::merge_remote_into_local`). Mirror the existing `merge_remote_into_local_convergence_after_create_and_delete`:

```rust
#[test]
fn burn_gc_records_tombstone_and_drops_maps() {
    use crate::owner_state_types::{GrantEntry, OwnerAddr};
    let cid = [0x7au8; 32];
    let mut s = OwnerState::default();
    s.file_deks.insert(cid, vec![1, 2, 3]);
    // GrantEntry timestamps are u64 wall-clock millis (ZEB-725), not Hlc.
    s.file_grants.insert(
        cid,
        vec![GrantEntry {
            grantee_owner: OwnerAddr([0x0b; 16]),
            granted_at: 1,
            revoked_at: 0,
        }],
    );
    s.burn_gc(cid);
    assert!(!s.file_deks.contains_key(&cid), "DEK dropped");
    assert!(!s.file_grants.contains_key(&cid), "grants dropped");
    assert!(s.burned_content.contains(&cid), "tombstone recorded");
}

#[test]
fn merge_sweeps_burned_cid_and_is_order_independent() {
    let cid = [0x7bu8; 32];

    // Device A burned the CID (tombstone present, maps empty).
    let mut a = OwnerState::default();
    a.burn_gc(cid);

    // Device B still holds the DEK for that CID (stale sibling).
    let mut b = OwnerState::default();
    b.file_deks.insert(cid, vec![9, 9, 9]);

    // A merges B: the union re-adds file_deks[cid], the sweep drops it again.
    let mut a1 = a.clone();
    super::merge_remote_into_local(&mut a1, b.clone());
    assert!(!a1.file_deks.contains_key(&cid), "burned CID must not resurrect on merge");
    assert!(a1.burned_content.contains(&cid), "tombstone retained");

    // B merges A: B learns the tombstone and sweeps its own entry — converges.
    let mut b1 = b.clone();
    super::merge_remote_into_local(&mut b1, a.clone());
    assert!(!b1.file_deks.contains_key(&cid), "sibling sweeps on learning the tombstone");
    assert!(b1.burned_content.contains(&cid), "tombstone propagated");

    // Both directions reached the same state.
    assert_eq!(a1.file_deks, b1.file_deks);
    assert_eq!(a1.burned_content, b1.burned_content);
}
```

`GrantEntry` (owner_state_types.rs): `grantee_owner: OwnerAddr` (a `([u8;16])` newtype), `granted_at: u64`, `revoked_at: u64` (wall-clock millis, ZEB-725) — the literal above matches.

- [ ] **Step 2: Run tests to verify they fail (no `burned_content` / `burn_gc` yet)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(burn_gc_records_tombstone) | test(merge_sweeps_burned_cid)'`
Expected: FAIL to compile — `no field burned_content` / `no method burn_gc`.

- [ ] **Step 3: Add the `burned_content` field to `OwnerState`**

In `owner_state_crdt.rs`, immediately after the `received_file_grants` field (line 153):

```rust
    /// ZEB-722: CIDs of encrypted personal files that have been BURNED (the
    /// last sidecar reference removed). A permanent tombstone: it GCs the
    /// grow-only `file_deks` / `file_grants` entries for the CID and keeps a
    /// stale sibling device from resurrecting them on the add-wins union merge
    /// (`owner_state_sync::merge_remote_into_local` sweeps the maps against this
    /// set after unioning them).
    ///
    /// Permanent (never un-set) and HLC-free is SAFE: encrypted ingest mints a
    /// fresh RANDOM DEK (`file_sharing::generate_file_dek` = `EpochKey::random`)
    /// and ZEB-726 derives the frame nonce from the DEK, so re-ingesting
    /// identical plaintext yields different ciphertext → a DIFFERENT CID. A
    /// burned CID is therefore cryptographically unreproducible; it can never
    /// re-appear as a live entry, so there is no "re-ingest after burn" race to
    /// arbitrate. Absent on the wire when empty (`skip_serializing_if` +
    /// `default`) so pre-ZEB-722 snapshots load empty.
    #[serde(rename = "bt", skip_serializing_if = "BTreeSet::is_empty", default)]
    pub burned_content: BTreeSet<[u8; 32]>,
```

(`BTreeSet` is already imported at line 6.)

- [ ] **Step 4: Add the `burn_gc` helper to `impl OwnerState`**

In the existing `impl OwnerState` block in `owner_state_crdt.rs` (the one with `apply_space` / `tombstone_space`), add:

```rust
    /// ZEB-722: GC the owner-side file maps when a personal file is burned (its
    /// last sidecar reference removed). Records a permanent `burned_content`
    /// tombstone so the removal converges across the owner's devices — a stale
    /// sibling cannot resurrect the entry on the add-wins union merge — then
    /// drops the CID's sealed DEK and grant list. The caller MUST `notify_dirty`
    /// afterward (ZEB-709) or the mutation is neither persisted nor replicated.
    pub fn burn_gc(&mut self, cid: [u8; 32]) {
        self.burned_content.insert(cid);
        self.file_deks.remove(&cid);
        self.file_grants.remove(&cid);
    }
```

- [ ] **Step 5: Wire the persisted mirror (`CrdtFileV2`) — compile-forced**

In `owner_state_persist.rs`, after the `received_file_grants` field (line 147):

```rust
    /// ZEB-722: persisted burn tombstones (root CID bytes). Absent in
    /// pre-ZEB-722 V2 files; `serde(default)` loads those as empty (no
    /// schema-version bump — absent == empty). `skip_serializing_if` keeps
    /// existing file shapes compact.
    #[serde(skip_serializing_if = "BTreeSet::is_empty", default)]
    burned_content: BTreeSet<[u8; 32]>,
```

In `From<&OwnerState> for CrdtFileV2` (after line 165): `burned_content: s.burned_content.clone(),`
In `From<CrdtFileV2> for OwnerState` (after line 185): `burned_content: f.burned_content,`

- [ ] **Step 6: Destructure + union + sweep in `merge_remote_into_local`**

In `owner_state_sync.rs`, add `burned_content` to the `let OwnerState { … } = remote;` destructure (after `received_file_grants,` ~line 264).

Then, immediately **after** the `file_grants` union loop (ends ~line 481, before the `received_file_grants` loop), add:

```rust
    // ZEB-722: burn tombstones — GROW-ONLY set union, then SWEEP the owner-side
    // file maps. Placed AFTER the file_deks + file_grants union loops so a
    // first-writer-wins `file_deks` re-add (or a grant union) from a stale
    // sibling is immediately swept back out, and a tombstone arriving in THIS
    // merge also cleans a pre-existing local entry. `received_file_grants` is
    // intentionally NOT swept (different trigger — burn never reaches it; see
    // ZEB-727). The disjoint-field `retain` mirrors the `revoked_dm_devices`
    // GC-on-de-friend prune above (both capture one `local` field in the closure
    // while retaining another).
    local.burned_content.extend(burned_content);
    local
        .file_deks
        .retain(|cid, _| !local.burned_content.contains(cid));
    local
        .file_grants
        .retain(|cid, _| !local.burned_content.contains(cid));
```

- [ ] **Step 7: Run the new tests + a persist round-trip check**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(burn_gc_records_tombstone) | test(merge_sweeps_burned_cid) | test(owner_state_persist) | package(harmony-app) and test(persist)'`
Expected: PASS. If `owner_state_persist.rs` has a round-trip test that asserts field-count or exact bytes, extend it to seed `burned_content` and assert it survives the `CrdtFileV2` round-trip; otherwise the merge/gc tests suffice for this task.

- [ ] **Step 8: Task gate — clippy + fmt + scoped tests**

Run:
```
cd src-tauri && cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --features test-fixtures -E 'package(harmony-app) and (test(burn) | test(merge_) | test(owner_state))'
```
Expected: fmt clean, clippy 0 warnings, tests green.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/owner_state_crdt.rs src-tauri/src/owner_state_persist.rs src-tauri/src/owner_state_sync.rs
git commit -m "feat(zeb-722): burned_content tombstone + convergent merge sweep + burn_gc"
```

---

### Task 2: hook the GC into `burn_content` (extract `burn_content_impl` seam)

**Files:**
- Modify: `src-tauri/src/lib.rs` — refactor `burn_content` (line 19591) into a thin `#[tauri::command]` over `burn_content_impl(sidecar_id, &Mutex<NodeState>)`; add the GC in the `Burn(cid)` arm; add a test in the file-sharing `#[cfg(test)] mod` (the module holding `grantable_state`, ~line 20670).

**Interfaces:**
- Consumes: `OwnerState::burn_gc` (Task 1); `SyncEngine::notify_dirty(&self)` (owner_state_sync.rs:171); `send_ingest_with_name` (lib.rs:21246); `grantable_state` / `RecordingStore` / `CID` / const helpers in the file-sharing test module.

- [ ] **Step 1: Write the failing test — burn GCs the owner-state maps end to end**

In the file-sharing test module (where `grantable_state`, `CID`, `RecordingStore` live), add:

```rust
#[tokio::test]
async fn burn_content_gcs_owner_state_maps() {
    let store = std::sync::Arc::new(RecordingStore::default());
    let state = grantable_state(store); // seeds crdt_state.file_deks[CID]

    // Seed a grant for the same CID so we can assert it's GC'd too.
    let (content_index, crdt) = {
        let g = state.lock().unwrap();
        (g.content_index.clone(), g.crdt_state.clone().unwrap())
    };
    crdt.lock().await.file_grants.insert(CID, Vec::new());

    // A single sidecar entry pointing at CID → burning it makes CID's last
    // reference disappear → RuntimeAction::Burn fires.
    let res = send_ingest_with_name(&content_index, CID, "f.bin".into(), 3, None)
        .await
        .expect("sidecar insert");
    // IngestResult.sidecar_id is already a String (lib.rs:19127 / :21301).
    let burned = burn_content_impl(res.sidecar_id, &state).await.expect("burn");
    assert!(burned, "burn returns true when an entry was removed");

    let owner = crdt.lock().await;
    assert!(!owner.file_deks.contains_key(&CID), "DEK GC'd on burn");
    assert!(!owner.file_grants.contains_key(&CID), "grants GC'd on burn");
    assert!(owner.burned_content.contains(&CID), "burn tombstone recorded");
}

#[tokio::test]
async fn burn_content_keeps_dek_while_a_sibling_reference_remains() {
    // TWO sidecar entries point at the same CID. Burning ONE leaves a live
    // reference, so RuntimeAction::Burn does NOT fire and the DEK must survive —
    // this pins the "GC only when the CID is fully burned" invariant (the reason
    // the GC lives in the Burn arm, not the entry-removal step).
    let store = std::sync::Arc::new(RecordingStore::default());
    let state = grantable_state(store); // crdt_state.file_deks[CID] seeded
    let content_index = state.lock().unwrap().content_index.clone();

    let first = send_ingest_with_name(&content_index, CID, "a.bin".into(), 3, None)
        .await
        .expect("first ref");
    let _second = send_ingest_with_name(&content_index, CID, "b.bin".into(), 3, None)
        .await
        .expect("second ref");

    let burned = burn_content_impl(first.sidecar_id, &state).await.expect("burn");
    assert!(burned, "the entry was removed");

    let crdt = state.lock().unwrap().crdt_state.clone().unwrap();
    let owner = crdt.lock().await;
    assert!(owner.file_deks.contains_key(&CID), "DEK retained while a sibling ref remains");
    assert!(!owner.burned_content.contains(&CID), "no tombstone until the last ref is burned");
}
```

`content_verb_tx` / `sync_engine` are `None` in `grantable_state` — the `Burn` arm handles a `None` verb_tx (warns) and the GC's `notify_dirty` is guarded by `if let Some(engine)`, so the test needs neither. (If `send_ingest_with_name`'s `entries_for_cid` book-keeping requires the two same-CID entries to be distinguishable, they already are — each call mints a fresh `SidecarId`.)

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(burn_content_gcs_owner_state_maps)'`
Expected: FAIL to compile — `burn_content_impl` doesn't exist yet.

- [ ] **Step 3: Extract the `burn_content_impl` seam**

Rename the existing `async fn burn_content(sidecar_id, state: tauri::State<…>)` body into:

```rust
pub(crate) async fn burn_content_impl(
    sidecar_id: String,
    state: &Mutex<NodeState>,
) -> Result<bool, String> {
    // …entire existing burn_content body, verbatim (its `state.lock()` calls
    // work unchanged on `&Mutex<NodeState>`)…
}
```

and make the command a thin delegator (mirror the `grant_read` → `grant_read_impl` command/impl split already in this file):

```rust
#[tauri::command]
async fn burn_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    burn_content_impl(sidecar_id, &state).await
}
```

- [ ] **Step 4: Pull the CRDT + sync handles and hook the GC**

In `burn_content_impl`, extend the existing handle-pull block (was `(index, maybe_verb_tx)` at ~19616):

```rust
    let (index, maybe_verb_tx, crdt_state, sync_engine) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (
            guard.content_index.clone(),
            guard.content_verb_tx.clone(),
            guard.crdt_state.clone(),
            guard.sync_engine.clone(),
        )
    };
```

Then at the **top of the `RuntimeAction::Burn(cid)` arm** (before the runtime-verb dispatch):

```rust
        RuntimeAction::Burn(cid) => {
            // ZEB-722: the CID's last sidecar reference is gone — GC its
            // owner-state DEK + grant list via a permanent burn tombstone that
            // converges across the owner's devices. Best-effort-symmetric with
            // the runtime Burn below: a headless/early-boot node without
            // owner-state simply skips (like the `maybe_verb_tx` None path).
            if let Some(crdt) = &crdt_state {
                crdt.lock().await.burn_gc(cid);
                if let Some(engine) = &sync_engine {
                    engine.notify_dirty();
                }
            }
            // …existing runtime Burn dispatch (unchanged)…
        }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(burn_content_gcs_owner_state_maps)'`
Expected: PASS.

- [ ] **Step 6: Task gate**

Run:
```
cd src-tauri && cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --features test-fixtures -E 'package(harmony-app) and (test(burn_content) | test(grant_read) | test(list_grants))'
```
Expected: fmt clean, clippy 0, tests green (the burn test + the pre-existing file-sharing command tests through the refactored seam).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-722): GC file_deks/file_grants in burn_content (burn_content_impl seam)"
```

---

### Task 3: tighten `sealed_blob_is_not_the_raw_dek` (review nit)

**Files:**
- Modify: `src-tauri/src/file_sharing.rs` (the test at line 618).

- [ ] **Step 1: Strengthen the assertion**

Replace the body of `sealed_blob_is_not_the_raw_dek` (lines 618-629) with a real "not plaintext" proof — the raw DEK must not appear as a contiguous window anywhere in the sealed blob — plus the positive round-trip proof, keeping the structure/length assert:

```rust
    #[test]
    fn sealed_blob_is_not_the_raw_dek() {
        let tree = test_tree();
        let dek = generate_file_dek();
        let sealed = seal_dek_at_rest(&tree, &dek).expect("seal");
        // Structure: nonce(12) + ciphertext(32) + tag(16) = 60 bytes.
        assert_eq!(sealed.len(), 60);
        // The raw DEK must not appear VERBATIM anywhere in the sealed blob — a
        // length inequality alone would pass even if the DEK were embedded
        // beside padding. Scan every 32-byte window.
        let raw: &[u8] = dek.as_bytes();
        assert!(
            !sealed.windows(raw.len()).any(|w| w == raw),
            "the raw DEK must not appear verbatim in the sealed-at-rest blob"
        );
        // Positive proof: the seal is reversible under the same tree.
        let opened = open_dek_at_rest(&tree, &sealed).expect("open");
        assert_eq!(opened.as_bytes(), dek.as_bytes());
    }
```

(If `EpochKey::as_bytes` returns `&[u8; 32]`, `let raw: &[u8] = dek.as_bytes();` coerces cleanly; `w == raw` compares `&[u8]` to `&[u8]`.)

- [ ] **Step 2: Run it**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(sealed_blob_is_not_the_raw_dek)'`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/file_sharing.rs
git commit -m "test(zeb-722): tighten sealed_blob_is_not_the_raw_dek — verbatim-DEK scan + round-trip"
```

---

## Final pre-PR sweep (after all tasks)

Full CI-exact gate from `src-tauri/`:
```
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
All three green before opening the PR.
