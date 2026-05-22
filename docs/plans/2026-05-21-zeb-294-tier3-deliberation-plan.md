# ZEB-294 Tier 3b Deliberation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship Pol.is-style deliberation between Sortition and Drafting in Tier 3 polls — `kd=dv` wire kind, materialized statement/vote projection, Diversity-of-Supporters bridging heuristic, three IPCs, two Tauri events, and a two-column UI.

**Architecture:** Per [spec](../specs/2026-05-21-zeb-294-tier3-deliberation-design.md) commit `a66d766`. Adds `DeliberationVotePayload` + `BridgingVoteCode` to `community_voting_core.rs`; extends `Tier3PollState` with `DeliberationState { statements, votes, statements_per_author }`; adds `bridging` submodule to `community_voting_sortition.rs` (Q32/Q64 integer math); 3 IPCs (`voting_submit_deliberation_statement`, `voting_cast_deliberation_vote`, `voting_list_bridging_statements`); 2 Tauri events (`voting-tier3-deliberation-statement-created`, `voting-tier3-deliberation-vote-cast`); 4 new Svelte components mounted from `Tier3ProposalPanel.svelte:435`.

**Tech Stack:** Rust (tokio + serde + ciborium + thiserror), Tauri 2, Svelte 5 (runes), TypeScript, vitest, cargo-nextest, real Zenoh for multi-engine convergence test.

**Branch:** `zeb-294-tier3-deliberation` off `origin/main` `7cd3765`. Spec commit `a66d766` is the first commit on this branch.

**HARD RULES** (from user memory — every task must honor these):
- 5 backend CI gates from `src-tauri/`: `cargo fmt --all -- --check` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- 2 frontend gates from repo root: `npx tsc --noEmit` + `npx vitest run`
- Pipe exit codes: use `set -o pipefail` or `${PIPESTATUS[0]}` when piping cargo output
- Tauri IPC: `#[tauri::command(rename_all = "snake_case")]`; snake_case Rust ↔ camelCase JS at boundary
- Tauri error extraction (frontend): `e instanceof Error ? e.message : String(e)`
- No worktrees — main repo only
- Per `feedback_implementer_gate_time_budget`: end every task with a commit BEFORE the gate sweep; 10-min wall-clock kill switch on any cargo command; if gates blow budget, return DONE_WITH_CONCERNS rather than hang
- Pre-existing orphan failures (~28) expected in nextest baseline: `folder_ingest::tests` (3), `mint::tests` (2), `mint_sync::tests` (2), `folder_ingest_walker_integration` (9), `rename_content_integration` (12). Task 0 captures the exact baseline; subsequent tasks must not introduce NEW failures beyond this list.

**Event-name convention note:** the spec drafted event names as `tier3-deliberation-*`. Implementation uses `voting-tier3-deliberation-*` to match the existing voting-event prefix (`voting-tier3-sortition-complete`, `voting-tier3-drafting-open`, etc.). Spec event names are a documentation detail; wire convention wins.

---

## File Structure

**New files:**

| Path | Responsibility |
|---|---|
| `src-tauri/src/community_voting_sortition.rs` (existing — add `bridging` submodule) | Pure DoS math: Q32 pairwise dissimilarity matrix + Q64 per-statement bridging score + deterministic sort |
| `src-tauri/tests/community_voting_tier3_deliberation_ipc_integration.rs` (NEW) | 3 IPCs end-to-end via test runtime; stage gating; spam-cap rejection; revote LWW; bridging sort |
| `src-tauri/tests/community_voting_tier3_deliberation_multi_engine_integration.rs` (NEW) | Two engines on real Zenoh; statement + vote convergence; **bitwise-identical bridging output across engines** |
| `src/lib/components/DeliberationView.svelte` (NEW) | Two-column container, mounts inside Tier3ProposalPanel on stage==='de' |
| `src/lib/components/StatementComposer.svelte` (NEW) | Left-top, mini-public only, 280-char textarea + click-confirm modal |
| `src/lib/components/StatementVoteList.svelte` (NEW) | Left-bottom, all viewers; chronological list + tri-button (agree/disagree/pass) + "Unvoted by me" filter |
| `src/lib/components/BridgingPanel.svelte` (NEW) | Right column, all viewers; top-10 bridging list with heat-bar visualization |
| `src/lib/components/__tests__/DeliberationView.test.ts` (NEW) | Container mounts + event-driven refresh |
| `src/lib/components/__tests__/StatementComposer.test.ts` (NEW) | mini-public gating + 5-cap gating + click-confirm |
| `src/lib/components/__tests__/StatementVoteList.test.ts` (NEW) | filter toggle + revote sync + observer read-only |
| `src/lib/components/__tests__/BridgingPanel.test.ts` (NEW) | top-10 sort + empty-state copy |

**Modified files:**

| Path | What changes |
|---|---|
| `src-tauri/src/community_voting_core.rs` | Add `DeliberationVotePayload`, `BridgingVoteCode` enum, `PollEventKindCode::DeliberationVote`, wire code `"dv"`, CBOR round-trip + same-length-keys pinning |
| `src-tauri/src/community_voting_tier3.rs` | Add `DeliberationState` struct on `Tier3PollState`; flesh out `apply_event` arms for `DeliberationStatement` + `DeliberationVote`; helper `Tier3PollState::bridging_inputs(now)` returning `(statements, votes, mini_public)` for the bridging submodule |
| `src-tauri/src/community_voting_sortition.rs` | Add `bridging` submodule with `compute_bridging_scores(...) -> Vec<BridgingScore>` + `BridgingScore` struct |
| `src-tauri/src/community_voting_log_engine.rs` | Emit `voting-tier3-deliberation-statement-created` + `voting-tier3-deliberation-vote-cast` post-apply hooks in `process_inbound_dispatch` |
| `src-tauri/src/lib.rs` | 3 new IPC commands + handler registration; extend `Tier3PollExport` with `deliberation_statements`, `my_deliberation_statement_count`, `my_deliberation_votes` fields; extend `build_tier3_export` |
| `src-tauri/tests/wire_format_voting_fixtures.rs` | Pin canonical CBOR bytes for `DeliberationVotePayload` |
| `src/lib/types/voting.ts` | Add `DeliberationStatementExport`, `BridgingScoreExport`, `DeliberationVoteCode`; extend `Tier3PollExport` with deliberation fields |
| `src/lib/voting-adapter.ts` | Add 3 IPC adapter methods + 2 event subscriber methods |
| `src/lib/components/Tier3ProposalPanel.svelte:435` | Mount `DeliberationView` when `selectedDetail.stage === 'de'` |

---

## Task 0: Pre-flight green-baseline confirmation (no commit)

**Goal:** Capture the exact orphan-failure baseline on `zeb-294-tier3-deliberation` HEAD `a66d766` so subsequent tasks know what's "expected red" vs "actually red."

- [ ] **Step 0.1:** Verify branch state.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status --short
git branch --show-current
git log --oneline -3
```

Expected output:
- Clean working tree
- Branch: `zeb-294-tier3-deliberation`
- HEAD: `a66d766 docs(zeb-294): Tier 3b Pol.is-style deliberation design spec`

- [ ] **Step 0.2:** Run backend gates and capture baseline failures.

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
set -o pipefail
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tee /tmp/zeb-294-task-0-baseline.log
```

Expected: fmt + clippy pass cleanly; nextest reports ~28 pre-existing failures in `folder_ingest::tests`, `mint::tests`, `mint_sync::tests`, `folder_ingest_walker_integration`, `rename_content_integration`. Test count of `community_voting_*` should be all green.

If nextest reports failures OUTSIDE the orphan-failure list above, STOP and surface — those are not orphans, and the green-baseline is not actually green.

- [ ] **Step 0.3:** Run frontend gates.

```bash
cd ..
npx tsc --noEmit
npx vitest run
```

Expected: both pass cleanly. If anything is red, STOP and surface.

- [ ] **Step 0.4:** Record the captured baseline failure count for downstream tasks.

```bash
grep -cE "^\s*FAIL" /tmp/zeb-294-task-0-baseline.log || true
```

Record the exact count (around 28). Subsequent tasks must show the SAME count — any delta is a regression we caused.

**No commit for Task 0.** This is a baseline snapshot only.

---

## Task 1: DeliberationVote wire kind + CBOR pinning

**Goal:** Add `DeliberationVotePayload`, `BridgingVoteCode`, and `PollEventKindCode::DeliberationVote` with wire code `"dv"`. CBOR round-trip + same-length-keys invariant pinning. No engine-level behavior yet.

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs:115-180` (payload area) + `:459-495` (PollEventKindCode enum) + `:660-708` (wire-code registration tests) + `:1670` (build_signed_* helper tests)
- Modify: `src-tauri/src/community_voting_core.rs` (test module) — add CBOR round-trip + reject-malformed tests

- [ ] **Step 1.1: Add `BridgingVoteCode` enum** after `DeliberationStatementPayload` (insert at line ~125).

```rust
/// Vote type for `kd=dv` DeliberationVote events. Wire encoding is a single
/// u8 (0=agree, 1=disagree, 2=pass) inside the payload; this enum is the
/// type-safe Rust representation used throughout the engine + IPC layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum BridgingVoteCode {
    Agree = 0,
    Disagree = 1,
    Pass = 2,
}

impl BridgingVoteCode {
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Agree),
            1 => Some(Self::Disagree),
            2 => Some(Self::Pass),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Agree => "agree",
            Self::Disagree => "disagree",
            Self::Pass => "pass",
        }
    }

    pub fn from_wire_str(s: &str) -> Option<Self> {
        match s {
            "agree" => Some(Self::Agree),
            "disagree" => Some(Self::Disagree),
            "pass" => Some(Self::Pass),
            _ => None,
        }
    }
}
```

- [ ] **Step 1.2: Add `DeliberationVotePayload`** immediately after `BridgingVoteCode`.

```rust
/// Payload for `kd=dv` DeliberationVote: a mini-public member's vote
/// (agree/disagree/pass) on another member's DeliberationStatement.
/// `statement_event_hash` is the SHA-256 of the signing bytes of the
/// referenced `kd=ds` event (32 bytes). `vote` is `BridgingVoteCode::as_u8`.
///
/// All field keys are 2 chars per spec §3 same-length-keys invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliberationVotePayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(
        rename = "sh",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub statement_event_hash: [u8; 32],
    #[serde(rename = "vt")]
    pub vote: u8,
}
```

- [ ] **Step 1.3: Add `PollEventKindCode::DeliberationVote` variant** in the enum at line ~459. Insert after `DeliberationStatement,` (line 482):

```rust
    DeliberationStatement,
    /// kd=dv DeliberationVote — mini-public member agrees/disagrees/passes
    /// on another member's DeliberationStatement.
    DeliberationVote,
    MiniPublicDecline,
```

- [ ] **Step 1.4: Register wire code `"dv"` in the bidirectional kind-code table.** Find the function/table that maps `PollEventKindCode` ↔ 2-char wire string (around line 681). Add `(PollEventKindCode::DeliberationVote, "dv")` to the tuple list. Also ensure it's covered in the enumeration test list around line 660.

```rust
// In the assert-all-kinds-have-wire-codes test (~line 660), add:
            PollEventKindCode::DeliberationVote,

// In the wire_code_kinds_match_internal_kinds test (~line 681), add:
            (PollEventKindCode::DeliberationVote, "dv"),
```

- [ ] **Step 1.5: Write the CBOR round-trip + reject-malformed tests.** Add to the test module (near line 240 where `DeliberationStatementPayload` round-trip lives):

```rust
    #[test]
    fn deliberation_vote_payload_round_trip() {
        let payload = DeliberationVotePayload {
            poll_id: PollId([0xAB; 32]),
            statement_event_hash: [0xCD; 32],
            vote: BridgingVoteCode::Agree.as_u8(),
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&payload, &mut buf).expect("encode");
        let decoded: DeliberationVotePayload =
            ciborium::de::from_reader(&buf[..]).expect("decode");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn deliberation_vote_payload_all_three_vote_codes_round_trip() {
        for code in [BridgingVoteCode::Agree, BridgingVoteCode::Disagree, BridgingVoteCode::Pass] {
            let payload = DeliberationVotePayload {
                poll_id: PollId([1; 32]),
                statement_event_hash: [2; 32],
                vote: code.as_u8(),
            };
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&payload, &mut buf).expect("encode");
            let decoded: DeliberationVotePayload =
                ciborium::de::from_reader(&buf[..]).expect("decode");
            assert_eq!(decoded.vote, code.as_u8());
            assert_eq!(BridgingVoteCode::from_u8(decoded.vote), Some(code));
        }
    }

    #[test]
    fn bridging_vote_code_from_u8_rejects_out_of_range() {
        assert_eq!(BridgingVoteCode::from_u8(3), None);
        assert_eq!(BridgingVoteCode::from_u8(255), None);
    }

    #[test]
    fn bridging_vote_code_wire_str_round_trip() {
        for code in [BridgingVoteCode::Agree, BridgingVoteCode::Disagree, BridgingVoteCode::Pass] {
            assert_eq!(BridgingVoteCode::from_wire_str(code.as_wire_str()), Some(code));
        }
        assert_eq!(BridgingVoteCode::from_wire_str("foo"), None);
    }
```

- [ ] **Step 1.6: Add `build_signed_deliberation_vote` helper** alongside the other `build_signed_*` helpers (around line 1310 next to `build_signed_mini_public_decline`). This is used by tests + by future IPC code.

```rust
/// Build a fully-signed `kd=dv` DeliberationVote event.
pub fn build_signed_deliberation_vote(
    signing_key: &SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    statement_event_hash: [u8; 32],
    vote: BridgingVoteCode,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    let payload_struct = DeliberationVotePayload {
        poll_id,
        statement_event_hash,
        vote: vote.as_u8(),
    };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|e| BuildError::EncodePayload(e.to_string()))?;
    let unsigned = UnsignedVotingEvent {
        tier: Tier::Sortition,
        kind: PollEventKindCode::DeliberationVote,
        community_id: SpaceId([0; 16]), // caller fills via from_unsigned if needed
        actor,
        hlc,
        payload,
    };
    SignedVotingEvent::from_unsigned(signing_key, unsigned)
}
```

(Note: the actual signature pattern — community_id setting + signing helper invocation — must match `build_signed_mini_public_decline` exactly. Read lines 1310-1330 of community_voting_core.rs to confirm; if the signature differs, mirror the real pattern verbatim instead of the template above.)

- [ ] **Step 1.7: Add signed round-trip test** alongside `signed_mini_public_decline_round_trip` (around line 1676):

```rust
    #[test]
    fn signed_deliberation_vote_round_trip() {
        let keypair = test_signing_key();
        let actor = OwnerAddr([0xAA; 16]);
        let pid = PollId([0xBB; 32]);
        let stmt_hash = [0xCC; 32];
        let hlc = Hlc::new(1_700_000_000_000, 0, "dev1".to_string());
        let ev = build_signed_deliberation_vote(
            &keypair,
            actor,
            pid,
            stmt_hash,
            BridgingVoteCode::Agree,
            hlc,
        )
        .expect("build");
        assert_eq!(ev.kind, PollEventKindCode::DeliberationVote);
        assert_eq!(ev.actor, actor);
        let payload: DeliberationVotePayload =
            ciborium::de::from_reader(&ev.payload[..]).expect("decode");
        assert_eq!(payload.poll_id, pid);
        assert_eq!(payload.statement_event_hash, stmt_hash);
        assert_eq!(BridgingVoteCode::from_u8(payload.vote), Some(BridgingVoteCode::Agree));
    }
```

(Match the exact helper-function name conventions actually used in lines 1670-1700; use `test_signing_key` or whatever pattern is already established.)

- [ ] **Step 1.8: Wire `DeliberationVote` into `apply_event`'s "kind not valid for Tier 3" rejection list IF needed.** Open `src-tauri/src/community_voting_tier3.rs:371-379` — the `kind @ (PollEventKindCode::PollCreate | ...)` match arm is the catch-all for "Tier 1/2 only" kinds. `DeliberationVote` is a Tier 3 kind, so it does NOT belong here. Instead, add a new arm for it in `apply_event` that's a parse-only no-op (mirrors the existing `DeliberationStatement` stub at lines 299-306):

```rust
    // kd=dv DeliberationVote: Phase 5 — payload parse only; full materialize in Task 4.
    PollEventKindCode::DeliberationVote => {
        let _payload: DeliberationVotePayload =
            decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
        // No-op: Task 4 wires DeliberationState insertion.
    }
```

This ensures Task 1's compile-time exhaustiveness check passes; Task 4 fleshes out the body.

- [ ] **Step 1.9: Run gates.**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(deliberation_vote) or test(bridging_vote_code) or test(wire_code) or test(signed_deliberation)' 2>&1 | tail -20
```

Expected: all new tests pass; no clippy warnings; no fmt diff.

- [ ] **Step 1.10: Commit.**

```bash
git add src-tauri/src/community_voting_core.rs src-tauri/src/community_voting_tier3.rs
git commit -m "$(cat <<'EOF'
feat(zeb-294): add DeliberationVote wire kind + BridgingVoteCode

Adds:
- PollEventKindCode::DeliberationVote ("dv" wire code)
- DeliberationVotePayload { pi, sh, vt } per spec §2.2 (same-length-keys)
- BridgingVoteCode enum (Agree/Disagree/Pass) + u8 + wire-str helpers
- build_signed_deliberation_vote helper for tests + future IPC

apply_event gets a parse-only arm; Task 4 wires real DeliberationState
insertion. No projection-state behavior yet — pure wire format + CBOR
round-trip + same-length-keys invariant pinning.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `DeliberationState` projection struct (passive integration)

**Goal:** Add `DeliberationState { statements, votes, statements_per_author }` to `Tier3PollState`. No new behavior in `apply_event` yet — that's Task 3 and Task 4. This task only adds the storage struct + default initialization + a passive helper method.

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs:60-92` (Tier3PollState struct + `new_from_create`)
- Modify: `src-tauri/src/community_voting_tier3.rs` (test module)

- [ ] **Step 2.1: Add Statement + VoteEntry + DeliberationState structs** above `Tier3PollState` at line ~60. Use `std::collections::BTreeMap` (import if not already present at file head).

```rust
use std::collections::BTreeMap;
// (Add to existing use statements if not present.)

/// A mini-public member's contribution during the deliberation stage,
/// stored after `kd=ds` apply. Immutable per spec §2 — no edit/retract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliberationStatement {
    pub event_hash: [u8; 32],
    pub author: OwnerAddr,
    pub text: String,
    pub created_at_hlc: Hlc,
}

/// A single (voter, statement) vote entry. LWW-resolved on apply by
/// `(last_update_hlc, last_update_event_hash)` lex comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliberationVoteEntry {
    pub voter: OwnerAddr,
    pub statement_event_hash: [u8; 32],
    pub vote: crate::community_voting_core::BridgingVoteCode,
    pub last_update_hlc: Hlc,
    pub last_update_event_hash: [u8; 32],
}

/// Tier 3 deliberation projection state. Materialized from `kd=ds` and
/// `kd=dv` events; consumed by the bridging algorithm + IPC read paths.
///
/// BTreeMap (not HashMap) for deterministic iteration — bridging-detection
/// determinism (spec acceptance criterion §3) depends on this.
#[derive(Debug, Clone, Default)]
pub struct DeliberationState {
    pub statements: BTreeMap<[u8; 32], DeliberationStatement>,
    pub votes: BTreeMap<(OwnerAddr, [u8; 32]), DeliberationVoteEntry>,
    pub statements_per_author: BTreeMap<OwnerAddr, u8>,
}
```

- [ ] **Step 2.2: Add the `deliberation` field to `Tier3PollState`** at line ~91 (just before `last_hlc`):

```rust
pub struct Tier3PollState {
    // ... existing fields ...
    pub ratification_ballots: Vec<RatificationBallotPayload>,
    pub close_event_hash: Option<[u8; 32]>,
    pub result: Option<StarResult>,
    /// Phase 5 (Tier 3b) deliberation projection. Populated by `kd=ds`
    /// and `kd=dv` apply paths. Default empty until Task 3/4 wire apply.
    pub deliberation: DeliberationState,
    pub last_hlc: Option<Hlc>,
}
```

- [ ] **Step 2.3: Initialize `deliberation: DeliberationState::default()` in `Tier3PollState::new_from_create`** at line ~233. Add the field to the struct literal:

```rust
Tier3PollState {
    meta,
    stage: Stage::Sortition,
    eligible_electorate_snapshot,
    sortition_result: None,
    declines: Vec::new(),
    candidates: Vec::new(),
    ratification_ballots: Vec::new(),
    close_event_hash: None,
    result: None,
    deliberation: DeliberationState::default(),
    last_hlc: None,
}
```

- [ ] **Step 2.4: Write a passive unit test** to confirm the new field initializes empty:

```rust
    #[test]
    fn new_tier3_poll_state_has_empty_deliberation() {
        let meta = test_meta(); // use whatever helper is already in the test module
        let poll = Tier3PollState::new_from_create(meta, vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])]);
        assert!(poll.deliberation.statements.is_empty());
        assert!(poll.deliberation.votes.is_empty());
        assert!(poll.deliberation.statements_per_author.is_empty());
    }
```

(`test_meta` is a placeholder — read the test module around line 1100 to find the actual fixture-builder helper used by neighboring tests; mirror its invocation.)

- [ ] **Step 2.5: Run gates.**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(new_tier3_poll_state_has_empty_deliberation)' 2>&1 | tail -10
```

- [ ] **Step 2.6: Commit.**

```bash
git add src-tauri/src/community_voting_tier3.rs
git commit -m "$(cat <<'EOF'
feat(zeb-294): add DeliberationState projection struct to Tier3PollState

Adds:
- DeliberationStatement (event_hash, author, text, created_at_hlc)
- DeliberationVoteEntry (voter, statement_event_hash, vote, last_update_*)
- DeliberationState (statements + votes BTreeMaps + statements_per_author)
- Tier3PollState::deliberation field, default empty in new_from_create

No apply-time behavior yet — Tasks 3 and 4 wire kd=ds + kd=dv materialize
paths into these maps. BTreeMap chosen for deterministic iteration order
(bridging-determinism guarantee, spec §4.7 point 2).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `apply_event` for `kd=ds` — DeliberationStatement materialize

**Goal:** Replace the existing parse-only stub at `community_voting_tier3.rs:299-306` with real materialize logic enforcing spec §2.3 apply rules: mini-public membership, decline status, stage window, text length 1..=280, spam-cap < 5. Silent-drop on failure (matches existing `DraftApproval` lookup-and-skip precedent at lines 326-336).

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs:299-306` (apply_event ds arm)
- Modify: `src-tauri/src/community_voting_tier3.rs` (test module — add 6 new tests)

- [ ] **Step 3.1: Replace the `DeliberationStatement` arm in `apply_event`** at line 301-306. New body:

```rust
    // kd=ds DeliberationStatement (Phase 5): materialize into deliberation
    // projection. Apply-time rules per spec §2.3:
    //   - Actor in current_mini_public(ev.hlc) [authoritative — handles
    //     declines + backup promotions]
    //   - Actor has NOT declined (no MiniPublicDecline with earlier HLC)
    //   - current_stage_at(ev.hlc) == Deliberation
    //   - Text length 1..=280; rejected if text.trim().is_empty()
    //   - Actor's prior-statement count < 5 (spam cap)
    // Failure → silent drop, debug log (matches DraftApproval precedent).
    PollEventKindCode::DeliberationStatement => {
        let payload: DeliberationStatementPayload =
            decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
        let event_hash = sha256_of_signing_bytes(ev);

        // Stage check: must be Deliberation at event HLC.
        if self.current_stage_at(&ev.hlc) != Stage::Deliberation {
            tracing::debug!(
                poll_id = %hex::encode(self.meta.poll_id.0),
                actor = %hex::encode(ev.actor.0),
                "kd=ds drop: not in Deliberation stage at ev.hlc"
            );
            self.last_hlc = Some(ev.hlc.clone());
            return Ok(());
        }

        // Mini-public membership (authoritative set; handles decline cascade).
        let mini_public = self.current_mini_public(&ev.hlc);
        if !mini_public.contains(&ev.actor) {
            tracing::debug!(
                poll_id = %hex::encode(self.meta.poll_id.0),
                actor = %hex::encode(ev.actor.0),
                "kd=ds drop: actor not in current mini-public"
            );
            self.last_hlc = Some(ev.hlc.clone());
            return Ok(());
        }

        // Text length: 1..=280 chars, non-empty after trim.
        if payload.text.chars().count() > 280 || payload.text.trim().is_empty() {
            tracing::debug!(
                poll_id = %hex::encode(self.meta.poll_id.0),
                actor = %hex::encode(ev.actor.0),
                "kd=ds drop: text length out of range or whitespace-only"
            );
            self.last_hlc = Some(ev.hlc.clone());
            return Ok(());
        }

        // Spam cap: actor's prior accepted statement count < 5.
        let prior_count = self
            .deliberation
            .statements_per_author
            .get(&ev.actor)
            .copied()
            .unwrap_or(0);
        if prior_count >= 5 {
            tracing::debug!(
                poll_id = %hex::encode(self.meta.poll_id.0),
                actor = %hex::encode(ev.actor.0),
                prior_count,
                "kd=ds drop: per-actor 5-statement spam cap reached"
            );
            self.last_hlc = Some(ev.hlc.clone());
            return Ok(());
        }

        // Accept: insert into projection.
        self.deliberation.statements.insert(
            event_hash,
            DeliberationStatement {
                event_hash,
                author: ev.actor,
                text: payload.text,
                created_at_hlc: ev.hlc.clone(),
            },
        );
        *self
            .deliberation
            .statements_per_author
            .entry(ev.actor)
            .or_insert(0) += 1;
    }
```

(Note: `tracing` is already a dependency on this crate — verify by checking the imports at the top of the file. If `use tracing;` is missing, add it.)

- [ ] **Step 3.2: Verify `sha256_of_signing_bytes` is already imported / accessible.** Check around line 312 where `kd=dc` uses it. It should be in scope — if not, add the import.

- [ ] **Step 3.3: Write 6 unit tests.** Place at the bottom of the `mod tests` block. Use the existing test fixtures (likely `test_meta`, `test_signing_key`, helpers around lines 1100-1300).

```rust
    // ── Task 3: apply_deliberation_statement materialize rules ──────────────────

    #[test]
    fn apply_ds_accepts_mini_public_member_in_window() {
        let mut poll = poll_in_deliberation_stage();
        let author = poll.sortition_result.as_ref().unwrap().primary[0];
        let ev = build_signed_ds_for_test(author, &poll, "Hello world", hlc_after_sortition(&poll));
        poll.apply_event(&ev).expect("apply");
        assert_eq!(poll.deliberation.statements.len(), 1);
        assert_eq!(poll.deliberation.statements_per_author[&author], 1);
    }

    #[test]
    fn apply_ds_drops_non_mini_public_actor() {
        let mut poll = poll_in_deliberation_stage();
        let outsider = OwnerAddr([0xFE; 16]);
        let ev = build_signed_ds_for_test(outsider, &poll, "Should not apply", hlc_after_sortition(&poll));
        poll.apply_event(&ev).expect("apply (silent drop)");
        assert_eq!(poll.deliberation.statements.len(), 0);
    }

    #[test]
    fn apply_ds_drops_281_char_text() {
        let mut poll = poll_in_deliberation_stage();
        let author = poll.sortition_result.as_ref().unwrap().primary[0];
        let too_long: String = "x".repeat(281);
        let ev = build_signed_ds_for_test(author, &poll, &too_long, hlc_after_sortition(&poll));
        poll.apply_event(&ev).expect("apply");
        assert_eq!(poll.deliberation.statements.len(), 0);
    }

    #[test]
    fn apply_ds_drops_whitespace_only_text() {
        let mut poll = poll_in_deliberation_stage();
        let author = poll.sortition_result.as_ref().unwrap().primary[0];
        let ev = build_signed_ds_for_test(author, &poll, "   \t\n  ", hlc_after_sortition(&poll));
        poll.apply_event(&ev).expect("apply");
        assert_eq!(poll.deliberation.statements.len(), 0);
    }

    #[test]
    fn apply_ds_enforces_5_statement_spam_cap() {
        let mut poll = poll_in_deliberation_stage();
        let author = poll.sortition_result.as_ref().unwrap().primary[0];
        for i in 0..6 {
            let ev = build_signed_ds_for_test(
                author,
                &poll,
                &format!("statement {}", i),
                hlc_at_step(&poll, i as u64),
            );
            poll.apply_event(&ev).expect("apply");
        }
        // 5 accepted, 6th dropped:
        assert_eq!(poll.deliberation.statements.len(), 5);
        assert_eq!(poll.deliberation.statements_per_author[&author], 5);
    }

    #[test]
    fn apply_ds_drops_event_outside_deliberation_window() {
        // Event with HLC AFTER deliberation_window expires.
        let mut poll = poll_in_deliberation_stage();
        let author = poll.sortition_result.as_ref().unwrap().primary[0];
        let post_deliberation_hlc = hlc_after_deliberation(&poll);
        let ev = build_signed_ds_for_test(author, &poll, "Too late", post_deliberation_hlc);
        poll.apply_event(&ev).expect("apply");
        assert_eq!(poll.deliberation.statements.len(), 0);
    }
```

(Test helpers `poll_in_deliberation_stage`, `build_signed_ds_for_test`, `hlc_after_sortition`, `hlc_at_step`, `hlc_after_deliberation` must be defined alongside these tests at the bottom of `mod tests`. Read existing test-builder helpers around lines 1100-1300 and mirror their style. A minimal helper sketch:

```rust
    fn poll_in_deliberation_stage() -> Tier3PollState {
        let mut poll = build_test_tier3_poll(/* args matching existing helpers */);
        // Apply kd=ss to advance to Deliberation:
        let ss_ev = build_signed_sortition_selection(...);
        poll.apply_event(&ss_ev).expect("ss apply");
        poll
    }

    fn build_signed_ds_for_test(
        author: OwnerAddr,
        poll: &Tier3PollState,
        text: &str,
        hlc: Hlc,
    ) -> SignedVotingEvent {
        let key = test_signing_key_for(author);
        build_signed_deliberation_statement(&key, author, poll.meta.poll_id, text.to_string(), hlc).expect("build")
    }

    fn hlc_after_sortition(poll: &Tier3PollState) -> Hlc {
        // Anytime > poll_create_hlc, < poll_create_hlc + deliberation_window_seconds
        Hlc::new(poll.meta.poll_create_hlc.wall_ms + 1_000, 0, "test".to_string())
    }
```

The actual helper implementations depend on what test infrastructure already exists. If there's no equivalent helper, write a minimal one in the test module — DRY-violations are fine inside the test module.)

- [ ] **Step 3.4: Run gates.**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(apply_ds)' 2>&1 | tail -20
```

Expected: 6 new tests pass; existing tests still green; no clippy warnings.

- [ ] **Step 3.5: Commit.**

```bash
git add src-tauri/src/community_voting_tier3.rs
git commit -m "$(cat <<'EOF'
feat(zeb-294): wire apply_event for kd=ds (DeliberationStatement)

Replaces the Phase 4 parse-only stub with full materialize logic per
spec §2.3:
- current_mini_public(ev.hlc) authoritative-set membership check
- current_stage_at(ev.hlc) == Deliberation gate
- Text length 1..=280; reject if text.trim().is_empty()
- Per-actor 5-statement spam cap (statements_per_author counter)

On failure: silent drop + debug log (matches DraftApproval lookup-and-skip
precedent). HLC monotonicity preserved via last_hlc update on every code
path, including drops.

Tests: 6 new unit tests covering all 5 reject paths + happy path.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `apply_event` for `kd=dv` — DeliberationVote LWW materialize

**Goal:** Replace the parse-only stub for `DeliberationVote` (added in Task 1.8) with full materialize logic per spec §2.3 — actor in mini-public, stage gated, target statement must exist in projection, valid `vote` u8, LWW by `(hlc, event_hash)`.

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (the `DeliberationVote` match arm added in Task 1.8)
- Modify: `src-tauri/src/community_voting_tier3.rs` (test module — 6 new tests)

- [ ] **Step 4.1: Replace the `DeliberationVote` parse-only stub** with full materialize logic. Body:

```rust
    // kd=dv DeliberationVote (Phase 5): LWW upsert by (voter, statement_hash)
    // per spec §2.3. Apply-time rules:
    //   - Actor in current_mini_public(ev.hlc)
    //   - current_stage_at(ev.hlc) == Deliberation
    //   - statement_event_hash references an existing kd=ds in projection
    //   - vote ∈ {0, 1, 2}
    // LWW key: (last_update_hlc, last_update_event_hash) lex comparison.
    // Failure → silent drop, debug log.
    PollEventKindCode::DeliberationVote => {
        let payload: DeliberationVotePayload =
            decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
        let event_hash = sha256_of_signing_bytes(ev);

        // Stage gate.
        if self.current_stage_at(&ev.hlc) != Stage::Deliberation {
            tracing::debug!(
                poll_id = %hex::encode(self.meta.poll_id.0),
                actor = %hex::encode(ev.actor.0),
                "kd=dv drop: not in Deliberation stage at ev.hlc"
            );
            self.last_hlc = Some(ev.hlc.clone());
            return Ok(());
        }

        // Mini-public membership.
        let mini_public = self.current_mini_public(&ev.hlc);
        if !mini_public.contains(&ev.actor) {
            tracing::debug!(
                poll_id = %hex::encode(self.meta.poll_id.0),
                actor = %hex::encode(ev.actor.0),
                "kd=dv drop: actor not in current mini-public"
            );
            self.last_hlc = Some(ev.hlc.clone());
            return Ok(());
        }

        // Vote byte must decode to a valid BridgingVoteCode.
        let vote_code = match crate::community_voting_core::BridgingVoteCode::from_u8(payload.vote) {
            Some(v) => v,
            None => {
                tracing::debug!(
                    poll_id = %hex::encode(self.meta.poll_id.0),
                    actor = %hex::encode(ev.actor.0),
                    vote_byte = payload.vote,
                    "kd=dv drop: vote byte out of range (must be 0/1/2)"
                );
                self.last_hlc = Some(ev.hlc.clone());
                return Ok(());
            }
        };

        // Target statement must exist in projection.
        if !self
            .deliberation
            .statements
            .contains_key(&payload.statement_event_hash)
        {
            tracing::debug!(
                poll_id = %hex::encode(self.meta.poll_id.0),
                actor = %hex::encode(ev.actor.0),
                statement = %hex::encode(payload.statement_event_hash),
                "kd=dv drop: target statement not in projection"
            );
            self.last_hlc = Some(ev.hlc.clone());
            return Ok(());
        }

        // LWW upsert by (voter, statement_hash). Compare (hlc, event_hash) lex.
        let key = (ev.actor, payload.statement_event_hash);
        let incoming_lww = (ev.hlc.clone(), event_hash);
        let should_insert = match self.deliberation.votes.get(&key) {
            None => true,
            Some(existing) => {
                let existing_lww = (existing.last_update_hlc.clone(), existing.last_update_event_hash);
                lww_lex_cmp(&incoming_lww, &existing_lww) == std::cmp::Ordering::Greater
            }
        };

        if should_insert {
            self.deliberation.votes.insert(
                key,
                DeliberationVoteEntry {
                    voter: ev.actor,
                    statement_event_hash: payload.statement_event_hash,
                    vote: vote_code,
                    last_update_hlc: ev.hlc.clone(),
                    last_update_event_hash: event_hash,
                },
            );
        }
        // Else: stale event (older HLC/hash than current entry); silent drop.
    }
```

- [ ] **Step 4.2: Add the `lww_lex_cmp` helper** at module scope (near the top of `community_voting_tier3.rs`, after the imports — wherever other private helpers live, or inline if no other helpers exist):

```rust
/// Lex comparison of (Hlc, [u8; 32]) LWW keys. Used by kd=dv apply to
/// resolve concurrent revotes deterministically.
///
/// Hlc compares as (wall_ms, logical, device_id) tuple; event_hash
/// breaks ties at the same HLC (extremely rare but possible if two
/// devices happen to share an HLC stamp).
fn lww_lex_cmp(a: &(Hlc, [u8; 32]), b: &(Hlc, [u8; 32])) -> std::cmp::Ordering {
    let (a_hlc, a_h) = a;
    let (b_hlc, b_h) = b;
    let a_tuple = (a_hlc.wall_ms, a_hlc.logical, a_hlc.device_id.as_str(), a_h);
    let b_tuple = (b_hlc.wall_ms, b_hlc.logical, b_hlc.device_id.as_str(), b_h);
    a_tuple.cmp(&b_tuple)
}
```

- [ ] **Step 4.3: Write 6 unit tests** in the test module:

```rust
    // ── Task 4: apply_deliberation_vote materialize rules ──────────────────

    #[test]
    fn apply_dv_accepts_valid_vote_from_mini_public() {
        let mut poll = poll_in_deliberation_stage();
        let mp_primary = poll.sortition_result.as_ref().unwrap().primary.clone();
        let (author, voter) = (mp_primary[0], mp_primary[1]);
        // First, accept the statement so dv has a target:
        let ds_ev = build_signed_ds_for_test(author, &poll, "Statement A", hlc_after_sortition(&poll));
        poll.apply_event(&ds_ev).expect("ds apply");
        let stmt_hash = *poll.deliberation.statements.keys().next().unwrap();
        // Now vote:
        let dv_ev = build_signed_dv_for_test(
            voter,
            poll.meta.poll_id,
            stmt_hash,
            crate::community_voting_core::BridgingVoteCode::Agree,
            hlc_at_step(&poll, 10),
        );
        poll.apply_event(&dv_ev).expect("dv apply");
        let entry = poll.deliberation.votes.get(&(voter, stmt_hash)).expect("entry exists");
        assert_eq!(entry.vote, crate::community_voting_core::BridgingVoteCode::Agree);
    }

    #[test]
    fn apply_dv_drops_non_existent_statement_reference() {
        let mut poll = poll_in_deliberation_stage();
        let voter = poll.sortition_result.as_ref().unwrap().primary[0];
        let fake_hash = [0x99u8; 32];
        let dv_ev = build_signed_dv_for_test(
            voter,
            poll.meta.poll_id,
            fake_hash,
            crate::community_voting_core::BridgingVoteCode::Agree,
            hlc_after_sortition(&poll),
        );
        poll.apply_event(&dv_ev).expect("apply");
        assert!(poll.deliberation.votes.is_empty());
    }

    #[test]
    fn apply_dv_drops_non_mini_public_voter() {
        let mut poll = poll_in_deliberation_stage();
        let author = poll.sortition_result.as_ref().unwrap().primary[0];
        let outsider = OwnerAddr([0xFE; 16]);
        let ds_ev = build_signed_ds_for_test(author, &poll, "stmt", hlc_after_sortition(&poll));
        poll.apply_event(&ds_ev).expect("ds apply");
        let stmt_hash = *poll.deliberation.statements.keys().next().unwrap();
        let dv_ev = build_signed_dv_for_test(
            outsider,
            poll.meta.poll_id,
            stmt_hash,
            crate::community_voting_core::BridgingVoteCode::Agree,
            hlc_at_step(&poll, 10),
        );
        poll.apply_event(&dv_ev).expect("apply");
        assert!(poll.deliberation.votes.is_empty());
    }

    #[test]
    fn apply_dv_revote_lww_later_hlc_wins() {
        let mut poll = poll_in_deliberation_stage();
        let mp = poll.sortition_result.as_ref().unwrap().primary.clone();
        let (author, voter) = (mp[0], mp[1]);
        let ds_ev = build_signed_ds_for_test(author, &poll, "stmt", hlc_after_sortition(&poll));
        poll.apply_event(&ds_ev).expect("ds apply");
        let stmt_hash = *poll.deliberation.statements.keys().next().unwrap();
        // First vote: Agree at HLC step 10
        let v1 = build_signed_dv_for_test(
            voter, poll.meta.poll_id, stmt_hash,
            crate::community_voting_core::BridgingVoteCode::Agree,
            hlc_at_step(&poll, 10),
        );
        poll.apply_event(&v1).expect("v1");
        // Revote: Disagree at HLC step 20
        let v2 = build_signed_dv_for_test(
            voter, poll.meta.poll_id, stmt_hash,
            crate::community_voting_core::BridgingVoteCode::Disagree,
            hlc_at_step(&poll, 20),
        );
        poll.apply_event(&v2).expect("v2");
        let entry = poll.deliberation.votes.get(&(voter, stmt_hash)).unwrap();
        assert_eq!(entry.vote, crate::community_voting_core::BridgingVoteCode::Disagree);
    }

    #[test]
    fn apply_dv_revote_lww_earlier_hlc_is_dropped() {
        let mut poll = poll_in_deliberation_stage();
        let mp = poll.sortition_result.as_ref().unwrap().primary.clone();
        let (author, voter) = (mp[0], mp[1]);
        let ds_ev = build_signed_ds_for_test(author, &poll, "stmt", hlc_after_sortition(&poll));
        poll.apply_event(&ds_ev).expect("ds apply");
        let stmt_hash = *poll.deliberation.statements.keys().next().unwrap();
        // First vote: Disagree at HLC step 20 (later)
        let v1 = build_signed_dv_for_test(
            voter, poll.meta.poll_id, stmt_hash,
            crate::community_voting_core::BridgingVoteCode::Disagree,
            hlc_at_step(&poll, 20),
        );
        poll.apply_event(&v1).expect("v1");
        // Out-of-order earlier event: Agree at HLC step 10
        let v2 = build_signed_dv_for_test(
            voter, poll.meta.poll_id, stmt_hash,
            crate::community_voting_core::BridgingVoteCode::Agree,
            hlc_at_step(&poll, 10),
        );
        // apply_event would normally reject out-of-order HLC at top of apply_event,
        // BUT this test demonstrates LWW semantics in the rare case it slips through.
        // If apply_event's monotonic check rejects, this test should still pass:
        // entry remains the v1 Disagree.
        let _ = poll.apply_event(&v2); // OK either way
        let entry = poll.deliberation.votes.get(&(voter, stmt_hash)).unwrap();
        assert_eq!(entry.vote, crate::community_voting_core::BridgingVoteCode::Disagree);
    }

    #[test]
    fn apply_dv_drops_pass_byte_value_3() {
        let mut poll = poll_in_deliberation_stage();
        let mp = poll.sortition_result.as_ref().unwrap().primary.clone();
        let (author, voter) = (mp[0], mp[1]);
        let ds_ev = build_signed_ds_for_test(author, &poll, "stmt", hlc_after_sortition(&poll));
        poll.apply_event(&ds_ev).expect("ds apply");
        let stmt_hash = *poll.deliberation.statements.keys().next().unwrap();
        // Build a vote with vote byte = 3 (out of range). Use the raw helper
        // that doesn't go through BridgingVoteCode:
        let raw_payload = DeliberationVotePayload {
            poll_id: poll.meta.poll_id,
            statement_event_hash: stmt_hash,
            vote: 3,
        };
        let dv_ev = sign_raw_payload_as_dv(voter, raw_payload, hlc_at_step(&poll, 10));
        poll.apply_event(&dv_ev).expect("apply");
        assert!(poll.deliberation.votes.is_empty());
    }
```

(`build_signed_dv_for_test` is a wrapper around `build_signed_deliberation_vote` mirroring the `build_signed_ds_for_test` pattern from Task 3. `sign_raw_payload_as_dv` is a test-only helper that signs an arbitrary `DeliberationVotePayload` — needed to build a deliberately-invalid vote byte=3. Sketch:

```rust
    fn build_signed_dv_for_test(
        voter: OwnerAddr,
        pid: PollId,
        stmt_hash: [u8; 32],
        vote: BridgingVoteCode,
        hlc: Hlc,
    ) -> SignedVotingEvent {
        let key = test_signing_key_for(voter);
        build_signed_deliberation_vote(&key, voter, pid, stmt_hash, vote, hlc).expect("build")
    }

    fn sign_raw_payload_as_dv(actor: OwnerAddr, payload: DeliberationVotePayload, hlc: Hlc) -> SignedVotingEvent {
        let key = test_signing_key_for(actor);
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&payload, &mut buf).expect("encode");
        let unsigned = UnsignedVotingEvent {
            tier: Tier::Sortition,
            kind: PollEventKindCode::DeliberationVote,
            community_id: SpaceId([0; 16]),
            actor,
            hlc,
            payload: buf,
        };
        SignedVotingEvent::from_unsigned(&key, unsigned).expect("sign")
    }
```

Match the actual signing helper signatures used in the test module — likely `test_signing_key_for(addr)` or a static keypair-map helper.)

- [ ] **Step 4.4: Run gates.**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(apply_dv)' 2>&1 | tail -20
```

- [ ] **Step 4.5: Commit.**

```bash
git add src-tauri/src/community_voting_tier3.rs
git commit -m "$(cat <<'EOF'
feat(zeb-294): wire apply_event for kd=dv (DeliberationVote)

LWW upsert by (voter, statement_event_hash) per spec §2.3:
- current_stage_at(ev.hlc) == Deliberation gate
- current_mini_public(ev.hlc) authoritative-set membership
- statement_event_hash must reference an existing kd=ds in projection
- vote byte must decode to a valid BridgingVoteCode (0/1/2); reject 3+
- lww_lex_cmp((hlc, event_hash)) for revote ordering across HLC stamps

Tests: 6 unit tests covering happy path + 4 reject paths + LWW direction.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `bridging` submodule — DoS heuristic with Q32/Q64 integer math

**Goal:** Implement the Diversity-of-Supporters bridging algorithm from spec §4 as a `bridging` submodule inside `src-tauri/src/community_voting_sortition.rs`. Pure-integer arithmetic (no `f64` in sort path), deterministic across engines.

**Files:**
- Modify: `src-tauri/src/community_voting_sortition.rs` (add `bridging` submodule)
- Modify: `src-tauri/src/community_voting_tier3.rs` (add `Tier3PollState::bridging_inputs(now)` helper)

- [ ] **Step 5.1: Add the `Tier3PollState::bridging_inputs` helper** to `community_voting_tier3.rs`, in the `impl Tier3PollState` block (near `current_mini_public` at line ~436):

```rust
    /// Build the (statements, votes, mini_public) tuple that the bridging
    /// algorithm consumes. Computed once per bridging IPC call so the
    /// authoritative-set snapshot is fixed for the entire computation
    /// (spec §4.7 determinism guarantee 4).
    pub fn bridging_inputs(
        &self,
        eval_hlc: &Hlc,
    ) -> (
        &BTreeMap<[u8; 32], DeliberationStatement>,
        &BTreeMap<(OwnerAddr, [u8; 32]), DeliberationVoteEntry>,
        std::collections::HashSet<OwnerAddr>,
    ) {
        let mini_public = self.current_mini_public(eval_hlc);
        (&self.deliberation.statements, &self.deliberation.votes, mini_public)
    }
```

- [ ] **Step 5.2: Add the `bridging` submodule** at the bottom of `community_voting_sortition.rs`:

```rust
// ── Phase 5 (Tier 3b) bridging-statement detection ─────────────────────────

pub mod bridging {
    //! Diversity-of-Supporters (DoS) bridging-statement detection per
    //! ZEB-294 spec §4. Pure integer arithmetic (Q32/Q64 fixed-point);
    //! no f64 in the sort path. Deterministic across engines.

    use crate::community_voting_core::BridgingVoteCode;
    use crate::community_voting_tier3::{DeliberationStatement, DeliberationVoteEntry};
    use crate::owner_state_types::OwnerAddr;
    use std::collections::{BTreeMap, HashSet};

    /// Per-statement bridging output. Sort by (`bridging_score_q64` DESC,
    /// `statement_event_hash` ASC) for deterministic ordering across engines.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BridgingScore {
        pub statement_event_hash: [u8; 32],
        pub statement_text: String,
        pub author: OwnerAddr,
        pub agree_count: u16,
        pub disagree_count: u16,
        pub pass_count: u16,
        /// Q32 fixed-point dissimilarity fraction in `[0, 2^32]`.
        pub diversity_q32: u64,
        /// `agree_count` × `diversity_q32`. Sort key.
        pub bridging_score_q64: u64,
    }

    /// Compute bridging scores for all statements. Deterministic given
    /// the same `(statements, votes, mini_public)` triple.
    ///
    /// Complexity: O(M² × S) precompute + O(S × |supporters|²) per stmt.
    /// Typical M=100 S=150 → ~3M ops; negligible.
    pub fn compute_bridging_scores(
        statements: &BTreeMap<[u8; 32], DeliberationStatement>,
        votes: &BTreeMap<(OwnerAddr, [u8; 32]), DeliberationVoteEntry>,
        mini_public: &HashSet<OwnerAddr>,
    ) -> Vec<BridgingScore> {
        // Sorted member list — BTreeSet would also work, but we need indexed
        // access for pairwise iteration.
        let mut members: Vec<OwnerAddr> = mini_public.iter().copied().collect();
        members.sort();

        // Sorted statement keys for deterministic iteration when building
        // per-member vote vectors. (statements is BTreeMap so iter is already
        // sorted; we collect to a Vec for indexed access.)
        let stmt_keys: Vec<[u8; 32]> = statements.keys().copied().collect();

        // Build per-member vote vector: V_m[stmt_idx] = +1 / -1 / 0.
        // Vec<Vec<i8>> indexed by (member_idx, stmt_idx).
        let n_members = members.len();
        let n_stmts = stmt_keys.len();
        let mut vote_vec: Vec<Vec<i8>> = vec![vec![0i8; n_stmts]; n_members];
        for (mi, m) in members.iter().enumerate() {
            for (si, s) in stmt_keys.iter().enumerate() {
                if let Some(entry) = votes.get(&(*m, *s)) {
                    vote_vec[mi][si] = match entry.vote {
                        BridgingVoteCode::Agree => 1,
                        BridgingVoteCode::Disagree => -1,
                        BridgingVoteCode::Pass => 0,
                    };
                }
            }
        }

        // Precompute pairwise dissimilarity matrix d_q32[i][j], i < j.
        // Triangular — store as flat Vec indexed by pair_index(i, j).
        // d_q32(m_i, m_j) = (disagree_count << 32) / max(1, joint_support)
        let pair_dissim: Vec<u64> = if n_members < 2 {
            Vec::new()
        } else {
            let mut out = Vec::with_capacity((n_members * (n_members - 1)) / 2);
            for i in 0..n_members {
                for j in (i + 1)..n_members {
                    let mut joint_support = 0u64;
                    let mut disagree_count = 0u64;
                    for si in 0..n_stmts {
                        let a = vote_vec[i][si];
                        let b = vote_vec[j][si];
                        if a != 0 && b != 0 {
                            joint_support += 1;
                            if a != b {
                                disagree_count += 1;
                            }
                        }
                    }
                    let d = if joint_support == 0 {
                        0u64
                    } else {
                        (disagree_count << 32) / joint_support
                    };
                    out.push(d);
                }
            }
            out
        };

        let pair_index = |i: usize, j: usize| -> usize {
            // For i < j and n total members: index = i * (2n - i - 1) / 2 + (j - i - 1)
            debug_assert!(i < j);
            i * (2 * n_members - i - 1) / 2 + (j - i - 1)
        };

        // Per-statement bridging score.
        let mut scores: Vec<BridgingScore> = Vec::with_capacity(n_stmts);
        for (si, s_hash) in stmt_keys.iter().enumerate() {
            let stmt = &statements[s_hash];

            // Aggregate vote counts AND supporter indices.
            let mut agree_indices: Vec<usize> = Vec::new();
            let mut agree_count = 0u16;
            let mut disagree_count = 0u16;
            let mut pass_count = 0u16;
            for (mi, m) in members.iter().enumerate() {
                if let Some(entry) = votes.get(&(*m, *s_hash)) {
                    match entry.vote {
                        BridgingVoteCode::Agree => {
                            agree_indices.push(mi);
                            agree_count += 1;
                        }
                        BridgingVoteCode::Disagree => disagree_count += 1,
                        BridgingVoteCode::Pass => pass_count += 1,
                    }
                }
                let _ = si; // silence unused
            }

            // Diversity_q32: mean pairwise dissimilarity among supporters.
            let diversity_q32 = if agree_indices.len() < 2 {
                0u64
            } else {
                let mut sum_d = 0u64;
                let mut pair_count = 0u64;
                for ai in 0..agree_indices.len() {
                    for aj in (ai + 1)..agree_indices.len() {
                        let (i, j) = (agree_indices[ai], agree_indices[aj]);
                        sum_d += pair_dissim[pair_index(i, j)];
                        pair_count += 1;
                    }
                }
                if pair_count == 0 { 0 } else { sum_d / pair_count }
            };

            // Bridging score = agree_count × diversity_q32 (u64 product).
            let bridging_score_q64 = (agree_count as u64) * diversity_q32;

            scores.push(BridgingScore {
                statement_event_hash: *s_hash,
                statement_text: stmt.text.clone(),
                author: stmt.author,
                agree_count,
                disagree_count,
                pass_count,
                diversity_q32,
                bridging_score_q64,
            });
        }

        // Sort by (bridging_score_q64 DESC, statement_event_hash ASC).
        scores.sort_by(|a, b| {
            b.bridging_score_q64
                .cmp(&a.bridging_score_q64)
                .then(a.statement_event_hash.cmp(&b.statement_event_hash))
        });

        scores
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::community_voting_core::BridgingVoteCode;
        use crate::community_voting_tier3::{DeliberationStatement, DeliberationVoteEntry};
        use crate::owner_state_types::OwnerAddr;
        use std::collections::{BTreeMap, HashSet};

        fn hash(b: u8) -> [u8; 32] { [b; 32] }
        fn addr(b: u8) -> OwnerAddr { OwnerAddr([b; 16]) }
        fn hlc(ms: u64) -> crate::owner_state_types::Hlc {
            crate::owner_state_types::Hlc::new(ms, 0, "test".to_string())
        }

        fn stmt(author: OwnerAddr, h: [u8; 32], text: &str) -> DeliberationStatement {
            DeliberationStatement {
                event_hash: h,
                author,
                text: text.to_string(),
                created_at_hlc: hlc(1000),
            }
        }

        fn vote(v: OwnerAddr, sh: [u8; 32], code: BridgingVoteCode) -> DeliberationVoteEntry {
            DeliberationVoteEntry {
                voter: v,
                statement_event_hash: sh,
                vote: code,
                last_update_hlc: hlc(2000),
                last_update_event_hash: [0; 32],
            }
        }

        #[test]
        fn empty_inputs_return_empty() {
            let res = compute_bridging_scores(&BTreeMap::new(), &BTreeMap::new(), &HashSet::new());
            assert!(res.is_empty());
        }

        #[test]
        fn single_statement_zero_supporters_scores_zero() {
            let mut statements = BTreeMap::new();
            statements.insert(hash(1), stmt(addr(1), hash(1), "S1"));
            let votes = BTreeMap::new();
            let mp: HashSet<_> = (1..=3).map(addr).collect();
            let res = compute_bridging_scores(&statements, &votes, &mp);
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].agree_count, 0);
            assert_eq!(res[0].bridging_score_q64, 0);
        }

        #[test]
        fn single_supporter_yields_zero_diversity() {
            let mut statements = BTreeMap::new();
            statements.insert(hash(1), stmt(addr(1), hash(1), "S1"));
            let mut votes = BTreeMap::new();
            votes.insert((addr(2), hash(1)), vote(addr(2), hash(1), BridgingVoteCode::Agree));
            let mp: HashSet<_> = (1..=3).map(addr).collect();
            let res = compute_bridging_scores(&statements, &votes, &mp);
            assert_eq!(res[0].agree_count, 1);
            assert_eq!(res[0].diversity_q32, 0); // single supporter → no pairs
            assert_eq!(res[0].bridging_score_q64, 0);
        }

        #[test]
        fn five_in_lockstep_supporters_score_less_than_five_cross_cluster() {
            // Setup: 6 members (1..=6) + 2 statements.
            //  S1 = "the bridging candidate"
            //  S2 = "the polarizer"
            // Lockstep scenario: members 1..=5 all agree on BOTH S1 and S2
            //                    (zero internal disagreement → diversity = 0)
            // Cross-cluster scenario: same supporters but disagree on a THIRD
            //                        statement S3 used for disagreement signal.
            let s1 = hash(1);
            let s2 = hash(2);
            let s3 = hash(3);

            // Lockstep: 5 supporters all agree on s1 + s2, no other votes.
            let mut statements_a = BTreeMap::new();
            statements_a.insert(s1, stmt(addr(0xAA), s1, "S1"));
            statements_a.insert(s2, stmt(addr(0xAA), s2, "S2"));
            let mut votes_a = BTreeMap::new();
            for v in 1..=5u8 {
                votes_a.insert((addr(v), s1), vote(addr(v), s1, BridgingVoteCode::Agree));
                votes_a.insert((addr(v), s2), vote(addr(v), s2, BridgingVoteCode::Agree));
            }
            let mp_a: HashSet<_> = (1..=5u8).map(addr).collect();
            let r_a = compute_bridging_scores(&statements_a, &votes_a, &mp_a);
            let s1_a_score = r_a.iter().find(|b| b.statement_event_hash == s1).unwrap().bridging_score_q64;

            // Cross-cluster: same agree on s1 + s2 BUT alternate on s3.
            let mut statements_b = BTreeMap::new();
            statements_b.insert(s1, stmt(addr(0xAA), s1, "S1"));
            statements_b.insert(s2, stmt(addr(0xAA), s2, "S2"));
            statements_b.insert(s3, stmt(addr(0xAA), s3, "S3"));
            let mut votes_b = BTreeMap::new();
            for v in 1..=5u8 {
                votes_b.insert((addr(v), s1), vote(addr(v), s1, BridgingVoteCode::Agree));
                votes_b.insert((addr(v), s2), vote(addr(v), s2, BridgingVoteCode::Agree));
                // Alternate: odd voters Agree on s3, even Disagree.
                let s3_vote = if v % 2 == 1 { BridgingVoteCode::Agree } else { BridgingVoteCode::Disagree };
                votes_b.insert((addr(v), s3), vote(addr(v), s3, s3_vote));
            }
            let mp_b = mp_a.clone();
            let r_b = compute_bridging_scores(&statements_b, &votes_b, &mp_b);
            let s1_b_score = r_b.iter().find(|b| b.statement_event_hash == s1).unwrap().bridging_score_q64;

            // s1 in scenario B should score HIGHER (supporters disagree on s3 → diverse).
            assert!(s1_b_score > s1_a_score,
                "cross-cluster score {} should beat lockstep {}", s1_b_score, s1_a_score);
            assert_eq!(s1_a_score, 0, "lockstep diversity is zero → bridging zero");
        }

        #[test]
        fn determinism_shuffle_votes_iteration_gives_same_output() {
            // Build same logical state two different ways via BTreeMap.
            // BTreeMap iteration is by-key, so insertion order doesn't matter
            // — but this test pins that guarantee.
            let s1 = hash(1);
            let s2 = hash(2);
            let mut statements = BTreeMap::new();
            statements.insert(s2, stmt(addr(0xBB), s2, "S2")); // out of order
            statements.insert(s1, stmt(addr(0xAA), s1, "S1"));
            let mut votes_a = BTreeMap::new();
            votes_a.insert((addr(1), s1), vote(addr(1), s1, BridgingVoteCode::Agree));
            votes_a.insert((addr(2), s1), vote(addr(2), s1, BridgingVoteCode::Agree));
            votes_a.insert((addr(3), s1), vote(addr(3), s1, BridgingVoteCode::Disagree));
            votes_a.insert((addr(1), s2), vote(addr(1), s2, BridgingVoteCode::Disagree));
            votes_a.insert((addr(2), s2), vote(addr(2), s2, BridgingVoteCode::Agree));
            let mut votes_b = BTreeMap::new();
            for (k, v) in votes_a.iter() {
                votes_b.insert(*k, v.clone());
            }
            let mp: HashSet<_> = (1..=3u8).map(addr).collect();
            let r_a = compute_bridging_scores(&statements, &votes_a, &mp);
            let r_b = compute_bridging_scores(&statements, &votes_b, &mp);
            assert_eq!(r_a, r_b);
        }

        #[test]
        fn sort_tie_break_by_statement_hash_ascending() {
            // Two statements with identical bridging_score_q64 (both zero) →
            // smaller hash comes first.
            let s_lo = hash(1);
            let s_hi = hash(2);
            let mut statements = BTreeMap::new();
            statements.insert(s_hi, stmt(addr(0xAA), s_hi, "Hi-hash"));
            statements.insert(s_lo, stmt(addr(0xAA), s_lo, "Lo-hash"));
            let res = compute_bridging_scores(&statements, &BTreeMap::new(), &HashSet::new());
            assert_eq!(res[0].statement_event_hash, s_lo);
            assert_eq!(res[1].statement_event_hash, s_hi);
        }

        #[test]
        fn pair_index_arithmetic_is_correct() {
            // n=4 → 6 pairs. Verify indexing matches expected upper-triangular order.
            let n: usize = 4;
            let pair_index = |i: usize, j: usize| -> usize {
                i * (2 * n - i - 1) / 2 + (j - i - 1)
            };
            assert_eq!(pair_index(0, 1), 0);
            assert_eq!(pair_index(0, 2), 1);
            assert_eq!(pair_index(0, 3), 2);
            assert_eq!(pair_index(1, 2), 3);
            assert_eq!(pair_index(1, 3), 4);
            assert_eq!(pair_index(2, 3), 5);
        }
    }
}
```

(If your `Hlc::new` signature differs from the placeholder above — check `src-tauri/src/owner_state_types.rs` — adjust accordingly. The shape `(wall_ms: u64, logical: u32, device_id: String)` matches the existing `last_tuple` comparison at community_voting_tier3.rs:267.)

- [ ] **Step 5.3: Run gates.**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(bridging::tests)' 2>&1 | tail -20
```

Expected: 7 new tests pass.

- [ ] **Step 5.4: Commit.**

```bash
git add src-tauri/src/community_voting_sortition.rs src-tauri/src/community_voting_tier3.rs
git commit -m "$(cat <<'EOF'
feat(zeb-294): bridging submodule + DoS algorithm (Q32/Q64 integer math)

Spec §4 Diversity-of-Supporters heuristic in src/community_voting_sortition/bridging:
- Q32 pairwise dissimilarity matrix from member vote vectors
- Q64 per-statement bridging score = agree_count × mean(pair_dissim)
- Sort by (bridging_score_q64 DESC, statement_event_hash ASC)
- Pure integer arithmetic; no f64 in sort path (acceptance §3 determinism)
- Tier3PollState::bridging_inputs(eval_hlc) helper assembles inputs with
  authoritative-set snapshot fixed at IPC entry

Tests: 7 unit tests — empty inputs / 0-1 supporter edge cases / lockstep
vs cross-cluster diversity correctness / sort tie-break / pair_index
arithmetic / iteration-order determinism.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: 3 Tauri IPCs + wire-format pinning extension

**Goal:** Add `voting_submit_deliberation_statement`, `voting_cast_deliberation_vote`, and `voting_list_bridging_statements` Tauri commands. Wire stage gating via `current_stage_at`. Extend `wire_format_voting_fixtures.rs` to pin canonical CBOR bytes for `DeliberationVotePayload`.

**Files:**
- Modify: `src-tauri/src/lib.rs` (3 new commands + handler registration + `BridgingScoreExport` wire DTO)
- Modify: `src-tauri/tests/wire_format_voting_fixtures.rs` (extend pinning)

- [ ] **Step 6.1: Add `BridgingScoreExport` wire DTO** in `lib.rs` near the existing `Tier3PollExport` (around line 22611). Hashes serialized as hex strings; u64 fixed-point values as decimal strings (JSON's `f64` precision-loss avoided):

```rust
/// camelCase wire DTO for a bridging-statement score row. u64 fixed-point
/// values are serialized as decimal strings to survive JSON's f64 mantissa
/// limit (2^53). Frontend converts to BigInt or Number based on need.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgingScoreExport {
    pub statement_event_hash: String, // 64-char hex
    pub statement_text: String,
    pub author: String,               // 32-char hex (OwnerAddr is 16 bytes → 32 hex)
    pub agree_count: u16,
    pub disagree_count: u16,
    pub pass_count: u16,
    /// Q32 fraction encoded as decimal u64 string, range [0, 2^32].
    pub diversity_q32: String,
    /// Q64 score encoded as decimal u64 string. Sort key.
    pub bridging_score_q64: String,
}

impl From<crate::community_voting_sortition::bridging::BridgingScore> for BridgingScoreExport {
    fn from(s: crate::community_voting_sortition::bridging::BridgingScore) -> Self {
        Self {
            statement_event_hash: hex::encode(s.statement_event_hash),
            statement_text: s.statement_text,
            author: hex::encode(s.author.0),
            agree_count: s.agree_count,
            disagree_count: s.disagree_count,
            pass_count: s.pass_count,
            diversity_q32: s.diversity_q32.to_string(),
            bridging_score_q64: s.bridging_score_q64.to_string(),
        }
    }
}
```

- [ ] **Step 6.2: Add the 3 IPC commands.** Place them adjacent to the existing `voting_get_tier3_poll` (line ~23982). Pattern: each command does the standard NodeState/AppHandle lookup, decodes hex inputs, locks the voting log mutex, projects state via `current_stage_at`, then performs the action.

Read `voting_get_tier3_poll` (lines 23982-24040) and `voting_get_tier3_poll_raw` (lines 24003-24040) to lift the exact lock-then-project pattern. Use the same `_raw` shim convention (the snake_case Tauri command wraps a private `_raw` async fn so unit tests can call it without the Tauri runtime).

Skeleton (must match exact NodeState extraction + parameter naming used in `voting_create_tier3_proposal` — read lines around 20633 to match style):

```rust
/// IPC: ZEB-294 — submit a deliberation statement during Tier 3b.
/// Stage gating: rejects unless current_stage_at(now) == Deliberation.
/// Author membership: rejects unless caller is in current_mini_public(now).
/// Spam cap: rejects if caller has 5+ accepted statements already.
/// Length: rejects unless 1..=280 chars and non-whitespace-only.
///
/// Returns the kd=ds event hash (64-char hex) on success.
#[tauri::command(rename_all = "snake_case")]
async fn voting_submit_deliberation_statement(
    app: tauri::AppHandle,
    state: tauri::State<'_, NodeState>,
    community_id: String,
    poll_id: String,
    text: String,
) -> Result<String, String> {
    let state_lock = state.lock_owned().await;
    voting_submit_deliberation_statement_raw(state_lock.inner(), &app, community_id, poll_id, text).await
}

async fn voting_submit_deliberation_statement_raw(
    state: &NodeStateInner,
    app: &tauri::AppHandle,
    community_id_hex: String,
    poll_id_hex: String,
    text: String,
) -> Result<String, String> {
    // 1. Decode community_id + poll_id (16-byte + 32-byte hex).
    let community_id = parse_space_id_hex(&community_id_hex)?;
    let poll_id = parse_poll_id_hex(&poll_id_hex)?;

    // 2. Pre-flight: text length 1..=280, non-whitespace.
    if text.chars().count() > 280 {
        return Err("statement text exceeds 280 chars".to_string());
    }
    if text.trim().is_empty() {
        return Err("statement text is empty or whitespace-only".to_string());
    }

    // 3. Locate voting log + ensure engine + lock log.
    let voting_log = ensure_voting_log_for(state, community_id).await?;
    let log_g = voting_log.lock().await;

    // 4. Stage check: must be Deliberation at "now".
    let now_hlc = current_node_hlc(state)?;
    let poll_state = log_g.polls.get(&poll_id).ok_or_else(|| {
        format!("poll {} not found", hex::encode(poll_id.0))
    })?;
    let t3 = poll_state.tier_state.as_tier3().ok_or_else(|| {
        format!("poll {} is not Tier 3", hex::encode(poll_id.0))
    })?;
    if t3.current_stage_at(&now_hlc) != crate::community_voting_tier3::Stage::Deliberation {
        return Err(format!("poll {} not in Deliberation stage", hex::encode(poll_id.0)));
    }

    // 5. Membership check: caller in current_mini_public.
    let caller = self_owner_addr(state)?;
    let mini_public = t3.current_mini_public(&now_hlc);
    if !mini_public.contains(&caller) {
        return Err("caller not in current mini-public for this poll".to_string());
    }

    // 6. Spam cap pre-check.
    let prior = t3.deliberation.statements_per_author.get(&caller).copied().unwrap_or(0);
    if prior >= 5 {
        return Err("per-actor 5-statement spam cap reached".to_string());
    }

    drop(log_g);

    // 7. Build + sign + publish via engine.publish_event (engine handles
    //    Zenoh + local apply + Tauri event emission).
    let signing_key = local_signing_key(state)?;
    let ev = crate::community_voting_core::build_signed_deliberation_statement(
        &signing_key,
        caller,
        poll_id,
        text,
        now_hlc,
    )
    .map_err(|e| format!("build kd=ds: {e:?}"))?;
    let event_hash_hex = hex::encode(crate::community_voting_core::sha256_of_signing_bytes(&ev));

    let engine = ensure_voting_engine_for(state, app, community_id).await?;
    engine.publish_event(community_id, ev).await
        .map_err(|e| format!("publish_event: {e}"))?;

    Ok(event_hash_hex)
}
```

(Replace placeholders `parse_space_id_hex`, `parse_poll_id_hex`, `ensure_voting_log_for`, `current_node_hlc`, `self_owner_addr`, `local_signing_key`, `ensure_voting_engine_for` with the actual helper names used by `voting_create_tier3_proposal`. Read lines 20600-20800 of lib.rs to lift the exact helper invocation pattern.)

- [ ] **Step 6.3: Add `voting_cast_deliberation_vote`** with the same skeleton + LWW awareness:

```rust
#[tauri::command(rename_all = "snake_case")]
async fn voting_cast_deliberation_vote(
    app: tauri::AppHandle,
    state: tauri::State<'_, NodeState>,
    community_id: String,
    poll_id: String,
    statement_event_hash: String,
    vote: String,
) -> Result<(), String> {
    let state_lock = state.lock_owned().await;
    voting_cast_deliberation_vote_raw(
        state_lock.inner(), &app, community_id, poll_id, statement_event_hash, vote,
    ).await
}

async fn voting_cast_deliberation_vote_raw(
    state: &NodeStateInner,
    app: &tauri::AppHandle,
    community_id_hex: String,
    poll_id_hex: String,
    statement_event_hash_hex: String,
    vote_str: String,
) -> Result<(), String> {
    let community_id = parse_space_id_hex(&community_id_hex)?;
    let poll_id = parse_poll_id_hex(&poll_id_hex)?;
    let stmt_hash_bytes = hex::decode(&statement_event_hash_hex)
        .map_err(|e| format!("decode statement_event_hash hex: {e}"))?;
    let stmt_hash: [u8; 32] = stmt_hash_bytes.as_slice().try_into()
        .map_err(|_| "statement_event_hash must be 32 bytes".to_string())?;
    let vote_code = crate::community_voting_core::BridgingVoteCode::from_wire_str(&vote_str)
        .ok_or_else(|| format!("invalid vote string {vote_str:?}; must be agree/disagree/pass"))?;

    let voting_log = ensure_voting_log_for(state, community_id).await?;
    let log_g = voting_log.lock().await;
    let now_hlc = current_node_hlc(state)?;
    let poll_state = log_g.polls.get(&poll_id).ok_or_else(|| {
        format!("poll {} not found", hex::encode(poll_id.0))
    })?;
    let t3 = poll_state.tier_state.as_tier3().ok_or_else(|| {
        format!("poll {} is not Tier 3", hex::encode(poll_id.0))
    })?;
    if t3.current_stage_at(&now_hlc) != crate::community_voting_tier3::Stage::Deliberation {
        return Err(format!("poll {} not in Deliberation stage", hex::encode(poll_id.0)));
    }
    let caller = self_owner_addr(state)?;
    if !t3.current_mini_public(&now_hlc).contains(&caller) {
        return Err("caller not in current mini-public".to_string());
    }
    if !t3.deliberation.statements.contains_key(&stmt_hash) {
        return Err("target statement not found in poll projection".to_string());
    }
    drop(log_g);

    let signing_key = local_signing_key(state)?;
    let ev = crate::community_voting_core::build_signed_deliberation_vote(
        &signing_key, caller, poll_id, stmt_hash, vote_code, now_hlc,
    )
    .map_err(|e| format!("build kd=dv: {e:?}"))?;

    let engine = ensure_voting_engine_for(state, app, community_id).await?;
    engine.publish_event(community_id, ev).await
        .map_err(|e| format!("publish_event: {e}"))?;

    Ok(())
}
```

- [ ] **Step 6.4: Add `voting_list_bridging_statements`** — read-only IPC, accepts stages `'de' | 'dr' | 'ra' | 'fi'`:

```rust
#[tauri::command(rename_all = "snake_case")]
async fn voting_list_bridging_statements(
    state: tauri::State<'_, NodeState>,
    community_id: String,
    poll_id: String,
    top_n: u16,
) -> Result<Vec<BridgingScoreExport>, String> {
    let state_lock = state.lock_owned().await;
    voting_list_bridging_statements_raw(state_lock.inner(), community_id, poll_id, top_n).await
}

async fn voting_list_bridging_statements_raw(
    state: &NodeStateInner,
    community_id_hex: String,
    poll_id_hex: String,
    top_n: u16,
) -> Result<Vec<BridgingScoreExport>, String> {
    let community_id = parse_space_id_hex(&community_id_hex)?;
    let poll_id = parse_poll_id_hex(&poll_id_hex)?;
    let voting_log = ensure_voting_log_for(state, community_id).await?;
    let log_g = voting_log.lock().await;
    let now_hlc = current_node_hlc(state)?;
    let poll_state = log_g.polls.get(&poll_id).ok_or_else(|| {
        format!("poll {} not found", hex::encode(poll_id.0))
    })?;
    let t3 = poll_state.tier_state.as_tier3().ok_or_else(|| {
        format!("poll {} is not Tier 3", hex::encode(poll_id.0))
    })?;
    let stage = t3.current_stage_at(&now_hlc);
    use crate::community_voting_tier3::Stage;
    if !matches!(stage, Stage::Deliberation | Stage::Drafting | Stage::Ratification | Stage::Finalized) {
        return Err(format!("bridging unavailable in stage {:?}", stage));
    }
    let (statements, votes, mini_public) = t3.bridging_inputs(&now_hlc);
    let scores = crate::community_voting_sortition::bridging::compute_bridging_scores(
        statements, votes, &mini_public,
    );
    let limit = (top_n as usize).min(scores.len());
    let truncated = scores.into_iter().take(limit).map(BridgingScoreExport::from).collect();
    Ok(truncated)
}
```

- [ ] **Step 6.5: Register the 3 new commands** in the `tauri::generate_handler![...]` macro list. Locate the macro invocation in `lib.rs` (search for `voting_get_tier3_poll` in the handler list; the new commands go alongside) and add:

```rust
voting_submit_deliberation_statement,
voting_cast_deliberation_vote,
voting_list_bridging_statements,
```

- [ ] **Step 6.6: Extend the wire-format pinning test.** Open `src-tauri/tests/wire_format_voting_fixtures.rs` and add a `DeliberationVotePayload` round-trip pinning the canonical CBOR bytes. Mirror the existing `DeliberationStatementPayload` test if present:

```rust
#[test]
fn deliberation_vote_payload_canonical_bytes_v1() {
    use harmony_app::community_voting_core::{DeliberationVotePayload, BridgingVoteCode};
    let payload = DeliberationVotePayload {
        poll_id: PollId([0x42; 32]),
        statement_event_hash: [0x7C; 32],
        vote: BridgingVoteCode::Agree.as_u8(),
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&payload, &mut buf).expect("encode");
    // Canonical bytes — these are the load-bearing fixture. Update this
    // assertion ONLY when a wire-format migration is intentional + minted.
    // Run the test once, observe the actual `hex::encode(&buf)`, paste that
    // hex string here.
    let expected_hex = "REPLACE_WITH_ACTUAL_CBOR_HEX_FROM_FIRST_RUN";
    assert_eq!(hex::encode(&buf), expected_hex,
        "DeliberationVotePayload CBOR wire format drift!");
}
```

(The first run fails with a hex mismatch; copy the actual CBOR bytes from the failure message into `expected_hex` and re-run. This is the standard "pin canonical bytes after first observation" workflow used by the channel-log fixtures.)

- [ ] **Step 6.7: Run gates.**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(deliberation) or test(bridging)' 2>&1 | tail -30
```

- [ ] **Step 6.8: Commit.**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/wire_format_voting_fixtures.rs
git commit -m "$(cat <<'EOF'
feat(zeb-294): 3 Tauri IPCs for deliberation + wire-format pinning

- voting_submit_deliberation_statement: stage gate + mini-public + spam-cap
- voting_cast_deliberation_vote: stage gate + mini-public + LWW (via engine apply)
- voting_list_bridging_statements: read-only across stages de/dr/ra/fi;
  uses Tier3PollState::bridging_inputs(now) for authoritative-set snapshot
- BridgingScoreExport: camelCase DTO; u64 fixed-point as decimal strings
  to avoid JSON f64 precision loss

Wire-format pinning: DeliberationVotePayload canonical CBOR bytes
asserted in tests/wire_format_voting_fixtures.rs to catch silent serde drift.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `Tier3PollExport` extension + IPC integration tests

**Goal:** Extend `Tier3PollExport` (camelCase wire DTO) with `deliberationStatements`, `myDeliberationStatementCount`, `myDeliberationVotes`. Vote aggregate counts ride on each statement export. Wire `build_tier3_export` to populate these. Add end-to-end IPC integration tests.

**Files:**
- Modify: `src-tauri/src/lib.rs:22611+` (Tier3PollExport struct extension + DeliberationStatementExport new type + build_tier3_export wiring)
- Create: `src-tauri/tests/community_voting_tier3_deliberation_ipc_integration.rs`

- [ ] **Step 7.1: Add `DeliberationStatementExport`** before `Tier3PollExport`:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliberationStatementExport {
    pub statement_event_hash: String, // 64-char hex
    pub author: String,               // 32-char hex
    pub text: String,
    pub created_at_hlc_ms: i128,
    pub agree_count: u16,
    pub disagree_count: u16,
    pub pass_count: u16,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyDeliberationVoteExport {
    pub statement_event_hash: String,
    pub vote: String, // "agree" | "disagree" | "pass"
}
```

- [ ] **Step 7.2: Extend `Tier3PollExport`** (line 22611). Add 3 new fields after `my_ratification_scores`:

```rust
    pub my_ratification_scores: Option<Vec<u8>>,
    /// Deliberation statements with aggregate vote counts. Ordered by
    /// statement_event_hash (BTreeMap iteration) — frontend re-sorts as needed.
    pub deliberation_statements: Vec<DeliberationStatementExport>,
    /// Caller's accepted-statement count in this poll (for spam-cap UX).
    pub my_deliberation_statement_count: u8,
    /// Caller's per-statement votes; entry exists only for statements the
    /// caller has voted on.
    pub my_deliberation_votes: Vec<MyDeliberationVoteExport>,
    pub winner_event_hash: Option<String>,
```

- [ ] **Step 7.3: Wire `build_tier3_export`** (line ~24041) to populate the new fields. Locate the function body and insert the projection logic right before the final struct literal:

```rust
    // Aggregate vote counts per statement, AND the caller's vote map.
    let mut agg_per_stmt: std::collections::BTreeMap<[u8; 32], (u16, u16, u16)> =
        std::collections::BTreeMap::new();
    let mut my_votes_vec: Vec<MyDeliberationVoteExport> = Vec::new();
    for ((voter, stmt_hash), entry) in t3.deliberation.votes.iter() {
        let agg = agg_per_stmt.entry(*stmt_hash).or_insert((0, 0, 0));
        match entry.vote {
            crate::community_voting_core::BridgingVoteCode::Agree => agg.0 += 1,
            crate::community_voting_core::BridgingVoteCode::Disagree => agg.1 += 1,
            crate::community_voting_core::BridgingVoteCode::Pass => agg.2 += 1,
        }
        if let Some(self_owner) = self_owner_opt {
            if voter == &self_owner {
                my_votes_vec.push(MyDeliberationVoteExport {
                    statement_event_hash: hex::encode(stmt_hash),
                    vote: entry.vote.as_wire_str().to_string(),
                });
            }
        }
    }

    let deliberation_statements: Vec<DeliberationStatementExport> = t3
        .deliberation
        .statements
        .iter()
        .map(|(_h, s)| {
            let (a, d, p) = agg_per_stmt.get(&s.event_hash).copied().unwrap_or((0, 0, 0));
            DeliberationStatementExport {
                statement_event_hash: hex::encode(s.event_hash),
                author: hex::encode(s.author.0),
                text: s.text.clone(),
                created_at_hlc_ms: s.created_at_hlc.wall_ms as i128,
                agree_count: a,
                disagree_count: d,
                pass_count: p,
            }
        })
        .collect();

    let my_deliberation_statement_count = self_owner_opt
        .and_then(|owner| t3.deliberation.statements_per_author.get(&owner).copied())
        .unwrap_or(0);
```

Then add to the struct literal at the end:

```rust
    Ok(Tier3PollExport {
        // ... existing fields ...
        my_ratification_scores: caller_scores,
        deliberation_statements,
        my_deliberation_statement_count,
        my_deliberation_votes: my_votes_vec,
        winner_event_hash,
        runner_up_event_hash,
    })
```

- [ ] **Step 7.4: Create IPC integration tests file** `src-tauri/tests/community_voting_tier3_deliberation_ipc_integration.rs`. Use the same test-runtime setup pattern as `community_voting_tier3_get_ipc_integration.rs` (the existing test from PR #152). Read it first to see the fixture-build + test-app-handle pattern.

```rust
//! IPC integration tests for ZEB-294 Tier 3b deliberation.
//!
//! Exercises voting_submit_deliberation_statement, voting_cast_deliberation_vote,
//! and voting_list_bridging_statements end-to-end through the test runtime.

use harmony_app::community_voting_core::BridgingVoteCode;
// (Other imports per existing integration test patterns.)

#[tokio::test]
async fn submit_statement_happy_path_emits_statement_in_export() {
    // Setup: single-engine test node in Deliberation stage.
    let fixture = build_tier3_in_deliberation_stage().await;
    let result = fixture
        .invoke_voting_submit_deliberation_statement(
            &fixture.community_id_hex(),
            &fixture.poll_id_hex(),
            "Our channels should require approval for joining".to_string(),
        )
        .await
        .expect("submit");
    let stmt_hash = result;
    let export = fixture.invoke_voting_get_tier3_poll(&fixture.poll_id_hex()).await.expect("get");
    assert_eq!(export.deliberation_statements.len(), 1);
    assert_eq!(export.deliberation_statements[0].statement_event_hash, stmt_hash);
    assert_eq!(export.my_deliberation_statement_count, 1);
}

#[tokio::test]
async fn submit_statement_rejects_observer() {
    let fixture = build_tier3_in_deliberation_stage_as_observer().await;
    let err = fixture
        .invoke_voting_submit_deliberation_statement(
            &fixture.community_id_hex(),
            &fixture.poll_id_hex(),
            "Should not work".to_string(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("mini-public"), "got: {err}");
}

#[tokio::test]
async fn submit_statement_rejects_6th_from_same_author() {
    let fixture = build_tier3_in_deliberation_stage().await;
    for i in 0..5 {
        fixture
            .invoke_voting_submit_deliberation_statement(
                &fixture.community_id_hex(),
                &fixture.poll_id_hex(),
                format!("statement {i}"),
            )
            .await
            .expect("submit");
    }
    let err = fixture
        .invoke_voting_submit_deliberation_statement(
            &fixture.community_id_hex(),
            &fixture.poll_id_hex(),
            "6th".to_string(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("spam cap"), "got: {err}");
}

#[tokio::test]
async fn cast_vote_revote_lww_updates_export() {
    let fixture = build_tier3_in_deliberation_stage().await;
    let stmt_hash = fixture
        .invoke_voting_submit_deliberation_statement(
            &fixture.community_id_hex(),
            &fixture.poll_id_hex(),
            "claim".to_string(),
        )
        .await
        .expect("submit");
    // Vote agree first.
    fixture
        .invoke_voting_cast_deliberation_vote(
            &fixture.community_id_hex(),
            &fixture.poll_id_hex(),
            &stmt_hash,
            "agree".to_string(),
        )
        .await
        .expect("v1");
    // Revote disagree.
    fixture
        .invoke_voting_cast_deliberation_vote(
            &fixture.community_id_hex(),
            &fixture.poll_id_hex(),
            &stmt_hash,
            "disagree".to_string(),
        )
        .await
        .expect("v2");
    let export = fixture.invoke_voting_get_tier3_poll(&fixture.poll_id_hex()).await.expect("get");
    let my_vote = export.my_deliberation_votes.iter()
        .find(|v| v.statement_event_hash == stmt_hash).expect("exists");
    assert_eq!(my_vote.vote, "disagree");
    assert_eq!(export.deliberation_statements[0].agree_count, 0);
    assert_eq!(export.deliberation_statements[0].disagree_count, 1);
}

#[tokio::test]
async fn cast_vote_rejects_non_existent_statement_target() {
    let fixture = build_tier3_in_deliberation_stage().await;
    let fake_hash = "0".repeat(64);
    let err = fixture
        .invoke_voting_cast_deliberation_vote(
            &fixture.community_id_hex(),
            &fixture.poll_id_hex(),
            &fake_hash,
            "agree".to_string(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("not found"), "got: {err}");
}

#[tokio::test]
async fn list_bridging_returns_sorted_desc_by_score() {
    let fixture = build_tier3_with_2_statements_3_supporters_each().await;
    let scores = fixture
        .invoke_voting_list_bridging_statements(
            &fixture.community_id_hex(),
            &fixture.poll_id_hex(),
            10,
        )
        .await
        .expect("list");
    assert_eq!(scores.len(), 2);
    // First entry has highest bridging_score_q64 (as decimal string compare).
    let s0: u64 = scores[0].bridging_score_q64.parse().unwrap();
    let s1: u64 = scores[1].bridging_score_q64.parse().unwrap();
    assert!(s0 >= s1);
}

#[tokio::test]
async fn list_bridging_rejects_sortition_stage() {
    let fixture = build_tier3_in_sortition_stage().await;
    let err = fixture
        .invoke_voting_list_bridging_statements(
            &fixture.community_id_hex(),
            &fixture.poll_id_hex(),
            10,
        )
        .await
        .unwrap_err();
    assert!(err.contains("Sortition"), "got: {err}");
}
```

(Build-fixture helpers `build_tier3_in_deliberation_stage`, `build_tier3_in_deliberation_stage_as_observer`, `build_tier3_with_2_statements_3_supporters_each`, `build_tier3_in_sortition_stage`, and their `invoke_*` methods are test-only utilities. Read `community_voting_tier3_get_ipc_integration.rs` to see the established pattern and extend it — fixture builders typically construct a `NodeState` + spawn the engine, then expose `invoke_*` methods that call `*_raw` directly.)

- [ ] **Step 7.5: Run gates.**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_voting_tier3_deliberation_ipc) or test(deliberation) or test(bridging)' 2>&1 | tail -30
```

- [ ] **Step 7.6: Commit.**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_voting_tier3_deliberation_ipc_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-294): Tier3PollExport extension + IPC integration tests

Tier3PollExport extension (camelCase wire DTO):
- deliberationStatements: Vec<DeliberationStatementExport> with aggregate
  agree/disagree/pass counts riding on each statement (avoids shipping the
  full vote-event log to the frontend)
- myDeliberationStatementCount: u8 (for composer 5-cap UX)
- myDeliberationVotes: caller's per-statement votes as wire-string enums

build_tier3_export wires the projection: one pass over t3.deliberation.votes
aggregates counts and captures caller's own votes simultaneously.

IPC integration tests (7 cases) cover happy path + spam-cap reject + revote
LWW + observer reject + bridging sort-DESC + stage-out-of-window reject.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Multi-engine integration test (determinism — acceptance criterion §3)

**Goal:** Two VotingLogEngines connected via real Zenoh transport. Engine A submits a statement, Engine B applies it. Engine B votes, Engine A applies. Both engines then run `compute_bridging_scores` and assert **bitwise-identical output**.

**Files:**
- Create: `src-tauri/tests/community_voting_tier3_deliberation_multi_engine_integration.rs`

- [ ] **Step 8.1: Locate the existing multi-engine test pattern.** Read `src-tauri/tests/community_voting_tier3_get_ipc_integration.rs` and any sibling `*_multi_engine_*.rs` test files. The pattern from ZEB-298+ZEB-312 PR 1 (`process_inbound_peer_apply_multi_engine_integration.rs` if present) is the closest analog — two real-Zenoh engines, each with its own NodeState, IPC events propagate over the wire.

```bash
ls src-tauri/tests/ | grep -E "(multi_engine|dfrost.*integration|voting.*integration)" | head -10
```

If `community_dfrost_transport_integration.rs` is the canonical multi-engine pattern (per ZEB-307 PR #146), read it first.

- [ ] **Step 8.2: Write the test file.**

```rust
//! ZEB-294 multi-engine integration: two voting engines on real Zenoh.
//! Acceptance criterion §3 + §5: deliberation events converge AND
//! `compute_bridging_scores` produces bitwise-identical output across engines.

use harmony_app::community_voting_core::BridgingVoteCode;
use harmony_app::community_voting_sortition::bridging::compute_bridging_scores;
// (Other imports follow the pattern from community_dfrost_transport_integration.rs)

/// Two-engine convergence:
///   1. Engine A and Engine B both have the same Tier 3 poll in Deliberation stage.
///   2. Engine A submits Statement-1 (text "A1") via voting_submit_deliberation_statement.
///   3. Engine B submits Statement-2 (text "B2") via the same IPC.
///   4. Engine A and Engine B both vote agree on each other's statements.
///   5. Wait for convergence (poll engine state).
///   6. Both engines compute_bridging_scores. Output MUST be bitwise-identical
///      (same Vec<BridgingScore> after serialization).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_converge_on_identical_bridging_output() {
    // Setup: spawn two engines with real-Zenoh transport on a private subnet.
    let (engine_a, engine_b) = spawn_paired_voting_engines_in_deliberation().await;

    // Engine A submits S1.
    let s1_hash = engine_a
        .invoke_voting_submit_deliberation_statement(
            &engine_a.community_id_hex(),
            &engine_a.poll_id_hex(),
            "Statement A1".to_string(),
        )
        .await
        .expect("A submit S1");

    // Engine B submits S2.
    let s2_hash = engine_b
        .invoke_voting_submit_deliberation_statement(
            &engine_b.community_id_hex(),
            &engine_b.poll_id_hex(),
            "Statement B2".to_string(),
        )
        .await
        .expect("B submit S2");

    // Wait for both engines to have both statements.
    wait_for_convergence(&engine_a, &engine_b, |fixture| async move {
        let exp = fixture
            .invoke_voting_get_tier3_poll(&fixture.poll_id_hex())
            .await
            .expect("get");
        exp.deliberation_statements.len() == 2
    })
    .await;

    // Engine A votes on both (agree on own + agree on B's).
    engine_a
        .invoke_voting_cast_deliberation_vote(
            &engine_a.community_id_hex(),
            &engine_a.poll_id_hex(),
            &s1_hash,
            "agree".to_string(),
        )
        .await
        .expect("A vote S1");
    engine_a
        .invoke_voting_cast_deliberation_vote(
            &engine_a.community_id_hex(),
            &engine_a.poll_id_hex(),
            &s2_hash,
            "agree".to_string(),
        )
        .await
        .expect("A vote S2");

    // Engine B votes too.
    engine_b
        .invoke_voting_cast_deliberation_vote(
            &engine_b.community_id_hex(),
            &engine_b.poll_id_hex(),
            &s1_hash,
            "disagree".to_string(),
        )
        .await
        .expect("B vote S1");
    engine_b
        .invoke_voting_cast_deliberation_vote(
            &engine_b.community_id_hex(),
            &engine_b.poll_id_hex(),
            &s2_hash,
            "agree".to_string(),
        )
        .await
        .expect("B vote S2");

    // Wait for both engines to have all 4 votes.
    wait_for_convergence(&engine_a, &engine_b, |fixture| async move {
        let exp = fixture
            .invoke_voting_get_tier3_poll(&fixture.poll_id_hex())
            .await
            .expect("get");
        let total_votes: u16 = exp
            .deliberation_statements
            .iter()
            .map(|s| s.agree_count + s.disagree_count + s.pass_count)
            .sum();
        total_votes == 4
    })
    .await;

    // Both engines compute bridging scores. Assert bitwise-identical.
    let scores_a = engine_a
        .invoke_voting_list_bridging_statements(
            &engine_a.community_id_hex(),
            &engine_a.poll_id_hex(),
            10,
        )
        .await
        .expect("A bridging");
    let scores_b = engine_b
        .invoke_voting_list_bridging_statements(
            &engine_b.community_id_hex(),
            &engine_b.poll_id_hex(),
            10,
        )
        .await
        .expect("B bridging");

    assert_eq!(scores_a.len(), scores_b.len(), "score count mismatch");
    for (sa, sb) in scores_a.iter().zip(scores_b.iter()) {
        assert_eq!(sa.statement_event_hash, sb.statement_event_hash);
        assert_eq!(sa.agree_count, sb.agree_count);
        assert_eq!(sa.disagree_count, sb.disagree_count);
        assert_eq!(sa.pass_count, sb.pass_count);
        assert_eq!(sa.diversity_q32, sb.diversity_q32,
            "diversity_q32 divergence! determinism property §4.7 broken");
        assert_eq!(sa.bridging_score_q64, sb.bridging_score_q64,
            "bridging_score_q64 divergence!");
    }
}

/// Helper: poll both engines until predicate holds on both, with timeout.
async fn wait_for_convergence<F, Fut>(
    a: &TestEngineHandle,
    b: &TestEngineHandle,
    predicate: F,
) where
    F: Fn(&TestEngineHandle) -> Fut + Clone,
    Fut: std::future::Future<Output = bool>,
{
    let timeout = std::time::Duration::from_secs(30);
    let start = std::time::Instant::now();
    loop {
        if predicate(a).await && predicate(b).await {
            return;
        }
        if start.elapsed() > timeout {
            panic!("convergence timeout after {timeout:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
```

(`spawn_paired_voting_engines_in_deliberation` is a test-helper that:
1. Builds 2 NodeStates with distinct device_ids
2. Establishes Zenoh sessions on the same local subnet (typical pattern: `zenoh::peer::open(config_with_loopback)`)
3. Creates the same Tier 3 community + poll on both
4. Applies kd=ss to both so mini-public is set
5. Returns 2 handles whose IPC methods mirror Task 7's fixture API

Read `community_dfrost_transport_integration.rs` to find the exact engine-pairing helper used in ZEB-307. If the helper exists as a public test utility (e.g. `harmony_app::test_util::spawn_paired_engines`), reuse it. Otherwise, copy the established setup pattern.)

- [ ] **Step 8.3: Run the multi-engine test in isolation** to catch any orchestration issues before re-running the full suite.

```bash
cd src-tauri
set -o pipefail
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(two_engines_converge_on_identical_bridging_output)' --test-threads 1 2>&1 | tail -40
```

Expected: passes within 30s (the convergence timeout). If it times out, the most likely cause is Zenoh subscriber wiring not flushing inbound — debug by running with `RUST_LOG=info,zenoh=info,harmony_app=debug`.

- [ ] **Step 8.4: Run full backend gates.**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -30
```

Confirm: orphan-failure count unchanged from Task 0's baseline; all new deliberation tests pass.

- [ ] **Step 8.5: Commit.**

```bash
git add src-tauri/tests/community_voting_tier3_deliberation_multi_engine_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-294): multi-engine bridging-determinism integration

Two voting engines on real Zenoh transport. Both engines submit a
statement and cast votes; once events converge, both run
compute_bridging_scores and assert bitwise-identical output.

This is acceptance criterion §3 + §5 combined — the load-bearing test
that catches Q32/Q64 arithmetic divergence across engines (the failure
mode that would break CRDT convergence on bridging output).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Frontend types + adapter bindings + event subscribers

**Goal:** Extend `types/voting.ts` with the new wire DTOs; add 3 IPC adapter methods + 2 event subscriber methods to `voting-adapter.ts`.

**Files:**
- Modify: `src/lib/types/voting.ts`
- Modify: `src/lib/voting-adapter.ts`

- [ ] **Step 9.1: Extend `types/voting.ts`** with the new wire types.

Open `src/lib/types/voting.ts`. Find the existing `Tier3PollExport` interface (it has `myRole`, `myRatificationScores`, etc.). Add the deliberation wire types before it:

```typescript
export type DeliberationVoteCode = 'agree' | 'disagree' | 'pass';

export interface DeliberationStatementExport {
  statementEventHash: string;       // 64-char hex
  author: string;                   // 32-char hex (OwnerAddr is 16 bytes)
  text: string;
  createdAtHlcMs: number;
  agreeCount: number;
  disagreeCount: number;
  passCount: number;
}

export interface MyDeliberationVoteExport {
  statementEventHash: string;
  vote: DeliberationVoteCode;
}

export interface BridgingScoreExport {
  statementEventHash: string;
  statementText: string;
  author: string;
  agreeCount: number;
  disagreeCount: number;
  passCount: number;
  /// Decimal string of Q32 fixed-point u64 (range 0..2^32). Frontend
  /// renders as a 0..1 float for visual heat bar; never used for sort.
  diversityQ32: string;
  /// Decimal string of Q64 fixed-point u64. Sort key.
  bridgingScoreQ64: string;
}

/// Tauri event payloads — match the Rust-side wire-DTO shapes.
export interface Tier3DeliberationStatementCreatedPayload {
  pollId: string;
  statementEventHash: string;
  author: string;
  text: string;
  createdAtHlcMs: number;
}

export interface Tier3DeliberationVoteCastPayload {
  pollId: string;
  statementEventHash: string;
  voter: string;
  vote: DeliberationVoteCode;
}
```

Then extend the existing `Tier3PollExport` interface to add 3 new fields:

```typescript
export interface Tier3PollExport {
  // ... existing fields ...
  myRatificationScores: number[] | null;
  deliberationStatements: DeliberationStatementExport[];
  myDeliberationStatementCount: number;
  myDeliberationVotes: MyDeliberationVoteExport[];
  winnerEventHash: string | null;
  runnerUpEventHash: string | null;
}
```

- [ ] **Step 9.2: Extend `voting-adapter.ts`** with 3 IPC methods + 2 event subscribers.

Open `src/lib/voting-adapter.ts`. Find the existing `castRatificationBallot` method (added in PR #149). Add the 3 new methods alongside, mirroring its shape:

```typescript
import type {
  // ... existing imports ...
  BridgingScoreExport,
  DeliberationVoteCode,
  Tier3DeliberationStatementCreatedPayload,
  Tier3DeliberationVoteCastPayload,
} from './types/voting';

// (Inside the VotingAdapter class)

  async submitDeliberationStatement(
    communityId: string,
    pollId: string,
    text: string,
  ): Promise<string> {
    return await this.invoke<string>('voting_submit_deliberation_statement', {
      communityId,
      pollId,
      text,
    });
  }

  async castDeliberationVote(
    communityId: string,
    pollId: string,
    statementEventHash: string,
    vote: DeliberationVoteCode,
  ): Promise<void> {
    await this.invoke<void>('voting_cast_deliberation_vote', {
      communityId,
      pollId,
      statementEventHash,
      vote,
    });
  }

  async listBridgingStatements(
    communityId: string,
    pollId: string,
    topN: number = 10,
  ): Promise<BridgingScoreExport[]> {
    return await this.invoke<BridgingScoreExport[]>('voting_list_bridging_statements', {
      communityId,
      pollId,
      topN,
    });
  }

  subscribeTier3DeliberationStatementCreated(
    handler: (payload: Tier3DeliberationStatementCreatedPayload) => void,
  ): () => void {
    return this.subscribeEvent('voting-tier3-deliberation-statement-created', handler);
  }

  subscribeTier3DeliberationVoteCast(
    handler: (payload: Tier3DeliberationVoteCastPayload) => void,
  ): () => void {
    return this.subscribeEvent('voting-tier3-deliberation-vote-cast', handler);
  }
```

(`invoke<T>` and `subscribeEvent` are existing private helpers on VotingAdapter — read the file to find their actual names, e.g. `private async invoke<T>(cmd: string, args: object): Promise<T>` and `private subscribeEvent<T>(name: string, h: (p: T) => void): () => void`. Match the actual signatures.)

- [ ] **Step 9.3: Run frontend gates.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: tsc passes cleanly (the new types are exported); vitest still green (no behavior change yet — components come in Task 10).

- [ ] **Step 9.4: Wire the Rust-side Tauri event emission** in `src-tauri/src/community_voting_log_engine.rs`. Locate the post-apply hook section near line 1259 (where `voting-tier3-sortition-complete` is emitted). Add two new hooks after the existing emit blocks:

```rust
// kd=ds applied → emit voting-tier3-deliberation-statement-created
if event.kind == PollEventKindCode::DeliberationStatement {
    // Re-decode payload + look up the freshly-applied statement to fill the payload.
    if let Ok(ds_payload) = ciborium::de::from_reader::<crate::community_voting_core::DeliberationStatementPayload, _>(&event.payload[..]) {
        let event_hash = crate::community_voting_core::sha256_of_signing_bytes(&event);
        let payload = serde_json::json!({
            "pollId": hex::encode(ds_payload.poll_id.0),
            "statementEventHash": hex::encode(event_hash),
            "author": hex::encode(event.actor.0),
            "text": ds_payload.text,
            "createdAtHlcMs": event.hlc.wall_ms as i128,
        });
        if let Err(e) = app_handle.emit("voting-tier3-deliberation-statement-created", &payload) {
            tracing::warn!(error = ?e, "voting-tier3-deliberation-statement-created emit failed (non-fatal)");
        }
    }
}

// kd=dv applied → emit voting-tier3-deliberation-vote-cast
if event.kind == PollEventKindCode::DeliberationVote {
    if let Ok(dv_payload) = ciborium::de::from_reader::<crate::community_voting_core::DeliberationVotePayload, _>(&event.payload[..]) {
        if let Some(vote_code) = crate::community_voting_core::BridgingVoteCode::from_u8(dv_payload.vote) {
            let payload = serde_json::json!({
                "pollId": hex::encode(dv_payload.poll_id.0),
                "statementEventHash": hex::encode(dv_payload.statement_event_hash),
                "voter": hex::encode(event.actor.0),
                "vote": vote_code.as_wire_str(),
            });
            if let Err(e) = app_handle.emit("voting-tier3-deliberation-vote-cast", &payload) {
                tracing::warn!(error = ?e, "voting-tier3-deliberation-vote-cast emit failed (non-fatal)");
            }
        }
    }
}
```

(Place this within the `process_inbound_dispatch` post-apply branch — the same code path where `voting-tier3-sortition-complete` is emitted. The exact insertion point is right after `apply_with_snapshot` succeeds and BEFORE the existing kd=ss / kd=da / kd=cl event-emit blocks. Match the surrounding `if event.kind == PollEventKindCode::X` style.)

- [ ] **Step 9.5: Run all gates.**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(deliberation) or test(bridging)' 2>&1 | tail -15
cd ..
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 9.6: Commit.**

```bash
git add src/lib/types/voting.ts src/lib/voting-adapter.ts src-tauri/src/community_voting_log_engine.rs
git commit -m "$(cat <<'EOF'
feat(zeb-294): frontend types + adapter bindings + Tauri event emission

Frontend (src/lib):
- types/voting.ts: DeliberationStatementExport, MyDeliberationVoteExport,
  BridgingScoreExport, DeliberationVoteCode, event payload interfaces.
  Tier3PollExport extended with 3 deliberation fields.
- voting-adapter.ts: submitDeliberationStatement, castDeliberationVote,
  listBridgingStatements IPC methods + 2 event subscribers.

Backend (src-tauri):
- community_voting_log_engine.rs: emit voting-tier3-deliberation-statement-created
  and voting-tier3-deliberation-vote-cast in process_inbound_dispatch
  post-apply hook (matches voting-tier3-sortition-complete pattern).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Frontend components — DeliberationView + 3 sub-components + mount + tests

**Goal:** Add 4 new Svelte 5 components implementing the two-column layout chosen in brainstorm; wire mount in `Tier3ProposalPanel.svelte`; ship vitest coverage for each.

**Files (NEW):**
- `src/lib/components/DeliberationView.svelte`
- `src/lib/components/StatementComposer.svelte`
- `src/lib/components/StatementVoteList.svelte`
- `src/lib/components/BridgingPanel.svelte`
- `src/lib/components/__tests__/DeliberationView.test.ts`
- `src/lib/components/__tests__/StatementComposer.test.ts`
- `src/lib/components/__tests__/StatementVoteList.test.ts`
- `src/lib/components/__tests__/BridgingPanel.test.ts`

**File (modified):**
- `src/lib/components/Tier3ProposalPanel.svelte:435` (mount conditional)

- [ ] **Step 10.1: Create `DeliberationView.svelte`** — the two-column container.

```svelte
<script lang="ts">
  /**
   * ZEB-294 — Tier 3b deliberation surface. Mounts inside Tier3ProposalPanel
   * when stage === 'de'. Two-column layout: composer + vote list on left,
   * live bridging panel on right. Refreshes bridging on every relevant
   * Tauri event.
   *
   * Per ZEB-287 R4: every $props field destructured below.
   * Per PR #152 R9: seq + key in-flight guard.
   */
  import { untrack } from 'svelte';
  import StatementComposer from './StatementComposer.svelte';
  import StatementVoteList from './StatementVoteList.svelte';
  import BridgingPanel from './BridgingPanel.svelte';
  import type { Tier3PollExport, BridgingScoreExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';

  let {
    detail,
    adapter,
    myAddr,
    onChange,
  }: {
    detail: Tier3PollExport;
    adapter: VotingAdapter;
    myAddr: string;
    onChange: () => void;
  } = $props();

  let bridgingScores = $state<BridgingScoreExport[]>([]);
  let bridgingError = $state<string | null>(null);

  // Plain `let` (NOT $state) per PR #152 R9 — increments inside $effect
  // cause effect_update_depth_exceeded if tracked.
  let bridgingRequestSeq = 0;

  async function loadBridging() {
    const seq = ++bridgingRequestSeq;
    const pollIdSnapshot = detail.pollId;
    const communityIdSnapshot = detail.communityId;
    try {
      const next = await adapter.listBridgingStatements(
        communityIdSnapshot,
        pollIdSnapshot,
        10,
      );
      if (seq !== bridgingRequestSeq || pollIdSnapshot !== detail.pollId) return;
      bridgingScores = next;
      bridgingError = null;
    } catch (e) {
      if (seq !== bridgingRequestSeq || pollIdSnapshot !== detail.pollId) return;
      bridgingError = e instanceof Error ? e.message : String(e);
    }
  }

  let unsubscribers: Array<() => void> = [];

  $effect(() => {
    // Re-arm subscriptions + initial load whenever the active poll changes.
    void detail.pollId;
    for (const u of unsubscribers) u();
    unsubscribers = [];
    bridgingScores = [];
    bridgingError = null;
    untrack(() => loadBridging());
    unsubscribers.push(
      adapter.subscribeTier3DeliberationStatementCreated(() => {
        onChange();
        loadBridging();
      }),
    );
    unsubscribers.push(
      adapter.subscribeTier3DeliberationVoteCast(() => {
        onChange();
        loadBridging();
      }),
    );
    return () => {
      for (const u of unsubscribers) u();
      unsubscribers = [];
    };
  });
</script>

<section class="deliberation-view">
  <h4>Deliberation</h4>
  <div class="two-column">
    <div class="left-col">
      {#if detail.myRole === 'mini_public'}
        <StatementComposer {detail} {adapter} {onChange} />
      {/if}
      <StatementVoteList {detail} {adapter} {myAddr} {onChange} />
    </div>
    <div class="right-col">
      <BridgingPanel scores={bridgingScores} error={bridgingError} />
    </div>
  </div>
</section>

<style>
  .deliberation-view { margin: 1rem 0; }
  .two-column {
    display: grid;
    grid-template-columns: 1.4fr 1fr;
    gap: 1rem;
  }
  @media (max-width: 720px) {
    .two-column { grid-template-columns: 1fr; }
  }
  .left-col, .right-col { display: flex; flex-direction: column; gap: 0.75rem; }
</style>
```

- [ ] **Step 10.2: Create `StatementComposer.svelte`** — mini-public + 5-cap gated, with click-confirm modal.

```svelte
<script lang="ts">
  /**
   * ZEB-294 — composer for mini-public deliberation statements.
   * Click-confirm modal (immutable → forces deliberate composition).
   * Disabled when stage exits Deliberation or 5-cap reached.
   */
  import type { Tier3PollExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';

  let {
    detail,
    adapter,
    onChange,
  }: {
    detail: Tier3PollExport;
    adapter: VotingAdapter;
    onChange: () => void;
  } = $props();

  let text = $state('');
  let confirming = $state(false);
  let submitting = $state(false);
  let submitError = $state<string | null>(null);

  let charsRemaining = $derived(280 - text.length);
  let canSubmit = $derived(
    text.trim().length > 0
      && text.length <= 280
      && detail.stage === 'de'
      && detail.myDeliberationStatementCount < 5
      && !submitting,
  );

  async function confirmSubmit() {
    confirming = false;
    submitting = true;
    submitError = null;
    try {
      await adapter.submitDeliberationStatement(detail.communityId, detail.pollId, text);
      text = '';
      onChange();
    } catch (e) {
      submitError = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }
</script>

<section class="composer">
  <h5>Compose statement</h5>
  <p class="cap-note">{detail.myDeliberationStatementCount} / 5 statements submitted</p>
  {#if detail.myDeliberationStatementCount >= 5}
    <p class="cap-warning">You've used all 5 statement slots for this poll.</p>
  {:else}
    <textarea
      maxlength="280"
      placeholder="Up to 280 characters. Statements are immutable once submitted."
      bind:value={text}
      disabled={submitting}
    ></textarea>
    <div class="footer">
      <span class="char-count">{charsRemaining} chars left</span>
      <button type="button" disabled={!canSubmit} onclick={() => (confirming = true)}>
        {submitting ? 'Submitting…' : 'Submit'}
      </button>
    </div>
    {#if submitError}<p class="error">{submitError}</p>{/if}
  {/if}
</section>

{#if confirming}
  <div class="confirm-modal" role="dialog" aria-modal="true" aria-label="Confirm statement">
    <div class="confirm-card">
      <p>Confirm statement submission</p>
      <blockquote class="preview">{text}</blockquote>
      <p class="caveat">Statements are immutable — once submitted, you cannot edit or retract.</p>
      <div class="actions">
        <button type="button" onclick={() => (confirming = false)}>Cancel</button>
        <button type="button" onclick={confirmSubmit}>Confirm</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .composer { background: var(--panel-bg, #1a1c24); padding: 0.75rem; border-radius: 6px; }
  textarea { width: 100%; min-height: 80px; padding: 0.4rem; background: #0e0f15; color: inherit; border: 1px solid #2a2c34; border-radius: 3px; }
  .footer { display: flex; justify-content: space-between; align-items: center; margin-top: 0.4rem; }
  .char-count { color: #8a8c95; font-size: 0.85rem; }
  .cap-note { color: #8a8c95; font-size: 0.8rem; margin: 0 0 0.4rem 0; }
  .cap-warning { color: #d9b438; font-size: 0.85rem; }
  .error { color: #d93838; }
  button { background: var(--accent, #4a9eff); color: #fff; border: 0; padding: 0.35rem 0.9rem; border-radius: 3px; cursor: pointer; }
  button:disabled { opacity: 0.5; cursor: not-allowed; }
  .confirm-modal { position: fixed; inset: 0; background: rgba(0,0,0,0.5); display: grid; place-items: center; z-index: 100; }
  .confirm-card { background: var(--panel-bg, #1a1c24); padding: 1.25rem; border-radius: 8px; max-width: 480px; display: flex; flex-direction: column; gap: 0.6rem; }
  .preview { background: #0e0f15; padding: 0.6rem; border-left: 3px solid var(--accent, #4a9eff); margin: 0; font-style: normal; }
  .caveat { color: #8a8c95; font-size: 0.8rem; }
  .actions { display: flex; gap: 0.5rem; justify-content: flex-end; }
  .actions button:last-child { background: var(--accent, #4a9eff); color: #fff; }
</style>
```

- [ ] **Step 10.3: Create `StatementVoteList.svelte`** — chronological list, tri-button voting (mini-public only), filter toggle.

```svelte
<script lang="ts">
  /**
   * ZEB-294 — statement vote list. Renders detail.deliberationStatements
   * chronologically ASC. Mini-public members see agree/disagree/pass
   * tri-button; observers see read-only count chips. "Unvoted by me"
   * filter defaults ON for mini-public.
   */
  import type {
    Tier3PollExport,
    DeliberationVoteCode,
    DeliberationStatementExport,
  } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';

  let {
    detail,
    adapter,
    myAddr,
    onChange,
  }: {
    detail: Tier3PollExport;
    adapter: VotingAdapter;
    myAddr: string;
    onChange: () => void;
  } = $props();

  // Default filter ON for mini-public, OFF for observers.
  let filterUnvoted = $state(detail.myRole === 'mini_public');

  let myVoteMap = $derived(
    new Map(detail.myDeliberationVotes.map((v) => [v.statementEventHash, v.vote])),
  );

  let isMiniPublic = $derived(detail.myRole === 'mini_public');
  let isWritable = $derived(isMiniPublic && detail.stage === 'de');

  let sortedStatements = $derived(
    [...detail.deliberationStatements].sort((a, b) => a.createdAtHlcMs - b.createdAtHlcMs),
  );

  let visibleStatements = $derived(
    filterUnvoted
      ? sortedStatements.filter((s) => !myVoteMap.has(s.statementEventHash))
      : sortedStatements,
  );

  let castError = $state<string | null>(null);
  let castingHash = $state<string | null>(null);

  async function castVote(statementEventHash: string, vote: DeliberationVoteCode) {
    castingHash = statementEventHash;
    castError = null;
    try {
      await adapter.castDeliberationVote(detail.communityId, detail.pollId, statementEventHash, vote);
      onChange();
    } catch (e) {
      castError = e instanceof Error ? e.message : String(e);
    } finally {
      castingHash = null;
    }
  }

  function authorShort(addr: string): string {
    return addr.length > 8 ? `${addr.slice(0, 8)}…` : addr;
  }

  function myVote(s: DeliberationStatementExport): DeliberationVoteCode | undefined {
    return myVoteMap.get(s.statementEventHash);
  }
</script>

<section class="vote-list">
  <header>
    <h5>Statements ({sortedStatements.length})</h5>
    {#if isMiniPublic}
      <label class="filter-toggle">
        <input type="checkbox" bind:checked={filterUnvoted} />
        Unvoted by me only
      </label>
    {/if}
  </header>

  {#if visibleStatements.length === 0}
    <p class="empty">
      {sortedStatements.length === 0
        ? 'No statements yet. Statements will appear here as mini-public members submit them.'
        : "You've voted on every statement currently visible. Toggle the filter off to revisit."}
    </p>
  {/if}

  <ol>
    {#each visibleStatements as s (s.statementEventHash)}
      <li class="row">
        <div class="text">{s.text}</div>
        <div class="meta">by {authorShort(s.author)}</div>
        {#if isWritable}
          <div class="tri-button">
            <button
              type="button"
              class:active={myVote(s) === 'agree'}
              disabled={castingHash === s.statementEventHash}
              onclick={() => castVote(s.statementEventHash, 'agree')}
            >👍 Agree</button>
            <button
              type="button"
              class:active={myVote(s) === 'disagree'}
              disabled={castingHash === s.statementEventHash}
              onclick={() => castVote(s.statementEventHash, 'disagree')}
            >👎 Disagree</button>
            <button
              type="button"
              class:active={myVote(s) === 'pass'}
              disabled={castingHash === s.statementEventHash}
              onclick={() => castVote(s.statementEventHash, 'pass')}
            >⊘ Pass</button>
          </div>
        {:else}
          <div class="chips">
            <span class="chip agree">👍 {s.agreeCount}</span>
            <span class="chip disagree">👎 {s.disagreeCount}</span>
            <span class="chip pass">⊘ {s.passCount}</span>
          </div>
        {/if}
      </li>
    {/each}
  </ol>
  {#if castError}<p class="error">{castError}</p>{/if}
</section>

<style>
  .vote-list { background: #0e1118; padding: 0.75rem; border-radius: 6px; }
  header { display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 0.5rem; }
  .filter-toggle { font-size: 0.85rem; color: #8a8c95; cursor: pointer; }
  ol { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 0.4rem; }
  .row { padding: 0.5rem; background: #1a1c24; border-radius: 4px; }
  .text { font-weight: 500; }
  .meta { font-size: 0.75rem; color: #8a8c95; margin-top: 0.2rem; }
  .tri-button { margin-top: 0.4rem; display: flex; gap: 0.3rem; }
  .tri-button button {
    background: #2a2c34; color: #d6d6d6; border: 1px solid transparent;
    padding: 0.2rem 0.5rem; border-radius: 3px; font-size: 0.8rem; cursor: pointer;
  }
  .tri-button button.active { border-color: var(--accent, #4a9eff); }
  .tri-button button:disabled { opacity: 0.5; cursor: not-allowed; }
  .chips { margin-top: 0.4rem; display: flex; gap: 0.4rem; font-size: 0.8rem; }
  .chip { padding: 0.1rem 0.4rem; background: #2a2c34; border-radius: 2px; color: #8a8c95; }
  .chip.agree { color: #4ad97a; }
  .chip.disagree { color: #d93838; }
  .empty { color: #8a8c95; font-style: italic; }
  .error { color: #d93838; }
</style>
```

- [ ] **Step 10.4: Create `BridgingPanel.svelte`** — top-10 bridging surface with heat bar.

```svelte
<script lang="ts">
  /**
   * ZEB-294 — bridging-statement surface. Renders BridgingScoreExport list
   * sorted DESC by bridging_score_q64 (already sorted by the IPC). Heat-bar
   * width = score / max_score * 100% (per-viewer-local f64; NEVER used for
   * sort).
   *
   * Empty state copy designed for the live state (not the empty state) per
   * feedback_design_for_eventual_state.
   */
  import type { BridgingScoreExport } from '../types/voting';

  let {
    scores,
    error,
  }: {
    scores: BridgingScoreExport[];
    error: string | null;
  } = $props();

  let maxScore = $derived(
    scores.length === 0 ? 1 : Math.max(...scores.map((s) => Number(s.bridgingScoreQ64))),
  );

  function heatPct(s: BridgingScoreExport): number {
    if (maxScore === 0) return 0;
    return Math.round((Number(s.bridgingScoreQ64) / maxScore) * 100);
  }

  function diversityPct(s: BridgingScoreExport): number {
    const q32 = Number(s.diversityQ32);
    return Math.round((q32 / 2 ** 32) * 100);
  }

  function authorShort(addr: string): string {
    return addr.length > 8 ? `${addr.slice(0, 8)}…` : addr;
  }
</script>

<aside class="bridging-panel">
  <h5>★ Bridging statements</h5>
  <p class="subtitle">Statements with broad support across people who otherwise disagree.</p>

  {#if error}
    <p class="error">Couldn't load bridging: {error}</p>
  {:else if scores.length === 0}
    <p class="empty">
      Bridging scores will appear once mini-public members vote on statements.
    </p>
  {:else}
    <ol>
      {#each scores as s (s.statementEventHash)}
        <li class="card">
          <div class="heat-bar" style:width={`${heatPct(s)}%`}></div>
          <div class="content">
            <p class="text">{s.statementText}</p>
            <div class="meta">
              <span>by {authorShort(s.author)}</span>
              <span class="chip agree">👍 {s.agreeCount}</span>
              <span class="chip diversity">diversity {diversityPct(s)}%</span>
            </div>
          </div>
        </li>
      {/each}
    </ol>
  {/if}
</aside>

<style>
  .bridging-panel { background: #0e1118; padding: 0.75rem; border-radius: 6px; }
  .subtitle { color: #8a8c95; font-size: 0.8rem; margin: 0 0 0.5rem 0; }
  ol { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 0.4rem; }
  .card { position: relative; padding: 0.5rem; background: #1a1c24; border-radius: 4px; overflow: hidden; }
  .heat-bar { position: absolute; left: 0; top: 0; bottom: 0; background: linear-gradient(to right, rgba(74, 217, 122, 0.18), rgba(74, 217, 122, 0)); z-index: 0; }
  .content { position: relative; z-index: 1; }
  .text { margin: 0; font-weight: 500; }
  .meta { margin-top: 0.3rem; display: flex; gap: 0.5rem; font-size: 0.75rem; color: #8a8c95; align-items: center; }
  .chip { padding: 0.05rem 0.35rem; background: #2a2c34; border-radius: 2px; }
  .chip.agree { color: #4ad97a; }
  .empty { color: #8a8c95; font-style: italic; }
  .error { color: #d93838; }
</style>
```

- [ ] **Step 10.5: Mount `DeliberationView` in `Tier3ProposalPanel.svelte:435`.** Read the file around line 433-437 and insert the new component BEFORE the existing `MiniPublicParticipationToggle` mount block. Add the import at the top:

```svelte
  import DeliberationView from './DeliberationView.svelte';
```

And the mount conditional:

```svelte
        {#if selectedDetail.stage === 'de'}
          <DeliberationView detail={selectedDetail} {adapter} {myAddr} onChange={refetchSelected} />
        {/if}
```

(Place this addition immediately AFTER the `SortitionRevealView` mount and BEFORE the existing `MiniPublicParticipationToggle` mount for stage 'de'/'dr'. The line range in the file may have shifted slightly since the plan was written — re-check with `grep -n "SortitionRevealView\|MiniPublicParticipationToggle\|DraftingPanel\|StarRatificationBallot" src/lib/components/Tier3ProposalPanel.svelte` and slot it appropriately.)

- [ ] **Step 10.6: Write `__tests__/DeliberationView.test.ts`.**

```typescript
import { render, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import DeliberationView from '../DeliberationView.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollExport } from '../../types/voting';

function createDetail(overrides: Partial<Tier3PollExport> = {}): Tier3PollExport {
  return {
    pollId: 'aa'.repeat(32),
    communityId: '11'.repeat(16),
    proposalText: 'Test proposal',
    proposer: '22'.repeat(32),
    stage: 'de',
    pollCreateHlcMs: 1_700_000_000_000,
    sortitionSize: 100,
    deliberationWindowSeconds: 1_209_600,
    draftingWindowSeconds: 604_800,
    ratificationWindowSeconds: 1_209_600,
    incentiveMode: 'd',
    miniPublic: ['33'.repeat(32)],
    backupPool: [],
    declined: [],
    draftCandidates: [],
    ratificationCandidates: [],
    myRole: 'mini_public',
    myDraftingApprovals: [],
    myRatificationScores: null,
    deliberationStatements: [],
    myDeliberationStatementCount: 0,
    myDeliberationVotes: [],
    winnerEventHash: null,
    runnerUpEventHash: null,
    ...overrides,
  };
}

function createAdapterMock() {
  const adapter = new VotingAdapter();
  vi.spyOn(adapter, 'listBridgingStatements').mockResolvedValue([]);
  vi.spyOn(adapter, 'subscribeTier3DeliberationStatementCreated').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3DeliberationVoteCast').mockReturnValue(() => {});
  return adapter;
}

describe('DeliberationView', () => {
  it('renders composer for mini-public', () => {
    const adapter = createAdapterMock();
    const { getByText } = render(DeliberationView, {
      props: { detail: createDetail(), adapter, myAddr: '33'.repeat(32), onChange: () => {} },
    });
    expect(getByText(/Compose statement/i)).toBeTruthy();
  });

  it('hides composer for observer', () => {
    const adapter = createAdapterMock();
    const { queryByText } = render(DeliberationView, {
      props: { detail: createDetail({ myRole: 'observer' }), adapter, myAddr: 'zz'.repeat(32), onChange: () => {} },
    });
    expect(queryByText(/Compose statement/i)).toBeNull();
  });

  it('loads bridging scores on mount', async () => {
    const adapter = createAdapterMock();
    render(DeliberationView, {
      props: { detail: createDetail(), adapter, myAddr: '33'.repeat(32), onChange: () => {} },
    });
    await waitFor(() => expect(adapter.listBridgingStatements).toHaveBeenCalledTimes(1));
    expect(adapter.listBridgingStatements).toHaveBeenLastCalledWith(
      '11'.repeat(16), 'aa'.repeat(32), 10,
    );
  });

  it('refreshes bridging when subscribeTier3DeliberationVoteCast fires', async () => {
    let voteHandler: (() => void) | null = null;
    const adapter = createAdapterMock();
    vi.spyOn(adapter, 'subscribeTier3DeliberationVoteCast').mockImplementation((h) => {
      voteHandler = h as () => void;
      return () => {};
    });
    render(DeliberationView, {
      props: { detail: createDetail(), adapter, myAddr: '33'.repeat(32), onChange: () => {} },
    });
    await waitFor(() => expect(adapter.listBridgingStatements).toHaveBeenCalledTimes(1));
    voteHandler!();
    await waitFor(() => expect(adapter.listBridgingStatements).toHaveBeenCalledTimes(2));
  });
});
```

- [ ] **Step 10.7: Write `__tests__/StatementComposer.test.ts`.**

```typescript
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StatementComposer from '../StatementComposer.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollExport } from '../../types/voting';

const baseDetail: Tier3PollExport = {
  pollId: 'aa'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Test',
  proposer: '22'.repeat(32),
  stage: 'de',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  deliberationWindowSeconds: 1_209_600,
  draftingWindowSeconds: 604_800,
  ratificationWindowSeconds: 1_209_600,
  incentiveMode: 'd',
  miniPublic: ['33'.repeat(32)],
  backupPool: [],
  declined: [],
  draftCandidates: [],
  ratificationCandidates: [],
  myRole: 'mini_public',
  myDraftingApprovals: [],
  myRatificationScores: null,
  deliberationStatements: [],
  myDeliberationStatementCount: 0,
  myDeliberationVotes: [],
  winnerEventHash: null,
  runnerUpEventHash: null,
};

describe('StatementComposer', () => {
  it('shows 5-cap warning when myDeliberationStatementCount === 5', () => {
    const adapter = new VotingAdapter();
    const { getByText, queryByPlaceholderText } = render(StatementComposer, {
      props: {
        detail: { ...baseDetail, myDeliberationStatementCount: 5 },
        adapter,
        onChange: () => {},
      },
    });
    expect(getByText(/used all 5 statement slots/i)).toBeTruthy();
    expect(queryByPlaceholderText(/Up to 280 characters/i)).toBeNull();
  });

  it('submit button opens confirm modal before invoking', async () => {
    const adapter = new VotingAdapter();
    vi.spyOn(adapter, 'submitDeliberationStatement').mockResolvedValue('hash');
    const { getByPlaceholderText, getByText, findByText } = render(StatementComposer, {
      props: { detail: baseDetail, adapter, onChange: () => {} },
    });
    await fireEvent.input(getByPlaceholderText(/Up to 280 characters/i), { target: { value: 'Hello' } });
    await fireEvent.click(getByText(/^Submit$/));
    expect(await findByText(/Confirm statement submission/i)).toBeTruthy();
    expect(adapter.submitDeliberationStatement).not.toHaveBeenCalled();
    await fireEvent.click(await findByText(/^Confirm$/));
    await waitFor(() =>
      expect(adapter.submitDeliberationStatement).toHaveBeenCalledWith('11'.repeat(16), 'aa'.repeat(32), 'Hello'),
    );
  });

  it('disables submit when stage is not de', async () => {
    const adapter = new VotingAdapter();
    const { getByPlaceholderText, getByText } = render(StatementComposer, {
      props: { detail: { ...baseDetail, stage: 'dr' }, adapter, onChange: () => {} },
    });
    await fireEvent.input(getByPlaceholderText(/Up to 280 characters/i), { target: { value: 'x' } });
    expect((getByText(/^Submit$/).closest('button') as HTMLButtonElement).disabled).toBe(true);
  });

  it('disables submit on whitespace-only text', async () => {
    const adapter = new VotingAdapter();
    const { getByPlaceholderText, getByText } = render(StatementComposer, {
      props: { detail: baseDetail, adapter, onChange: () => {} },
    });
    await fireEvent.input(getByPlaceholderText(/Up to 280 characters/i), { target: { value: '   \t  ' } });
    expect((getByText(/^Submit$/).closest('button') as HTMLButtonElement).disabled).toBe(true);
  });
});
```

- [ ] **Step 10.8: Write `__tests__/StatementVoteList.test.ts`.**

```typescript
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StatementVoteList from '../StatementVoteList.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollExport, DeliberationStatementExport } from '../../types/voting';

const stmt: DeliberationStatementExport = {
  statementEventHash: 'aa'.repeat(32),
  author: '33'.repeat(32),
  text: 'A bridging idea',
  createdAtHlcMs: 1_700_000_010_000,
  agreeCount: 0,
  disagreeCount: 0,
  passCount: 0,
};

const baseDetail: Tier3PollExport = {
  pollId: 'bb'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Test',
  proposer: '22'.repeat(32),
  stage: 'de',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  deliberationWindowSeconds: 1_209_600,
  draftingWindowSeconds: 604_800,
  ratificationWindowSeconds: 1_209_600,
  incentiveMode: 'd',
  miniPublic: ['33'.repeat(32)],
  backupPool: [],
  declined: [],
  draftCandidates: [],
  ratificationCandidates: [],
  myRole: 'mini_public',
  myDraftingApprovals: [],
  myRatificationScores: null,
  deliberationStatements: [stmt],
  myDeliberationStatementCount: 0,
  myDeliberationVotes: [],
  winnerEventHash: null,
  runnerUpEventHash: null,
};

describe('StatementVoteList', () => {
  it('renders tri-button for mini-public', () => {
    const adapter = new VotingAdapter();
    const { getByText } = render(StatementVoteList, {
      props: { detail: baseDetail, adapter, myAddr: '33'.repeat(32), onChange: () => {} },
    });
    expect(getByText(/👍 Agree/)).toBeTruthy();
    expect(getByText(/👎 Disagree/)).toBeTruthy();
    expect(getByText(/⊘ Pass/)).toBeTruthy();
  });

  it('renders read-only chips for observer', () => {
    const adapter = new VotingAdapter();
    const { queryByText, getByText } = render(StatementVoteList, {
      props: {
        detail: { ...baseDetail, myRole: 'observer' },
        adapter, myAddr: 'zz'.repeat(32), onChange: () => {},
      },
    });
    expect(queryByText(/👍 Agree/)).toBeNull();
    expect(getByText(/👍 0/)).toBeTruthy();
  });

  it('casts vote via adapter when tri-button clicked', async () => {
    const adapter = new VotingAdapter();
    vi.spyOn(adapter, 'castDeliberationVote').mockResolvedValue();
    const { getByText } = render(StatementVoteList, {
      props: { detail: baseDetail, adapter, myAddr: '33'.repeat(32), onChange: () => {} },
    });
    await fireEvent.click(getByText(/👍 Agree/));
    await waitFor(() =>
      expect(adapter.castDeliberationVote).toHaveBeenCalledWith(
        '11'.repeat(16), 'bb'.repeat(32), 'aa'.repeat(32), 'agree',
      ),
    );
  });

  it('filter "Unvoted by me" hides statements I have voted on', async () => {
    const adapter = new VotingAdapter();
    const { queryByText } = render(StatementVoteList, {
      props: {
        detail: {
          ...baseDetail,
          myDeliberationVotes: [{ statementEventHash: stmt.statementEventHash, vote: 'agree' }],
        },
        adapter,
        myAddr: '33'.repeat(32),
        onChange: () => {},
      },
    });
    // Statement is voted-on, filter is default-on for mini-public → hidden.
    expect(queryByText('A bridging idea')).toBeNull();
  });
});
```

- [ ] **Step 10.9: Write `__tests__/BridgingPanel.test.ts`.**

```typescript
import { render } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import BridgingPanel from '../BridgingPanel.svelte';
import type { BridgingScoreExport } from '../../types/voting';

const score1: BridgingScoreExport = {
  statementEventHash: 'aa'.repeat(32),
  statementText: 'Top bridging',
  author: '33'.repeat(32),
  agreeCount: 10,
  disagreeCount: 2,
  passCount: 1,
  diversityQ32: '2147483648', // ≈ 0.5
  bridgingScoreQ64: '21474836480',
};

const score2: BridgingScoreExport = {
  statementEventHash: 'bb'.repeat(32),
  statementText: 'Second bridging',
  author: '44'.repeat(32),
  agreeCount: 8,
  disagreeCount: 3,
  passCount: 1,
  diversityQ32: '1073741824',
  bridgingScoreQ64: '8589934592',
};

describe('BridgingPanel', () => {
  it('renders empty-state copy when scores is empty', () => {
    const { getByText } = render(BridgingPanel, { props: { scores: [], error: null } });
    expect(getByText(/Bridging scores will appear once/i)).toBeTruthy();
  });

  it('renders top-N cards when scores present', () => {
    const { getByText } = render(BridgingPanel, { props: { scores: [score1, score2], error: null } });
    expect(getByText('Top bridging')).toBeTruthy();
    expect(getByText('Second bridging')).toBeTruthy();
  });

  it('renders error string when error prop set', () => {
    const { getByText } = render(BridgingPanel, { props: { scores: [], error: 'IPC failed' } });
    expect(getByText(/IPC failed/i)).toBeTruthy();
  });
});
```

- [ ] **Step 10.10: Run all gates.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
cd src-tauri
cargo fmt --all
set -o pipefail
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(deliberation) or test(bridging)' 2>&1 | tail -20
```

- [ ] **Step 10.11: Commit.**

```bash
git add src/lib/components/DeliberationView.svelte \
        src/lib/components/StatementComposer.svelte \
        src/lib/components/StatementVoteList.svelte \
        src/lib/components/BridgingPanel.svelte \
        src/lib/components/__tests__/DeliberationView.test.ts \
        src/lib/components/__tests__/StatementComposer.test.ts \
        src/lib/components/__tests__/StatementVoteList.test.ts \
        src/lib/components/__tests__/BridgingPanel.test.ts \
        src/lib/components/Tier3ProposalPanel.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-294): mini-public deliberation UI — four new Svelte 5 components

- DeliberationView: two-column container, mounts on stage==='de'.
  Seq+key in-flight guard for bridging IPC. Subscribes to both Tauri
  events, refetches detail + bridging on every state-change signal.
- StatementComposer: mini-public-only; 280-char textarea + click-confirm
  modal (immutable submission per spec §2). Disables on 5-cap reached.
- StatementVoteList: chronological-ASC list with tri-button agree/disagree/
  pass (mini-public) or read-only count chips (observers). "Unvoted by me"
  filter defaults ON for mini-public, OFF for observers.
- BridgingPanel: top-10 bridging cards with f64-derived heat bar (visual
  only — NEVER used for sort per spec §4.7).
- Tier3ProposalPanel: small conditional mount when stage==='de'.

Frontend tests: 14 vitest cases covering composer 5-cap UX, vote-list filter
behavior + observer/mini-public split, bridging empty-state copy, event-
driven refresh.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Final 5-gate sweep + push + PR creation

**Goal:** Run the full CI gate matrix; verify orphan-failure count unchanged from Task 0; push branch; open PR with markdown-linked refs + `Closes ZEB-294`.

- [ ] **Step 11.1: Run the full backend gate matrix.**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
set -o pipefail
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tee /tmp/zeb-294-task-11-final.log
```

Expected:
- `cargo fmt --check` passes (no diff)
- `cargo clippy` passes (no warnings, since `-D warnings`)
- `cargo nextest`: orphan-failure count IDENTICAL to Task 0 baseline (`/tmp/zeb-294-task-0-baseline.log`)

If `cargo nextest` shows extra failures beyond the orphan list, identify the failing test and fix BEFORE proceeding. Compare:

```bash
grep -E "^\s+FAIL" /tmp/zeb-294-task-0-baseline.log | sort > /tmp/orphans-baseline.txt
grep -E "^\s+FAIL" /tmp/zeb-294-task-11-final.log | sort > /tmp/orphans-final.txt
diff /tmp/orphans-baseline.txt /tmp/orphans-final.txt
```

`diff` output must be empty. Any new failure is a regression we introduced.

- [ ] **Step 11.2: Run the frontend gate matrix.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: both pass cleanly.

- [ ] **Step 11.3: Push the branch + open the PR.**

```bash
git push -u origin zeb-294-tier3-deliberation
```

```bash
gh pr create --title "ZEB-294: Tier 3b Pol.is-style deliberation" --body "$(cat <<'EOF'
## Summary

Phase 5 of the [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) voting umbrella. Implements [ZEB-294](https://linear.app/zeblith/issue/ZEB-294) — Pol.is-style deliberation between Tier 3 Sortition and Drafting.

- Activates `kd=ds` (DeliberationStatement, Phase 4 scaffold) and adds new `kd=dv` (DeliberationVote) wire kind
- Diversity-of-Supporters bridging heuristic; pure-integer Q32/Q64 fixed-point math (no `f64` in the determinism path)
- Three new IPCs (`submit_deliberation_statement`, `cast_deliberation_vote`, `list_bridging_statements`)
- Two new Tauri events (`voting-tier3-deliberation-statement-created`, `voting-tier3-deliberation-vote-cast`)
- Four new Svelte 5 components (two-column DeliberationView + composer + vote list + bridging panel)
- Multi-engine integration test verifies bitwise-identical bridging output across engines (acceptance §3 + §5)

PCA-based opinion clustering is explicitly deferred per umbrella spec §13 — the heuristic ships first.

## Spec

- Design: `docs/specs/2026-05-21-zeb-294-tier3-deliberation-design.md`
- Plan: `docs/plans/2026-05-21-zeb-294-tier3-deliberation-plan.md`
- Umbrella: `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` §6.2, §6.6, §6.7, §13

## Test plan

- [x] `cargo fmt --all -- --check` passes
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` passes
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — no new failures beyond the pre-existing ~28 orphan-failure baseline
- [x] `npx tsc --noEmit` passes
- [x] `npx vitest run` passes
- [x] Multi-engine determinism test (`two_engines_converge_on_identical_bridging_output`) passes — acceptance §3
- [x] IPC integration tests cover happy path + spam-cap + revote LWW + observer reject + stage gating

## Acceptance criteria (per [ZEB-294](https://linear.app/zeblith/issue/ZEB-294))

1. ✅ Five CI gates green.
2. ✅ Deliberation statements + votes accepted from mini-public only (apply-time second-pass enforces).
3. ✅ Bridging-statement detection deterministic: identical event log → identical bridging output. Verified by multi-engine integration test.
4. ✅ UI usable for ~50–150 statement deliberations (typical mini-public size).
5. ✅ Multi-engine integration test: deliberation events converge + bridging detection converges across engines.
6. ✅ No regression on existing Tier 1/2/3 voting tests.

Closes ZEB-294

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

(`Closes ZEB-294` is bare per `feedback_linear_pr_auto_close` exception note: Closes-lines must target ONLY the issue this PR fully completes. Parent [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) is referenced as a markdown link, NOT a bare `ZEB-289` — that prevents Linear's GitHub integration from cascade-closing the umbrella epic.)

- [ ] **Step 11.4: Verify PR is open + branch is up-to-date.**

```bash
gh pr view --json state,url,headRefName,baseRefName,mergeable
```

Expected: state MERGEABLE / OPEN; baseRefName main; headRefName zeb-294-tier3-deliberation.

- [ ] **Step 11.5: Return control to the controller.** PR is open; control passes back for the autonomous bot-review monitoring loop (CodeRabbit + Cursor Bugbot + CodeAnt + Qodo per `feedback_autonomous_pr_monitoring_loop`). Greptile is manual-only per `feedback_greptile_manual_trigger`; CI is disabled per `feedback_ci_disabled`.

---

## Spec coverage self-review

Verified each spec section maps to a task:

| Spec section | Implemented in |
|---|---|
| §1 Architecture overview | Tasks 1-10 (file by file) |
| §2.1 DeliberationStatement (exists) | Task 3 (apply wiring) |
| §2.2 DeliberationVote wire format | Task 1 |
| §2.3 Verify rules — apply-time | Tasks 3 + 4 |
| §3.1 DeliberationState struct | Task 2 |
| §3.2 Materialize handlers | Tasks 3 + 4 |
| §4 Bridging algorithm | Task 5 |
| §5.1 IPCs | Task 6 |
| §5.2 Tauri events | Task 9 (Rust emission) + Task 9 (frontend subscriber) |
| §5.3 Confirmation severity tiers | Task 10 (composer click-confirm; vote no-confirm) |
| §5.4 In-flight race protection | Task 10 (seq+key in DeliberationView) |
| §6 UI components (4 files) | Task 10 |
| §6.2 Mounting at Tier3ProposalPanel:435 | Task 10.5 |
| §6.7 Tier3PollExport extension | Task 7 |
| §7 Eligibility (mini-public write, all read) | Tasks 3-4 (write gate); Task 6.4 (read all-stages) |
| §8 Visibility timeline | Task 10 (myRole-driven render variants) |
| §9 Tests | Tasks 3, 4, 5, 7, 8, 10 |
| §10 PR shape + acceptance criteria | Task 11 |
| §11 Open questions resolved | (locked-in via Tasks 1-10) |

No gaps. PCA upgrade (§Non-goals 1) and statement edit/retract (§Non-goals 2) are explicitly deferred — no task needed.
