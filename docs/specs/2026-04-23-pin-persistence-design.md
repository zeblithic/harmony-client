# Pin Persistence Across Restart (ZEB-155)

**Status:** Draft
**Scope:** `harmony-client` — `ContentIndexEntry` schema extension, `pin_content` / `unpin_content` / `list_content` command behavior, and a new fetch-completion replay hook in the event loop. No frontend changes; no wire-format changes.
**Linear:** [ZEB-155](https://linear.app/zeblith/issue/ZEB-155)
**Depends on:** [ZEB-146](https://linear.app/zeblith/issue/ZEB-146) (sidecar infrastructure) and [ZEB-154](https://linear.app/zeblith/issue/ZEB-154) (bundle-aware cascade verbs). Both merged on `main`.

## Background

As of [ZEB-146](https://linear.app/zeblith/issue/ZEB-146), the client persists per-content sidecar metadata (`file_name`, `stored_at`, `sensitivity`, `replication_tier`, `licensed`, `archived`) to `app_data_dir/content-index.json`. The `pinned` flag is explicitly NOT in the sidecar — it lives in the runtime cache's `PinnedSet`, which is pure RAM and resets on every app restart.

Observed during the ZEB-146 smoke test on 2026-04-23: a file pinned through the File Manager UI still appears in the list after a restart (sidecar survived) but shows `pinned: false`. This is strictly correct for the current architecture — pin is an eviction concept the cache owns, and with no durable byte storage there are no bytes to protect — but it leaks the RAM-only architectural limitation into user-visible UX. The pin click survives logically for this session only; the next restart erases the user's intent.

This spec persists pin intent on the sidecar, joins it with the runtime's pin effect at display time, and re-pins automatically when bytes re-enter the cache via a fetch.

## Design decisions

Four scope questions shaped this during brainstorming:

1. **Where does persisted intent live?** → **Field on the existing sidecar entry.** A new `pinned: bool` field on `ContentIndexEntry`. Using `#[serde(default)]` keeps v1 sidecars readable — old entries deserialize cleanly with `pinned: false`, which is correct (they weren't pinned at their last save). No version bump, no data migration. Because the sidecar only ever holds user-ingested root CIDs (never leaves), the set of sidecar entries with `pinned: true` is a root-pin-set by construction — the same shape [ZEB-156](https://linear.app/zeblith/issue/ZEB-156) will later make explicit.

2. **What does startup replay look like?** → **Display-layer join + fetch-completion hook.** Two pieces: (a) `list_content` changes its `pinned` computation from `runtime.contains(cid)` to `sidecar.pinned || runtime.contains(cid)` — restores the pin badge unconditionally from intent, even when bytes aren't resident. (b) A dedicated fetch-completion arm in `event_loop.rs` observes successful `fetch_recursive` completions: if the fetched root carries sidecar pin intent, it collects descendants from the runtime cache and calls `runtime.pin_content` directly for each so the runtime's cascade protects the now-resident bytes. This is not re-issued through the `ContentVerbRequest::Pin` verb channel — it's its own `select!` arm fed by a completion channel the fetch task writes to after replying to the Tauri caller.

3. **Eager startup re-pin into the runtime cache?** → **No.** The cache is empty at startup. `runtime.pin_content(cid)` on a non-admitted CID is a no-op (`iter_admitted()` is empty, so there's nothing to pin). Eager replay would do no useful work in the RAM-only world; it only becomes meaningful once durable byte storage lands, and at that point it's a separate concern.

4. **Pins on CIDs not in the sidecar (e.g., cached DM attachments)?** → **Runtime-only, no persisted intent.** `set_pinned` on a missing CID returns `false` and is a no-op (matches `set_archived`'s existing contract). The runtime-side Pin still goes through. Intent is sidecar-scoped; content that doesn't have a sidecar entry doesn't get persisted intent, and that's the correct default.

## Architecture

Two sources of pin information, one display join, one replay hook:

```text
┌────────────────────┐          ┌─────────────────────────┐
│  sidecar (on disk) │          │ runtime cache (in RAM)  │
│  content-index.json│          │ PinnedSet + W-TinyLFU   │
│                    │          │                         │
│  pin INTENT —      │          │  pin EFFECT —           │
│  "user wants this  │          │  "these admitted CIDs   │
│   pinned, for      │          │   are protected from    │
│   whenever bytes   │          │   eviction right now"   │
│   are resident"    │          │                         │
└──────────┬─────────┘          └───────────┬─────────────┘
           │                                │
           │   list_content joins both:     │
           │   pinned = intent || effect    │
           └────────────┬───────────────────┘
                        ▼
                 ContentItemWire → UI

           Replay hook: on fetch_recursive success for
           a root with intent=true, issue Pin(root) so
           the existing cascade pins the now-resident
           bundle tree.
```

The sidecar is the durable authority for intent. The runtime cache is the ephemeral authority for effect. The display layer ORs them. The replay hook re-converges them after a fetch.

## Data model

### `ContentIndexEntry` (change)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentIndexEntry {
    #[serde(with = "hex_cid")]
    pub cid: [u8; 32],
    pub file_name: String,
    pub size_bytes: u64,
    pub stored_at_ms: u64,
    pub sensitivity: Sensitivity,
    pub replication_tier: ReplicationTier,
    pub licensed: bool,
    pub archived: bool,
    /// ZEB-155: persisted pin intent. True when the user has asked for this
    /// content to remain pinned across restarts. The runtime cache's
    /// `PinnedSet` is still authoritative for active eviction protection —
    /// this field is "the user wants this pinned whenever bytes are
    /// resident," joined with the runtime set at `list_content` time.
    #[serde(default)]
    pub pinned: bool,
}
```

`#[serde(default)]` is load-bearing: it's what makes legacy sidecars without the field deserialize to `pinned: false` instead of erroring. `FILE_VERSION` stays at `1`. We do not wipe old sidecars.

### `pin_intent` (event loop, in-memory cache)

```rust
/// Event-loop-owned snapshot of sidecar-persisted pin intent. Rebuilt from
/// the sidecar on every start_node; kept in sync by the Pin/Unpin arms.
/// Used by the fetch-completion hook to decide whether to re-pin a root.
pin_intent: std::collections::HashSet<[u8; 32]>,
```

This is a performance cache to keep disk I/O off the fetch hot path. The sidecar is the source of truth; on any start_node the in-memory set is rebuilt from disk, self-healing any drift.

## Control flow

### Write path

`pin_content(cid)` Tauri command:

1. Parse CID (reject malformed).
2. **Sidecar first:** acquire `ContentIndex` lock, call `set_pinned(&cid, true)`. Atomic tmp+rename write. On I/O failure, log via `tracing::warn!` and continue — matches existing best-effort sidecar semantics (`set_archived`, `set_replication_tier`).
3. **Runtime second:** send `ContentVerbRequest::Pin { cid, reply }` on the unified verb channel.
4. **Event-loop arm:** on receipt, insert `cid` into `pin_intent` before dispatching the existing cascade. `runtime.pin_content(id)` on each descendant returned by `collect_descendants`.
5. Return the runtime's reply bool to the frontend.

`unpin_content(cid)`: mirrored. Sidecar `set_pinned(&cid, false)`, then `ContentVerbRequest::Unpin`. The `Unpin` arm removes `cid` from `pin_intent` and dispatches the cascade.

`burn_content(cid)`: unchanged surface — it already does `ContentVerbRequest::Burn` then `ContentIndex::remove(&cid)`. The `Burn` arm additionally removes `cid` from `pin_intent` (safe if absent). Removed sidecar entries carry no intent by construction, so no special handling.

**Why sidecar-first?**
Durability bias. If the runtime-side call fails (event loop gone, channel closed), the sidecar intent still carries forward to the next session and the user's action persists as intent. If the sidecar write fails, the runtime-side still takes effect — worst case matches pre-ZEB-155 behavior (the pin exists this session, is lost on restart). Runtime-first would risk "pinned in runtime but never persisted" on crash between steps — a silent regression on restart.

### Read path

`list_content`:

```rust
// Before (ZEB-146):
pinned: pinned_set.contains(&e.cid),

// After (ZEB-155):
pinned: e.pinned || pinned_set.contains(&e.cid),
```

One line. No new Tauri command, no wire-format change (`ContentItemWire.pinned` is already a `bool`), no frontend work.

### Fetch-completion replay hook

Two arms in `event_loop.rs`'s `select!`: the existing `fetch_rx` arm spawns a task that replies to the caller first, then best-effort signals a completion channel. A dedicated `fetch_completion_rx` arm consumes those signals and runs the repin cascade.

```rust
// Pseudocode — the real fetch_rx arm does more handshaking; the
// essentials are that it replies to the Tauri caller BEFORE the
// try_send so a full completion channel cannot stall the reply.
Some(req) = fetch_rx.recv() => {
    let root = /* parse req.cid_hex */;
    let completion_tx = fetch_completion_tx.clone();
    tokio::spawn(async move {
        let result = fetch_recursive(|id| fetch_one(id), root).await;
        let is_ok = result.is_ok();
        let _ = req.reply.send(result);
        if is_ok {
            // try_send: reply correctness must not depend on channel capacity.
            let _ = completion_tx.try_send(root.to_bytes());
        }
    });
}

Some(root_bytes) = fetch_completion_rx.recv() => {
    if pin_intent.contains(&root_bytes) {
        // Bytes are now resident (fetch_recursive walked the full bundle
        // into the cache). The pin cascade is guaranteed to find them.
        let root = ContentId::from_bytes(root_bytes);
        let all = collect_descendants(runtime.storage_tier().cache(), root);
        for id in all {
            runtime.pin_content(id);
        }
    }
}
```

**Correctness argument (post-ZEB-159):**

The hook is architecturally correct and test-proven in isolation (given admitted bytes + seeded intent, repin fires), but its practical reach in the current client is gated by [ZEB-159](https://linear.app/zeblith/issue/ZEB-159). Today's `fetch_rx` arm returns fetched bytes to the Tauri caller without admitting them into `ContentStore` — so in production, `collect_descendants` walks an empty cache for the fetched CID and `runtime.pin_content` is a no-op. When ZEB-159 wires fetch success to cache admission, the correctness argument below holds end-to-end.

* Once ZEB-159 lands, `fetch_recursive` returning `Ok` implies every descendant is materialised in the cache. `collect_descendants` then sees a fully-populated tree — the ZEB-146 "cache empty → Pin is a no-op" failure mode becomes structurally absent.
* `runtime.pin_content(id)` is idempotent. Re-issuing on a fetch of an already-pinned root is a no-op, not a correctness hazard.
* `pin_intent` is a snapshot of sidecar truth, refreshed at `start_node` and kept in sync by the Pin/Unpin/Burn arms. No cross-thread synchronization needed — the event loop owns it exclusively.

**Why not hook on every cache admission?**
Admissions happen inside `runtime.tick()`, and there's no callback API today. A post-tick scan would be O(admitted)-per-tick, which is wasteful. Fetch completion is the only admission path that can resurrect a previously-intent-pinned root, and it's already a natural boundary in the event loop. Ingest admissions are by-construction `pinned: false` at creation time (user hasn't clicked pin yet); subscription-driven arrivals never hit the sidecar at all.

### Startup wiring

In `start_node`, after loading the `ContentIndex`:

```rust
let pin_intent: HashSet<[u8; 32]> = {
    let idx = content_index.lock().unwrap();
    idx.entries().filter(|e| e.pinned).map(|e| e.cid).collect()
};
```

Pass `pin_intent` into the event loop alongside the existing channels. The event loop owns it thereafter.

## Error handling

| Failure | Behavior | Rationale |
|---|---|---|
| Sidecar write fails during `pin_content` | `tracing::warn!`, return runtime's reply. Pin takes effect this session; intent not persisted. | Matches existing `set_archived` / `set_replication_tier` best-effort pattern. |
| Sidecar write fails during `unpin_content` | `tracing::warn!`, return runtime's reply. Unpin takes effect this session; intent still says pinned. User sees pin return on next restart; log trail exists. | Symmetric with pin failure. |
| `set_pinned` called on a CID not in sidecar | Return `false`, no-op. | Matches `set_archived`. Runtime-side Pin still runs (intent is sidecar-scoped). |
| Legacy sidecar (pre-ZEB-155) | Deserializes cleanly; all entries read `pinned: false` via `#[serde(default)]`. | No version bump, no data loss. |
| Fetch-complete hook on a CID with no sidecar entry | `pin_intent.contains(root)` returns false → hook does nothing. | Correct: no intent = no replay. |
| `pin_intent` drifts from sidecar (bug in a verb arm) | Rebuilt from disk on next `start_node`. | Sidecar stays authoritative for durability. |
| Cascade failure inside the fetch-complete hook | Existing cascade semantics: `runtime.pin_content` returns false per CID; we don't fail the fetch. | Same as ZEB-154's cascade. |

## Testing

### Unit tests (`content_index.rs`)

1. `set_pinned_flips_flag_and_reports_change` — mirrors `set_archived_flips_flag_and_reports_change`.
2. `set_pinned_missing_cid_returns_false`.
3. `save_persists_pin_mutations` — extend the existing `save_persists_mutations` round-trip to include a pin flip, assert it survives reload.
4. `legacy_sidecar_without_pinned_field_loads_as_unpinned` — craft a raw v1 JSON without the `pinned` key, verify it deserializes and entries read `pinned: false`.

### Unit tests (`lib.rs`)

1. `list_content_shows_pinned_when_only_intent_is_set` — sidecar has an entry with `pinned: true`; runtime `PinnedSet` is empty. Wire shows `pinned: true`.
2. `list_content_shows_pinned_when_only_runtime_effect_is_set` — sidecar has `pinned: false`; runtime `PinnedSet` contains the CID. Wire shows `pinned: true` (inverse direction).

### Integration test (`src-tauri/tests/content_index_integration.rs`)

1. `pin_intent_survives_restart` — ingest a file via the existing test harness, pin it, drop the node, reload the sidecar, verify the reloaded entry has `pinned: true` and `list_content` shows `pinned: true`.
2. `fetch_complete_repins_on_intent` — the full B-path test. Construct a sidecar with `pinned: true` on a root CID, simulate `fetch_recursive` landing bytes in the runtime cache, drive a `fetch` request through the event loop, verify the runtime `PinnedSet` now contains the root (and its descendants, if multi-chunk) without any user pin action.

Integration tests use `pub` items only — consistent with ZEB-154's integration-test pattern. Any helper that needs to be reached from the external test crate must be `pub`, not `pub(crate)`.

## Out of scope

* **Piecemeal descendant admission.** If a leaf shared between two bundles is admitted by a fetch of the non-intent bundle after an intent-bundle's fetch already completed, that leaf won't auto-pin. This is the rare-sharing case [ZEB-156](https://linear.app/zeblith/issue/ZEB-156) explicitly owns; ZEB-155 defers it by doc comment.
* **Durable byte storage (real archive tier).** ZEB-155 persists intent, not bytes. The "real archive tier" concern from ZEB-146's follow-ups gets its own client-persistence spec.
* **Eager startup re-pin into the runtime.** No-op today (cache empty); becomes meaningful only alongside durable byte storage, and is better designed there.
* **Per-community or per-peer pin policies.** Out of scope.
* **UI changes.** None required. `ContentItemWire.pinned` is already a bool; the frontend doesn't care about the source.
* **Mesh-side pin propagation** (telling peers we've pinned something). Not a ZEB-155 concern.

## Related

* [ZEB-146](https://linear.app/zeblith/issue/ZEB-146) — file manager backend that introduced the sidecar and the runtime-pin pattern this spec builds on.
* [ZEB-154](https://linear.app/zeblith/issue/ZEB-154) — chunked ingest + bundle-aware cascade verbs. Its `collect_descendants` and `ContentVerbRequest::Pin/Unpin/Burn` are reused verbatim by the replay hook.
* [ZEB-156](https://linear.app/zeblith/issue/ZEB-156) — root-pin-set model. ZEB-155's `pinned: bool` on the sidecar IS a root-pin-set for flat bundles; ZEB-156 layers on top without a data migration when sharing becomes real.
* [ZEB-157](https://linear.app/zeblith/issue/ZEB-157) — partial-ingest rollback (orphan leaf cleanup). Independent.
* [ZEB-158](https://linear.app/zeblith/issue/ZEB-158) — folder / directory support. Depends on ZEB-155 sequencing-wise: shipping folders that forget pins on restart would be a regression, and ZEB-155 closes that gap first.
