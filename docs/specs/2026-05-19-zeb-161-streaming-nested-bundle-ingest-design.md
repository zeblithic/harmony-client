# ZEB-161 — Streaming nested-bundle ingest

Status: design (2026-05-19)
Linear: [ZEB-161](https://linear.app/zeblith/issue/ZEB-161/)
Parent: [ZEB-158](https://linear.app/zeblith/issue/ZEB-158/) (Axis A)
Supersedes (in scope): the structural-cap-only framing — this design lifts ingest to filesystem-bound, not 32 GiB RAM-bound.

## Context

`harmony-client`'s ingest pipeline (`ingest_content`, `ingest_file_at_path`, `send_ingest_bytes_only`, `chunk_and_bundle`) currently reads each file whole into `Vec<u8>`, then either takes the single-`for_book` path or runs FastCDC across the buffer and assembles a **single flat bundle**. Files larger than `FLAT_BUNDLE_MAX = MAX_BUNDLE_ENTRIES × min_chunk ≈ 8 GiB` are rejected with `IngestError::Oversized` and surface in the ZEB-163 folder-ingest summary modal as `skipped.oversized`.

The current shape has two coupled limits:
1. **Structural cap** — bundle entry count can't exceed `MAX_BUNDLE_ENTRIES ≈ 32,767`. Lifted by chaining bundles into a Merkle tree (already implemented for the canonical algorithm in [`harmony-content/src/dag.rs`](../../src-tauri/../harmony/crates/harmony-content/src/dag.rs)).
2. **RAM cap** — the whole file lives in `Vec<u8>` for the duration of the ingest. Without lifting this, removing (1) only moves the cap from 8 GiB to ~32 GiB (depends on host RAM) — not a meaningful win.

ZEB-161 lifts **both** caps: streaming from disk so RAM use is bounded by chunker buffer + accumulated leaf CIDs, and porting the recursive bundle-tree builder so the structural cap is `min_chunk × MAX_BUNDLE_ENTRIES^MAX_BUNDLE_DEPTH` (effectively filesystem-bound).

## Decisions

| | Decision | Confirmed |
|---|---|---|
| D1 | Port `harmony-content::dag::ingest`'s bottom-up loop into the client (do not refactor upstream crate) | 2026-05-19 |
| D2 | **Stream from disk via `tokio::io::AsyncRead`** (single code path for path and bytes-in callers via `Cursor<Vec<u8>>`) | 2026-05-19 |
| D3 | Attach inline metadata `(total_size, chunk_count, 0, [0;8])` to the root bundle, matching `dag::ingest` | 2026-05-19 |
| D4 | Remove `FLAT_BUNDLE_MAX`, `IngestError::Oversized`, and `SkipCounts.oversized` outright — no longer reachable | implied by D2 |
| D5 | Single-chunk files skip the bundle wrapper (return the leaf's `Book` CID directly), matching `dag::ingest:28-30` | implied by D1+D3 |

## API surface

### New: `streaming_ingest`

```rust
/// Drives a byte stream through FastCDC, ingests each leaf, builds the
/// bundle tree bottom-up, and returns the root CID. Memory is bounded by
/// the chunker buffer (~1 MiB) + the leaf-CID vec (32 B × leaf_count).
///
/// Returns:
///   - `CidType::Book` for inputs that fit in a single chunk (no bundle wrap).
///   - `CidType::Bundle(depth)` with `depth >= 1` for multi-chunk inputs.
///
/// The first entry of the root bundle (when present) is a sentinel
/// `InlineData` CID carrying `(total_size, chunk_count, 0, [0; 8])` —
/// matches `harmony_content::dag::ingest` so root reassembly can pre-size
/// its output buffer.
pub(crate) async fn streaming_ingest<R>(
    reader: R,
    ingest_tx: &tokio::sync::mpsc::Sender<event_loop::IngestRequest>,
    chunker_config: ChunkerConfig,
) -> Result<ContentId, IngestError>
where
    R: tokio::io::AsyncRead + Unpin;
```

Production callers pass `ChunkerConfig::DEFAULT`. Tests pass a small config (`min=64, avg=128, max=256`) to reach multi-level trees on small inputs.

### Removed

- `pub fn chunk_and_bundle(bytes: &[u8]) -> Result<(Vec<(ContentId, &[u8])>, Vec<u8>, ContentId), String>`
- `pub(crate) const FLAT_BUNDLE_MAX: u64`
- `pub(crate) enum IngestDispatch { Single, Chunked }`
- `pub(crate) fn ingest_dispatch(size: u64) -> Result<IngestDispatch, String>`
- `IngestError::Oversized { size, cap }`
- `folder_ingest::SkipCounts.oversized` (Rust struct field)
- `IngestFolderTreeResult.skipped.oversized` (TS type field)

### Modified

- `send_ingest_bytes_only(ingest_tx, bytes, file_name)` — internals wrap `bytes` in `Cursor::new(bytes)` and call `streaming_ingest(...)`. Signature preserved for the folder-walker call site (which already holds bytes after `tokio::fs::read`).
- `ingest_content` IPC handler — opens the picker-returned `path` via `tokio::fs::File::open`, calls `streaming_ingest(file, ...)` directly. No intermediate `Vec<u8>`.
- `ingest_file_at_path` — replaces its `tokio::fs::read(path).await` + `send_ingest_with_name` with a streaming variant that opens the file and routes through `streaming_ingest`, then inserts the sidecar row from the returned root CID.
- `send_ingest_with_name` — adapts to take the root CID from `streaming_ingest` rather than computing CID locally. The sidecar-row insertion logic is unchanged.

## Pipeline

### Streaming chunk emission

```rust
let mut chunker = Chunker::new(chunker_config)?;
let mut open_chunk = Vec::with_capacity(chunker_config.max_chunk);  // current unclosed chunk
let mut leaf_cids = Vec::<ContentId>::new();
let mut total_bytes = 0u64;
let mut read_buf = vec![0u8; READ_WINDOW_SIZE];  // const: 1 MiB

loop {
    let n = reader.read(&mut read_buf).await?;
    if n == 0 { break; }
    let window = &read_buf[..n];
    total_bytes += n as u64;

    let cuts = chunker.feed(window);

    let mut window_pos = 0;
    for cut in cuts {
        open_chunk.extend_from_slice(&window[window_pos..cut]);
        // open_chunk now contains a complete chunk
        let cid = ContentId::for_book(&open_chunk, ContentFlags::default())
            .map_err(|e| IngestError::other(format!("leaf CID: {e:?}")))?;
        send_ingest(ingest_tx, hex::encode(cid.to_bytes()), open_chunk.clone())
            .await
            .map_err(IngestError::IngestChannel)?;
        leaf_cids.push(cid);
        open_chunk.clear();
        window_pos = cut;
    }
    open_chunk.extend_from_slice(&window[window_pos..]);
}

// Finalize: any tail in open_chunk is the last leaf
if chunker.finalize().is_some() {
    // sanity: chunker.finalize()'s tail length equals open_chunk.len()
    debug_assert_eq!(chunker.finalize_was_some_len, open_chunk.len());
    let cid = ContentId::for_book(&open_chunk, ContentFlags::default())?;
    send_ingest(ingest_tx, hex::encode(cid.to_bytes()), open_chunk.clone()).await?;
    leaf_cids.push(cid);
    open_chunk.clear();
}
```

**Invariants:**
- `open_chunk.len() <= chunker_config.max_chunk` at all times — `max_chunk ≤ MAX_PAYLOAD_SIZE` ensures it fits in a single `for_book` CID.
- The sum of leaf-bytes-sent equals `total_bytes` (every input byte lands in exactly one leaf).
- The leaf-CID order is the linearization order of the file (DFS / left-to-right).
- The `Chunker::feed` cut offsets are window-relative; we maintain absolute position implicitly via `open_chunk` accumulation.

**`READ_WINDOW_SIZE`:** start at 1 MiB. Larger windows reduce `read()` syscalls and (modestly) help disk throughput; smaller windows have lower latency to first leaf emission. 1 MiB is a defensible middle ground and matches `max_chunk` so the worst-case `cuts.len()` per feed is bounded.

### Bundle-tree build

After the streaming loop, `leaf_cids` is fully populated. Build bottom-up:

```rust
// Single-chunk degenerate case: no bundle wrapper.
if leaf_cids.len() == 1 {
    return Ok(leaf_cids[0]);
}

let chunk_count = leaf_cids.len() as u32;
let mut current_level = leaf_cids;

loop {
    let is_root_level = current_level.len() <= MAX_BUNDLE_ENTRIES;
    let mut next_level = Vec::with_capacity(current_level.len().div_ceil(MAX_BUNDLE_ENTRIES));

    for group in current_level.chunks(MAX_BUNDLE_ENTRIES) {
        let mut builder = BundleBuilder::new();
        for cid in group { builder.add(*cid); }
        if is_root_level && next_level.is_empty() {
            builder.with_metadata(total_bytes, chunk_count, 0, [0u8; 8]);
        }
        let (bundle_bytes, bundle_cid) = builder
            .build_with_flags(ContentFlags::default())
            .map_err(|e| IngestError::ManifestBuild(format!("{e:?}")))?;
        send_ingest(ingest_tx, hex::encode(bundle_cid.to_bytes()), bundle_bytes)
            .await
            .map_err(IngestError::IngestChannel)?;
        next_level.push(bundle_cid);
    }

    if next_level.len() == 1 {
        return Ok(next_level[0]);
    }
    current_level = next_level;
}
```

**Differences from `dag::ingest`:**
- Sends each leaf and bundle through the IPC channel (`send_ingest`) instead of writing to a `BookStore`. The runtime's chunk cache handles persistence/eviction downstream.
- Uses `build_with_flags(ContentFlags::default())` (matching the current client) rather than `build()`. Semantically equivalent under default flags; preserves existing behaviour.

## Demolitions — call site by call site

| File:Line | Before | After |
|---|---|---|
| `src-tauri/src/lib.rs:92-93` | `pub(crate) const FLAT_BUNDLE_MAX` | **Removed** |
| `src-tauri/src/lib.rs:96-103` | `enum IngestDispatch { Single, Chunked }` | **Removed** |
| `src-tauri/src/lib.rs:107-120` | `fn ingest_dispatch(size) -> Result<_, String>` | **Removed** |
| `src-tauri/src/lib.rs:130-155` | `IngestError::Oversized { size, cap }` | Variant removed; thiserror enum repacks |
| `src-tauri/src/lib.rs:181-224` | `pub fn chunk_and_bundle(&[u8])` | Removed; replaced by `streaming_ingest` |
| `src-tauri/src/lib.rs:6085-6111` | `ingest_file_at_path` body | `tokio::fs::File::open` + `streaming_ingest` |
| `src-tauri/src/lib.rs:6128-6189` | `send_ingest_bytes_only` body | `Cursor::new(bytes)` + `streaming_ingest` |
| `src-tauri/src/lib.rs:6097-6101, 6143-6147` | Oversize early returns | **Removed** |
| `src-tauri/src/lib.rs:5984-..` | `ingest_content` body | `tokio::fs::File::open` + `streaming_ingest` |
| `src-tauri/src/lib.rs:6140-6181` | `Chunked` arm of `send_ingest_bytes_only` | **Removed** (no longer reachable; single arm now) |
| `src-tauri/src/lib.rs:22571-22631` | 5 `chunk_and_bundle_*` unit tests | Replaced by `streaming_ingest_*` and `build_bundle_tree_*` tests |
| `src-tauri/src/folder_ingest.rs:14-17` | Doc comment about `FLAT_BUNDLE_MAX` skip | Removed; mention streaming instead |
| `src-tauri/src/folder_ingest.rs:37` | `use crate::{... FLAT_BUNDLE_MAX}` | Drop `FLAT_BUNDLE_MAX` from imports |
| `src-tauri/src/folder_ingest.rs:75-78` | `SkipCounts.oversized: u64` field | **Removed** |
| `src-tauri/src/folder_ingest.rs:195` | `metadata.is_file() && metadata.len() <= FLAT_BUNDLE_MAX` (pre-walk count) | Drop the size predicate — every file counts |
| `src-tauri/src/folder_ingest.rs:547-566` | Per-leaf size cap + capped read | **Removed** — streaming bounds memory regardless |
| `src-tauri/tests/folder_ingest_walker_integration.rs:473-494` | Oversized-leaf test (sparse set_len past cap) | Replaced by a depth-2+ integration test |
| `src-tauri/tests/content_index_integration.rs:307-..` | `chunked_ingest_pin_cascade_fetch_burn_roundtrip` (3 MB file, flat-bundle) | Driver switches to `streaming_ingest`; assertions unchanged |
| `src/lib/file-manager-service.ts:66-75` | `SkipCounts.oversized: number` | **Removed** |
| `src/lib/components/FolderIngestSummaryModal.svelte:78-80` | `{#if result.skipped.oversized > 0}` bullet | **Removed** |
| `src/lib/components/__tests__/file-browser-folder-ingest.test.ts` | Fixtures asserting `oversized` count | Updated to drop the field |

## Inline metadata semantics

Adopt `dag::ingest`'s metadata format on the root bundle:

```rust
builder.with_metadata(
    total_bytes,           // u64 — actual file length (accumulated during read)
    chunk_count,           // u32 — leaf_cids.len() (NOT total leaves at root level)
    0,                     // u64 — timestamp placeholder
    [0u8; 8],              // [u8; 8] — MIME placeholder
);
```

- **Set only on the root bundle** at the moment when `current_level.len() <= MAX_BUNDLE_ENTRIES` (the level that becomes the root).
- **For single-leaf files** the early-return at `leaf_cids.len() == 1` means no bundle is built; no metadata sentinel exists. This matches `dag::ingest:27-30`.
- **CID divergence note:** chunked-ingest root CIDs computed against `main` (without metadata) differ from those computed by this branch. No on-disk fixture pins a chunked root CID — the only existing test (`chunked_ingest_pin_cascade_fetch_burn_roundtrip`) re-derives the root via the helper, so it absorbs the change.

## Test plan

### Unit — bundle tree builder (`build_bundle_tree`)

Driven with synthetic `ContentId`s (no chunking) — fast and exhaustive.

1. **Single leaf** returns the leaf CID directly (no bundle).
2. **Two leaves** return a depth-1 bundle whose entries are `[metadata_sentinel, leaf_0, leaf_1]`.
3. **`MAX_BUNDLE_ENTRIES` leaves** return a single depth-1 bundle.
4. **`MAX_BUNDLE_ENTRIES + 1` leaves** return a depth-2 root with two children: one with `MAX_BUNDLE_ENTRIES` entries, one with 1 entry. The depth-2 root holds the metadata sentinel, NOT the depth-1 bundles.
5. **`MAX_BUNDLE_ENTRIES^2` leaves** return a depth-2 root with `MAX_BUNDLE_ENTRIES` depth-1 children.
6. **Metadata correctness:** `(total_size, chunk_count, 0, [0;8])` round-trips through `parse_inline_metadata`.

### Unit — streaming bridge (`streaming_ingest` with `Cursor<Vec<u8>>` + small chunker config)

Uses `ChunkerConfig { min_chunk: 64, avg_chunk: 128, max_chunk: 256 }` so multi-level trees are reachable on small inputs.

1. **Empty reader** — returns the expected error (zero leaves; we don't accept empty files because there's no meaningful CID for them — match `dag::ingest`'s `EmptyData` behaviour).
2. **Single-chunk input (< min_chunk bytes)** returns a `Book` CID.
3. **Two-chunk input** returns a `Bundle(_)` CID, walks to two leaves.
4. **Multi-bundle-level input** (~64 KB at small config) returns a `Bundle(2+)` and reassembles correctly.
5. **Multi-feed equivalence** — driving `Chunker::feed` with the whole input vs.~with 100-byte windows produces the same leaf CIDs and same root CID.
6. **Send ordering** — captured-channel test verifies leaves arrive in the IPC channel in chunker order, followed by bundles in bottom-up level order.

### Integration — runtime IPC round-trip (`content_index_integration::chunked_ingest_pin_cascade_fetch_burn_roundtrip`)

Existing 3 MB test: switch the driver from `chunk_and_bundle` to `streaming_ingest(Cursor::new(bytes), ingest_tx, ChunkerConfig::DEFAULT)`. Recompute expected descendants from the returned root. With 3 MB at default chunker config the tree stays depth-1 — same shape as today, just via the new driver.

### Integration — depth-2+ tree round-trip (new)

`folder_ingest_walker_integration::nested_bundle_tree_round_trip`:
- Build a sparse ~9 GiB file via `set_len` on a tempfile.
- Drive `streaming_ingest(tokio::fs::File::open(path), ingest_tx, DEFAULT)`.
- Assert root is `CidType::Bundle(2)` (or greater).
- Assert `walk_recursive` over the root produces all leaves.
- Assert `parse_inline_metadata` on the root's first entry returns the original file size.

Gated behind `--features nightly-tests` or a `HARMONY_LARGE_TESTS=1` env var so contributors on small machines can skip.

### Replaced

The ZEB-163 oversized-leaf test (`folder_ingest_walker_integration` line ~473) — its premise (skipping > 8 GiB files) no longer exists. Replaced by the depth-2+ tree test above.

## Frontend changes

### `src/lib/file-manager-service.ts`

```diff
 export interface SkipCounts {
   hidden: number;
   symlink: number;
-  oversized: number;
   other: number;
 }
```

### `src/lib/components/FolderIngestSummaryModal.svelte`

Remove the `{#if result.skipped.oversized > 0}` bullet (lines 78-80).

### `src/lib/components/__tests__/file-browser-folder-ingest.test.ts`

Drop the `oversized` field from all `SkipCounts` fixtures.

### No other frontend change

- `IngestFolderTreeResult.preWalkTotal` semantics unchanged — it's still a leaf-file count.
- `IngestFolderTreeResult.skipped.other` semantics unchanged.
- The progress modal's "current path" rendering is unchanged.

## ZEB-157 (partial-ingest rollback) interaction

Unchanged from the flat-bundle path: if a mid-tree `send_ingest` fails, `streaming_ingest` returns the error and no sidecar is created. Leaves and partial-level bundles already sent stay in the runtime's chunk cache as garbage (no references hold them; W-TinyLFU evicts on pressure). User sees no partial entry.

ZEB-157 will later add explicit cleanup of these orphans. The depth-N tree has more potential orphans per failed ingest (up to ~4M leaves for a 1 TB file vs.~~32K for the flat-bundle cap), so ZEB-157 becomes more valuable post-ZEB-161. Tracked as a follow-up; not addressed here.

## Memory analysis

For a 1 TB file at `ChunkerConfig::DEFAULT` (min=256 KiB):

| Region | Peak size | Note |
|---|---|---|
| `read_buf` | 1 MiB | const |
| `open_chunk` | ≤ `max_chunk` (≈1 MiB) | bounded by FastCDC `max_chunk` |
| `leaf_cids` | ~128 MB | 4 M leaves × 32 B/CID |
| `current_level` during tree-build | ~128 MB | same vec, transiently reused for higher levels |
| `next_level` during tree-build | ≤ 4 MB | 32k× smaller |
| bundle payload buffers | ≤ 1 MiB each | one held at a time; not retained after `send_ingest` |
| **Total** | **~130 MB** | regardless of file size up to 1 TB |

For multi-TB files, leaf CIDs become the bottleneck (~128 MB per TB). A future optimization could spill `leaf_cids` to disk in chunks of `MAX_BUNDLE_ENTRIES` (build level-1 bundles streaming, then only retain level-1 CIDs). Out of scope here.

## Non-goals

- **Pipelined leaf sends.** Each leaf is sent through `send_ingest` and awaited sequentially (matches existing behaviour). Concurrent sends with a bounded semaphore would speed up high-latency channels but is orthogonal. File a follow-up if profiling shows the await chain dominates.
- **Resumable ingest.** A crash mid-tree restarts from byte 0 of the source file. No journal of "leaves sent so far."
- **Multi-TB files (> 1 TB).** The architecture supports them; the in-RAM leaf-CID vec is the practical ceiling. A follow-up can spill leaf CIDs.
- **Inline-data leaves.** Inputs < `INLINE_DATA_THRESHOLD` could fit in an inline CID with no chunk store entry. Current `chunk_and_bundle` doesn't do this and `dag::ingest` doesn't either; preserving that.

## Risks

- **R1 — `IngestError::Oversized` removed but cached in serialized data.** No callers persist `IngestError` discriminants across processes; only displayed as strings via `to_string()`. Safe to remove.
- **R2 — `SkipCounts.oversized` field removal breaks deserialization of any persisted summary.** No call site persists `IngestFolderTreeResult` — it's a one-shot IPC return delivered to a Svelte component and discarded. Safe to remove.
- **R3 — Bot reviewers may flag the missing `Oversized` case as a regression on R-class size limits.** Defense: ticket explicitly removes the cap; no per-file size limit exists post-ZEB-161. The cap is the filesystem.
- **R4 — `chunked_ingest_pin_cascade_fetch_burn_roundtrip` root CID changes (because metadata is now attached).** The test re-derives the root via the helper; no fixed value pinned. Verified at design time.
- **R5 — 9 GiB sparse-file integration test is slow on small machines.** Gated behind `HARMONY_LARGE_TESTS=1`. CI runs it on the full job; local dev defaults to skip.
