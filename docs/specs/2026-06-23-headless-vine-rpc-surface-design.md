# Headless Vine RPC Surface + Two-Node Harness Scenario — Design

**Goal:** Add vine operations to the headless `api` RPC surface so the ZEB-447 two-agent
harness can script a Vines flow, and add a two-node scenario that — because the harness runs
real engines over real Zenoh — *is* the live publish→feed→view→reshare round-trip.

**Why:** The headless `api` (`src-tauri/src/api/rpc.rs`, 64 curated RPCs) exposes
community/channel/DM/file/presence verbs but **no vine verbs**, so `e2e_two_node.rs` cannot
exercise Vines. The vine stack is built (real Zenoh+IPC, `vine_feed_cache.rs`) but its
cross-node descriptor-propagation leg has no deterministic test (the in-process round-trip in
`vine_content_roundtrip_integration.rs` calls `VineFeedCache::on_descriptor_sample` directly —
no transport). This closes that gap **headlessly and deterministically**, satisfying both the
follow-up "live two-engine Zenoh descriptor-propagation run" and the parent Vines-e2e live DoD.

Closes the headless-vine-RPC follow-up; the harness scenario also closes the live-two-engine
follow-up and the parent Vines-e2e live DoD. (ZEB IDs intentionally kept out of branch/commit/PR
per repo policy; recorded in Linear.)

---

## Architecture (verified against the code)

- **Headless RPC seam** (`api/rpc.rs`): each method = one `rpc!()` macro line + an `*Args`
  struct (`#[serde(rename_all = "camelCase")]`) + a line in the
  `registry_has_exactly_the_curated_v1_surface` gate test. Handlers receive `&Mutex<NodeState>`
  (via `node_state()`), an event `sink`, and `serde_json::Value` args; return
  `Result<serde_json::Value, RpcError>`.
- **Shared `*_impl` seam:** Tauri `#[command]` and the RPC twin call the *same*
  `*_impl(state: &Mutex<NodeState>, …)` (e.g. `send_dm_impl`). The vine commands are **not**
  seam-split yet — `publish_vine`/`list_vine_videos`/`mark_vine_viewed` take `tauri::State`
  directly — so this work extracts seams.
- **Always-on vine subscription:** node declares `harmony/vines/*` (+ `…/reactions/**`) at
  startup (`event_loop.rs:2559`), routed by `emit_frontend_event` (`:7051`) into
  `vine_feed_cache` + a `vine-received` sink emit. Headless nodes receive descriptors with no
  GUI. **This is the load-bearing fact that makes a headless cross-node test possible.**
- **Publish path:** build `VineDescriptorPayload` → JSON → `PublishRequest` on `publish_tx` →
  event loop → `session.put("harmony/vines/{addr}", bytes)`. (`publish_tx`, `ingest_tx`,
  `vine_feed_cache`, `node_addr` all live on `NodeState`.)
- **CAS ingest seam:** `streaming_ingest(reader, &ingest_tx, ChunkerConfig, None) -> (ContentId, u64)`
  mints a real CID from raw bytes headlessly.

### Types (serde camelCase — exact keys asserted in the test)
- `VineDescriptorPayload`: `id, creatorAddress, creatorName, createdAt, videoCid, title?,
  reshareOf?, originalCreatorAddress?, originalCreatorName?`
- `VineVideoDto`: `id, creatorAddress, creatorName, createdAt, videoCid, title?, reshareOf?,
  viewed, originalCreatorAddress?, originalCreatorName?`

---

## RPC surface (4 new curated methods)

Names mirror the **existing Tauri command names** for one-mental-model consistency with the rest
of the registry (so `list_vine_videos`, not the provisional `list_vine_feed`). `reshare_vine` is
new (the GUI composes reshare in TS via `publish_vine`; this is the headless convenience seam).

1. **`publish_vine`** — Args `{ title?: string, videoCid?: string, creatorName?: string }`.
   If `videoCid` is absent/empty, ingest a small synthetic payload via `streaming_ingest` to mint
   a real CID (so the harness needs no separate ingest RPC and the descriptor points at real,
   fetchable content). Publishes a non-reshare descriptor on `harmony/vines/<ownerAddr>`.
   Returns `{ vineId, videoCid }`. `creatorName` defaults to "".
2. **`list_vine_videos`** — Args `{}`. Returns a flat `[VineVideoDto]` (newest-first), mirroring
   the existing command (`VineVideoDto` has no `source` field; the followed/discover split is a
   frontend concern).
3. **`mark_vine_viewed`** — Args `{ vineId: string }`. Returns `{ viewed: bool }` — `true` on
   first mark, `false` on repeat (idempotent). Safe before the descriptor arrives.
4. **`reshare_vine`** — Args `{ vineId: string, creatorName?: string }`. Resolves the original
   descriptor from the local feed (`list_descriptors().find(id == vineId)`); errors if not
   present. Publishes a reshare on `harmony/vines/<resharerAddr>` carrying
   `reshareOf = vineId`, `originalCreatorAddress` / `originalCreatorName` from the original
   (preserving the original's own original-creator fields if it was itself a reshare). Returns
   `{ vineId, reshareOf }`. RPC-only (no Tauri command).

### Seam strategy (DRY, GUI contract unchanged)
- Extract a private `publish_vine_descriptor(state, descriptor) -> Result<(), String>` (the
  build-bytes + send-on-`publish_tx` core). Used by the existing GUI `publish_vine` command, the
  new `publish_vine` RPC seam, and `reshare_vine`.
- `list_vine_videos_impl(state)` and `mark_vine_viewed_impl(state, vine_id)`: the existing Tauri
  commands become thin wrappers delegating to these.
- `publish_vine_impl(state, args)` and `reshare_vine_impl(state, args)`: new RPC seams.
- The GUI `publish_vine(PublishVinePayload)` command keeps its exact signature/return (`()`),
  delegating its descriptor build to `publish_vine_descriptor`.

---

## Harness scenario — `e2e_two_node.rs`, `#![cfg(feature = "e2e")]`

`s_vines_publish_feed_view_reshare`: two minted, co-located nodes A & B (reuse
`two_minted_nodes("vines")`). Driver helpers added to `driver.rs`: `publish_vine`,
`list_vine_videos`, `mark_vine_viewed`, `reshare_vine`.

Zenoh `put` is **not retained** — a single publish before subscriber-match is lost. The barrier
is therefore **republish-until-seen** (mirrors S2's `poll_until` retry philosophy), tolerating
pre-match dropped puts without a fixed sleep:

1. **A→B publish.** Loop: A `publish_vine { title: "vine-<rand>", creatorName: "alice" }` (fresh
   id each attempt) until B's `list_vine_videos` shows a descriptor with that unique title,
   within a bounded total timeout. Assert that descriptor's `creatorAddress == A.ownerAddr`,
   `creatorName == "alice"`, `videoCid` non-empty, `viewed == false`. Capture its `vineId`.
2. **View round-trip.** B `mark_vine_viewed(vineId)` → `true`; again → `false`; B's feed entry
   now shows `viewed == true`.
3. **B→A reshare.** Loop: B `reshare_vine { vineId, creatorName: "bob" }` until A's
   `list_vine_videos` shows a descriptor with `reshareOf == vineId`, bounded. Assert
   `originalCreatorAddress == A.ownerAddr` and `originalCreatorName == "alice"`.

Both legs cross real Zenoh between two real engines — this is the live descriptor-propagation
round-trip (publish leg A→B, reshare-attribution leg B→A), as a green CI test rather than a
flake-prone fixed-sleep test.

### Why non-flaky
- No fixed sleeps; both legs poll-until-observed with a bounded deadline.
- Republish tolerates the one genuine race (pub before sub-match) instead of asserting a single
  put lands.
- Asserts on the descriptor's own camelCase keys (per the e2e-assertion-camelCase-keys rule).

---

## Out of scope / non-goals
- No frontend changes (GUI reshare keeps its TS composition path).
- No content-bytes cross-node fetch assertion (already covered by
  `creator_ingests_video_recipient_fetches_bytes`); the synthetic-ingest CID makes one *possible*
  later but the scenario asserts descriptor propagation, not byte transfer.
- No reactions RPC (separate surface; not in the publish→feed→view→reshare DoD).
- No cross-WAN run (co-located harness is sufficient for the deterministic descriptor leg; a true
  WAN GUI run remains the fleet-gated option on the parent ticket).

## Testing & gates
- Rust unit: `publish_vine_impl` auto-ingest mints a CID + publishes; `reshare_vine_impl`
  resolves attribution and errors on unknown id; curated-surface gate test lists the 4 new names.
- Harness: the scenario above, `--features e2e` (builds + spawns the real `harmony-app` binary).
- Gates: `cargo fmt --all -- --check`, `cargo clippy` (scoped per relink-cost rule), `cargo
  nextest` (scoped), `tsc`/`vitest` unaffected (no frontend change) but run for safety.
