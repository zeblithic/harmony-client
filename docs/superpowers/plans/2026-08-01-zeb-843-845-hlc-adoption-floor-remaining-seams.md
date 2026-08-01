# ZEB-843 + ZEB-845: Wire the two remaining HLC-adoption-floor seams Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish ZEB-790 by feeding the `HlcAdoptFloor` from the two verified-accept seams that #578 deliberately left unwired — tier-3 voting inbound (ZEB-843) and `MintSyncEngine` mint-root sync (ZEB-845) — and reconcile the spec/docs to reality.

**Architecture:** ZEB-790 established one node-wide `HlcAdoptFloor` (`Arc<AtomicU64>`, cheap clone) created once in `start_node` (`lib.rs:5183`) and threaded to every mint/accept engine. Each *verified-accept* path calls `floor.observe(remote_wall_ms)` strictly **after** its replay-tracker commit/record succeeds (so a rejected frame can never move the floor); each *mint* seam reads `floor.merged_now(wall_now)` (clamps adoption to `now + 5000ms`). This plan mirrors that exact discipline onto two more engines. It is additive — no existing feed/mint behavior changes.

**Tech Stack:** Rust, Tauri, tokio, `ciborium` (CBOR), `rusqlite`, `nextest`.

## Global Constraints

- **Feed discipline (load-bearing):** every new `observe()` call MUST sit structurally *after* the path's verify + apply + replay-tracker `record`/`insert`/`commit` has succeeded, and before any `Ok(...)` return — so every earlier `return`/`?` on a rejection leaves the floor untouched. This mirrors the three existing sites (`community_state_sync.rs:4474`, `community_channel_log_engine.rs:1738`, `fleet_sync.rs:1445`). Never feed before verify; never feed on a drop/replay/error path.
- **`adopt_floor` is never `Option`** anywhere (spec §5): tests construct a fresh empty floor (the identity — `merged_now` is a no-op on an empty floor), production threads the real one. Do not introduce `Option<HlcAdoptFloor>`.
- **Wall field path is uniform:** every payload carries `owner_state_types::Hlc { wall_ms: u64, logical: u32, device_id: String }`. The feed is always `floor.observe(<hlc>.wall_ms)`.
- **Feed value semantics:** `observe` stores `max_observed_wall + 1` internally; callers pass the raw `wall_ms` (do NOT pre-add 1).
- **Deterministic engine-auto mints must NOT read the floor:** `engine_auto_hlc_from_base` (`community_voting_log_engine.rs:3038`) derives its HLC purely from `base.logical+1` for replica-determinism — leave it untouched.
- **One PR, closes both tickets.** Branch `zeblith/zeb-843-zeb-845-finish-hlc-adoption-floor-seams`. PR body: `Closes ZEB-843` and `Closes ZEB-845`.
- **CI gates (must pass locally before push):** `cargo fmt --all -- --check`; `cargo clippy --all-targets` (clippy `--all-targets`, not `--lib`, since tests change); `cargo nextest` lib + affected integration; FE untouched (no `src/` changes in this plan).
- **Out of scope (recorded decision, not a task):** ZEB-843 "minor #1" (convert the acceptor `.with_adopt_floor()` fluent setter to a required constructor arg) is **deferred**. Rationale: production is already correctly wired at both sole `start_node` sites; omission degrades to a *safe* per-device-only default (unlike the security-critical `with_revoked` sibling); the refactor touches 10 call sites (8 tests) and adds a positional arg to an already-`#[allow(clippy::too_many_arguments)]` constructor — churn exceeds payoff. This will be noted on ZEB-843 and left as a possible future hardening.

---

## File Structure

- **`src-tauri/src/community_voting_log_engine.rs`** (modify) — Task 1. Add feed calls to the two verified-inbound accept paths + tests.
- **`src-tauri/src/mint_sync.rs`** (modify) — Task 2. Thread `adopt_floor` into `EngineShared`/`new`, read at mint seam, feed at inbound apply + tests.
- **`src-tauri/src/lib.rs`** (modify) — Task 2. Pass `adopt_floor.clone()` at the single production `MintSyncEngine::new` site (`lib.rs:5452`).
- **`docs/superpowers/specs/2026-07-31-zeb-790-hlc-bounded-adoption-design.md`** (modify) — Task 3. §4/§5/§6/§11 reconciliation + line-drift refresh.
- **`src-tauri/src/owner_state_types.rs`** (modify) — Task 3. Collapse the "Scope of guarantee (2)" caveat (both named seams now wired).
- **`src-tauri/src/community_channel_log_engine.rs`** (modify) — Task 3. One-line comment parity (ZEB-843 minor #2).

---

### Task 1: ZEB-843 — feed the two verified voting-inbound accept paths

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`process_inbound` ~2738-2810, its caller `process_inbound_dispatch` ~2909, `process_inbound_for_test` ~3291, `apply_backfilled_event` ~2826-2880)
- Test: `src-tauri/src/community_voting_log_engine.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: `HlcAdoptFloor::{observe, merged_now}` (from `hlc_adopt_floor.rs`); `SignedVotingEvent { hlc: Hlc, .. }` (`community_voting_core.rs:905`); the engine's existing `self.adopt_floor` field (already present, set at construction line ~532).
- Produces: nothing new for later tasks. Task 3 documents the resulting behavior.

**Context:** `VotingLogEngine` already holds `adopt_floor` and its *outbound* mint (`reserve_next_local_hlc`, line ~590) already reads the floor. The only gaps are the two verified-*inbound* accept paths that never feed it. Decision B.1a: feed inside the static `process_inbound` (thread a `floor` param) so the rationale comment sits at the record point, exactly like the three existing sites. Decision B.1b: also feed the structurally-identical backfill twin `apply_backfilled_event` (it has `&self`).

- [ ] **Step 1: Write the failing test — verified inbound accept feeds the floor**

Add to the test module. The test constructs an engine with a *shared* known floor via `VotingLogEngineParams.adopt_floor`, drives a verified inbound event whose `hlc.wall_ms` is well ahead of "now", and asserts the shared floor advanced past that wall. (Use the existing test scaffolding that builds a `VotingLogEngine` + a verified `SignedVotingEvent`; follow the nearest existing inbound-path test, e.g. around the `process_inbound_for_test` call sites.)

```rust
#[tokio::test]
async fn verified_inbound_feeds_adopt_floor() {
    // A shared floor handed to the engine; observing the SAME handle proves the feed.
    let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
    let fx = VotingFixture::with_adopt_floor(floor.clone()).await; // test helper builds engine w/ this floor
    let remote_wall = fx.now_ms() + 2_000; // inside CAP, ahead of local now
    let event = fx.signed_tier1_event_at_wall(remote_wall).await; // a verifiable inbound event

    fx.dispatch_inbound(&event).await.expect("verified accept");

    // Feed stores max_observed_wall + 1, so merged_now at local-now must reach remote_wall+1.
    assert_eq!(
        floor.merged_now(fx.now_ms()),
        remote_wall + 1,
        "verified voting-inbound accept must feed the adoption floor",
    );
}
```

> NOTE FOR IMPLEMENTER: the exact fixture/helper names above are illustrative — use or extend whatever the existing inbound tests use to (a) construct a `VotingLogEngine` with a caller-supplied `adopt_floor`, (b) mint a verifiable `SignedVotingEvent` at a chosen `hlc.wall_ms`, and (c) run it through the real inbound dispatch. Do NOT invent parallel machinery if a helper already exists.

- [ ] **Step 2: Run it to confirm it fails**

Run: `cd src-tauri && cargo nextest run --features test-fixtures verified_inbound_feeds_adopt_floor`
Expected: FAIL — floor stays empty (`merged_now == fx.now_ms()`), because `process_inbound` does not feed yet.

- [ ] **Step 3: Thread the floor into `process_inbound` and feed after `record`**

Add a `floor: &crate::hlc_adopt_floor::HlcAdoptFloor` parameter to the static `process_inbound` (append it to the existing loose-`&Arc` param list, matching that fn's style), and feed immediately after the `tracker.record(&event)` block, before `Ok(Some(...))`:

```rust
        // Record AFTER successful apply on the inbound path: ... (existing comment)
        {
            let mut tracker = tracker.lock().await;
            tracker.record(&event);
        }

        // ZEB-843: feed the adoption floor ONLY here — after verify (V6
        // membership + Ed25519) + apply + record all succeeded. Every earlier
        // `?`/`return` on a rejection (decode, dedup, absent resolver, verify,
        // eligibility, apply) leaves the floor untouched — the same
        // rejection-inert discipline as the three ZEB-790 feed sites.
        floor.observe(event.hlc.wall_ms);

        Ok(Some((event, applied_poll_id)))
```

- [ ] **Step 4: Update the two callers**

In `process_inbound_dispatch` (the `self`-having wrapper, ~2909), pass `&self.adopt_floor` as the new argument. In `process_inbound_for_test` (~3291), pass a fresh floor so existing test call sites are unchanged:

```rust
    // process_inbound_dispatch:
    Self::process_inbound(
        self.community_id,
        &self.voting_log,
        &self.tracker,
        self.identity_resolver.as_ref(),
        self.membership_resolver.as_ref(),
        &self.adopt_floor, // ZEB-843
        packet,
    ).await
```

```rust
    // process_inbound_for_test: default to a fresh (identity) floor so existing
    // callers need no change — feed behavior is exercised via the engine path.
    let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
    Self::process_inbound(community_id, voting_log, tracker, identity_resolver,
        membership_resolver, &floor, packet).await
```

- [ ] **Step 5: Feed the backfill twin `apply_backfilled_event`**

`apply_backfilled_event(&self, ...)` (~2826-2880) has `&self`, so `self.adopt_floor` is directly reachable. After its own `tracker.record(&event)` (and before its `Ok`/`persist_now`), add:

```rust
        // ZEB-843: same trust class as process_inbound (verified + applied +
        // recorded), so feed the floor here too — keeps the two voting-inbound
        // accept twins symmetric.
        self.adopt_floor.observe(event.hlc.wall_ms);
```

- [ ] **Step 6: Write the rejection-inert negative test**

Assert a *rejected* inbound event (fails verify, or is a dedup replay) does NOT move the floor — mirrors the channel-log `rejected_replay_does_not_feed_floor` test added in #578:

```rust
#[tokio::test]
async fn rejected_inbound_does_not_feed_adopt_floor() {
    let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
    let fx = VotingFixture::with_adopt_floor(floor.clone()).await;
    let remote_wall = fx.now_ms() + 2_000;
    // An event that fails verification (bad signature / non-member actor).
    let bad = fx.unverifiable_event_at_wall(remote_wall).await;

    let _ = fx.dispatch_inbound(&bad).await; // rejected (Err or Ok(None))

    assert_eq!(
        floor.merged_now(fx.now_ms()),
        fx.now_ms(),
        "a rejected voting-inbound event must NOT feed the floor",
    );
}
```

- [ ] **Step 7: Run the full affected test set + gates**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --all-targets --features test-fixtures -- -D warnings \
  && cargo nextest run --features test-fixtures voting
```
Expected: both new tests PASS; no regression in existing voting tests; fmt + clippy clean.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/community_voting_log_engine.rs
git commit -m "ZEB-843: feed the HLC adoption floor from verified voting-inbound accepts (process_inbound + backfill twin)"
```

---

### Task 2: ZEB-845 — wire `MintSyncEngine` to the adoption floor

**Files:**
- Modify: `src-tauri/src/mint_sync.rs` (`EngineShared` ~274, `MintSyncEngine::new` ~419, `internal_task_zenoh`, `next_hlc_mint` ~968, its caller `publish_root_now_zenoh` ~1058, `handle_incoming_publish_zenoh` step 6 ~1234)
- Modify: `src-tauri/src/lib.rs` (single production `MintSyncEngine::new` call ~5452)
- Test: `src-tauri/src/mint_sync.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: `HlcAdoptFloor::{observe, merged_now}`; `MintRootPublishPayload { at: Hlc, .. }` (`mint_sync_types.rs:57`); the node-wide `adopt_floor` in scope at `lib.rs:5452` (same `start_node` block that created it at `lib.rs:5183`).
- Produces: `MintSyncEngine::new` gains a trailing `adopt_floor: HlcAdoptFloor` parameter (production call site only; `new_for_test*` keep an internal fresh-floor default).

**Context:** `mint_sync.rs` has ZERO `adopt_floor` references today (confirmed by grep). Decision D.1: thread the floor via `EngineShared` (the `Clone` bag already handed to the feed site `handle_incoming_publish_zenoh(&EngineShared, ...)`), and pass it down to the one mint call. This was a spec-documented v1 exclusion (§5), not an oversight — the inbound-feed gap was never even in the spec.

- [ ] **Step 1: Write the failing test — verified inbound mint-root feeds the floor**

Add to the test module, using the existing `new_for_test*` scaffolding. Build an engine with a shared floor, apply a *verified* sibling `MintRootPublishPayload` whose `at.wall_ms` is ahead of now (inside CAP), assert the shared floor advanced:

```rust
#[tokio::test]
async fn verified_mint_root_apply_feeds_adopt_floor() {
    let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
    let fx = MintSyncFixture::with_adopt_floor(floor.clone()).await;
    let remote_wall = fx.now_ms() + 2_000;
    fx.apply_verified_sibling_root_at_wall(remote_wall).await; // decrypt+decode+merge+record ok

    assert_eq!(
        floor.merged_now(fx.now_ms()),
        remote_wall + 1,
        "verified mint-root apply must feed the adoption floor",
    );
}
```

> NOTE FOR IMPLEMENTER: reuse the existing mint-sync inbound test path (the tests that already exercise `handle_incoming_publish_zenoh` / a sibling publish). Extend the test constructor to accept a caller-supplied floor; do not build parallel machinery.

- [ ] **Step 2: Run it to confirm it fails**

Run: `cd src-tauri && cargo nextest run --features test-fixtures verified_mint_root_apply_feeds_adopt_floor`
Expected: FAIL to compile first (constructor has no floor param) → after minimal test-constructor plumbing, FAIL on the assertion (floor stays empty).

- [ ] **Step 3: Add `adopt_floor` to `EngineShared` and `new`**

```rust
struct EngineShared {
    mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    content_store: Arc<dyn crate::content_store::ContentStore>,
    sync_state: Arc<TokioMutex<MintSyncState>>,
    sync_state_path: Option<std::path::PathBuf>,
    app_handle: Option<std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>>,
    /// ZEB-845: node-wide bounded-adoption floor (see `hlc_adopt_floor` module
    /// docs). Read at `next_hlc_mint`, fed at the inbound mint-root apply.
    adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
}
```

Add a trailing `adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor` parameter to `pub async fn new(...)` and store it into `EngineShared`. The `new_for_test*` constructors keep an internal `HlcAdoptFloor::new()` default (mirroring how they already diverge from `new`'s Zenoh params) — except the test fixture used by Step 1, which threads the caller-supplied floor.

- [ ] **Step 4: Read the floor at the mint seam `next_hlc_mint`**

Add a `floor: &crate::hlc_adopt_floor::HlcAdoptFloor` param to `next_hlc_mint` and clamp the wall read (same substitution as `community_state_sync::next_hlc` and `fleet_sync::mint_next_hlc`):

```rust
async fn next_hlc_mint(
    sync_state: &Arc<TokioMutex<MintSyncState>>,
    device_id: &str,
    floor: &crate::hlc_adopt_floor::HlcAdoptFloor, // ZEB-845
) -> crate::owner_state_types::Hlc {
    use std::time::{SystemTime, UNIX_EPOCH};
    // ZEB-845: bounded causal adoption — clamp local wall up to a verified
    // remote wall (≤ CAP), same as the other mint seams.
    let wall_ms = floor.merged_now(
        SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64).unwrap_or(0),
    );
    // ... unchanged tracker logic ...
}
```

Update the single caller `publish_root_now_zenoh` (~1058) to pass the floor: `next_hlc_mint(sync_state, device_id, &shared.adopt_floor).await` (thread `&shared.adopt_floor` — or the floor cloned into `internal_task_zenoh`'s scope — to that call site).

- [ ] **Step 5: Feed the floor at the inbound apply (step 6)**

In `handle_incoming_publish_zenoh`, immediately after the `st.replay_tracker.insert(payload.at.device_id.clone(), payload.at.clone())`:

```rust
        st.replay_tracker
            .insert(payload.at.device_id.clone(), payload.at.clone());
        // ZEB-845: verified sibling mint-root — feed the adoption floor after
        // the replay-tracker advance (step 6). Every earlier `?`/echo-suppress/
        // replay-skip returns before this, so a rejected frame never feeds.
        shared.adopt_floor.observe(payload.at.wall_ms);
```

(`handle_incoming_publish_zenoh` already takes `shared: &EngineShared`, so no new param here.)

- [ ] **Step 6: Wire the production call site (`lib.rs:5452`)**

Pass the in-scope node-wide floor as the new trailing arg:

```rust
    let (mint_engine, _mint_handle) = crate::mint_sync::MintSyncEngine::new(
        keys.clone(),
        device_id.clone(),
        mint_db_for_engine,
        std::sync::Arc::clone(&content_store),
        mint_sync_state,
        mint_sync_state_path,
        mint_out_tx,
        mint_in_rx,
        crate::mint_sync::DEFAULT_DEBOUNCE_MS,
        app.clone(),
        adopt_floor.clone(), // ZEB-845: node-wide bounded-adoption floor
    )
    .await;
```

- [ ] **Step 7: Write the mint-side liveness + regression tests**

(a) Mint-side: after feeding the floor with a high remote wall, a locally-minted `next_hlc_mint` clamps to that wall (adopts within CAP). (b) Regression: a distinct sibling's mint-row still wins/loses purely by `updated_at` LWW — the envelope-HLC feed does NOT change row-merge outcomes (assert an existing merge test still passes with a poisoned envelope wall). (c) Rejection-inert: an echo-suppressed / replay-skipped inbound root does NOT feed.

```rust
#[tokio::test]
async fn mint_after_observe_clamps_within_cap() {
    let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
    let fx = MintSyncFixture::with_adopt_floor(floor.clone()).await;
    let remote_wall = fx.now_ms() + 3_000;
    floor.observe(remote_wall);
    let minted = fx.mint_local_root_hlc().await; // calls next_hlc_mint(.., &floor)
    assert!(minted.wall_ms >= remote_wall + 1, "local mint adopts the fed remote wall");
}
```

- [ ] **Step 8: Run affected tests + gates**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --all-targets --features test-fixtures -- -D warnings \
  && cargo nextest run --features test-fixtures mint_sync
```
Expected: new tests PASS; existing mint-sync tests unaffected; gates clean.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/mint_sync.rs src-tauri/src/lib.rs
git commit -m "ZEB-845: wire MintSyncEngine to the shared HLC adoption floor (read at mint, feed at verified apply)"
```

---

### Task 3: Reconcile spec + docs to the shipped behavior

**Files:**
- Modify: `docs/superpowers/specs/2026-07-31-zeb-790-hlc-bounded-adoption-design.md` (§4, §5, §6, §11)
- Modify: `src-tauri/src/owner_state_types.rs` (`Hlc` doc, "Scope of guarantee (2)" ~335-344)
- Modify: `src-tauri/src/community_voting_log_engine.rs` (comment at `current_hlc_estimate` ~626 — minor #3)
- Modify: `src-tauri/src/community_channel_log_engine.rs` (one-line comment ~1738 — minor #2)

**Interfaces:** none (doc/comment only). Must run *after* Tasks 1-2 so it describes shipped code.

**Context:** Several ticket premises were stale (see the plan's decision table). §11's "only owner-state engine" wording is already correct — do NOT touch it except to add the two new seams to the nudge-surface list. Both named seams are now wired, so the Hlc doc's two-ticket caveat collapses. Minor #1 (acceptor refactor) is deferred, not done — the docs must not claim it.

- [ ] **Step 1: Spec §4 — promote voting + mint-sync from excluded to fed**

In the §4 feed-site table, add two rows (voting inbound; mint-sync inbound) with current line citations. In the "Deliberately excluded in v1" list: remove the **Tier-3 voting inbound** bullet (now fed via ZEB-843) and do NOT add mint-sync as excluded (now fed via ZEB-845). Leave the **DM `sent_at`** and **unverified/synthetic** exclusions intact. While here, refresh the drifted citations noted in grounding §E.1 (`community_state_sync.rs:4457-4460` → current `commit`/`observe` lines; wrong filename `community_channel_log.rs` → `community_channel_log_engine.rs`; `fleet_sync.rs:1422` → current).

- [ ] **Step 2: Spec §5 — mint-sync no longer a bypass**

Update the §5 "known bypasses" line that cites `mint_sync.rs:976 ... does not adopt in v1` — it now reads + feeds the floor (ZEB-845). Remove/rewrite that bypass entry.

- [ ] **Step 3: Spec §6 — flip the ZEB-843 caveat + name the `current_hlc_estimate` asymmetry**

Rewrite the §6 side-benefit caveat: the tier-3 lockout-shrink now applies unconditionally to any verified voting-inbound accept (not "only when learned via a different fed path"). Add a short note (minor #3): `current_hlc_estimate` (voting deadline/expiry comparator) deliberately reads **raw** wall-clock, not `merged_now` — a ≤ CAP (5s) same-instant asymmetry vs. the mint seam, conservative in direction (a deadline is never judged *past* earlier than the local clock says), inside the analyzed consumer-budget envelope.

- [ ] **Step 4: Spec §11 — add the two new nudge-surface entries**

Extend the §11 "Nudge surface" bullet to include community voting members (Ed25519-bound, same trust class as channel authors) and own-fleet mint-state siblings (AEAD). Do NOT alter the already-correct "every fleet-doc engine" clause.

- [ ] **Step 5: `owner_state_types.rs` — collapse the "Scope of guarantee (2)" caveat**

Both named seams (ZEB-843 voting-inbound, ZEB-845 mint-sync) are now wired, so remove the "Two mint/sync seams are **not yet wired**…" paragraph (~335-344). Ensure the surrounding `Hlc` doc still accurately states guarantee (2) holds at the mint seams that consume the floor, without over-claiming universal coverage (DM `sent_at` remains a deliberately non-adopting *input*, per spec §4 — it is not a mint seam, so guarantee (2)'s scope statement need not enumerate it, but do not write "no exceptions anywhere" either).

- [ ] **Step 6: `community_channel_log_engine.rs` — minor #2 comment parity**

At the channel-log feed (~1738), add the one-line note that the `observe()` fires *before* the step-3 `closing` check, mirroring the pre-existing 2c replay-tracker advance (also pre-closing) — so a future reviewer doesn't re-derive that the feed-point and durability-point are intentionally asymmetric on a shutdown race (harmless: floor is session-only/non-persisted).

- [ ] **Step 7: Verify docs build-clean + no stale refs**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets --features test-fixtures -- -D warnings` (doc-comment changes must not break rustdoc/clippy). Grep the spec for any remaining `process_inbound_packet` / `now_hlc_estimate` stale names and fix to `process_inbound` / `current_hlc_estimate`.

- [ ] **Step 8: Commit**

```bash
git add docs/superpowers/specs/2026-07-31-zeb-790-hlc-bounded-adoption-design.md \
        src-tauri/src/owner_state_types.rs \
        src-tauri/src/community_voting_log_engine.rs \
        src-tauri/src/community_channel_log_engine.rs
git commit -m "ZEB-843/ZEB-845: reconcile ZEB-790 spec + Hlc docs to the two newly-wired seams"
```

---

## Self-Review

- **Spec coverage:** ZEB-843 core (voting inbound feed) = Task 1 steps 3-5; ZEB-843 minor #2 (channel-log comment) = Task 3 step 6; minor #3 (`current_hlc_estimate` naming) = Task 3 step 3; minor #1 (acceptor refactor) = explicitly deferred (Global Constraints). ZEB-843 spec §4/§11 = Task 3 steps 1/4. ZEB-845 core (thread/read/feed) = Task 2 steps 3-6; ZEB-845 doc-caveat removal = Task 3 step 5; tests = Task 2 step 7. All covered.
- **Type consistency:** `floor.observe(<hlc>.wall_ms)` and `floor.merged_now(wall_now)` used uniformly; `HlcAdoptFloor` (non-`Option`) threaded as a value/param consistently; `next_hlc_mint`/`process_inbound` both gain a trailing `floor` param matching the existing loose-param style.
- **Placeholder scan:** test fixture/helper names are flagged as illustrative with explicit "reuse existing scaffolding" notes — implementer must bind them to real helpers, not invent parallel machinery.
- **Ordering:** Task 3 (docs) depends on Tasks 1-2 and runs last so it reflects shipped code.
