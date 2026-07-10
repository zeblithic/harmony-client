# ZEB-672: Vine Reaction Rehydration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Vine reaction hearts (counts + own-like state) survive app restart by exposing the Rust cache's persisted reaction rows through a new `list_vine_reactions` IPC and merging them in `VineService.hydrate()`.

**Architecture:** `vine_feed_cache` already persists reaction rows (`HashMap<(vine_id, reactor_address), CachedReaction>`) to `vine_feed.json` and restores them on load — but the only frontend path is the live `vine-reaction-received` event (event_loop.rs), which never re-fires for reloaded rows. We add a flat-row query IPC (GUI command + headless RPC, mirroring `list_vine_videos`), merge rows into the frontend `reactionMap` during hydrate with the same dedupe posture as the live handler, and fix two adjacent correctness hazards: App.svelte fetches `ownAddress` only *after* hydrate (self-row `likedByMe` unrecognizable), and `_toggleLikeInner` mutates `count` without consulting the `reactors` set (double-count when a hydrated self row pre-exists).

**Why flat rows, not an aggregate:** the frontend `reactionMap` keeps `reactors: Set<string>` to dedupe live events; an aggregate (`ReactionSummary`) cannot rebuild that set, so live events arriving post-hydrate would double-count. Rows rebuild the exact set.

**Tech Stack:** Rust (Tauri command + headless RPC), TypeScript (VineService), vitest, cargo-nextest.

## Global Constraints

- Tauri IPC: Rust params snake_case, JS callers camelCase; DTOs `#[serde(rename_all = "camelCase")]`.
- Wire row shape (pinned by tests): `{ vineId, reactorAddress, reactorName, liked, timestamp }`.
- Error extraction: `e instanceof Error ? e.message : String(e)`.
- Gates: `npx tsc --noEmit`, `npx vitest run`, `cd src-tauri && cargo fmt --all`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; iterative Rust tests via `scripts/test-select --context task`; final pre-PR full sweep `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- One commit per task; trailers per session convention.

---

### Task 1: Rust — `VineReactionRow` DTO + `VineFeedCache::list_reactions()`

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs` (DTO near `ReactionSummary` ~line 117; method near `get_reaction` ~line 521; tests near `reaction_insert_persists_to_disk_without_update` ~line 1463)

**Interfaces:**
- Produces: `pub struct VineReactionRow { vine_id, reactor_address, reactor_name, liked, timestamp }` (camelCase serde) and `pub fn list_reactions(&self) -> Vec<VineReactionRow>` sorted by `(vine_id, reactor_address)` for deterministic output.

- [ ] **Step 1: Write failing tests** — (a) `list_reactions_returns_rows_after_reload`: insert two reactions via `on_reaction_sample`, `save()`, `load()` into a fresh cache, assert both rows come back with fields intact; (b) `vine_reaction_row_camel_case`: `serde_json::to_value` pins `vineId`/`reactorAddress`/`reactorName`/`liked`/`timestamp` keys.
- [ ] **Step 2: Run to verify fail** — `cargo nextest run --locked -p harmony-app -E 'test(list_reactions)' --features test-fixtures` fails to compile (method missing).
- [ ] **Step 3: Implement** —

```rust
/// Flat reaction row for the `list_vine_reactions` IPC (ZEB-672).
/// Mirrors the `vine-reaction-received` event payload shape so the
/// frontend hydrate merge and the live handler share one wire model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VineReactionRow {
    pub vine_id: String,
    pub reactor_address: String,
    pub reactor_name: String,
    pub liked: bool,
    pub timestamp: u64,
}

/// All cached reaction rows (liked and unliked), sorted by
/// `(vine_id, reactor_address)` for deterministic output. Orphan rows
/// never appear: `load()` prunes reactions whose descriptor was dropped.
pub fn list_reactions(&self) -> Vec<VineReactionRow> {
    let mut rows: Vec<VineReactionRow> = self
        .reactions
        .iter()
        .map(|((vine_id, reactor_address), r)| VineReactionRow {
            vine_id: vine_id.clone(),
            reactor_address: reactor_address.clone(),
            reactor_name: r.reactor_name.clone(),
            liked: r.liked,
            timestamp: r.timestamp,
        })
        .collect();
    rows.sort_by(|a, b| {
        (a.vine_id.as_str(), a.reactor_address.as_str())
            .cmp(&(b.vine_id.as_str(), b.reactor_address.as_str()))
    });
    rows
}
```

- [ ] **Step 4: Run to verify pass**, **Step 5: fmt + commit** `feat(zeb-672): expose cached vine reaction rows via list_reactions`

### Task 2: Rust — `list_vine_reactions` IPC (GUI command + headless RPC)

**Files:**
- Modify: `src-tauri/src/lib.rs` (impl + command after `list_vine_videos` ~line 13178; register in `invoke_handler` list ~line 53306)
- Modify: `src-tauri/src/api/rpc.rs` (rpc! registration after `list_vine_videos` ~line 683; method-name inventory ~line 1560)

**Interfaces:**
- Consumes: Task 1's `list_reactions()`.
- Produces: `pub(crate) fn list_vine_reactions_impl(state: &Mutex<NodeState>) -> Result<Vec<VineReactionRow>, String>`; GUI command `list_vine_reactions` (no args); headless RPC `"list_vine_reactions"`.

- [ ] **Step 1: Implement** (mirror `list_vine_videos_impl` exactly, including the `"not connected"` contract):

```rust
/// Shared seam for `list_vine_reactions` (GUI command + headless RPC):
/// return all persisted reaction rows so a restarted frontend can rebuild
/// its reaction map — live `vine-reaction-received` events only fire on
/// first receipt, never for cache-reloaded rows (ZEB-672).
pub(crate) fn list_vine_reactions_impl(
    state: &Mutex<NodeState>,
) -> Result<Vec<crate::vine_feed_cache::VineReactionRow>, String> {
    let cache = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .vine_feed_cache
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    let result = cache
        .lock()
        .map_err(|e| format!("vine_feed_cache lock: {e}"))?
        .list_reactions();
    Ok(result)
}

#[tauri::command]
fn list_vine_reactions(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<crate::vine_feed_cache::VineReactionRow>, String> {
    list_vine_reactions_impl(state.inner())
}
```

rpc.rs (next to the `list_vine_videos` rpc! block, and add `"list_vine_reactions"` to the vines group in the method inventory):

```rust
rpc!(
    m,
    "list_vine_reactions",
    EmptyArgs,
    |state, _sink, _a| async move { crate::list_vine_reactions_impl(state) }
);
```

- [ ] **Step 2: Test** — mirror the existing not-connected/dispatch test pattern for `list_vine_videos` in rpc.rs tests if present (verify while editing); at minimum the inventory test at ~1560 pins registration.
- [ ] **Step 3: Gates** — `scripts/test-select --context task`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo fmt --all`.
- [ ] **Step 4: Commit** `feat(zeb-672): list_vine_reactions IPC — GUI command + headless RPC`

### Task 3: Frontend — hydrate merge + toggle idempotence + App ordering

**Files:**
- Modify: `src/lib/vine-service.ts` (`hydrate()` ~line 364; `_toggleLikeInner` ~line 450)
- Modify: `src/App.svelte` (move `await tryConnect('vine.hydrate', …)` from ~line 2176 to after `await fetchOwnAddress()` ~line 2249)
- Test: `src/lib/vine-service.test.ts` (hydrate describe ~line 931 — update mocks to answer `list_vine_reactions`; new tests)

**Interfaces:**
- Consumes: `list_vine_reactions` rows `{vineId, reactorAddress, reactorName, liked, timestamp}`.
- Produces: no signature changes; `hydrate()` additionally rebuilds `reactionMap`.

- [ ] **Step 1: Write failing tests** —
  1. `restores reaction counts and likedByMe from list_vine_reactions` (2 rows for one vine incl. own address → `getReaction` = `{count: 2, likedByMe: true}`).
  2. `dedupes hydrated rows against a live event that raced ahead` (emit live reaction for reactor X, then hydrate returns X's row → count 1, not 2).
  3. `skips liked=false rows and rows for unknown vines`.
  4. `toggleLike on a hydrated own-like unlikes instead of double-counting` (hydrate own row → toggle → `{count: 0, likedByMe: false}`, publish called with `liked: false`).
  5. Update the five existing hydrate mocks: `cmd === 'list_vine_videos' ? rows : cmd === 'list_vine_reactions' ? [] : …`.
- [ ] **Step 2: Verify fail**, **Step 3: Implement**:

hydrate() — after the descriptor/viewed merge, before the final onChange (replace the existing onChange condition):

```ts
// ZEB-672: rebuild the reaction map from the Rust-persisted rows —
// vine-reaction-received only fires on live receipt, so reloaded
// reactions never re-emit. Same dedupe posture as the live handler
// (reactors set); rows are orphan-pruned by the cache so the known-vine
// check is belt-and-braces. Requires ownAddress to already be set to
// recognize our own row (App.svelte hydrates after fetchOwnAddress).
const reactionRows = (await this.adapter.invoke('list_vine_reactions', {})) as Array<{
  vineId: string; reactorAddress: string; reactorName: string;
  liked: boolean; timestamp: number;
}>;
let reactionsChanged = false;
for (const row of reactionRows) {
  if (!row.liked) continue; // unliked rows carry no boot-time count state
  const known = this.followedVines.some(v => v.id === row.vineId)
    || this.discoverVines.some(v => v.id === row.vineId);
  if (!known) continue;
  const entry = this.reactionMap.get(row.vineId)
    ?? { count: 0, likedByMe: false, reactors: new Set<string>() };
  if (!entry.reactors.has(row.reactorAddress)) {
    entry.reactors.add(row.reactorAddress);
    entry.count += 1;
    reactionsChanged = true;
  }
  if (this.ownAddress != null && row.reactorAddress === this.ownAddress && !entry.likedByMe) {
    entry.likedByMe = true;
    reactionsChanged = true;
  }
  this.reactionMap.set(row.vineId, entry);
}
if (viewedGrew || followedAdd.length > 0 || discoverAdd.length > 0 || reactionsChanged) this.onChange?.();
```

_toggleLikeInner — guard count math on set membership (fixes the latent double-count; the optimistic count/set updates become idempotent):

```ts
entry.likedByMe = newLiked;
if (newLiked) {
  if (!entry.reactors.has(selfKey)) {
    entry.reactors.add(selfKey);
    entry.count += 1;
  }
} else {
  if (entry.reactors.has(selfKey)) {
    entry.reactors.delete(selfKey);
    entry.count = Math.max(0, entry.count - 1);
  }
}
```

(mirror the same guard in the rollback branch)

App.svelte — move the hydrate line below `await fetchOwnAddress()` with the comment amended: hydrate stays after `loadFollowed` (classification) AND now after `fetchOwnAddress` (self-row `likedByMe`).

- [ ] **Step 4: Verify pass** — `npx vitest run src/lib/vine-service.test.ts`; `npx tsc --noEmit`.
- [ ] **Step 5: Commit** `fix(zeb-672): rehydrate vine reactions on boot; idempotent like toggle`

### Task 4: Full gates + PR

- [ ] `npx tsc --noEmit && npx vitest run` (full frontend)
- [ ] `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (final full sweep)
- [ ] Open PR to main; fire `@coderabbitai review` once at open; converge rounds per standing flow.
