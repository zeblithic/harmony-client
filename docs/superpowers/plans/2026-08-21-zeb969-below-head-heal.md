# ZEB-969 Below-Head Hole Healing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let RBSR reconcile heal channel-log holes below an advanced author-lane head, with append-level dedup replacing the replay tracker as the log's duplicate guard.

**Architecture:** Three-layer change in `community_channel_log.rs` + `community_channel_log_engine.rs`: (1) max-aware replay-tracker `record`, (2) `ChannelLog::append` dedupes by ReconcileKey presence and returns `AppendOutcome`, (3) `process_inbound_packet` gains `IngestProvenance`; `Reconcile` provenance admits fully-verified below-head events.

**Tech Stack:** Rust, cargo-nextest, `scripts/test-select --context task` per task, full `--workspace --all-targets --features test-fixtures` sweep + clippy + fmt pre-PR.

**Spec:** `docs/specs/2026-08-21-zeb-969-below-head-heal-design.md`

## Global Constraints

1. All cargo commands from `src-tauri/`, always `--locked`, tests always `--features test-fixtures`.
2. TDD: watch every new test fail before implementing.
3. `Live` ingest semantics must remain byte-for-byte identical (drop sites, costs, tracker advances).
4. Below-head admission NEVER bypasses `verify_channel_event` (signature, membership-at-HLC, forward-skew).

---

### Task 1: Max-aware `ChannelLogReplayTracker::record`

**Files:** Modify `src-tauri/src/community_channel_log.rs` (`record`, ~line 1132; its doc comment; the `last_seen` insert). Tests inline in the same file's `mod tests`.

**Interfaces:** Produces: `record(&mut self, event: &SignedChannelEvent)` — same signature, new contract: advances a lane's `last_seen` only when `event.at()` is strictly newer (`Hlc::is_strictly_newer_than`); older/equal stamps are no-ops.

- [ ] Test `record_does_not_regress_lane_head`: record event at `wall_ms=2000`, then record same-lane event at `wall_ms=1000`; assert `last_seen()` for the lane is still the 2000 stamp. Run: `cargo nextest run --locked --features test-fixtures -E 'test(record_does_not_regress)'` — expect FAIL (head regresses to 1000).
- [ ] Implement: in `record`, look up the lane entry; insert/overwrite only if absent or `at.is_strictly_newer_than(prev)`. Update the doc comment ("overwrites unconditionally" → max-fold contract).
- [ ] Run the test — PASS. Run `scripts/test-select --context task` (tracker tests like `replay_tracker_rejects_duplicate` are in the always-run set) — green.
- [ ] Update the engine boot-rebuild comment block (community_channel_log_engine.rs:552–576): the segments-then-tail order note is now historical; rebuild is order-independent. Keep the walk itself unchanged (record-per-event still yields the per-lane max).
- [ ] Commit: `fix(channel-log): replay-tracker record is max-fold — boot rebuild order-independent (ZEB-969)`

### Task 2: `AppendOutcome` + ReconcileKey dedup inside `ChannelLog::append`

**Files:** Modify `src-tauri/src/community_channel_log.rs` (`append` ~line 1990, new `AppendOutcome` struct, new `contains_reconcile_key`); `src-tauri/src/community_channel_log_engine.rs` call sites 1169, 1340, 1800 + step-4 emit gating (~1818).

**Interfaces:** Produces:
- `pub struct AppendOutcome { pub newly_appended: bool, pub seal_ready: bool }`
- `ChannelLog::append(&mut self, event: SignedChannelEvent) -> Result<AppendOutcome, ChannelLogPersistError>`
- `ChannelLog::contains_reconcile_key(&self, event: &SignedChannelEvent) -> bool` (partition_point on `reconcile_entries` with `reconcile_key(event)`)

- [ ] Test `append_dedupes_by_reconcile_key`: append event `e`; assert `newly_appended`; append the identical `e` again; assert `!newly_appended` and tail length still 1 and reaction/watermark state unchanged. Run scoped — expect FAIL (`bool` has no field / tail length 2).
- [ ] Implement: at the top of `append` (after the manifest binding check), `if self.contains_reconcile_key(&event) { return Ok(AppendOutcome { newly_appended: false, seal_ready: self.tail.len() >= self.config.seal_threshold_events }); }`. Wrap the existing tail-push path's return in `AppendOutcome { newly_appended: true, seal_ready: <old bool> }`.
- [ ] Fix the three destructuring call sites by compiler error (`scoped build`): local-publish ×2 use `.seal_ready` where they used the bool; inbound step 3 binds the outcome and step 4 emits + `flush_dirty.notify_one()` only when `newly_appended`.
- [ ] Run scoped tests + `scripts/test-select --context task` — green (test call sites using `.expect("append")` compile unchanged).
- [ ] Commit: `fix(channel-log): append dedupes by ReconcileKey, returns AppendOutcome (ZEB-969)`

### Task 3: `IngestProvenance` + below-head admission on the RBSR path

**Files:** Modify `src-tauri/src/community_channel_log_engine.rs`: new `IngestProvenance` enum; `process_inbound_packet(self, packet, provenance)` (~1649); subscriber call site (~643) passes `Live`; `rbsr_ingest_and_next` call site (~2114) passes `Reconcile`; the test-only ingest helper (~2073) keeps `Live` unless a test opts in.

**Interfaces:** Consumes Task 2's `AppendOutcome` and Task 1's max-fold `record`. Produces: 2a/2c behavior per spec — for `Reconcile`: 2a `Err(Replay)` → log-lock `contains_reconcile_key`; present → drop (debug, replay_drops++); absent → set `below_head`, continue. 2c `Err(Replay)` → skip advance, continue. On `newly_appended && below_head` → `tracing::info!(... "below-head heal (ZEB-969)")` with community, channel, author, device, wall_ms/logical.

- [ ] Test `live_below_head_event_still_drops`: seed engine with same-lane event at `wall_ms=2000`; feed an encrypted valid packet at `wall_ms=1000` via the `Live` ingest; assert log length unchanged, `replay_drops` incremented. Expect FAIL only after the enum lands (write it against the new signature; it pins Live semantics).
- [ ] Test `reconcile_below_head_event_heals`: same seeding; feed the 1000-stamp packet via `Reconcile`; assert log now contains both events and the tracker head is still 2000. Expect FAIL (dropped at 2a).
- [ ] Implement the enum, thread the parameter through both call sites, add the 2a/2c `Reconcile` branches + INFO heal line.
- [ ] Both tests PASS; `scripts/test-select --context task` green.
- [ ] Commit: `feat(channel-log): RBSR-provenance ingest heals below-head holes (ZEB-969)`

### Task 4: End-to-end + regression coverage

**Files:** Tests in `src-tauri/src/community_channel_log_engine.rs` (two-fixture RBSR pattern, near `rbsr_ingest_and_next_recovers_missing_events_via_inbound_path` ~3786) and `community_channel_log.rs`.

- [ ] Test `rbsr_heals_hole_below_advanced_lane_head` (the Krile repro): responder log `[e1,e2,e3,e4]` one lane; requester log `[e1,e4]` (tracker head e4); drive `rbsr_build_initial` → `rbsr_respond` → `rbsr_ingest_and_next`; assert requester log has all 4, tracker head e4, and re-running the round appends nothing (dedup).
- [ ] Test `boot_rebuild_after_heal_keeps_lane_max`: build a log whose tail holds `[e4, e2]` (healed event last in storage order); respawn the engine over that dir (registry fixture pattern, ~6268); assert rebuilt tracker head is e4.
- [ ] Test `below_head_heal_requires_valid_signature`: `Reconcile`-ingest a below-head packet with a corrupted sig; assert dropped (log unchanged) — verify still gates.
- [ ] All new tests PASS; commit: `test(channel-log): below-head heal e2e + rebuild regression (ZEB-969)`

### Task 5: Full gates + PR

- [ ] `cd src-tauri && cargo fmt --all`
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (full pre-PR sweep; test-select is not sufficient here)
- [ ] `npx tsc --noEmit && npx vitest run` from repo root (no frontend changes expected — confirms nothing leaked)
- [ ] `git status` clean-tree check, push branch, open PR "fix(channel-log): heal holes below an advanced author-lane head via RBSR (ZEB-969)" with `Fixes ZEB-969`, fire `@coderabbitai review` once, converge, report at merge gate.
