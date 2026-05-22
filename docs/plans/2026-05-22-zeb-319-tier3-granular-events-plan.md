# ZEB-319 Tier 3 granular post-apply events — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the 5 s polling fallback in `Tier3ProposalPanel.svelte` with three event-driven refetch triggers (`voting-tier3-mini-public-decline` / `-draft-candidate` / `-draft-approval`), completing the event-driven Tier 3 UI pattern that ZEB-294 + ZEB-295 established.

**Architecture:** Three new branches inside the existing `maybe_emit_tier3_lifecycle_events` function in `community_voting_log_engine.rs`. Each emits a `serde_json::json!{}` payload via `app_handle.emit(...)` when its CRDT event kind has just successfully applied. Frontend types + adapter subscribers + panel wiring follow the exact pattern of the existing `voting-tier3-tally-share-applied` event (PR #155).

**Tech Stack:** Rust + tokio + serde_json + Tauri 2 IPC (snake_case Rust ↔ camelCase JS) + Svelte 5 runes + Vitest.

**Spec:** `docs/specs/2026-05-22-zeb-319-tier3-granular-events-design.md` (commit `af4e042`).

**Branch:** `zeb-319-tier3-granular-events` off `origin/main` `9ab41f6`.

---

## Hard Rules (Implementer Discipline)

All implementer subagents MUST:

- Run from project root unless instructed otherwise. **Cargo commands run from `src-tauri/`. Frontend commands run from repo root.**
- 4 backend gates per task:
  1. `cd src-tauri && cargo fmt --all -- --check`
  2. `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  3. `cd src-tauri && cargo check --locked --all-targets --features test-fixtures` — MSRV gate — declared toolchain via `rust-toolchain.toml`. Should be fast (incremental check, no codegen).
  4. `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (10-minute wall-clock kill switch)
- 2 frontend gates per task touching `src/`:
  5. `npx tsc --noEmit` (from repo root)
  6. `npx vitest run` (from repo root)
- **Commit BEFORE running the gates** so a 10-min wall-clock timeout doesn't lose work. The DONE_WITH_CONCERNS escape hatch is for "gate timed out, but commit is in place and observable concerns are X, Y, Z."
- Use `set -o pipefail` or `${PIPESTATUS[0]}` whenever piping cargo output through `tail`/`grep` — pipe exit codes lie.
- No worktrees — `git checkout` in the main repo only. The branch is already created.
- Pre-existing orphan failures (~28 baseline from ZEB-295 PR #155): `folder_ingest::tests` (3), `mint::tests` (2), `mint_sync::tests` (2), `folder_ingest_walker_integration` (9), `rename_content_integration` (12). **These are not blocking.** New failures introduced by this PR are blocking.
- Tauri IPC parameter naming: `snake_case` Rust ↔ `camelCase` JS. New event payloads use `serde_json::json!{}` with camelCase string keys.
- Frontend error extraction: `e instanceof Error ? e.message : String(e)`.

---

## File Structure

**Modified files (single PR):**

| File | Change |
|---|---|
| `src-tauri/src/community_voting_log_engine.rs` | 3 new `if applied_event.kind == ...` branches inside `maybe_emit_tier3_lifecycle_events` (function spans ~lines 1757–2159 today) |
| `src-tauri/tests/community_voting_tier3_granular_events_integration.rs` | **NEW** — 3 integration tests (one per event kind), peer-inbound + originator-self-emit |
| `src/lib/types/voting.ts` | 3 new payload interfaces appended near existing `Tier3TallyShareAppliedPayload` (line 641) |
| `src/lib/voting-adapter.ts` | 3 new `subscribeTier3*` methods near existing `subscribeTier3TallyShareApplied` (line 304) |
| `src/lib/components/Tier3ProposalPanel.svelte` | Add 3 subscribers in the community-switching `$effect` (after the tally-share subscriber, ~line 261-266). Remove the 5 s polling `$effect` (lines 269–284) entirely. |
| `src/lib/components/__tests__/Tier3ProposalPanel.test.ts` | 3 new test cases: no polling scheduled; events refetch on community+poll match; events ignored on mismatched communityId. |

---

## Task 0: Pre-flight baseline

**Purpose:** Verify the working tree is on `zeb-319-tier3-granular-events` off `origin/main` `9ab41f6`, working tree clean, spec doc committed at `af4e042`. No code changes. No commit.

- [ ] **Step 1: Verify branch + clean tree**

```bash
git status
git log --oneline -3
git merge-base HEAD origin/main
```

Expected:
- `On branch zeb-319-tier3-granular-events`
- `nothing to commit, working tree clean`
- Top 3 commits include `af4e042 docs(zeb-319):` then `9ab41f6 ZEB-295 Phase 6: Tier 3c ballot-secret ratification` then `0bf89c3 ZEB-320:`
- `git merge-base` returns `9ab41f6` (latest origin/main HEAD pre-spec-commit)

- [ ] **Step 2: Confirm backend baseline compiles + tests pass modulo known orphans**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -50
```

Expected: workspace compiles cleanly; ~28 known orphan failures (see Hard Rules). No new failures.

- [ ] **Step 3: Confirm frontend baseline compiles + tests pass**

```bash
npx tsc --noEmit && npx vitest run
```

Expected: zero TS errors, all Vitest suites green.

- [ ] **No commit.** This is a baseline verification only.

---

## Task 1: Backend — three new Tauri-event branches inside `maybe_emit_tier3_lifecycle_events`

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (function `maybe_emit_tier3_lifecycle_events`, ~lines 2014–2159 region)

**Architectural anchor:** The pattern source is the ZEB-295 Phase 6 emit branch at lines 2014–2068 (kd=ts → `voting-tier3-tally-share-applied`) and the ZEB-294 emits at lines 2077–2158 (kd=ds, kd=dv). All three follow the same recipe:

1. Cheap kind-equality gate.
2. CBOR-decode the applied event's payload using the existing `community_voting_core::*Payload` types.
3. Re-acquire `voting_log` briefly to confirm the apply actually landed in the projection (mirror existing acceptance check).
4. Build a `serde_json::json!{...}` payload with camelCase string keys.
5. `app_handle.emit("voting-tier3-...", &payload)` with non-fatal `tracing::warn!` on error.

- [ ] **Step 1: Write the failing test for the kd=md emit (unit-level payload shape)**

Add to the existing inline `#[cfg(test)] mod tests { ... }` block in `community_voting_log_engine.rs` (locate via `grep -n "mod tests" src-tauri/src/community_voting_log_engine.rs | head -3`). Mirror the unit test for `voting-tier3-tally-share-applied` payload shape.

```rust
#[tokio::test]
async fn emits_voting_tier3_mini_public_decline_payload_shape() {
    // Two-engine harness: A publishes pc+ss+md; B receives md via process_inbound.
    // Assert B's MockAppHandle captured exactly one
    // voting-tier3-mini-public-decline event with payload keys:
    //   { pollId, communityId, decliner, declineHlcMs }
    // and decliner == hex(actor pubkey), declineHlcMs == event.hlc.wall_ms.
    //
    // (Spell out the test body following the kd=ts unit-test precedent
    //  — see test fn covering voting-tier3-tally-share-applied for the
    //  exact harness scaffolding.)
    unimplemented!("Task 1 Step 1 — fill in")
}
```

- [ ] **Step 2: Run the test to confirm it fails**

```bash
cd src-tauri && set -o pipefail && cargo nextest run --locked --features test-fixtures -E 'test(emits_voting_tier3_mini_public_decline_payload_shape)' 2>&1 | tail -20
```

Expected: FAIL (`unimplemented!`).

- [ ] **Step 3: Add the kd=md emit branch in `maybe_emit_tier3_lifecycle_events`**

After the kd=dv branch (currently ends ~line 2158) and before the closing `}` of the function (~line 2159), append:

```rust
// 7. ZEB-319: mini-public-decline (kd=md applied). Mirror the kd=ds
// acceptance check: emit only when the decline actually landed in
// t3.declines (apply rules drop invalid declines silently).
//
// Note: t3.declines is Vec<(OwnerAddr, Hlc)>, so use tuple indexing
// d.0/d.1 — not named struct access.
if applied_event.kind == PollEventKindCode::MiniPublicDecline {
    let accepted: bool = {
        let log = self.voting_log.lock().await;
        log.polls
            .get(pid)
            .and_then(|ps| ps.tier_state.as_tier3())
            .is_some_and(|t3| {
                t3.declines
                    .iter()
                    .any(|d| d.0 == applied_event.actor && d.1 == applied_event.hlc)
            })
    };
    if accepted {
        let payload = serde_json::json!({
            "pollId": pid_hex,
            "communityId": community_id_hex,
            "decliner": hex::encode(applied_event.actor.0),
            "declineHlcMs": applied_event.hlc.wall_ms,
        });
        if let Err(e) = app_handle.emit("voting-tier3-mini-public-decline", &payload) {
            tracing::warn!(
                error = %e,
                poll_id = %pid_hex,
                "voting-tier3-mini-public-decline emit failed (non-fatal)"
            );
        }
    }
}
```

**Field source notes:**
- `pid_hex` and `community_id_hex` are already in scope from the function preamble (grep them in lines 1757-1900 of the existing function).
- `applied_event.actor.0` is the `OwnerAddr`'s underlying `[u8; 32]` — `hex::encode` of it matches the convention used at line 2095 (`hex::encode(applied_event.actor.0)`).
- `applied_event.hlc.wall_ms` matches the field name used elsewhere in the engine (`applied_event.hlc.wall_ms` at line 2097).
- The `Decline` projection struct is at `community_voting_tier3.rs:~197` (line `pub declines: Vec<...>`); confirm field names via `grep -A3 "pub declines" src-tauri/src/community_voting_tier3.rs`.

- [ ] **Step 4: Run the test to verify it passes**

```bash
cd src-tauri && set -o pipefail && cargo nextest run --locked --features test-fixtures -E 'test(emits_voting_tier3_mini_public_decline_payload_shape)' 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 5: Repeat Steps 1–4 for kd=dc (`voting-tier3-draft-candidate`)**

The unit test asserts payload keys `{ pollId, communityId, proposer, eventHash, candidateText }` and that `eventHash == hex(event_hash_of(applied_event))`, `proposer == hex(actor.0)`, `candidateText == DraftCandidatePayload::text` (CBOR-decoded).

The emit branch sits after the kd=md branch:

```rust
// 8. ZEB-319: draft-candidate (kd=dc applied). Acceptance check:
// the candidate is in t3.candidates with the just-applied event_hash.
if applied_event.kind == PollEventKindCode::DraftCandidate {
    if let Ok(dc_payload) = ciborium::de::from_reader::<
        crate::community_voting_core::DraftCandidatePayload,
        _,
    >(&applied_event.payload[..])
    {
        let event_hash = crate::community_voting_tier3::event_hash_of(applied_event);
        let accepted: bool = {
            let log = self.voting_log.lock().await;
            log.polls
                .get(pid)
                .and_then(|ps| ps.tier_state.as_tier3())
                .is_some_and(|t3| {
                    t3.candidates.iter().any(|c| c.event_hash == event_hash)
                })
        };
        if accepted {
            let payload = serde_json::json!({
                "pollId": pid_hex,
                "communityId": community_id_hex,
                "proposer": hex::encode(applied_event.actor.0),
                "eventHash": hex::encode(event_hash),
                "candidateText": dc_payload.text,
            });
            if let Err(e) = app_handle.emit("voting-tier3-draft-candidate", &payload) {
                tracing::warn!(
                    error = %e,
                    poll_id = %pid_hex,
                    event_hash = %hex::encode(event_hash),
                    "voting-tier3-draft-candidate emit failed (non-fatal)"
                );
            }
        }
    }
}
```

**Field name verification (before writing the branch):**
- Verify `DraftCandidatePayload` has a `.text` field via `grep -A5 "struct DraftCandidatePayload" src-tauri/src/community_voting_core.rs`. If the field is named differently (e.g. `.proposal_text`), use the actual name and update the `payload["candidateText"]` source accordingly.
- Verify `DraftCandidateState` exposes `event_hash: [u8; 32]` via `grep -A8 "pub struct DraftCandidateState" src-tauri/src/community_voting_tier3.rs`.

- [ ] **Step 6: Repeat Steps 1–4 for kd=da (`voting-tier3-draft-approval`)**

The unit test asserts payload keys `{ pollId, communityId, approver, targetEventHash }`.

The emit branch sits after the kd=dc branch:

```rust
// 9. ZEB-319: draft-approval (kd=da applied). Acceptance check:
// the actor is in the targeted candidate's approvals set.
//
// Note: DraftApprovalPayload field is `candidate_event_hash`, not
// `target_event_hash` — verify via grep before writing.
if applied_event.kind == PollEventKindCode::DraftApproval {
    if let Ok(da_payload) = ciborium::de::from_reader::<
        crate::community_voting_core::DraftApprovalPayload,
        _,
    >(&applied_event.payload[..])
    {
        let target_hash = da_payload.candidate_event_hash;
        let accepted: bool = {
            let log = self.voting_log.lock().await;
            log.polls
                .get(pid)
                .and_then(|ps| ps.tier_state.as_tier3())
                .is_some_and(|t3| {
                    t3.candidates
                        .iter()
                        .find(|c| c.event_hash == target_hash)
                        .is_some_and(|c| c.approvals.contains(&applied_event.actor))
                })
        };
        if accepted {
            let payload = serde_json::json!({
                "pollId": pid_hex,
                "communityId": community_id_hex,
                "approver": hex::encode(applied_event.actor.0),
                "targetEventHash": hex::encode(target_hash),
            });
            if let Err(e) = app_handle.emit("voting-tier3-draft-approval", &payload) {
                tracing::warn!(
                    error = %e,
                    poll_id = %pid_hex,
                    target_event_hash = %hex::encode(target_hash),
                    "voting-tier3-draft-approval emit failed (non-fatal)"
                );
            }
        }
    }
}
```

**Field name verification:**
- Verify `DraftApprovalPayload` has a `.candidate_event_hash` field via `grep -A5 "struct DraftApprovalPayload" src-tauri/src/community_voting_core.rs`. Adjust if named differently.
- Verify `DraftCandidateState` has an `.approvals` field that contains `OwnerAddr` values (check existing usage at line 705: `// kd=da DraftApproval: add actor to the named candidate's approvals (idempotent).`).

- [ ] **Step 7: Run full backend gates**

```bash
cd src-tauri && set -o pipefail && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -30
```

Expected: fmt OK, clippy zero warnings, nextest passes (modulo the 28 known orphan failures).

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/community_voting_log_engine.rs
git commit -m "$(cat <<'EOF'
feat(zeb-319): emit Tauri events on Tier 3 kd=md/dc/da apply

Adds three new Tauri-event branches inside
maybe_emit_tier3_lifecycle_events for sortition declines
(voting-tier3-mini-public-decline), draft-candidate proposals
(voting-tier3-draft-candidate), and draft-approval signals
(voting-tier3-draft-approval). Each follows the existing kd=ds/dv/ts
acceptance-check pattern: only emit when the event landed in the
projection (apply-rule drops stay silent).

Payload keys use camelCase via serde_json::json! literals — matches
the existing 3 mid-stage Tier 3 event payloads.

Unit tests assert payload shape per event kind.

ZEB-319 progress (1/5): backend emit branches.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Backend integration test — peer-inbound + originator-self-emit

**Files:**
- Create: `src-tauri/tests/community_voting_tier3_granular_events_integration.rs`

**Architectural anchor:** Two-engine harness pattern from `src-tauri/tests/community_voting_tier3_secret_kd_ts_emission_integration.rs`. Each integration test sets up engines A + B, has A publish events, has B receive them via inbound dispatch, and asserts both engines' `MockAppHandle` captured the right Tauri events.

- [ ] **Step 1: Create the test file scaffold**

```bash
test -f src-tauri/tests/community_voting_tier3_granular_events_integration.rs && echo "File exists, will overwrite" || echo "Creating new file"
```

- [ ] **Step 2: Write the kd=md two-engine test**

```rust
// src-tauri/tests/community_voting_tier3_granular_events_integration.rs
//
// ZEB-319: granular post-apply Tauri events for Tier 3 mid-stage
// mutations (kd=md / kd=dc / kd=da). Each test:
//
//   1. Spins up engines A + B with a Tier 3 poll past sortition.
//   2. Engine A publishes the test event kind.
//   3. Asserts A's MockAppHandle captured the corresponding Tauri
//      event (originator-self-emit on publish_event).
//   4. Forwards A's outbound packet to B via process_inbound.
//   5. Asserts B's MockAppHandle captured exactly one matching Tauri
//      event (peer-inbound emit on process_inbound_dispatch).
//   6. Asserts the captured payload keys match the spec.

use harmony_app::test_helpers::voting_tier3::*; // adapt path to actual test-helper module
// ... (use existing imports from community_voting_tier3_secret_kd_ts_emission_integration.rs)

#[tokio::test]
async fn voting_tier3_mini_public_decline_emits_on_originator_and_peer() {
    // (Spell out per the pattern source. Key assertions:
    //   - A's captured event["pollId"] == hex(pc.poll_id)
    //   - A's captured event["communityId"] == hex(community_id)
    //   - A's captured event["decliner"] == hex(actor.0)
    //   - A's captured event["declineHlcMs"] == md_event.hlc.wall_ms
    //   - B's captured event matches A's exactly (modulo capture order)
    //   - A captured exactly 1 voting-tier3-mini-public-decline event
    //     (not 2 — originator does not re-enter inbound dispatch).
    //   - B captured exactly 1 voting-tier3-mini-public-decline event
    //     (the peer-inbound emit).
    todo!("Task 2 Step 2 — fill in")
}
```

- [ ] **Step 3: Run the test, confirm it fails (compile error or todo panic)**

```bash
cd src-tauri && set -o pipefail && cargo nextest run --locked --features test-fixtures -E 'test(voting_tier3_mini_public_decline_emits)' 2>&1 | tail -30
```

Expected: FAIL or compile error.

- [ ] **Step 4: Fill in the test body using the kd=ts harness as pattern source**

Read `src-tauri/tests/community_voting_tier3_secret_kd_ts_emission_integration.rs` for the exact engine-construction recipe (`MockAppHandle`, `MockMembershipResolver`, signing keys, log fixtures). Use the same harness setup, swap kd=ts for kd=md.

- [ ] **Step 5: Run the test, confirm it passes**

```bash
cd src-tauri && set -o pipefail && cargo nextest run --locked --features test-fixtures -E 'test(voting_tier3_mini_public_decline_emits)' 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 6: Repeat Steps 2–5 for `voting_tier3_draft_candidate_emits_on_originator_and_peer`**

Setup advances the poll past sortition into Drafting (the apply rules require it), then publishes a kd=dc with `DraftCandidatePayload { poll_id, text: "test candidate" }`. Asserts payload `eventHash` matches `event_hash_of(dc_event)` and `candidateText == "test candidate"`.

- [ ] **Step 7: Repeat Steps 2–5 for `voting_tier3_draft_approval_emits_on_originator_and_peer`**

Setup advances poll into Drafting + publishes a kd=dc + then publishes a kd=da targeting that candidate. Asserts payload `targetEventHash` matches the kd=dc's hash and `approver == hex(actor.0)`.

- [ ] **Step 8: Run full backend gates**

```bash
cd src-tauri && set -o pipefail && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -30
```

Expected: all gates green modulo the 28 known orphan failures.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/tests/community_voting_tier3_granular_events_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-319): two-engine integration for granular Tier 3 events

Adds 3 integration tests covering originator-self-emit
(publish_event path) and peer-inbound emit (process_inbound_dispatch
path) for voting-tier3-mini-public-decline, voting-tier3-draft-candidate,
voting-tier3-draft-approval. Mirrors the kd=ts emission test harness
shipped in ZEB-295 Phase 6.

Each test asserts payload keys, hex-encoded actor / event-hash, and
exactly-once emit semantics on both engines.

ZEB-319 progress (2/5): backend integration tests.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Frontend — TS payload types + adapter subscribers

**Files:**
- Modify: `src/lib/types/voting.ts` (~line 641, after `Tier3TallyShareAppliedPayload`)
- Modify: `src/lib/voting-adapter.ts` (~line 304, after `subscribeTier3TallyShareApplied`)

**Architectural anchor:** `Tier3TallyShareAppliedPayload` interface + `subscribeTier3TallyShareApplied` method (PR #155 ZEB-295 Phase 6 Task 11). Identical shape.

- [ ] **Step 1: Add the three payload interfaces in `src/lib/types/voting.ts`**

Place after the existing `Tier3TallyShareAppliedPayload` block (locate via `grep -n "Tier3TallyShareAppliedPayload" src/lib/types/voting.ts`).

```typescript
/**
 * ZEB-319: payload of `voting-tier3-mini-public-decline` Tauri event.
 * Emitted when a kd=md MiniPublicDecline event is successfully applied
 * to a Tier 3 poll's projection. Frontend subscribers refetch poll
 * detail + summaries on match.
 */
export interface Tier3MiniPublicDeclinePayload {
  pollId: string;
  communityId: string;
  decliner: string;
  declineHlcMs: number;
}

/**
 * ZEB-319: payload of `voting-tier3-draft-candidate` Tauri event.
 * Emitted when a kd=dc DraftCandidate event is successfully applied
 * to a Tier 3 poll's projection.
 */
export interface Tier3DraftCandidatePayload {
  pollId: string;
  communityId: string;
  proposer: string;
  eventHash: string;
  candidateText: string;
}

/**
 * ZEB-319: payload of `voting-tier3-draft-approval` Tauri event.
 * Emitted when a kd=da DraftApproval event is successfully applied
 * to a Tier 3 poll's projection.
 */
export interface Tier3DraftApprovalPayload {
  pollId: string;
  communityId: string;
  approver: string;
  targetEventHash: string;
}
```

- [ ] **Step 2: Add the three subscriber methods in `src/lib/voting-adapter.ts`**

Locate `subscribeTier3TallyShareApplied` (around line 304). Append three new methods following the same shape:

```typescript
/**
 * ZEB-319: subscribe to mid-stage sortition-decline events.
 * @returns an UnlistenFn — call to remove the listener.
 */
async subscribeTier3MiniPublicDecline(
  handler: (payload: Tier3MiniPublicDeclinePayload) => void,
): Promise<UnlistenFn> {
  return listen<Tier3MiniPublicDeclinePayload>(
    'voting-tier3-mini-public-decline',
    (event) => handler(event.payload),
  );
}

/**
 * ZEB-319: subscribe to mid-stage draft-candidate proposals.
 */
async subscribeTier3DraftCandidate(
  handler: (payload: Tier3DraftCandidatePayload) => void,
): Promise<UnlistenFn> {
  return listen<Tier3DraftCandidatePayload>(
    'voting-tier3-draft-candidate',
    (event) => handler(event.payload),
  );
}

/**
 * ZEB-319: subscribe to mid-stage draft-approval signals.
 */
async subscribeTier3DraftApproval(
  handler: (payload: Tier3DraftApprovalPayload) => void,
): Promise<UnlistenFn> {
  return listen<Tier3DraftApprovalPayload>(
    'voting-tier3-draft-approval',
    (event) => handler(event.payload),
  );
}
```

**Import + return-type verification (before writing):**
- Verify the exact `listen` / `UnlistenFn` import shape used by `subscribeTier3TallyShareApplied` (Promise vs sync return, callback signature). Match it precisely.
- If the existing method uses a non-async signature, match that style instead.

- [ ] **Step 3: Run frontend gates**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: zero TS errors, all suites green.

- [ ] **Step 4: Commit**

```bash
git add src/lib/types/voting.ts src/lib/voting-adapter.ts
git commit -m "$(cat <<'EOF'
feat(zeb-319): TS types + adapter subscribers for granular events

Adds Tier3MiniPublicDeclinePayload, Tier3DraftCandidatePayload, and
Tier3DraftApprovalPayload interfaces in types/voting.ts, plus three
subscribeTier3* methods on the VotingAdapter that wrap Tauri's
listen() for the corresponding event names.

Mirrors the shape of subscribeTier3TallyShareApplied shipped in
ZEB-295 Phase 6.

ZEB-319 progress (3/5): frontend adapter.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Frontend — wire subscribers in `Tier3ProposalPanel.svelte` + remove polling

**Files:**
- Modify: `src/lib/components/Tier3ProposalPanel.svelte`

**Anchor:** Lines 211–267 (community-switching `$effect` that registers the existing 6 subscribers) and lines 269–284 (the 5 s polling `$effect` to delete).

- [ ] **Step 1: Add three subscriber registrations in the community-switching `$effect`**

After the existing `subscribeTier3TallyShareApplied(...)` registration (~line 261–266) and before the closing `}` of the `$effect` (~line 267), append:

```svelte
// ZEB-319: refetch on mid-stage mutations (replaces the 5s polling
// fallback). Filter by (communityId, pollId) to avoid needless
// refetches in multi-community / multi-poll panels.
unsubscribers.push(
  await adapter.subscribeTier3MiniPublicDecline((p) => {
    if (p.communityId !== communityId) return;
    if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
    // Declines affect the mini-public size shown in the summary list
    // (backup promotions), so refetch summaries on any matched community.
    loadSummaries();
  }),
);
unsubscribers.push(
  await adapter.subscribeTier3DraftCandidate((p) => {
    if (p.communityId !== communityId) return;
    if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
  }),
);
unsubscribers.push(
  await adapter.subscribeTier3DraftApproval((p) => {
    if (p.communityId !== communityId) return;
    if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
  }),
);
```

**Async-pattern verification (before writing):**
- Look at lines 227–266 to see whether the existing `unsubscribers.push(adapter.subscribeTier3...)` calls are awaited or not. If they're awaited (likely, given the async `listen` signature), prepend `await` as shown. If not, drop the `await`.
- If the `$effect` callback is not async (Svelte 5 effects can be either), promote it to async — or fold the `await`-needed subscriptions into a separate inner async IIFE. Match the surrounding code's pattern exactly.

- [ ] **Step 2: Remove the 5 s polling `$effect` block**

Delete lines 269–284 in their entirety (the entire `$effect` block guarding `selectedPollId`, calling `setInterval(..., 5_000)`, with the comment header at lines 269–275).

- [ ] **Step 3: Run frontend gates**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: zero TS errors. Existing `Tier3ProposalPanel.test.ts` continues to pass (no test yet asserts the polling-removal, that's Task 5).

- [ ] **Step 4: Commit**

```bash
git add src/lib/components/Tier3ProposalPanel.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-319): event-driven refetch in Tier3ProposalPanel

Adds three filtered subscribers in the community-switching \$effect
covering mini-public-decline, draft-candidate, and draft-approval
mid-stage mutations. Removes the 5s polling \$effect that ZEB-311
shipped as a stopgap. Panel is now fully event-driven for all 9
Tier 3 Tauri events.

ZEB-319 progress (4/5): panel integration.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Frontend — extend `Tier3ProposalPanel.test.ts`

**Files:**
- Modify: `src/lib/components/__tests__/Tier3ProposalPanel.test.ts`

**Anchor:** Existing test file structure (locate test count via `grep -n "it(" src/lib/components/__tests__/Tier3ProposalPanel.test.ts | wc -l`). The existing tests likely exercise mount + stage-transition event subscribers; we extend with three new assertions.

- [ ] **Step 1: Write failing assertions for the three new behaviours**

Add four new test cases to the existing file:

```typescript
it('no longer schedules 5s setInterval polling once mounted with a selected poll', async () => {
  vi.useFakeTimers();
  // (Mount the panel with a selectedPollId from a stubbed loadDetail.)
  // ...
  const loadDetailMock = vi.spyOn(adapter, 'getTier3PollDetail');
  loadDetailMock.mockClear();
  vi.advanceTimersByTime(10_000);
  // Without polling, advancing 10s should not trigger any additional refetches.
  expect(loadDetailMock).toHaveBeenCalledTimes(0);
  vi.useRealTimers();
});

it('refetches on voting-tier3-mini-public-decline matching selected community + poll', async () => {
  // (Mount panel + select poll. Capture the registered handler from
  //  the adapter mock for subscribeTier3MiniPublicDecline.)
  const handler = capturedSubscribers['voting-tier3-mini-public-decline'];
  const loadDetailMock = vi.spyOn(adapter, 'getTier3PollDetail');
  const loadSummariesMock = vi.spyOn(adapter, 'listTier3Polls');
  loadDetailMock.mockClear();
  loadSummariesMock.mockClear();

  handler({
    communityId: TEST_COMMUNITY_ID,
    pollId: TEST_POLL_ID,
    decliner: 'aa'.repeat(32),
    declineHlcMs: 1234567890,
  });
  await flushPromises();

  expect(loadDetailMock).toHaveBeenCalledWith(TEST_POLL_ID);
  expect(loadSummariesMock).toHaveBeenCalled();
});

it('ignores voting-tier3-mini-public-decline with mismatched communityId', async () => {
  // ... fire handler with a different communityId; expect no calls.
  expect(loadDetailMock).not.toHaveBeenCalled();
  expect(loadSummariesMock).not.toHaveBeenCalled();
});

it('refetches on voting-tier3-draft-candidate + voting-tier3-draft-approval', async () => {
  // ... parallel assertions for the other two event names.
});
```

**Mock-shape verification (before writing):**
- Open the existing test file and note exactly how the existing `subscribeTier3TallyShareApplied` test asserts handler invocation. Mirror that pattern. The `capturedSubscribers` map I sketched is illustrative — use the actual mock shape from the existing test.
- Confirm `flushPromises` is the helper used elsewhere in the file (or `vi.runAllTimersAsync()`, or `tick()` from Svelte). Use whichever the existing tests use.

- [ ] **Step 2: Run frontend gates**

```bash
npx tsc --noEmit
npx vitest run --reporter=verbose 2>&1 | tail -40
```

Expected: 4 new tests visible in output, all PASS.

- [ ] **Step 3: Commit**

```bash
git add src/lib/components/__tests__/Tier3ProposalPanel.test.ts
git commit -m "$(cat <<'EOF'
test(zeb-319): panel asserts event-driven refetch + no polling

Adds Vitest cases verifying Tier3ProposalPanel:
- Does NOT schedule a 5s setInterval after mount.
- Refetches detail + summaries on voting-tier3-mini-public-decline
  matching the selected (communityId, pollId).
- Ignores events with mismatched communityId.
- Refetches on voting-tier3-draft-candidate + -draft-approval.

ZEB-319 progress (5/5): frontend test coverage.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Final 5-gate sweep + push + PR creation

**Files:** None modified.

- [ ] **Step 1: Final backend sweep**

```bash
cd src-tauri && set -o pipefail && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -30
```

Expected: green modulo 28 orphan failures.

- [ ] **Step 2: Final frontend sweep**

```bash
npx tsc --noEmit && npx vitest run
```

Expected: zero TS errors, all Vitest green.

- [ ] **Step 3: Push branch**

```bash
git push -u origin zeb-319-tier3-granular-events
```

- [ ] **Step 4: Open PR**

```bash
gh pr create --title "ZEB-319: Tier 3 granular post-apply events" --body "$(cat <<'EOF'
## Summary

Replaces the 5 s polling fallback in `Tier3ProposalPanel.svelte` with three new mid-stage Tauri events:

- `voting-tier3-mini-public-decline` — fires on `kd=md` apply (sortition decline → backup promotion)
- `voting-tier3-draft-candidate` — fires on `kd=dc` apply (drafting-stage candidate proposal)
- `voting-tier3-draft-approval` — fires on `kd=da` apply (drafting-stage approval signal)

Completes the event-driven Tier 3 UI pattern that [ZEB-294](https://linear.app/zeblith/issue/ZEB-294) (`voting-tier3-deliberation-statement-created` / `-deliberation-vote-cast`) and [ZEB-295](https://linear.app/zeblith/issue/ZEB-295) (`voting-tier3-tally-share-applied`) established. Observers now see all 9 Tier 3 mutations within ~1 s of CRDT delivery with zero polling IPC overhead.

Spec: `docs/specs/2026-05-22-zeb-319-tier3-granular-events-design.md` (commit `af4e042`).
Plan: `docs/plans/2026-05-22-zeb-319-tier3-granular-events-plan.md`.

## Changes

**Backend:**
- Three new branches in `maybe_emit_tier3_lifecycle_events` (community_voting_log_engine.rs).
- Each follows the existing kd=ds/dv/ts acceptance-check pattern: emit only when the projection state confirms the event actually landed.
- Payload keys use `serde_json::json!{}` literals with camelCase strings — matches the precedent set by the three existing mid-stage events.

**Frontend:**
- 3 new payload interfaces in `types/voting.ts`.
- 3 new `subscribeTier3*` methods in `voting-adapter.ts`.
- 3 new filtered subscribers in `Tier3ProposalPanel.svelte`, replacing the 5 s polling `$effect` (deleted entirely).

**Tests:**
- 3 backend integration tests (one per event kind), two-engine harness covering both originator-self-emit + peer-inbound emit. Mirrors the kd=ts emission test from PR #155.
- 4 new Vitest cases on `Tier3ProposalPanel`: no polling scheduled; events refetch on community+poll match; events ignored on mismatched community; same coverage for draft-candidate + draft-approval.

## Test plan

- [ ] `cd src-tauri && cargo fmt --all -- --check` — clean
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — zero warnings
- [ ] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` — green modulo 28 known orphan failures (folder_ingest, mint, rename_content from prior PRs)
- [ ] `npx tsc --noEmit` — clean
- [ ] `npx vitest run` — green

## References

- Parent: [ZEB-293](https://linear.app/zeblith/issue/ZEB-293) (Phase 4 Tier 3a)
- Umbrella: [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) (voting/polling)
- Predecessor pattern PRs: [#152](https://github.com/zeblithic/harmony-client/pull/152) (ZEB-311 5s polling stopgap), [#153](https://github.com/zeblithic/harmony-client/pull/153) (ZEB-294 deliberation events), [#155](https://github.com/zeblithic/harmony-client/pull/155) (ZEB-295 tally-share event)

Closes ZEB-319

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Capture the PR URL + number** for the autonomous bot-review loop handoff.

---

## Self-Review

**Spec coverage check** (against `docs/specs/2026-05-22-zeb-319-tier3-granular-events-design.md`):
- §3.1 (three events + payloads): Task 1 Steps 3+5+6.
- §3.2 (dual-emit convention): Task 1 sits inside `maybe_emit_tier3_lifecycle_events` which is called from both publish + inbound paths; Task 2 verifies both sides.
- §3.3 (helper shape): superseded by the **inline-branch** structural choice (corrected in Task 1 preamble); spec intent preserved.
- §3.4–3.5 (call sites): Task 1 inserts adjacent to the existing kd=ts/ds/dv branches.
- §3.6 (frontend adapter): Task 3.
- §3.7 (panel integration): Task 4.
- §3.8 (invariants): tests in Task 2 cover no-double-emit + no-emit-on-drop.
- §4.1 (backend integration tests): Task 2.
- §4.2 (backend unit tests): Task 1 Steps 1+5+6.
- §4.3 (frontend tests): Task 5.
- §4.4 (no wire fixtures): correctly omitted across all tasks.
- §8 (acceptance criteria 1–9): all covered.

**Placeholder scan:** the `unimplemented!` / `todo!` in Task 1 Step 1 + Task 2 Step 2 are deliberate TDD red-state placeholders that the implementer fills in immediately at Step 3/4 — not plan-failure placeholders.

**Type consistency:** payload field names (`pollId`, `communityId`, `decliner`, `declineHlcMs`, `proposer`, `eventHash`, `candidateText`, `approver`, `targetEventHash`) are identical between Task 1 (Rust json literals) and Task 3 (TS interfaces).
