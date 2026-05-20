# ZEB-310 Phase 4a-main IPCs Design

**Status:** Design refinement of [ZEB-289 umbrella spec](2026-05-16-zeb-289-voting-polling-design.md) §6.7. Backend mechanism settled in [ZEB-309](2026-05-20-zeb-309-phase4a-main-design.md) (merged in PR #148 / commit `0902ff2`).

**Scope:** Tauri command + event surface + frontend TypeScript wrapper + signed-event builders + engine-auto-orchestration for terminal events, so [ZEB-311](https://linear.app/zeblith/issue/ZEB-311) UI can drive Tier 3 polls end-to-end.

## Summary

| Layer | Additions |
|---|---|
| `community_voting_core.rs` | 9 `pub fn build_signed_*_tier3` constructors |
| `community_voting_log_engine.rs` | post-apply orchestration for kd=sf / kd=cl / kd=rs |
| `lib.rs` | 6 `#[tauri::command]` handlers + 5 emit sites + 5 payload structs |
| `src/lib/voting-adapter.ts` | 6 IPC methods + 5 subscriber methods + connectAdapter wiring |
| `src/lib/types/voting.ts` | 5 payload typedefs + `CreateTier3ProposalArgs` |
| `tests/community_voting_tier3_ipc_integration.rs` | E2E IPC-driven test file (new) |
| `tests/wire_format_voting_tier3_fixtures.rs` | Pin fixtures for the 3 engine-auto kinds (sf / cl / rs) |

## Open-question resolutions

| Q | Resolution |
|---|---|
| Q1 IPC scope for kd=da/sf/cl/rs | **+1 IPC for kd=da; engine-auto for kd=sf/cl/rs** (user-approved, 2026-05-20) |
| Q2 Frontend lib factoring | Extend `voting-adapter.ts` (consistent with existing Tier 1 + Tier 2 + delegation surface; ~720 LOC total) |
| Q3 Integration test strategy | **New file** `community_voting_tier3_ipc_integration.rs` (keep ZEB-309's direct-engine tests as-is) |
| Q4 Event payload completeness | Payloads carry enough to render UI without re-querying (poll_id, community_id, channel_id, plus kind-specific data) |
| Q5 Decline-reason encoding | Pass-through 2-char `String` (validated by `validate_decline_reason`); UI picks tags from dropdown |
| Q6 IPC eligibility shape | Decompose into flat `min_power: u8` + `min_vouching_depth: Option<u8>` (mirrors `voting_create_tier1_poll`) |
| Q7 Incentive-mode encoding | 2-char `String` matching wire-format `im` field: `"se"` / `"ab"` / `"co"` / `"dp"` |
| Q8 Builder location | Move test-only `build_*` helpers from `community_voting_tier3_integration.rs` to `pub fn` in `community_voting_core.rs` (parity with `build_signed_poll_create_tier1`) |

## IPC surface (6 commands)

All `#[tauri::command]` handlers live in `src-tauri/src/lib.rs`. Parameters in `snake_case` Rust; JS callers use `camelCase` (auto-converted at the boundary per `feedback_tauri_error_extraction`).

### 1. `voting_create_tier3_proposal`

```rust
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn voting_create_tier3_proposal<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,                      // 32 hex
    channel_id: String,                        // 32 hex
    proposal_text: String,
    sortition_size: u16,
    deliberation_window_seconds: u32,
    drafting_window_seconds: u32,
    ratification_window_seconds: u32,
    incentive_mode: String,                    // "se" / "ab" / "co" / "dp"
    min_power: u8,
    min_vouching_depth: Option<u8>,
    retry_of: Option<String>,                  // 64 hex if Some
) -> Result<String, String>                    // returns PollId hex (64 chars)
```

Pre-flight ordering (per `feedback_metadata_before_irreversible_write`):

1. Decode hex → `SpaceId` / `ChannelId` / `PollId`.
2. Build `Tier3PollConfigPayload` (validate via `validate_tier3_poll_config`).
3. Snapshot eligible electorate.
4. Check proposer eligibility (`check_eligibility`).
5. Reserve next HLC.
6. Mint signed event via `build_signed_poll_create_tier3`.
7. `VotingLogEngine::publish_event(event)` — local apply + Zenoh broadcast.
8. Emit `voting-tier3-poll-created`.
9. Return `PollId` hex.

### 2. `voting_submit_deliberation_statement`

```rust
async fn voting_submit_deliberation_statement<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    poll_id: String,
    text: String,                              // ≤512 chars, validated
) -> Result<String, String>                    // returns event_hash hex
```

Phase 5 wires Pol.is clustering; this phase emits valid kd=ds events that store the statement but no clustering is run yet.

### 3. `voting_propose_draft_candidate`

```rust
async fn voting_propose_draft_candidate<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    poll_id: String,
    candidate_text: String,                    // ≤512 chars
) -> Result<String, String>                    // returns candidate_event_hash hex (32 bytes)
```

Proposer implicitly approves their own candidate (apply path handles this; no separate kd=da needed). Returns the candidate's event_hash so the caller can subsequently approve via `voting_approve_draft_candidate`.

### 4. `voting_approve_draft_candidate` (NEW — kd=da)

```rust
async fn voting_approve_draft_candidate<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    poll_id: String,
    candidate_event_hash: String,              // 64 hex
) -> Result<(), String>
```

Verify path requires actor ∈ mini-public AND `candidate_event_hash` exists in `Tier3PollState::draft_candidates` (per `verify_da_candidate_exists`).

### 5. `voting_decline_sortition`

```rust
async fn voting_decline_sortition<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    poll_id: String,
    reason: Option<String>,                    // 2-char tag or None
) -> Result<(), String>
```

Engine post-apply hook detects `decline_count >= backup_pool_size` AND local node is the proposer → triggers kd=sf orchestration (see Engine-auto section below).

### 6. `voting_cast_ratification_ballot`

```rust
async fn voting_cast_ratification_ballot<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    poll_id: String,
    scores: Vec<u8>,                           // 0-5 per ratification candidate
) -> Result<(), String>
```

Validated by `validate_ratification_ballot` (length matches `ratification_candidates_ordering`, all values 0-5).

## Tauri events (5)

All payload structs `#[derive(Debug, Clone, Serialize)] #[serde(rename_all = "camelCase")]` per existing convention.

### 1. `voting-tier3-poll-created`

Emitted on apply of kd=cr.

```rust
pub struct VotingTier3PollCreatedPayload {
    pub poll_id: String,                       // 64 hex
    pub channel_id: String,                    // 32 hex
    pub community_id: String,                  // 32 hex
    pub proposer: String,                      // 32 hex OwnerAddr
    pub sortition_size: u16,
    pub deliberation_window_seconds: u32,
    pub drafting_window_seconds: u32,
    pub ratification_window_seconds: u32,
}
```

### 2. `voting-tier3-sortition-complete`

Emitted on apply of kd=ss (engine-auto-published by the first node whose beacon callback fires).

```rust
pub struct VotingTier3SortitionCompletePayload {
    pub poll_id: String,
    pub community_id: String,
    pub primary: Vec<String>,                  // OwnerAddr hex array
    pub backup: Vec<String>,                   // OwnerAddr hex array
}
```

### 3. `voting-tier3-drafting-open`

Emitted on `materialize()` stage 2→3 transition.

```rust
pub struct VotingTier3DraftingOpenPayload {
    pub poll_id: String,
    pub community_id: String,
}
```

### 4. `voting-tier3-ratification-open`

Emitted on `materialize()` stage 3→4 transition.

```rust
pub struct VotingTier3RatificationOpenPayload {
    pub poll_id: String,
    pub community_id: String,
    pub candidate_ordering: Vec<CandidateRefDto>,
}
pub struct CandidateRefDto {
    pub event_hash: String,                    // 64 hex
    pub text: String,
    pub approval_count: u32,
}
```

Includes synthesized status_quo at the end (per ZEB-309 §3 `ratification_candidates_ordering`).

### 5. `voting-tier3-finalized`

Emitted on apply of kd=rs.

```rust
pub struct VotingTier3FinalizedPayload {
    pub poll_id: String,
    pub community_id: String,
    pub winner_event_hash: String,             // 64 hex
    pub winner_text: String,
    pub runner_up_event_hash: Option<String>,
    pub scores_summary: Vec<CandidateScoreDto>,
}
pub struct CandidateScoreDto {
    pub event_hash: String,
    pub total_score: u32,
    pub runoff_votes: u32,
}
```

## Engine-auto-orchestration (terminal events)

Three orchestration paths added to `VotingLogEngine::publish_event` + `apply_with_snapshot`. Each is **race-tolerant**: first valid event wins by HLC LWW; later duplicates are rejected by L1 (lifecycle gate).

### kd=sf — SortitionFailed

**Trigger:** After successful local apply, check:
- Poll is in `Stage::Sortition`.
- `decline_count_at(now) >= primary_size + backup_size`.
- Local `self_owner == Tier3PollMeta.proposer`.

**Action:** Mint signed kd=sf via `build_signed_sortition_failed(self_owner, poll_id, hlc)` → `publish_event`. Apply transitions poll to `Stage::Failed`.

**Reentrancy guard:** Apply rejects kd=sf if `Tier3PollState.stage != Sortition` (L1). Two proposer devices racing both publish kd=sf; first by HLC wins.

### kd=cl — PollClose (Tier 3 path)

**Trigger:** On any apply that touches a Tier 3 poll, check:
- Poll is in `Stage::Ratification`.
- `now_hlc >= meta.created_hlc + (deliberation_window + drafting_window + ratification_window)`.
- `poll_state.close_event_hash.is_none()`.

**Action:** Any member can sign kd=cl (verify rule allows anyone). Mint via `build_signed_poll_close_tier3(self_owner, poll_id, hlc)` → `publish_event`.

**Race-tolerant:** First-valid by HLC wins; later duplicates rejected by L1 (close_event_hash already set).

### kd=rs — PollResult (Tier 3 path)

**Trigger:** On apply of kd=cl (above), check:
- `poll_state.result.is_none()`.

**Action:** Deterministically compute STAR tally via `tally_star(candidates, ballots)`. Build `Tier3PollResultPayload`. Mint via `build_signed_poll_result_tier3(self_owner, poll_id, payload, hlc)` → `publish_event`.

**Race-tolerant:** Determinism guarantees bit-identical payloads from any signer; later duplicates rejected by R2.

### Lazy-vs-timer design choice

These three orchestrations fire as **post-apply hooks**, NOT as background timers. They activate any time:
- Local IPC call triggers apply (proposer signs, voter casts).
- Inbound Zenoh event triggers apply.
- Materialize is called during a fetch.

This means a poll **does not finalize in dead silence** — at least one peer-touch is required to advance terminal stages. This is acceptable for ZEB-310; in practice any active Tier 3 poll will have continuous engagement during ratification.

Out-of-band timer-driven sweeps are deferred (no Linear ticket filed yet; will file if observed lag in real-world use).

## Signed-event builders

Move test-only helpers from `community_voting_tier3_integration.rs` to `pub fn` in `community_voting_core.rs`. Same signature shape as `build_signed_poll_create_tier1`.

```rust
// Already exists (move from test fixture → core):
pub fn build_signed_poll_create_tier3(
    signing_key: &SigningKey,
    actor: OwnerAddr,
    config: &Tier3PollConfigPayload,
    hlc: Hlc,
) -> Result<SignedVotingEvent, String>;

pub fn build_signed_deliberation_statement(/* ... */) -> Result<SignedVotingEvent, String>;
pub fn build_signed_draft_candidate(/* ... */) -> Result<SignedVotingEvent, String>;
pub fn build_signed_draft_approval(/* ... */) -> Result<SignedVotingEvent, String>;
pub fn build_signed_mini_public_decline(/* ... */) -> Result<SignedVotingEvent, String>;
pub fn build_signed_ratification_ballot(/* ... */) -> Result<SignedVotingEvent, String>;
pub fn build_signed_sortition_failed(/* ... */) -> Result<SignedVotingEvent, String>;
pub fn build_signed_poll_close_tier3(/* ... */) -> Result<SignedVotingEvent, String>;
pub fn build_signed_poll_result_tier3(/* ... */) -> Result<SignedVotingEvent, String>;
```

The 6 IPC handlers and the 3 engine-auto paths share these as the **only** signed-event minting surface. Test-only helpers in `community_voting_tier3_integration.rs` become thin wrappers (`pub(crate)` test re-exports) or are deleted.

## Frontend TypeScript layer

### `src/lib/voting-adapter.ts` extensions

6 new methods (mirroring the existing `createTier1Poll` / `castTier1Ballot` shape):

```typescript
async createTier3Proposal(args: CreateTier3ProposalArgs): Promise<string>;
async submitDeliberationStatement(pollId: string, text: string): Promise<string>;
async proposeDraftCandidate(pollId: string, candidateText: string): Promise<string>;
async approveDraftCandidate(pollId: string, candidateEventHash: string): Promise<void>;
async declineSortition(pollId: string, reason?: string): Promise<void>;
async castRatificationBallot(pollId: string, scores: number[]): Promise<void>;
```

5 new subscribers + their backing subscriber-list arrays:

```typescript
subscribeTier3PollCreated(handler: (p: VotingTier3PollCreatedPayload) => void): () => void;
subscribeTier3SortitionComplete(handler: (p: VotingTier3SortitionCompletePayload) => void): () => void;
subscribeTier3DraftingOpen(handler: (p: VotingTier3DraftingOpenPayload) => void): () => void;
subscribeTier3RatificationOpen(handler: (p: VotingTier3RatificationOpenPayload) => void): () => void;
subscribeTier3Finalized(handler: (p: VotingTier3FinalizedPayload) => void): () => void;
```

`connectAdapter()` wires 5 new `adapter.listen()` calls (staged-unlistener pattern preserved).

### `src/lib/types/voting.ts` extensions

```typescript
export interface CreateTier3ProposalArgs {
  communityId: string;
  channelId: string;
  proposalText: string;
  sortitionSize: number;
  deliberationWindowSeconds: number;
  draftingWindowSeconds: number;
  ratificationWindowSeconds: number;
  incentiveMode: 'se' | 'ab' | 'co' | 'dp';
  minPower: number;
  minVouchingDepth?: number;
  retryOf?: string;
}

export interface VotingTier3PollCreatedPayload { /* mirrors Rust */ }
export interface VotingTier3SortitionCompletePayload { /* mirrors Rust */ }
export interface VotingTier3DraftingOpenPayload { /* mirrors Rust */ }
export interface VotingTier3RatificationOpenPayload {
  pollId: string;
  communityId: string;
  candidateOrdering: CandidateRef[];
}
export interface CandidateRef {
  eventHash: string;
  text: string;
  approvalCount: number;
}
export interface VotingTier3FinalizedPayload {
  pollId: string;
  communityId: string;
  winnerEventHash: string;
  winnerText: string;
  runnerUpEventHash?: string;
  scoresSummary: CandidateScore[];
}
export interface CandidateScore {
  eventHash: string;
  totalScore: number;
  runoffVotes: number;
}
```

## Integration tests

### `src-tauri/tests/community_voting_tier3_ipc_integration.rs` (new)

Mirrors `community_voting_tier3_integration.rs` infrastructure (`TwoVotingEngines`, `fixture_identity`) but **drives via Tauri command handlers** rather than `engine.publish_event()` directly.

Test cases:

1. **`tier3_full_lifecycle_via_ipcs`** — proposer calls `voting_create_tier3_proposal`; backup-pool members call `voting_decline_sortition`; remaining members call `voting_propose_draft_candidate` + `voting_approve_draft_candidate`; everyone calls `voting_cast_ratification_ballot`; engine-auto kd=cl + kd=rs fire on materialize after ratification window. Verify both engines converge on identical winner.

2. **`tier3_engine_auto_kd_sf_on_mass_decline`** — proposer creates; all primary+backup members decline; on the proposer's node engine-auto kd=sf fires; second engine sees Stage::Failed.

3. **`tier3_engine_auto_kd_cl_kd_rs_race_tolerant`** — two engines both detect ratification window expired; both publish kd=cl; first-by-HLC wins; resulting kd=rs is bit-identical on both engines.

4. **`tier3_ipc_error_extraction_conforms`** — IPCs return `Result<_, String>` and frontend extracts via `e instanceof Error ? e.message : String(e)`. Smoke test from frontend vitest covers this.

5. **`tier3_retry_of_via_ipc`** — sortition_failed poll triggers a follow-up create via `voting_create_tier3_proposal` with `retry_of: Some(failed_poll_id)`.

### `src-tauri/tests/wire_format_voting_tier3_fixtures.rs` (extend)

Pre-existing fixtures from ZEB-309 (re-verify still match under engine-auto producers — no regen):
- `sortition_failed.cbor`
- `tier3_poll_result.cbor`

Add 1 new fixture for the engine-auto producer not previously pinned:
- `tier3_poll_close.cbor` — NEW (engine-auto kd=cl Tier 3 path).

### Frontend `src/lib/__tests__/voting-adapter.test.ts` (extend or create)

vitest unit tests covering:
- All 6 new IPC wrapper methods invoke with correct camelCase params.
- All 5 new subscribers register + receive payload + unsubscribe correctly.
- Error extraction conforms (test fires both `Error` and string-rejection cases).

## Out of scope

| Item | Where it lives |
|---|---|
| 6 Svelte UI components | [ZEB-311](https://linear.app/zeblith/issue/ZEB-311) |
| Background timer for auto-close in dead silence | Future ticket (file if observed lag) |
| Ballot-secret encryption | [ZEB-295](https://linear.app/zeblith/issue/ZEB-295) (Phase 6) |
| Pol.is clustering | [ZEB-294](https://linear.app/zeblith/issue/ZEB-294) (Phase 5) |
| TRIP receipt-free | [ZEB-296](https://linear.app/zeblith/issue/ZEB-296) (Phase 7) |

**In scope (clarification):** `voting_create_tier3_proposal` reuses the Tier 1 chat-fanout machinery — after successful apply, post a poll-kind chat message into the host channel using `community_channel_log_engine::POLL_BODY_MAGIC` + 64-char ASCII hex `poll_id` (matches `voting_create_tier1_poll` line 20767-20816 in `lib.rs`).

## Acceptance criteria

1. Five CI gates green (`cargo fmt --check`, `cargo clippy -D warnings --all-targets --features test-fixtures`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`).
2. All 6 IPCs callable via `tauri::invoke` with correct snake_case→camelCase boundary.
3. All 5 Tauri events emit at the expected apply / materialize points.
4. Engine-auto kd=sf fires on mass-decline; kd=cl + kd=rs fire on ratification expiry.
5. Race-tolerant: two engines both produce kd=cl → first-by-HLC wins, second rejected by L1.
6. Frontend `voting-adapter.ts` extensions pass vitest coverage (happy-path for each method + subscriber).
7. Error-extraction conforms to `e instanceof Error ? e.message : String(e)` convention.
8. New IPC-driven integration test E2E passes on both engines.
9. 1 new wire-format fixture (kd=cl) pinned via regen-on-first-run pattern; 2 pre-existing fixtures (sf / rs) still bit-identical under engine-auto producers.
10. Per `feedback_metadata_before_irreversible_write`: eligibility verify + validate_poll_config + validate_ballot run BEFORE signing + applying + broadcasting.

## References

- Umbrella spec: [`docs/specs/2026-05-16-zeb-289-voting-polling-design.md`](2026-05-16-zeb-289-voting-polling-design.md) (§6 Tier 3, §6.7 IPC commands, §8 verify rules)
- Backend design: [`docs/specs/2026-05-20-zeb-309-phase4a-main-design.md`](2026-05-20-zeb-309-phase4a-main-design.md)
- Pattern source: [ZEB-305](https://linear.app/zeblith/issue/ZEB-305) PR #143 (D-FROST IPC + Tauri event surface)
- Backend dependency: [ZEB-309](https://linear.app/zeblith/issue/ZEB-309) PR #148 (commit `0902ff2`)
- Downstream consumer: [ZEB-311](https://linear.app/zeblith/issue/ZEB-311)
- [ZEB-310](https://linear.app/zeblith/issue/ZEB-310) — this ticket
