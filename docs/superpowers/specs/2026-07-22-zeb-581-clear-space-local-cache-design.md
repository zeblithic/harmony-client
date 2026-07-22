# ZEB-581 — `clear_space_local_cache` design

**Ticket:** [ZEB-581](https://linear.app/zeblith/issue/ZEB-581) — Reclaim a left community's local disk cache without tombstoning (keep it rejoinable).

**Goal (one sentence):** Add a backend op that deletes a *left* community's on-disk dataset (`~/.harmony/communities/<id>/`) to reclaim disk, **without** tombstoning it — so the community stays rejoinable and a later rejoin re-backfills history from peers (ZEB-418 P3a).

## The three-state model this fills

A community's local data has three states; the middle one is unbuilt:

| State | `left_at` | Space row | Local dir | Rejoinable? | Built? |
|---|---|---|---|---|---|
| `leave_community` | set | kept | **kept** | yes | ✅ `lib.rs:41014` |
| **`clear_space_local_cache` (THIS)** | set (untouched) | kept | **deleted** | yes | ← this PR |
| `remove_space` | — | tombstoned + removed | deleted | **no** (permanent) | ✅ `lib.rs:41998` (backend, no GUI) |

`remove_space` writes a permanent tombstone (`owner_state_crdt.rs:430 tombstone_space` → `apply_space` `:244` rejects re-add; no untombstone path). Because a community's `SpaceId` **is** its stable `community_id`, that tombstone permanently bars rejoin — so `remove_space` can't be used for routine disk hygiene. This op fills the gap: reclaim disk while keeping rejoinability.

## Architecture — a strict *subset* of `remove_space_impl`

`clear_space_local_cache_impl` mirrors `remove_space_impl` (`lib.rs:41998`) but **drops the entire tombstone/durability-fence path**. The `fence_remove_space_flush` machinery in `remove_space` exists solely so the destructive delete never outruns the *tombstone's* persistence (a crash mustn't resurrect the Space while its bytes are gone). We write **no tombstone** and **don't touch `left_at`** — the Space row's durability was already established by `leave_community` — so there is nothing to fence. What must stay:

- the **`left_at` precondition guard** (ticket §2), and
- the **node-generation re-check** before the destructive delete (deleting a community dir out from under a freshly-started live engine after a concurrent `start_node` is the ZEB-427 split-brain data-loss hazard the ticket names).

Both the deletion mechanic and the no-registry fallback already exist and are reused verbatim:
- `community_state_sync.rs:4880 shutdown_engine_and_cleanup_persistence` — stop engine → `remove_dir_all` on `identity_dir/communities/<id_hex>/`; idempotent on missing dir.
- `lib.rs:41885 cleanup_community_data` — registry present → the above; registry absent (owner loaded, node not started) → `lib.rs:41956 delete_community_dir` (plain filesystem delete, idempotent on absent).

## Behaviour spec — `clear_space_local_cache_impl(state, community_id: String) -> Result<(), String>`

1. **Parse** `community_id` hex → `[u8; 16]` → `SpaceId`; `id_hex = hex::encode(...)`. Bad hex / wrong length → `Err` (mirror `remove_space_impl:42002-42006` messages).
2. **Snapshot** under the `NodeState` lock: `(dm_self_owner, community_registry, crdt_state, generation)`. (No `sync_engine` — we never flush a tombstone.)
3. **`check_generation` closure** — re-reads `g.generation`, `Err` if it changed vs the snapshot (concurrent `stop_node`/`start_node`). Same shape as `remove_space_impl:42027-42039`.
4. **Look up the Space** in `crdt_state`:
   - not found → `Err("no space {id_hex} to clear")`. (No tombstone-retry branch — we never tombstone; an already-`remove_space`d community is gone and its dir already deleted.)
   - kind ≠ `SpaceKind::Community` → `Err("clear_space_local_cache: {id_hex} is not a community (only communities have a local dataset dir)")`. Only communities own `communities/<id>/`.
   - `left_at.is_none()` → `Err("community {id_hex} has not been left — call leave_community before clearing its local cache")`. (Ticket §2 precondition.)
5. **Defense-in-depth** (mirror `remove_space_impl:42083-42099`): if a live engine exists for this id, materialize self against the membership CRDT; if we're still an active member (`Joined`/`PendingJoin`), refuse with a "still an active member — leave first" `Err`. (`left_at` can be set locally before the Leave actually commits; clearing under a truly-active membership would trigger a needless split-brain re-download.)
6. **Delete:** `check_generation()?;` then `cleanup_community_data(&community_registry, &space_id, &id_hex).await;`. **No `tombstone_space`. `left_at` untouched.**
7. **Return `Ok(())`.** Idempotent: dir already absent → the reused helpers no-op → `Ok`.

## Surfaces (Jake's fork: **IPC + headless verb + tests**, no GUI)

1. **`#[tauri::command] async fn clear_space_local_cache(state_lock, community_id: String) -> Result<(), String>`** delegating to `clear_space_local_cache_impl` — mirror `remove_space` wrapper (`lib.rs:42147-42153`). Register in `generate_handler!` at `lib.rs:64745` (next to `remove_space,`).
   - Param is `community_id` (JS camelCase `communityId`) — this op is community-only, unlike `remove_space`'s generic `space_id`.
2. **Headless verb** in `api/rpc.rs`: an `rpc!(m, "clear_space_local_cache", CommunityIdArgs, |state, _sink, a| async move { crate::clear_space_local_cache_impl(state, a.community_id).await })` next to the `remove_space` verb (`rpc.rs:758-763`). Uses `CommunityIdArgs` (community-only), NOT `SpaceIdArgs`. Add the verb name to the verb-name assertion list near `rpc.rs:2256` if one gates registration.
3. **Unit tests** — a `#[cfg(test)] mod clear_space_local_cache_tests` mirroring `remove_space_tests` (`lib.rs:42155`). Six shipped:
   - unknown id → `Err("no space … to clear")`;
   - oversized hex (≠ 32 chars) → `Err` via the length-guard, before any decode (Qodo PR #530);
   - not-a-community kind → `Err`;
   - community not left (`left_at.is_none()`) → `Err`, **and a planted dir survives** (destructive-safety: a refused op deletes nothing);
   - **happy path**: left community with an on-disk dir → `Ok`, dir gone, **Space row still present with `left_at` still set** (the load-bearing invariant vs `remove_space`), **no tombstone written**;
   - **idempotent**: plant a dir, clear (dir deleted), clear again (dir absent) → `Ok`, row + `left_at` kept.
   - **Waived** — a live-engine *still-active-member* (`Joined`) refusal test: the active-member path reuses the already-tested pure guard `remove_space_community_guard` (`remove_space_tests::guard_refuses_only_active_members`), matching the sibling's coverage posture; standing up a live `CommunitySyncEngine` fixture here is disproportionate. The node-generation-abort concurrency seam is likewise impractical to unit-test (identical to `remove_space`).

## Out of scope (explicitly deferred)

- **GUI** — ticket §4 defers UX; a "Clear cache" affordance on the ZEB-435 left-communities surface is a later ticket.
- **Cascade into `leave_community`** — ticket §4 muses clear-cache *could* be folded into leave; that's a UX/product call, standalone IPC here.
- **Non-community spaces** — DMs/channels have no dataset dir; rejected, not silently no-op'd.

## Global constraints

- Rust gates (from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend gate is untouched (no JS in this PR beyond the implicit IPC name).
- IPC naming: Rust `snake_case` params, JS `camelCase` (`community_id` ↔ `communityId`).
- Tauri error extraction / messages are plain `String`s (production rejections are strings).
- Reuse `cleanup_community_data` / `shutdown_engine_and_cleanup_persistence` / `delete_community_dir` verbatim — do **not** re-implement dir deletion.
- **Second-order correctness:** the diff must NOT write a tombstone, must NOT clear `left_at`, and must NOT add an anti-backdating or `left_at`-mutating side effect — those would silently convert this reversible op into an irreversible one.
