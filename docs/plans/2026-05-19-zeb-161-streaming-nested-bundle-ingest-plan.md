# ZEB-161 — Implementation plan

Spec: [`docs/specs/2026-05-19-zeb-161-streaming-nested-bundle-ingest-design.md`](../specs/2026-05-19-zeb-161-streaming-nested-bundle-ingest-design.md)
Branch: `zeb-161-streaming-nested-bundle-ingest`

Decomposed for subagent-driven execution. Each task is self-contained — implementer reviews the spec section called out, writes code + tests, commits, hands to spec/code reviewers, iterates until both approve, then yields to the next task.

## Task ordering

```text
Task 1: streaming_ingest + build_bundle_tree (pure helpers, unit-tested)
      ↓
Task 2: wire send_ingest_bytes_only; drop Oversized + size gates
      ↓
Task 3: wire ingest_content IPC + ingest_file_at_path; remove chunk_and_bundle + dispatch
      ↓
Task 4: remove SkipCounts.oversized end-to-end
      ↓
Task 5: update existing integration test; add depth-2+ round-trip
```

Tasks 1-3 are tightly serialized (each removes APIs the previous step established). Tasks 4-5 could parallelize if a subagent has spare capacity, but order them sequentially for review-loop clarity.

---

## Task 1 — Streaming primitives + unit tests

**Spec sections:** "API surface > `streaming_ingest`", "Pipeline", "Test plan > Unit"

**Files:**
- `src-tauri/src/lib.rs` — add `streaming_ingest` and `build_bundle_tree` (the inner helper called by `streaming_ingest` after leaf collection completes). Keep them `pub(crate)` since only this crate and its `tests/` consume them.
- `src-tauri/src/lib.rs` (unit-test module) — replace the four `chunk_and_bundle_*` unit tests at lines 22571-22631 with 6 tree-builder tests + 5 streaming-bridge tests per the spec's "Test plan > Unit" section.

**Constraints:**
- Do NOT remove `chunk_and_bundle`, `FLAT_BUNDLE_MAX`, `IngestDispatch`, `ingest_dispatch`, or `IngestError::Oversized` in this task. They stay temporarily — the production callers still rely on them. They go away in Tasks 2-3.
- Do NOT change `send_ingest_bytes_only`, `ingest_file_at_path`, or `ingest_content` in this task.
- `streaming_ingest` must be generic over `R: AsyncRead + Unpin` so `Cursor<Vec<u8>>`, `tokio::fs::File`, and a `tokio_test::io::Builder` mock reader all work.
- Implement `build_bundle_tree` as a separate `pub(crate)` function that takes `(leaf_cids: Vec<ContentId>, total_size: u64, ingest_tx: &Sender<IngestRequest>)` so it can be unit-tested with synthetic CIDs without going through the chunker.

**Test gates:**
- `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(streaming_ingest) or test(build_bundle_tree)'` — all new tests green.
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — zero warnings.
- `cargo fmt --all -- --check`.

**Commit message:** `feat(ingest): add streaming_ingest + build_bundle_tree primitives (ZEB-161)`

---

## Task 2 — Wire `send_ingest_bytes_only` to streaming; drop `IngestError::Oversized`

**Spec sections:** "API surface > Modified > `send_ingest_bytes_only`", "Demolitions" rows for lib.rs:6128-6189, lib.rs:6097-6101+6143-6147, IngestError::Oversized

**Files:**
- `src-tauri/src/lib.rs:6128-6189` (`send_ingest_bytes_only`) — replace the dispatch body with `let reader = tokio::io::BufReader::new(std::io::Cursor::new(bytes));` then `streaming_ingest(reader, ingest_tx, ChunkerConfig::DEFAULT).await`. Drop the `IngestDispatch` branching and the explicit `Oversized` early-return.
- `src-tauri/src/lib.rs:6097-6101` (`ingest_file_at_path`, partial) — drop the `if size > FLAT_BUNDLE_MAX` early-return block. The full conversion to streaming lands in Task 3; this step just removes the gate.
- `src-tauri/src/lib.rs:130-155` (`IngestError`) — remove the `Oversized { size, cap }` variant. Adjust the `#[error("...")]` macros in remaining variants if compile errors cascade.
- `src-tauri/src/folder_ingest.rs` — drop the `IngestError::Oversized` handling arm in `record_error` / `match` against the walker's error routing. Leaf files that previously routed to `skipped.oversized` no longer have anywhere to land — and that's correct, because they no longer fail at this layer.

**Constraints:**
- `chunk_and_bundle`, `IngestDispatch`, `ingest_dispatch`, `FLAT_BUNDLE_MAX` stay in place this task. They're still consumed by `ingest_content` (which Task 3 converts).
- `SkipCounts.oversized` field stays this task. Task 4 removes it. Walker callers that previously incremented it now have no path to do so (the variant is gone) — the field's value remains 0 until Task 4 removes it.

**Test gates:** all of Task 1's plus —
- `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(folder_ingest) or test(send_ingest)'` — no regressions.
- Folder-walker integration tests that previously asserted `oversized > 0` (in `folder_ingest_walker_integration.rs`) must be temporarily updated to assert `oversized == 0` (the field still exists; Task 4 removes it). The walker should not produce oversized leaves anymore.

**Commit message:** `refactor(ingest): route send_ingest_bytes_only through streaming, drop Oversized variant (ZEB-161)`

---

## Task 3 — Wire `ingest_content` IPC + `ingest_file_at_path`; remove `chunk_and_bundle`

**Spec sections:** "API surface > Modified" entries for `ingest_content` and `ingest_file_at_path`; "Demolitions" rows for lib.rs:92-93, 96-103, 107-120, 181-224, 6085-6111, 5984-..

**Files:**
- `src-tauri/src/lib.rs` (`ingest_content` IPC handler, line ~5984) — after the picker returns a `path`, open the file via `tokio::fs::File::open(path).await?`, wrap in `tokio::io::BufReader::new(...)`, call `streaming_ingest(reader, ingest_tx, ChunkerConfig::DEFAULT, None).await?` which returns `(root_cid, total_size)`. Pass both to `send_ingest_with_name` (which now only does the sidecar-row insertion). The streamed `total_size` eliminates the pre-ZEB-161 stat-then-read TOCTOU window where a concurrent truncate/append between `tokio::fs::metadata()` and the actual read could land a stale size on the sidecar row. The intermediate `Vec<u8>` and the `let bytes = tokio::fs::read(path).await?` line at ~6009 are gone.
- `src-tauri/src/lib.rs:6085-6111` (`ingest_file_at_path`) — same restructure as `ingest_content`: open file, stream, insert sidecar from returned CID.
- `src-tauri/src/lib.rs` (`send_ingest_with_name`) — refactor to take the precomputed root CID instead of computing it internally. The new shape: `async fn send_ingest_with_name(content_index, root_cid: [u8; 32], file_name, size_bytes, parent_sidecar_id) -> Result<IngestResult, IngestError>` — pure sidecar insertion. Update the two call sites.
- `src-tauri/src/lib.rs:181-224` — **delete** `chunk_and_bundle`.
- `src-tauri/src/lib.rs:92-93, 96-103, 107-120` — **delete** `FLAT_BUNDLE_MAX`, `IngestDispatch`, `ingest_dispatch`.
- `src-tauri/tests/content_index_integration.rs:307-..` — `chunked_ingest_pin_cascade_fetch_burn_roundtrip`. Change `use harmony_app::chunk_and_bundle;` to `use harmony_app::streaming_ingest;`. The 3 MB test driver becomes a single call: `streaming_ingest(Cursor::new(bytes), &ingest_tx, ChunkerConfig::DEFAULT).await` returning a `ContentId`. Recompute `expected_descendants` from `walk_recursive` over the root rather than from local `leaves` (since leaves are no longer returned as a tuple member).
- `src-tauri/src/folder_ingest.rs` — remove the `use crate::{... FLAT_BUNDLE_MAX}` import (line 37) and the per-leaf size cap in `walk` / `pre_walk_count`. The pre-walk now counts every leaf file regardless of size.

**Constraints:**
- After this task, `cargo check --all-targets` MUST pass — every reference to the demolished APIs is removed.
- `IngestError` no longer has `Oversized`; ensure folder_ingest's error routing maps the remaining variants to `failed` / `record_fail` with the right messages.
- `SkipCounts.oversized` field still exists (Task 4 removes it). It's just never incremented.

**Test gates:** all of Task 2's plus —
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — all green.
- `cargo check --locked --all-targets --features test-fixtures` (MSRV gate).

**Commit message:** `refactor(ingest): route IPC entry points through streaming, demolish flat-bundle dispatch (ZEB-161)`

---

## Task 4 — Remove `SkipCounts.oversized` end-to-end

**Spec sections:** "Frontend changes", "Demolitions" rows for folder_ingest.rs:75-78 and frontend files

**Files:**
- `src-tauri/src/folder_ingest.rs:75-78` — drop the `oversized: u64` field from `SkipCounts`. Update the `#[derive(Default)]` (auto-handled) and any field-by-field constructor in tests.
- `src-tauri/src/folder_ingest.rs` walker — remove any remaining `counters.skipped.oversized += 1` calls. (After Task 2 this is the unreachable arm; Task 4 just lifts the field.)
- `src-tauri/tests/folder_ingest_walker_integration.rs` — remove the now-trivially-zero `assert_eq!(result.skipped.oversized, ...)` lines and any test fixtures that build a `SkipCounts { oversized, .. }` literal.
- `src/lib/file-manager-service.ts:69` — drop the `oversized: number;` field.
- `src/lib/components/FolderIngestSummaryModal.svelte:78-80` — drop the `{#if result.skipped.oversized > 0}` bullet.
- `src/lib/components/__tests__/file-browser-folder-ingest.test.ts` — drop `oversized` from all fixtures.

**Test gates:**
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — green.
- `npx tsc --noEmit` from repo root — zero errors.
- `npx vitest run` from repo root — green (no fixture missing the `oversized` field).

**Commit message:** `chore(ingest): remove SkipCounts.oversized — no longer reachable post-streaming (ZEB-161)`

---

## Task 5 — Depth-2+ integration test; update existing chunked-ingest test driver

**Spec sections:** "Test plan > Integration"

**Files:**
- `src-tauri/tests/content_index_integration.rs:307-..` — verify Task 3's driver swap is in place. If Task 3's PR-iteration loop adjusted the assertions, confirm `expected_descendants` is correctly recomputed from `walk_recursive(root)`. (This is verification — not a new edit unless Task 3's commit needs a fix-up.)
- `src-tauri/tests/folder_ingest_walker_integration.rs:473-494` — delete the oversized-leaf test (`set_len past FLAT_BUNDLE_MAX`).
- `src-tauri/tests/folder_ingest_walker_integration.rs` (new test) — add `nested_bundle_tree_round_trip`:
  - Skip with `if std::env::var("HARMONY_LARGE_TESTS").ok().as_deref() != Some("1") { return; }` for local-dev opt-out — exact-"1" check (not just `.is_err()`) so `HARMONY_LARGE_TESTS=0` doesn't accidentally enable the 36 GiB path.
  - Open a tempfile, `set_len(36 * 1024 * 1024 * 1024 + 1)` — sparse 36 GiB.
  - Drive `streaming_ingest(tokio::fs::File::open(path).await?, &ingest_tx, ChunkerConfig::DEFAULT).await` — record the elapsed time as a smoke-test bound (warn at > 60 s).
  - Assert the returned CID is `CidType::Bundle(d)` with `d >= 2`.
  - Walk via the runtime's fetch path; assert `walk_recursive(root)` returns the expected leaf count (~36_864 leaves at ~1 MiB forced cuts for 36 GiB sparse-zero file — see the spec's chunker-on-zeros analysis for why DEFAULT config forces every cut at max_chunk).
  - Parse the root bundle's first entry; assert `parse_inline_metadata` returns `(36 * 1024 * 1024 * 1024 + 1, 36864ish, _, _)`.

**Constraints:**
- The 36 GiB sparse-file test must be opt-in (env-var gated). CI workflow can opt in via `HARMONY_LARGE_TESTS=1` in the `rust-test` job — that's a separate `.github/workflows/ci.yml` edit handled in this task.
- The leaf-count assertion uses `> 32_767 && < 50_000` (loose bound; FastCDC distribution variance means exact count drifts) — not a tight equality. The expected count for the 36 GiB sparse fixture is ~36_864 (forced max_chunk cuts on pure-zero input — see the gear-hash analysis in the design spec); `< 50_000` is ~1.36× headroom which catches pathological chunker misconfiguration without false-failing on routine drift.

**Test gates:**
- `HARMONY_LARGE_TESTS=1 cargo nextest run --locked --features test-fixtures -E 'test(nested_bundle_tree_round_trip)'` — green locally on the Ildwyn dev machine (≥ 36 GiB free disk for the sparse fixture; sparse files don't consume real disk, but the tempdir filesystem needs to support sparse holes).
- Default `cargo nextest run` (without the env var) — test skipped, all others green.
- CI `rust-test` job — green with `HARMONY_LARGE_TESTS=1` set.

**Commit message:** `test(ingest): add depth-2+ nested-bundle round-trip integration test (ZEB-161)`

---

## Post-task gates (before opening PR)

Run from the repo root:

```powershell
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
npx tsc --noEmit
npx vitest run
```

All green. Then open the PR per the `superpowers:finishing-a-development-branch` flow.

## Out-of-scope follow-ups (mention in PR body)

- ZEB-157 — partial-ingest rollback (more orphans possible with depth-N trees).
- Leaf-CID spill to disk for multi-TB files.
- Pipelined leaf sends with bounded concurrency.
- Updating `docs/specs/2026-04-23-chunked-ingest-design.md` (Q1 references FLAT_BUNDLE_MAX as v1 limit — now obsolete). Either annotate as superseded or leave as-is for historical record.
