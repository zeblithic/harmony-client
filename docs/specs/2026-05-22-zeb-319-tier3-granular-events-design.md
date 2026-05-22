# ZEB-319 Tier 3 governance — granular post-apply Tauri events

**Linear:** [ZEB-319](https://linear.app/zeblith/issue/ZEB-319) (parent [ZEB-293](https://linear.app/zeblith/issue/ZEB-293) → [ZEB-289](https://linear.app/zeblith/issue/ZEB-289))
**Branch:** `zeb-319-tier3-granular-events` off `origin/main` `9ab41f6` (post-ZEB-295)
**Status:** Approved 2026-05-22 (Jake), inline design summary in conversation

## 1. Goal

Replace the 5-second polling fallback in `Tier3ProposalPanel.svelte` with three new mid-stage Tauri events emitted from the voting log engine, so observers see sortition declines, draft-candidate proposals, and draft-approval signals within ~1 s of CRDT delivery instead of after a polling interval — and with zero IPC traffic when nothing has changed.

This completes the event-driven Tier 3 UI pattern that ZEB-294 (`voting-tier3-deliberation-statement-created`, `voting-tier3-deliberation-vote-cast`) and ZEB-295 (`voting-tier3-tally-share-applied`) established for the other mid-stage event kinds.

## 2. Background

The Tier 3 UI surface currently emits 8 Tauri events from `community_voting_log_engine.rs`:

| Category | Event | Fires on |
|---|---|---|
| Stage transition | `voting-tier3-poll-created` | kd=pc apply |
| Stage transition | `voting-tier3-sortition-complete` | kd=ss apply |
| Stage transition | `voting-tier3-drafting-open` | stage→Drafting |
| Stage transition | `voting-tier3-ratification-open` | stage→Ratification |
| Stage transition | `voting-tier3-finalized` | kd=rs apply |
| Mid-stage | `voting-tier3-deliberation-statement-created` | kd=ds apply |
| Mid-stage | `voting-tier3-deliberation-vote-cast` | kd=sv apply |
| Mid-stage | `voting-tier3-tally-share-applied` | kd=ts apply |

Three CRDT event kinds — **`kd=md`** (MiniPublicDecline), **`kd=dc`** (DraftCandidate), **`kd=da`** (DraftApproval) — currently emit no Tauri event. As a result, an observer with a poll expanded in `Tier3ProposalPanel.svelte` would see stale roster + approval data until a stage transition fired or they mutated the poll themselves.

PR #152 (ZEB-311) shipped a stopgap: a 5-second `setInterval` calling `loadDetail` + `loadSummaries` while a poll is expanded (`Tier3ProposalPanel.svelte:276-284`). Cursor Bugbot R8 flagged this as medium-severity UX/efficiency drag and filed ZEB-319 for the proper fix.

## 3. Design

### 3.1 Three new Tauri events

| Tauri event name | Payload (camelCase, serde) | CRDT event kind |
|---|---|---|
| `voting-tier3-mini-public-decline` | `{ pollId, communityId, decliner, declineHlcMs }` | `PollEventKindCode::MiniPublicDecline` |
| `voting-tier3-draft-candidate` | `{ pollId, communityId, proposer, eventHash, candidateText }` | `PollEventKindCode::DraftCandidate` |
| `voting-tier3-draft-approval` | `{ pollId, communityId, approver, targetEventHash }` | `PollEventKindCode::DraftApproval` |

**Payload field semantics:**

- `pollId`: hex-string (`PollId` debug-format) of the poll the event targets.
- `communityId`: hex-string of `self.community_id` (matches the existing pattern in `Tier3DeliberationStatementCreatedPayload`).
- `decliner` / `proposer` / `approver`: hex-string of the actor's `OwnerAddr` (Ed25519 public-key hash).
- `declineHlcMs`: wall-time component (u64) of the kd=md event's HLC, useful for ordering backup-promotion UI.
- `eventHash`: 32-byte hex of the kd=dc event's signed-payload hash — same value the kd=da's `target_event_hash` field references.
- `targetEventHash`: 32-byte hex of the kd=dc that the approval targets — joinable to the kd=dc event's `eventHash`.
- `candidateText`: the proposal text from the kd=dc payload (clamped to the existing max-length pre-apply gate, no extra truncation in the emit).

**Serialization convention:** all payload structs use `#[derive(Serialize)]` + `#[serde(rename_all = "camelCase")]`, matching the eight existing payloads (e.g. `Tier3TallyShareAppliedPayload`).

### 3.2 Emit call sites — dual-emit convention

Each new emit helper must be invoked from **both** the originator path and the peer-inbound path, matching the convention established by `maybe_emit_tally_share` (ZEB-295 C7/F24) and `maybe_emit_deliberation_statement`/`vote_cast` (ZEB-294):

1. **`publish_event`** (originator-self-emit): after `apply_with_snapshot` returns Ok, when the published event's kind matches one of the three new kinds.
2. **`process_inbound_dispatch`** (peer-event): in the post-apply hook section (`community_voting_log_engine.rs:2456+`), after the existing lifecycle/tally-share emits, gated by `event.tier == Tier::Sortition && event.kind == PollEventKindCode::{MiniPublicDecline, DraftCandidate, DraftApproval}`.

The emit helpers themselves are responsible for:

- Re-acquiring `voting_log` briefly to read the just-applied state (so payload reflects the actual stored data, not the wire-event payload).
- Returning silently if `app_handle.is_none()` (test-mode without Tauri).
- Returning silently if the poll is no longer present (e.g., racing log truncation).
- Logging at `warn!` on emit failure, never propagating (mirrors the existing emit pattern).

### 3.3 Helper function shape

Three new private methods on `VotingLogEngine`, mirroring `maybe_emit_tally_share`'s signature scaffolding:

```rust
async fn maybe_emit_mini_public_decline(
    self: &Arc<Self>,
    pid: &PollId,
    event: &SignedVotingEvent,
) { /* ... */ }

async fn maybe_emit_draft_candidate(
    self: &Arc<Self>,
    pid: &PollId,
    event: &SignedVotingEvent,
) { /* ... */ }

async fn maybe_emit_draft_approval(
    self: &Arc<Self>,
    pid: &PollId,
    event: &SignedVotingEvent,
) { /* ... */ }
```

Each:

1. Returns early if `self.app_handle.is_none()`.
2. Decodes the event's CBOR payload (`MiniPublicDeclinePayload` / `DraftCandidatePayload` / `DraftApprovalPayload` from `community_voting_core`) — payload was already validated by apply, so decode failure is a programmer error (panic-with-log).
3. Pulls the actor's owner-addr from `event.signing_owner_address()` (the same accessor existing emits use) or directly from the wire payload's actor field.
4. Builds the camelCase payload struct and calls `app_handle.emit(EVENT_NAME, &payload)`.
5. Logs failure non-fatally and returns.

### 3.4 Helper call sites in `publish_event`

`publish_event` already dispatches per-kind branches after `apply_with_snapshot`. Add three new conditional emits adjacent to the existing tally-share emit. The match should be on `event.kind` against `PollEventKindCode::{MiniPublicDecline, DraftCandidate, DraftApproval}` and only fire when `event.tier == Tier::Sortition` (Tier 3).

### 3.5 Helper call sites in `process_inbound_dispatch`

Add a new gated block after the existing `maybe_emit_tier3_lifecycle_events` call (at `community_voting_log_engine.rs:~2499`) and before the `TallyShare | PollClose` block:

```rust
if event.tier == Tier::Sortition {
    match event.kind {
        PollEventKindCode::MiniPublicDecline =>
            self.maybe_emit_mini_public_decline(&applied_poll_id, &event).await,
        PollEventKindCode::DraftCandidate =>
            self.maybe_emit_draft_candidate(&applied_poll_id, &event).await,
        PollEventKindCode::DraftApproval =>
            self.maybe_emit_draft_approval(&applied_poll_id, &event).await,
        _ => {}
    }
}
```

### 3.6 Frontend adapter changes

`src/lib/voting-adapter.ts`:

```typescript
subscribeTier3MiniPublicDecline(handler: (p: Tier3MiniPublicDeclinePayload) => void): UnlistenFn
subscribeTier3DraftCandidate(handler: (p: Tier3DraftCandidatePayload) => void): UnlistenFn
subscribeTier3DraftApproval(handler: (p: Tier3DraftApprovalPayload) => void): UnlistenFn
```

Each registers a Tauri `listen` against the matching event name and returns the unsubscribe closure. Same shape as the existing `subscribeTier3TallyShareApplied` (voting-adapter.ts:497-507).

`src/lib/types/voting.ts`:

```typescript
export interface Tier3MiniPublicDeclinePayload {
  pollId: string;
  communityId: string;
  decliner: string;
  declineHlcMs: number;
}

export interface Tier3DraftCandidatePayload {
  pollId: string;
  communityId: string;
  proposer: string;
  eventHash: string;
  candidateText: string;
}

export interface Tier3DraftApprovalPayload {
  pollId: string;
  communityId: string;
  approver: string;
  targetEventHash: string;
}
```

### 3.7 `Tier3ProposalPanel.svelte` integration

In the existing community-switching `$effect` block (~line 211), after the `subscribeTier3TallyShareApplied` registration, add three filtered subscribers:

```typescript
unsubscribers.push(
  adapter.subscribeTier3MiniPublicDecline((p) => {
    if (p.communityId !== communityId) return;
    if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
    // declines affect roster shown in the summary list too (mini-public size)
    loadSummaries();
  }),
);
unsubscribers.push(
  adapter.subscribeTier3DraftCandidate((p) => {
    if (p.communityId !== communityId) return;
    if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
  }),
);
unsubscribers.push(
  adapter.subscribeTier3DraftApproval((p) => {
    if (p.communityId !== communityId) return;
    if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
  }),
);
```

**Filter shape:** same `(communityId match, pollId match for detail, refetch summaries on any community match)` as the existing tally-share subscriber. Declines also call `loadSummaries()` because backup promotions can change the effective mini-public size shown in the list view (the others only affect detail).

**Remove the polling block:** delete the entire `$effect` at lines 276–284 (the 5 s `setInterval` block) and its comment header at lines 269–275.

### 3.8 Behaviour invariants

- **No double-emit on round-trip:** an event the local node publishes goes through `publish_event` (originator emit) and does NOT re-enter `process_inbound_dispatch` (which is for peer packets only). One emit per event.
- **No emit on drop:** the existing apply-time validation (verify_event power gate, ZEB-320 silent-drop semantics) runs before the post-apply hook. Drops are not emitted.
- **Idempotent re-apply:** kd=da has an idempotent-on-repeat-actor branch in apply (`community_voting_tier3.rs:~705`). When the second apply is a no-op, the event is still considered "applied" by the dispatcher and an emit fires. This is acceptable — frontend handlers are idempotent (`refetchSelected` deduplicates in-flight detail fetches via `loadDetail`'s race protection at line 87+).
- **Cross-community filter:** payloads carry `communityId` so a multi-community panel can ignore irrelevant traffic — same convention as `Tier3TallyShareAppliedPayload`.

## 4. Tests

### 4.1 Backend integration (3 tests)

Add `src-tauri/tests/community_voting_tier3_granular_events_integration.rs`. For each of the three event kinds:

1. Spin up a two-engine harness (mirror the helper pattern in `community_voting_tier3_secret_kd_ts_emission_integration.rs`).
2. Engine A publishes a kd=pc, sortition+drafting+ratification windows advance via test seam, then publishes a kd=md / kd=dc / kd=da.
3. Engine B receives the event via `process_inbound`.
4. Assert engine B's `MockAppHandle::captured_events()` contains exactly one matching `voting-tier3-{mini-public-decline,draft-candidate,draft-approval}` event with the correct payload.
5. Negative assertion: same engine A (originator-self-emit) captured the event once on `publish_event` and NOT again on inbound replay.

### 4.2 Backend unit (1 file, per-kind payload tests)

In `community_voting_log_engine.rs` (or a sibling unit-test module), one test per payload struct asserting `serde_json::to_value` produces exactly the expected camelCase keys and shapes — protects against accidental field rename / addition.

### 4.3 Frontend unit (Vitest)

Extend `src/lib/components/__tests__/Tier3ProposalPanel.test.ts`:

1. Mount panel with a selected poll.
2. Verify polling-`setInterval` is NOT scheduled (assert `vi.useFakeTimers(); vi.advanceTimersByTime(10_000); expect(loadDetail).not.toHaveBeenCalledExtraTimes`).
3. Fire mock `voting-tier3-mini-public-decline` / `-draft-candidate` / `-draft-approval` Tauri events matching the selected community + poll → expect `loadDetail` called.
4. Fire same events with mismatched `communityId` → expect no additional `loadDetail` calls.

### 4.4 Wire-format CBOR fixtures

**Not added.** The underlying CRDT events (`MiniPublicDeclinePayload`, `DraftCandidatePayload`, `DraftApprovalPayload`) already have CBOR fixtures in the existing wire-format pin suite. Tauri-event JSON payloads are not on-the-wire — their contract is the serde-`Serialize` struct + matching TS type, consistent with the other three mid-stage Tier 3 events.

### 4.5 Regression coverage

- The existing 8 events keep firing — exercised by the existing integration suites in `community_voting_tier3_integration.rs`, `community_voting_tier3_polis_integration.rs`, `community_voting_tier3_secret_kd_ts_emission_integration.rs`.
- The existing `Tier3ProposalPanel.test.ts` test of stage-transition refetches must continue to pass.

## 5. Out of scope

1. **Wire-format CBOR pinning** for the new Tauri-event JSON payloads — payload contract is serde struct + TS type, same as existing 3 mid-stage events; no precedent for JSON-payload byte-pinning here.
2. **60 s safety-net polling** or **`visibilitychange` refetch** — explicitly chosen against (Q1, 2026-05-22). Events are the single source of UI refresh post-ZEB-319.
3. **Granular emit on draft-approval *removal*** — there's no kd=da removal event in the current CRDT (approval is append-only); nothing to emit.
4. **Backwards-compat with the 5 s polling** — removed entirely. Any callers depending on the panel polling get one-time refresh on mount + event-driven thereafter.
5. **ZEB-293 epic closure** — handled manually post-merge (verify no other open children, mark Done in Linear). PR body uses `Closes ZEB-319` as the only bare ref; ZEB-289 and ZEB-293 stay as markdown links to avoid auto-cascade.
6. **Tier 2 / Tier 1 granular events** — Tier 1 + 2 panels don't have the same expanded-detail polling concern; out of scope here.

## 6. Files touched

**Backend (4 files):**

- `src-tauri/src/community_voting_log_engine.rs` — 3 new payload structs, 3 new helper methods, 2 new call sites (publish_event + process_inbound_dispatch), associated unit tests.
- `src-tauri/tests/community_voting_tier3_granular_events_integration.rs` — new test file, 3 integration tests.

**Frontend (3 files):**

- `src/lib/types/voting.ts` — 3 new payload interfaces.
- `src/lib/voting-adapter.ts` — 3 new `subscribeTier3*` methods.
- `src/lib/components/Tier3ProposalPanel.svelte` — 3 new subscribers in the community-switching `$effect`; remove the 5 s polling `$effect` + its comment block.
- `src/lib/components/__tests__/Tier3ProposalPanel.test.ts` — extend with 4 new assertions (no polling, 3 event-driven refetches).

## 7. Hard rules (memory-locked)

- 5 backend gates from `src-tauri/`: `cargo fmt --all -- --check` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- 2 frontend gates from repo root via npx: `npx tsc --noEmit` + `npx vitest run`.
- No worktrees — `git checkout -b` in main repo only (branch `zeb-319-tier3-granular-events` already created).
- Pull-before-work satisfied: branch is off `origin/main` `9ab41f6`.
- Tauri IPC: `snake_case` Rust ↔ `camelCase` JS (serde `rename_all = "camelCase"`).
- Tauri error extraction (frontend): `e instanceof Error ? e.message : String(e)`.
- Per `feedback_implementer_gate_time_budget`: commit-before-gate + 10-min wall-clock kill switch + DONE_WITH_CONCERNS escape hatch per implementer task.
- Per `feedback_cargo_fmt_gate`: include `cargo fmt` in every implementer verification, not just clippy.
- Per `feedback_linear_pr_auto_close`: PR body uses `Closes ZEB-319`; parent ZEB-289 + ZEB-293 stay markdown-linked.
- Per `feedback_second_order_correctness_review`: the new emits do not mutate state — they're pure post-apply observers. No second-order field-readers to enumerate.
- Per `feedback_test_drift_is_our_fault`: pre-existing orphan failures (~28 nextest baseline from ZEB-295 PR) are non-blocking; new failures introduced by this PR are blocking.

## 8. Acceptance criteria

1. Three new Tauri events fire on both the originator path (`publish_event`) and the peer-inbound path (`process_inbound_dispatch`) for the corresponding CRDT event kinds.
2. Payload structs serialize with the exact camelCase keys specified in §3.1, asserted by unit tests.
3. `Tier3ProposalPanel.svelte` subscribes to all three events, filters by `(communityId, pollId)`, and refetches detail/summaries on match.
4. The 5 s polling `$effect` is removed from `Tier3ProposalPanel.svelte`.
5. Three new backend integration tests pass: two-engine peer-delivery → observer receives the corresponding Tauri event.
6. Frontend Vitest assertion: no polling `setInterval` is scheduled in the panel; events are the sole refresh trigger.
7. Existing 8 Tier 3 events continue to fire (regression coverage from existing integration suites).
8. 5 backend + 2 frontend CI gates green.
9. PR body closes ZEB-319, markdown-links ZEB-289 + ZEB-293; post-merge, ZEB-293 manually marked Done after verifying no other open children.

## 9. Risks + mitigations

- **Double-emit on local round-trip:** mitigated by `publish_event`/`process_inbound_dispatch` being mutually exclusive on the originator (locally-published events do not re-enter the inbound path). Existing tally-share + deliberation emits prove this convention; we follow it.
- **Stale UI on dropped Tauri event:** mitigated by acceptance of the trade-off (Q1 answer). If a Tauri event is missed, the next stage-transition event fires `loadDetail` + `loadSummaries`, recovering staleness. The 5 s polling fallback is intentionally removed.
- **High-frequency emit on many quick-fire kd=da:** approvals are 1-per-actor-per-candidate (idempotent on repeat), so worst case is `O(mini_public_size × candidates)` emits over the drafting window — a few hundred for a large community. Each emit is a JSON object < 200 B + an IPC bridge call; negligible.
- **CRDT-replay backfill emit storm:** on initial sync of a community, the engine replays its full log, which would emit Tauri events for every historical kd=md/dc/da. This matches the existing emit behaviour for kd=ds/sv/ts — accepted as-is; the panel deduplicates via `selectedPollId` filter, and `loadSummaries`/`loadDetail` have race protection.

## 10. References

- Predecessor PR: [#152](https://github.com/zeblithic/harmony-client/pull/152) (ZEB-311) shipped the 5 s polling stopgap.
- Predecessor PR: [#155](https://github.com/zeblithic/harmony-client/pull/155) (ZEB-295) shipped `voting-tier3-tally-share-applied` — closest pattern source.
- Predecessor PR: [#153](https://github.com/zeblithic/harmony-client/pull/153) (ZEB-294) shipped `voting-tier3-deliberation-statement-created` + `-deliberation-vote-cast` — also pattern source.
- Predecessor PR: [#154](https://github.com/zeblithic/harmony-client/pull/154) (ZEB-320) — apply_event drop-path watermark fix; relevant because emits hang off "apply succeeded" semantics.
- Spec: [ZEB-289 umbrella](docs/specs/2026-05-16-zeb-289-voting-polling-design.md) §8 verify rules (B5, T1, T2) — the apply gates that determine "applied".
- Memory: `feedback_linear_pr_auto_close`, `feedback_cargo_fmt_gate`, `feedback_implementer_gate_time_budget`, `feedback_test_drift_is_our_fault`.
