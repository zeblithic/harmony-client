# ZEB-906 Implementation Plan — known-PendingJoin countersign healing

> **For agentic workers:** execute task-by-task; each task ends with a scoped
> test cycle. Spec: `docs/specs/2026-08-11-zeb-906-pendingjoin-countersign-healing-design.md`

**Goal:** a host (or any eligible Joined member) that holds an
un-countersigned `PendingJoin` re-drives the auto-countersign both on inbound
publishes from that member (live heal) and at boot (restart heal).

**Branch:** `zeblith/zeb-906-pendingjoin-salvage`

## Global constraints

- Cargo commands from `src-tauri/`, always `--locked --features test-fixtures`;
  clippy `--all-targets -D warnings`; `cargo fmt --all -- --check`.
- No wire/schema changes; no frontend changes.
- The `PublisherNotJoined` rejection itself is UNCHANGED in every case.

---

### Task 1: Fix A — ingest-seam re-drive

**Files:** `src-tauri/src/community_state_sync.rs`

1. In `handle_incoming_publish`'s pre-mutation gate strict-reject arm
   (~4295), before the `return IncomingOutcome::ErrPreMutation(...)`:
   - gate on `matches!(status_now, Some(MemberStatus::PendingJoin))`;
   - find the publisher's latest `PendingJoin` in the already-cloned `events`
     (`.filter(actor == payload.publisher_addr, kind PendingJoin)`
     `.max_by(event_sort_key)`);
   - `maybe_spawn_auto_counter_sign_for_ctx(ctx, pending_ev)` + `info!` log.
   - Rewrite the arm's "no salvage needed" comment to describe the re-drive.
2. Tests (existing in-file `handle_incoming_publish` harness, ~7206 region):
   - `zeb906_known_pending_join_publish_redrives_countersign` — reject +
     countersign appears (poll) + member materializes Joined + re-publish
     accepted.
   - `zeb906_left_publisher_rejected_without_countersign` (and Banned).
   - `zeb906_redrive_idempotent_when_countersign_exists`.
3. Scoped gate: `cargo nextest run --locked -p harmony-app --features
   test-fixtures -E 'test(zeb906)'` + touched-module tests. Commit.

### Task 2: Fix B — engine recheck method

**Files:** `src-tauri/src/community_state_sync.rs`

1. `pub(crate) async fn recheck_uncountersigned_pending_joins(&self)` on
   `CommunitySyncEngine`: under one state lock collect `PendingJoin` events
   with no self-authored `JoinCountersign` targeting their id; drop lock;
   `self.maybe_spawn_auto_counter_sign(&ev)` per candidate; `info!` count
   when non-zero.
2. Update the two "re-derived on next boot (C1)" comments (~2240, ~2369) to
   reference this pass.
3. Tests: eligible self → countersign appears; ineligible self (not Joined)
   → no-op; already-countersigned → no candidates.
4. Scoped gate + commit.

### Task 3: Boot wiring

**Files:** `src-tauri/src/lib.rs` (BOOT-PROBE 09 healing-pass region)

1. After the joiner-side C3 pass: for each community engine in the registry,
   call `recheck_uncountersigned_pending_joins().await`.
2. Verify both GUI and headless flow through this site (start_node shared).
3. Scoped gate + commit.

### Task 4: Full gates + PR

1. Full sweep: nextest workspace `--all-targets`, clippy, fmt, tsc + vitest
   (should be no-op frontend), MSRV check.
2. `git status` clean; push branch; open PR (body links ZEB-906, closes
   keyword), fire single `@coderabbitai review`; converge per protocol.
