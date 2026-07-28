# ZEB-805 Community Sync Stall Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A community-state publish whose CAS blob cannot be fetched is retried under an adequate, caller-declared budget instead of dropped terminally — and a node that is receiving-and-discarding is distinguishable from a quiet one from its own vantage.

**Architecture:** Three changes on one causal chain. (1) `ContentStore` gains a per-call budget so state-root fetches stop inheriting a 500 ms default tuned for small reads. (2) `community_state_sync` and `mint_sync` adopt `fleet_sync`'s existing ZEB-705 bounded-retry pattern instead of dropping. (3) `network_health_snapshot` gains per-community `lastInboundMs` / `lastAdvanceMs`, whose divergence is the drop-loop signature.

**Tech Stack:** Rust (tokio, async-trait, zenoh), Svelte 5 + TypeScript frontend, `cargo nextest` / `vitest`.

**Spec:** `docs/superpowers/specs/2026-07-28-zeb-805-community-sync-stall-design.md` — read §2 (root cause), §4 (decisions of record), §5-8 (components) before starting.

## Global Constraints

- Base branch `zeb-805-community-sync-stall` off `main @ c91087d9`.
- `STATE_ROOT_FETCH_TIMEOUT_MS = 5_000`; `DEFAULT_FETCH_TIMEOUT_MS` stays `500`.
- Retry constants are reused **verbatim** from `fleet_sync.rs` — `FETCH_RETRY_ATTEMPTS = 3`, `FETCH_RETRY_DELAY_MS = 2000`, `FETCH_RETRY_MAX_INFLIGHT = 8`. Do not re-derive them; cross-engine consistency is the deliverable.
- INVARIANT: the state-root budget must stay strictly below `fetch_via_zenoh`'s internal 30 s deadline (`event_loop.rs:7575`). Pin it with a test.
- New DTO fields: additive, camelCase, `Option` / `#[serde(default)]`, serde-pinned with a snake-leak sweep. TS types hand-written — there is no `gen/` for `PeerHealth`-family types.
- Gates per task: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, and `scripts/test-select --context task`. Paste the printed `round=… bucket=…` line into the task record (rule 1601747). Cargo commands run from `src-tauri/`.
- Do not weaken the `#[cfg(any(test, feature = "test-fixtures"))]` gates on deterministic-nonce helpers.

---

### Task 1: Per-call fetch budget

**Files:**
- Modify: `src-tauri/src/content_store.rs` (trait + constant + `RuntimeContentStore` override)
- Modify: `src-tauri/src/community_state_sync.rs:3827`, `src-tauri/src/fleet_sync.rs:1365`, `src-tauri/src/mint_sync.rs:1030` (call sites)
- Test: `src-tauri/src/content_store.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `ContentStore::get_with_budget(&self, cid, budget) -> Result<Option<Vec<u8>>, ContentStoreError>` with a default body delegating to `get`; `pub const STATE_ROOT_FETCH_TIMEOUT_MS: u64 = 5_000`.
- Consumes: existing `CasOp::GetOrFetch { cid, timeout, reply }` — unchanged; the event-loop handler already reads `timeout` from the op, so **no `event_loop.rs` change is required for this task**.

- [ ] **Step 1: Write the failing test** — assert `get_with_budget` reaches `CasOp::GetOrFetch` carrying the caller's duration, not the store default. Mirror the existing `GetLocal` op-assertion harness at `content_store.rs:363-385`.

```rust
#[tokio::test]
async fn get_with_budget_passes_caller_budget_to_the_op() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let store = RuntimeContentStore::new(tx, std::time::Duration::from_millis(500));
    let want = ContentId::from_bytes([0x11; 32]);
    let budget = std::time::Duration::from_millis(5_000);
    let h = tokio::spawn(async move { store.get_with_budget(&want, budget).await });
    match rx.recv().await.expect("op sent") {
        CasOp::GetOrFetch { timeout, reply, .. } => {
            assert_eq!(timeout, budget, "op must carry the CALLER budget, not the store default");
            let _ = reply.send(Ok(None));
        }
        other => panic!("expected GetOrFetch, got {other:?}"),
    }
    let _ = h.await;
}

#[test]
fn state_root_budget_stays_under_the_zenoh_fetch_backstop() {
    // fetch_via_zenoh (event_loop.rs) wraps its reply loop in a 30s deadline.
    // A caller budget at or above it silently becomes that deadline.
    assert!(STATE_ROOT_FETCH_TIMEOUT_MS < 30_000);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(get_with_budget)'`
Expected: FAIL — `get_with_budget` does not exist.

- [ ] **Step 3: Add the constant and the trait method**

In `content_store.rs`, beside `DEFAULT_FETCH_TIMEOUT_MS`:

```rust
/// Fetch budget for state-root blobs (community / fleet / mint). A different
/// payload class from the 500 ms default: ~100 KB and growing with membership,
/// frequently fetched cross-WAN.
///
/// 5 s sits ~3x above a pessimistic real fetch (100 KB at 500 kbps ≈ 1.6 s plus
/// query round-trips at the fleet's worst measured 121 ms RTT), and well below
/// both `fetch_via_zenoh`'s 30 s backstop and the 450 s relay-pull cadence, so a
/// retry chain cannot overlap the next pass. See ZEB-805 §2.1-2.3.
pub const STATE_ROOT_FETCH_TIMEOUT_MS: u64 = 5_000;
```

On the `ContentStore` trait, immediately after `get_local` (same default-body idiom):

```rust
/// Like `get`, but with a caller-declared network budget instead of the store's
/// default. Callers fetching a known-large payload class (community / fleet /
/// mint state-root blobs) MUST use this — the default budget is tuned for
/// small, latency-sensitive reads, and a state root has not fit inside it since
/// communities grew past a few hundred members (ZEB-805).
///
/// INVARIANT: `budget` must stay below `fetch_via_zenoh`'s internal deadline
/// (`event_loop.rs`), which is the hard backstop; a larger budget silently
/// becomes that deadline.
///
/// The default body ignores the budget and delegates — correct for stores whose
/// `get` is already local-only (e.g. `InMemoryStub`).
async fn get_with_budget(
    &self,
    cid: &ContentId,
    budget: std::time::Duration,
) -> Result<Option<Vec<u8>>, ContentStoreError> {
    let _ = budget;
    self.get(cid).await
}
```

On `impl ContentStore for RuntimeContentStore`, override it — identical to `get` except the timeout source:

```rust
async fn get_with_budget(
    &self,
    cid: &ContentId,
    budget: std::time::Duration,
) -> Result<Option<Vec<u8>>, ContentStoreError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    self.cas_op_tx
        .send(CasOp::GetOrFetch { cid: *cid, timeout: budget, reply: reply_tx })
        .await
        .map_err(|e| ContentStoreError::Io(format!("cas_op channel closed: {e}")))?;
    reply_rx
        .await
        .map_err(|e| ContentStoreError::Io(format!("cas_op reply dropped: {e}")))?
}
```

> Match the exact error-string and field idiom of the adjacent `get` impl rather than the sketch above if they differ.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(get_with_budget) or test(state_root_budget)'`
Expected: PASS.

- [ ] **Step 5: Switch the three call sites, and log size against budget**

At each of `community_state_sync.rs:3827`, `fleet_sync.rs:1365`, `mint_sync.rs:1030`, replace
`content_store.get(&payload.root_cid)` with
`content_store.get_with_budget(&payload.root_cid, Duration::from_millis(STATE_ROOT_FETCH_TIMEOUT_MS))`.

In each miss arm, add the size and budget to the existing `warn!`:

```rust
blob_bytes = payload.root_cid.payload_size(),
budget_ms = STATE_ROOT_FETCH_TIMEOUT_MS,
```

`ContentId` carries the payload size before the fetch (`harmony-content/src/cid.rs:491-499`). A line reading `blob 109151 B under a 500 ms budget` is self-evidently wrong on sight; its absence is why the incident took three nodes and a night.

- [ ] **Step 6: Gates + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd .. && scripts/test-select --context task
git add -A && git commit -m "ZEB-805: per-call CAS fetch budget; state roots stop inheriting the 500ms default"
```

---

### Task 2: Bounded retry in `community_state_sync`

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs`
- Reference (read, do not modify): `src-tauri/src/fleet_sync.rs:66-81, 358-359, 645, 867, 1240-1290`
- Test: `src-tauri/src/community_state_sync.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Consumes: `STATE_ROOT_FETCH_TIMEOUT_MS` (Task 1).
- Produces: `IncomingOutcome::FetchMiss(Vec<u8>)`; engine counters `fetch_retries_scheduled`, `fetch_retries_dropped`, `fetch_retries_exhausted`, `fetch_retry_inflight_peak` with `pub(crate)` accessors — Task 4 reads these.

- [ ] **Step 1: Write the failing tests**

Three behaviours, mirroring `fleet_sync`'s at `:3083`, `:3146`, `:3353`:

1. `retry_succeeds_once_blob_becomes_fetchable` — first `get_with_budget` returns `Ok(None)`, a later one returns the bytes; assert the CRDT merged and `fetch_retries_scheduled == 1`.
2. `retry_exhaustion_drops_and_reports_degraded` — always `Ok(None)`; assert exactly `1 + FETCH_RETRY_ATTEMPTS` fetch attempts, `fetch_retries_exhausted == 1`, and one `report_degraded` with class `blob_not_found`.
3. `retry_flood_shield_caps_inflight_sleepers` — `FETCH_RETRY_MAX_INFLIGHT + 4` distinct publishers all missing; assert `fetch_retry_inflight_peak() <= FETCH_RETRY_MAX_INFLIGHT` and `fetch_retries_dropped >= 4`.

Plus the invariant pin, which is the one that makes retry safe:

4. `tracker_stays_unadvanced_across_miss_and_exhaustion` — after a CAS miss, and again after retry exhaustion, re-delivery of the *same* frame is still admitted (not `Duplicate`). This is currently incidental (`:3806-3810`, ZEB-750 — every early return drops the `CommitTicket`); make it explicit.

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(retry_succeeds_once) or test(retry_exhaustion) or test(retry_flood_shield) or test(tracker_stays_unadvanced)'`
Expected: FAIL — `FetchMiss` does not exist.

- [ ] **Step 3: Add the outcome variant and the retry machinery**

Copy the shape from `fleet_sync.rs` rather than inventing it:

- constants: `FETCH_RETRY_ATTEMPTS: u8 = 3`, `FETCH_RETRY_DELAY_MS: u64 = 2000`, `FETCH_RETRY_MAX_INFLIGHT: usize = 8`, each with the doc comment explaining what it bounds;
- engine fields: `fetch_retry_tx` / `fetch_retry_rx` (`mpsc::channel(FETCH_RETRY_MAX_INFLIGHT)`) and `fetch_retry_sem: Arc<Semaphore>`;
- `handle_incoming_publish`'s CAS-miss arm returns `IncomingOutcome::FetchMiss(wire)` instead of `ErrPreMutation(BlobNotFound)`, carrying the wire bytes;
- the engine loop's outcome match gains a `FetchMiss` arm that: acquires a permit with `try_acquire_owned()` **before** spawning (so detached sleepers, each retaining a wire buffer, are hard-capped), records `inflight_peak`, spawns a task holding the permit for its whole lifetime, sleeps `FETCH_RETRY_DELAY_MS`, then `try_send((wire, attempts_left - 1))` — never a blocking `send().await`;
- the engine loop gains a `fetch_retry_rx.recv()` select arm re-entering `handle_incoming_publish` with the decremented budget;
- on `attempts_left == 0`: the existing terminal drop path, keeping the existing `report_degraded`.

**Do NOT touch the replay-tracker handling.** It is already correct for retry (un-advanced on every early return) and Step 1's test 4 pins that.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_state_sync)'`
Expected: PASS, including the pre-existing suite.

- [ ] **Step 5: Mutation-check the flood shield**

Temporarily change `try_acquire_owned()` to an unbounded spawn and confirm test 3 fails; revert. A flood-shield test that passes without the shield is worthless.

- [ ] **Step 6: Gates + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd .. && scripts/test-select --context task
git add -A && git commit -m "ZEB-805: bounded blob re-fetch in community_state_sync (adopts fleet_sync's ZEB-705 pattern)"
```

---

### Task 3: `mint_sync` sweep

**Files:**
- Modify: `src-tauri/src/mint_sync.rs:1030-1044`
- Test: `src-tauri/src/mint_sync.rs` (inline `#[cfg(test)]`)

**Interfaces:** consumes Task 1's budget and Task 2's retry shape.

`mint_sync`'s current miss arm is the worst of the three engines: it `return Ok(())`, so the caller cannot distinguish a swallowed fetch failure from success.

- [ ] **Step 1: Write the failing test** — a CAS miss must be observable to the caller (a distinct return/outcome, not `Ok(())`), and must schedule a bounded retry.
- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Apply Task 2's pattern.** If `mint_sync`'s loop shape cannot host the same retry machinery without a disproportionate refactor, the minimum acceptable outcome is: stop returning bare `Ok(())`, surface the miss to the caller, and record why the retry was not adopted in the task record. Do not silently leave it swallowed.
- [ ] **Step 4: Run tests to verify they pass.**
- [ ] **Step 5: Gates + commit**

```bash
git commit -m "ZEB-805: mint_sync stops swallowing CAS fetch misses"
```

---

### Task 4: Sync-advance observability

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (record the two stamps)
- Modify: `src-tauri/src/network_health.rs` (DTO + tier + merge)
- Modify: `src/lib/types.ts` or the `PeerHealth`-adjacent hand-written TS types
- Test: `src-tauri/src/network_health.rs` (inline), `src/lib/__tests__/`

**Interfaces:**
- Consumes: Task 2's counters.
- Produces: `communitySync: CommunitySyncHealth[]` on the `network_health_snapshot` DTO.

**The load-bearing design point (spec §7.1):** track **both** stamps. `lastInboundMs` advancing while `lastAdvanceMs` stays frozen *is* the drop-loop signature. Either alone is insufficient — inbound alone cannot distinguish "applying fine" from "dropping everything"; advance alone cannot distinguish "wedged" from "genuinely quiet".

- [ ] **Step 1: Write the failing test — the incident replay**

```rust
#[test]
fn receiving_and_discarding_renders_dark_not_healthy() {
    // ZEB-805 incident replay: publishes arriving and being dropped.
    // lastInboundMs advances; lastAdvanceMs is frozen at boot.
    let now = 1_800_000_000_000u64;
    let row = community_sync_row(CommunitySyncInput {
        community_id: cid(1),
        last_inbound_ms: Some(now - 10_000),        // publishes ARE arriving
        last_advance_ms: Some(now - 5_400_000),     // ...and none has merged in 90 min
        has_peers: true,
        ..Default::default()
    }, now);
    assert_eq!(row.staleness.as_deref(), Some("dark"));
    assert_eq!(row.last_inbound_ms, Some(now - 10_000));
}
```

Plus: `quiet`/`fresh` boundaries; `staleness == None` when the community has no peers to sync with (mirrors ZEB-804's `null`-under-`noConnection` rule); serde round-trip + snake-leak sweep on the new fields.

- [ ] **Step 2: Run to verify it fails.**

- [ ] **Step 3: Record the two stamps in the engine.**
  `last_inbound_ms` — stamped on every inbound publish received, **before** any outcome branch.
  `last_advance_ms` — stamped **only** where the state actually merges (the step-14 mutation point).
  Both `AtomicU64`, `pub(crate)` accessors, `0` meaning never.

- [ ] **Step 4: Add the DTO, the tier, and the merge.**
  Reuse ZEB-804's `STALENESS_QUIET_MS` / `STALENESS_DARK_MS` and its `fresh`/`quiet`/`dark` vocabulary — one staleness idiom across the whole surface, not two. Derive the tier from `last_advance_ms`.

- [ ] **Step 5: Extend the TS types** to match, hand-written, camelCase.

- [ ] **Step 6: Mutation-check the tier.** Derive it from `last_inbound_ms` instead and confirm the replay test fails; revert. That mutation is exactly the bug the ticket is about — if the test survives it, the test is not testing anything.

- [ ] **Step 7: Gates + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd .. && npx tsc --noEmit && npx vitest run && scripts/test-select --context task
git add -A && git commit -m "ZEB-805: per-community sync-advance staleness — lastInboundMs vs lastAdvanceMs"
```

---

### Task 5: Adjacent corrections

**Files:**
- Modify: `src-tauri/src/content_store.rs` (2 comment sites), `src-tauri/src/event_loop.rs` (1 comment site + the log line), `src-tauri/src/community_state_sync.rs` (1 comment site)

- [ ] **Step 1: Rewrite the four false-premise comments** (spec §2.4 lists them with exact locations). Each currently justifies dropping with "the next state-root from any peer recovers". State what is now true: a miss is retried under a bounded budget, then dropped. **Delete the eventual-consistency reasoning — it is the claim this incident falsified**, and leaving it in place invites the next reader to restore the old behaviour.

- [ ] **Step 2: Fix the unearned-reassurance log line** at `event_loop.rs:3663`:

```
"startup root query: no responder — retrying with backoff; live push also catches up on next gateway publish"
```

Both clauses were false in the incident. Reword to assert only what the code guarantees. A log line that claims a fallback covers the failure actively suppresses investigation — that is the defect, not the wording.

- [ ] **Step 3: Resolve the root-query forever-retry contract.** The driver is documented as re-invoking "forever (600 s cap)"; observed behaviour was 11 attempts then stop. Trace the driver and determine which is wrong.

  **If the code is correct and the comment is wrong, fixing the comment is the whole fix.** Do not invent a retry loop to match a comment. Record the finding either way — the ticket asks for "fix the driver **or** correct the comment", and which one it turned out to be is itself the answer.

- [ ] **Step 4: Full gates + commit**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd .. && npx tsc --noEmit && npx vitest run
git commit -m "ZEB-805: correct the four eventual-consistency comments and the root-query log line"
```

Task 5 ends with the **full** CI-parity sweep, not `test-select` — this is the pre-PR gate.
