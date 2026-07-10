# ZEB-612 Slice 3 — Files: observed-holders counter, quota IPC, de-mocking — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace every fabricated Files datum with real data (observed replica counts, real pinned budget), surface the CID, remove mock-only panel surfaces, and restyle the browser chrome per the Commons Files reference — spec `docs/specs/2026-07-09-zeb-612-commons-i-town-hall-vines-files-design.md` §4.

**Architecture:** A new Rust `ObservedHolders` map (cid → distinct announcing Zenoh sessions, staleness-pruned) fed from the existing `harmony/announce/*` subscription where `source_zid` is already in scope, kept fresh by a new client-driven 60 s re-announce tick over the content index's public entries; `replica_count` joins onto `ContentItemWire`. A `get_storage_budget` IPC exposes the (const) `StorageBudget`. The frontend drops its four fabricated fields and all mock-backed panel surfaces, and renders honest replication/quota/CID affordances.

**Tech Stack:** Rust (tauri command + tokio event loop), Svelte 5 runes, TypeScript, vitest + @testing-library/svelte, cargo-nextest.

## Ground-truth premise corrections (vs spec §4 wording — flag in PR body)

The spec's backend paragraph assumed announcement mechanics that don't exist. Verified 2026-07-09 (two parallel surveys + direct reads):

1. **Announcements carry no peer identity** — key `harmony/announce/{cid_hex}`, payload = 4-byte BE u32 size (`lib.rs:13307-13339`). But the subscription arm already receives `source_zid` (`event_loop.rs:3662`, used for hop-distance at `:3707`). **Correction:** holder identity = announcing session's zid. Two devices of one owner = two zids = two physical copies — correct for replica counting. Samples without source info are skipped (lower-bound posture).
2. **No re-announcement exists** — announce actions fire only on store/publish (`storage_tier.rs:782,884,1058,1108`); a TTL-pruned set would decay to empty. The spec's "staleness-pruned" presupposed refresh. **Correction:** the client re-announces its own announceable content every `REANNOUNCE_INTERVAL_MS` (60 s), TTL = 3× (180 s) — the `community_presence.rs` interval/TTL discipline. Same key/payload format (`parse_content_announcement` accepts it unchanged); old nodes interoperate untouched. Scale note: O(library) publishes per interval — documented ceiling, real hosting accounting is ZEB-669.
3. **Private content never announces** — `should_announce` (`storage_tier.rs:984`) gates by content class; production policy has `encrypted_durable_announce: false` (`lib.rs:26-35`, existence-leak prevention). The re-announce driver mirrors the gate via the index's `sensitivity == Public`. Consequence: private files honestly read ×1 (self) — true: nothing replicates private content yet (ZEB-669).
4. **`StorageBudget` is a pinned-content budget, not an overall quota** — `{ cache_capacity: 512 items, max_pinned_bytes: 50 MB }` (`storage_tier.rs:19-26`, enforced by eviction; config literal `lib.rs:9073-9077`). The frontend's 10 GB `quotaBytes` is fiction. **Correction:** QuotaBar shows real used-bytes with no invented denominator + a real pinned-used vs pinned-budget meter.

## Global Constraints

- **Honesty rule (spec §0/§8):** no fabricated data, no invented denominators. Copy verbatim: row chip `×{n} healthy` / `×{n} at risk`; detail box line 1 `×{n} · copies seen across your peers`, line 2 `Above the ×{target} target for {tier}.` / `Below the ×{target} target for {tier}.`; toolbar search placeholder `Search files or paste a CID…`; add-files button `⤓ Add files`.
- **Wire casing:** camelCase everywhere (`replicaCount`, `cacheCapacity`, `maxPinnedBytes`).
- **Tokens only** (style-token-guard budget-0 for all File surfaces; `FileActions.svelte` carries 1 pre-existing debt unit — leave it): healthy = `--accent`, at-risk = `--gov-clay`, soft box = `--primary-soft`/`--primary-border`, labels `--faint` uppercase, counts/CIDs `--font-mono`. Radii: pills 999px, buttons/inputs 5px, cards/banners 8px.
- **Tier targets (frontend only):** `tierTarget` map `file-utils.ts:13-19` — expendable 1 / light 2 / default 3 / high 5 / ultra 9. No Rust mirror exists or is added.
- **Rust gates (CLAUDE.md):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; targeted `cargo nextest run --locked --features test-fixtures -E '<filter>'` per task.
- **Frontend gates:** `npx tsc --noEmit` + targeted `npx vitest run <files>` per task; full `npx vitest run` in the final task.
- **Never hold a std Mutex lock across `.await`** (collect-then-publish in the re-announce arm).
- **One commit per task**, trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.
- Monotonic loop clock (`start.elapsed().as_millis() as u64`) for all holder timestamps — never wall-clock.
- Out of scope: CleanupView (mock recs are inert on real data — sidecarIds never match; existing TODO stands), StorageBuddyList/ShareList replacements (ZEB-669), delete verb (ZEB-670), transitive Discover (ZEB-671).

---

### Task 1: `ObservedHolders` module (Rust)

**Files:**
- Create: `src-tauri/src/observed_holders.rs`
- Modify: `src-tauri/src/lib.rs` (module declaration, next to `mod content_index;`)

**Interfaces:**
- Produces: `crate::observed_holders::{ObservedHolders, REANNOUNCE_INTERVAL_MS, HOLDER_STALE_MS}`; methods `new()`, `note(&mut self, cid_hex: &str, zid: &str, now_ms: u64)`, `peer_count(&self, cid_hex: &str) -> u32`, `sweep(&mut self, now_ms: u64, ttl_ms: u64)`. Tasks 2–3 consume all of these.

- [ ] **Step 1: Write the failing tests** (inside `observed_holders.rs`, `#[cfg(test)] mod tests`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_counts_distinct_zids() {
        let mut h = ObservedHolders::new();
        h.note("aa", "zid-1", 100);
        h.note("aa", "zid-2", 110);
        h.note("bb", "zid-1", 120);
        assert_eq!(h.peer_count("aa"), 2);
        assert_eq!(h.peer_count("bb"), 1);
    }

    #[test]
    fn note_same_zid_refreshes_without_double_count() {
        let mut h = ObservedHolders::new();
        h.note("aa", "zid-1", 100);
        h.note("aa", "zid-1", 500);
        assert_eq!(h.peer_count("aa"), 1);
        // refresh took: sweep with ttl that would evict the 100-stamp keeps it
        h.sweep(600, 200);
        assert_eq!(h.peer_count("aa"), 1);
    }

    #[test]
    fn peer_count_unknown_cid_is_zero() {
        assert_eq!(ObservedHolders::new().peer_count("nope"), 0);
    }

    #[test]
    fn sweep_evicts_stale_keeps_fresh() {
        let mut h = ObservedHolders::new();
        h.note("aa", "zid-old", 0);
        h.note("aa", "zid-new", 900);
        h.sweep(1000, 200); // cutoff: last_seen >= 800
        assert_eq!(h.peer_count("aa"), 1);
    }

    #[test]
    fn sweep_drops_cids_with_no_holders() {
        let mut h = ObservedHolders::new();
        h.note("aa", "zid-1", 0);
        h.sweep(10_000, 100);
        assert_eq!(h.peer_count("aa"), 0);
        assert!(h.inner.is_empty(), "empty cid entries must be dropped");
    }

    #[test]
    fn stale_ttl_is_three_reannounce_intervals() {
        // The presence-map discipline (community_presence.rs): TTL = 3× beacon
        // interval so two lost announcements don't evict a live holder.
        assert_eq!(HOLDER_STALE_MS, 3 * REANNOUNCE_INTERVAL_MS);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(/observed_holders/)'`
Expected: compile error — module doesn't exist yet (add `mod observed_holders;` + empty file first if you want a cleaner red, then missing types).

- [ ] **Step 3: Implement**

```rust
//! ZEB-612 S3: per-CID observed-holder tracking.
//!
//! Counts distinct Zenoh sessions (zids) seen announcing each CID on
//! `harmony/announce/{cid_hex}`. This is an OBSERVED LOWER BOUND on
//! replicas: announcements carry no owner identity, encrypted content
//! never announces (existence-leak policy), and observation starts at
//! loop boot. UI copy must say "copies seen across your peers".
//!
//! Freshness: this node re-announces its own announceable content every
//! `REANNOUNCE_INTERVAL_MS` (driver in event_loop.rs); entries older
//! than `HOLDER_STALE_MS` are dropped by `sweep`. Timestamps are the
//! event loop's monotonic ms (`start.elapsed()`), never wall-clock.

use std::collections::HashMap;

/// How often the event loop re-announces own announceable content (ms).
pub const REANNOUNCE_INTERVAL_MS: u64 = 60_000;
/// Holder entries older than this are pruned — 3 missed re-announces,
/// the `community_presence.rs` interval/TTL discipline.
pub const HOLDER_STALE_MS: u64 = 3 * REANNOUNCE_INTERVAL_MS;

/// cid_hex → (announcer zid → last_seen_ms).
#[derive(Debug, Default)]
pub struct ObservedHolders {
    inner: HashMap<String, HashMap<String, u64>>,
}

impl ObservedHolders {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an announcement of `cid_hex` by `zid` at `now_ms`. Callers
    /// must exclude the own session's zid — self is counted separately
    /// (deterministically) at read time.
    pub fn note(&mut self, cid_hex: &str, zid: &str, now_ms: u64) {
        self.inner
            .entry(cid_hex.to_string())
            .or_default()
            .insert(zid.to_string(), now_ms);
    }

    /// Distinct peer sessions seen announcing `cid_hex` (unswept entries).
    pub fn peer_count(&self, cid_hex: &str) -> u32 {
        self.inner.get(cid_hex).map_or(0, |m| m.len() as u32)
    }

    /// Drop entries not refreshed within `ttl_ms` of `now_ms`, then drop
    /// CIDs with no remaining holders (mirrors `CommunityPresenceMap::sweep`).
    pub fn sweep(&mut self, now_ms: u64, ttl_ms: u64) {
        for holders in self.inner.values_mut() {
            holders.retain(|_, last| now_ms.saturating_sub(*last) <= ttl_ms);
        }
        self.inner.retain(|_, holders| !holders.is_empty());
    }
}
```

Note: the `sweep_drops_cids_with_no_holders` test reads `h.inner` — keep `inner` private but the test module is in-file, so field access works.

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(/observed_holders/)'`
Expected: 6/6 PASS.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/observed_holders.rs src-tauri/src/lib.rs
git commit -m "ZEB-612 S3: ObservedHolders — per-CID distinct-announcer map with presence-style TTL sweep"
```

---

### Task 2: Event-loop wiring — note announcements, re-announce tick (Rust)

**Files:**
- Modify: `src-tauri/src/lib.rs` — `collect_reannouncements` helper + tests; `NodeState` gains `observed_holders`; `start_node` passes the two Arcs into `event_loop::run`
- Modify: `src-tauri/src/event_loop.rs` — two new `run()` params, Subscription-arm note, re-announce+sweep select arm

**Interfaces:**
- Consumes: Task 1's `ObservedHolders` API.
- Produces: `NodeState.observed_holders: Arc<Mutex<ObservedHolders>>` (read by Task 3's join); `pub fn collect_reannouncements(index: &ContentIndex) -> Vec<(String, Vec<u8>)>` in lib.rs.

**Plan-time verifications to do first (adjust mechanically if drifted):**
- `ContentIndex` iteration accessor — if no `entries()`-style public iterator exists, add `pub fn entries(&self) -> impl Iterator<Item = &ContentIndexEntry>` to `content_index.rs` (read-only, trivial).
- `start` (the loop's boot `Instant`) is in scope at the Subscription arm (`event_loop.rs:~3662`) — it is at `:3277`; if the arm sits in a nested closure without it, clone the existing `voice_now_ms` Arc instead.
- `Sensitivity` enum variant name for public content in `content_index.rs` (`Sensitivity::Public`).

- [ ] **Step 1: Write failing tests for `collect_reannouncements`** (lib.rs test module near the announcement parser tests at `:54040`)

```rust
#[test]
fn collect_reannouncements_public_only_deduped() {
    let mut index = ContentIndex::default(); // or the test constructor used nearby
    // three entries: public, private, archived-public; two sidecars share a CID
    index.insert(test_entry("pub-a", [0xAA; 32], Sensitivity::Public, false, 1024));
    index.insert(test_entry("pub-a2", [0xAA; 32], Sensitivity::Public, false, 1024)); // same CID
    index.insert(test_entry("priv", [0xBB; 32], Sensitivity::Private, false, 2048));
    index.insert(test_entry("arch", [0xCC; 32], Sensitivity::Public, true, 4096));

    let out = collect_reannouncements(&index);
    assert_eq!(out.len(), 1, "private + archived excluded, shared CID deduped");
    let (key, payload) = &out[0];
    assert_eq!(key, &format!("harmony/announce/{}", hex::encode([0xAA; 32])));
    assert_eq!(payload.as_slice(), &1024u32.to_be_bytes());
}

#[test]
fn collect_reannouncements_saturates_oversized() {
    let mut index = ContentIndex::default();
    index.insert(test_entry("big", [0xDD; 32], Sensitivity::Public, false, u64::MAX));
    let out = collect_reannouncements(&index);
    assert_eq!(out[0].1.as_slice(), &u32::MAX.to_be_bytes());
}
```

(`test_entry` = small local helper building a `ContentIndexEntry` with the given sidecar id, cid, sensitivity, archived flag, size; copy field defaults from the existing content_index tests at `content_index.rs:684-739`. Match the real `ContentIndex` insert/constructor API found in Step 0 verification.)

- [ ] **Step 2: Run to verify failure** — same nextest filter `-E 'test(/collect_reannouncements/)'`, expect missing-fn compile error.

- [ ] **Step 3: Implement `collect_reannouncements`** (lib.rs, near `parse_content_announcement`)

```rust
/// ZEB-612 S3: build re-announcement publishes for the announceable
/// subset of the content index — `Sensitivity::Public`, non-archived,
/// deduped by CID (symlink-style sidecars share CIDs). This mirrors
/// harmony-content's `should_announce` class gate: encrypted content
/// must not leak existence on the announce topic. Payload is the 4-byte
/// BE size `parse_content_announcement` pins; sizes over u32::MAX
/// saturate (the announce size is advisory).
pub fn collect_reannouncements(index: &ContentIndex) -> Vec<(String, Vec<u8>)> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for entry in index.entries() {
        if entry.sensitivity != Sensitivity::Public || entry.archived {
            continue;
        }
        if !seen.insert(entry.cid) {
            continue;
        }
        let size = u32::try_from(entry.size_bytes).unwrap_or(u32::MAX);
        out.push((
            format!("harmony/announce/{}", hex::encode(entry.cid)),
            size.to_be_bytes().to_vec(),
        ));
    }
    out
}
```

- [ ] **Step 4: Wire `NodeState` + `start_node` + `run()`**

1. `NodeState` (lib.rs, next to `content_index` at `:763`):
```rust
/// ZEB-612 S3: per-CID distinct announcing sessions, written by the
/// event loop, read by list_content/list_root for `replicaCount`.
pub observed_holders: std::sync::Arc<std::sync::Mutex<crate::observed_holders::ObservedHolders>>,
```
Initialize at every `NodeState` construction site with `Arc::new(Mutex::new(ObservedHolders::new()))` (grep `content_index:` initializers and mirror).

2. `event_loop::run()` — two params appended after `mail_sync` (signature already `#[allow(clippy::too_many_arguments)]`):
```rust
observed_holders: std::sync::Arc<std::sync::Mutex<crate::observed_holders::ObservedHolders>>,
content_index: std::sync::Arc<std::sync::Mutex<crate::content_index::ContentIndex>>,
```
Update the `start_node` call site (pass `state.observed_holders.clone()`, `state.content_index.clone()`) and any test invocations of `run(` (grep; thread fresh Arcs).

3. Subscription arm (event_loop.rs, after the pairing `continue`, before the `hop_distance` computation):
```rust
// ZEB-612 S3: record distinct announcing sessions per CID. Own
// announcements loop back on the local session — exclude own_zid so
// replica_count = 1 (self) + peers doesn't double-count. Samples
// without source info can't be attributed and are skipped (the count
// is an observed lower bound).
if key_expr.starts_with("harmony/announce/") {
    if let (Some(zid), Some(a)) = (
        source_zid.as_ref(),
        crate::parse_content_announcement(&key_expr, &payload),
    ) {
        if *zid != own_zid {
            let now = start.elapsed().as_millis() as u64;
            observed_holders.lock().unwrap().note(&a.cid, zid, now);
        }
    }
}
```

4. Re-announce + sweep tick — interval next to `voice_sweep_tick` (`:3281`):
```rust
// ZEB-612 S3: keep peer holder-maps fresh. No upstream re-announce
// exists (announces fire only on store/publish), so this node refreshes
// its own announceable content every REANNOUNCE_INTERVAL_MS; receivers
// sweep holders at 3× (HOLDER_STALE_MS). O(library) tiny publishes per
// interval — acceptable at current scale; real hosting accounting is
// ZEB-669.
let mut reannounce_tick = tokio::time::interval(Duration::from_millis(
    crate::observed_holders::REANNOUNCE_INTERVAL_MS,
));
reannounce_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
```
Select arm (with the other ticks; NEVER hold a lock across await):
```rust
_ = reannounce_tick.tick() => {
    let now = start.elapsed().as_millis() as u64;
    observed_holders
        .lock()
        .unwrap()
        .sweep(now, crate::observed_holders::HOLDER_STALE_MS);
    let announcements = {
        let idx = content_index.lock().unwrap();
        crate::collect_reannouncements(&idx)
    };
    for (key_expr, payload) in announcements {
        dispatch_action(
            RuntimeAction::Publish { key_expr, payload },
            &session, &zenoh_tx, &app, &closing, &own_zid,
        )
        .await;
    }
}
```
(Match `dispatch_action`'s real argument list at the existing call sites, e.g. `:3882`.)

- [ ] **Step 5: Run gates**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(/collect_reannouncements/) or test(/observed_holders/)'` → PASS; then `cargo fmt --all` + clippy (full `--all-targets` — the run() signature change touches tests).

- [ ] **Step 6: Commit**

```bash
git add -A src-tauri
git commit -m "ZEB-612 S3: wire ObservedHolders into the event loop — announce noting + 60s re-announce/sweep tick"
```

---

### Task 3: `replica_count` on `ContentItemWire` + join (Rust)

**Files:**
- Modify: `src-tauri/src/lib.rs` — wire struct (`:13347-13365`), `list_root` (`:13457-13488`), `list_content` folder branch (`:13425-13455`), serde pin test (`:54729`)

**Interfaces:**
- Consumes: `NodeState.observed_holders`, `ObservedHolders::peer_count`.
- Produces: `ContentItemWire.replica_count: u32` → JSON `replicaCount` (Task 5 consumes).

- [ ] **Step 1: Extend the serde pin test** (`content_item_wire_serializes_sidecar_id_and_kind`, `:54729`) — construct with `replica_count: 3` and assert the JSON contains `"replicaCount":3`. Add a unit test for the join helper:

```rust
#[test]
fn apply_replica_counts_is_one_plus_observed_peers() {
    let mut holders = crate::observed_holders::ObservedHolders::new();
    holders.note("aa", "zid-1", 0);
    holders.note("aa", "zid-2", 0);
    let mut items = vec![test_wire_item("aa"), test_wire_item("bb")];
    apply_replica_counts(&mut items, &holders);
    assert_eq!(items[0].replica_count, 3, "self + 2 seen peers");
    assert_eq!(items[1].replica_count, 1, "self only when nothing observed");
}
```
(`test_wire_item(cid)` = local helper building a `ContentItemWire` with placeholder fields, `replica_count: 1`.)

- [ ] **Step 2: Run to verify failure** — `-E 'test(/apply_replica_counts/) or test(=content_item_wire_serializes_sidecar_id_and_kind)'`, expect missing-field compile error.

- [ ] **Step 3: Implement**

1. Field on `ContentItemWire` (doc comment carries the honesty contract):
```rust
/// ZEB-612 S3: observed replica count — 1 (self) + distinct peer
/// sessions seen announcing this CID since boot (staleness-pruned).
/// A LOWER BOUND, not global truth: UI copy must say "copies seen".
pub replica_count: u32,
```
2. Both construction sites (`list_root` map at `:13467-13479`, `list_folder` at `:13544-13556`) initialize `replica_count: 1`.
3. Join helper + application:
```rust
/// Overwrite `replica_count` with 1 (self) + observed peer sessions.
fn apply_replica_counts(
    items: &mut [ContentItemWire],
    holders: &crate::observed_holders::ObservedHolders,
) {
    for item in items {
        item.replica_count = 1 + holders.peer_count(&item.cid);
    }
}
```
In `list_content` (covers both branches — root and folder), after items are built:
```rust
{
    let holders = state.observed_holders.lock().unwrap();
    apply_replica_counts(&mut items, &holders);
}
```
(`list_root` is also called directly — apply inside `list_root` itself and after the `list_folder` call in `list_content`, whichever matches control flow; do NOT double-apply. Follow the actual code shape: the join is idempotent-by-overwrite either way.)

- [ ] **Step 4: Run to verify pass** — same filter, plus `-E 'test(/list_folder/)'` sanity.

- [ ] **Step 5: fmt + clippy + commit**

```bash
git commit -am "ZEB-612 S3: ContentItemWire.replicaCount — 1 + observed distinct announcers join in list_content/list_root"
```

---

### Task 4: `get_storage_budget` IPC (Rust)

**Files:**
- Modify: `src-tauri/src/lib.rs` — const near the announcement types; command near `list_content`; replace the literal at `:9073-9077`; register in `generate_handler!` content block (`:53167-53185`)

**Interfaces:**
- Produces: IPC `get_storage_budget` → `{ "cacheCapacity": 512, "maxPinnedBytes": 50000000 }` (Task 5 consumes).

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn storage_budget_wire_camel_case() {
    let json = serde_json::to_string(&StorageBudgetWire::from(&NODE_STORAGE_BUDGET)).unwrap();
    assert!(json.contains("\"cacheCapacity\":512"), "{json}");
    assert!(json.contains("\"maxPinnedBytes\":50000000"), "{json}");
}
```

- [ ] **Step 2: Verify failure** — `-E 'test(=storage_budget_wire_camel_case)'`.

- [ ] **Step 3: Implement**

```rust
/// ZEB-612 S3: single source for the node's storage budget — also used
/// by NodeConfig in start_node. Hardcoded pending a settings surface.
pub const NODE_STORAGE_BUDGET: StorageBudget = StorageBudget {
    cache_capacity: 512,
    max_pinned_bytes: 50_000_000,
};

/// Wire shape for `get_storage_budget`. `maxPinnedBytes` is the PINNED
/// content budget the runtime enforces — not an overall storage quota
/// (none exists); the frontend must not render it as one.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageBudgetWire {
    pub cache_capacity: u64,
    pub max_pinned_bytes: u64,
}

impl From<&StorageBudget> for StorageBudgetWire {
    fn from(b: &StorageBudget) -> Self {
        Self {
            cache_capacity: b.cache_capacity as u64,
            max_pinned_bytes: b.max_pinned_bytes,
        }
    }
}

/// Query the node's storage budget (ZEB-612 S3). Const-backed — works
/// before node boot.
#[tauri::command]
async fn get_storage_budget() -> Result<StorageBudgetWire, String> {
    Ok(StorageBudgetWire::from(&NODE_STORAGE_BUDGET))
}
```
Replace `:9073-9077`'s literal with `storage_budget: NODE_STORAGE_BUDGET,` (if `StorageBudget` isn't const-constructible — non-const field types — fall back to a `fn node_storage_budget() -> StorageBudget` single source). Register `get_storage_budget` in `generate_handler!`.

- [ ] **Step 4: Verify pass**, then full-file gates: `cargo fmt --all -- --check`, clippy `--all-targets`, `-E 'test(=storage_budget_wire_camel_case)'`.

- [ ] **Step 5: Commit** — `"ZEB-612 S3: get_storage_budget IPC — expose the enforced pinned-content budget (const single-source)"`

---

### Task 5: Frontend service — real `replicaCount`, quota reshape (TS)

**Files:**
- Modify: `src/lib/file-manager-service.ts`, `src/lib/types.ts`, `src/lib/file-manager-service.test.ts`

**Interfaces:**
- Consumes: wire `replicaCount` (Task 3), `get_storage_budget` (Task 4).
- Produces: `ContentItem.replicaCount` real; `QuotaStatus { usedBytes, byCategory, pinnedUsedBytes, pinnedBudgetBytes: number | null }`; `FileManagerSettings` loses `quotaBytes`. Fabrications `stalenessScore`/`accessCount`/`lastAccessed` still present (removed with their renderers in Task 7 — keeps every commit green).

- [ ] **Step 1: Failing tests** (rewrite the pinned-quota block `file-manager-service.test.ts:8-9,55-58` + add):

```ts
it('maps wire replicaCount instead of fabricating 1', async () => {
  // list_content resolves one wire item with replicaCount: 4
  // expect getContents()[0].replicaCount === 4
});

it('fetches the pinned budget on connect and exposes it in quota status', async () => {
  // get_storage_budget mocked → { cacheCapacity: 512, maxPinnedBytes: 50_000_000 }
  // expect getQuotaStatus().pinnedBudgetBytes === 50_000_000
});

it('quota status has no invented total and computes pinned usage from pinned items', async () => {
  // two items (1000, 2000 bytes), only the first pinned →
  // usedBytes 3000, pinnedUsedBytes 1000; 'totalBytes' not a key
});

it('budget fetch failure degrades to null budget (used-only display)', async () => {
  // get_storage_budget rejects → pinnedBudgetBytes null, no throw from connectAdapter
});
```
Write them as real tests against the `createMockAdapter()` + `mockImplementation` idiom already used in this file (route on the command name; `list_content` → wire fixtures, `get_storage_budget` → budget or rejection).

- [ ] **Step 2: `npx vitest run src/lib/file-manager-service.test.ts`** — new tests FAIL (plus the old 10 GB pins now obsolete: delete them in the same edit).

- [ ] **Step 3: Implement**

- `types.ts`: `QuotaStatus` → `{ usedBytes: number; byCategory: Record<ContentCategory, number>; pinnedUsedBytes: number; pinnedBudgetBytes: number | null }`. `FileManagerSettings` drops `quotaBytes` (grep consumers; fix or delete). Add `StorageBudgetWire` TS type in file-manager-service.ts: `{ cacheCapacity: number; maxPinnedBytes: number }`.
- `ContentItemWire` (service-local type `:105-118`): add `replicaCount: number`.
- `wireToContentItem` `:153`: `replicaCount: wire.replicaCount` (leave the other three fabrications for Task 7). Same in the `ingest()` mapping (`:446`): fresh ingest is self-only → `replicaCount: 1` stays correct there ONLY if the ingest wire lacks the field; if `IngestResult` carries a wire item, map it; else keep literal 1 with a comment (fresh ingest = self only, honest).
- `connectAdapter`: after `list_content`, `try { const b = await adapter.invoke('get_storage_budget') as StorageBudgetWire; this.pinnedBudgetBytes = b.maxPinnedBytes; } catch { this.pinnedBudgetBytes = null; }` (private field, default null).
- `getQuotaStatus`: keep `usedBytes`/`byCategory` math; add `pinnedUsedBytes` (same distinct-CID walk, only `pinned` items); return `pinnedBudgetBytes: this.pinnedBudgetBytes`. Delete `totalBytes` and the `quotaBytes` setting read.
- `mock-file-data.ts`: mock items keep realistic varied `replicaCount` (demo mode).

- [ ] **Step 4: Verify** — `npx vitest run src/lib/file-manager-service.test.ts src/lib/components/__tests__/FileBrowser.integration.test.ts` (QuotaBar consumers may reference `totalBytes` — expect breakage handled in Task 9's QuotaBar reshape; if FileBrowser/QuotaBar compile-break now, do the minimal QuotaBar prop rename in THIS task to stay green and leave the visual reshape to Task 9). `npx tsc --noEmit` clean.

- [ ] **Step 5: Commit** — `"ZEB-612 S3: real replicaCount from wire + pinned-budget quota status (10 GB fiction removed)"`

---

### Task 6: `countByVideoCid` on VineService (TS)

**Files:**
- Modify: `src/lib/vine-service.ts`, `src/lib/vine-service.test.ts`

**Interfaces:**
- Produces: `vineService.countByVideoCid(cid: string): number` — distinct vines (both feeds) whose `videoCid === cid`. Task 8 consumes via App snippet.

- [ ] **Step 1: Failing tests** (mirror the `getReshareCount` tests):

```ts
describe('countByVideoCid (ZEB-612 S3 "Used by N vines")', () => {
  it('counts vines across followed and discover feeds sharing the cid', ...);
  it('returns 0 for an unreferenced cid', ...);
  it('counts a reshare and its original separately (both reference the blob)', ...);
});
```

- [ ] **Step 2: red** → **Step 3: implement** (pattern `vine-service.ts:414-419`):

```ts
/** ZEB-612 S3: how many vines (across both feeds) reference a video blob.
 *  Drives the Files detail panel's "Used by N vines" row — computed
 *  client-side from real descriptors; no backend involvement. */
countByVideoCid(videoCid: string): number {
  return this.followedVines.filter((v) => v.videoCid === videoCid).length
    + this.discoverVines.filter((v) => v.videoCid === videoCid).length;
}
```

- [ ] **Step 4: green** (`npx vitest run src/lib/vine-service.test.ts`) → **Step 5: commit** — `"ZEB-612 S3: VineService.countByVideoCid for the Files 'Used by N vines' row"`

---

### Task 7: Rows/list/grid — CID chip, replication chip, staleness/lastAccessed removal (TS/Svelte)

**Files:**
- Modify: `src/lib/file-utils.ts` (+test), `src/lib/types.ts`, `src/lib/file-manager-service.ts`, `src/lib/mock-file-data.ts`, `src/lib/components/FileRow.svelte`, `FileCard.svelte`, `FileList.svelte`, `FileMetadata.svelte`
- Delete: `src/lib/components/StalenessIndicator.svelte`
- Tests: `FileRow.test.ts`, `FileCard.test.ts`, `file-utils.test.ts`, plus assertion sweeps in `FileBrowser.test.ts`/`FileBrowser.integration.test.ts`/`FileDetailPanel.test.ts`

**Interfaces:**
- Consumes: real `replicaCount` (Task 5), `tierTarget` (existing).
- Produces: `shortCid(cid)` in file-utils; `ContentItem` loses `stalenessScore`/`accessCount`/`lastAccessed`; FileList columns Name / Size / Replication / Sensitivity.

- [ ] **Step 1: Failing tests**

`file-utils.test.ts`:
```ts
describe('shortCid', () => {
  it('truncates long hex cids to first-6…last-4', () =>
    expect(shortCid('3f9a2c81d4e5f60718293a4b5c6d7e8f')).toBe('3f9a2c…7e8f'));
  it('passes short cids through', () => expect(shortCid('abcdef123456')).toBe('abcdef123456'));
});
```
`FileRow.test.ts`: mono CID chip renders `cid:{shortCid}` (`data-testid="cid-chip"`); replication chip text `×3 healthy` when replicaCount ≥ tierTarget, `×1 at risk` below; staleness dot and Last-Accessed cell GONE (`queryBy…` null).
`FileCard.test.ts`: staleness dot gone.

- [ ] **Step 2: red** (`npx vitest run src/lib/file-utils.test.ts src/lib/components/__tests__/FileRow.test.ts src/lib/components/__tests__/FileCard.test.ts`)

- [ ] **Step 3: Implement**

- `file-utils.ts`:
```ts
/** ZEB-612 S3: `3f9a2c…7e8f` — compact hex-CID display for rows. */
export function shortCid(cid: string): string {
  return cid.length <= 12 ? cid : `${cid.slice(0, 6)}…${cid.slice(-4)}`;
}
```
- `types.ts` `ContentItem`: delete `stalenessScore`, `accessCount`, `lastAccessed` (comment: fabricated → removed with renderers, ZEB-612 S3; real signals return with real backends).
- `file-manager-service.ts`: drop the three fabrications in `wireToContentItem` + `ingest()`.
- `mock-file-data.ts`: strip the fields from mock items.
- `FileRow.svelte`: remove StalenessIndicator + lastAccessed cell. Add mono CID chip (`--font-mono`, `--faint`, 999px pill, `title={item.cid}`): `cid:{shortCid(item.cid)}`. Replication cell becomes the chip: dot (6px, `--accent` healthy / `--gov-clay` at-risk) + `×{item.replicaCount} {healthy ? 'healthy' : 'at risk'}` mono. `const healthy = $derived(item.replicaCount >= tierTarget(item.replicationTier))` (folders: keep whatever the row currently renders for folders — verify; folders have no tier semantics → render `—`).
- `FileList.svelte`: header columns → Name / Size / Replication / Sensitivity (keep uppercase-label styling `:109-114`); grid-template updated to match.
- `FileCard.svelte`: StalenessIndicator removed.
- `FileMetadata.svelte`: remove the Last-accessed and Access-count rows (origin row stays until Task 8).
- Delete `StalenessIndicator.svelte`; grep `StalenessIndicator` — expected remaining refs: none (FileDetailPanel's staleness bar reads `detail.stalenessScore` directly or via the component — remove that bar HERE if it blocks compile, else Task 8).
- Sweep test assertions referencing removed fields/columns across the File test files.

- [ ] **Step 4: green** — targeted files above + `npx tsc --noEmit`.

- [ ] **Step 5: Commit** — `"ZEB-612 S3: rows go honest — CID chip, ×N healthy/at-risk replication chip; staleness/access/lastAccessed fabrications deleted with their renderers"`

---

### Task 8: Detail panel — full CID + copy, honest replication box, mock-surface removal, Used-by-vines (TS/Svelte)

**Files:**
- Modify: `src/lib/components/FileDetailPanel.svelte`, `ReplicationStatus.svelte`, `FileMetadata.svelte`, `src/lib/file-manager-service.ts`, `src/lib/types.ts`, `src/lib/mock-file-data.ts`, `src/App.svelte` (snippets `:3767-3815`, derived state `:2784-2795`, NavPanel props `:3507-3513`), NavPanel buddy section (verify + remove)
- Delete: `src/lib/components/ShareList.svelte`, `StorageBuddyList.svelte` + their tests
- Tests: `FileDetailPanel.test.ts` rewrite, `file-manager-service.test.ts` detail tests

**Interfaces:**
- Consumes: `countByVideoCid` (Task 6), `shortCid` idiom (Task 7).
- Produces: `ContentDetail` = alias of `ContentItem`; `FileDetailPanel` props gain `usedByVines: number`; `getStorageBuddies` deleted.

- [ ] **Step 1: Failing tests** (`FileDetailPanel.test.ts` rewrite; the file's literal `ContentDetail` mock drops the removed fields):

```ts
it('renders the full CID in mono with word-break and a Copy CID button', ...);
  // getByTestId('cid-full') textContent === cid; button name 'Copy CID'
it('Copy CID writes the cid to the clipboard and flips to ✓ Copied', ...);
  // navigator.clipboard.writeText stubbed (vi.stubGlobal), fireEvent.click, await tick
it('replication box: healthy copy above target', ...);
  // replicaCount 5, tier default → '×5 · copies seen across your peers'
  // + 'Above the ×3 target for default.'
it('replication box: at-risk copy below target', ...);
  // replicaCount 1, tier high → 'Below the ×5 target for high.'
it('shows Used by N vines only when N > 0', ...);
it('mock surfaces are gone: no ShareList, no StorageBuddyList, no origin row, no staleness bar', ...);
```

- [ ] **Step 2: red** → **Step 3: Implement**

- `types.ts`: `export type ContentDetail = ContentItem; // buddies/sharedWith/origin removed (mock-only) → ZEB-669`.
- `file-manager-service.ts`: `getContentDetail` returns the found item as-is; delete `getStorageBuddies` + `storageBuddies` field + constructor seed; `mock-file-data.ts` deletes `mockStorageBuddies`/`mockPeers` (grep first — delete only if unreferenced after ShareList/StorageBuddyList removal).
- `FileDetailPanel.svelte`: new prop `usedByVines: number = 0`. Sections top-to-bottom: FileMetadata (origin row removed in FileMetadata.svelte) · SensitivityBadge · **CID box** (uppercase `--faint` label `CONTENT ID`; full cid `--font-mono` with `word-break: break-all` in an 8px card on `--primary-soft`; `Copy CID` button — `InviteLinkManager.svelte:41-60` idiom: await `navigator.clipboard.writeText`, `NotAllowedError` catch → inline error, `✓ Copied` state with cleared-on-unmount 2 s timer; 5px button radius) · **ReplicationStatus** · `Used by {usedByVines} vine{s}` mono row when > 0 · FileActions. ShareList/StorageBuddyList/staleness bar deleted.
- `ReplicationStatus.svelte`: box on `--primary-soft` + `--primary-border`, 8px. Line 1 mono: `×{replicaCount} · copies seen across your peers`. Line 2: `Above the ×{target} target for {tier}.` (`--accent`) / `Below the ×{target} target for {tier}.` (`--gov-clay`). Tier `<select>` stays (real IPC).
- `App.svelte`: `fileDetailPanel` snippet passes `usedByVines={vineService.countByVideoCid(selectedFileDetail.cid)}` (recompute via existing `fileManagerVersion`/vine `$state` mirrors — verify reactivity source at impl); delete `fileBuddies`/`availablePeers` deriveds and the NavPanel `storageBuddies` prop; remove NavPanel's buddy-section rendering (verify what it renders first — remove the mock contribution meter per spec).
- Delete `ShareList.svelte` + `StorageBuddyList.svelte` + their test files; grep for stragglers.

- [ ] **Step 4: green** — `npx vitest run src/lib/components/__tests__/FileDetailPanel.test.ts src/lib/file-manager-service.test.ts` + tsc + `npx vitest run` (App.svelte touched — broad blast radius).

- [ ] **Step 5: Commit** — `"ZEB-612 S3: detail panel goes honest — full CID + copy, 'copies seen' replication box, Used-by-vines; mock ShareList/StorageBuddyList/origin removed (→ ZEB-669)"`

---

### Task 9: QuotaBar reshape + browser chrome + CID search + full gates (TS/Svelte)

**Files:**
- Modify: `src/lib/components/QuotaBar.svelte`, `BrowserToolbar.svelte`, `QuickFilters.svelte`, `FileBrowser.svelte` (search predicate), `src/style-token-allowlist.json` (only if guard demands regen)
- Tests: `FileBrowser.test.ts` / `FileBrowser.integration.test.ts` additions

**Interfaces:**
- Consumes: `QuotaStatus` (Task 5).

- [ ] **Step 1: Failing tests**

```ts
it('quota shows real used bytes with no invented total', ...);
  // '3.0 KB stored locally'; no '/ 10 GB' anywhere
it('pinned meter renders used-of-budget when the budget is known', ...);
  // 'Pinned 1.0 KB of 50 MB'; hidden when pinnedBudgetBytes null
it('search matches CID paste', ...);
  // filter text = full or partial cid → row matches
it('toolbar copy: search placeholder + Add files verb', ...);
  // placeholder 'Search files or paste a CID…'; button '⤓ Add files'
```

- [ ] **Step 2: red** → **Step 3: Implement**

- `QuotaBar.svelte`: `{formatBytes(usedBytes)} stored locally` headline (mono value); when `pinnedBudgetBytes != null`: meter `Pinned {formatBytes(pinnedUsedBytes)} of {formatBytes(pinnedBudgetBytes)}` — track `--tally-track`, fill `--accent`, fill switches `--gov-clay` when ratio > 0.85; bar 999px. Keep `byCategory` breakdown if currently rendered (real data).
- `BrowserToolbar.svelte`: search `placeholder="Search files or paste a CID…"`; upload button label `⤓ Add files`. Keep list/grid toggle.
- `FileBrowser.svelte` `applyFiltersAndSort`: search predicate ORs `item.cid.toLowerCase().includes(q)` with the name match.
- `QuickFilters.svelte`: nav labels per VF — `All files / Videos / Images / Documents / Pinned by me` (map to existing category/pinned filters; underReplicated toggle keeps its current label — now truthful). Relabel only; no filter-model changes.
- Style pass: verify File-surface styles use tokens/radius idiom touched this slice; run the guard — if it flags, fix with tokens (do NOT add allowlist entries).

- [ ] **Step 4: FULL gates**

```bash
npx tsc --noEmit && npx vitest run
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --features test-fixtures -E 'test(/observed_holders/) or test(/collect_reannouncements/) or test(/apply_replica_counts/) or test(=storage_budget_wire_camel_case) or test(/content_item_wire/) or test(/list_folder/) or test(/content_announcement/)'
```
Expected: all green. (CI runs the full Rust workspace suite.)

- [ ] **Step 5: Commit** — `"ZEB-612 S3: honest quota bar (used + pinned budget), VF toolbar copy, CID search, storage-nav labels"`

---

## Post-plan checklist (before opening the PR)

- [ ] Self-review the diff for second-order breakage: does removing `quotaBytes` orphan any settings UI? Does `run()`'s new params break `#[cfg(test)]` invocations?
- [ ] PR body: lead with the two premise corrections (zid identity + client re-announce; pinned-budget quota) as "spec deviations, honesty-rule-governed" — Jake sees them at review, not buried.
- [ ] PR body honesty ledger (S3 rows): zid ≈ session not owner; freshness 60 s/180 s; private always ×1 self (true today); no overall quota exists — used-bytes shown denominator-free; re-announce O(library)/min scale ceiling → ZEB-669.
- [ ] Fire `@coderabbitai review` ONCE at PR-open. Never trigger Greptile. Scan all three comment buckets each round.
