# ZEB-841 — Durable avatar image-byte cache

**Status:** approved (design)
**Ticket:** [ZEB-841](https://linear.app/zeblith/issue/ZEB-841)
**Relates:** ZEB-839 (§7 fast-follow), ZEB-840, ZEB-159, ZEB-343/344
**Date:** 2026-07-31

## 1. Problem

ZEB-839 made peer **names** durable across restart and peer-offline by persisting the
profile card — including its `avatar_cid` — to disk (`persistent_card_store.rs`). It
deliberately did **not** persist the avatar **image bytes** that CID points at.

Traced verdict (file:line):

| Property | Result | Evidence |
|---|---|---|
| Avatar bytes survive peer **offline** (same session) | ✅ yes | RAM cache hit, no network — `event_loop.rs:4888-4899` |
| Avatar bytes survive app **restart** | ❌ **no** | cache is `MemoryBookStore` = `HashMap<ContentId, Vec<u8>>` (`harmony-content/book.rs:37-40`); runtime always built empty (`lib.rs:12121`); nothing serializes it on shutdown |

Path today: `fetch_avatar` IPC (`lib.rs:25797`) → CAS fetch over Zenoh → verified bytes
admitted to the runtime W-TinyLFU `ContentStore` over `MemoryBookStore` (ZEB-159,
`event_loop.rs:5003`) — **RAM only**, 512-item cap, avatars unpinned/evictable.

**User-visible symptom:** restart while a peer is offline → their **name** renders (the
persistent card store kept the CID) but the **avatar goes blank** — the bytes evaporated.
ZEB-839 built the durable *pointer*; this builds the durable *payload*.

## 2. Approach (narrow, client-side)

A small content-addressed **disk** cache scoped to avatars, wrapping the `fetch_avatar`
IPC seam. Matches ZEB-839 §7 ("a small CAS blob cache keyed by CID, populated when we
fetch an avatar, read as a fallback when the peer's content store is unreachable").

Chosen over the general alternative — a disk-backed `BookStore` upstream in `harmony`
making *all* CAS content durable — because that touches the frozen-ish content/storage
layer, needs the ~8-crate lockstep rev bump, and is a materially larger project. The
narrow avatar cache stays entirely in `harmony-client` and retires cleanly if the general
store ever lands.

### 2.1 New module `src-tauri/src/avatar_blob_store.rs`

```text
{app_data_dir}/avatars/{cid_hex}.bin
```

Per-identity (under `resolve_app_data_dir()`, same root as `mail/`, `follows/`), so it is
wiped on identity reset for free — preserving the ZEB-586 per-identity-isolation lesson.
Mirrors the proven `mail.rs` blob pattern (`mail.rs:160-196, 271-321`).

```rust
pub struct AvatarBlobStore {
    dir: PathBuf,
    max_bytes: u64,       // total-cache budget; `put` prunes down to this
    max_blob_bytes: u64,  // per-blob ceiling (mirrors AVATAR_MAX_BYTES)
    prune_lock: Mutex<()>, // serializes concurrent prune scans
}
```

There is no in-memory byte tracker: `prune` recomputes the total by scanning the directory
(`read_dir`) each call — the cache is small (hundreds of files at most).

- **`get(cid: &str) -> Option<Vec<u8>>`** — `stat` `{dir}/{cid}.bin`; if it exceeds
  `max_blob_bytes`, remove it and return `None` (self-heal) — the size check is **before**
  the read, so a corrupt/oversized on-disk blob can't force a large allocation or bypass the
  ceiling the network path enforces. Otherwise read the bytes, recompute the `ContentId` and
  compare to `cid`: match → `Some(bytes)`; mismatch / I/O error → remove the file, return
  `None` (self-heal → caller falls through to network). A read hit does **not** touch mtime
  (eviction is write-time-ordered, not access-ordered).
- **`put(cid: &str, bytes: &[u8])`** — reject a malformed CID, over-`max_blob_bytes` bytes,
  or bytes failing `hash == cid`; else write atomically (a **uniquely-named** temp file —
  `{cid}.{pid}.{seq}.tmp`, removed on failure — then rename) and prune to `max_bytes`,
  evicting oldest-**mtime** (write-time) files first. Best-effort: any error logs and is
  swallowed (the cache is an optimization, never a correctness dependency).
- **`load(dir, max_bytes, max_blob_bytes) -> Self`** — `create_dir_all`, then sweep any
  stale `*.tmp` orphaned by a crash mid-write. Never fails the caller (warn-and-continue on
  dir errors, like `MailManager::load`).

**Concurrency:** held as `Arc<AvatarBlobStore>` on `NodeState`. `get`/`put` take `&self`;
all filesystem I/O runs **outside** any lock (distinct CID files never collide, and unique
temp names make concurrent same-CID writes safe); `prune_lock` is held only for the prune
directory scan so two `put`s can't race on eviction. `fetch_avatar` calls `get`/`put` inside
`tokio::task::spawn_blocking` so the blocking `std::fs` work stays off the async runtime.

**Cap:** `DEFAULT_MAX_BYTES = 32 * 1024 * 1024` (32 MiB ≈ hundreds of avatars at the
`AVATAR_MAX_BYTES = 512 KiB` per-blob ceiling). mtime is the LRU key, so recency survives
restart with no separate index file.

### 2.2 Wire into `fetch_avatar` (`lib.rs:25797`)

```text
1. clone Arc<AvatarBlobStore> out of the NodeState guard (alongside fetch_tx)
2. disk-first: spawn_blocking(|| store.get(&cid)) — Some(bytes) → return Ok(bytes)
3. <existing network FetchRequest path, unchanged>
4. on Ok(bytes): spawn_blocking(|| store.put(&cid, &bytes)); return Ok(bytes)
```

Disk-first is safe because a CID is an immutable content-address: a disk hit is *always*
the exact bytes the caller asked for (guaranteed by verify-on-load), so there is no
staleness window. The disk read/write happen with the `NodeState` lock **not** held, and on
a `spawn_blocking` thread so the blocking `std::fs` work (read + prune scan) stays off the
async worker.

### 2.3 No frontend change

The IPC contract is byte-identical. `avatar-resolver.ts` keeps its per-session object-URL
map as an L1 in front of the new disk L2, and its decode-bomb dimension guard still runs
on returned bytes. Because `get` verifies `hash==cid`, a tampered disk file can never serve
bytes that disagree with the signed card's `avatar_cid` — no security regression.

## 3. Testing

Backend unit tests in `avatar_blob_store.rs` (`AvatarBlobStore` is fully testable in
isolation over a `tempfile::tempdir()`):

- `put_then_get_round_trips` — store bytes under their real CID, read them back.
- `get_absent_is_none`.
- `get_rejects_and_removes_tampered_bytes` — write junk under a CID whose hash ≠ contents;
  `get` returns `None` and the file is gone (self-heal).
- `get_rejects_and_removes_oversized_file` / `put_refuses_oversized_blob` — a blob over
  `max_blob_bytes` is neither served (removed before read, self-heal) nor written.
- `eviction_drops_lru_over_budget` — small `max_bytes`; putting past budget evicts the
  oldest-mtime blob, keeps the newest.
- `fresh_instance_reads_existing_blobs` — a second `AvatarBlobStore::load` over the same
  dir serves blobs the first one wrote ("survives restart").
- `load_sweeps_stale_temp_files` — a `*.tmp` orphan is removed at load; committed blobs
  survive.

Handler wiring stays thin over the tested store. Manual/e2e (not in CI): restart a node
while a previously-seen peer is offline → avatar still renders (direct repro of the report).

## 4. Scope

1 new module (`avatar_blob_store.rs`) + `mod` decl + `fetch_avatar` handler edit + one
`NodeState` field and its `start_node` construction. All in `src-tauri`. No frontend, no
upstream `harmony` change, no lockstep rev bump.

## 5. References

- Backend: `src-tauri/src/lib.rs` (`fetch_avatar` `:25797`, `AVATAR_MAX_BYTES` `:22483`,
  `resolve_app_data_dir` `:3583`, `MailManager::load` call `:4048`, `NodeState` struct),
  `event_loop.rs` (verify `hash==cid` `:4960`, admit `:5003`), `persistent_card_store.rs`
  (`avatar_cid` persisted `:63`), `mail.rs:160-196/271-321` (blob-store template).
- Frontend (unchanged): `src/lib/avatar-resolver.ts`, `src/lib/components/Avatar.svelte`.
