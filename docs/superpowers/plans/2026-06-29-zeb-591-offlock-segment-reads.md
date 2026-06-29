# ZEB-591: collect_events / collect_events_vector — read segments off-lock

**Goal:** Stop holding the async log mutex (`self.log.lock().await`) across synchronous `std::fs::read` segment I/O in the channel-log catch-up readers, so concurrent log ops (e.g. live `append`) aren't stalled for the duration of a multi-segment catch-up read.

**Architecture:** Mirror the off-lock pattern already shipped in `ChannelLogEngine::find_attachment` (`community_channel_log_engine.rs`): snapshot the segment descriptors + in-memory tail + root `PathBuf` under the lock, drop the lock, then read + walk segments off the async executor via `tokio::task::spawn_blocking` using the free function `read_segment_at(&root, seg)`. The entire oldest-first walk (segments then tail, with the `keep` predicate, `since`/vector filter, and `limit` early-exit) runs inside the blocking closure — moving `keep` in by value avoids splitting closure ownership across the await.

**Tech Stack:** Rust, tokio, ciborium segments. All cargo from `src-tauri/`.

## Global Constraints

- Behavior-preserving: identical events returned, oldest-first, same paging/limit semantics, same `since` segment-skip on the scalar path, same vector filter on the vector path, same error surface (`ChannelLogEngineError::Persist`). New-only error path: a `spawn_blocking` JoinError (task panic) maps to `Persist(Io(..))`, matching `find_attachment`.
- Scope is exactly `collect_events` + `collect_events_vector`. Boot-time `rebuild_reaction_index` / watermark rebuild also read segments under `&mut self`, but they run single-threaded at construction (uncontended) — out of scope.
- `keep` callers pass non-capturing closures (`|_| true`, `|ev| matches!(ev, Post)`) → already `Send + 'static`; the signature gains `+ Send + 'static`.
- Local gates green before PR: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked -p harmony-app --features test-fixtures` (scoped) then `--all-targets` sweep.

---

## Task 1: Characterization tests (pin behavior across the off-lock boundary)

**Files:**
- Test: `src-tauri/src/community_channel_log_engine.rs` (`#[cfg(test)] mod tests`)

These pass on the CURRENT code (behavior is unchanged) — they are the refactor's safety net and exercise the exact multi-segment + tail read path that moves off-lock.

- [ ] **Step 1: scalar — order + paging + since across 2 sealed segments + tail.** `build_engine_fixture(4, 250, 1000)`; append 10 Post events `msg-0..msg-9` (wall_ms `100..109`, device `test-device`), `seal_and_persist()` every 4 → 2 segments (msg-0..3, msg-4..7) + tail (msg-8,9). Assert: `list_messages(None, 1000)` bodies == `msg-0..msg-9` in order; `list_messages(None, 6)` == `msg-0..msg-5` (limit cuts inside segment 2); `list_messages(Some(hlc msg-3 = wall 103), 1000)` == `msg-4..msg-9` (segment 1 skipped via `seg.range.1` not strictly-newer-than `since`).
- [ ] **Step 2: vector — never-seen lane in the OLDEST sealed segment is served.** `build_engine_fixture(4, 250, 1000)`; append events so a never-seen `(author, device-b)` event sits in segment 0 (sealed), plus a seen `(author, device-a)` lane. Assert `list_messages_vector(v, 1000)` serves the never-seen-lane event from the sealed segment (proves all segments are scanned off-lock and the vector filter applies post-`spawn_blocking`).
- [ ] **Step 3: run them green on current code** (`cargo nextest run -p harmony-app --features test-fixtures -E 'test(collect_events_offlock) + test(collect_events_vector_offlock)'`), then commit the tests.

## Task 2: Refactor `collect_events` off-lock

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (`collect_events`)

- [ ] **Step 1:** Change signature `keep: impl Fn(&SignedChannelEvent) -> bool` → `+ Send + 'static`.
- [ ] **Step 2:** Replace the under-lock body: snapshot `(segments, tail, root)` in a scoped lock block, then `spawn_blocking(move || { walk segments (with `since` seg-skip + per-event `since` + `keep` + limit), then walk `tail` (consumed; same filters) })`, awaiting + double-`map_err` exactly as `find_attachment`.
- [ ] **Step 3:** `cargo nextest run -p harmony-app --features test-fixtures -E 'test(collect_events) + test(list_messages) + test(list_post)'` → green. Commit.

## Task 3: Refactor `collect_events_vector` off-lock

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (`collect_events_vector`)

- [ ] **Step 1:** Same signature change; clone `vector` to move into the closure.
- [ ] **Step 2:** Same snapshot + `spawn_blocking` walk (no `since` skip — vector path scans all segments by design), `Self::vector_serves(&vector, &ev)` filter.
- [ ] **Step 3:** Full scoped run, then the `--all-targets` clippy + nextest sweep (check-only clippy as final gate when test code changes). Commit.

## Task 4: PR

- [ ] Push `channel-log-offlock-segment-reads`; open PR (body: what/why/test-strategy incl. the characterization-vs-timing rationale; `Closes ZEB-591`); trigger CodeRabbit; converge bots; Jake merges.
