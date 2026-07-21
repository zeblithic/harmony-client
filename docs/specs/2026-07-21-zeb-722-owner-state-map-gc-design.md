# ZEB-722 — owner-state map GC on content burn (design)

**Status:** settled 2026-07-21. Follow-on to the encrypted-file-sharing arc
(ZEB-674 → 723 → 724 → 726, PRs #512–#515).

## Problem

`OwnerState.file_deks` and `OwnerState.file_grants` (added in ZEB-674) are
**grow-only**: an entry is inserted at ingest/share time and never removed when
the underlying content is burned. Each ~92-byte sealed DEK (plus its per-CID
grant list) then lingers forever in the owner state, which is **fully
replicated across the owner's bound devices** (Flow A). The project is
engineered "for billions", so an unbounded, replicated map is a real
scaling/hygiene liability — the sibling map `revoked_dm_devices` is already
bounded/pruned; these are not.

This is a scaling concern, not a correctness bug: ZEB-674's MVP scoped to the
encryption/sharing foundation and did not require GC.

## Why a plain `remove` is wrong here

The three ZEB-674 maps merge as a **grow-only union** (add-wins) in
`owner_state_sync::merge_remote_into_local`:

- `file_deks` — first-writer-wins `or_insert` per CID.
- `file_grants` — union per CID; revoke is an LWW-element-set on each
  `GrantEntry` (`revoked_at`), tombstoned not dropped.

Under add-wins union, an entry removed on device A is **resurrected** the next
time A merges a sibling B's snapshot that still holds it — and B never learns of
the removal. Convergent removal in an add-wins CRDT therefore requires a
**tombstone that propagates the removal**, exactly the idiom this codebase
already uses three times: `outbox_tombstones` (LWW-by-HLC), `file_grants` revoke
(LWW-element-set), and the permanent `SpaceId` `tombstones` set.

## Design — a permanent burn tombstone

Add one field, mirroring the existing permanent `tombstones: BTreeSet<SpaceId>`:

```rust
/// ZEB-722: CIDs of encrypted personal files that have been BURNED (the last
/// sidecar reference removed). A permanent tombstone that GCs the grow-only
/// `file_deks` / `file_grants` entries for the CID and — critically — keeps a
/// stale sibling device from resurrecting them on the add-wins union merge.
///
/// Permanent (never un-set) and HLC-free is SAFE here: encrypted ingest mints a
/// fresh RANDOM DEK (`generate_file_dek` = `EpochKey::random`) and ZEB-726
/// derives the frame nonce from the DEK, so re-ingesting identical plaintext
/// yields different ciphertext → a DIFFERENT CID. A burned CID is therefore
/// cryptographically unreproducible; it can never re-appear as a live entry, so
/// there is no "re-ingest after burn" race for an HLC to arbitrate. Absent on
/// the wire when empty (`skip_serializing_if` + `default`) so pre-ZEB-722
/// snapshots load empty.
#[serde(rename = "bt", skip_serializing_if = "BTreeSet::is_empty", default)]
pub burned_content: BTreeSet<[u8; 32]>,
```

### Local burn

`burn_content` (`src-tauri/src/lib.rs`) resolves a three-branch
`RuntimeAction`; the `Burn(cid)` arm fires **only when the last sidecar entry
referencing the CID is gone** (`idx.entries_for_cid(&cid).next().is_none()`) —
the precise "this content is truly gone" signal. GC hooks there:

1. Lock `crdt_state` (the `Arc<tokio::Mutex<OwnerState>>` already on `NodeState`).
2. `burned_content.insert(cid)`; `file_deks.remove(&cid)`; `file_grants.remove(&cid)`.
3. `sync_engine.notify_dirty()` — without it the mutation is neither persisted
   nor replicated (ZEB-709).

`burn_content` currently does not touch owner-state at all; the hook is additive
and best-effort-symmetric with its existing runtime-Burn dispatch (a missing
`crdt_state`/`sync_engine`, as in headless/early-boot, is a no-op, matching the
verb-tx `None` handling already there).

Burning one of several sidecar entries that share a CID does **not** GC (the
`Burn` arm doesn't fire until the last reference is removed), so shared content
keeps its DEK while any reference remains.

### Merge convergence

In `merge_remote_into_local`, union the tombstone, then sweep the maps against
it **after** the existing union loops (so a tombstone arriving in this merge
also cleans an entry the same merge just unioned in, and a first-writer-wins
`file_deks` re-add is immediately swept back out):

```rust
local.burned_content.extend(burned_content);      // grow-only union
local.file_deks.retain(|cid, _| !local.burned_content.contains(cid));
local.file_grants.retain(|cid, _| !local.burned_content.contains(cid));
```

**Convergence sketch.** `burned_content` is a join-semilattice under set union
(commutative, associative, idempotent). The sweep is a pure function of the
merged `burned_content` and the merged maps, so the post-merge state is
independent of merge order — device A burning and device B holding the entry
converge to `{tombstone present, entry absent}` on both. Burn is terminal: a
concurrent share of the same file (a `file_grants` append on B) is swept by A's
burn, which is the correct resolution — you cannot share a file you burned.

**No false sweep.** A burned CID is unique per encrypted ingest and
unreproducible (random DEK), so it can never collide with a live entry's CID; the
`retain` only ever drops the intended burned entries.

## Scope

**In:**
- `burned_content` field (`owner_state_crdt.rs`) + its persisted mirror
  (`owner_state_persist.rs`).
- Burn hook in `burn_content` (`lib.rs`) — GC `file_deks` + `file_grants`.
- Merge union + sweep (`owner_state_sync.rs`).
- Tighten `sealed_dek_at_rest_is_not_plaintext` — assert the unseal round-trip +
  structure, not merely a length inequality (ZEB-722 review nit).

**Deferred (own tickets / triggers):**
- **`received_file_grants` GC** — files OTHERS shared with this owner. Populated
  by `ingest_grant_push`, projected straight to "shared with me" DTOs; it does
  **not** flow through `burn_content` (no sidecar entry). Its GC needs a
  different trigger (grantee-dismiss, or owner-revoke propagation). New
  follow-up ticket.
- **`friend_aead` → `sealing_aead` rename** — the field name is serialized:
  `FleetKeyMaterial` (owner_state_crypto.rs) is ciborium-encoded by field name
  (no `#[serde(rename)]`) for sealed enrolled-device distribution and the
  encrypted-vault `fleet_keytree` slot. A naive rename breaks deserialization of
  stored KeyTree material. Makeable-safe later via `#[serde(rename =
  "friend_aead")]` to pin the wire key; not worth riding a CRDT-correctness PR.

## Testing

Mirror the existing `merge_remote_into_local_convergence_after_create_and_delete`:

1. **burn GCs the DEK** — ingest an encrypted file (`file_deks[cid]` present) →
   burn its last sidecar entry → assert `file_deks[cid]` and `file_grants[cid]`
   gone and `burned_content` contains the CID.
2. **merge converges across devices** — A burns; B still holds the entry; merge
   both directions → both reach `{tombstone, no entry}`.
3. **resurrection is swept** — a sibling snapshot re-supplying `file_deks[cid]`
   is dropped by the post-union sweep (order-independent).
4. **shared-CID guard** — two sidecar entries share a CID; burning one leaves the
   DEK intact (Burn arm doesn't fire).
5. Tightened `sealed_dek_at_rest` unseal round-trip.

Keychain-free, wall-clock-free. Full `nextest --workspace --all-targets
--features test-fixtures` + CI-exact clippy + fmt green locally before PR.

## Non-goals

- Bounding `burned_content` itself (32 bytes/burned-file, permanent like the
  `SpaceId` tombstone set; negligible, YAGNI now).
- Reconciling runtime bytes that survive a best-effort runtime-Burn failure
  (pre-existing `burn_content` behavior, unchanged).
