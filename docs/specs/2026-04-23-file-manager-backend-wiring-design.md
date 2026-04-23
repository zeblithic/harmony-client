# File Manager Backend Wiring (ZEB-146)

**Status:** Draft
**Scope:** `harmony-client` — `list_content`, `pin_content`, `unpin_content`,
`burn_content`, `archive_content` Tauri commands + a new `set_replication_tier`
command, plus a frontend authoritative-list change in
`file-manager-service.ts`.
**Linear:** [ZEB-146](https://linear.app/zeblith/issue/ZEB-146)
**Depends on:** a small upstream PR to `harmony-runtime` exposing
`NodeRuntime::storage_tier()` / `_mut()` accessors. That PR must merge first.

## Background

The File Manager UI in the Svelte client ships with a rich surface — facets
(Category / Status / Replication Tier), a storage-buddies bar, a quota summary,
and bulk actions for burn / archive / publish / release / pin / unpin. The
Rust commands backing those actions are no-op stubs:

* `list_content()` returns `Vec::new()`.
* `pin_content` / `unpin_content` / `burn_content` / `archive_content`
  return `Ok(true)` without touching any state.

`FileManagerService` in TypeScript seeds 7 mock items on boot, and
`connectAdapter()` doesn't clear them when the empty real list arrives. The
result is a UI that looks fully functional but whose buttons silently do
nothing.

This spec wires real backend state for every File Manager verb except those
that cross the mesh (publish/release — out of scope).

## Design decisions

The design was shaped by five scope questions during brainstorming:

1. **How rich should `list_content` be?** → **B. Client-side metadata
   sidecar.** A new `content_index.rs` module persists ingest-time metadata
   (`file_name`, `stored_at`, `sensitivity`, `replication_tier`, `licensed`,
   `archived`) to `app_data_dir/content-index.json`. `list_content` joins
   sidecar entries with runtime cache state for `pinned` and `size_bytes`.
   Disk/archive tier stays disabled — this is pure metadata.

2. **Which CIDs populate the list?** → **Self-ingested only.** Announced-on-
   mesh CIDs stay in the existing `announcedCids` bucket (separate UI pane).
   Transit/opportunistic cache entries (avatar fetches, mail body blobs) are
   implementation details and don't surface in Files.

3. **Folders (`parent_cid` / `is_folder` / `create_folder`)?** → **Deferred.**
   The flat-list case is most of the value; folder hierarchy deserves its own
   spec for tree-invariant design.

4. **`burn` and `archive` semantics in a RAM-only client?** → **Both lean on
   the natural tier behavior.** Burn removes the sidecar entry and unpins the
   cache slot; blob lingers in RAM until W-TinyLFU evicts it. Archive flips
   `archived: true` on the sidecar (UI hides it); no byte movement, since no
   cold tier is configured. Mesh retraction on burn is a future-work item.

5. **One unified verb channel, or per-verb channels?** → **Unified
   `ContentVerbRequest` enum channel**, carrying `Pin` / `Unpin` / `Burn` /
   `PinnedSet` variants. Consistent with `FollowRequest` (already an enum)
   and keeps the event-loop arm count flat as we add verbs.

## Architecture

Three layers collaborate:

1. **`content_index.rs` — new Rust sidecar module.**
   Owns a `ContentIndex` that holds `HashMap<[u8; 32], ContentIndexEntry>`
   persisted to JSON at `app_data_dir/content-index.json`. Same shape and
   load/save pattern as `follows.rs`. Wrapped in `Arc<Mutex<_>>` for command
   access.

2. **`ContentVerbRequest` enum channel — new, in `event_loop.rs`.**
   Single `mpsc::Sender<ContentVerbRequest>` piped into the event loop
   alongside `ingest_rx` / `fetch_rx`. Variants that touch the runtime cache
   (`Pin`, `Unpin`, `Burn`, `PinnedSet`) are handled here. Verbs that are pure
   sidecar mutations (`archive`, `set_replication_tier`) don't use the
   channel — they run directly against the `Arc<Mutex<ContentIndex>>` from
   the Tauri command handler.

3. **Tauri commands — `lib.rs`.** Each command either:
   - Writes the sidecar directly, OR
   - Sends a request on the verb channel and awaits the reply, OR
   - Does both (e.g., `burn_content` = remove sidecar + unpin in runtime).

**Invariant:** The sidecar is authoritative for "is this a user file"
(membership) and for `size_bytes` (content is immutable by CID, so the size
computed at ingest never drifts). The runtime cache is authoritative for
`pinned` (pin is an eviction-policy concept the cache owns). Storing size
in the sidecar also means a W-TinyLFU eviction of the ingested bytes
doesn't turn the UI row into a "0-byte file" — the entry still displays
correctly; the blob is simply no longer resident and would be re-fetched on
open.

## `content_index.rs`

```rust
pub struct ContentIndex {
    dir: PathBuf,
    entries: HashMap<[u8; 32], ContentIndexEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ContentIndexEntry {
    pub cid: [u8; 32],
    pub file_name: String,
    pub size_bytes: u64,
    pub stored_at_ms: u64,
    pub sensitivity: Sensitivity,
    pub replication_tier: ReplicationTier,
    pub licensed: bool,
    pub archived: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum Sensitivity { Private, Confidential, Public }

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum ReplicationTier { Minimal, Default, Durable }
```

**Public API:**

* `ContentIndex::load(dir: &Path) -> Self` — reads `content-index.json`.
  Missing file → empty. Malformed file → log warning, start empty (mirrors
  `FollowManager::load`). Duplicate CIDs in the serialized form → last-write-
  wins, log warning.
* `fn save(&self) -> io::Result<()>` — writes the full map to disk.
* `fn insert(&mut self, entry: ContentIndexEntry) -> bool` — dedup by CID;
  returns `true` if the map changed.
* `fn remove(&mut self, cid: &[u8; 32]) -> bool` — returns `true` if the
  entry existed.
* `fn set_archived(&mut self, cid: &[u8; 32], archived: bool) -> bool` —
  returns `true` if the flag changed (idempotent when already set).
* `fn set_replication_tier(&mut self, cids: &[[u8; 32]], tier: ReplicationTier)
  -> usize` — returns the count of entries updated.
* `fn entries(&self) -> impl Iterator<Item = &ContentIndexEntry>`.
* `fn get(&self, cid: &[u8; 32]) -> Option<&ContentIndexEntry>`.

**Persistence format:** serde JSON of a `Vec<ContentIndexEntry>`; the CID
field serializes as hex at the serde boundary so the file is human-readable
and the same hex representation round-trips through the Tauri wire.

**Concurrency:** `Arc<Mutex<ContentIndex>>`, constructed in `lib.rs::run()`
next to `follow_mgr`, injected into whichever commands need it.

## `ContentVerbRequest` channel

```rust
pub enum ContentVerbRequest {
    Pin { cid: [u8; 32], reply: oneshot::Sender<Result<bool, String>> },
    Unpin { cid: [u8; 32], reply: oneshot::Sender<Result<bool, String>> },
    Burn { cid: [u8; 32], reply: oneshot::Sender<Result<bool, String>> },
    PinnedSet { reply: oneshot::Sender<HashSet<[u8; 32]>> },
}
```

**Event-loop handler** (new arm in the `tokio::select!` block, next to
`ingest_rx.recv()`):

```rust
Some(req) = content_verb_rx.recv() => {
    match req {
        ContentVerbRequest::Pin { cid, reply } => {
            let id = ContentId::from_bytes(cid);
            let ok = runtime.storage_tier_mut().cache_mut().pin(id);
            let _ = reply.send(Ok(ok));
        }
        ContentVerbRequest::Unpin { cid, reply } => {
            let id = ContentId::from_bytes(cid);
            runtime.storage_tier_mut().cache_mut().unpin(&id);
            let _ = reply.send(Ok(true));
        }
        ContentVerbRequest::Burn { cid, reply } => {
            let id = ContentId::from_bytes(cid);
            runtime.storage_tier_mut().cache_mut().unpin(&id);
            let _ = reply.send(Ok(true));
        }
        ContentVerbRequest::PinnedSet { reply } => {
            let cache = runtime.storage_tier().cache();
            let pinned: HashSet<[u8; 32]> = cache
                .iter_admitted()
                .filter(|id| cache.is_pinned(id))
                .map(|id| id.to_bytes())
                .collect();
            let _ = reply.send(pinned);
        }
    }
}
```

**Upstream prerequisite:** `harmony-runtime::NodeRuntime` today doesn't expose
`storage_tier()` / `storage_tier_mut()`. A small PR to `harmony` must add
those accessors before this event-loop change compiles. `StorageTier<B>` is
already a public type; precedent is `NodeRuntime::metrics()` returning
`&StorageMetrics`.

## Tauri wire contract

**`list_content() -> Vec<ContentItemWire>`** (async):

```rust
#[serde(rename_all = "camelCase")]
pub struct ContentItemWire {
    pub cid: String,              // hex
    pub name: String,
    pub size_bytes: u64,          // from sidecar (set at ingest, never drifts)
    pub stored_at: u64,           // ms since epoch
    pub sensitivity: String,      // "private" | "confidential" | "public"
    pub replication_tier: String, // "minimal" | "default" | "durable"
    pub pinned: bool,             // from PinnedSet snapshot
    pub licensed: bool,
    pub archived: bool,
}
```

Implementation:

1. Ask the event loop for the current pinned CID set via
   `ContentVerbRequest::PinnedSet`.
2. Iterate the sidecar; for each entry, set `pinned = pinned_set.contains(cid)`.
3. Return the joined list.

**Verb commands** (`async`, each routes through the verb channel or
sidecar):

| Command | Runtime? | Sidecar? | Return |
|---|---|---|---|
| `pin_content(cid)` | `Pin { cid }` | — | `Result<bool, String>` — `false` if pin quota exhausted |
| `unpin_content(cid)` | `Unpin { cid }` | — | `Result<bool, String>` — always `Ok(true)` |
| `burn_content(cid)` | `Burn { cid }` | `remove(cid)` | `Result<bool, String>` — `true` if sidecar had it |
| `archive_content(cid)` | — | `set_archived(cid, true)` | `Result<bool, String>` — `true` if flag flipped |
| `set_replication_tier(cids, tier)` | — | `set_replication_tier(&cids, tier)` | `Result<u32, String>` — count updated |

**Error paths:**
* Invalid hex CID → `Err("invalid cid")`.
* Sidecar entry missing on burn/archive/set_replication_tier → `Ok(false)` /
  `Ok(0)` (idempotent).
* Runtime channel closed → `Err("runtime unavailable")`.

**`ingest_content` change:** after the runtime reply, the Tauri command
acquires the `ContentIndex` lock and inserts a new entry with `file_name`
from the dialog, `size_bytes` from the file metadata (already computed
pre-ingest), `stored_at_ms = now()`, defaults for `sensitivity: Private`,
`replication_tier: Default`, `licensed: false`, `archived: false`.

## Frontend changes (`file-manager-service.ts`)

**`connectAdapter` becomes authoritative:**

```ts
async connectAdapter(adapter: TauriAdapter): Promise<void> {
  if (this.adapter) return;
  this.adapter = adapter;

  const real = (await adapter.invoke('list_content')) as ContentItemWire[];
  this.privateContent = real.map(wireToContentItem);
  this.onChange?.();

  const unlisten = await adapter.listen('content-announced', /* … */);
  this.unlisteners.push(unlisten);
}
```

Mocks are replaced unconditionally — even when `real.length === 0` — per the
explicit ticket requirement. The existing `mock-file-data.ts` stays on disk
for Storybook/Playwright visual fixtures, but production code paths no longer
seed from it for `privateContent`.

**`wireToContentItem` (new pure helper)** maps `ContentItemWire` →
`ContentItem` and fills runtime-unknown fields with stable defaults:

| Field | Source |
|---|---|
| `cid`, `name`, `sizeBytes`, `sensitivity`, `replicationTier`, `licensed`, `pinned` | wire |
| `storedAt` | wire |
| `category` | `inferCategory(name)` — existing helper |
| `lastAccessed` | defaults to `storedAt` |
| `accessCount` | `0` |
| `stalenessScore` | `0` |
| `replicaCount` | `1` |
| `parentCid` | `null` (folders deferred) |
| `isFolder` | `false` |

**Service methods become authoritative-result, not fire-and-forget.** Today's
`invoke(...).catch(() => {})` pattern silently swallows pin-quota errors and
is exactly the "lies to the user" failure mode ZEB-146 targets. New pattern
across `pin` / `unpin` / `burn` / `archive` / `setReplicationTier`:

```ts
async pin(cid: string): Promise<void> {
  const ok = await this.adapter!.invoke('pin_content', { cid }) as boolean;
  if (ok) {
    const item = this.privateContent.find(i => i.cid === cid);
    if (item) item.pinned = true;
    this.onChange?.();
  } else {
    // surface pin-quota error to caller — UI shows toast
    throw new Error('pin quota exhausted');
  }
}
```

Publish / release stay mocked (mesh-publish path is out of scope).

## Testing

**Rust unit tests** (inline in each module):

* `content_index.rs` — roundtrip save/load, malformed-JSON recovery starts
  empty, duplicate-CID dedup, archive toggle idempotency,
  `set_replication_tier` count correctness, `remove` returns `false` on
  missing CID.
* `lib.rs` commands — mock the verb channel receiver, assert each command
  sends the expected request variant and propagates the reply; covers
  invalid-hex error path, channel-closed error path, `set_replication_tier`
  argument validation.

**Rust integration test** (`src-tauri/tests/content_index_integration.rs`,
new file):

End-to-end round-trip following the `mail_sync_integration.rs` pattern:

1. Spin up an in-process Zenoh session and a real
   `NodeRuntime<MemoryBookStore>` via `event_loop::run`.
2. Ingest bytes via the `IngestRequest` channel.
3. Write a sidecar entry for the same CID (what `ingest_content` would do
   after runtime ack).
4. Send a `PinnedSet` request on the verb channel; assert the CID is *not*
   in the set (fresh ingests are not auto-pinned).
5. Send `Pin { cid }`; send `PinnedSet`; assert the CID is now in the set.
6. Send `Burn { cid }`; remove sidecar entry; send `PinnedSet`; assert CID
   is no longer in the set and sidecar is empty.

This is the "ingest → list → mutate" round-trip ZEB-146 explicitly calls
out.

**What we skip:**

* Playwright/WebView2 UI test — blocked on ZEB-150 infra.
* Multi-client replication/sync — archive tier is off, no cross-machine
  behavior changes.
* Pin-quota-under-load stress tests — `ContentStore::pin_limit` is a
  function of cache capacity config; edge-case tests belong with a future
  cache-config spec.

**Regression protection:** existing `tests/mail_sync_integration.rs`
continues to pass unchanged — the verb channel is additive to the event
loop.

**Unrelated test failures policy.** If the test suite surfaces failures
unrelated to this work, file Linear follow-up tickets rather than folding
fixes into this PR. See `feedback_unrelated_test_failures.md`.

## Out of scope / follow-ups

Items deferred from this PR, to be filed as separate Linear tickets once
this lands:

* **Folders** — `parent_cid` / `is_folder`, `create_folder` /
  `move_content` / `delete_folder` commands, tree-invariant tests.
* **Mesh retraction on burn** — `harmony/retract/{cid}` or similar wire
  event so peers holding replicas are notified. Needs gateway-side design.
* **Real archive tier** — flipping `disk_enabled` / `archive_enabled` in the
  client's `NodeConfig` so "archive" actually moves bytes to cold storage.
  Substantially larger than ZEB-146; deserves its own client-persistence
  spec.
* **Pin-quota UX** — "pin slots used: 3/8" indicator, pin-quota toast
  formatting.
* **Merging announced-but-not-local CIDs into Files view** — currently in a
  separate UI bucket; a future spec might unify.

## Dependency ordering

This PR cannot compile until `harmony` exposes
`NodeRuntime::storage_tier()` / `_mut()`. Two branches in sequence, both
tracked under ZEB-146 (the accessor work is load-bearing for this issue,
not a separable initiative):

1. **harmony PR** — add accessors to `NodeRuntime`, with a short test. Tiny;
   can likely land same-day. PR title should reference ZEB-146 as the
   motivating issue.
2. **harmony-client PR (this spec)** — bump the `harmony` git dep to the
   merged commit, then land the `content_index.rs` + verb channel +
   command + frontend changes together.

## Changelog

* **2026-04-23 — v1 (this document):** Spec drafted. Scope locked in via
  brainstorming Q1–Q5. Approaches fork resolved in favor of unified
  `ContentVerbRequest` channel. Folders, mesh retraction, and real
  archive-tier wiring deferred to separate specs.
