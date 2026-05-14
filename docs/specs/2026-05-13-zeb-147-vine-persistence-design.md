# ZEB-147: Vine persistence — disk-backed `VineFeedCache`

**Status:** Design approved (2026-05-13)
**Branch:** `zeb-147-vine-persistence` (cut from `979ea84` on `origin/main`)
**Linear:** [ZEB-147](https://linear.app/zeblith/issue/ZEB-147)
**Predecessor:** [ZEB-286](https://linear.app/zeblith/issue/ZEB-286) (landed the in-memory `VineFeedCache` architectural seam — PR #118 merged at commit `979ea84`)

## 1. Goal

Wire the in-memory `VineFeedCache` (landed in ZEB-286) to disk so the Vine feed
survives app reload. Local persistence only — cross-device viewed-state sync
deferred to a follow-up ticket.

## 2. Background

### 2.1 What ZEB-286 delivered

`src-tauri/src/vine_feed_cache.rs` exposes `VineFeedCache` with:

- Three in-memory maps: `descriptors: HashMap<vine_id, CachedVine>`,
  `reactions: HashMap<(vine_id, reactor_addr), CachedReaction>`,
  `viewed: HashSet<vine_id>`.
- Public methods `on_descriptor_sample`, `on_reaction_sample`,
  `list_descriptors`, `get_reaction`, `mark_viewed`.
- Stored on `NodeState` as `Option<Arc<Mutex<VineFeedCache>>>`, constructed
  via `VineFeedCache::new()` in `start_node`, cleared on `stop_node`.
- `event_loop::emit_frontend_event` routes `harmony/vines/*` samples
  through the cache before emitting Tauri events.
- `list_vine_videos()` and `mark_vine_viewed()` IPCs delegate to the cache.

### 2.2 What's missing (this ticket)

The cache is in-memory only. App reload empties it. ZEB-286's PR description
called this out as ZEB-147 territory; the cache architecture explicitly
left the disk-wiring seam ready to slot into.

The original ZEB-147 ticket also mentioned cross-device viewed-state sync.
Per design decision (2026-05-13), that's split out: this PR ships local
persistence only; sync is a follow-up.

### 2.3 Codebase persistence precedent

Three siblings demonstrate the same JSON-file-with-atomic-write pattern:

- `src-tauri/src/follows.rs`: `FollowsFile { version, follows }` →
  `follows.json` via `serde_json::to_vec_pretty` + tempfile + rename.
- `src-tauri/src/content_index.rs`: same shape, larger payload.
- `src-tauri/src/mail.rs`: per-message blobs + JSON index, same atomicity.

All three:

- Use synchronous `std::fs`.
- Return an empty struct on missing-file or parse-failure.
- Write on every mutation (no debounce).
- Use a versioned envelope at the top level for forward-compatibility.

This PR follows that pattern verbatim. No new patterns introduced.

## 3. Disk layout

### 3.1 File path

`{app_data_dir}/vine_feed.json` — single file at the same level as
`follows.json`, `content_index.json`, `mail/index.json`.

### 3.2 File format

```json
{
  "version": 1,
  "descriptors": [
    {
      "id": "vine-...",
      "creatorAddress": "...",
      "creatorName": "...",
      "createdAt": 1700000000,
      "videoCid": "...",
      "title": "hello world",
      "reshareOf": null,
      "receivedAtMs": 1700000123,
      "source": "followed"
    }
  ],
  "reactions": [
    {
      "vineId": "vine-...",
      "reactorAddress": "...",
      "reactorName": "Alice",
      "liked": true,
      "timestamp": 1700000456
    }
  ],
  "viewed": ["vine-...", "vine-..."]
}
```

Field naming uses camelCase via `#[serde(rename_all = "camelCase")]` to
match the in-memory DTOs and existing siblings.

`title` and `reshareOf` use `#[serde(skip_serializing_if = "Option::is_none")]`
on the descriptor variant emitted by `vine-received` (already in place
post-ZEB-286 PR fixup), so on-disk we accept both `null` and absent. On
write we omit them when `None` for compactness.

### 3.3 Why single file (not split)

A single file ensures cross-section atomicity: a single tempfile + rename
commits all three pieces (descriptors, reactions, viewed) consistently.
Split files would either require sequential writes (risking torn state
on mid-batch crash) or a fsync-coordinated commit protocol (overkill).

At the 5000-descriptor cap, the file is well under 5 MB (descriptors
average ~250 bytes each + reactions + viewed-set), comfortably within
sub-millisecond fsync time on modern SSDs.

## 4. API additions to `VineFeedCache`

### 4.1 Constructor split

```rust
impl VineFeedCache {
    /// In-memory only. No persistence path; `save()` is a no-op.
    /// Used by tests and any caller that explicitly wants ephemeral state.
    pub fn new() -> Self;

    /// Load from `data_dir/vine_feed.json`. Returns an empty cache
    /// (with `path` set) if the file is missing or corrupt. Applies
    /// the age-prune and capacity-trim on load.
    pub fn load(data_dir: &Path) -> Self;
}
```

### 4.2 New internal field

```rust
pub struct VineFeedCache {
    descriptors: HashMap<String, CachedVine>,
    reactions: HashMap<(String, String), CachedReaction>,
    viewed: HashSet<String>,
    /// `Some` when constructed via `load()`; `None` for `new()`.
    /// When `None`, `save()` is a no-op (test path).
    path: Option<PathBuf>,
}
```

### 4.3 New private method

```rust
impl VineFeedCache {
    /// Atomic save: write to `<path>.tmp`, rename to `<path>`.
    /// No-op when `self.path.is_none()`.
    /// Errors logged via `tracing::warn!` but not propagated
    /// (matches the `follows.rs` and `content_index.rs` philosophy:
    /// best-effort persistence, never crash the caller).
    fn save(&self);
}
```

### 4.4 Mutation-time save hooks

Each mutator calls `self.save()` at the end **only when state actually changed**:

| Method | Save when outcome is | Skip save when outcome is |
|---|---|---|
| `on_descriptor_sample` | `Inserted` | `AlreadyPresent`, `Rejected`, `None` |
| `on_reaction_sample` | `Inserted`, `UpdatedNewer` | `Stale`, `Rejected`, `None` |
| `mark_viewed` | `true` (newly added) | `false` (already viewed) |

This minimizes I/O for the common no-op paths (re-deliveries,
duplicate marks). Same dedupe logic the cache already uses to gate
Tauri emits.

### 4.5 Public constants

```rust
/// Max descriptors retained in the cache. On insert into a full cache,
/// the oldest descriptor (lowest `created_at`) is dropped, along with
/// its reactions. Viewed-set entries are NOT dropped (low byte cost).
pub const MAX_DESCRIPTORS: usize = 5000;

/// Max age of a descriptor in seconds. Applied ONCE on `load()`:
/// descriptors with `created_at < now_secs - MAX_AGE_SECS` are dropped
/// along with their reactions. Runtime mutations do not re-age-prune.
pub const MAX_AGE_SECS: u64 = 90 * 86_400;
```

## 5. `load()` algorithm

```text
fn load(data_dir):
    path = data_dir / "vine_feed.json"
    try:
        bytes = std::fs::read(path)
        file: VineFeedFile = serde_json::from_slice(bytes)
        if file.version != 1: return empty cache (with path set)
    catch: return empty cache (with path set)

    now_secs = current_unix_secs() (defaulting to 0 on pre-epoch)
    age_cutoff = now_secs.saturating_sub(MAX_AGE_SECS)

    # Age-prune (one-shot on load)
    file.descriptors.retain(|d| d.created_at >= age_cutoff)
    surviving_vine_ids = file.descriptors.iter().map(|d| d.id).collect::<HashSet>()
    file.reactions.retain(|r| surviving_vine_ids.contains(&r.vine_id))

    # Capacity-trim (in case persisted state already over cap;
    # defensive — production write path enforces cap on insert)
    if file.descriptors.len() > MAX_DESCRIPTORS:
        file.descriptors.sort_by_key(|d| Reverse(d.created_at))
        file.descriptors.truncate(MAX_DESCRIPTORS)
        kept = file.descriptors.iter().map(|d| d.id).collect::<HashSet>()
        file.reactions.retain(|r| kept.contains(&r.vine_id))

    return populated cache (with path set)
```

## 6. Runtime capacity-trim

Inside `on_descriptor_sample` after a successful insert:

```text
if self.descriptors.len() > MAX_DESCRIPTORS:
    # Collect (created_at, id) pairs, sort ascending, drop oldest
    # until len == MAX_DESCRIPTORS.
    let mut entries: Vec<(u64, String)> = self.descriptors
        .iter()
        .map(|(id, cv)| (cv.descriptor.created_at, id.clone()))
        .collect();
    entries.sort_by_key(|(ts, _)| *ts);  # ascending
    let drop_count = self.descriptors.len() - MAX_DESCRIPTORS;
    for (_, id) in entries.into_iter().take(drop_count):
        self.descriptors.remove(&id);
        self.reactions.retain(|(vid, _), _| vid != &id);
```

Single trim per insert keeps amortized cost bounded. `entries.sort_by_key`
on 5001 elements is sub-millisecond; the trim path runs at most once per
insert and only when the cap is exceeded.

**Determinism note:** sort by `created_at` ascending, ties broken by
`id` (lexicographic). With `id` containing the publisher's node_addr
prefix + timestamp + random suffix, ties are extremely rare but the
secondary sort ensures cross-replica consistency if they occur.

## 7. `save()` algorithm

```text
fn save(&self):
    if self.path.is_none(): return  # in-memory only path

    file = VineFeedFile {
        version: 1,
        descriptors: self.descriptors.values().map(to_serde_form).collect(),
        reactions: self.reactions.values_with_keys().map(to_serde_form).collect(),
        viewed: self.viewed.iter().cloned().collect(),
    }

    bytes = serde_json::to_vec_pretty(&file)  # on error → log + return
    tmp_path = path.with_extension("json.tmp")
    if let Some(parent) = path.parent(): std::fs::create_dir_all(parent)?
    std::fs::write(&tmp_path, &bytes)?
    std::fs::rename(&tmp_path, &path)?
    # Errors above: tracing::warn! + return (best-effort)
```

`to_serde_form` translates the internal `CachedVine` / `CachedReaction`
structs to the on-disk shape (which exposes `id` for descriptors and
the `(vine_id, reactor_address)` key for reactions). The on-disk
schema is intentionally close to the in-memory shape so the
round-trip is straightforward.

## 8. Production wiring (`start_node`)

Single line change in `src-tauri/src/lib.rs`:

```rust
// Before (ZEB-286):
let vine_feed_cache = std::sync::Arc::new(std::sync::Mutex::new(
    vine_feed_cache::VineFeedCache::new(),
));

// After (ZEB-147):
let vine_feed_cache = std::sync::Arc::new(std::sync::Mutex::new(
    vine_feed_cache::VineFeedCache::load(&app_data_dir),
));
```

`app_data_dir: &Path` is already in scope at this site (used by
`follow_mgr::load`, `mail::MailManager::load`, etc.).

`stop_node` requires no change — the cache is already saved on every
mutation, so no flush-on-shutdown is needed. `vine_feed_cache.take()`
drops the Arc; persistence is already on disk.

## 9. Tests

### 9.1 New module unit tests (in `vine_feed_cache.rs`'s `mod tests`)

1. `load_missing_file_returns_empty_cache` — `load(tempdir)` on empty
   directory returns empty cache with `path` set; subsequent
   `len_descriptors()` is 0.

2. `load_corrupt_json_returns_empty_cache` — write `b"{ not valid json"`
   to the path, `load(tempdir)` returns empty cache.

3. `load_wrong_version_returns_empty_cache` — write `{"version": 999, ...}`,
   `load(tempdir)` returns empty cache.

4. `save_load_round_trip_preserves_descriptors_reactions_viewed` —
   construct cache via `load(tempdir)`, insert 3 descriptors via
   `on_descriptor_sample`, 2 reactions via `on_reaction_sample`,
   mark 1 viewed via `mark_viewed`. Drop. `load(tempdir)` again. Assert
   `len_descriptors == 3`, `len_reactions == 2`, `is_viewed("v-1")`.

5. `age_prune_on_load_drops_old_descriptors_and_their_reactions` —
   write a `vine_feed.json` containing a descriptor with
   `created_at = now - (91 * 86400)` and a recent one. Add a reaction
   for the old vine. `load(tempdir)` returns a cache with only the
   recent descriptor and 0 reactions.

6. `capacity_trim_drops_oldest_when_insert_exceeds_max` — construct
   `VineFeedCache::new()`, manually insert MAX_DESCRIPTORS + 1 descriptors
   via repeated `on_descriptor_sample` calls (with monotonically
   increasing `created_at`). Assert `len_descriptors() == MAX_DESCRIPTORS`,
   and the oldest (created_at = 0) is no longer present.

### 9.2 New integration test (`src-tauri/tests/vine_feed_persistence_integration.rs`)

7. `cache_survives_reload` — `tempdir`-backed cache via
   `VineFeedCache::load(&tempdir)`, insert descriptor + reaction +
   `mark_viewed`, drop the cache, re-`load(&tempdir)`, assert all three
   pieces present.

### 9.3 Pre-existing tests

All 35 tests from ZEB-286 should pass unchanged. The mutator outcomes
(`Inserted` / `UpdatedNewer` / etc.) are unchanged; only the
side-effect (save-to-disk) is added.

## 10. Acceptance criteria

1. `src-tauri/src/vine_feed_cache.rs` exports:
   - `pub const MAX_DESCRIPTORS: usize = 5000`
   - `pub const MAX_AGE_SECS: u64 = 90 * 86_400`
   - `pub fn VineFeedCache::load(data_dir: &Path) -> Self`
   - existing `pub fn VineFeedCache::new() -> Self` unchanged
2. `VineFeedCache.path: Option<PathBuf>` private field, set by `load()`,
   `None` for `new()`.
3. `on_descriptor_sample`, `on_reaction_sample`, `mark_viewed` each call
   `self.save()` only on the mutating outcome (per §4.4 table).
4. `save()` is no-op when `path.is_none()` (test path).
5. `save()` uses atomic write (tempfile + rename) and logs errors.
6. `load()` applies age-prune + capacity-trim per §5.
7. Runtime capacity-trim runs inside `on_descriptor_sample` per §6.
8. `start_node` swaps `new()` → `load(&app_data_dir)` per §8.
9. All 7 new tests pass; all 35 ZEB-286 tests pass unchanged.
10. All 5 CI gates green: `cargo fmt`, `cargo clippy`, `cargo nextest`,
    `npx tsc`, `npx vitest`.

## 11. Out of scope (explicit non-goals)

- **Cross-device viewed-state sync** → split out as a follow-up ticket.
  Will need privacy design (opt-in mirror of [ZEB-214](https://linear.app/zeblith/issue/ZEB-214) DM read-receipts) and wire format
  (likely a new Zenoh topic like `harmony/vines/{addr}/viewed`).
- **VineService frontend mock-clear** → [ZEB-209](https://linear.app/zeblith/issue/ZEB-209).
- **Reshare attribution UX** → [ZEB-103](https://linear.app/zeblith/issue/ZEB-103).
- **`get_reaction` secondary index for O(reactors_per_vine) lookup** →
  Cursor Low from ZEB-286 review. File as separate optimization.
- **Viewed-set GC for stale entries** — viewed-IDs whose descriptors
  age-pruned remain in the set indefinitely. Acceptable for v1 (low
  byte cost). File as follow-up if the set grows pathologically.
- **CBOR/binary format** — JSON for parity with `follows.rs` /
  `content_index.rs`.
- **Debounced writes** — match `follows.rs` write-on-every-mutation.
  Revisit only if profiling shows hot path.
- **Migration from `version != 1`** — first launch of ZEB-147 sees no
  prior persisted state (ZEB-286's in-memory cache is ephemeral). No
  migration logic needed.
- **Tighter sub-cap (e.g., 1000 vines)** — 5000 chosen for headroom;
  adjustable later via setting.

## 12. Follow-up tickets to file post-merge

1. **Cross-device viewed-state sync** — original optional part of
   ZEB-147, split out per scope decision. Needs privacy design.
2. **Viewed-set GC for orphaned entries** — only if the set grows
   pathologically (low priority).
3. **`get_reaction` secondary index** — Cursor Low from ZEB-286 review.

## 13. Risks

- **5000-descriptor cap is somewhat arbitrary.** Defensible default;
  user-tunable via setting if needed later. The cap is a constant
  rather than a config so v1 keeps the system surface narrow.
- **90-day age cutoff is somewhat arbitrary.** Same shape — defensible
  default, adjustable later.
- **Synchronous `save()` on the dispatch hot path** — at typical
  ~5KB-50KB feed sizes, sub-ms. If profiling shows otherwise (e.g., a
  power user with many followed creators publishing in bursts), the
  debounce migration is straightforward to add later.
- **`stop_node` race with concurrent dispatch holding the Arc clone**
  — the post-stop `save()` writes valid state and the next `start_node`
  picks it up cleanly. No data loss path.
