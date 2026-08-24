# ZEB-986 PR-1 — Dangerous recovery contracts (data-destroying) — design

**Ticket:** ZEB-986 (Local-persistence hardening sweep), PR 1 of a 3-PR split.
**Scope:** the *recovery-contract* dimension only. Write-atomicity + permissions (PR 2)
and at-rest encryption (PR 3) are explicitly out of scope here.
**Date:** 2026-08-24

## Problem

Five plaintext app-data stores lose data on the first corruption or transient read
error. Each collapses **both** a read/IO error **and** content corruption into the
same silent-default path, then the next in-memory mutation overwrites the (possibly
still-good) on-disk file with an empty value:

| Store | File | Current load behavior | Failure |
|---|---|---|---|
| `follows.rs` `FollowManager` | `follows.json` | `read_file(&path).unwrap_or_default()` — `.ok()?` on both read and parse, no warn | one EIO or one corrupt byte + next `follow()`/`unfollow()` ⇒ **outbound follow graph destroyed** |
| `friend_nicknames.rs` `FriendNicknames` | `friend_nicknames.json` | `load_or_default` warns but every arm returns empty; no freeze | transient EIO + next `save()` ⇒ **all nicknames destroyed** (legacy/migration store) |
| `content_index.rs` `ContentIndex` | `content-index.json` | `read_file(&path).unwrap_or_default()`, silent | corrupt index + next `insert()`/`remove()` ⇒ empty index overwrites ⇒ **every stored blob orphaned** (metadata unrecoverable) |
| `vine_feed_cache.rs` `VineFeedCache` | `vine_feed.json` | warns, empty cache; no quarantine, no freeze | corrupt cache silently discarded (regenerable, lower stakes) |
| `vine_pull_driver.rs` | `vine_pull.cbor` | `load_vine_pull` silent default on any error | corrupt pull progress silently discarded (re-derivable, lower stakes) |

The fix is to bring all five up to the **Io-vs-content discrimination** already proven
by `persistent_card_store` (`disk_write_frozen`, ZEB-982) and the **quarantine-aside**
already proven by `fleet_dataset_file::load_or_recover` / `friend_requests.rs` (ZEB-784) —
but for plaintext stores, via one small shared helper. Separately, no `.corrupt.*`
sidecar is ever collected anywhere in the tree; add a bounded-retention boot sweep.

## Global constraints

- **Plaintext only.** No cipher, no envelope. Encryption of these families is PR-3
  (ZEB-983 `DeviceCipher`), deliberately deferred so this PR's diff is pure recovery
  semantics under one review lens.
- **No write-atomicity change.** Folding the fixed-`.tmp`/no-fsync writers onto
  `save_atomically` is PR-2. This PR does not touch the *write* path except to add the
  frozen no-op guard.
- **Determinism / testability.** All time enters as an explicit `now_ms: u64`
  parameter (quarantine sidecar names and the sweep's age gate). No store reads the
  wall clock internally on a path a test exercises. (Follows the ZEB wall-clock-gate
  testing convention: existing now-injecting seams, fixed clocks in tests.)
- **Never panic on load.** Every load path returns a value (real or default); no
  `unwrap`/`expect` on file contents.

## The shared helper: `src/recoverable_load.rs` (new module)

A plaintext load-or-recover primitive. It owns the `fs::read` so it can classify
Io vs content; the store supplies a `parse` closure for deserialize + version/shape
validation.

> **Note:** this block shows the final single-policy shape. The original design had a
> two-policy `CorruptPolicy` enum; it was collapsed during review — see *Review-round
> revisions* at the end.

```rust
/// Outcome of a recoverable load.
pub struct Recovered<T> {
    pub value: T,
    /// True iff writes MUST be frozen: the on-disk bytes may still be good and must
    /// not be overwritten with `value` (which is a default). Set on a transient read
    /// error, and on a quarantine-rename failure (could not move the corrupt file
    /// aside, so healing over it would clobber recoverable bytes — the ZEB-784 rule).
    pub disk_write_frozen: bool,
}

/// Classification:
///   missing file        -> (default, frozen=false)            [first run, silent]
///   read Io error       -> (default, frozen=true)  + warn      [preserve maybe-good bytes]
///   parse Err(reason)   -> quarantine aside (`.corrupt-<ms>`), heal next write
///                          -> (default, frozen=false) + warn; rename fail -> frozen=true
///   parse Ok(v)         -> (v, frozen=false)
///
/// A store that wants an unsupported-but-parseable file frozen *in place* (rather than
/// quarantined) — e.g. vine-feed's forward-version case — parses it as `Ok` and decides
/// to freeze at its own layer.
pub fn load_or_recover<T: Default>(
    path: &Path,
    now_ms: u64,
    parse: impl FnOnce(&[u8]) -> Result<T, String>,
) -> Recovered<T>;

/// Rename `path` -> `<path>.corrupt-<now_ms>` (dash dialect, matching the
/// fleet/sync majority). Best-effort; returns false and warns on rename failure.
fn quarantine(path: &Path, now_ms: u64) -> bool;
```

The `warn!` messages carry `path`, the classification (`io` / `corrupt` / `quarantined`
/ `frozen`), and the reason string, so a wrong-permissions/bad-disk situation is
distinguishable from first boot in the logs.

### Why a new module rather than reuse

`persistent_card_store`'s freeze logic and `fleet_dataset_file::load_or_recover` are
both bound to the sealed-image envelope (`DeviceCipher` / `DatasetCipher`,
`ImageError`, `SyncError`). Neither drops onto a plaintext `serde_json`/`ciborium`
store. `recoverable_load` is the plaintext generalization of the same two disciplines;
when PR-3 seals these files, the parse closure will decode the envelope and the same
`Recovered`/policy split still applies unchanged.

## Per-store integration

Each store gains a `disk_write_frozen: bool` field (set once at load, never mutated)
and an early-return no-op at the top of its `save()`:

```rust
if self.disk_write_frozen {
    tracing::warn!(path = ?self.path, "save skipped — file unreadable at load; preserving existing bytes");
    return; // (or Ok(()) for Result-returning saves)
}
```

`now_ms` is threaded into each load (production callers pass the real clock; tests
pass a fixed value). Callers of the changed load signatures are updated accordingly.

1. **`follows.rs`** — `FollowManager` gains `disk_write_frozen`. `load(data_dir, now_ms)`
   calls `load_or_recover(path, now_ms, QuarantineAndHeal, parse)` where `parse`
   deserializes `FollowsFile` and enforces `version == FILE_VERSION`, returning
   `Ok(file.follows)` or `Err(reason)`. `save()` gets the frozen guard.
2. **`friend_nicknames.rs`** — **EXCLUDED after code verification (implementation
   finding).** The ticket's premise ("transient EIO + `set()` destroys all nicknames")
   is stale: ZEB-977 made this a legacy migration-only store, and `save()`/`set()` now
   have **zero production callers** (the write path is dead code). The two live readers
   are already safe: the migration (`contacts_commands::migrate_friend_nicknames_to_contacts`)
   uses an *error-preserving* read that leaves the file in place on any read/parse
   failure — deliberately NOT `load_or_default` — and lib.rs's display read
   (`FriendNicknames::load_or_default`) never writes back. Applying `QuarantineAndHeal`
   here would be net-negative: renaming a corrupt legacy file aside on the display path
   would make the migration's `legacy_nicknames_path.exists()` gate skip forever,
   converting a retry-on-next-boot state into permanent loss. Left untouched.
3. **`content_index.rs`** — `ContentIndex` gains `disk_write_frozen`.
   `load` → `QuarantineAndHeal` (revised in review — see the revisions section below).
   `save()` gains the frozen guard in addition to the existing bare-path guard. Corrupt
   ⇒ quarantine aside + empty in-memory + heal on next write (new ingests persist);
   transient Io ⇒ freeze.
4. **`vine_feed_cache.rs`** — `VineFeedCache` gains `disk_write_frozen`.
   `populate_from_disk` routes through `load_or_recover` (`QuarantineAndHeal`); the
   existing backward age-prune still runs on a successful parse. `save()` frozen guard.
5. **`vine_pull_driver.rs`** — `load_vine_pull(path, now_ms)` returns
   `Recovered<VinePullSidecar>`. The driver stores `disk_write_frozen` and skips
   `save_vine_pull` while it is set (the flag travels from load to the driver's save
   site at the `spawn_blocking(save_vine_pull)` call).

## Quarantine sidecar retention: `sweep_corrupt_sidecars`

```rust
/// Delete stale quarantine sidecars under `dir` (recursively). Matches BOTH dialects:
///   *.corrupt.<ms>   (dotted — community/channel/voting families)
///   *.corrupt-<ms>   (dashed — fleet/sync/DM families + PR-1 stores)
/// and BOTH files and directories (channel-log quarantines a whole `<root>` dir).
/// Age is the embedded `<ms>` (clock-independent; falls back to mtime if unparseable).
/// Retention: within each (parent dir, base-name) group, always keep the single
/// newest sidecar; delete any other whose age > `max_age_ms`. Best-effort and
/// non-fatal — logs a one-line summary (scanned / deleted / kept / errors); an
/// unreadable subdir is skipped, not fatal.
pub fn sweep_corrupt_sidecars(dir: &Path, now_ms: u64, max_age_ms: u64);
```

- **Policy:** `max_age_ms` = 30 days. Keep ≥1 newest per base name as a forensics
  floor regardless of age.
- **Where it runs:** once, early in node/app boot, given the resolved app data dir.
  Non-fatal: a sweep failure never blocks startup. (Exact call site pinned during
  implementation — the boot path that already knows the resolved data dir, guarded so
  it runs once per process.)
- **Bonus:** because it matches both dialects tree-wide, it also bounds the existing
  fleet/community/DM sealed-family quarantines that PR-1 does not otherwise touch —
  closing the "never GC'd anywhere" gap in one place.
- **Safety:** deletion is gated on (a) name matching a strict `.corrupt[.-]<digits>`
  suffix, (b) age > 30d, (c) not being the newest in its group. A directory sidecar
  is removed with `remove_dir_all` only after passing all three. Parse-failure on the
  `<ms>` suffix ⇒ treat as un-ageable ⇒ never deleted (kept), so an unrecognized name
  is never destroyed.

## Testing

Recovery-contract regression tests live in each store's in-file `#[cfg(test)] mod
tests` (none of follows/friend_nicknames/vine_pull has an integration file). Mirror
the card-store pins (`read_io_error_freezes_but_content_corruption_self_heals`,
`frozen_store_preserves_existing_file_on_flush`). Per store:

- **corrupt file ⇒ quarantined + empty + heals:** write garbage bytes, load, assert
  in-memory empty, assert a `.corrupt-<ms>` sidecar exists holding the original bytes,
  mutate + save, assert the main file is rewritten (not frozen).
- **content-index corrupt ⇒ freeze in place:** write garbage, load, assert empty +
  frozen, assert the original file is **untouched** (byte-identical, no sidecar),
  mutate + save, assert the file is **still** the original garbage (save was a no-op).
- **Io error ⇒ freeze:** simulate an unreadable file (unix: `chmod 0` parent or a
  path that is a directory) → load → assert frozen; mutate + save → assert the file is
  not overwritten. (`#[cfg(unix)]` where it needs a permission trick.)
- **missing file ⇒ empty, not frozen** (first-run): load on empty dir, mutate, save,
  assert the file is created.
- **quarantine-rename failure ⇒ freeze, no heal:** (unix) make the parent dir
  unwritable so the rename fails under `QuarantineAndHeal`; assert frozen and the
  original left in place.
- **`sweep_corrupt_sidecars`:** seed both dialects (file + dir) at varied embedded
  ages; assert >30d non-newest deleted, newest-per-base kept, <30d kept,
  unparseable-name kept, and a non-`.corrupt` file untouched.

Full gates before PR: `cargo fmt`, `cargo clippy --all-targets -D warnings`, scoped
`--lib` during iteration, full `--workspace --all-targets` sweep before push; frontend
untouched (no tsc/vitest needed, but run them to confirm zero drift).

## Out of scope (later PRs / follow-ups)

- Write-atomicity onto `save_atomically` + 0600 permissions — **PR 2**.
- At-rest sealing of these families + `storage_records`/`storage_ledger` TOFU anchors
  with signature re-verify on load — **PR 3**.
- content-index rebuild-from-directory-scan (recover the file→id mapping when the
  index is genuinely corrupt) — a possible future ticket; the quarantined
  `content-index.json.corrupt-<ms>` sidecar preserves the bytes (30-day window) so that
  feature can consume it.

## Review-round revisions (PR #730, bot convergence)

The initial design shipped a two-policy `CorruptPolicy` (`QuarantineAndHeal` /
`FreezeInPlace`), with content-index on `FreezeInPlace`. Bot review surfaced three
issues that revised this:

1. **content-index → `QuarantineAndHeal`** (CodeAnt *Critical*). Under `FreezeInPlace`
   a corrupt index still let `insert()` mutate in-memory and return `true` while `save()`
   was a no-op, so `send_ingest_with_name` reported ingest success while the entry never
   persisted — orphaning the just-stored blob after restart. A corrupt index cannot be
   read into memory under *either* policy, so old blobs are unreferenced regardless; the
   real difference is that `QuarantineAndHeal` lets **new** ingests persist and preserves
   the old bytes in a `.corrupt-<ms>` sidecar. Decision confirmed with the maintainer.
2. **`FreezeInPlace` removed.** With content-index moved off it, no store used it, so the
   `CorruptPolicy` enum was dropped entirely: `load_or_recover(path, now_ms, parse)` now
   always quarantine-and-heals (Io still freezes; a failed quarantine-rename still
   freezes). A store that wants an unsupported-but-parseable file frozen *in place*
   (vine-feed's forward-version case) parses it as `Ok` and freezes at its own layer.
3. **vine-feed unsupported version now freezes** (CodeRabbit + CodeAnt *Major*). The
   version check moved out of the parse closure; a forward/foreign-version file is frozen
   in place (not quarantined) so the next `save()` cannot overwrite it — honoring the
   "left intact for its originating build" rule the original comment claimed but did not
   enforce.
4. **Collision-safe quarantine** (CodeRabbit *Major*). Two corrupt loads of the same path
   at the same `now_ms` (e.g. a stuck clock) would have had the second `rename` replace
   the first sidecar (Unix) or fail (Windows). `quarantine` now probes for a free
   `<stamp>` (incrementing keeps the name sweep-parseable) so both payloads stay
   recoverable.

**Accepted degradation (not fixed):** while frozen (transient Io or failed quarantine),
`follows`/`vine_feed`/`vine_pull` mutations still return success and, for follows, publish
to the wire, even though the write did not persist and reverts next boot. This is a rare,
self-healing transient-error state (the freeze exists precisely to protect the existing
on-disk data, which is the PR's goal); surfacing a persistence error through every
mutator's return type and caller is disproportionate. content-index is exempt because its
`QuarantineAndHeal` path is never frozen on corruption, and the Io-freeze case is equally
rare — the CodeAnt Critical there was specifically the *silent-drop-then-orphan* under the
old `FreezeInPlace`, which the policy switch resolves.
