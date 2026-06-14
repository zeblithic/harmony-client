# ZEB-460 — Fleet-dataset persist recovery: quarantine on corruption only, propagate transient I/O

**Status:** approved (bug fix; approach dictated by the ticket + codebase precedent)
**Ticket:** ZEB-460 (child of ZEB-418 SP2 Butler). Raised by CodeRabbit on PR #251.

## Problem

Each FleetSync-backed persist module exposes `load_doc_or_recover` /
`load_replay_or_recover` wrappers used at boot to load a replicated dataset.
The lower-level `load()` already distinguishes error kinds —
`SyncError::Persist` for a read I/O failure vs `SyncError::CborDecode` for
genuine corruption (empty file, unknown schema byte, decode failure, trailing
bytes) — and maps `NotFound` to `Ok(default)`.

But the `*_or_recover` wrapper collapses every non-`NotFound` error into a
single catch-all arm:

```rust
match load(path) {
    Ok(doc) => doc,
    Err(e) => { quarantine(path, &e); Default::default() }
}
```

So a **transient** I/O error (EACCES, EBUSY, a momentary FS hiccup) is treated
identically to permanent corruption: the bad file is renamed aside
(`.corrupt-<ms>`) and the dataset boots **empty**. The next persist then writes
that empty doc to the now-vacant canonical path → the real held data (deposited
DMs, relay blobs, fleet-net advertisements) is silently discarded; the original
bytes survive only as an orphaned `.corrupt-<ms>` file the app never reads back.

## Decision

Restore in `*_or_recover` the error-kind distinction `load()` already draws:

- `Ok` → `Ok(value)`.
- `Err(CborDecode)` → **permanent** corruption: quarantine the bad bytes aside
  and self-heal to `Ok(default())` (unchanged behaviour — the app still boots
  and never bricks on a corrupt file).
- `Err(_)` (i.e. `Persist`, transient) → **do NOT quarantine**; return `Err(e)`
  so the caller decides.

The wrappers therefore change signature from `-> T` to `-> Result<T, SyncError>`.

At the 12 boot call sites in `start_node_inner` (`lib.rs`), propagate the error
with `.map_err(|e| format!(...))?`, **failing the boot loudly**. This is the
established idiom in the very same boot region: the owner-state CRDT load already
does exactly this (`lib.rs` ~3660,
`.map_err(|e| format!("load owner_state_crdt.cbor: {e}"))?`).

### Why fail loudly rather than degrade-per-dataset

The ticket lists "propagate / retry / fail the boot loudly" as acceptable. The
asymmetry between the two error kinds makes loud-fail the right minimal choice:

- **Corruption is permanent** → auto-recover (quarantine + fresh). Retrying
  would loop forever, so the app must start. Unchanged.
- **Transient I/O is ephemeral** → fail this boot loudly; the retry on next
  launch succeeds with the file **intact** (we never renamed or overwrote it).
  `start_node` surfaces the `Err(String)` to the UI as a retryable start
  failure — no data is lost either way.

A per-dataset graceful-skip (mirroring the `'mint_init` break pattern) would
also be correct but requires wrapping six engine-construction blocks in labeled
blocks in the boot path — more surface, more risk, no data-safety gain over
loud-fail. Rejected for this fix.

## Scope

In scope — the six byte-identical FleetSync persist modules:
`dm_inbox_persist`, `dm_outhold_persist`, `fleet_net_persist`, `notes_persist`,
`relay_hold_persist`, `relay_optin_persist`.

Out of scope — verified already correct during investigation:

- `community_state_persist::load_replay` — quarantines on the decode arm only
  and `return Err(PersistError::Io(e))` for read I/O. Correct template.
- `mint_sync_persist`, `owner_state_persist` — propagate all load errors
  (`Err(e) => return Err(e.into())`); no quarantine, no boot-empty-on-error.

## Testing

Per module, add a test that a **transient** (non-decode) error does NOT
quarantine and surfaces `Err`:

- Force a `SyncError::Persist` from `load()` by pointing it at a **directory**
  (`std::fs::read` on a dir returns a non-`NotFound` error → mapped to
  `Persist`). Portable across macOS / Linux.
- Assert `load_*_or_recover(&path)` returns `Err(SyncError::Persist(_))`, the
  path is **not** renamed aside, and no `.corrupt-*` sibling was created.

Existing tests that exercised the corruption path keep their behaviour but now
unwrap the recovered default (`load_doc_or_recover(&path).unwrap()` →
`Ok(default)`), confirming corruption still self-heals.

## Risk

Low. Corruption recovery is behaviourally unchanged; the only new behaviour is
that a (rare) transient read error at boot now aborts the start attempt loudly
and retryably instead of silently discarding state. No wire-format change.
