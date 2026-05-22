# ZEB-294 — Tier 3b Pol.is-style Deliberation Design

**Linear:** [ZEB-294](https://linear.app/zeblith/issue/ZEB-294) (parent: [ZEB-289](https://linear.app/zeblith/issue/ZEB-289))
**Phase:** ZEB-289 Phase 5 (Tier 3b)
**Sibling:** [ZEB-293](https://linear.app/zeblith/issue/ZEB-293) (Tier 3a — sortition + STAR, shipped via PRs #148/#149/#150/#151/#152)
**Branched off:** `origin/main` `7cd3765`
**Status:** Design

## Summary

Phase 5 of the [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) voting umbrella ([spec](2026-05-16-zeb-289-voting-polling-design.md)) — adds Pol.is-style deliberation between Tier 3 Sortition (already shipped) and Drafting (already shipped). Mini-public members submit short statements (≤280 chars); all mini-public members vote agree / disagree / pass on every other member's statements; a deterministic Diversity-of-Supporters (DoS) heuristic surfaces "bridging" statements — statements with both wide agreement *and* supporters who otherwise disagree with each other.

PCA-based opinion clustering remains an open research question per [umbrella spec §13](2026-05-16-zeb-289-voting-polling-design.md) and is **explicitly deferred** to a future ticket. The DoS heuristic ships first.

## Context

ZEB-293 (Tier 3a) shipped the full Sortition + Drafting + STAR Ratification stack. The Deliberation stage between Sortition and Drafting is currently a passive waiting window — the engine auto-transitions `'so' → 'de' → 'dr' → 'ra' → 'fi'` (per ZEB-310) but `'de'` does no work. ZEB-294 fills that stage with content: statement composition, voting, and bridging-statement surfacing.

The `kd=ds` (DeliberationStatement) wire kind was scaffolded in Phase 4 ([`community_voting_core.rs:119`](../../src-tauri/src/community_voting_core.rs)). ZEB-294 activates it and adds a new `kd=dv` (DeliberationVote) kind.

Pattern sources verified to exist:
- `src-tauri/src/community_voting_core.rs` — `DeliberationStatementPayload`, `PollEventKindCode::DeliberationStatement` (line 482), `verify_voting_event` (line 949)
- `src-tauri/src/community_voting_log_engine.rs` — `Tier3PollState` projection, `current_stage_at(&now)`, `current_mini_public(&now)` authoritative-set helpers
- `src-tauri/src/community_voting_sortition.rs` — module created by ZEB-309; we add a `bridging` submodule here
- `src/lib/components/Tier3ProposalPanel.svelte:435` — the stage dispatch where DeliberationView mounts
- `src/lib/components/SortitionRevealView.svelte` — peer model for myRole-driven write affordances

## Goals & Non-goals

### Goals

1. Full activation of `kd=ds` + new `kd=dv` wire kinds, with verify rules + multi-engine convergence.
2. Mini-public member writes statements + votes; observers + ratification electorate read everything live.
3. Deterministic bridging-statement detection. Identical event log → identical bridging output across engines (acceptance criterion §3).
4. Mini-public deliberation UI usable for ~50–150 statement deliberations (acceptance criterion §4).
5. Five CI gates green; no regression on existing Tier 1/2/3 tests.

### Non-goals

1. **Full Pol.is PCA + k-means clustering** — explicitly deferred per umbrella spec §13. DoS heuristic ships first.
2. **Statement edit / retract** — statements are immutable per Section 2. Add `kd=dx` tombstone only if real users hit it.
3. **Bridging → drafting coupling** — bridging output is informational only. Drafting candidates remain selected by Tier 1 Approval among mini-public per umbrella spec §6.3 (already shipped via ZEB-309).
4. **Configurable spam cap** — 5 statements/member hardcoded; per-poll override only if needed.
5. **Granular per-event Tauri events for mid-deliberation declines/promotions** — covered by the 5s polling fallback shipped in PR #152 R8; ZEB-319 follow-up still applies but does not block ZEB-294.
6. **Ballot-secret encryption** — umbrella Phase 6.
7. **Receipt-free TRIP credentials** — umbrella Phase 7.

## 1. Architecture overview

```text
┌──────────────────────────────────────────────────────────────────────┐
│  community_voting_core.rs                                            │
│    • DeliberationStatementPayload (EXISTS — Phase 4)                 │
│    • DeliberationVotePayload (NEW)                                   │
│    • PollEventKindCode::DeliberationVote / "dv" (NEW)                │
│    • verify_voting_event (UNCHANGED — first-pass check)              │
└──────────────────────────────────────────────────────────────────────┘
                                  │
┌──────────────────────────────────────────────────────────────────────┐
│  community_voting_log_engine.rs                                      │
│    • Tier3PollState::deliberation: DeliberationState (NEW field)     │
│    • apply_deliberation_statement (NEW — second-pass apply rules)    │
│    • apply_deliberation_vote (NEW — LWW per (voter, statement))      │
│    • Emits Tauri events: tier3-deliberation-statement-created,       │
│                          tier3-deliberation-vote-cast                │
└──────────────────────────────────────────────────────────────────────┘
                                  │
┌──────────────────────────────────────────────────────────────────────┐
│  community_voting_sortition.rs                                       │
│    • bridging submodule (NEW)                                        │
│      - compute_bridging_scores(state: &DeliberationState,            │
│                                mini_public: &HashSet<OwnerAddr>)     │
│        -> Vec<BridgingScore>                                         │
│      - Pure integer arithmetic (Q32 fixed-point); IEEE-754-free      │
└──────────────────────────────────────────────────────────────────────┘
                                  │
┌──────────────────────────────────────────────────────────────────────┐
│  lib.rs (Tauri commands)                                             │
│    • voting_submit_deliberation_statement (NEW)                      │
│    • voting_cast_deliberation_vote (NEW)                             │
│    • voting_list_bridging_statements (NEW)                           │
└──────────────────────────────────────────────────────────────────────┘
                                  │
┌──────────────────────────────────────────────────────────────────────┐
│  src/lib (frontend)                                                  │
│    • types/voting.ts — DeliberationStatementExport, BridgingScore,   │
│                         Tier3PollExport.deliberationStatements,      │
│                         Tier3PollExport.myDeliberationStatementCount,│
│                         Tier3PollExport.myDeliberationVotes          │
│    • voting-adapter.ts — 3 IPC bindings + 2 event subscribers        │
│    • components/DeliberationView.svelte (NEW, two-column)            │
│      ├── StatementComposer.svelte (NEW, mini-public only)            │
│      ├── StatementVoteList.svelte (NEW, all viewers)                 │
│      └── BridgingPanel.svelte (NEW, all viewers)                     │
└──────────────────────────────────────────────────────────────────────┘
```

**Lifecycle.** Engine-auto orchestration (ZEB-310) already drives Sortition → Deliberation → Drafting → Ratification at window boundaries. No new transition logic. The three deliberation IPCs use `current_stage_at(&now)` for stage gating, mirroring PR #152 R10.

**Scope.** ZEB-294 touches the Deliberation stage only. Sortition, Drafting, and Ratification stages are untouched. `Tier3ProposalPanel.svelte` change is a small conditional addition mounting `DeliberationView` when `stage === 'de'`.

## 2. Wire format

### 2.1 DeliberationStatement (`kd=ds`) — EXISTS

Scaffolded in Phase 4. No changes:

```rust
pub struct DeliberationStatementPayload {
    #[serde(rename = "pi")] pub poll_id: PollId,
    #[serde(rename = "tx")] pub text: String,
}
```

### 2.2 DeliberationVote (`kd=dv`) — NEW

```rust
pub struct DeliberationVotePayload {
    #[serde(rename = "pi")] pub poll_id: PollId,
    #[serde(rename = "sh",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr")]
    pub statement_event_hash: StatementEventHash,  // 32-byte signing-bytes hash of the kd=ds event
    #[serde(rename = "vt")] pub vote: u8,           // 0=agree, 1=disagree, 2=pass
}
```

All field names are 2 chars per the umbrella spec §3 same-length-keys invariant.

Adds one variant to `PollEventKindCode::DeliberationVote` with wire code `"dv"`. Mirrors `DraftApproval`'s event-hash reference pattern (`ch` field with `serialize_bytes_as_bstr`). A new `BridgingVoteCode` enum (`Agree`, `Disagree`, `Pass`) wraps the u8 for type safety inside the engine.

### 2.3 Verify rules — two-pass model

**First-pass — `verify_voting_event`:** unchanged. Validates signature + that actor was in the eligibility snapshot at PollCreate. Applies to both `ds` and `dv`.

**Second-pass — apply-time rules** (in `Tier3PollState::apply_deliberation_*`):

| Rule | `ds` | `dv` |
|---|---|---|
| Actor in `current_mini_public(ev_hlc)` (authoritative-set check, handles declines + backup promotions) | ✓ | ✓ |
| Actor has NOT submitted a MiniPublicDecline with earlier HLC | ✓ | ✓ |
| `current_stage_at(ev_hlc) == 'de'` (within deliberation window) | ✓ | ✓ |
| Text length 1..=280 chars; reject if `text.trim().is_empty()` (stored as-is, including original whitespace) | ✓ | — |
| Actor's prior-statement count in this poll < 5 (spam cap) | ✓ | — |
| `statement_event_hash` references an existing `ds` event in this poll's state | — | ✓ |
| `vote` ∈ {0, 1, 2} | — | ✓ |

**Revote semantics.** A later `dv` from the same `(voter, statement_event_hash)` supersedes the earlier — last-write-wins by `(ev_hlc, ev_signing_bytes_hash)` tuple compared lexicographically. `pass` is a legitimate revote target.

## 3. Projection state

### 3.1 `DeliberationState`

```rust
pub struct Statement {
    pub event_hash: StatementEventHash,
    pub author: OwnerAddr,
    pub text: String,
    pub created_at_hlc: HlcStamp,
}

pub struct VoteEntry {
    pub voter: OwnerAddr,
    pub statement_event_hash: StatementEventHash,
    pub vote: BridgingVoteCode,
    pub last_update_hlc: HlcStamp,
    pub last_update_event_hash: EventHash,  // HLC-tie breaker
}

pub struct DeliberationState {
    pub statements: BTreeMap<StatementEventHash, Statement>,
    pub votes: BTreeMap<(OwnerAddr, StatementEventHash), VoteEntry>,
    pub statements_per_author: BTreeMap<OwnerAddr, u8>,   // O(1) spam-cap check
}
```

Lives at `Tier3PollState::deliberation`. `BTreeMap` (not `HashMap`) for deterministic iteration, mirroring the rest of `Tier3PollState`.

### 3.2 Materialize handlers

`apply_deliberation_statement`:
1. Run all `ds` apply rules from §2.3. On failure: silently drop, debug-log (matches `apply_ratification_ballot` precedent).
2. Insert into `statements`; increment `statements_per_author[author]`.
3. Emit Tauri event `tier3-deliberation-statement-created`.

`apply_deliberation_vote`:
1. Run all `dv` apply rules.
2. Check existing entry in `votes[(voter, statement_hash)]`. LWW-compare `(ev_hlc, event_hash)` lexicographically; insert if newer, otherwise drop.
3. Emit Tauri event `tier3-deliberation-vote-cast` only on actual state change (not on LWW-loss).

## 4. Bridging algorithm — Diversity-of-Supporters (DoS)

**Goal.** Surface statements with both wide agreement *and* supporters who otherwise disagree with each other. Captures Pol.is's cross-cluster-consensus semantic without requiring PCA.

### 4.1 Inputs

- `statements: &BTreeMap<StatementEventHash, Statement>` — current poll's statements
- `votes: &BTreeMap<(OwnerAddr, StatementEventHash), VoteEntry>` — current poll's votes
- `mini_public: &BTreeSet<OwnerAddr>` — authoritative `current_mini_public(eval_hlc)` set, where `eval_hlc` is the HLC at the bridging IPC call. Computed once per IPC call by the engine — fixes the membership snapshot the algorithm sees so the same call from the same engine state returns identical output (determinism property 5, §4.7).

### 4.2 Per-member vote vector

For each `m ∈ mini_public`, build `V_m: BTreeMap<StatementEventHash, i8>`:
- `+1` if `votes[(m, s)].vote == Agree`
- `-1` if `votes[(m, s)].vote == Disagree`
- `0` if `votes[(m, s)].vote == Pass` OR no vote entry

### 4.3 Pairwise dissimilarity (integer Hamming-flavor)

For each pair `(m1, m2)` with `m1 < m2` lex order:

```text
joint_support = count of statements s where V_m1[s] ≠ 0 AND V_m2[s] ≠ 0
disagree_count = count where V_m1[s] ≠ V_m2[s] AND both ≠ 0
d_q32(m1, m2) = ((disagree_count as u64) << 32) / max(1u64, joint_support as u64)
```

Result is a Q32 fixed-point fraction in `[0, 2^32]`. Pure integer arithmetic — no `f64`, no `sqrt`, IEEE-754-divergence-free across platforms.

### 4.4 Per-statement bridging score

```text
For each statement s in statements (iter in BTreeMap order):
    supporters(s) = { m ∈ mini_public : V_m[s] == +1 }
    if |supporters(s)| < 2:
        diversity_q32(s) = 0
    else:
        sum_d = 0u64
        pair_count = 0u64
        for each (m_i, m_j) in supporters(s) with m_i < m_j:
            sum_d += d_q32(m_i, m_j)
            pair_count += 1
        diversity_q32(s) = sum_d / pair_count
    bridging_score_q64(s) = (supporters(s) count as u64) * diversity_q32(s)
```

### 4.5 Output + sort

```rust
pub struct BridgingScore {
    pub statement_event_hash: StatementEventHash,
    pub statement_text: String,
    pub author: OwnerAddr,
    pub agree_count: u16,
    pub disagree_count: u16,
    pub pass_count: u16,
    pub diversity_q32: u64,       // 0..2^32
    pub bridging_score_q64: u64,  // agree_count × diversity_q32
}
```

Sort statements by `(bridging_score_q64 DESC, statement_event_hash ASC)`. Tie-break by event hash gives deterministic ordering across engines.

### 4.6 Complexity

- Pairwise matrix: O(M² × S) where M = mini-public size, S = statement count
- Bridging pass: O(S × Avg|Supporters|²) — worst case O(S × M²)

For typical M=100, S=150: ~1.5M ops pairwise + ~1.5M ops bridging = ~3M ops total. Negligible. Recompute on each IPC call; no caching needed.

### 4.7 Determinism guarantees

1. Pure integer arithmetic (Q32/Q64 fixed-point). **No `f64` in the sort path**; the only `f64` use is `BridgingPanel.svelte` rendering a per-viewer heat bar from `bridgingScoreQ64 / 2^32 / max_observed`. That f64 is purely visual + viewer-local; it never feeds back into the sort or any consensus path.
2. BTreeMap iteration order is deterministic.
3. Tie-break by statement event hash → fully deterministic sort.
4. `current_mini_public(eval_hlc)` is the authoritative voter set; fixed at IPC entry to avoid drift mid-computation.
5. Acceptance criterion §3 verified via multi-engine integration test (§9).

## 5. IPCs + Tauri events

### 5.1 IPCs

```rust
#[tauri::command(rename_all = "snake_case")]
async fn voting_submit_deliberation_statement(
    state: State<'_, AppState>,
    community_id: String,         // hex SpaceId
    poll_id: String,              // hex PollId
    text: String,
) -> Result<String, String>       // returns statement_event_hash hex

#[tauri::command(rename_all = "snake_case")]
async fn voting_cast_deliberation_vote(
    state: State<'_, AppState>,
    community_id: String,
    poll_id: String,
    statement_event_hash: String, // hex
    vote: String,                 // "agree" | "disagree" | "pass"
) -> Result<(), String>

#[tauri::command(rename_all = "snake_case")]
async fn voting_list_bridging_statements(
    state: State<'_, AppState>,
    community_id: String,
    poll_id: String,
    top_n: u16,                   // adapter default 10
) -> Result<Vec<BridgingScoreExport>, String>
```

**Stage gating:**
- `submit_deliberation_statement` and `cast_deliberation_vote` reject when `current_stage_at(now) != 'de'`.
- `list_bridging_statements` accepts stages `'de' | 'dr' | 'ra' | 'fi'` (read-only access for the bridging surface remains available through ratification + finalization as historical reference; rejects `'so'` and `'fa'`).

**Wire vs IPC boundary representation.** Internal `kd=dv` payload (§2.2) stores `vote: u8` (0/1/2) for compact wire format. The IPC layer takes/returns `vote: String` ("agree"/"disagree"/"pass") for readability. The mapping is centralized in `BridgingVoteCode::{from_u8, to_str, from_str}` helpers in `community_voting_core.rs`. Invalid string → IPC rejects with `MalformedRequest`.

`BridgingScoreExport` is the camelCase wire DTO. Hashes/addresses serialized as hex strings; `diversity_q32` and `bridging_score_q64` serialized as decimal strings (JSON's number is `f64`; u64 values past 2^53 lose precision).

### 5.2 Tauri events

| Event name | Emitted by | Payload (camelCase) |
|---|---|---|
| `tier3-deliberation-statement-created` | `apply_deliberation_statement` post-apply | `{ pollId, statementEventHash, author, text, createdAtHlcMs }` |
| `tier3-deliberation-vote-cast` | `apply_deliberation_vote` post-apply (state change only) | `{ pollId, statementEventHash, voter, vote }` |

Frontend subscribers: `subscribeTier3DeliberationStatementCreated`, `subscribeTier3DeliberationVoteCast`. UI refreshes detail + bridging list when either fires. No "bridging updated" event — bridging is derived state recomputed by frontend via the read IPC.

### 5.3 Confirmation severity tiers

Per `feedback_severe_action_confirmation`:

| Action | Tier | Why |
|---|---|---|
| Submit statement | Click-confirm | Immutable + publicly visible. Forces deliberate composition. |
| Cast / change vote | No confirm | Low-risk, revote allowed; would train users to dismiss. |

### 5.4 In-flight response race protection

Adapter calls in `DeliberationView.svelte` use the seq + key snapshot pattern (per PR #152 R9): stale responses from a previous `pollId` or `communityId` are dropped on resolution.

## 6. UI components

### 6.1 Component tree

```text
src/lib/components/
  DeliberationView.svelte            (NEW — two-column container)
    ├── StatementComposer.svelte     (NEW — left-top, mini-public only)
    ├── StatementVoteList.svelte     (NEW — left-bottom, all viewers)
    └── BridgingPanel.svelte         (NEW — right column, all viewers)
```

### 6.2 Mounting

`Tier3ProposalPanel.svelte` near line 435, add to the existing stage dispatch:

```svelte
{#if selectedDetail.stage === 'de'}
  <DeliberationView detail={selectedDetail} {adapter} {myAddr} onChange={refetchSelected} />
{/if}
```

Sits between `SortitionRevealView` (always rendered for `'de'|'dr'|'ra'|'fi'`) and `MiniPublicParticipationToggle` (existing decline UI).

### 6.3 `DeliberationView.svelte`

- Props: `{ detail: Tier3PollExport, adapter: VotingAdapter, myAddr: OwnerAddr, onChange: () => void }`
- State: `bridgingScores = $state<BridgingScore[]>([])`, refreshed on mount + on every `tier3-deliberation-statement-created` / `tier3-deliberation-vote-cast` event
- Layout: CSS grid `grid-template-columns: 1.4fr 1fr`; single-column collapse at narrow widths
- Uses seq + key in-flight guard pattern

### 6.4 `StatementComposer.svelte`

- Renders only when `detail.myRole === 'mini_public'` AND `detail.myDeliberationStatementCount < 5`
- Textarea (280-char hard cap with live char counter)
- Paired number input is not needed (text input, not slider) — `feedback_slider_pair_with_number_input` does not apply
- Submit triggers click-confirm modal per §5.3 → IPC → success toast → reset
- Disabled when stage transitions out of `'de'`

### 6.5 `StatementVoteList.svelte`

- Renders `detail.deliberationStatements` chronologically ASC (oldest first)
- "Unvoted by me" toggle defaults ON for mini-public, hidden for observers
- Per row: text + author short-id + tri-button (agree/disagree/pass) for mini-public OR read-only count chips for observers + after stage `'de'`
- Vote click fires IPC with no confirmation
- Active vote shown by colored border on chosen button
- Selected statement highlights in `BridgingPanel` via shared selection state

### 6.6 `BridgingPanel.svelte`

- Renders `bridgingScores` top-10 by `bridgingScoreQ64` DESC
- Each card: heat-rendered score (`(bridgingScoreQ64 / 2^32 / max_observed) * 100%`), statement text, agree-count badge, diversity-percentage chip
- Live re-sorts on incoming events with brief `transition:slide` animation
- Empty state copy: "Bridging scores will appear once members vote on statements" — concrete real-state, not designed-for-empty per `feedback_design_for_eventual_state`

### 6.7 `Tier3PollExport` extension

```typescript
interface Tier3PollExport {
  // ... existing fields from ZEB-309/310/311 ...
  deliberationStatements: DeliberationStatementExport[];
  myDeliberationStatementCount: number;
  myDeliberationVotes: Array<{
    statementEventHash: string;
    vote: 'agree' | 'disagree' | 'pass';
  }>;
}

interface DeliberationStatementExport {
  statementEventHash: string;
  author: string;        // hex OwnerAddr
  text: string;
  createdAtHlcMs: number;
  agreeCount: number;
  disagreeCount: number;
  passCount: number;
}
```

Vote aggregate counts (`agreeCount`, `disagreeCount`, `passCount`) ride on each statement export rather than shipping the full vote-event log — collapses ~1500 vote events into ~50 statements at ~28 bytes/aggregate. Bridging IPC ships the per-pair diversity computation result separately.

## 7. Eligibility

- **Submit statement / cast vote:** mini-public only (write affordance gated by `current_mini_public(ev_hlc)`).
- **Read statements / votes / bridging surface:** entire community (CRDT events are public; UI surfaces them to all viewers including the full ratification electorate, observers, and non-mini-public community members). Aligns with Pol.is transparency model and Harmony's polycentric / no-algorithmic-gating norms.

Bridging detection runs client-side from the IPC result for any viewer; the algorithm itself is server-side in Rust but the IPC is open to all community members.

## 8. Visibility timeline

| Stage | Mini-public sees | Observers see |
|---|---|---|
| `so` (Sortition) | n/a | n/a |
| `de` (Deliberation) | Composer + vote list (writable) + live bridging panel | Statements + votes (read-only) + live bridging panel |
| `dr` (Drafting) | Read-only statements + votes + bridging panel | Same |
| `ra` (Ratification) | Same as `dr` | Same |
| `fi` (Finalized) | Same as `dr` | Same |

Deliberation events accumulate; nothing is hidden post-stage. Bridging panel remains accessible during drafting and ratification as historical reference.

## 9. Tests

| Layer | File | Coverage |
|---|---|---|
| Unit — core | `community_voting_core.rs` (in-file `#[cfg(test)] mod tests`) | DeliberationVotePayload CBOR round-trip; reject malformed; reject 281-char text; same-length-keys invariant pinning |
| Unit — bridging math | `community_voting_sortition.rs` (in-file `#[cfg(test)] mod tests::bridging`) | 0/1/2 statements edge cases; 1 unanimous statement outranks 1 polarizing; diversity correctness (5 in-lockstep supporters score < 5 cross-cluster); determinism (shuffle event log → same output); empty mini-public; promoted backup contributes |
| Unit — engine apply | `community_voting_log_engine.rs` (in-file `#[cfg(test)] mod tests::deliberation`) | `apply_deliberation_statement` enforces mini-public + length + spam-cap + stage-window; `apply_deliberation_vote` LWW by `(hlc, event_hash)`; rejects non-existent statement reference; rejects declined-member events |
| Wire-format pinning | `tests/wire_format_voting_fixtures.rs` (extend) | Pin canonical CBOR bytes for DeliberationVote — prevents future serde-drift |
| IPC integration | `tests/community_voting_tier3_deliberation_ipc_integration.rs` (NEW) | All 3 IPCs happy path; observer rejected; 6th statement rejected; revote LWW; stage-out-of-window rejected; bridging list sorted-DESC |
| **Multi-engine integration** ⭐ | `tests/community_voting_tier3_deliberation_multi_engine_integration.rs` (NEW) | Two engines on real Zenoh: A submits statement → B applies → B votes → A applies → **both engines compute bitwise-identical bridging output**. 3 members × 5 statements × 12 votes scenario. Satisfies acceptance criteria §3 + §5 combined. |
| Frontend | `__tests__/DeliberationView.test.ts` + 3 sub-component test files (NEW) | Mounts only when `stage==='de'`; refreshes on Tauri event; composer mini-public-gated + 5-cap-gated + click-confirm; vote tri-button no-confirm + revote UI sync; bridging panel top-10 sort + empty-state copy |

The bridging-determinism multi-engine test is the load-bearing one — if integer-DoS math diverges across engines, this catches it before merge.

## 10. PR shape + acceptance criteria

Single PR, estimated ~3500–4500 LOC across:

| Area | Approx LOC |
|---|---|
| Core (payload + verify rules + helpers) | ~600 |
| Bridging math (with tests) | ~400 |
| Engine projection + materialize handlers | ~800 |
| IPCs + adapter | ~400 |
| Frontend components (4 files) | ~500 |
| Tests (unit + integration + multi-engine + frontend) | ~800 |

PR body uses markdown-linked refs `[ZEB-294](url)` (per `feedback_linear_pr_auto_close`). PR body includes `Closes ZEB-294` since this PR fully completes the ticket; parent ZEB-289 stays Backlog (cascade-resistance verified by PR #152 pattern).

**Acceptance criteria** (matching Linear ticket):
1. Five CI gates green (cargo fmt + cargo clippy + cargo nextest + npx tsc + npx vitest).
2. Deliberation statements + votes accepted from mini-public only (verify_event second-pass enforces).
3. Bridging-statement detection deterministic: identical event log → identical bridging output. Verified by multi-engine integration test.
4. UI usable for ~50–150 statement deliberations (typical mini-public size).
5. Multi-engine integration test: deliberation events converge + bridging detection converges across engines.
6. No regression on existing Tier 1/2/3 voting tests.

## 11. Open questions resolved during brainstorm

1. **PR scope** — Full Tier 3b in one PR.
2. **Algorithm** — Diversity-of-Supporters heuristic. PCA deferred.
3. **Vote model** — 3-way (agree/disagree/pass); revote allowed via LWW.
4. **Anti-spam** — Hard cap 5 statements/member, enforced in second-pass apply.
5. **Visibility** — Full transparency (public read, mini-public-only write).
6. **Lifecycle** — Statements immutable.
7. **Layout** — Two-column (compose + vote list left, live bridging right).
8. **Math placement** — Rust-side (mirrors ZEB-309's pattern; single canonical algorithm).
9. **Determinism** — Pure integer arithmetic (Q32/Q64 fixed-point + lexicographic tie-break).

## 12. References

- [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) — voting/polling umbrella epic
- [ZEB-294](https://linear.app/zeblith/issue/ZEB-294) — this ticket
- [Umbrella spec](2026-05-16-zeb-289-voting-polling-design.md) §6.2 (deliberation), §6.6 (eligibility), §6.7 (IPCs), §13 (open research)
- [ZEB-293 design](2026-05-16-zeb-289-voting-polling-design.md) (Tier 3a sibling — shipped)
- [ZEB-298+ZEB-312 design](2026-05-20-zeb-298-zeb-312-engine-production-wiring-design.md) (production-wiring foundation that this builds on)
