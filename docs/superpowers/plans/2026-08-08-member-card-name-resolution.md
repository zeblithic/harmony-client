# Member-Card Name Resolution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a peer's display name resolve reliably (D1: card is actually published) and fast for later joiners (D2: query-on-subscribe), fixing ZEB-882 + ZEB-884.

**Architecture:** Three seams. (A) an in-memory `NodeState.pending_card` latch that auto-completes a boot-race publish; (B) a `--display-name` flag + serve-boot publish for headless parity; (C) a publisher-side Zenoh queryable + subscriber-side query-on-subscribe `get` sharing one `ingest_card_bytes` pipeline.

**Tech Stack:** Rust (tokio, Zenoh), Tauri IPC, existing `ProfileCardPublisher`/`ProfileCardCache` machinery.

## Global Constraints

- Cargo commands run from `src-tauri/`. Gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend from root: `npx tsc --noEmit`; `npx vitest run`.
- **Reply-drain wedge (ZEB-803/812):** in the D2 subscriber `session.get`, ZERO `.await` forwarding a Zenoh `Reply` into a bounded channel. Local-drain into an owned `Option<Vec<u8>>` only. Mirror `fetch_via_zenoh` (event_loop.rs:7650) / `query_mail_root` (event_loop.rs:7706).
- Display name is NEVER persisted backend-side. The latch is in-memory only.
- No placeholder/empty-name card is ever published; a node with no real name publishes nothing.
- Card wire cap `MAX_CARD_WIRE_BYTES = 4096`; the drop-before-decode guard stays.
- `MissedTickBehavior`/burst cadence in `ProfileCardPublisher` is unchanged.

---

### Task 1: Publisher `latest_handle()` accessor

**Files:**
- Modify: `src-tauri/src/profile_card_broadcast.rs` (`ProfileCardPublisher`, ~527-629)
- Test: same file `mod tests` (~646+)

**Interfaces:**
- Produces: `ProfileCardPublisher::latest_handle(&self) -> std::sync::Arc<Mutex<Option<CardWire>>>` (an `Arc::clone` of the private `latest`; `CardWire = (String, Vec<u8>)`). Consumed by Task 5's queryable.

- [ ] **Step 1:** Write a failing unit test `latest_handle_observes_published_card`: spawn a publisher (via a `CapturingSink`, `spawn_no_burst`), grab `let h = pub.latest_handle();` assert `h.lock().await.is_none()`; `pub.publish_now(topic, bytes).await.unwrap();` assert `h.lock().await.as_ref().map(|(_,b)| b.clone()) == Some(bytes)`.
- [ ] **Step 2:** Run it — fails to compile (`latest_handle` undefined).
- [ ] **Step 3:** Add `pub fn latest_handle(&self) -> std::sync::Arc<Mutex<Option<CardWire>>> { std::sync::Arc::clone(&self.latest) }`.
- [ ] **Step 4:** Run the test → PASS. `cargo clippy` clean on the crate.
- [ ] **Step 5:** Commit `feat(profile-card): expose latest_handle for query-on-subscribe (ZEB-884)`.

---

### Task 2: Pending-card latch (D1 GUI)

**Files:**
- Modify: `src-tauri/src/lib.rs` — `NodeState` struct (add field); `publish_owner_card` (~14982-15065, stash at the not-ready branch ~15016); `start_node_inner` end (~13186, drain).
- Test: `src-tauri/src/lib.rs` unit tests, and/or `src-tauri/tests/mint/mint_owner_lifecycle.rs`.

**Interfaces:**
- Produces: `struct PendingCard { display_name: String, status_text: String, avatar_cid: Option<[u8;32]>, profile_page_root: Option<[u8;32]> }`; `NodeState.pending_card: Option<PendingCard>` (default `None`).
- Consumes: existing `publish_owner_card` / `republish_owner_card_impl` signatures.

- [ ] **Step 1:** Read `publish_owner_card` (14982-15065), the not-ready `else` (15016), `NodeState` definition, and the `start_node_inner` tail near 13186 to pin the exact drain site and the field accessors.
- [ ] **Step 2:** Write a failing test `publish_owner_card_stashes_pending_when_runtime_not_ready`: with a `NodeState` whose owner-runtime components are `None`, call the publish path with a name; assert it returns `Err` containing `"owner card runtime not ready"` AND `state.pending_card` is `Some` with the passed `display_name`.
- [ ] **Step 3:** Add `PendingCard` + `NodeState.pending_card` (default `None`); in the not-ready branch, set `pending_card = Some(PendingCard{..})` before returning the existing `Err`.
- [ ] **Step 4:** Run the test → PASS.
- [ ] **Step 5:** Write a failing test for the drain: `pending_card_drains_and_publishes_when_runtime_ready` — seam that, given a ready runtime + `pending_card = Some(..)`, runs the drain, asserts a card was published (via the publisher's `latest` becoming `Some` / a capturing sink) and `pending_card` is cleared to `None`. (If a clean unit seam over `start_node_inner` isn't available, factor the drain into a `drain_pending_card(state)` fn and test that directly; wire it into `start_node_inner`.)
- [ ] **Step 6:** Implement `drain_pending_card` + call it at the `start_node_inner` tail; run → PASS. Assert drain-with-`None` is a no-op.
- [ ] **Step 7:** `cargo nextest` on the touched tests; commit `fix(profile-card): latch a boot-race owner-card publish and auto-complete on runtime-ready (ZEB-882)`.

---

### Task 3: Serve `--display-name` + serve-boot publish (D1 headless)

**Files:**
- Modify: `src-tauri/src/main.rs` (serve arg parsing + dispatch ~341), `src-tauri/src/lib.rs` `serve_cli` (29281-29448; publish after start ~29355).
- Test: unit for the name-policy helper.

**Interfaces:**
- Produces: `fn resolve_serve_card_name(flag: Option<String>, profile: Option<String>) -> Option<String>` (flag > profile > None); `serve_cli` gains a `display_name: Option<String>` param.

- [ ] **Step 1:** Read `main.rs` serve dispatch + CLI parsing and `crate::profile::active_profile()` to get the profile-name accessor and how to read a `--display-name` arg.
- [ ] **Step 2:** Write failing unit tests for `resolve_serve_card_name`: `Some("x"), Some("p") -> Some("x")`; `None, Some("p") -> Some("p")`; `None, None -> None`.
- [ ] **Step 3:** Add the helper; run → PASS.
- [ ] **Step 4:** Add the `--display-name <NAME>` flag to serve arg parsing; thread `Option<String>` into `serve_cli(api_port, display_name)`.
- [ ] **Step 5:** In `serve_cli`, after `start_node_inner` succeeds (before/alongside `auto_subscribe_presence_all_communities`), compute `resolve_serve_card_name(flag, active_profile_name)`; if `Some(name)`, call `republish_owner_card_impl(&state, name, String::new(), None, None).await` (ignore-but-log error, like the presence hook); log the chosen name source at info.
- [ ] **Step 6:** `cargo check`/`clippy` for serve path; `cargo nextest` on the helper test. Manual: `--help`/arg-parse smoke if cheap.
- [ ] **Step 7:** Commit `feat(serve): --display-name + serve-boot card publish, fallback to profile name (ZEB-882)`.

---

### Task 4: `ingest_card_bytes` shared helper

**Files:**
- Modify: `src-tauri/src/event_loop.rs` — card subscriber sample handler (3005-3102).
- Test: existing `tests/profile/profile_card_cross_peer_integration.rs` already drives the pipeline; keep green (no behavior change).

**Interfaces:**
- Produces: an async helper capturing the decode→size-guard→`verify_card`→attribution→`insert_verified`→emit(`member-card-received`) body, callable with `(bytes, subscription_id, owner_id, cache, event_sink/emit_ctx)`. Consumed by the live-PUT arm (this task) and Task 6's get path.

- [ ] **Step 1:** Read 3005-3102 to capture exact inputs (size cap, `ciborium::from_reader`, `verify_card(&card, wall_now_secs())`, attribution `verified_owner == owner_id`, `cache.insert_verified`, the `member-card-received` payload fields).
- [ ] **Step 2:** Extract the body into `ingest_card_bytes(...)` (same module). The live-PUT sample arm calls it. No behavior change.
- [ ] **Step 3:** Run `tests/profile/*` integration + the crate build → all green (pipeline unchanged).
- [ ] **Step 4:** `clippy --all-targets` clean. Commit `refactor(profile-card): extract ingest_card_bytes shared by PUT + query paths (ZEB-884)`.

---

### Task 5: Publisher-side queryable

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (card subscriber-pool region ~2947, where `session_for_card` lives) OR `src-tauri/src/lib.rs` ~10738 (publisher spawn). Choose the site where both the shared session and the `latest_handle` are cleanly reachable.
- Test: covered indirectly; queryable reply-shape asserted via a focused test if a seam exists, else by the local-drain test in Task 6.

**Interfaces:**
- Consumes: `ProfileCardPublisher::latest_handle()` (Task 1), `card_topic_for(own_owner_id)`, the shared `session_arc`.

- [ ] **Step 1:** Read an existing `declare_queryable` site (e.g. event_loop.rs:1733/9768) to mirror the reply idiom (`while let Ok(query) = queryable.recv_async().await { query.reply(key, bytes).await }`).
- [ ] **Step 2:** Plumb the publisher's `latest_handle` to the chosen declaration site (add a field/param as needed).
- [ ] **Step 3:** Declare a `session.declare_queryable(card_topic_for(own_owner_id))`; on each `Query`, snapshot `latest_handle.lock().await.clone()`; if `Some((_, bytes))` reply `query.reply(query.key_expr().clone(), bytes).await`; if `None`, drop the query (reply nothing). Reply arm has no engine-channel await (only the queryable's own reply).
- [ ] **Step 4:** `cargo check`/`clippy`. Commit `feat(profile-card): publisher-side queryable answers cached card (ZEB-884)`.

---

### Task 6: Subscriber query-on-subscribe

**Files:**
- Modify: `src-tauri/src/event_loop.rs` — per-subscription task, right after `declare_subscriber` (2987).
- Test: `tests/profile/*` pipeline + a focused local-drain unit if seam-able.

**Interfaces:**
- Consumes: `ingest_card_bytes` (Task 4), the queryable (Task 5), `session.get`.

- [ ] **Step 1:** Read `fetch_via_zenoh` (7650-7684) to copy the safe local-drain shape (timeout + `while replies.recv_async().await` into a local `Option<Vec<u8>>`).
- [ ] **Step 2:** After `declare_subscriber`, issue `session.get(&key_expr)` once; local-drain the first valid reply (bounded by a few-second `tokio::time::timeout`); on `Some(bytes)` call `ingest_card_bytes(bytes, subscription_id, owner_id, cache, ..)`; on timeout/none, do nothing (fall through to live PUTs). Add a comment citing ZEB-803: no `.await` forwarding into a bounded channel.
- [ ] **Step 3:** Verify no behavior regression: `tests/profile/*` green; the get path reuses the same verify/attribution/cache as PUTs.
- [ ] **Step 4:** `clippy --all-targets` clean. Commit `feat(profile-card): query-on-subscribe fast path for late joiners (ZEB-884)`.

---

### Task 7: Full gate + PR

- [ ] **Step 1:** From `src-tauri/`: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. From root: `npx tsc --noEmit`; `npx vitest run`. Fix any failures.
- [ ] **Step 2:** `git status` clean (all committed), push branch.
- [ ] **Step 3:** Open PR against `main`, body linking ZEB-882 + ZEB-884 (keyword+ID for both), summarizing D1/D2 + the reply-drain safety note + the no-live-fleet-repro caveat.
- [ ] **Step 4:** Fire `@coderabbitai review` once (per the ONE-@-then-ZERO rule). Converge all three comment buckets across all bot authors + CI green. No auto-merge.
