# ZEB-718 Voting Backfill + Persistence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the community voting subsystem restart-durability (persist `VotingLog`, replay on boot) and peer-to-peer anti-entropy (a full-dump backfill pull that recovers events missed on the live topic, including ZEB-717 cross-rotation drops).

**Architecture:** Mirror channel-log's backfill *structure* (Zenoh queryable + engine-supplied read closure + a per-engine driver) but the voting adapter's *crypto* (re-encrypt served events under the current epoch — which doubles as backfill access control, preserving the ZEB-717 cut). Backfill apply dedups by exact event coordinate (not the per-lane high-water tracker) so in-lane gaps recover. Persist the serde-clean subset `{events, policy, poll_restore}` and replay `events` to rebuild materialized tally/conviction — with the per-poll `poll_restore` overlay reapplied after replay to restore tick-driven lifecycle state that events alone don't reconstruct (see Task 1 and design §D3).

**Tech Stack:** Rust (tokio, zenoh, ciborium/serde, ChaCha20-Poly1305 via existing `community_state_sync` seams). Design: `docs/specs/2026-07-19-zeb-718-voting-backfill-and-persistence-design.md`.

## Global Constraints

- Gates (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend unaffected (no TS/Svelte changes expected).
- Iterative dev gates use `scripts/test-select --context task` (k=4) then `--context round` for converge; **paste the emitted `round=… bucket=…` summary line into the task report** so the selective run is auditable (CLAUDE.md convention). Final pre-PR sweep is the full `--workspace --all-targets` run (CI-parity; not selective).
- **Engine stays wire-agnostic** (ZEB-717 invariant): the `VotingLogEngine` never encrypts/decrypts; all crypto lives in the adapter (`event_loop.rs`). The engine hands out and receives *plaintext* `SignedVotingEvent` CBOR.
- **At-rest = plaintext CBOR** (codebase convention; events are signed → tamper-evident). No per-community at-rest key.
- **Keychain safety:** any test touching identity persistence sets `HARMONY_PASSPHRASE` and injects `keychain: None`; never construct `KeychainStore::new()` from test-reachable code (CLAUDE.md ZEB-428).
- Preserve the **self-loopback ordering invariant**: `tracker.record(&event)` (and the new `seen_coords.insert`) must run **before** `publisher_tx.send` in `publish_event` (`community_voting_log_engine.rs:1551`).

## Reference map (from codebase exploration — exact anchors)

- `VotingLog`: `community_voting_log.rs:56` (`pub events: Vec<SignedVotingEvent>`, `polls`, `delegation_graph`, private `policy`; accessor `policy()`). `apply_with_snapshot`: `:305`. `archive_finalized_polls`: `:1191`.
- `SignedVotingEvent`: `community_voting_core.rs:905` (serde+CBOR ready). `Hlc`: `owner_state_types.rs:318` (`wall_ms:u64, logical:u32, device_id:String`). `OwnerAddr`: `:364`. `CommunityVotingPolicy`: `community_voting_conviction.rs:137` (serde).
- Engine: `community_voting_log_engine.rs`. `VotingReplayTracker`: `:152` (`seen: HashMap<(OwnerAddr,String),(u64,u32)>`, `contains`/`record` `:169-188`). `VotingLogEngineParams`: `:198`. `publish_event`: `:1533` (record@`:1551`, apply@`:1639`, send@`:1691`). `process_inbound`: `:2406` (dedup@`:2420`, apply@`:2461`, record@`:2471`). `start`: `:317`.
- Registry field: `NodeState.voting_logs` `lib.rs:1132`; `voting_log_engines` `:1171`. `ensure_voting_engine_for`: `lib.rs:47851`. `NodeStateMembershipResolver`: `lib.rs:47810` (`snapshot_at(community_id, hlc)`).
- Adapter: `VotingLogAdapterRequest`: `event_loop.rs:167`. `spawn_voting_log_zenoh_adapter`: `:9363` (topic `:9373`, encrypt-on-put `:9421-9430`, current-epoch cut + decrypt `:9553-9612`, `MAX_VOTING_PAYLOAD_BYTES=64*1024` `:9540`). Adapter drain in event loop select!: `:6651`.
- Crypto seams: `community_state_sync.rs` — `VOTING_TOPIC_AAD` `:386`; `EncryptedEnvelope{epoch,nonce,ciphertext,ratchet_generation}` `:323`; `encrypt_for_topic_with_aad` `:416`; `decrypt_for_topic_with_aad` `:471`.
- **Channel-log pattern to mirror:** persist `community_state_persist.rs` (`write_atomic` `:213`, `save_crdt`/`load_crdt` `:64/:89`, `quarantine_corrupted` `:165`). Backfill adapter `spawn_channel_log_zenoh_adapter` `event_loop.rs:9661` (since queryable `:9846-9920`, `ConsolidationMode::None` rationale `:10102-10109`, `Locality::Remote` `:10678`). `read_for_query` closure `community_channel_log_engine.rs:2557`. Driver `channel_backfill.rs` (`BackfillLatch` `:159`, `run_backfill_driver` `:548`, `BACKFILL_RETRY_BASE_MS=30_000`/`CAP_MS=600_000`, periodic `:329`). Re-arm signals: `transport_epoch_rx` (peer_liveness `on_transport_up`), wired for channel-log at `community_channel_log_engine.rs:2859-2878`. Boot reconcile `reconcile_from_state` `community_channel_log_engine.rs:3032`, called `lib.rs:7954-7986`; community enumeration `lib.rs:7830-7869` (`OwnerState.spaces`, Community, `left_at.is_none()`).
- Atomic-write helper for a fsync-strong variant if needed: `owner_state_persist::save_atomically` `owner_state_persist.rs:40`. Data dir: `resolve_identity_dir()` (`owner_commands`), per-community root `identity_dir/communities/{id_hex}/`.

---

### Task 1: Voting-log persistence module

**Files:**
- Create: `src-tauri/src/community_voting_persist.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod community_voting_persist;` near the other `mod` decls)
- Test: inline `#[cfg(test)]` in the new module.

**Interfaces:**
- Consumes: `VotingLog` (`community_voting_log.rs:56`, `pub events`, `policy()`), `SignedVotingEvent`, `CommunityVotingPolicy`, `SpaceId`.
- Produces:
  - `pub struct PersistError` (enum: `Io(String)`, `Decode(String)`, `Version(u8)`, `CommunityIdMismatch)` — mirror `community_state_persist::PersistError`.
  - `pub fn voting_path_for(identity_dir: &Path, community_id: &SpaceId) -> PathBuf` → `communities/{id_hex}/voting.cbor`. The helper owns the `SpaceId`→hex conversion (never accept a preformatted `&str`) so `voting.cbor` can never diverge from the sibling `crdt.cbor`.
  - `pub fn save_voting_log(path: &Path, log: &VotingLog, community_id: &SpaceId) -> Result<(), PersistError>` (sync convenience wrapper). Hot paths use the split form `snapshot_for_persist(log, id) -> VotingLogSnapshot` (clone under the lock) + `write_snapshot(path, &snapshot)` (blocking encode + atomic write, run under `spawn_blocking`) so disk I/O never parks a Tokio worker or holds a log lock (repo pattern — PRs #74/#380/#381).
  - `pub fn load_voting_log(path: &Path, expected_id: &SpaceId) -> Result<(Vec<SignedVotingEvent>, CommunityVotingPolicy, HashMap<PollId, PollRestore>), PersistError>`. **Missing file** → `Ok((vec![], default, empty))`. **Decode / version / community_id mismatch** → quarantine aside + return the empty default (self-heals; peer-recoverable). **Other I/O error on an existing file** → `Err` (the file is present but temporarily unreadable — the caller must NOT arm persistence and clobber it with empty). `poll_restore` (a `PollId → PollRestore` overlay) carries tick-driven state replay can't reconstruct.

**Design notes:** Serialize a versioned record. `SpaceId` hex: reuse the existing `id_hex` conversion used at `community_state_sync.rs:4870` (`paths_for`) — match it exactly so paths align with `crdt.cbor`. Plaintext CBOR. Atomic write = temp + rename (mirror `community_state_persist::write_atomic` `:213`); dir-fsync not required (peer-recoverable), but reuse `owner_state_persist::save_atomically` if you prefer the fsync-strong path — either is acceptable; match `community_state_persist` for consistency with the sibling `crdt.cbor`.

- [ ] **Step 1: Write the failing test** — round-trip + version + quarantine.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // helper: build a VotingLog with N events via VotingLog::apply / a test builder.
    // If a test event builder already exists in community_voting_log tests, reuse it;
    // otherwise construct SignedVotingEvent fixtures the same way those tests do.

    #[test]
    fn save_then_load_round_trips_events_and_policy() {
        let dir = tempfile::tempdir().unwrap();
        let cid = /* deterministic test SpaceId */;
        let path = voting_path_for(dir.path(), &cid);
        let mut log = VotingLog::default();
        // apply a Tier-1 PollCreate + a couple of ballots so events is non-empty; set a non-default policy.
        // (use the same fixture path community_voting_log.rs tests use)
        save_voting_log(&path, &log, &cid).unwrap();
        let (events, policy) = load_voting_log(&path, &cid).unwrap();
        assert_eq!(events, log.events);
        assert_eq!(policy, log.policy().clone());
    }

    #[test]
    fn load_wrong_community_id_quarantines_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cid_a = /* SpaceId A */; let cid_b = /* SpaceId B */;
        let path = voting_path_for(dir.path(), &cid_a);
        save_voting_log(&path, &VotingLog::default(), &cid_a).unwrap();
        let (events, _policy) = load_voting_log(&path, &cid_b).unwrap(); // mismatch
        assert!(events.is_empty());
        // a .corrupt.* sibling exists
        let quarantined = std::fs::read_dir(path.parent().unwrap()).unwrap()
            .filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().contains(".corrupt."));
        assert!(quarantined);
    }

    #[test]
    fn load_bad_version_byte_quarantines() {
        let dir = tempfile::tempdir().unwrap();
        let cid = /* SpaceId */; let path = voting_path_for(dir.path(), &cid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, [0xFFu8, 1, 2, 3]).unwrap(); // unknown version prefix
        let (events, _) = load_voting_log(&path, &cid).unwrap();
        assert!(events.is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_persist)'` → FAIL (module/functions absent).
- [ ] **Step 3: Implement the module** — `PersistedVotingLog { version, community_id, events, policy }` (serde); `VOTING_LOG_SCHEMA_V1: u8 = 1` written as a 1-byte prefix ahead of the CBOR body (mirror `community_state_persist`'s prefix scheme exactly); `save_voting_log` builds the record from `log.events.clone()` + `log.policy().clone()`, writes `[V1] ++ cbor(record)` atomically; `load_voting_log` reads prefix → checks version → `ciborium::from_reader` → checks `record.community_id == *expected_id` → returns `(events, policy)`; on any error, `quarantine_corrupted(path)` (rename to `path.with_extension("corrupt.<unix_ms>")` — pass a millis arg in or reuse the codebase helper) and return `(vec![], CommunityVotingPolicy::default())`.
- [ ] **Step 4: Run tests to verify they pass.**
- [ ] **Step 5: Gate + commit** — `scripts/test-select --context task`; then `cargo fmt --all`; `git add -A && git commit`.

```bash
git add src-tauri/src/community_voting_persist.rs src-tauri/src/lib.rs
git commit -m "ZEB-718: voting-log persistence module (save/load/quarantine, versioned, id-checked)"
```

---

### Task 2: Coordinate dedup + `apply_backfilled_event`

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`VotingReplayTracker` + new engine method)
- Test: inline `#[cfg(test)]`.

**Interfaces:**
- Consumes: `VotingLog::apply_with_snapshot`, `MembershipSnapshotResolver::snapshot_at`, `verify_voting_event` + `inbound_eligibility_check` (whatever `process_inbound` calls, `:2406-2465`).
- Produces:
  - `VotingReplayTracker` additive field `seen_coords: HashSet<VotingEventCoord>` where `type VotingEventCoord = (OwnerAddr, String, u64, u32)` = `(actor, device_id, wall_ms, logical)`, plus `fn seen_coord(&self, ev) -> bool` and `fn record_coord(&mut self, ev)`.
  - `pub(crate) async fn apply_backfilled_event(self: &Arc<Self>, plaintext: &[u8]) -> Result<Option<PollId>, String>`.

**Design notes:** `seen_coords` is additive — the live `contains`/`record` high-water logic is unchanged, so the live inbound path and its tests are untouched. Both `publish_event` and `process_inbound` gain a `record_coord(&event)` call adjacent to their existing `tracker.record(&event)` (preserving the pre-broadcast ordering in `publish_event`). `apply_backfilled_event` mirrors `process_inbound` (`:2406-2477`) but swaps the `tracker.contains` gate for `tracker.seen_coord`, and does **not** fire post-apply orchestration hooks (backfilled events are historical; match the inbound path which already suppresses the orchestration cascade, comment `:2592-2603`).

- [ ] **Step 1: Write the failing test** — dedup skip + in-lane-gap apply.

```rust
#[tokio::test]
async fn apply_backfilled_skips_already_applied_coordinate() {
    let engine = /* start a test engine with a stub membership resolver that returns a Joined snapshot */;
    let ev = /* a valid SignedVotingEvent (PollCreate) authored by a member */;
    // first apply through the normal inbound path
    engine.process_inbound_for_test(&cbor(&ev)).await.unwrap();
    // backfilling the same event is a no-op
    let r = engine.apply_backfilled_event(&cbor(&ev)).await.unwrap();
    assert!(r.is_none());
    assert_eq!(engine.voting_log_len().await, 1); // no double-append
}

#[tokio::test]
async fn apply_backfilled_recovers_in_lane_gap_the_high_water_tracker_would_drop() {
    let engine = /* test engine + resolver */;
    // Two events on the SAME lane (same actor+device), e1 older than e2.
    let e1 = /* PollCreate @ hlc(wall=100, logical=0, device="d") by actor A */;
    let e2 = /* a follow-up event @ hlc(wall=200, logical=0, device="d") by A, valid after e1 */;
    // Simulate the cross-rotation drop: receive e2 first (e1 was dropped on the live cut).
    engine.process_inbound_for_test(&cbor(&e2)).await.ok(); // may be gated if it depends on e1; if so, use two independent polls
    // The live tracker's high-water for (A,"d") is now >= e2; a live re-delivery of e1 would be dropped.
    // Backfill MUST still apply e1:
    let r = engine.apply_backfilled_event(&cbor(&e1)).await.unwrap();
    assert!(r.is_some(), "in-lane gap e1 must be recovered by coordinate dedup");
}
```

> Implementer note: if `e2` is gated on `e1` (lifecycle), make `e1`/`e2` two *independent* polls' PollCreate events on the same device lane — the point is only that `e1.hlc < e2.hlc` on the same `(actor, device)` lane so the high-water tracker would drop `e1` but coordinate dedup does not. Reuse `process_inbound_for_test` (`:2884`) and whatever stub resolver the existing engine tests use.

- [ ] **Step 2: Run tests → FAIL** (`apply_backfilled_event` absent).
- [ ] **Step 3: Implement** — add `seen_coords` + helpers to `VotingReplayTracker`; call `record_coord` alongside every `record` (publish@`:1551`, inbound@`:2471`); write `apply_backfilled_event` as a `process_inbound` clone with the `seen_coord` gate and no orchestration hooks. Add small test accessors `voting_log_len()` / `voting_log_has_coord()` behind `#[cfg(test)]` if not already present.
- [ ] **Step 4: Run tests → PASS.**
- [ ] **Step 5: Gate + commit** — `scripts/test-select --context task`; fmt; commit `"ZEB-718: coordinate-dedup backfill apply path (recovers in-lane cross-rotation gaps)"`.

---

### Task 3: Persist hook + `identity_dir` threading

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`VotingLogEngineParams` + `VotingLogEngine` gain `identity_dir`; `persist_now()`; call sites)
- Modify: `src-tauri/src/lib.rs` (`ensure_voting_engine_for` passes `identity_dir`)
- Test: inline engine test + a lib-level test if convenient.

**Interfaces:**
- Consumes: Task 1 `save_voting_log`, `voting_path_for`.
- Produces: `VotingLogEngineParams.identity_dir: std::path::PathBuf`; `VotingLogEngine` stores it; `async fn persist_now(&self)` (locks `voting_log`, calls `save_voting_log`, `warn!` on error, never panics).

**Design notes:** Call `persist_now()` after each mutation: end of `publish_event` (after apply, `:1652`), end of `process_inbound` success (after `:2465`), end of `apply_backfilled_event`, after `archive_finalized_polls` wherever the tick invokes it, and after the policy-set IPC. `ensure_voting_engine_for` (`lib.rs:47851`) already has `identity_dir` in scope via the caller? — if not, add a param and thread `resolve_identity_dir()` from callers (`VotingEngineNodeHandles` extract site). Keep the write off the hot path only if profiling demands; at voting volume a synchronous atomic write per mutation is fine.

- [ ] **Step 1: Write the failing test.**

```rust
#[tokio::test]
async fn publish_persists_voting_log_to_disk_and_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let cid = /* SpaceId */;
    let engine = /* start engine with identity_dir = dir.path() */;
    let ev = /* valid PollCreate */;
    engine.publish_event(ev.clone(), None).await.unwrap();
    let path = crate::community_voting_persist::voting_path_for(dir.path(), &cid);
    assert!(path.exists());
    let (events, _policy) = crate::community_voting_persist::load_voting_log(&path, &cid).unwrap();
    assert_eq!(events, vec![ev]);
}
```

- [ ] **Step 2: Run → FAIL** (no `identity_dir` param / no persist).
- [ ] **Step 3: Implement** — thread `identity_dir`; add `persist_now`; wire the five call sites. Update every `VotingLogEngineParams { .. }` construction (prod at `ensure_voting_engine_for`; tests) to pass an `identity_dir` (tests can pass a tempdir; add a test-default helper to avoid touching all 27 bridge tests — e.g. default the field to a `std::env::temp_dir()`-based path only under `#[cfg(test)]`, or add a `params_for_test` constructor). **Prefer** a builder/default so the ~27 mpsc-bridge tests don't each need editing.
- [ ] **Step 4: Run → PASS**, and run the voting engine test family to confirm no bridge-test breakage: `scripts/test-select --context task` then `cargo nextest run --locked --features test-fixtures -E 'test(voting)'`.
- [ ] **Step 5: Gate + commit** — `"ZEB-718: persist voting log after each mutation (identity_dir + persist_now hook)"`.

---

### Task 4: Boot reconcile — load + replay + ensure-engine

**Files:**
- Modify: `src-tauri/src/lib.rs` (`reconcile_voting_from_state` + call in the boot loop `:7871-7987`)
- Test: `src-tauri/tests/community_voting/…` (new boot-replay test) or an inline `start_node`-adjacent test if the harness supports it.

**Interfaces:**
- Consumes: Task 1 `load_voting_log`; `VotingLog::apply_with_snapshot`; `NodeStateMembershipResolver::snapshot_at` (`lib.rs:47810`); `ensure_voting_engine_for`.
- Produces: `async fn reconcile_voting_from_state(voting_logs, voting_log_engines, identity_dir, community_id, resolver, ...engine params...) -> Result<(), String>`.

**Design notes:** For each community (already enumerated as `community_snapshots`, `:7830`), after its sync engine is reconciled (so membership-at-HLC is available): `load_voting_log(voting_path_for(identity_dir, id), id)` → build `VotingLog`, `set_policy(policy)` → for each event in stored order, `let snap = resolver.snapshot_at(id, event.hlc()).await.ok(); log.apply_with_snapshot(event.clone(), &id, snap).ok();` (skip+log per-event errors) → overlay `poll_restore` (restore each poll's `meta` + Tier-2 timing) → insert `Arc::new(Mutex::new(log))` into `voting_logs` (via `entry().or_insert_with`, so a policy-only log with zero events is still inserted).

> **As-built (reload timing):** reconcile runs **lazily on first voting access** for a community (invoked at the head of `ensure_voting_engine_for`), not from an eager boot loop. Restart-durability holds either way — the first IPC/tick/inbound touch for a community reloads-then-attaches before any mutation. Eager boot-spawn (pre-warming every community's engine at `start_node`) is a deferred enhancement (design §7); it would only change *when* the reload happens, not *whether*. If the reload hits an I/O error on an existing file, `ensure_voting_engine_for` leaves persistence **disarmed** for that session rather than overwriting the unreadable file with empty state.

- [ ] **Step 1: Write the failing test** — persist a log, drop in-memory state, reconcile, assert rebuild.

```rust
#[tokio::test]
async fn boot_reconcile_replays_persisted_voting_log() {
    let dir = tempfile::tempdir().unwrap();
    let cid = /* SpaceId with a persisted crdt so the resolver can answer */;
    // 1. Build a VotingLog with a finalized Tier-1 poll + tally; save it.
    // 2. Fresh empty voting_logs map.
    // 3. reconcile_voting_from_state(...) with a resolver over a seeded crdt_state.
    // 4. Assert voting_logs[cid] materialized poll count + tally == pre-persist.
}
```

> Implementer note: this needs a membership resolver backed by a seeded `crdt_state` + community registry. Reuse the seeding helper the ZEB-717 integration test uses (`community_voting_zenoh_integration.rs`) or the `NodeStateMembershipResolver` test double. If a full boot harness is too heavy, test `reconcile_voting_from_state` directly (it is the unit of value); a full `start_node` e2e is not required here.

- [ ] **Step 2: Run → FAIL.**
- [ ] **Step 3: Implement** `reconcile_voting_from_state` and call it in the boot loop after the channel-log reconcile (`:7986`). Thread the engine params already assembled there.
- [ ] **Step 4: Run → PASS.**
- [ ] **Step 5: Gate + commit** — `"ZEB-718: boot reconcile — load + replay persisted voting log, eager engine spawn"`.

---

### Task 5: Backfill responder — adapter queryable (encrypt-on-serve@current)

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`read_for_backfill` accessor/closure)
- Modify: `src-tauri/src/event_loop.rs` (`VotingLogAdapterRequest` + `spawn_voting_log_zenoh_adapter`)
- Modify: `src-tauri/src/lib.rs` (`ensure_voting_engine_for` supplies the closure into the adapter request)
- Test: unit for `read_for_backfill`; the queryable itself is covered by Task 7 integration.

**Interfaces:**
- Produces:
  - Engine: `pub(crate) async fn read_backfill_frames(&self) -> Vec<Vec<u8>>` — plaintext `SignedVotingEvent` CBOR, one per live event (`self.voting_log.lock().events` cloned + encoded). (Live-only falls out: archived events are already pruned from `events`.)
  - `VotingLogAdapterRequest` gains `read_for_backfill: Arc<dyn Fn() -> Pin<Box<dyn Future<Output=Vec<Vec<u8>>> + Send>> + Send + Sync>` (mirror channel-log's closure boxing).
  - `spawn_voting_log_zenoh_adapter` declares a queryable on `harmony/community/{id_hex}/voting/backfill`.

**Design notes:** On a query, the adapter: `let frames = (read_for_backfill)().await;` then for each frame, `crdt_state.lock().await` → `spaces.get(&community_id)` → `encrypt_for_topic_with_aad(space, &frame, VOTING_TOPIC_AAD)` → `ciborium`-encode the envelope → `query.reply(key, envelope_bytes)`. Reply per-frame; missing epoch/space → reply nothing (drop, `debug`). Copy the channel-log queryable's reply loop and `ConsolidationMode::None` handling (`event_loop.rs:9846-9920`) but swap the encryption to the voting current-epoch seam. Do **not** hold the `crdt_state` lock across `read_for_backfill().await`.

- [ ] **Step 1: Write the failing test** — `read_backfill_frames` returns one plaintext frame per live event and decodes back to the events.

```rust
#[tokio::test]
async fn read_backfill_frames_returns_live_events_as_plaintext_cbor() {
    let engine = /* engine with 2 applied events, 0 archived */;
    let frames = engine.read_backfill_frames().await;
    assert_eq!(frames.len(), 2);
    let decoded: Vec<SignedVotingEvent> = frames.iter()
        .map(|f| ciborium::from_reader(f.as_slice()).unwrap()).collect();
    assert_eq!(decoded, engine.snapshot_events().await);
}
```

- [ ] **Step 2: Run → FAIL.**
- [ ] **Step 3: Implement** engine accessor, adapter queryable, and the closure plumbing through `ensure_voting_engine_for`. Add `community_id`/`crdt_state` are already on `VotingLogAdapterRequest` (ZEB-717); only `read_for_backfill` is new.
- [ ] **Step 4: Run → PASS** (unit); adapter queryable validated in Task 7.
- [ ] **Step 5: Gate + commit** — `"ZEB-718: backfill responder — voting/backfill queryable, encrypt-on-serve under current epoch"`.

---

### Task 6: Backfill requester — get-driver + re-arm

**Files:**
- Create: `src-tauri/src/community_voting_backfill.rs` (lean full-dump driver: spawn / transport-up / periodic / backoff)
- Modify: `src-tauri/src/event_loop.rs` (`spawn_voting_log_zenoh_adapter` requester get + current-epoch-cut decrypt → deliver plaintext to the engine's `apply_backfilled_event`)
- Modify: `src-tauri/src/lib.rs` (`ensure_voting_engine_for` spawns the driver; wires `transport_epoch_rx`)
- Test: unit for the driver's re-arm/backoff logic (pure); wire path in Task 7.

**Interfaces:**
- Consumes: Task 2 `apply_backfilled_event`; Task 5 queryable; `transport_epoch_rx` (`peer_liveness`).
- Produces: `pub async fn run_voting_backfill_driver(engine: Arc<VotingLogEngine>, request_pull: <trigger>, transport_epoch_rx: Option<watch::Receiver<u64>>, periodic_ms: u64)` and a `VotingBackfillLatch`-style pure retry helper (mirror `channel_backfill.rs` but no paging — a pull either applied ≥0 events or saw zero responders → backoff).

**Design notes — how a reply reaches `apply_backfilled_event`:** simplest is to keep the requester loop inside the adapter (it already owns the zenoh `Session` + `crdt_state` for the cut/decrypt), and give the adapter an `Arc<VotingLogEngine>` handle (or a delivery closure) so it can call `engine.apply_backfilled_event(&plaintext)` directly for each decrypted reply — no new mpsc. The driver task (in `community_voting_backfill.rs`, spawned from `ensure_voting_engine_for`) decides *when* to pull and signals the adapter (a `tokio::sync::Notify` or a bounded `mpsc<()>` "pull now" channel added to `VotingLogAdapterRequest`); the adapter performs the `session.get(voting/backfill).consolidation(None).allowed_destination(Remote)`, then per reply: decode envelope → current-epoch cut → `decrypt_for_topic_with_aad` → `engine.apply_backfilled_event(plaintext)`. Pull triggers: once at spawn, on `transport_epoch_rx` change, on the periodic interval; backoff on zero replies.

- [ ] **Step 1: Write the failing test** — pure driver logic: given "zero responders" the latch schedules a backoff; given "applied" it returns to idle; a transport-up signal re-arms from idle.

```rust
#[test]
fn latch_backs_off_on_zero_responders_then_rearms_on_transport_up() {
    let mut latch = VotingBackfillLatch::new(BASE_MS, CAP_MS);
    latch.on_pull_result(PullResult::NoResponders);
    assert!(latch.next_delay_ms() >= BASE_MS);
    latch.on_pull_result(PullResult::NoResponders);
    assert!(latch.next_delay_ms() >= BASE_MS * 2); // exponential
    latch.on_signal(ReArm::TransportUp);
    assert_eq!(latch.state(), LatchState::PullNow);
}
```

- [ ] **Step 2: Run → FAIL.**
- [ ] **Step 3: Implement** the pure latch + the async driver + the adapter requester get/decrypt/apply. Reuse `BACKFILL_RETRY_BASE_MS`/`CAP_MS` values (or re-declare voting-local constants). Register `mod community_voting_backfill;`.
- [ ] **Step 4: Run → PASS** (unit).
- [ ] **Step 5: Gate + commit** — `"ZEB-718: backfill requester — full-dump get-driver, current-epoch decrypt, transport-up + periodic re-arm"`.

---

### Task 7: Integration acceptance tests (three ZEB-718 criteria)

**Files:**
- Modify/Create: `src-tauri/tests/community_voting/community_voting_zenoh_integration.rs` (extend the two-session harness `:75`)

**Interfaces:**
- Consumes: the full stack from Tasks 3–6 running over two real `zenoh::Session`s with `spawn_voting_log_zenoh_adapter` on each; the epoch-seeding helper this file already uses (ZEB-717).

**Design notes:** Reuse the ZEB-717 test's node/session/epoch seeding. "Offline" = start B's adapter *after* A has published (or hold B's live subscriber and only run its backfill get). Rotation = advance B's `Space.current_epoch` to N+1 with `old_epoch_keys[N]` retained, exactly as the ZEB-717 post-kick+rotation test does.

- [ ] **Step 1: Write the three tests.**

```rust
#[tokio::test]
async fn backfill_recovers_events_missed_while_offline() {
    // A publishes P1..P3 while B's live delivery is not attached.
    // Attach B's engine+adapter, trigger a backfill pull.
    // Assert B materializes P1..P3.  (Criterion 1)
}

#[tokio::test]
async fn backfill_recovers_cross_rotation_dropped_vote_under_new_epoch() {
    // A publishes e1 under epoch N; B rotates to N+1 (retains old K(N)); B drops e1 on the live cut.
    // B backfill-pulls; responder (A or C) re-encrypts e1 under N+1; B applies e1.
    // Assert B materializes e1.  (Criterion 2 — re-encrypt@current + coordinate-dedup)
}

#[tokio::test]
async fn backfill_does_not_weaken_the_cut_for_a_kicked_rotated_identity() {
    // Kicked-then-rotated identity K (holds only K(N)) issues a backfill get.
    // Responder serves current-epoch (N+1) envelopes; K cannot decrypt → recovers nothing;
    // K cannot inject a current-epoch envelope either.
    // Assert K's log stays empty and the members' logs are unaffected.  (Criterion 3 / D5)
}
```

- [ ] **Step 2: Run → FAIL / iterate** until green.
- [ ] **Step 3: Refresh wire fixtures** if any voting fixture pins backfill bytes (reuse `EncryptedEnvelope` wire; assert round-trip, do not byte-pin the nondeterministic nonce).
- [ ] **Step 4: Full pre-PR sweep** — `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- [ ] **Step 5: Commit** — `"ZEB-718: integration acceptance — offline-miss, cross-rotation recovery, cut-not-weakened"`.

---

## Self-Review (completed)

- **Spec coverage:** Criterion 1 → Task 7#1 (+ driver Task 6, responder Task 5). Criterion 2 → Task 2 (coordinate dedup) + Task 5 (re-encrypt@current) + Task 7#2. Criterion 3 (cut not weakened) → D5 serve-encryption + Task 7#3. Persistence/restart (A+) → Tasks 1,3,4. All spec §4 files appear in a task.
- **Type consistency:** `VotingEventCoord = (OwnerAddr, String, u64, u32)` used in Tasks 2/6; `read_backfill_frames -> Vec<Vec<u8>>` (plaintext) consumed by the adapter in Task 5; `apply_backfilled_event(&[u8]) -> Result<Option<PollId>, String>` produced in Task 2, consumed in Task 6; `save/load_voting_log` signatures produced in Task 1, consumed in Tasks 3/4.
- **Placeholder scan:** test bodies carry `/* … */` for fixtures that must match existing test builders (deliberate — the exact `SignedVotingEvent`/`SpaceId` construction lives in the voting test modules and must be reused, not reinvented); every step names the concrete function/anchor to implement.
- **Ordering invariant** (self-loopback) called out in Global Constraints and Task 2/3.
