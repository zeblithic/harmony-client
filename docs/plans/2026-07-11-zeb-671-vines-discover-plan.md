# ZEB-671: Vines Discover = transitive follows — Implementation Plan

**Goal:** Replace the flat Discover feed with follow-graph discovery: published
signed follow lists, bounded 2°/3° BFS, degree chips, provenance paths, and a
Tune sheet — per the drawn model in `Harmony Vines Feed.dc.html`.

**Decision (Jake, 2026-07-11): public + opt-out.** Follow lists are published
as signed wire records by default; a `share_follows` toggle stops publishing
and retracts (empty-list LWW). Design pinned in the ZEB-671 Linear comment.

**Architecture:** New signed wire record `VineFollowListPayload` on
`harmony/vines/{owner}/follows` (ZEB-673 `vine_signing` pattern, domain
`harmony-vine-follows-v1`, strict-on-wire from day one — no legacy records
exist). Receive side caches lists in `VineFeedCache` (LWW by `updated_at`,
capped, persisted additively in `VineFeedDiskV1`, sigs never retained on
disk). A pure `vine_follow_graph` module computes degree 2/3 reachability +
shortest via-paths from **local** follows (degree 1 ground truth — never the
own published echo). Discover-source DTOs/events gain optional `degree`/`via`;
`vine-graph-updated` tells the frontend to refetch. Frontend renders chips +
provenance and the Tune sheet (2°/3° toggles, mute-a-follow, count,
Share-my-follows).

## Global Constraints

- Follow lists carry **addresses only** — local pet-names never go on the wire.
- Strict-on-wire for follow lists: unsigned / bad pubkey→address binding /
  topic owner ≠ payload owner → rejected **before any state effect**.
- Canonical bytes are length-prefixed (u32-LE len ‖ bytes per field; u64 = 8-byte
  LE; list = u32-LE count then per-item length-prefix). Domain prefix
  `harmony-vine-follows-v1`. Never pipe-separated, never `canonical_cbor_encode`.
- Signer-authority guard on every signing path: embedded `owner_address` must
  equal `vine_signing::signer_address(&identity)` or hard-error.
- Disk (`VineFeedDiskV1`) is additive via `#[serde(default)]`; signatures are
  NOT retained (verify-once-at-ingest, same posture as descriptors/reactions).
- Caps: `MAX_FOLLOWS_PER_LIST = 1000` (reject larger at ingest, truncate at
  publish), `MAX_FOLLOW_LISTS = 5000` (evict oldest `updated_at`), BFS depth ≤ 3,
  visited cap `MAX_BFS_VISITED = 50_000`.
- Polycentric constraint: no ranking beyond the user's own graph. Discover
  renders graph-reachable vines only.
- Never inline-await in `start_node` reachable paths — boot republish is a
  spawned task.
- Gates: per-task `scripts/test-select --context task` (paste round/bucket
  line), `npx tsc --noEmit` + `npx vitest run` for frontend tasks; final sweep
  = full `cargo nextest run --locked --workspace --all-targets --features
  test-fixtures` + clippy `--all-targets` + `cargo fmt --all -- --check`.
- One commit per task; trailers per session convention.

---

### Task 1: `vine_signing` follows record + payload struct

**Files:** Modify `src-tauri/src/vine_signing.rs`, `src-tauri/src/lib.rs`.

- `VineFollowListPayload` in lib.rs (camelCase serde like the other vine
  payloads): `owner_address: String`, `follows: Vec<String>`,
  `updated_at: u64`, `identity_pub: Option<String>` + `sig: Option<String>`
  (skip-if-None; strict verify at ingest).
- `follow_list_canonical_bytes(p)`: domain ‖ owner_address ‖ updated_at(u64-LE)
  ‖ u32-LE count ‖ each follow address length-prefixed.
- `sign_follow_list(private, &mut p)` / `verify_follow_list(p) -> Result<(), String>`
  via the existing `verify_signed` plumbing (`what = "follow list"`).
- Tests (mirror descriptor/reaction suites): sign→verify roundtrip, tamper
  sweep over owner/updated_at/follows (add, remove, reorder, mutate entry),
  empty-vs-one-empty-entry canonical distinction, adjacent-field shift,
  pubkey/address mismatch rejection, unsigned rejection, serde camelCase pin,
  legacy-free JSON parse (no identityPub/sig keys → parses, verify rejects).

**Gate:** `cargo nextest run -p harmony-app --features test-fixtures -E 'test(follow_list)'` + fmt/clippy touched. Commit.

### Task 2: publish path — follow/unfollow triggers + event-loop publisher + boot republish

**Files:** Modify `src-tauri/src/lib.rs`, `src-tauri/src/event_loop.rs`.

- `build_signed_follow_list(guard) -> Result<VineFollowListPayload, String>`:
  reads `follow_mgr.addresses()` (truncate to cap), `updated_at = now-secs`,
  signer-authority guard, sign with `owner_private_identity`
  ("identity unavailable: cannot sign follow list" when absent).
- Extend `FollowRequest` (currently no-op in the event loop) with
  `PublishFollowList { owner: String, payload: Vec<u8> }`. Follow/unfollow
  impls: after the existing followed_set update, build+sign and
  `try_send(PublishFollowList…)` (log on full). Keep `Follow`/`Unfollow`
  variants for graph recompute (Task 5).
- Event-loop `follow_rx` handler: `PublishFollowList` → `session.put` on
  `harmony/vines/{owner}/follows`, `tracing::error!` on failure.
- Boot republish: where `start_node` finishes vine wiring, spawn a task that
  builds+signs (via the same helper through the state mutex) and sends the
  publish request. Honors `share_follows` (Task 3; until then unconditional).
- Tests: echo-publish fixture (pattern from `vine_publish_signing_tests`):
  follow → published payload parses + `verify_follow_list` passes + key_expr
  is `harmony/vines/{node_addr}/follows`; unfollow republishes without the
  removed address; errs-without-identity; diverged-signer refusal.

**Gate:** `scripts/test-select --context task`. Commit.

### Task 3: `share_follows` opt-out setting

**Files:** Create `src-tauri/src/vine_settings.rs`; modify `src-tauri/src/lib.rs`.

- `VineSettings { share_follows: bool }` default `true`; atomic file
  `vine_settings.json` (temp+rename, version field — mirror `follows.rs`).
- IPC `get_vine_settings` / `set_vine_settings(share_follows: bool)` +
  `*_impl` seams, registered in the handler list.
- Semantics: disabling → publish **empty** follow list (retraction, newer
  `updated_at`); enabling → republish current list; follow/unfollow and boot
  republish skip publishing while disabled (followed_set/graph updates still
  happen — sharing is about the wire, not local state).
- Tests: default-true on missing file, round-trip persistence, disable
  publishes empty list via echo fixture, follow-while-disabled publishes
  nothing, re-enable publishes full list.

**Gate:** `scripts/test-select --context task`. Commit.

### Task 4: receive side — subscription, routing, cache ingest, disk

**Files:** Modify `src-tauri/src/event_loop.rs`, `src-tauri/src/vine_feed_cache.rs`.

- Subscription `harmony/vines/*/follows` (own key space comment, like
  tombstones); router branch `key_expr.ends_with("/follows")` inside the
  `harmony/vines/` arm, **before** the bare-descriptor fallthrough.
- `FollowListOutcome { Inserted, UpdatedNewer, IgnoredOlder, Rejected(String) }`.
- `VineFeedCache::on_follow_list_sample(key_expr, payload)`: parse →
  `verify_follow_list` → topic owner segment must equal `owner_address`
  (strip `harmony/vines/`, strip `/follows`, no extra segments) → per-list
  cap check → LWW upsert into `follow_lists: HashMap<String, FollowListEntry>`
  (`{ follows: Vec<String>, updated_at: u64 }`) → global cap eviction
  (oldest `updated_at`) → `save()`. Own echo (owner == self) is ingested like
  any record but degree-1 always comes from local follows (Task 5).
- Disk: `FollowListOnDisk { owner, follows, updated_at }` appended to
  `VineFeedDiskV1` with `#[serde(default)]`; load restores the map.
- Tests: unsigned/tampered/foreign-topic/deeper-key rejected (no state
  effect), LWW older-ignored + newer-replaces, per-list cap rejection,
  global cap eviction order, disk round-trip, router integration (sample →
  cache) in event_loop tests.

**Gate:** `scripts/test-select --context task`. Commit.

### Task 5: graph — BFS module, reach recompute, DTO/event annotation

**Files:** Create `src-tauri/src/vine_follow_graph.rs`; modify
`src-tauri/src/vine_feed_cache.rs`, `src-tauri/src/event_loop.rs`, `src-tauri/src/lib.rs`.

- `vine_follow_graph::compute_reach(me: &str, my_follows: &[String], lists:
  &HashMap<String, Vec<String>>, max_depth: u8, visited_cap: usize) ->
  HashMap<String, Reach>` where `Reach { degree: u8, via: Vec<String> }`.
  BFS from my follows (they are depth 1; excluded from output along with
  `me`); neighbors iterated in sorted order for deterministic shortest
  paths; `via` = full address chain starting at my follow (e.g. `[devin,
  ravi]` for a 3° vine by `ada` reached via devin→ravi… `via` excludes the
  creator themselves; degree = via.len() + 1).
- Cache holds `reach: HashMap<String, Reach>`; `recompute_reach(me,
  my_follows)` returns `bool` changed. Triggers: accepted follow-list sample,
  `FollowRequest::Follow/Unfollow` in the event loop, once after boot load.
  On change → emit `vine-graph-updated` (empty payload).
- Annotation: `VineVideoDto` + descriptor emit path gain
  `degree: Option<u8>` + `via: Option<Vec<String>>` (camelCase, skip-if-None)
  — populated for `source == Discover` creators found in `reach`;
  `list_descriptors()` reads the map at DTO-build time (no stored denorm).
- Tests: BFS unit suite (2°/3° assignment, me/1° exclusion, depth bound,
  visited cap, deterministic tie-break, diamond graph shortest path,
  empty-graph), recompute-change detection, DTO annotation joins, router
  emits `vine-graph-updated` only on real change.

**Gate:** `scripts/test-select --context task`. Commit.

### Task 6: frontend — chips, provenance, graph-only Discover, Tune sheet

**Files:** Modify `src/lib/vine-service.ts`, `src/lib/types.ts` (VineVideo),
`src/lib/components/VineFeed.svelte`, `src/lib/components/VineCard.svelte`;
tests in `src/lib/vine-service.test.ts`, `src/lib/components/__tests__/`.

- Types: `degree?: number`, `via?: string[]` on `VineDescriptorEvent` +
  `VineVideo`; wire through the event → VineVideo mapping and
  `list_vine_videos` hydration; listen for `vine-graph-updated` → refetch
  `list_vine_videos` and reconcile degree/via in place.
- Discover = graph-only: `discoverVines` renders only entries with a degree
  (2 or 3), filtered by Tune toggles; muted roots (via[0] ∈ muted) hidden.
  Followed feed unchanged.
- Degree chip on the card: `2nd` (green `rgba(70,107,76,.9)`) / `3rd` (amber
  `rgba(185,116,44,.85)`) per the drawing; provenance line: 2° → `{A} follows
  @{B}`; 3° → `{A} → @{B} → @{C}` — names resolved: my follow pet-name for
  hop 1, cached `creatorName` for deeper hops, `addr.slice(0,8)…` fallback.
- Tune sheet (from the drawing): ⚙ Tune pill on Discover → overlay with 2°/3°
  toggles, mute-a-follow list (my follows), live `Done · N vines in Discover`
  count, and the **Share my follows** toggle (wired to
  `get_vine_settings`/`set_vine_settings`). Tune prefs persist in
  localStorage; share_follows is backend state.
- Tests: service (graph-only filter, toggle/mute filtering, graph-updated
  refetch reconciliation), component (chip renders per degree, via line copy,
  Tune interactions, share toggle invokes IPC).

**Gate:** `npx tsc --noEmit` + `npx vitest run`. Commit.

### Task 7: full gates + PR

- `scripts/test-select --full` equivalent: full nextest sweep + clippy
  `--all-targets` + fmt + tsc + vitest.
- Self-review the branch diff; update this plan's amendment section if
  anything diverged.
- PR: title `ZEB-671: Vines Discover = transitive follows (signed follow
  lists, 2°/3° graph, degree chips, Tune sheet)`; body with design summary +
  test evidence; fire `@coderabbitai review` once at open.

---

## Post-plan amendments (pre-PR self-review)

1. **Session-monotonic `updated_at`** — wall seconds alone made two
   changes within one second LWW-equal (receivers ignore `<=`), so a
   follow immediately unfollowed would stay visible remotely.
   `NodeState::follow_list_clock` floors every signed list at
   `max(now, prev + 1)`; pinned by
   `rapid_changes_get_strictly_increasing_timestamps`.
2. **`DescriptorOutcome::Inserted { dto }` boxed** — the new
   `degree`/`via` fields pushed the variant over clippy's
   `large_enum_variant` threshold.
3. **Graph recompute trigger for local changes** moved out of the
   event-loop `FollowRequest::Follow/Unfollow` arm (which stays a no-op)
   into `refresh_vine_graph_inputs`, called by the follow/unfollow IPCs
   and the boot block — the IPC returns only after the cache reach map
   is fresh, so the frontend's follow-triggered refetch never races it.
4. **Mock vines carry `degree`/`via`** (one deliberately unconnected) so
   dev mode exercises the graph-only Discover, chips, and Tune sheet.
5. **Disk envelope key is `follow_lists`** (snake_case like its
   siblings); per-row keys stay camelCase.
