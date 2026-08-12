# ZEB-918 Rendezvous Live Epoch Key Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rendezvous beacons publish under the LIVE membership epoch key and member resolvers read live + one epoch back, replacing the spawn-pinned key on both sides (spec: `docs/superpowers/specs/2026-08-12-zeb918-rendezvous-live-epoch-key-design.md`).

**Architecture:** One new candidates helper in `community_state_sync.rs`; the publisher arm in `lib.rs` switches to `live_epoch_key` with spawn-key degrade; `GatewayDialCtx::epoch_key_of` becomes `epoch_key_candidates_of` (`None` still = engine unregistered); the ladder tries candidates in order. No wire, CRDT-schema, or IPC changes.

**Tech Stack:** Rust (src-tauri), tokio, existing `MockPkarrRelay` e2e harness from PR #657.

## Global Constraints

- Cargo commands run from `src-tauri/`; always `--locked --features test-fixtures`; clippy `--all-targets --no-deps -- -D warnings`; `cargo fmt --all -- --check`.
- Iterative gates may use `scripts/test-select --context task`; the final pre-PR sweep is the full `--workspace --all-targets` run.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D`.
- Publisher degrades / never skips; resolver candidate list is never empty and never more than 2 keys.

---

### Task 1: `epoch_key_candidates` helper

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (next to `community_publish_epoch_key`, ~line 3363)

**Interfaces:**
- Produces: `pub(crate) async fn epoch_key_candidates(community_id: SpaceId, crdt_state: Option<&Arc<Mutex<crate::owner_state_crdt::OwnerState>>>, fallback: &EpochKey) -> Vec<EpochKey>`

- [ ] **Step 1: Write failing tests** in the existing `#[cfg(test)]` module of `community_state_sync.rs` (follow the module's existing OwnerState-fixture style — see the tests around `live_epoch_key`):

```rust
#[tokio::test]
async fn candidates_no_crdt_state_falls_back_to_spawn_key() {
    let fb = EpochKey::new([0xaa; 32]);
    let got = epoch_key_candidates(test_space_id(), None, &fb).await;
    assert_eq!(got, vec![fb]);
}

#[tokio::test]
async fn candidates_pre_rotation_is_current_only() {
    // OwnerState with Space { current_epoch: Some(0), current_epoch_key: Some(K0), old_epoch_keys: {} }
    // → [K0]
}

#[tokio::test]
async fn candidates_post_rotation_is_current_then_previous() {
    // Space { current_epoch: Some(1), current_epoch_key: Some(K1), old_epoch_keys: {0: K0} }
    // → [K1, K0] (order pinned)
}

#[tokio::test]
async fn candidates_missing_archive_entry_is_current_only() {
    // Space { current_epoch: Some(2), current_epoch_key: Some(K2), old_epoch_keys: {0: K0} }
    // (no entry for epoch 1) → [K2]
}

#[tokio::test]
async fn candidates_incomplete_space_degrades_to_spawn_key() {
    // Space { current_epoch: None, current_epoch_key: None } → [fallback]
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo nextest run --locked --features test-fixtures -E 'test(candidates_)'` → FAIL (fn not defined)

- [ ] **Step 3: Implement**

```rust
/// ZEB-918: ordered membership-epoch key candidates for rendezvous beacon
/// RESOLUTION — the live current key first, then the immediately-previous
/// epoch's archived key (`Space.old_epoch_keys[current_epoch - 1]`) when one
/// exists. Never more than one epoch back. Falls back to `[fallback]` (the
/// engine's spawn-time key) when the live read is unavailable: this is
/// publisher-degrades coherence (ZEB-597 mirror), NOT the seeker-skip of
/// `community_contexts_for_target` — the gateway ladder is the community's
/// healing path, and in degraded mode both publisher and resolver fall back
/// to the same spawn key, so probing beats not probing.
pub(crate) async fn epoch_key_candidates(
    community_id: SpaceId,
    crdt_state: Option<&Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,
    fallback: &EpochKey,
) -> Vec<EpochKey> {
    let Some(cs) = crdt_state else {
        return vec![fallback.clone()];
    };
    let guard = cs.lock().await;
    let Some(space) = guard.spaces.get(&community_id) else {
        return vec![fallback.clone()];
    };
    match (&space.current_epoch_key, space.current_epoch) {
        (Some(k), Some(e)) => {
            let mut out = vec![k.clone()];
            if let Some(prev) = e
                .checked_sub(1)
                .and_then(|pe| space.old_epoch_keys.get(&pe))
            {
                out.push(prev.clone());
            }
            out
        }
        _ => vec![fallback.clone()],
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**
- [ ] **Step 5: Commit** — `git commit -m "ZEB-918: epoch_key_candidates helper — live current + one epoch back"`

---

### Task 2: Publisher publishes under the live key

**Files:**
- Modify: `src-tauri/src/lib.rs` — the slot-refresh arm of the relay publish closure (~line 12632, `for (c, engine) in slot_refreshes`)

**Interfaces:**
- Consumes: `crate::community_state_sync::live_epoch_key` (existing), `crdt_state: Arc<Mutex<OwnerState>>` (in `start_node` scope; clone into the closure following the existing `addrbook_*` capture pattern at the closure top)

- [ ] **Step 1: Implement** (no isolated unit seam — covered by Task 5's e2e; keep the diff minimal):

```rust
for (c, engine) in slot_refreshes {
    let advertisers = rendezvous_resolver
        .advertiser_addrs_for_community(&c, now_ms);
    // ZEB-918: publish under the LIVE membership epoch key, degrading
    // to the spawn-time key only when the live read is unavailable —
    // so beacons re-key on the first refresh after an epoch rotation
    // (the membership-change force-wake) instead of pinning the
    // spawn-time key for the engine's lifetime. Publisher-degrades
    // (ZEB-597 mirror): still publish *something* on a degraded read.
    let fallback = engine.membership_key();
    let publish_key = match crate::community_state_sync::live_epoch_key(
        c,
        Some(&slot_crdt_state),
        &fallback,
    )
    .await
    {
        Ok((k, _epoch)) => k,
        Err(_) => fallback,
    };
    rendezvous_publisher
        .refresh_slot(c, publish_key, advertisers, actor)
        .await;
}
```

with `let slot_crdt_state = std::sync::Arc::clone(&crdt_state);` added to the closure's captures (outside), cloned per-invocation inside the closure body exactly as the neighboring `addrbook_book`/`addrbook_rr` captures are.

- [ ] **Step 2: Verify compile + neighborhood tests** — `cargo check --locked --all-targets --features test-fixtures`, then `scripts/test-select --context task`
- [ ] **Step 3: Commit** — `git commit -m "ZEB-918: rendezvous publisher reads the live membership epoch key per refresh"`

---

### Task 3: `GatewayDialCtx` candidates + prod wiring + comment rewrites

**Files:**
- Modify: `src-tauri/src/community_gateway_dial_driver.rs` (trait ~line 54, `ProdGatewayDialCtx` ~179-213, ctx doc ~51, test stubs in the file's test module)
- Modify: `src-tauri/src/lib.rs:12311` (ProdGatewayDialCtx construction)

**Interfaces:**
- Produces: `async fn epoch_key_candidates_of(&self, community: &SpaceId) -> Option<Vec<EpochKey>>` — `None` = no engine registered (preserves the load-bearing `EngineUnregistered` signal); `Some(v)` = ordered, non-empty, ≤ 2
- Consumes: Task 1's `epoch_key_candidates`

- [ ] **Step 1: Replace the trait method** (rename `epoch_key_of` → `epoch_key_candidates_of`, returning `Option<Vec<EpochKey>>`), update the trait doc.

- [ ] **Step 2: Prod impl + field:**

```rust
pub struct ProdGatewayDialCtx {
    pub registry: Arc<crate::community_state_sync::CommunitySyncRegistry>,
    pub self_owner: OwnerAddr,
    /// ZEB-918: live owner-state handle for epoch-key candidate reads.
    /// `None` (test/legacy) degrades every read to the spawn-time key.
    pub crdt_state: Option<Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,
}

async fn epoch_key_candidates_of(&self, community: &SpaceId) -> Option<Vec<EpochKey>> {
    // Engine presence stays the None-gate: "no engine registered" must keep
    // reaching the ladder as EngineUnregistered, distinct from "no members".
    let engine = self.registry.engine_arc(community).await?;
    let fallback = engine.membership_key();
    Some(
        crate::community_state_sync::epoch_key_candidates(
            *community,
            self.crdt_state.as_ref(),
            &fallback,
        )
        .await,
    )
}
```

Rewrite the `:51` ctx doc and the old `:206-213` "deliberately NOT live_epoch_key" comment to state the new invariant: *publisher publishes only under the live key; resolvers read live + one epoch back; degraded mode falls back to the spawn key on BOTH sides so the pair stays coherent.*

- [ ] **Step 3: Wire construction** at `lib.rs:12311`: add `crdt_state: Some(std::sync::Arc::clone(&crdt_state)),`.

- [ ] **Step 4: Update every test stub** implementing `GatewayDialCtx` in the driver's test module to the new signature (wrap their existing single key in `Some(vec![key])`; keep `None` cases as `None`).

- [ ] **Step 5: Compile + driver tests** — `cargo nextest run --locked --features test-fixtures -E 'test(gateway)'` → PASS (behavior unchanged so far; the ladder still uses only the first candidate until Task 4)

  *Note:* Task 3 leaves the ladder call site consuming `candidates[0]` via a minimal shim (`let epoch_key = candidates.first().cloned()`), so the tree is green between Tasks 3 and 4.

- [ ] **Step 6: Commit** — `git commit -m "ZEB-918: GatewayDialCtx epoch-key candidates (live + one epoch back)"`

---

### Task 4: Ladder tries candidates in order

**Files:**
- Modify: `src-tauri/src/community_gateway_dial_driver.rs` (resolve site ~line 377 and ~460-478; test module)

**Interfaces:**
- Consumes: Task 3's `epoch_key_candidates_of`, existing `BeaconResolver::resolve_beacon(&EpochKey, SpaceId, Arc<HashSet<[u8;32]>>, u64) -> BeaconResolution`

- [ ] **Step 1: Write failing driver tests** (stub ctx + counting stub resolver keyed by epoch key):

```rust
// (a) rotation skew, resolver ahead: candidates [K_new, K_old], beacon only
//     under K_old → BeaconSeeded; resolver called twice, K_new first.
// (b) healthy path: candidates [K], beacon under K → BeaconSeeded with
//     exactly ONE resolve call (no extra probe cost).
// (c) nothing anywhere: candidates [K_new, K_old], no beacon → outcome
//     NoBeacon recorded (the CURRENT-key attempt's outcome), two calls.
// (d) rejected-current, valid-previous: RejectedNonMember under K_new,
//     valid beacon under K_old → BeaconSeeded (healing wins; the reject
//     was another publisher's bad vouch, not a reason to stay dark).
```

- [ ] **Step 2: Run to verify they fail** (stub resolver counts / multi-key seams don't exist yet)

- [ ] **Step 3: Implement** — replace the single `resolve_beacon` call:

```rust
let enrolled_keys = Arc::new(self.ctx.enrolled_device_keys_of(&community).await);
// ZEB-918: try candidates in order (live current first, previous epoch
// second); stop at the first live beacon. When nothing is found the
// CURRENT-key attempt's outcome is recorded — it is the canonical health
// signal, and the previous-key attempt exists only to heal rotation skew,
// so it must not mask current-key telemetry.
let mut resolution = BeaconResolution::NotFound;
for (i, candidate) in epoch_keys.iter().enumerate() {
    let attempt = self
        .beacons
        .resolve_beacon(candidate, community, Arc::clone(&enrolled_keys), now_ms)
        .await;
    let found = matches!(attempt, BeaconResolution::Found(_));
    if i == 0 || found {
        resolution = attempt;
    }
    if found {
        break;
    }
}
let hit = match resolution { /* existing four-arm match, unchanged */ };
```

(`epoch_keys` is Task 3's candidates vec, replacing the Task-3 shim; delete the shim.)

- [ ] **Step 4: Run driver tests** → PASS; run `scripts/test-select --context task`
- [ ] **Step 5: Commit** — `git commit -m "ZEB-918: gateway ladder resolves live key first, previous epoch as rotation-skew fallback"`

---

### Task 5: E2E rotation regression

**Files:**
- Create: `src-tauri/tests/pkarr_net/zeb918_epoch_rotation.rs`
- Modify: `src-tauri/tests/pkarr_net_tests.rs` (add `#[path = "pkarr_net/zeb918_epoch_rotation.rs"] mod zeb918_epoch_rotation;`)

**Interfaces:**
- Consumes: the #657 harness pattern (`tests/pkarr_net/zeb880_record_size.rs`): `MockPkarrRelay::start_strict`, `RelayPool`/`RelayClient`, `PkarrPublisher::new(...).spawn()`, `CommunityRendezvousPublisher::new`, `rendezvous_slot_verifying_key`, `PkarrResolver`, `current_epoch_id`

- [ ] **Step 1: Write the test** (RED against nothing — this is a regression pin; it must pass only with Tasks 1-4 in place when driven through prod types, but its publisher half is directly assertable):

```rust
//! ZEB-918: after a membership epoch rotation the publisher's NEXT refresh
//! must publish the beacon under the NEW epoch key, while the OLD key's
//! last record remains resolvable until it ages out (the natural overlap
//! window) — replacing the pre-fix behavior where an un-restarted process
//! pinned the spawn-time key forever.

#[tokio::test]
async fn rotation_rekeys_beacon_on_next_refresh_and_old_record_ages_out_naturally() {
    // Harness identical to zeb880_record_size.rs (strict mock relay,
    // real publisher stack, small single-address payload).
    // 1. refresh_slot(cid, K1, [me], me) → poll: record resolvable under
    //    rendezvous_slot_verifying_key(K1, 0, epoch_now).
    // 2. refresh_slot(cid, K2, [me], me)  // rotation: caller now passes live key
    //    → poll: record resolvable under K2's slot key (same slot, same handle).
    // 3. Assert the K1 record STILL resolves from the relay (natural window:
    //    the relay retains the old signed packet; freshness gate accepts it).
    // 4. Assert both records decode via decode_rendezvous_blob and carry a
    //    vouch (publisher invariants preserved across the rekey).
}
```

Poll pattern: 20 s deadline, 100 ms interval, fresh `PkarrResolver` per attempt, both time epochs (`epoch_now`, `epoch_now - 1`) per attempt — copied from the #657 test.

- [ ] **Step 2: Run it** — `cargo nextest run --locked --features test-fixtures -E 'test(zeb918)'` → PASS with Tasks 1-4 applied. Then `git stash` Tasks 2's lib.rs hunk is NOT separable here — instead pin the *pre-fix* failure mode at the unit level only (Task 1 tests already assert candidate content; this e2e pins the publisher rekey path end-to-end). No red-first gymnastics for the e2e: its value is the regression pin.

- [ ] **Step 3: Commit** — `git commit -m "ZEB-918: e2e — rotation rekeys the beacon on next refresh; old record ages out naturally"`

---

### Task 6: Sweep, docs, PR

- [ ] **Step 1:** `cargo fmt --all` → `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] **Step 2:** Full sweep: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; confirm `git status` clean after commit (local gates run the working tree)
- [ ] **Step 3:** File the follow-up Linear ticket enumerating the remaining spawn-pin sites (presence `community_presence.rs:483,577`; addrbook `address_book_sync.rs:347,769` + `lib.rs:12559`; open-join acceptor `iroh_invite_acceptor.rs:716`; invite-mint sites `lib.rs:9037,9798,32383,32441,36611`) and reference it from the PR body + spec.
- [ ] **Step 4:** Push branch; open PR (`--repo zeblithic/harmony-client`) titled `ZEB-918: rendezvous beacons publish under the live membership epoch key (+ one-epoch resolver fallback)`; body: problem, verified map, design (incl. the no-dual-publish decision), tests, "Closes ZEB-918", session footer. Fire `@coderabbitai review` ONCE at open. Pushover at ready.
