# ZEB-932 Voting-log RBSR Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (inline, planner==implementer) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the voting log's O(peers × events) 300 s full-dump backfill with range-based set reconciliation (RBSR), reusing the proven channel-log machinery (ZEB-592/593), while keeping the full-dump as fallback + a ~1 h periodic backstop.

**Architecture:** Approach A from `docs/superpowers/specs/2026-08-15-zeb932-voting-log-rbsr-design.md` — live-set RBSR over `VotingLog.events` (archived ballots already pruned, so out of scope) + retained full-dump backstop. No change to the 90-day retention/archival path. The `channel_rbsr.rs` protocol core, `channel_chunk_index.rs`, the Zenoh RBSR transport scaffolding, and the backfill scheduler are reused; voting adds a `RangeReconcileSource` impl, a domain-separated seal/open, three engine halves, and transport wiring.

**Tech Stack:** Rust, tokio, Zenoh, ciborium (CBOR), ChaCha20-Poly1305, SHA-256.

## Global Constraints

- cargo run from `src-tauri/`; build/test with `--locked --features test-fixtures`.
- Scope test builds with `--lib` (or `scripts/test-select`) to avoid relinking ~97 `harmony-app` binaries during dev; run the full `nextest --workspace --all-targets` only at the final gate.
- Gates before "green": `cargo fmt --all -- --check` + `cargo clippy --all-targets --no-deps -D warnings` + full `cargo nextest run --workspace --all-targets`. Working tree must be clean at gate time.
- TDD Iron Law: no production code without a failing test first; watch it fail for the right reason.
- Preserve every invariant in spec §7. Never weaken the existing `apply_backfilled_event` admission path — RBSR is a delivery path, not a trust path.
- Reference implementation to mirror throughout: `channel_rbsr.rs`, `community_channel_log.rs` (`event_element_hash`:619, `reconcile_key`:2563, `RangeReconcileSource for ChannelLog`:2573, seal/open:1002-1045, `events_for_keys`:2520, `reconcile_entries`:1884, `rebuild_reconcile_index`:2491, `maintain_reconcile_index`:2498), `community_channel_log_engine.rs` (`rbsr_respond`:1998, `rbsr_build_initial`:2044, `rbsr_ingest_and_next`:2078, hook bundle:2731-2765), `event_loop.rs` (`spawn_channel_log_zenoh_adapter`:10997, `rbsr/**` queryable:11271-11342, `drive_rbsr_rounds`:12041, `rbsr_get_frames`:12108, `format_rbsr_key`/`parse_rbsr_key`:11983/11993, `RbsrAdapterHooks`:326-335, `RbsrStep`:12013), `channel_backfill.rs` (`ReconcileMode`:75-107, periodic floor/jitter:143-168).
- Existing voting seams: `VotingLog`:community_voting_log.rs:57 (`events`:65, `polls`:67), `canonical_key`:community_voting_tier3.rs:456, `event_hash_of`/`sha256_of_signing_bytes`:2420/2426, `SignedVotingEvent::signing_bytes`:community_voting_core.rs:933, `read_backfill_frames`:community_voting_log_engine.rs:543, `apply_backfilled_event`:3049, `VotingReplayTracker`/`seen_coords`:181/194, `archive_finalized_polls`:community_voting_log.rs:1348, voting Zenoh adapter:event_loop.rs:10522-10813, `VOTING_BACKFILL_INTERVAL`:lib.rs:58284.

---

### Task 1: Voting reconcile index + `RangeReconcileSource` + archive-drop seam

**Files:**
- Modify: `src-tauri/src/community_voting_log.rs` (add reconcile index to `VotingLog`; maintain at load/append/archive)
- Modify: `src-tauri/src/community_voting_tier3.rs` (expose `voting_reconcile_key` wrapper over `canonical_key`)
- Create/Modify: a `RangeReconcileSource for VotingLog` impl (in `community_voting_log.rs` or a sibling `voting_rbsr.rs` module)
- Test: inline `#[cfg(test)]` in the same module(s)

**Interfaces:**
- Consumes: `channel_rbsr::{ReconcileKey, RangeReconcileSource, RangeFingerprint}`, `channel_chunk_index::ChunkIndex`, `canonical_key`.
- Produces: `VotingLog::reconcile_index` (sorted `Vec<(ReconcileKey, [u8;32])>` mirroring `ChannelLog::reconcile_entries`), `impl RangeReconcileSource for VotingLog`, `fn voting_reconcile_key(&SignedVotingEvent) -> ReconcileKey`, and a `fn events_for_reconcile_keys(&[ReconcileKey]) -> Vec<SignedVotingEvent>` resolver.

- [ ] **Step 1: Failing test — index built from unordered events is sorted by key.** Insert events out of `canonical_key` order into a `VotingLog`; assert `reconcile_index` is strictly ascending by `ReconcileKey` and its length equals `events.len()`. Run scoped (`cargo nextest run -p <voting-lib-crate> --lib voting_rbsr::index -E 'test(reconcile_index)'`); expect FAIL (no index yet).
- [ ] **Step 2: Implement the index + build-on-load + insert-on-append.** Add `reconcile_index: Vec<(ReconcileKey,[u8;32])>` + a `ChunkIndex` to `VotingLog`; rebuild on load/deserialize (mirror `rebuild_reconcile_index`); incrementally insert at the single `events.push` append choke point (mirror `maintain_reconcile_index`). Sort by `canonical_key`. Verify test passes.
- [ ] **Step 3: Failing test — `RangeReconcileSource` methods.** Over a known event set, assert `range_count([lo,hi))`, `keys_in_range`, `split_key` (interior midpoint), and that `range_fingerprint` over the whole universe equals a hand-folded `RangeFingerprint` of the element hashes. Expect FAIL.
- [ ] **Step 4: Implement `impl RangeReconcileSource for VotingLog`** delegating to the sorted index + `ChunkIndex` (mirror `community_channel_log.rs:2573-2607`). Verify pass.
- [ ] **Step 5: Failing test — archive drops keys in lockstep.** Build a log with a finalized poll's ballots; call `archive_finalized_polls` past the horizon; assert (a) the pruned ballots are gone from `events` AND from `reconcile_index`, (b) every remaining index key resolves to a present body via `events_for_reconcile_keys`, and vice-versa. Expect FAIL (archive doesn't touch the index yet).
- [ ] **Step 6: Implement the drop-on-archive seam.** In `archive_finalized_polls`, after the `retain`s, rebuild/patch the reconcile index so it matches `events` exactly (drop pruned keys, repair affected chunks). Verify pass.
- [ ] **Step 7: Failing test — `events_for_reconcile_keys` discipline.** Requesting N keys returns exactly N distinct bodies whose `canonical_key` matches; a key absent from the log yields a short vec (caller must detect len mismatch). Assert distinct-key dedup. Expect FAIL, then implement the resolver (binary-search the sorted index; do not store raw `Vec` indices across archive). Verify pass.
- [ ] **Step 8: Determinism test.** Build two indices from the same event set inserted in two different orders; assert identical whole-universe fingerprints. Verify pass.
- [ ] **Step 9: Commit.** `git add -A && git commit` — "ZEB-932: voting reconcile index + RangeReconcileSource + archive-drop seam".

### Task 2: Voting RBSR seal/open + domain-separated AAD

**Files:**
- Create: `src-tauri/src/voting_rbsr.rs` (or a section alongside the voting engine) for seal/open + constants
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Consumes: `channel_rbsr::RbsrMessage`, ChaCha20-Poly1305, the voting epoch key type used by `voting_encrypt_current_epoch`.
- Produces: `const VOTING_RBSR_AAD: &[u8] = b"harmony-voting-rbsr-v1"`, `const MAX_VOTING_RBSR_MESSAGE_BYTES: usize = 64*1024`, `fn seal_rbsr_message(key, &RbsrMessage) -> Vec<u8>`, `fn open_rbsr_message(key, &[u8]) -> Option<RbsrMessage>`, `fn open_rbsr_message_with_any(keys, &[u8]) -> Option<RbsrMessage>`.

- [ ] **Step 1: Failing test — round-trip + AAD domain separation.** Seal an `RbsrMessage`, open it back (equal). Then assert a message sealed with `VOTING_RBSR_AAD` does NOT open under the channel `RBSR_AAD` or the voting live/backfill AAD, and a voting *event packet* does not open as an `RbsrMessage`. Also assert over-cap payloads are rejected before decrypt. Expect FAIL.
- [ ] **Step 2: Implement seal/open** mirroring `community_channel_log.rs:1002-1045` (`[12B nonce][ChaCha20-Poly1305(key, cbor(msg), VOTING_RBSR_AAD)]`), cap-checked before encrypt and decrypt; `open_*_with_any` tries `[current, previous]`. Verify pass.
- [ ] **Step 3: Commit** — "ZEB-932: voting RBSR seal/open + domain-separated AAD".

### Task 3: Engine halves (in-memory, no transport)

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (add `rbsr_build_initial`, `rbsr_respond`, `rbsr_ingest_and_next`)
- Test: inline `#[cfg(test)]` — an engine-pair reconcile without Zenoh

**Interfaces:**
- Consumes: Task 1 source + resolver, Task 2 seal/open, `channel_rbsr::{initial_request, respond, process_reply}`, existing `apply_backfilled_event`.
- Produces: `fn rbsr_build_initial(&self) -> Vec<u8>`, `fn rbsr_respond(&self, sealed_request: &[u8]) -> Option<(Vec<u8>, Vec<Vec<u8>>)>` (sealed_reply, have-packets), `async fn rbsr_ingest_and_next(&self, frames: Vec<Vec<u8>>) -> RbsrStep`.

- [ ] **Step 1: Failing test — two divergent logs converge with O(diff) transfer.** Build engine A (holder, superset) and engine B (behind by *k* events). Drive rounds B↔A purely in-memory: `B.rbsr_build_initial()` → `A.rbsr_respond()` → `B.rbsr_ingest_and_next()` → repeat until `Converged`. Assert B's log now equals A's, and the total `Have`-key count transferred ≈ *k* (not the log size). Expect FAIL.
- [ ] **Step 2: Implement the three halves** mirroring `community_channel_log_engine.rs:1998-2135`: `respond` runs `channel_rbsr::respond` against the `VotingLog` source, resolves `Have` keys → bodies (enforce `bodies.len() == have_keys.len()`, else `None`), seals reply + each body-as-voting-packet under **one** epoch key. `ingest_and_next` classifies frames (reply opens as `RbsrMessage`; else inline packet → route through `apply_backfilled_event`), guards `saw_extra_reply → Failed`, then `process_reply` for the next partition. Verify pass.
- [ ] **Step 3: Failing test — identical logs converge in one round, zero `Have`.** Expect FAIL then pass with Step 2 (assert no bodies transferred).
- [ ] **Step 4: Failing test — archive-window non-convergence is safe.** A archived a poll B still holds; drive rounds; assert it does NOT converge, and after the round cap the driver would signal fallback (return the terminal `RbsrStep`), and **no archived event is resurrected in A** (A requests, gets Have, `seen_coords` drops them, A.events unchanged). Verify pass.
- [ ] **Step 5: Failing test — multi-holder guard.** Inject a second sealed reply into the frame vec; assert `RbsrStep::Failed`. Verify pass.
- [ ] **Step 6: Failing test — epoch rotation mid-fetch.** Seal reply + Have packets under one epoch, open under `[current, previous]` after a simulated rotation; assert events still ingest. Verify pass.
- [ ] **Step 7: Commit** — "ZEB-932: voting RBSR engine halves + in-memory convergence tests".

### Task 4: Transport — voting `rbsr/**` queryable + RBSR-first requester driver

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (voting Zenoh adapter around :10522-10813; add rbsr queryable + drive_rbsr_rounds-first requester; reuse `format_rbsr_key`/`parse_rbsr_key`/`drive_rbsr_rounds`/`rbsr_get_frames` by parametrizing the topic prefix)
- Modify: `src-tauri/src/lib.rs` (wire the hook bundle into the voting adapter request)
- Test: inline where feasible; otherwise covered by the engine-pair tests + a fallback-mode unit test

**Interfaces:**
- Consumes: Task 3 halves as an `RbsrAdapterHooks`-shaped bundle; `ReconcileMode` mapping from `channel_backfill.rs`.
- Produces: a voting `rbsr/**` responder task + an RBSR-first requester path that falls back to the existing full-dump GET on `VectorFallback`/`Failed`/round-cap.

- [ ] **Step 1: Parametrize shared scaffolding.** Extract the channel-hard-coded topic prefix in `format_rbsr_key`/`parse_rbsr_key`/the queryable block so a `harmony/community/{id}/voting/rbsr/**` variant is expressible without forking behavior. Guard: existing channel-log RBSR tests stay green (run them scoped). Commit as a mechanical refactor if it stands alone.
- [ ] **Step 2: Failing test — fallback mode selection.** Unit-test the requester's mode logic: 0 frames round 0 → `VectorFallback`; converged → done; round-cap/`Failed` → full-dump. (Mirror `reconcile_mode_after_round0`/`reconcile_mode_after_round`.) Expect FAIL then implement. 
- [ ] **Step 3: Add the responder queryable.** Declare `harmony/community/{id_hex}/voting/rbsr/**`; on a valid request (guard parse, cap-before-alloc, payload-less → reply nothing) call `rbsr_respond` and stream `[sealed_reply, have_packets…]`. 
- [ ] **Step 4: Add the RBSR-first requester.** On each backfill trigger, `drive_rbsr_rounds` first (Locality::Remote, ConsolidationMode::None, 10 s, `MAX_RBSR_ROUND_BYTES` cap); on `VectorFallback`/`Failed`/round-cap run the existing full-dump GET. Wire the hook bundle in `lib.rs`.
- [ ] **Step 5: Scoped gate + commit.** `cargo clippy --all-targets` on the touched crate; commit — "ZEB-932: voting RBSR Zenoh transport + RBSR-first requester with full-dump fallback".

### Task 5: Backstop retune + final gates + docs

**Files:**
- Modify: `src-tauri/src/lib.rs` (`VOTING_BACKFILL_INTERVAL` → periodic floor + jitter role)
- Modify: memory / spec cross-ref as needed

**Interfaces:**
- Consumes: everything above.
- Produces: the full-dump firing as a ≤1 h backstop + jitter, not every 300 s.

- [ ] **Step 1: Failing test — backstop interval.** Assert the periodic full-dump floor is ~1 h (+ jitter bound) and that RBSR is attempted before it on each trigger. (If the interval is a plain const, assert its value + that the requester calls RBSR first; a wall-clock test is unnecessary — assert the const and the call ordering.) Implement: split the single 300 s pull into RBSR-first + a `PERIODIC_RESYNC_FLOOR_MS`-style backstop. Verify pass.
- [ ] **Step 2: Full local gates.** `cargo fmt --all -- --check`; `cargo clippy --all-targets --no-deps -D warnings`; `cargo nextest run --workspace --all-targets --features test-fixtures`. All green; working tree clean.
- [ ] **Step 3: Commit** — "ZEB-932: retune voting full-dump to a ~1h backstop behind RBSR".

---

## Finishing

After Task 5 gates are green: push the branch, open the PR against `main` (body ends with the Generated-with-Claude-Code footer), fire exactly one `@coderabbitai review`, then converge all bot findings in one bundle. Do NOT merge. Update ZEB-932 with the PR link.

## Self-Review notes
- Spec coverage: Task 1 = spec §5.1 + §6; Task 2 = §5.2; Task 3 = §5.3 + invariants §7.3-7.9; Task 4 = §5.4; Task 5 = §5.5. Determinism (§6) tested in Task 1 Step 8. All 10 spec tests mapped across tasks.
- Type consistency: `ReconcileKey`, `RbsrMessage`, `RbsrStep`, `RangeFingerprint` all sourced from the existing generic modules; voting adds only the source impl, seal/open, and engine halves.
- No archival-semantics change: Task 1 Step 6 only mirrors the index to `events`; it does not alter what `archive_finalized_polls` prunes.
