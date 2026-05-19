# ZEB-156 — Root-pin-set cascade for unpin/burn

Status: design (2026-05-19)
Linear: [ZEB-156](https://linear.app/zeblith/issue/ZEB-156/)
Bundles: [ZEB-160](https://linear.app/zeblith/issue/ZEB-160/) (pin/unpin/burn serialization mutex)

## Context

harmony-client's pin/unpin/burn pipeline today has two layers:

**Tauri command layer** (`src-tauri/src/lib.rs:5673-5840`) — sidecar source of truth.
- `pin_content(sidecar_id)`: flips `pinned: true` on the entry, looks up the CID, dispatches `Pin` verb.
- `unpin_content(sidecar_id)`: flips `pinned: false`. **Already does sibling-level OR-join via `ContentIndex::is_cid_pinned_by_any(&cid)`** — if any other sidecar entry still has the same CID with `pinned: true`, the `Unpin` verb is suppressed. This handles the "same file uploaded twice as two sidecar rows" case correctly.
- `burn_content(sidecar_id)`: removes the entry, then dispatches `Burn` (or `Unpin` or `Nothing` depending on sibling state).

**Event-loop verb layer** (`src-tauri/src/event_loop.rs:1719-1742`) — runtime cache + `pin_intent` mirror.
- `Pin`: `collect_descendants(root) → for each id, runtime.pin_content(id)` (idempotent at the cache layer).
- `Unpin`: same walk, calls `runtime.unpin_content(&id)` for every descendant.
- `Burn`: identical to `Unpin` — sidecar removal happens at the Tauri layer, so the runtime side just unpins.

### The bug

The Tauri OR-join only handles **sibling sharing** (two sidecar entries with the same root CID). It does NOT handle **transitive sharing** — where one sidecar entry's CID is a *descendant* of another sidecar entry's CID.

Worked example (post-ZEB-163, post-ZEB-161):
- User drops folder `C` containing files `A.txt` and `B.txt`. Sidecar gets one entry: `C` (cid = folder manifest CID; descendants include A and B's CIDs).
- User separately drops `A.txt` standalone. Sidecar gets another entry: `A` (cid = A's leaf CID).
- User pins both: `A` and `C`. Runtime `pin_intent` = `{cid_A, cid_C}`. Runtime cache pins: `walk(cid_C) ∪ walk(cid_A)` = `{cid_C, cid_A, A's chunks, B, B's chunks}`.
- User unpins `C`.
  - Tauri layer: sets `C.pinned = false`. `is_cid_pinned_by_any(cid_C)` → false (only C had cid_C). Dispatch `Unpin(cid_C)`.
  - Event loop: `pin_intent.remove(&cid_C)`. Cascade `walk(cid_C)` yields `{cid_C, cid_A, A's chunks, B, B's chunks}`. Calls `runtime.unpin_content` on each.
  - **`cid_A` and `A's chunks` are now unpinned in the runtime cache, even though entry `A` is still in the sidecar with `pinned: true`.** `pin_intent` still contains `cid_A` (correctly), but the cache state diverges.
  - Self-healing kicks in on the next `start_node` (the pin-restore sweep walks the sidecar and re-pins everything), but until restart the cache is wrong and `A`'s chunks become eligible for W-TinyLFU eviction.

ZEB-161 made this practically important — nested-bundle ingest produces deeper trees with more potential interior-CID sharing (CDC dedup, shared sub-folders across multi-folder drops).

### A second, distinct bug — ZEB-160

`pin_content` / `unpin_content` / `burn_content` Tauri commands update the sidecar under a short lock, release the lock, then `verb_tx.send(...).await` the runtime verb. Rapid toggling can interleave these so the runtime sees the verbs in a different order than the sidecar mutations occurred:

- t=0: `pin_content(X)` — lock sidecar, set `pinned=true`, UNLOCK, dispatch `Pin(cid_X)`.
- t=1: `unpin_content(X)` — lock sidecar, set `pinned=false`, UNLOCK, dispatch `Unpin(cid_X)`.
- Verbs land at event loop in order: `Unpin(cid_X)` then `Pin(cid_X)`. Sidecar says unpinned; runtime says pinned.

Bundled into this PR because the fix touches the same three Tauri commands and the cost of bundling is small.

## Decisions

| | Decision | Confirmed |
|---|---|---|
| D1 | Event-loop `Unpin`/`Burn` verbs compute a **keep set** from remaining `pin_intent` roots, and only call `runtime.unpin_content(id)` for descendants NOT in the keep set | 2026-05-19 |
| D2 | **Burn additionally calls `cache.remove(cid)`** for every CID it unpins (immediate expunge, don't wait for W-TinyLFU pressure) | 2026-05-19 |
| D3 | **Recompute keep set on every `Unpin`/`Burn`** call (no in-memory cache of effective set). Profile later if it dominates. | 2026-05-19 |
| D4 | `Pin` verb stays untouched — over-pinning is idempotent at the cache layer (`ContentStore::pin` returns whether newly pinned) and the current cascade is semantically correct under root-set semantics. | implied |
| D5 | **Bundle the ZEB-160 fix**: add a `NodeState::pin_serial_lock: Arc<tokio::sync::Mutex<()>>` and acquire it across (sidecar mutation + verb dispatch) in all three Tauri commands. | 2026-05-19 |

## API surface

### Modified — event loop verb handlers

```rust
ContentVerbRequest::Unpin { cid, reply } => {
    pin_intent.remove(&cid);
    let root = ContentId::from_bytes(cid);
    let doomed = collect_descendants(runtime.storage_tier().cache(), root);

    // Keep set: union of descendants of every remaining pinned root.
    // The user's intent is encoded in pin_intent (the runtime mirror of
    // sidecar `pinned=true` entries). Any descendant reachable from any
    // remaining root must stay pinned.
    let mut keep = HashSet::with_capacity(doomed.len());
    for keep_root_bytes in pin_intent.iter() {
        let kr = ContentId::from_bytes(*keep_root_bytes);
        keep.extend(collect_descendants(runtime.storage_tier().cache(), kr));
    }

    for id in doomed {
        if !keep.contains(&id) {
            runtime.unpin_content(&id);
        }
    }
    let _ = reply.send(Ok(true));
}

ContentVerbRequest::Burn { cid, reply } => {
    pin_intent.remove(&cid);
    let root = ContentId::from_bytes(cid);
    let doomed = collect_descendants(runtime.storage_tier().cache(), root);

    let mut keep = HashSet::with_capacity(doomed.len());
    for keep_root_bytes in pin_intent.iter() {
        let kr = ContentId::from_bytes(*keep_root_bytes);
        keep.extend(collect_descendants(runtime.storage_tier().cache(), kr));
    }

    for id in doomed {
        if !keep.contains(&id) {
            runtime.unpin_content(&id);
            // Burn-specific: also evict from the cache immediately
            // rather than waiting for W-TinyLFU pressure.
            let _ = runtime.storage_tier_mut().cache_mut().remove(&id);
        }
    }
    let _ = reply.send(Ok(true));
}
```

`ContentVerbRequest::Pin` is unchanged — the existing cascade is correct under root-set semantics.

### Modified — fetch-completion replay hook

`event_loop.rs:1774-1781` (the `fetch_completion_rx` arm that re-pins after a fetch lands) is also unchanged — it pins everything in `descendants(root)`, which is correct (idempotent re-pin matches the new model).

### New — `NodeState::pin_serial_lock`

```rust
pub struct NodeState {
    // ... existing fields ...
    /// ZEB-160: serializes the (sidecar mutation + runtime verb dispatch)
    /// sequence in `pin_content` / `unpin_content` / `burn_content` so
    /// rapid toggling can't reorder verb arrivals at the event loop
    /// relative to sidecar mutations. tokio Mutex (not std) so it's safe
    /// to hold across the `verb_tx.send(...).await`.
    pub(crate) pin_serial_lock: Arc<tokio::sync::Mutex<()>>,
}
```

The three Tauri commands acquire this lock at the top and hold it until the verb's reply (or send-failure) resolves. The existing `state.lock()` (sync mutex on `NodeState`) is acquired transiently inside that scope to read `(content_index, content_verb_tx)`.

### Cache eviction primitive

`harmony_content::cache::ContentStore::remove(&mut self, cid: &ContentId) -> Option<Vec<u8>>` already exists at `harmony/crates/harmony-content/src/cache.rs:424`. No new primitive needed. We expose it through `runtime.storage_tier_mut().cache_mut()` (the runtime's `&mut` accessor for the cache) — verify the exact method names against the runtime's current API in implementation.

## Test plan

### Unit / inline tests in `event_loop.rs`

The event-loop's `#[cfg(test)] mod tests` (if present) gains:

1. **Unpin with single root, no sharing** — verify behaviour identical to pre-fix: every descendant unpinned. Regression guard.
2. **Unpin with two roots sharing a leaf** — pin two synthetic roots whose `collect_descendants` overlaps on at least one CID; unpin one; assert the shared CID stays pinned (via `cache.is_pinned(&shared_cid)`).
3. **Unpin with two roots sharing a full subtree** — `C = bundle(A, B)`, pin `A` and `C` separately, unpin `C`, assert `A` and `A's chunks` stay pinned, `B` and `B's chunks` get unpinned, `C` itself gets unpinned.
4. **Burn evicts from cache** — pin a root, burn it, assert every descendant CID returns `None` from `cache.get()`. Distinct from unpin which keeps bytes in the cache (just demotes them).
5. **Burn respects keep set** — same shared-subtree fixture as test 3 but with burn instead of unpin; assert shared CIDs stay in the cache AND remain pinned (other root still holds them).
6. **Empty `pin_intent` corner case** — burn the only pinned root, keep set is empty, every descendant gets unpinned + evicted.

### Integration test — `content_index_integration.rs` or new `pin_cascade_integration.rs`

7. **Two-root sidecar fixture**: ingest a folder containing one leaf, separately ingest the same leaf as a standalone file. Pin both sidecar entries. Verify runtime cache has both pinned. Unpin the folder. Verify the leaf is still pinned in the cache.
8. **ZEB-160 race regression**: concurrently call `pin_content(X)` and `unpin_content(X)` 100 times in rapid succession; assert final state matches the last-issued command (sidecar and runtime cache agree).

### Tauri-command tests in `lib.rs`

9. **`pin_serial_lock` held across await**: assert that two concurrent `pin_content` calls don't interleave their sidecar mutations and verb dispatches (test via a recorded verb-tx + `tokio::join!` pattern).

## ZEB-157 (partial-ingest rollback) interaction

Unchanged. ZEB-157 will still need to clean up chunks from a failed ingest. The keep-set semantics here make ZEB-157 *easier* — once the failed root is discarded, the keep set's recompute won't include those chunks and the next unpin/burn cycle naturally drops them.

## Risks

- **R1 — Keep-set walk cost on large `pin_intent`.** Recomputing on every unpin/burn is O(|pin_intent| × avg_descendants). For 100 pinned items averaging 1000 chunks each, that's ~100k cache lookups per call. Each lookup is a HashMap hit (~µs), so total ~100ms worst case. Acceptable for v1; profile if it shows up in user-perceived latency.
- **R2 — `pin_intent` and sidecar drift.** `pin_intent` is rebuilt from the sidecar on `start_node`, so steady-state drift is bounded by uptime. The ZEB-160 mutex tightens this further by preventing the dispatch reorder that was the main drift source.
- **R3 — `tokio::sync::Mutex` held across `verb_tx.send()`.** The send is bounded by channel capacity (16-slot or whatever the event loop's `IngestRequest` / `ContentVerbRequest` channel sizes are) and the reply's `.await`. Worst case the lock holder waits for the event loop to drain. Lock contention is low because pin/unpin/burn are user-triggered, not autonomous.
- **R4 — `cache.remove(cid)` failure semantics in `Burn`.** The current `remove` returns `Option<Vec<u8>>` — `None` if the CID isn't in the cache. We ignore the return (`let _ = ...`). For a bundle CID that was never fetched into the cache (e.g., the user burns a remote pin they never resolved), `remove` is a no-op; that's correct.

## Non-goals

- **Effective-set caching.** Per D3, recompute every call. Cache as a follow-up if profiling demands.
- **Persisting the effective set.** Always derivable from `pin_intent` + cache structure. No new on-disk schema.
- **Per-CID locking granularity.** A single `pin_serial_lock` covers all three IPCs. Fine-grained per-CID locks would allow more concurrency but the user-triggered traffic volume doesn't justify the complexity.
- **Walker cancellation through pin verbs.** The verbs are bounded-latency (no streaming) — no cancellation primitive needed.
