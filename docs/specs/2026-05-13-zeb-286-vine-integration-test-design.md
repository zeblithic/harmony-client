# ZEB-286: VineFeedCache + two-node Vine integration tests

**Status:** Design approved (2026-05-13)
**Branch:** `zeb-286-vine-integration-test` (cut from `6ffe8b0` on `origin/main`)
**Linear:** [ZEB-286](https://linear.app/zeblith/issue/ZEB-286)
**Parent epic:** Harmony Client v1
**Forward refs:** [ZEB-147](https://linear.app/zeblith/issue/ZEB-147), [ZEB-209](https://linear.app/zeblith/issue/ZEB-209), [ZEB-103](https://linear.app/zeblith/issue/ZEB-103)

## 1. Goal

Verify and validate the actual network-based functionality of the Vine section
of the client by introducing the missing integration tests, and lay down the
Rust-side state surface (`VineFeedCache`) that production IPCs will query and
persistence will eventually plug into.

This is the first sub-deliverable of the "Vine network functionality" track
that the user prioritized after merging ZEB-284 (community moderation UX).

## 2. Background

### 2.1 Current Vine wiring (verified 2026-05-13)

| Capability | Wired? | Code |
|---|---|---|
| `publish_vine` IPC over `harmony/vines/{addr}` | ✓ | `src-tauri/src/lib.rs:4350` |
| `publish_vine_reaction` over `harmony/vines/{creator}/reactions/{vine_id}/{addr}` | ✓ | `src-tauri/src/lib.rs:4412` |
| Subscribe + emit `vine-received` (filtered by `followed_set`) | ✓ | `src-tauri/src/event_loop.rs:1060, 2742-2765` |
| Subscribe + emit `vine-reaction-received` | ✓ | `src-tauri/src/event_loop.rs:1075, 2742-2750` |
| `follow_vine_creator` / `unfollow_vine_creator` | ✓ | `src-tauri/src/lib.rs:4473, 4507` |
| `fetch_content` → blob URL for video playback | ✓ | `src/App.svelte:600` |
| `list_vine_videos()` (persistence/replay on reload) | ✗ **STUB** returns `Vec::new()` | `src-tauri/src/lib.rs:4467` |
| `mark_vine_viewed(id)` viewed-state | ✗ **STUB** no-op | `src-tauri/src/lib.rs:4553` |
| **No Vine integration test** | ✗ Gap (every other Harmony flow has one) | not previously ticketed |

The two stubs are tracked under [ZEB-147](https://linear.app/zeblith/issue/ZEB-147) (Vine persistence gap).

### 2.2 Codebase test pattern precedent

Three established two-node integration-test patterns in this repo:

- **Pure-CRDT / sync-engine** (`community_open_flow_integration.rs`, 729 LOC):
  two engines bridged via in-memory `tokio::mpsc` forwarders. No event loop,
  no real Zenoh.
- **Cache-on-sample** (`profile_broadcast_integration.rs`, 199 LOC): hand
  canonical wire bytes directly to a peer's `Cache::on_sample()`. Explicit
  comment: *"Full Zenoh end-to-end is too heavy for nextest."*
- **Full event-loop** (`content_index_integration.rs`, 859 LOC): spawn real
  `event_loop::run` on a dedicated OS thread, drive via channels.

The bulk of this design (lightweight tests) follows the **cache-on-sample**
pattern. The heavy fetch_content tests follow the **full event-loop** pattern.

### 2.3 Why a cache, not a refactor

We could test the receive path by extracting the inline dispatch logic at
`event_loop.rs:2742-2765` into a pure function. That covers wire format +
follow-set filtering, but leaves no Rust-side state — `list_vine_videos()`
remains a stub indefinitely, and ZEB-147 has to invent a cache later.

A cache is the natural seam. Adding it now means:
- The integration test can assert `cache.list_descriptors()` (real
  user-visible state), not just synthetic dispatch outcomes
- `list_vine_videos()` IPC can return real data immediately
- ZEB-147 reduces to "wire the cache to disk", not "design a cache and wire
  it to disk"

## 3. New module: `src-tauri/src/vine_feed_cache.rs`

### 3.1 Types

```rust
pub struct VineFeedCache {
    descriptors: HashMap<String, CachedVine>,
    reactions: HashMap<(String, String), CachedReaction>,
    viewed: HashSet<String>,
}

struct CachedVine {
    descriptor: VineDescriptorPayload,
    received_at_ms: u64,
    source: VineSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VineSource {
    Followed,
    Discover,
}

struct CachedReaction {
    liked: bool,
    timestamp: u64,
    reactor_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DescriptorOutcome {
    Inserted { dto: VineVideoDtoWithSource },
    AlreadyPresent,
    Rejected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionOutcome {
    Inserted,
    UpdatedNewer,
    Stale,
    Rejected,
}

pub struct ReactionSummary {
    pub count: usize,
    pub liked_by_me: bool,
}
```

`VineVideoDtoWithSource` extends the existing `VineVideoDto` with the
`source: VineSource` field that the frontend already consumes from the
`vine-received` event payload.

### 3.2 Public API

```rust
impl VineFeedCache {
    pub fn new() -> Self;

    /// Parse + insert a vine descriptor.
    ///
    /// Returns `None` if `key_expr` is not a vine-descriptor topic.
    /// Returns `Some(Rejected(reason))` for malformed payloads.
    /// Idempotent: re-arrival of the same vine_id returns `AlreadyPresent`.
    /// The `source` field is decided ONCE at first insert via the
    /// followed_set lookup; subsequent re-arrivals do not change it.
    pub fn on_descriptor_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
        followed_set: &HashSet<String>,
        now_ms: u64,
    ) -> Option<DescriptorOutcome>;

    /// Parse + insert/LWW-update a reaction.
    ///
    /// Returns `None` if `key_expr` is not a vine-reaction topic.
    /// LWW per (vine_id, reactor_addr) by `timestamp`. Stale samples
    /// (older timestamp than existing entry) return `Stale`.
    pub fn on_reaction_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
    ) -> Option<ReactionOutcome>;

    /// Return all cached descriptors as `VineVideoDto`, sorted by
    /// `created_at` DESC. Populates `viewed` from `self.viewed`.
    pub fn list_descriptors(&self) -> Vec<VineVideoDto>;

    /// Aggregate reaction state for `vine_id` from the viewing peer's
    /// perspective. `liked_by_me` checks whether the viewer_addr has
    /// a `liked=true` entry in the reactions map.
    pub fn get_reaction(
        &self,
        vine_id: &str,
        viewer_addr: &str,
    ) -> ReactionSummary;

    /// Mark a vine viewed by this local peer. Local-only in this PR —
    /// cross-device sync deferred to ZEB-147. Returns true if newly
    /// marked, false if already viewed.
    pub fn mark_viewed(&mut self, vine_id: String) -> bool;

    // Test helpers
    pub fn len_descriptors(&self) -> usize;
    pub fn len_reactions(&self) -> usize;
    pub fn is_viewed(&self, vine_id: &str) -> bool;
}
```

### 3.3 Wire format expectations

The cache parses the existing wire formats unchanged:

- **Descriptor topic:** `harmony/vines/{creator_address}`
- **Descriptor payload:** JSON-serialized `VineDescriptorPayload` (camelCase
  fields per `#[serde(rename_all = "camelCase")]`)
- **Reaction topic:** `harmony/vines/{creator_address}/reactions/{vine_id}/{reactor_address}`
- **Reaction payload:** JSON-serialized `VineReactionPayload` (camelCase)

No wire-format changes in this PR.

### 3.4 `viewed` semantics

`mark_viewed` writes to the local cache only. This PR does NOT publish
viewed-state to the network. Cross-device sync (so my phone marks a vine
viewed and my desktop reflects it) is explicitly deferred to ZEB-147.

The `viewed` HashSet can hold IDs for vines that haven't arrived yet
(`mark_viewed` is called before `on_descriptor_sample`). When the descriptor
later arrives, `list_descriptors` correctly reports `viewed=true` because the
join happens at query time, not insert time.

## 4. Production wiring changes

### 4.1 `NodeState`

```rust
pub struct NodeState {
    // ...existing fields...
    pub vine_feed_cache: Option<Arc<Mutex<VineFeedCache>>>,
}
```

- Constructed inside `start_node` alongside `follow_mgr`
- Cleared in `stop_node` (matches `follow_mgr` lifecycle)
- Uses `std::sync::Mutex` (matches NodeState pattern, all cache ops are quick
  with no awaits while holding the lock)

### 4.2 `event_loop::dispatch_sample`

The function gains one new parameter:

```rust
vine_feed_cache: &Arc<Mutex<VineFeedCache>>,
```

The `harmony/vines/` branch (currently lines 2742-2765) becomes:

```rust
} else if key_expr.starts_with("harmony/vines/") {
    if key_expr.contains("/reactions/") {
        let outcome = {
            let mut cache = vine_feed_cache.lock().unwrap();
            cache.on_reaction_sample(key_expr, payload)
        };
        if matches!(
            outcome,
            Some(ReactionOutcome::Inserted | ReactionOutcome::UpdatedNewer)
        ) {
            if let Ok(reaction) = serde_json::from_slice::<VineReactionPayload>(payload) {
                let _ = app.emit("vine-reaction-received", &reaction);
            }
        }
    } else {
        let outcome = {
            let mut cache = vine_feed_cache.lock().unwrap();
            let set = followed_set.lock().unwrap();
            cache.on_descriptor_sample(key_expr, payload, &set, now_ms())
        };
        if let Some(DescriptorOutcome::Inserted { dto }) = outcome {
            let _ = app.emit("vine-received", &dto);
        }
    }
}
```

Key changes from the current code:
- The cache becomes the authority for the `source` (followed vs discover)
  decision; the inline JSON-Value mutation that injected `source` is removed
- The emit only fires when the cache reports a NEW entry (idempotent
  re-arrival no longer re-emits — matches expected dedupe semantics)
- A re-arrival on a *stale* reaction is also not re-emitted

### 4.3 `list_vine_videos()` IPC

```rust
#[tauri::command]
fn list_vine_videos(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<VineVideoDto>, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let cache = guard
        .vine_feed_cache
        .as_ref()
        .ok_or_else(|| "not connected".to_string())?;
    Ok(cache.lock().unwrap().list_descriptors())
}
```

Signature changes:
- `fn() -> Vec<VineVideoDto>` → `fn(state) -> Result<Vec<VineVideoDto>, String>`
- The frontend `VineService.fetchInitialVines()` already handles error returns
  from this IPC (catches and falls back to mock seed)

### 4.4 `mark_vine_viewed(id)` IPC

```rust
#[tauri::command]
fn mark_vine_viewed(
    vine_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let cache = guard
        .vine_feed_cache
        .as_ref()
        .ok_or_else(|| "not connected".to_string())?;
    Ok(cache.lock().unwrap().mark_viewed(vine_id))
}
```

Return semantics: `true` = newly marked viewed, `false` = already viewed
(matches `follow_vine_creator`'s "did this change anything" convention).

### 4.5 Frontend touchpoints (minimal)

- `vine-service.ts` already calls `list_vine_videos` on connect and re-fetches
  on `vine-received` — no change needed
- The `source` field on `vine-received` event payload is already consumed by
  VineService — no change needed (cache continues to emit it)
- VineService mock-clear policy (ZEB-209) NOT touched in this PR — mock seeds
  still overlay on real cache data until ZEB-209

## 5. Tests

### 5.1 `src-tauri/tests/vine_feed_cache_integration.rs` (lightweight)

Modeled on `profile_broadcast_integration.rs`. Each test hands canonical wire
bytes (built via the same `serde_json::to_vec` shape that `publish_vine`
emits in production) directly to the recipient peer's `VineFeedCache`. Bytes
"as if Zenoh had transported them."

**Category 1 — Descriptor publish/receive + follow-set filtering (4 tests):**

1. `descriptor_from_followed_creator_lands_in_followed_bucket` — A publishes,
   B's cache (with A in followed_set) on_descriptor_sample, list_descriptors
   returns vine with source=Followed
2. `descriptor_from_unfollowed_creator_lands_in_discover_bucket` — same but
   A not followed → source=Discover
3. `re_arrival_of_same_descriptor_is_idempotent` — second on_descriptor_sample
   of same vine_id returns AlreadyPresent, no duplicate
4. `descriptor_with_malformed_payload_is_rejected` — invalid JSON bytes →
   returns Rejected with reason string, cache state unchanged

**Category 2 — Reaction publish/receive + LWW aggregation (4 tests):**

1. `two_reactors_like_same_vine_count_is_two` — both reactors fire liked=true,
   get_reaction(vine_id, anyone) returns count=2
2. `same_reactor_unlikes_then_likes_lww_wins` — same reactor publishes
   liked=false (t=100), then liked=true (t=200) → count=1 for that vine
3. `stale_reaction_does_not_overwrite_newer` — reactor publishes liked=true
   (t=200), then a delayed liked=false (t=100) arrives → newer wins, count
   stays at 1, outcome is Stale
4. `liked_by_me_reflects_viewer_addr` — viewer A is one of the reactors →
   get_reaction(vine_id, A).liked_by_me == true; viewer B is not a reactor
   → false

**Category 3 — Reshare wire path (2 tests):**

1. `reshare_descriptor_carries_reshare_of_link` — B reshares A's vine
   (publishes new descriptor with reshare_of = Some(A_vine_id)), C receives
   both via two on_descriptor_sample calls → cache exposes both vines, B's
   exposes reshare_of pointing to A's vine_id
2. `reshare_of_unknown_vine_id_still_accepted` — recipient hasn't seen the
   original; reshare descriptor still lands (no FK constraint); cache exposes
   the reshare with the dangling reshare_of pointer

**Category 4 — Viewed-state (2 tests):**

1. `mark_viewed_idempotent_and_local_only` — mark_viewed("v1") returns true;
   second call returns false; list_descriptors() shows viewed=true;
   cache.len_descriptors unchanged
2. `viewed_state_survives_descriptor_insertion_order` — mark_viewed("v1")
   BEFORE descriptor arrives; descriptor then arrives → list shows
   viewed=true even though it was marked before insert

**Category 5 — Wire format pinning (2 tests):**

1. `descriptor_canonical_json_pinned` — assert exact byte sequence for a
   canonical `VineDescriptorPayload`. camelCase fields (`videoCid`,
   `reshareOf`, `creatorAddress`, `creatorName`, `createdAt`).
2. `reaction_canonical_json_pinned` — assert exact byte sequence for a
   canonical `VineReactionPayload`. camelCase fields (`vineId`,
   `reactorAddress`, `reactorName`).

### 5.2 `src-tauri/tests/vine_content_roundtrip_integration.rs` (heavy)

Modeled on `content_index_integration.rs`. Spawns real `event_loop::run` on a
dedicated OS thread (NodeRuntime is `!Send`), drives via channels. NO real
Zenoh — content is round-tripped through the same NodeRuntime that ingested
it (i.e., self-fetch). The point is to exercise the production `fetch_content`
+ `ingest_content` plumbing for vine-sized payloads.

**3 tests:**

1. `creator_ingests_video_recipient_fetches_bytes` — creator's NodeRuntime
   ingests video bytes via `ingest_content` → produces video_cid. Same
   NodeRuntime calls `fetch_content(cid)` → gets the same bytes back. This
   verifies the production CAS round-trip works for a vine-sized payload,
   independent of the descriptor flow.
2. `fetch_content_for_unknown_cid_returns_err` — call `fetch_content` for a
   cid that has never been ingested → bounded-timeout `Err`. Surfaces what
   the frontend sees in the "no peer has this video" case.
3. `descriptor_arrives_before_video_cid_resolves_fetch_content_retry` —
   simulate the production-realistic ordering where `vine-received` fires
   before the content sample arrives. Asserts the bounded-retry behavior of
   `fetch_content`.

The "two-node" framing here is conceptual rather than process-level: we use
two `VineFeedCache` instances bridged in-memory to simulate the two peers,
plus a single NodeRuntime acting as the CAS. The codebase has no precedent
for spinning up two `NodeRuntime`s in a single test, and the value-add of
doing so would be marginal — the CAS round-trip is what matters, not the
Zenoh delivery.

### 5.3 Test count summary

- File A (cache integration): 14 tests
- File B (content roundtrip): 3 tests
- Total new tests: 17

## 6. Acceptance criteria

1. `src-tauri/src/vine_feed_cache.rs` exists with the public API in §3.2
2. `NodeState.vine_feed_cache` is constructed on `start_node`, cleared on
   `stop_node`
3. `event_loop::dispatch_sample` routes vine descriptors and reactions
   through the cache before emitting `vine-received` / `vine-reaction-received`
4. `list_vine_videos()` IPC returns real cache contents (no longer
   `Vec::new()`)
5. `mark_vine_viewed(id)` IPC actually marks state (no longer no-op)
6. Both integration test files exist, all 17 tests pass
7. ≥ 2 wire-format pinning tests (drift detector for Vine canonical JSON,
   mirroring community/library/profile precedent)
8. All 5 CI gates green: `cargo fmt`, `cargo clippy --features test-fixtures`,
   `cargo nextest --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`
9. No frontend visual regressions: existing vine UI still works against the
   real cache (mock seeds still overlay until ZEB-209)

## 7. Out of scope (explicit non-goals)

- **Disk persistence of the cache** → [ZEB-147](https://linear.app/zeblith/issue/ZEB-147). The cache is in-memory only; reload empties it. The architectural seam is what this PR delivers; the disk wiring is the next ticket.
- **VineService mock-clear policy** → [ZEB-209](https://linear.app/zeblith/issue/ZEB-209). Mock Alice/Bob seeds in VineService still overlay on cache data.
- **Reshare UX layer** → [ZEB-103](https://linear.app/zeblith/issue/ZEB-103). The wire path is tested; attribution display, counts, and confirmation dialog are separate.
- **Real Zenoh transport in tests** — codebase-wide convention (per `profile_broadcast_integration.rs`). Wire-format pinning + in-memory dispatch covers what real-Zenoh tests would.
- **Cross-device viewed-state sync** — `mark_viewed` is local-only in this PR. Sync deferred to ZEB-147 or a new ticket.
- **Two-NodeRuntime test scaffolding** — codebase has no precedent; the test plan uses one NodeRuntime + two cache instances to model two peers.
- **CBOR wire format** — Vine wire format is JSON (legacy). Migration to CBOR is not in scope; the pinning tests pin JSON canonical bytes.
- **Vine viewed-state UI** — `viewed: bool` already exists on `VineVideoDto`; this PR ensures the IPC returns it correctly. No new UI surface.
- **Adversary / malicious-publisher hardening** — out of scope. The malformed-payload test is for accidental corruption, not signed-payload attacks. A future ticket could add adversarial coverage (e.g., spoofed `creator_address` in topic vs payload).

## 8. Implementation phasing

Single-PR scope. Suggested task breakdown for the implementation plan:

- **Task 0**: Pre-flight verification (all 5 gates green on the cut branch, no
  drift baseline)
- **Task 1**: `vine_feed_cache.rs` module + types + on_descriptor_sample +
  list_descriptors + cache-level unit tests (5-7 tests)
- **Task 2**: on_reaction_sample + get_reaction + LWW + cache-level unit tests
  (4-5 tests)
- **Task 3**: mark_viewed + viewed-state cache-level unit tests (2-3 tests)
- **Task 4**: Wire into `NodeState` + `start_node` / `stop_node` lifecycle
- **Task 5**: Wire into `event_loop::dispatch_sample`; remove inline
  source-tag injection
- **Task 6**: Wire `list_vine_videos()` + `mark_vine_viewed()` IPCs
- **Task 7**: `vine_feed_cache_integration.rs` test file (14 tests)
- **Task 8**: `vine_content_roundtrip_integration.rs` test file (3 tests)
- **Task 9**: Final verification + push + PR

## 9. Risks

- **Frontend reactive-update race**: VineService listens to `vine-received`
  and ALSO calls `list_vine_videos` on connect. With the cache in place, the
  event handler will deliver the same vines that `list_vine_videos` returns,
  which could cause duplicates in the frontend. Mitigation: VineService
  already dedupes by `vine.id`, so this should be safe — but verify in the
  TypeScript unit tests during implementation.

- **NodeState lock-order**: Cache requires `Arc<Mutex<VineFeedCache>>`. The
  dispatch path must clone the Arc out of NodeState before acquiring the
  cache lock, never hold both simultaneously. Plan task 5 calls this out
  explicitly.

- **`now_ms` parameterization for tests**: `on_descriptor_sample` takes
  `now_ms` to make `received_at_ms` deterministic. Production code passes
  `SystemTime::now()` derived value; tests pass fixed values. Sole purpose
  is test determinism.
