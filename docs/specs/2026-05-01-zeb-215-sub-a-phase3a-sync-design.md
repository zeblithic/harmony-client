# ZEB-215 Sub-A Phase 3a: Owner-state sync (state-root + persistence) — Design

**Status:** Approved (2026-05-01)
**Implements:** ZEB-215 Sub-A Phase 3a (sub-scope of the original Phase 3 framing — see [Scope split](#scope-split-phase-3a-vs-3b)).
**Depends on:** Phase 1 ([crypto](2026-04-30-zeb-211-owner-state-encryption-design.md)) merged in PR #72; Phase 2 ([CRDT primitives](2026-04-30-zeb-206-nav-tree-design.md)) merged in PR #73.
**Unblocks:** Phase 3b (real harmony-content CAS), Phase 4 (Tauri IPC), Phase 5 (frontend NavService rewrite).

## Goals

Phase 3a wires Phase 1's encryption and Phase 2's typed CRDT into a working sync surface for the harmony-client desktop app, scoped down to what's testable without integrating the separate harmony-content repo.

In scope:

1. **On-disk persistence of `OwnerState`** — load at boot, save on debounced + shutdown flush, atomic-rename + fsync. New file `owner_state_crdt.cbor`.
2. **On-disk persistence of `RootReplayTracker`** — separate file `state_root_replay.cbor` to honor ZEB-211 §"Replay protection" across reboots.
3. **Zenoh state-root publisher** — encrypts via `encrypt_root_publish` (Phase 1), sends on `harmony/owner/{addr_hex}/state-root-v1` (per ZEB-211 §"Wire format").
4. **Zenoh state-root subscriber** — decrypts → validates replay → fetches encrypted blob via a `ContentStore` trait → applies via Phase 2's `apply_*` methods.
5. **`ContentStore` trait + `InMemoryStub`** — trait shape lands here so Phase 3b's harmony-content client can drop in without SyncEngine surgery; the in-memory stub is load-bearing for unit + integration tests.
6. **`SyncEngine` struct** — owns Zenoh session ref, ContentStore, debounce timer, dirty flag, references to OwnerState + RootReplayTracker. Public API: `new`, `notify_dirty`, `flush_now`, `shutdown`.

## Non-goals

These are deferred and intentionally NOT in Phase 3a's scope:

- **Real harmony-content CAS integration** — `ContentStore` trait shape lands here; only `InMemoryStub` is wired in production. Phase 3b swaps in the real client.
- **Per-entry blob CAS layout** — Phase 3a treats the entire `OwnerState` as a single content-addressed blob. See [Root blob simplification](#root-blob-shape--phase-3a-simplification). Phase 3b/c restructures into per-entry blobs for actual scale.
- **Tauri IPC commands wrapping SyncEngine** — Phase 4.
- **Frontend integration / Svelte stores listening to `nav-updated` events** — Phase 5. Phase 3a emits no Tauri events; the surface is Rust-only.
- **DM content encryption** (per-Space content keys) — Sub-B / ZEB-219.
- **Cross-internet sync** (non-LAN, NAT traversal) — current Zenoh session is LAN/mDNS; cross-NAT is a separate concern handled at the transport layer.
- **Migration of any data from the legacy `owner_state.cbor`** — the legacy file's contents (master_seed reference, identity-level data) stay where they are. The new file is purely additive.

## Scope split: Phase 3a vs 3b

The original Phase 3 framing in the Sub-A roadmap bundled "Zenoh state-root sync + harmony-content CAS persistence." This spec splits that into:

- **Phase 3a** (this spec): state-root pub/sub + on-disk persistence + `ContentStore` trait shape + `InMemoryStub`. The merge path is wired end-to-end through the trait, so Phase 3b is purely a client-implementation swap.
- **Phase 3b** (future spec): real harmony-content CAS client wired in place of `InMemoryStub`. Likely also restructures the root blob from "single OwnerState blob" to "per-entry blobs" because real CAS makes the granularity matter.

Rationale for the split:

- The harmony-content API contract is currently unverified from harmony-client's side. Bundling its integration with the state-root protocol design would mix two unknowns in one PR.
- Phase 3a is independently exercisable: a single-process integration test instantiates two `SyncEngine`s sharing one `Arc<InMemoryStub>` and verifies end-to-end CRDT convergence.
- Each phase's review surface stays tractable. Phase 2 took 5 review rounds at 16 tasks; bundling Phase 3a + 3b would likely double both.

## Module boundaries

Three new files in `src-tauri/src/`:

- **`content_store.rs`** (~150 lines) — `ContentStore` trait + `InMemoryStub` + tests. No async, no I/O dependencies.
- **`owner_state_persist.rs`** (~300 lines) — load/save both new files, atomic-rename, schema-version dispatch, write locks, error types.
- **`owner_state_sync.rs`** (~600 lines) — `SyncEngine` struct + internal task + Zenoh publisher/subscriber + debounce timer.

Phase 2's `owner_state_crdt.rs` and `owner_state_types.rs` stay untouched **except** for adding `RootPublishPayload` to `owner_state_types.rs` and to the `impl_canonical!` macro list (one new wire type joins the 15 existing ones).

Phase 1's `owner_state_crypto.rs` exposes existing functions unchanged. The legacy `owner_state.rs` is touched only to read master_seed at boot.

## Architecture

### Publisher path

A local mutation flows from an `apply_*` call (Phase 2) through `notify_dirty()` into a debounced publish. The CRDT mutation itself is synchronous; the network publish is async and happens in the SyncEngine's internal task.

```
Tauri command handler  (Phase 4 will own this; 3a's tests stand in)
  │
  ▼ state.apply_outbox(entry)               ← Phase 2 CRDT mutation, sync
  │
  ▼ engine.notify_dirty()                   ← non-blocking, sets atomic flag
  │
  └── (fast return to caller)

Internal SyncEngine task (debounce window):
  loop {
    select! {
      _ = dirty_flag.notified() => {
        // schedule wakeup at now + 250ms unless already scheduled
      }
      _ = scheduled_wakeup => {
        publish_root_now()
      }
      _ = flush_now_signal => {
        publish_root_now()
      }
      _ = shutdown_signal => break;
    }
  }

publish_root_now():
  let snapshot = self.owner_state.lock().clone();
  let now = self.next_hlc();                               ← strictly newer than last
  let root_blob_cleartext = canonical_cbor_encode(&snapshot)?;
  let root_blob_ciphertext =
      encrypt_entry(&kt, &owner_state_root_lookup_key, &root_blob_cleartext)?;
  let root_cid = ContentId(blake3::hash(&root_blob_ciphertext).into());
  self.content_store.put(root_cid, root_blob_ciphertext)?;
  let payload = canonical_cbor_encode(&RootPublishPayload { root_cid, at: now })?;
  let wire = encrypt_root_publish(&kt, &payload)?;          ← random nonce, Phase 1
  self.zenoh_publisher.put(wire).await?;
  self.persist_crdt_debounced();                            ← schedules disk save
```

The 250ms debounce is a constant `DEFAULT_DEBOUNCE_MS = 250` exposed at `SyncEngine::new` for test override. It's small enough to feel near-instant to a human and large enough to collapse keystroke-rate mutations into a single publish.

`next_hlc()` constructs the publisher's HLC as `Hlc { wall_ms, logical, device_id }` where `wall_ms = SystemTime::now()`, `device_id` is the local device's identifier (read from legacy `owner_state.cbor` at boot), and `logical` increments by one if `wall_ms` equals the previously-published HLC's `wall_ms` (handles wall-clock non-monotonicity and same-millisecond bursts) — otherwise resets to 0. This satisfies Phase 1's `is_strictly_newer_than` precondition.

### Subscriber path

Zenoh delivery on the topic enters the SyncEngine's subscriber task. The first thing it does is replay-check; only valid publishes proceed to the CAS fetch + CRDT merge.

```
Zenoh subscriber task receives bytes:
  let payload = decrypt_root_publish(&kt, &wire)?;          ← Phase 1
  let RootPublishPayload { root_cid, at } = canonical_cbor_decode(&payload)?;
  if !self.tracker.accept(&at)?:                            ← replay protection
    return;  // older or equal HLC from same publisher
  let blob_ciphertext = self.content_store.get(&root_cid)?
      .ok_or(SyncError::MissingBlob)?;
  let blob_cleartext =
      decrypt_entry(&kt, &owner_state_root_lookup_key, &blob_ciphertext)?;
  let remote: OwnerState = canonical_cbor_decode(&blob_cleartext)?;

  let mut local = self.owner_state.lock();
  for (_, space) in remote.spaces { local.apply_space_with_canonicalization(space); }
  for (_, entry) in remote.outbox { local.apply_outbox(entry); }
  for (_, entry) in remote.inbox  { local.apply_inbox(entry); }
  for (_, marker) in remote.markers { local.apply_marker(marker); }
  for tomb in remote.tombstones { local.tombstones.insert(tomb); }
  drop(local);

  self.tracker_dirty.set(true);                             ← schedule tracker save
  self.persist_crdt_debounced();                            ← schedule CRDT save
  // (no Tauri event in 3a — Phase 5 adds nav-updated)
```

The merge is a straightforward iteration: every entry in the remote `OwnerState` flows through Phase 2's CRDT methods. This relies on the CRDT properties Phase 2 already proved: `apply_*` is idempotent and commutative, so re-receiving a publish (replay tracker missed, or our own publish bouncing back) just no-ops. Cross-device dedupe (Space ULIDs colliding on dedupe_keys), envelope immutability checks, replay rejection, and all the round-1-through-5 invariants from Phase 2 fire normally during merge.

### Lifecycle

Boot sequence:

```
1. Tauri setup hook fires.
2. Legacy load_owner_state()                  → master_seed available
3. KeyTree::derive(&master_seed)              → AEAD keys
4. load_crdt_from_disk()                      → OwnerState (or empty if first run)
5. load_replay_from_disk()                    → RootReplayTracker (or empty)
6. SyncEngine::new(zenoh, content_store, kt, owner_state, tracker)
   ↳ spawns internal debounce task
   ↳ opens Zenoh subscriber on harmony/owner/{addr}/state-root-v1
7. App ready.
```

If step 2 fails (no identity yet — first run or pairing pending), steps 3-6 skip; SyncEngine starts later when identity is established. The Tauri pairing hook becomes the trigger.

Shutdown:

`SyncEngine::shutdown().await` is called explicitly by the Tauri shutdown hook. It signals the internal task to fire one last `publish_root_now()` if dirty, then synchronously writes both files (no debounce on the way out). `Drop` is best-effort only — a panic mid-task could lose the debounce window — so `shutdown()` is the documented safe path.

## State-root payload format

### Wire shape (per ZEB-211)

```rust
// In owner_state_types.rs (joins the impl_canonical! list as the 16th type):
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootPublishPayload {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,        // bstr[32], BLAKE3 of encrypted root blob
    #[serde(rename = "at")]
    pub at: Hlc,                    // strictly-newer wall_ms / logical / device_id
}

impl_canonical!(RootPublishPayload);
```

Both keys are 2 chars to satisfy the same-length-keys precondition we locked into `canonical_cbor_encode` in Phase 1.

### Zenoh topic

`harmony/owner/{addr_hex}/state-root-v1` per ZEB-211. `addr_hex` is the lowercase-hex-encoded `OwnerAddr` (32 chars for a 16-byte addr). Single global Zenoh session — reuses the existing pattern from pairing/mail/voice; no separate session for owner-state.

### Root blob shape — Phase 3a simplification

The "root blob" in 3a is the **full `OwnerState` canonical-CBOR-encoded as a single object**, encrypted via `encrypt_entry` (deterministic AEAD) using a fixed lookup key:

```rust
let owner_state_root_lookup_key =
    space_lookup_key(&kt, b"owner-state-root-blob-v1");
```

The cipher_cid of that single blob is the `root_cid` published on Zenoh.

This is a deliberate trade-off vs. the eventual shape (per-entry blobs in a Prolly Tree per ZEB-206 Flow A): every state-root publish re-encrypts and re-stores the entire OwnerState. Acceptable for Phase 3a because:

- The OwnerState is single-digit-KB sized for typical usage.
- `InMemoryStub` doesn't care about blob size.
- The CRDT merge semantics are unchanged — subscriber still iterates entries through `apply_*`.
- Phase 3b/c will refactor to per-entry CAS once harmony-content lands.

The `b"owner-state-root-blob-v1"` byte literal is versioned to give future schema migrations a clean discriminator.

## ContentStore trait

```rust
// content_store.rs
use crate::owner_state_types::ContentId;

pub trait ContentStore: Send + Sync {
    fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError>;
    fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError>;
}

#[derive(thiserror::Error, Debug)]
pub enum ContentStoreError {
    #[error("content store I/O: {0}")]
    Io(String),
}

#[derive(Default)]
pub struct InMemoryStub {
    inner: std::sync::Mutex<std::collections::HashMap<ContentId, Vec<u8>>>,
}

impl ContentStore for InMemoryStub {
    fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        self.inner.lock().unwrap().insert(cid, blob);
        Ok(())
    }
    fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
        Ok(self.inner.lock().unwrap().get(cid).cloned())
    }
}
```

`SyncEngine::new` takes `Arc<dyn ContentStore>`. For 3a, the binary wires `Arc::new(InMemoryStub::default())`. Tests construct the same Arc and share it between two SyncEngines to simulate two devices.

In production with the stub, real cross-device sync doesn't actually work end-to-end (state-roots fly via Zenoh; blob fetches no-op because the receiving stub has no entry). That non-working path is intentional — it's gated by Phase 3b.

## Persistence layer

### File locations

In the same directory as legacy `owner_state.cbor` (derived from Tauri's `app_data_dir()`):

- `owner_state_crdt.cbor` — full `OwnerState` CRDT.
- `state_root_replay.cbor` — `RootReplayTracker` map.

### On-disk format

Both files are prefixed with a 1-byte schema version. Future migrations dispatch on this byte.

```rust
const CRDT_FILE_SCHEMA_V1: u8 = 1;
const REPLAY_FILE_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct CrdtFileV1 {
    spaces: BTreeMap<SpaceId, Space>,
    outbox: BTreeMap<OutboxEntryId, OutboxEntry>,
    inbox: BTreeMap<InboxKey, InboxEntry>,
    markers: BTreeMap<SpaceId, ReadMarker>,
    tombstones: BTreeSet<SpaceId>,
}
// Wire bytes = [SCHEMA_V1, ...canonical CBOR of CrdtFileV1...]

#[derive(Serialize, Deserialize)]
struct ReplayFileV1(BTreeMap<String, Hlc>);  // device_id → last accepted at
// Wire bytes = [SCHEMA_V1, ...canonical CBOR of ReplayFileV1...]
```

The CRDT file's CBOR shape mirrors `OwnerState` field-for-field. Loading reads the version byte, dispatches to the right decoder, builds an `OwnerState`. Unknown version → loud error, no fallback (corruption beats silent data loss).

### Atomic-rename + fsync

```rust
fn save_atomically(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    let dir = path.parent().expect("no parent");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;          // fsync data
    tmp.persist(path)?;                  // atomic rename
    File::open(dir)?.sync_all()?;        // fsync directory entry
    Ok(())
}
```

### Locks

Two new `tokio::sync::Mutex` write locks:

- `OWNER_STATE_CRDT_WRITE_LOCK` — gates `owner_state_crdt.cbor` writes.
- `STATE_ROOT_REPLAY_WRITE_LOCK` — gates `state_root_replay.cbor` writes.

The legacy `OWNER_STATE_WRITE_LOCK` stays untouched. Each lock is held only across the atomic-rename block, never during CBOR encode/decode (those happen on owned bytes). The SyncEngine's internal task is the only writer for both new files.

### Error handling

- File missing → empty struct, no error (first run / fresh install).
- Schema version unknown → return `Err(PersistError::UnknownSchemaVersion)`, surface in logs, refuse to overwrite (operator intervention required — likely a downgrade scenario).
- CBOR decode error mid-file → return `Err(PersistError::Corrupt)`, refuse to overwrite (operator intervention).
- I/O error during save → log + surface to `flush_now()` caller via `Result`; debounced background saves log + retry on next debounce window.

## Testing strategy

### Unit tests

`content_store.rs`:

- `put` then `get` returns the same blob.
- `get` on unknown CID returns `Ok(None)`.
- Concurrent `put`s from multiple threads land all blobs (mutex correctness).

`owner_state_persist.rs`:

- Round-trip `OwnerState` → bytes → `OwnerState`, equality via `PartialEq` (already derived in Phase 2).
- Round-trip `RootReplayTracker` → bytes → tracker, equality.
- Schema-version-byte mismatch returns `Err(UnknownSchemaVersion)`.
- Missing file → empty struct.
- Truncated file (last byte chopped) returns `Err(Corrupt)`.
- Atomic-rename survives partial writes: write, kill mid-write (drop the `NamedTempFile` without `persist`), reload, expect previous-good content.

`owner_state_sync.rs`:

- `notify_dirty` followed by 250ms wait fires exactly one publish.
- 50 rapid `notify_dirty` calls in a 100ms window fire exactly one publish (debounce collapse).
- `flush_now()` while debounce is pending fires immediately and cancels the pending wakeup.
- `shutdown()` after a `notify_dirty` flushes the pending publish before returning.
- Subscriber rejects a publish with `at` HLC older than `tracker[device_id]`.
- Subscriber accepts a publish with strictly-newer `at` HLC and updates the tracker.
- Subscriber on `get` returning `None` logs and skips (doesn't panic, doesn't poison the subscriber task).
- Replay tracker survives a save→reload cycle (uses real persist module, not a mock).

### Integration tests

Two `SyncEngine`s sharing one `Arc<InMemoryStub>` and one `Arc<ZenohSession>` simulate two devices. Each has its own `KeyTree` derived from the same master_seed (matching the "two bound devices" model from Phase 2's crypto integration tests).

- **E2E convergence:** Device A applies a Space → notifies dirty → publishes. Device B's subscriber receives → merges. After both `flush_now`, both `OwnerState`s are equal under `PartialEq`.
- **Bidirectional convergence:** A and B both mutate concurrently → both publish → both subscribe → both converge to the union (CRDT property).
- **Cross-device dedupe through sync:** A creates DM with id=5, B creates DM with id=1 (same sorted members), both publish, both subscribe → both converge on id=1 with id=5 collapsed (this is the round-3 scenario from Phase 2 we documented but couldn't actually exercise without sync).
- **Old peer ack scenario from PR #73 round 5:** A and B's DMs collapse via dedupe; a third "lagging" SyncEngine C still on id=5 publishes an outbox ack referencing id=5 → A and B accept the merge with their canonicalized id=1 and the ack lands.
- **Replay rejection:** A publishes; B accepts; A's same publish replayed (Zenoh reorder) → B's tracker rejects.
- **Crash + restart:** A publishes 10 times, B accepts all 10. Simulate B crash by dropping its SyncEngine without `shutdown()`. Reload B's CRDT + tracker from disk. Verify B has all 10 applied AND tracker is at A's latest HLC (no replay window after restart).
- **InMemoryStub blob miss:** Drop a blob from the stub between publish and receive, expect subscriber to log + skip.

### Property-style coverage

Handwritten (no `proptest` crate dep): 100x repeat, random sequence of mutations interleaved with publishes/receives, assert final state equality across both devices. Catches non-deterministic merge bugs.

### Out of scope for 3a tests

- Real network conditions (Zenoh in-process loopback only; no UDP / mDNS path).
- Real harmony-content (covered by 3b).
- Multi-process tests (single-process `tokio::test` only).
- Frontend Tauri event tests (no events emitted in 3a).

## Open questions / future work

Items deliberately deferred and worth surfacing in Phase 3b's brainstorm:

- **Per-entry blob CAS layout** — when harmony-content lands, the single-root-blob model becomes a scaling bottleneck (O(state_size) per publish). Phase 3b should restructure into per-entry blobs, likely matching the Prolly Tree shape ZEB-206 Flow A documents. The `b"owner-state-root-blob-v1"` versioning gives us a discriminator.
- **HLC source for publishes** — the publisher's `next_hlc()` needs a strictly-monotonic source. Phase 3a uses a simple `(wall_ms, logical_counter, device_id)` triple where `logical_counter` bumps on equal `wall_ms`. Cross-device clock skew handling (NTP drift, etc.) is bounded by the strictly-newer rule but worth measuring.
- **Tauri event surface** — Phase 5's `nav-updated` event. Phase 3a leaves a clean hook (a callback in `SyncEngine` that defaults to no-op).
- **Backpressure on the subscriber** — if a peer floods publishes, the subscriber currently processes them as fast as Zenoh delivers. Phase 3b/c should bound the queue or rate-limit.
- **Multi-bound-device coordination** — pairing already exists. Phase 3a uses what's there. Whether all bound devices need their own `state-root` topic share or whether they share a single per-identity topic is settled by ZEB-211 — single shared topic per identity, all devices publish + subscribe.

## Dependencies on other tickets

- **ZEB-219** (DM content encryption design) — does not block Phase 3a. It blocks Sub-B (DMs) which is downstream.
- **harmony-content** — the upstream repo's API contract is the gating dependency for Phase 3b. Phase 3a's `ContentStore` trait is shaped to make that swap clean.
