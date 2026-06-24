# Headless Vine RPC Surface — Implementation Plan

> Execute task-by-task, TDD, commit per task. Spec:
> `docs/specs/2026-06-23-headless-vine-rpc-surface-design.md`.

**Goal:** 4 headless vine RPCs (`publish_vine`, `list_vine_videos`, `mark_vine_viewed`,
`reshare_vine`) on the shared `*_impl` seam + a real-transport two-node harness scenario.

**Tech:** Rust (`src-tauri/src/lib.rs`, `src-tauri/src/api/rpc.rs`), e2e-harness crate.

**Gates (relink-cost-scoped):**
- Rust unit: `cargo nextest run -p harmony-app --lib` (+ targeted `--test` for any integration).
- `cargo clippy -p harmony-app --lib --no-deps -- -D warnings`; full `--all-targets` clippy +
  `cargo fmt --all -- --check` in the final sweep.
- Harness: `cargo build --bin harmony-app` then `cargo test -p e2e-harness --features e2e
  s_vines_publish_feed_view_reshare` (the e2e scenario; heavy — final verification).
- Frontend untouched but run `npx tsc --noEmit` + `npx vitest run` once at the end.

---

### Task 1: Seam-split `list_vine_videos` + `mark_vine_viewed`

**Files:** Modify `src-tauri/src/lib.rs` (the two `#[tauri::command]`s ~`:11875`, `:11978`).

- [ ] Add `pub(crate) fn list_vine_videos_impl(state: &std::sync::Mutex<NodeState>) ->
  Result<Vec<VineVideoDto>, String>` containing the current body (snapshot `vine_feed_cache`
  under the std lock, `cache.lock().list_descriptors()`). The `#[tauri::command]
  list_vine_videos(state)` becomes `list_vine_videos_impl(state.inner())`.
- [ ] Add `pub(crate) fn mark_vine_viewed_impl(state: &std::sync::Mutex<NodeState>, vine_id:
  String) -> Result<bool, String>`; command delegates.
- [ ] Verify: `cargo nextest run -p harmony-app --lib` (existing vine tests still green; pure
  refactor). Commit: `refactor(vines): extract list/mark-viewed impl seams`.

### Task 2: Extract `publish_vine_descriptor` helper

**Files:** Modify `src-tauri/src/lib.rs` (`publish_vine` `~:11749`).

- [ ] Add `pub(crate) async fn publish_vine_descriptor(state: &std::sync::Mutex<NodeState>,
  descriptor: VineDescriptorPayload) -> Result<(), String>`: snapshot `publish_tx` + `node_addr`
  under the std lock (drop before await), serialize the descriptor to JSON, send `PublishRequest
  { key_expr: format!("harmony/vines/{}", descriptor.creator_address), payload, reply }`, await
  the ack. (Lift the existing send block out of the command.)
- [ ] Refactor the GUI `publish_vine` command to build its `VineDescriptorPayload` as today, then
  call `publish_vine_descriptor`. Signature/return (`Result<(), String>`) unchanged.
- [ ] Verify `cargo nextest run -p harmony-app --lib`. Commit:
  `refactor(vines): extract publish_vine_descriptor core`.

### Task 3: `publish_vine_impl` (RPC seam, auto-ingest)

**Files:** Modify `src-tauri/src/lib.rs`; add a unit test (e.g. in the lib `#[cfg(test)]` or a
`src-tauri/tests/` integration if `publish_tx`/`ingest_tx` need a spawned loop — prefer reusing
the `vine_content_roundtrip_integration.rs` harness helpers `make_node_config`/`spawn_event_loop`).

- [ ] Define `#[derive(Deserialize)] #[serde(rename_all="camelCase")] struct PublishVineArgs {
  #[serde(default)] title: Option<String>, #[serde(default)] video_cid: Option<String>,
  #[serde(default)] creator_name: Option<String> }` and `#[derive(Serialize)]
  #[serde(rename_all="camelCase")] struct PublishVineResult { vine_id: String, video_cid: String }`.
- [ ] `pub(crate) async fn publish_vine_impl(state, args: PublishVineArgs) ->
  Result<PublishVineResult, String>`: resolve `video_cid` — if `None`/empty, snapshot `ingest_tx`
  from state and `streaming_ingest(&b"harmony-e2e-vine-synthetic-bytes"[..], &ingest_tx,
  ChunkerConfig::default(), None).await` → hex CID. Build a `VineDescriptorPayload` (mirror the
  command: `id = format!("vine-{}-{}-{:08x}", &addr[..8], now_secs, rand::random::<u32>())`,
  `creator_address = node_addr`, `creator_name = args.creator_name.unwrap_or_default()`,
  `created_at = now`, `title`, no reshare fields). Call `publish_vine_descriptor`. Return
  `{ vine_id, video_cid }`.
- [ ] Test (integration, spawned loop): `publish_vine_impl` with `video_cid: None` returns a
  64-hex `video_cid` + a `vine-…` id, and the bytes are retrievable via `ReadBytes` (CID really
  ingested). Verify scoped `--test`.
- [ ] Commit: `feat(vines): headless publish_vine impl with synthetic ingest`.

### Task 4: `reshare_vine_impl`

**Files:** Modify `src-tauri/src/lib.rs`; unit/integration test.

- [ ] `struct ReshareVineArgs { vine_id: String, #[serde(default)] creator_name: Option<String> }`,
  `struct ReshareVineResult { vine_id: String, reshare_of: String }` (camelCase).
- [ ] `pub(crate) async fn reshare_vine_impl(state, args) -> Result<ReshareVineResult, String>`:
  `list_vine_videos_impl(state)?.into_iter().find(|d| d.id == args.vine_id)` → err
  `"reshare_vine: unknown vine_id"` if absent. Build reshare descriptor: fresh id,
  `creator_address = node_addr`, `creator_name = args.creator_name.unwrap_or_default()`,
  `video_cid` = original's, `title` = original's, `reshare_of = Some(args.vine_id)`,
  `original_creator_address = original.original_creator_address.or(Some(original.creator_address))`,
  `original_creator_name = original.original_creator_name.or(Some(original.creator_name))`
  (preserve attribution if the original was itself a reshare). Call `publish_vine_descriptor`.
- [ ] Test: seed a descriptor via `on_descriptor_sample`/publish, `reshare_vine_impl` produces a
  descriptor with `reshare_of` + `original_creator_*` set; unknown id → Err.
- [ ] Commit: `feat(vines): headless reshare_vine impl with attribution`.

### Task 5: Register the 4 RPCs + curated-surface gate

**Files:** Modify `src-tauri/src/api/rpc.rs` (`build_registry` + the `*Args` structs region +
`registry_has_exactly_the_curated_v1_surface` expected list).

- [ ] If `PublishVineArgs`/`ReshareVineArgs` live in `lib.rs`, reference them via `crate::`; else
  define thin RPC arg structs in `rpc.rs` mirroring them (match the existing file's convention).
- [ ] `rpc!(m, "publish_vine", crate::PublishVineArgs, |state, _sink, a| async move {
  crate::publish_vine_impl(state, a).await });` and likewise `list_vine_videos` (no-args struct →
  `crate::list_vine_videos_impl(state)`, wrap sync in `async move { … }`), `mark_vine_viewed`
  (`{ vineId }` → `crate::mark_vine_viewed_impl(state, a.vine_id)`), `reshare_vine`.
- [ ] Add the 4 names to the curated-surface expected list (keep it sorted/grouped as the file
  does). Add a "Vines" comment group.
- [ ] Verify `cargo nextest run -p harmony-app --lib` (the gate test now passes with 68 methods).
  Commit: `feat(api): register vine RPCs on the headless surface`.

### Task 6: Driver helpers

**Files:** Modify `e2e-harness/src/driver.rs`.

- [ ] `publish_vine(node, title, creator_name) -> Result<(String /*vineId*/, String /*videoCid*/)>`
  (post `publish_vine`, read `vineId`/`videoCid`). `list_vine_videos(node) -> Result<Vec<Value>>`.
  `mark_vine_viewed(node, vine_id) -> Result<bool>`. `reshare_vine(node, vine_id, creator_name) ->
  Result<String /*reshareOf*/>`. Mirror existing helpers' `json!` + extraction style.
- [ ] `cargo check -p e2e-harness` (driver is non-`e2e`-gated lib code). Commit:
  `test(e2e): vine driver helpers`.

### Task 7: Two-node scenario

**Files:** Modify `e2e-harness/tests/e2e_two_node.rs`.

- [ ] Add `#[tokio::test(flavor="multi_thread", worker_threads=4)] async fn
  s_vines_publish_feed_view_reshare()` per the spec: `two_minted_nodes("vines")`, unique title,
  republish-until-seen for A→B (bounded ~60s), assert descriptor camelCase keys, mark-viewed
  round-trip, republish-until-seen reshare for B→A asserting `reshareOf` + `originalCreator*`.
  `run.mark_success()`.
- [ ] Verify: `cd src-tauri && cargo build --bin harmony-app` then `cargo test -p e2e-harness
  --features e2e s_vines_publish_feed_view_reshare -- --nocapture`. (Heavy; background with a
  wall-clock safety net per the long-running-supervision rule.)
- [ ] Commit: `test(e2e): two-node vines publish/feed/view/reshare scenario`.

### Task 8: Final gate sweep

- [ ] `cargo fmt --all -- --check`; full `cargo clippy --locked --all-targets --features
  test-fixtures --no-deps -- -D warnings` (reserve `--all-targets` for here per relink-cost);
  `cargo nextest run --locked --features test-fixtures` (lib + integration).
- [ ] `npx tsc --noEmit` + `npx vitest run` (unaffected; safety).
- [ ] Push branch, open PR (ZEB IDs out of title/body), enter bot-review loop.
