# ZEB-311 Phase 4a-main UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the 6-component Svelte 5 UI for Tier 3 governance (sortition + STAR ratification, public ballots only) plus the two pull-style Tauri IPCs that ZEB-310 deferred (`voting_get_tier3_poll`, `voting_list_tier3_polls`).

**Architecture:** Backend additions are small (~250 LOC Rust): one new export DTO + two IPCs that project `Tier3PollState` into a frontend-friendly camelCased shape. Frontend adds 6 Svelte components (flat under `src/lib/components/`, matching Tier 1/Tier 2 pattern) plus a new `'tier3'` tab in `CommunityView.svelte`. The `Tier3ProposalPanel` is the parent container (lists polls via `listTier3Polls`, exposes the create form); the other 5 components render conditionally based on the poll's stage and the caller's role (proposer / mini-public / observer).

**Tech Stack:** Svelte 5 runes (`$props`, `$derived`, `$state`, `$effect`), Vitest with mocked Tauri adapter, Rust serde + ciborium CBOR, Tauri 2 IPC (`#[tauri::command(rename_all = "snake_case")]`).

---

## File Structure

**Create:**
- `src/lib/components/Tier3ProposalPanel.svelte` — create form + list of polls
- `src/lib/components/Tier3LifecycleStatus.svelte` — stage indicator + countdowns
- `src/lib/components/SortitionRevealView.svelte` — mini-public + backup roster
- `src/lib/components/MiniPublicParticipationToggle.svelte` — accept/decline
- `src/lib/components/DraftingPanel.svelte` — drafting candidates + approvals
- `src/lib/components/StarRatificationBallot.svelte` — 0-5 sliders per candidate
- `src/lib/components/__tests__/Tier3ProposalPanel.test.ts`
- `src/lib/components/__tests__/Tier3LifecycleStatus.test.ts`
- `src/lib/components/__tests__/SortitionRevealView.test.ts`
- `src/lib/components/__tests__/MiniPublicParticipationToggle.test.ts`
- `src/lib/components/__tests__/DraftingPanel.test.ts`
- `src/lib/components/__tests__/StarRatificationBallot.test.ts`
- `src-tauri/tests/wire_format_tier3_poll_export_fixtures.rs` — CBOR pinning for Tier3PollExport
- `src-tauri/tests/community_voting_tier3_get_ipc_integration.rs` — IPC happy path

**Modify:**
- `src-tauri/src/lib.rs` — add `Tier3PollExport`, `Tier3PollSummary`, `voting_get_tier3_poll`, `voting_list_tier3_polls` IPCs; register both in `invoke_handler`
- `src/lib/types/voting.ts` — add `Tier3PollExport`, `Tier3PollSummary`, `Tier3Stage`, `Tier3MyRole`, `DraftCandidateExport`, `RatificationCandidateExport` types
- `src/lib/voting-adapter.ts` — add `getTier3Poll(pollId)` + `listTier3Polls(communityId)`
- `src/lib/components/CommunityView.svelte` — wire `activeView === 'tier3'` route + tab nav

---

### Task 0: Pre-flight green-baseline confirm

No commit. Verify the working tree compiles cleanly and the 5 backend gates + 2 frontend gates pass against `origin/main` (`70b1ed7`) before changing anything.

- [ ] **Step 1: Confirm branch state**

Run: `git status && git rev-parse HEAD && git rev-parse origin/main`
Expected: clean tree, HEAD = `origin/main` = `70b1ed7`. If different, stop and surface to user.

- [ ] **Step 2: Run backend gates**

Run from `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt PASS, clippy PASS (0 warnings), nextest PASS aside from the 28 pre-existing orphan failures (folder_ingest::tests, mint::tests, mint_sync::tests, folder_ingest_walker_integration, rename_content_integration).

- [ ] **Step 3: Run frontend gates**

Run from repo root:
```bash
npx tsc --noEmit
npx vitest run
```
Expected: both PASS.

- [ ] **Step 4: Create branch (do NOT commit)**

Run: `git checkout -b zeb-311-tier3-ui`
Expected: branch created off `origin/main`. Working tree clean.

---

### Task 1: Add `Tier3PollExport` + `Tier3PollSummary` Rust types + wire-format CBOR pinning

**Files:**
- Modify: `src-tauri/src/lib.rs` (add structs near `Tier2ProposalExport` at line 22495)
- Create: `src-tauri/tests/wire_format_tier3_poll_export_fixtures.rs`

The export projects `Tier3PollState` into a frontend-friendly camelCased DTO. `Tier3PollSummary` is the lightweight list shape (the per-row data the panel needs without fetching candidate details).

- [ ] **Step 1: Write the failing wire-format pinning test**

Create `src-tauri/tests/wire_format_tier3_poll_export_fixtures.rs`:

```rust
//! ZEB-311: pin CBOR wire-format encoding of Tier3PollExport + Tier3PollSummary.
//!
//! These types are camelCased serde structs that flow through Tauri IPC to
//! the frontend. The CBOR round-trip is what guarantees JS-side field names
//! match the spec. Any field rename or default change must be deliberate and
//! reflected in this fixture.

use harmony_app::{Tier3PollExport, Tier3PollSummary, Tier3MyRole, Tier3StageTag};

#[test]
fn tier3_poll_export_round_trips_through_cbor() {
    let export = Tier3PollExport {
        poll_id: "aa".repeat(32),
        community_id: "11".repeat(16),
        proposal_text: "Amend charter §3".to_string(),
        proposer: "22".repeat(32),
        stage: Tier3StageTag::Drafting,
        poll_create_hlc_ms: 1_700_000_000_000,
        sortition_size: 100,
        deliberation_window_seconds: 1_209_600,
        drafting_window_seconds: 604_800,
        ratification_window_seconds: 1_209_600,
        incentive_mode: "d".to_string(),
        mini_public: vec!["33".repeat(32), "44".repeat(32)],
        backup_pool: vec!["55".repeat(32)],
        declined: vec![("44".repeat(32), 1_700_000_500_000)],
        draft_candidates: vec![],
        ratification_candidates: vec![],
        my_role: Tier3MyRole::MiniPublic,
        my_drafting_approvals: vec![],
        my_ratification_scores: None,
        winner_event_hash: None,
        runner_up_event_hash: None,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&export, &mut buf).expect("encode");
    let decoded: Tier3PollExport = ciborium::from_reader(&buf[..]).expect("decode");
    assert_eq!(decoded.poll_id, export.poll_id);
    assert_eq!(decoded.stage, Tier3StageTag::Drafting);
    assert_eq!(decoded.my_role, Tier3MyRole::MiniPublic);
}

#[test]
fn tier3_poll_summary_round_trips_through_cbor() {
    let summary = Tier3PollSummary {
        poll_id: "aa".repeat(32),
        community_id: "11".repeat(16),
        proposal_text: "Amend charter §3".to_string(),
        proposer: "22".repeat(32),
        stage: Tier3StageTag::Ratification,
        poll_create_hlc_ms: 1_700_000_000_000,
        sortition_size: 100,
        winner_text: None,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&summary, &mut buf).expect("encode");
    let decoded: Tier3PollSummary = ciborium::from_reader(&buf[..]).expect("decode");
    assert_eq!(decoded.stage, Tier3StageTag::Ratification);
    assert_eq!(decoded.winner_text, None);
}

#[test]
fn tier3_stage_tag_serializes_as_two_char_string() {
    use serde::Serialize;
    use serde_json::{self, Serializer};
    let mut buf = Vec::new();
    let mut ser = Serializer::new(&mut buf);
    Tier3StageTag::Drafting.serialize(&mut ser).unwrap();
    assert_eq!(std::str::from_utf8(&buf).unwrap(), "\"dr\"");
}
```

Run: `cd src-tauri && cargo nextest run --locked -E 'test(tier3_poll_export)' --features test-fixtures`
Expected: FAIL — `Tier3PollExport`, `Tier3PollSummary`, `Tier3MyRole`, `Tier3StageTag` don't exist yet.

- [ ] **Step 2: Add the types in lib.rs (near `Tier2ProposalExport` ~line 22495)**

```rust
/// ZEB-311: 2-char tag for Tier 3 poll stage. Serializes as a short
/// string so the JS-side switch matches the wire bytes without an
/// indirection (mirrors `kd` 2-char codes used elsewhere in the
/// voting layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Tier3StageTag {
    #[serde(rename = "so")]
    Sortition,
    #[serde(rename = "de")]
    Deliberation,
    #[serde(rename = "dr")]
    Drafting,
    #[serde(rename = "ra")]
    Ratification,
    #[serde(rename = "fi")]
    Finalized,
    #[serde(rename = "fa")]
    Failed,
}

impl From<crate::community_voting_tier3::Stage> for Tier3StageTag {
    fn from(s: crate::community_voting_tier3::Stage) -> Self {
        use crate::community_voting_tier3::Stage;
        match s {
            Stage::Sortition => Tier3StageTag::Sortition,
            Stage::Deliberation => Tier3StageTag::Deliberation,
            Stage::Drafting => Tier3StageTag::Drafting,
            Stage::Ratification => Tier3StageTag::Ratification,
            Stage::Finalized => Tier3StageTag::Finalized,
            Stage::Failed => Tier3StageTag::Failed,
        }
    }
}

/// ZEB-311: caller's role for a specific Tier 3 poll. Determines which
/// UI affordances are available — mini-public members get the
/// drafting + decline buttons; the proposer gets the retry affordance
/// on Failed; observers get read-only views of every stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier3MyRole {
    Proposer,
    MiniPublic,
    Backup,
    Observer,
}

/// ZEB-311: one draft candidate as exposed to the frontend.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftCandidateExport {
    /// 64-char hex of the SHA-256 of the kd=dc event's signing bytes.
    pub event_hash: String,
    pub text: String,
    /// 64-char hex of the proposer's OwnerAddr; `None` for the
    /// synthetic status_quo candidate.
    pub proposer: Option<String>,
    pub approval_count: u32,
}

/// ZEB-311: one ratification candidate (subset of `DraftCandidateExport`
/// with no proposer/approval bookkeeping; ratification cares only about
/// candidate identity + presentation text).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RatificationCandidateExport {
    pub event_hash: String,
    pub text: String,
}

/// ZEB-311: full state for a single Tier 3 poll, projected from
/// `Tier3PollState` plus caller-derived `my_*` fields.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tier3PollExport {
    pub poll_id: String,        // 64-char hex
    pub community_id: String,   // 32-char hex
    pub proposal_text: String,
    pub proposer: String,       // 64-char hex
    pub stage: Tier3StageTag,
    /// HLC of PollCreate in ms since UNIX_EPOCH (whatever the HLC's
    /// wall-clock projection resolves to).
    pub poll_create_hlc_ms: i128,
    pub sortition_size: u16,
    pub deliberation_window_seconds: u32,
    pub drafting_window_seconds: u32,
    pub ratification_window_seconds: u32,
    /// Single-character tag ("a" | "b" | "c" | "d") matching
    /// the validated config field.
    pub incentive_mode: String,
    /// 64-char hex `OwnerAddr` strings of the primary mini-public.
    /// Empty before kd=ss applies.
    pub mini_public: Vec<String>,
    pub backup_pool: Vec<String>,
    /// (owner_hex, hlc_ms) pairs for each kd=md decline.
    pub declined: Vec<(String, i128)>,
    pub draft_candidates: Vec<DraftCandidateExport>,
    pub ratification_candidates: Vec<RatificationCandidateExport>,
    pub my_role: Tier3MyRole,
    /// Event hashes of draft candidates the caller has approved.
    pub my_drafting_approvals: Vec<String>,
    /// Caller's most recent ratification ballot scores (indexed by
    /// `ratification_candidates` order). `None` if never cast or
    /// stage hasn't reached Ratification.
    pub my_ratification_scores: Option<Vec<u8>>,
    /// 64-char hex of the winning candidate's kd=dc event hash;
    /// `None` until stage = Finalized.
    pub winner_event_hash: Option<String>,
    pub runner_up_event_hash: Option<String>,
}

/// ZEB-311: lightweight per-row shape returned by
/// `voting_list_tier3_polls`. Doesn't include drafting candidates or
/// ratification scores — the panel pulls those via `get_tier3_poll`
/// when the user expands a row.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tier3PollSummary {
    pub poll_id: String,
    pub community_id: String,
    pub proposal_text: String,
    pub proposer: String,
    pub stage: Tier3StageTag,
    pub poll_create_hlc_ms: i128,
    pub sortition_size: u16,
    /// Set once stage = Finalized; lets the panel show "Charter §3
    /// amended" without an extra fetch.
    pub winner_text: Option<String>,
}
```

- [ ] **Step 3: Re-export from the crate root**

In `src-tauri/src/lib.rs`, ensure each of `Tier3PollExport`, `Tier3PollSummary`, `Tier3MyRole`, `Tier3StageTag`, `DraftCandidateExport`, `RatificationCandidateExport` is `pub` so the integration test can import via `use harmony_app::*`. They're already in the root crate; just ensure each `struct`/`enum` has `pub` (not `pub(crate)`).

- [ ] **Step 4: Run the fixture test**

Run: `cd src-tauri && cargo nextest run --locked -E 'test(tier3_poll_export)' --features test-fixtures`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/wire_format_tier3_poll_export_fixtures.rs
git commit -m "feat(zeb-311): add Tier3PollExport + Tier3PollSummary wire types

Frontend-facing DTOs that project Tier3PollState into a camelCased
shape consumed by the upcoming Tier 3 UI. CBOR-pinned via fixture.
"
```

---

### Task 2: `voting_get_tier3_poll` IPC + integration test

**Files:**
- Modify: `src-tauri/src/lib.rs` (add IPC near `voting_get_tier2_proposal` ~line 23789; register in both `invoke_handler` sites at 26591 and 26661)
- Create: `src-tauri/tests/community_voting_tier3_get_ipc_integration.rs`

- [ ] **Step 1: Write the failing integration test**

Create `src-tauri/tests/community_voting_tier3_get_ipc_integration.rs`:

```rust
//! ZEB-311: integration tests for voting_get_tier3_poll IPC.
//! Builds a NodeState with a known Tier 3 poll applied directly to
//! VotingLog, then drives the IPC through its async fn signature and
//! asserts the Tier3PollExport is shaped correctly per stage.

// NOTE: this test reaches into private types via the test-fixtures
// feature; mirrors the pattern in community_voting_tier3_ipc_integration.rs.

#![cfg(feature = "test-fixtures")]

use harmony_app::{
    test_helpers::tier3_test_harness::*,
    Tier3MyRole, Tier3StageTag,
};

#[tokio::test]
async fn get_tier3_poll_returns_sortition_stage_with_observer_role() {
    let h = Tier3TestHarness::with_poll_in_sortition_stage().await;
    let export = h.get_tier3_poll(&h.poll_id_hex).await.expect("ok");
    assert_eq!(export.stage, Tier3StageTag::Sortition);
    assert_eq!(export.my_role, Tier3MyRole::Observer);
    assert!(export.mini_public.is_empty(), "kd=ss not yet applied");
    assert!(export.draft_candidates.is_empty());
}

#[tokio::test]
async fn get_tier3_poll_returns_drafting_stage_with_mini_public_role_when_self_selected() {
    let h = Tier3TestHarness::with_poll_in_drafting_stage_and_self_in_mini_public().await;
    let export = h.get_tier3_poll(&h.poll_id_hex).await.expect("ok");
    assert_eq!(export.stage, Tier3StageTag::Drafting);
    assert_eq!(export.my_role, Tier3MyRole::MiniPublic);
    assert!(!export.mini_public.is_empty());
}

#[tokio::test]
async fn get_tier3_poll_returns_error_on_unknown_poll() {
    let h = Tier3TestHarness::empty().await;
    let err = h.get_tier3_poll("00".repeat(32).as_str()).await.unwrap_err();
    assert!(err.contains("not found"));
}

#[tokio::test]
async fn get_tier3_poll_returns_error_on_tier_mismatch() {
    let h = Tier3TestHarness::with_tier1_poll().await;
    let err = h.get_tier3_poll(&h.poll_id_hex).await.unwrap_err();
    assert!(err.contains("not tier3") || err.contains("not Tier3"));
}
```

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(get_tier3_poll)'`
Expected: FAIL — harness module + IPC don't exist yet.

- [ ] **Step 2: Build a tiny test harness module**

Add `pub mod tier3_test_harness` under a feature-gated `pub mod test_helpers` in `lib.rs` (if `test_helpers` doesn't already exist, create it). The harness:

```rust
#[cfg(feature = "test-fixtures")]
pub mod test_helpers {
    pub mod tier3_test_harness {
        use crate::community_voting_core::*;
        use crate::community_voting_log::VotingLog;
        use crate::community_voting_tier3::*;
        use crate::owner_state_types::*;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        pub struct Tier3TestHarness {
            pub state: Arc<std::sync::Mutex<crate::NodeState>>,
            pub poll_id_hex: String,
            pub community_id_hex: String,
        }

        impl Tier3TestHarness {
            pub async fn empty() -> Self { /* minimal NodeState; no polls */ ... }
            pub async fn with_tier1_poll() -> Self { /* Tier 1 PollCreate applied */ ... }
            pub async fn with_poll_in_sortition_stage() -> Self { /* kd=cr applied, no kd=ss yet */ ... }
            pub async fn with_poll_in_drafting_stage_and_self_in_mini_public() -> Self {
                /* kd=cr + kd=ss with self in primary + advance to Drafting */
                ...
            }

            pub async fn get_tier3_poll(&self, poll_id_hex: &str) -> Result<crate::Tier3PollExport, String> {
                let state = tauri::State::from(&*self.state);
                crate::voting_get_tier3_poll(state, poll_id_hex.to_string()).await
            }
        }
    }
}
```

Implementation details are intentionally elided — the implementer fills the harness in by mirroring the setup pattern in `community_voting_tier3_ipc_integration.rs`. The constraint is: each constructor must produce a `NodeState` with `voting_logs` populated such that the IPC under test can read it.

- [ ] **Step 3: Add the IPC**

Add to `src-tauri/src/lib.rs` near line 23789 (mirror `voting_get_tier2_proposal`):

```rust
/// ZEB-311: Tauri IPC — get the full state of a single Tier 3 poll by id.
///
/// Returns a `Tier3PollExport` projecting `Tier3PollState` into a
/// camelCased frontend shape. `my_*` fields are resolved from the
/// caller's `dm_self_owner` (None if unavailable → `my_role = Observer`).
///
/// Errors:
/// - "invalid poll_id hex" — hex decode failure
/// - "voting_get_tier3_poll: poll {id} not found" — no PollState matches
/// - "voting_get_tier3_poll: poll {id} is tier {t:?}, not Tier3" — tier mismatch
#[tauri::command(rename_all = "snake_case")]
async fn voting_get_tier3_poll(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    poll_id: String,
) -> Result<Tier3PollExport, String> {
    let pid_bytes: [u8; 32] = hex::decode(&poll_id)
        .map_err(|e| format!("invalid poll_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "poll_id must be 32 bytes (64 hex chars)".to_string())?;
    let pid = crate::community_voting_core::PollId(pid_bytes);

    let (self_owner_opt, voting_logs) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (g.dm_self_owner, std::sync::Arc::clone(&g.voting_logs))
    };

    let log_arcs: Vec<_> = {
        let map = voting_logs.lock().await;
        map.values().cloned().collect()
    };
    for log_arc in log_arcs.iter() {
        let g = log_arc.lock().await;
        if let Some(state) = g.polls.get(&pid) {
            if state.meta.tier != crate::community_voting_core::Tier::Sortition {
                return Err(format!(
                    "voting_get_tier3_poll: poll {} is tier {:?}, not Tier3",
                    poll_id, state.meta.tier
                ));
            }
            return build_tier3_export(state, self_owner_opt);
        }
    }
    Err(format!(
        "voting_get_tier3_poll: poll {} not found",
        poll_id
    ))
}

/// Pure projection: `PollState` (must be Tier 3) → `Tier3PollExport`.
fn build_tier3_export(
    state: &crate::community_voting_log::PollState,
    self_owner_opt: Option<crate::owner_state_types::OwnerAddr>,
) -> Result<Tier3PollExport, String> {
    let t3 = state
        .tier_state
        .as_tier3()
        .ok_or("build_tier3_export: poll is not Tier 3")?;

    let self_in_primary = self_owner_opt
        .as_ref()
        .map(|s| {
            t3.sortition_result
                .as_ref()
                .map(|r| r.primary.contains(s))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let self_in_backup = self_owner_opt
        .as_ref()
        .map(|s| {
            t3.sortition_result
                .as_ref()
                .map(|r| r.backup.contains(s))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let self_is_proposer = self_owner_opt
        .as_ref()
        .map(|s| s == &t3.meta.proposer)
        .unwrap_or(false);
    let my_role = if self_is_proposer {
        Tier3MyRole::Proposer
    } else if self_in_primary {
        Tier3MyRole::MiniPublic
    } else if self_in_backup {
        Tier3MyRole::Backup
    } else {
        Tier3MyRole::Observer
    };

    let my_drafting_approvals: Vec<String> = self_owner_opt
        .map(|s| {
            t3.candidates
                .iter()
                .filter(|c| c.approvals.contains(&s))
                .map(|c| hex::encode(c.event_hash))
                .collect()
        })
        .unwrap_or_default();

    let my_ratification_scores: Option<Vec<u8>> = self_owner_opt.and_then(|s| {
        t3.ratification_ballots
            .iter()
            .rev()
            .find(|b| b.voter == s)
            .map(|b| b.scores.clone())
    });

    let mini_public = t3
        .sortition_result
        .as_ref()
        .map(|r| r.primary.iter().map(|o| hex::encode(o.0)).collect())
        .unwrap_or_default();
    let backup_pool = t3
        .sortition_result
        .as_ref()
        .map(|r| r.backup.iter().map(|o| hex::encode(o.0)).collect())
        .unwrap_or_default();

    let declined = t3
        .declines
        .iter()
        .map(|(o, h)| (hex::encode(o.0), h.wall_ms as i128))
        .collect();

    let draft_candidates = t3
        .candidates
        .iter()
        .map(|c| DraftCandidateExport {
            event_hash: hex::encode(c.event_hash),
            text: c.text.clone(),
            proposer: c.proposer.as_ref().map(|p| hex::encode(p.0)),
            approval_count: c.approvals.len() as u32,
        })
        .collect();

    let ratification_candidates: Vec<RatificationCandidateExport> = t3
        .result
        .as_ref()
        .map(|r| {
            r.finalists
                .iter()
                .map(|f| RatificationCandidateExport {
                    event_hash: hex::encode(f.event_hash),
                    text: t3
                        .candidates
                        .iter()
                        .find(|c| c.event_hash == f.event_hash)
                        .map(|c| c.text.clone())
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();

    let winner_event_hash = t3
        .result
        .as_ref()
        .map(|r| hex::encode(r.winner.event_hash));
    let runner_up_event_hash = t3.result.as_ref().and_then(|r| {
        r.finalists
            .iter()
            .find(|f| f.event_hash != r.winner.event_hash)
            .map(|f| hex::encode(f.event_hash))
    });

    Ok(Tier3PollExport {
        poll_id: hex::encode(state.meta.poll_id.0),
        community_id: hex::encode(state.meta.community_id.0),
        proposal_text: t3.meta.config.proposal_text.clone(),
        proposer: hex::encode(t3.meta.proposer.0),
        stage: t3.stage.into(),
        poll_create_hlc_ms: t3.meta.poll_create_hlc.wall_ms as i128,
        sortition_size: t3.meta.config.sortition_size,
        deliberation_window_seconds: t3.meta.config.deliberation_window_seconds,
        drafting_window_seconds: t3.meta.config.drafting_window_seconds,
        ratification_window_seconds: t3.meta.config.ratification_window_seconds,
        incentive_mode: t3.meta.config.incentive_mode.clone(),
        mini_public,
        backup_pool,
        declined,
        draft_candidates,
        ratification_candidates,
        my_role,
        my_drafting_approvals,
        my_ratification_scores,
        winner_event_hash,
        runner_up_event_hash,
    })
}
```

- [ ] **Step 4: Register the IPC in both invoke_handler sites**

In both invoke_handler builders (lines 26591 and 26661), add `voting_get_tier3_poll` to the `tauri::generate_handler!` list. Keep the additions alphabetically near the other `voting_get_*` entries.

- [ ] **Step 5: Run the integration test**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(get_tier3_poll)'`
Expected: PASS (4 tests).

- [ ] **Step 6: Run clippy + fmt**

Run from `src-tauri/`: `cargo fmt --all` then `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: both PASS.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_voting_tier3_get_ipc_integration.rs
git commit -m "feat(zeb-311): add voting_get_tier3_poll IPC + Tier3PollExport projection

Pull-style getter that returns the full Tier 3 poll state in a
frontend-friendly shape, plus caller-derived my_role / my_*
fields. Tier mismatch + unknown-poll errors mirror Tier 2 pattern.
"
```

---

### Task 3: `voting_list_tier3_polls` IPC + integration test

**Files:**
- Modify: `src-tauri/src/lib.rs` (add IPC; register in both invoke_handler sites)
- Modify: `src-tauri/tests/community_voting_tier3_get_ipc_integration.rs` (extend with list tests)

Returns every Tier 3 poll in the community regardless of lifecycle, ordered by `poll_create_hlc_ms` descending (newest first). Finalized polls retain visibility per the Tier 3 use case ("constitutional decisions stay visible").

- [ ] **Step 1: Write the failing list tests (append to Task 2's file)**

```rust
#[tokio::test]
async fn list_tier3_polls_returns_polls_ordered_newest_first() {
    let h = Tier3TestHarness::with_two_polls_in_sortition_stage_at_different_hlcs().await;
    let summaries = h.list_tier3_polls(&h.community_id_hex).await.expect("ok");
    assert_eq!(summaries.len(), 2);
    assert!(summaries[0].poll_create_hlc_ms >= summaries[1].poll_create_hlc_ms);
}

#[tokio::test]
async fn list_tier3_polls_excludes_tier1_and_tier2_polls() {
    let h = Tier3TestHarness::with_one_tier3_and_one_tier1_in_same_community().await;
    let summaries = h.list_tier3_polls(&h.community_id_hex).await.expect("ok");
    assert_eq!(summaries.len(), 1);
}

#[tokio::test]
async fn list_tier3_polls_includes_finalized_with_winner_text() {
    let h = Tier3TestHarness::with_finalized_tier3_poll().await;
    let summaries = h.list_tier3_polls(&h.community_id_hex).await.expect("ok");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].stage, Tier3StageTag::Finalized);
    assert!(summaries[0].winner_text.is_some());
}

#[tokio::test]
async fn list_tier3_polls_returns_empty_for_unknown_community() {
    let h = Tier3TestHarness::empty().await;
    let summaries = h
        .list_tier3_polls(&"00".repeat(16))
        .await
        .expect("ok");
    assert!(summaries.is_empty());
}
```

Add the `list_tier3_polls` + harness constructors. Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_tier3_polls)'`. Expected: FAIL — IPC doesn't exist.

- [ ] **Step 2: Add the IPC + summary projection**

```rust
#[tauri::command(rename_all = "snake_case")]
async fn voting_list_tier3_polls(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<Tier3PollSummary>, String> {
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(cid_bytes);

    let voting_logs = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        std::sync::Arc::clone(&g.voting_logs)
    };

    let log_arc = {
        let map = voting_logs.lock().await;
        match map.get(&space_id) {
            Some(arc) => arc.clone(),
            None => return Ok(Vec::new()),
        }
    };
    let g = log_arc.lock().await;

    let mut summaries: Vec<Tier3PollSummary> = g
        .polls
        .values()
        .filter_map(|state| {
            let t3 = state.tier_state.as_tier3()?;
            let winner_text = t3
                .result
                .as_ref()
                .and_then(|r| {
                    t3.candidates
                        .iter()
                        .find(|c| c.event_hash == r.winner.event_hash)
                        .map(|c| c.text.clone())
                });
            Some(Tier3PollSummary {
                poll_id: hex::encode(state.meta.poll_id.0),
                community_id: hex::encode(state.meta.community_id.0),
                proposal_text: t3.meta.config.proposal_text.clone(),
                proposer: hex::encode(t3.meta.proposer.0),
                stage: t3.stage.into(),
                poll_create_hlc_ms: t3.meta.poll_create_hlc.wall_ms as i128,
                sortition_size: t3.meta.config.sortition_size,
                winner_text,
            })
        })
        .collect();
    summaries.sort_by_key(|s| std::cmp::Reverse(s.poll_create_hlc_ms));
    Ok(summaries)
}
```

Register in both invoke_handler sites.

- [ ] **Step 3: Run tests + clippy + fmt**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_tier3_polls)' && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: all PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_voting_tier3_get_ipc_integration.rs
git commit -m "feat(zeb-311): add voting_list_tier3_polls IPC

Returns Tier3PollSummary list ordered by PollCreate.hlc descending.
Includes Finalized polls with cached winner_text — constitutional
decisions stay visible in the tab indefinitely (no lifecycle filter).
"
```

---

### Task 4: TS types + adapter methods + adapter unit tests

**Files:**
- Modify: `src/lib/types/voting.ts` (add 6 new types/enums)
- Modify: `src/lib/voting-adapter.ts` (add 2 new methods)
- Modify: `src/lib/__tests__/voting-adapter.test.ts` (extend with 2 new tests)

- [ ] **Step 1: Add TS types in `src/lib/types/voting.ts`**

Append:

```typescript
/** ZEB-311 — Tier 3 stage tag. Wire-encoded as 2-char strings. */
export type Tier3Stage = 'so' | 'de' | 'dr' | 'ra' | 'fi' | 'fa';

/** ZEB-311 — caller's role for a Tier 3 poll. snake_case on the wire. */
export type Tier3MyRole = 'proposer' | 'mini_public' | 'backup' | 'observer';

/** ZEB-311 — one draft candidate visible to the frontend. */
export interface DraftCandidateExport {
  /** 64-char hex of the SHA-256 of the kd=dc event's signing bytes. */
  eventHash: string;
  text: string;
  /** 64-char hex OwnerAddr; null for the synthetic status_quo. */
  proposer: string | null;
  approvalCount: number;
}

/** ZEB-311 — one ratification candidate. */
export interface RatificationCandidateExport {
  eventHash: string;
  text: string;
}

/** ZEB-311 — full Tier 3 poll state for the UI. Returned by
 *  `adapter.getTier3Poll(pollId)`. */
export interface Tier3PollExport {
  pollId: string;
  communityId: string;
  proposalText: string;
  proposer: string;
  stage: Tier3Stage;
  /** HLC wall_ms projection. */
  pollCreateHlcMs: number;
  sortitionSize: number;
  deliberationWindowSeconds: number;
  draftingWindowSeconds: number;
  ratificationWindowSeconds: number;
  /** 1-char incentive_mode tag from validate_tier3_poll_config: 'a' | 'b' | 'c' | 'd'. */
  incentiveMode: string;
  miniPublic: string[];
  backupPool: string[];
  /** Tuples of (ownerHex, hlcMs). */
  declined: [string, number][];
  draftCandidates: DraftCandidateExport[];
  ratificationCandidates: RatificationCandidateExport[];
  myRole: Tier3MyRole;
  myDraftingApprovals: string[];
  myRatificationScores: number[] | null;
  winnerEventHash: string | null;
  runnerUpEventHash: string | null;
}

/** ZEB-311 — list-row shape. Lightweight; no candidate details. */
export interface Tier3PollSummary {
  pollId: string;
  communityId: string;
  proposalText: string;
  proposer: string;
  stage: Tier3Stage;
  pollCreateHlcMs: number;
  sortitionSize: number;
  winnerText: string | null;
}

/** Stage label for UI display. */
export function tier3StageLabel(s: Tier3Stage): string {
  switch (s) {
    case 'so': return 'Sortition';
    case 'de': return 'Deliberation';
    case 'dr': return 'Drafting';
    case 'ra': return 'Ratification';
    case 'fi': return 'Finalized';
    case 'fa': return 'Failed';
  }
}
```

- [ ] **Step 2: Add adapter methods (append in voting-adapter.ts near line 623)**

```typescript
async getTier3Poll(pollId: string): Promise<Tier3PollExport> {
  if (!this.tauriAdapter) throw new Error('VotingAdapter not connected');
  return this.tauriAdapter.invoke<Tier3PollExport>('voting_get_tier3_poll', {
    pollId,
  });
}

async listTier3Polls(communityId: string): Promise<Tier3PollSummary[]> {
  if (!this.tauriAdapter) throw new Error('VotingAdapter not connected');
  return this.tauriAdapter.invoke<Tier3PollSummary[]>('voting_list_tier3_polls', {
    communityId,
  });
}
```

Don't forget to add `Tier3PollExport, Tier3PollSummary` to the imports at the top of the file.

- [ ] **Step 3: Add adapter tests in `src/lib/__tests__/voting-adapter.test.ts`**

```typescript
describe('VotingAdapter.getTier3Poll', () => {
  it('invokes voting_get_tier3_poll with the camelCased pollId', async () => {
    const mock = createMockAdapter();
    const a = new VotingAdapter();
    await a.connectAdapter(mock);
    mock.invoke.mockResolvedValueOnce({ pollId: 'aa', stage: 'so' });
    await a.getTier3Poll('aa');
    expect(mock.invoke).toHaveBeenCalledWith('voting_get_tier3_poll', { pollId: 'aa' });
  });
});

describe('VotingAdapter.listTier3Polls', () => {
  it('invokes voting_list_tier3_polls with the camelCased communityId', async () => {
    const mock = createMockAdapter();
    const a = new VotingAdapter();
    await a.connectAdapter(mock);
    mock.invoke.mockResolvedValueOnce([]);
    await a.listTier3Polls('11');
    expect(mock.invoke).toHaveBeenCalledWith('voting_list_tier3_polls', { communityId: '11' });
  });
});
```

If `createMockAdapter()` isn't already exported from the test file, mirror however the existing adapter tests construct their mock.

- [ ] **Step 4: Run frontend gates**

Run: `npx tsc --noEmit && npx vitest run -t "Tier3"`
Expected: type-check PASS, both adapter tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/types/voting.ts src/lib/voting-adapter.ts src/lib/__tests__/voting-adapter.test.ts
git commit -m "feat(zeb-311): add Tier3PollExport TS types + adapter getters

Mirrors the new Rust IPCs. adapter.getTier3Poll(pollId) +
adapter.listTier3Polls(communityId). Tier3Stage + Tier3MyRole as
type-aliased string unions matching the wire-encoded tags.
"
```

---

### Task 5: `Tier3LifecycleStatus.svelte` + Vitest

**Files:**
- Create: `src/lib/components/Tier3LifecycleStatus.svelte`
- Create: `src/lib/components/__tests__/Tier3LifecycleStatus.test.ts`

Compact stage indicator: shows current stage in a 1→2→3→4→F progression and a countdown to the next stage transition. Renders SortitionFailed terminal state distinctly.

- [ ] **Step 1: Write the failing test**

```typescript
import { render } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import Tier3LifecycleStatus from '../Tier3LifecycleStatus.svelte';
import type { Tier3PollSummary } from '$lib/types/voting';

const baseSummary: Tier3PollSummary = {
  pollId: 'aa'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Amend §3',
  proposer: '22'.repeat(32),
  stage: 'so',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  winnerText: null,
};

describe('Tier3LifecycleStatus', () => {
  it('renders all four stage chips with current stage highlighted', () => {
    const { container, getByText } = render(Tier3LifecycleStatus, {
      props: { summary: { ...baseSummary, stage: 'dr' } },
    });
    expect(getByText('Sortition')).toBeTruthy();
    expect(getByText('Deliberation')).toBeTruthy();
    expect(getByText('Drafting')).toBeTruthy();
    expect(getByText('Ratification')).toBeTruthy();
    const current = container.querySelector('.stage-chip.current');
    expect(current?.textContent).toContain('Drafting');
  });

  it('renders a failed badge when stage is fa', () => {
    const { getByText } = render(Tier3LifecycleStatus, {
      props: { summary: { ...baseSummary, stage: 'fa' } },
    });
    expect(getByText(/sortition failed/i)).toBeTruthy();
  });

  it('renders the winner text when stage is fi', () => {
    const { getByText } = render(Tier3LifecycleStatus, {
      props: { summary: { ...baseSummary, stage: 'fi', winnerText: 'Charter amended' } },
    });
    expect(getByText('Charter amended')).toBeTruthy();
  });
});
```

Run: `npx vitest run Tier3LifecycleStatus`
Expected: FAIL — component doesn't exist.

- [ ] **Step 2: Implement the component**

```svelte
<script lang="ts">
  /**
   * ZEB-311 — Tier 3 poll stage indicator.
   *
   * Renders a 4-chip progression Sortition → Deliberation → Drafting →
   * Ratification with the current stage highlighted, plus a countdown
   * to the next stage transition based on PollCreate.hlc + cumulative
   * window durations. SortitionFailed shows a single red badge
   * (proposer-initiated retry button is mounted by the parent panel,
   * not here — this component is presentation-only).
   *
   * Per ZEB-287 R4: every $props field is destructured below.
   */
  import { tier3StageLabel, type Tier3PollSummary } from '../types/voting';

  let { summary }: { summary: Tier3PollSummary } = $props();

  const stages = ['so', 'de', 'dr', 'ra'] as const;

  // Cumulative ms-since-PollCreate at the END of each stage (= START of next).
  // Stage 'so' ends when kd=ss applies — that's not deadline-driven, so we
  // show no countdown for Sortition. Subsequent stages all have wall-clock
  // deadlines via the kd=cl auto-mint at PollCreate.hlc + sum(windows so far).
  // (Phase 4a-main does not surface kd=ss arrival ETA — only stage chips.)
</script>

{#if summary.stage === 'fa'}
  <div class="failed-badge">⚠ Sortition failed (backup pool exhausted)</div>
{:else if summary.stage === 'fi'}
  <div class="finalized-badge">
    <span class="checkmark">✓</span>
    <span class="winner">{summary.winnerText ?? 'Finalized'}</span>
  </div>
{:else}
  <ol class="stage-chips" aria-label="Tier 3 poll stage progression">
    {#each stages as s}
      <li
        class="stage-chip"
        class:current={summary.stage === s}
        class:past={stages.indexOf(s) < stages.indexOf(summary.stage as typeof stages[number])}
      >
        {tier3StageLabel(s)}
      </li>
    {/each}
  </ol>
{/if}

<style>
  .stage-chips {
    display: flex;
    list-style: none;
    gap: 0.25rem;
    padding: 0;
    margin: 0;
    font-size: 0.85rem;
  }
  .stage-chip {
    padding: 0.25rem 0.6rem;
    border-radius: 999px;
    background: var(--chip-bg, #2a2c34);
    color: var(--chip-fg, #c8c9d1);
    border: 1px solid transparent;
  }
  .stage-chip.past {
    color: #8a8c95;
  }
  .stage-chip.current {
    background: var(--accent, #4a9eff);
    color: #fff;
    border-color: var(--accent, #4a9eff);
  }
  .failed-badge {
    color: #d93838;
    font-weight: 600;
  }
  .finalized-badge {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    color: var(--success, #4ad97a);
  }
</style>
```

- [ ] **Step 3: Run the test**

Run: `npx vitest run Tier3LifecycleStatus && npx tsc --noEmit`
Expected: all 3 component tests PASS, type check clean.

- [ ] **Step 4: Commit**

```bash
git add src/lib/components/Tier3LifecycleStatus.svelte src/lib/components/__tests__/Tier3LifecycleStatus.test.ts
git commit -m "feat(zeb-311): Tier3LifecycleStatus stage indicator

Compact 4-chip progression with current-stage highlight + distinct
treatment for Failed / Finalized terminal states. Pure presentation
component; parent panel owns retry affordance for Failed polls.
"
```

---

### Task 6: `Tier3ProposalPanel.svelte` + Vitest

**Files:**
- Create: `src/lib/components/Tier3ProposalPanel.svelte`
- Create: `src/lib/components/__tests__/Tier3ProposalPanel.test.ts`

Parent container for the Tier 3 tab. Top section is the create form (proposal text + sortition size + 3 window sliders, each paired with a number input per `feedback_slider_pair_with_number_input`). Submit triggers `adapter.createTier3Proposal`. Below the form, lists all Tier 3 polls in this community via `adapter.listTier3Polls`. Each row is `Tier3LifecycleStatus` + click-to-expand into a detail pane that conditionally mounts SortitionRevealView / MiniPublicParticipationToggle / DraftingPanel / StarRatificationBallot based on stage + my_role.

Subscribes to all 5 Tier 3 Tauri events to refetch the list/detail on changes. Click-confirm severity tier on the "Create proposal" button (per `feedback_severe_action_confirmation` — severe but reversible). Retry button for Failed polls (proposer-only) pre-fills the form with the failed poll's fields.

- [ ] **Step 1: Write the failing test**

```typescript
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import Tier3ProposalPanel from '../Tier3ProposalPanel.svelte';
import { VotingAdapter } from '$lib/voting-adapter';
import type { Tier3PollSummary } from '$lib/types/voting';

function createAdapterMock(summaries: Tier3PollSummary[] = []) {
  const adapter = new VotingAdapter();
  vi.spyOn(adapter, 'listTier3Polls').mockResolvedValue(summaries);
  vi.spyOn(adapter, 'createTier3Proposal').mockResolvedValue('pollid'.padEnd(64, '0'));
  vi.spyOn(adapter, 'subscribeTier3PollCreated').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3SortitionComplete').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3DraftingOpen').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3RatificationOpen').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3Finalized').mockReturnValue(() => {});
  return adapter;
}

describe('Tier3ProposalPanel', () => {
  it('lists existing Tier 3 polls on mount', async () => {
    const adapter = createAdapterMock([
      {
        pollId: 'aa'.repeat(32),
        communityId: '11'.repeat(16),
        proposalText: 'Existing proposal',
        proposer: '22'.repeat(32),
        stage: 'dr',
        pollCreateHlcMs: 1_700_000_000_000,
        sortitionSize: 100,
        winnerText: null,
      },
    ]);
    const { findByText } = render(Tier3ProposalPanel, {
      props: { communityId: '11'.repeat(16), adapter, myAddr: '22'.repeat(32) },
    });
    expect(await findByText('Existing proposal')).toBeTruthy();
  });

  it('opens click-confirm before invoking createTier3Proposal', async () => {
    const adapter = createAdapterMock();
    const { getByLabelText, findByText } = render(Tier3ProposalPanel, {
      props: { communityId: '11'.repeat(16), adapter, myAddr: '22'.repeat(32) },
    });
    await fireEvent.input(getByLabelText(/Proposal text/i), { target: { value: 'New' } });
    await fireEvent.click(await findByText(/Create proposal/i));
    // Confirm modal appears
    expect(await findByText(/Confirm new Tier 3 proposal/i)).toBeTruthy();
    // Not yet invoked
    expect(adapter.createTier3Proposal).not.toHaveBeenCalled();
    // Click the confirm button
    await fireEvent.click(await findByText(/^Confirm$/i));
    await waitFor(() => expect(adapter.createTier3Proposal).toHaveBeenCalledTimes(1));
  });

  it('refetches the list when subscribeTier3Finalized fires', async () => {
    let finalizedHandler: (() => void) | null = null;
    const adapter = createAdapterMock();
    vi.spyOn(adapter, 'subscribeTier3Finalized').mockImplementation((h) => {
      finalizedHandler = h as () => void;
      return () => {};
    });
    render(Tier3ProposalPanel, {
      props: { communityId: '11'.repeat(16), adapter, myAddr: '22'.repeat(32) },
    });
    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(1));
    finalizedHandler!();
    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(2));
  });
});
```

Run: `npx vitest run Tier3ProposalPanel`
Expected: FAIL — component doesn't exist.

- [ ] **Step 2: Implement the component**

```svelte
<script lang="ts">
  /**
   * ZEB-311 — Tier 3 governance panel: create form + poll list +
   * stage-specific detail view.
   *
   * Sections:
   *   1. Create form (proposal text + sortition_size + 3 paired
   *      slider/number-input window controls). Submit goes through
   *      a click-confirm per `feedback_severe_action_confirmation`.
   *   2. List of existing Tier 3 polls (via adapter.listTier3Polls).
   *      Each row renders Tier3LifecycleStatus + click-to-expand.
   *   3. Expanded detail pane: dispatches on poll.stage + poll.myRole
   *      to mount SortitionRevealView / MiniPublicParticipationToggle /
   *      DraftingPanel / StarRatificationBallot.
   *
   * Refetches list/detail when ANY of the 5 Tier 3 Tauri events fire.
   *
   * Retry: a Failed poll where myRole = 'proposer' shows a "Retry"
   * button that pre-fills the create form with the failed poll's
   * fields. No retry_of linkage — fresh proposal per user direction.
   *
   * Per ZEB-287 R4: every $props field destructured below.
   * Per Tauri error-extraction memory: e instanceof Error ? e.message : String(e).
   */
  import { onDestroy, onMount } from 'svelte';
  import type {
    Tier3PollExport,
    Tier3PollSummary,
  } from '../types/voting';
  import { tier3StageLabel } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';
  import Tier3LifecycleStatus from './Tier3LifecycleStatus.svelte';
  import SortitionRevealView from './SortitionRevealView.svelte';
  import MiniPublicParticipationToggle from './MiniPublicParticipationToggle.svelte';
  import DraftingPanel from './DraftingPanel.svelte';
  import StarRatificationBallot from './StarRatificationBallot.svelte';

  let {
    communityId,
    adapter,
    myAddr,
  }: {
    communityId: string;
    adapter: VotingAdapter;
    myAddr: string;
  } = $props();

  // Create-form state
  let proposalText = $state('');
  let sortitionSize = $state(100);
  let deliberationWindowSeconds = $state(1_209_600); // 14d
  let draftingWindowSeconds = $state(604_800);       // 7d
  let ratificationWindowSeconds = $state(1_209_600); // 14d
  let incentiveMode = $state<'a' | 'b' | 'c' | 'd'>('d');
  let confirmingCreate = $state(false);
  let createError = $state<string | null>(null);

  // List + selection state
  let summaries = $state<Tier3PollSummary[]>([]);
  let listError = $state<string | null>(null);
  let selectedPollId = $state<string | null>(null);
  let selectedDetail = $state<Tier3PollExport | null>(null);
  let detailError = $state<string | null>(null);

  let unsubscribers: Array<() => void> = [];

  async function loadSummaries() {
    try {
      summaries = await adapter.listTier3Polls(communityId);
      listError = null;
    } catch (e) {
      listError = e instanceof Error ? e.message : String(e);
    }
  }

  async function loadDetail(pollId: string) {
    try {
      selectedDetail = await adapter.getTier3Poll(pollId);
      detailError = null;
    } catch (e) {
      detailError = e instanceof Error ? e.message : String(e);
    }
  }

  function select(pollId: string) {
    selectedPollId = pollId;
    loadDetail(pollId);
  }

  function refetchSelected() {
    if (selectedPollId) loadDetail(selectedPollId);
  }

  async function submitCreate() {
    try {
      await adapter.createTier3Proposal({
        communityId,
        proposalText,
        sortitionSize,
        deliberationWindowSeconds,
        draftingWindowSeconds,
        ratificationWindowSeconds,
        incentiveMode,
      });
      proposalText = '';
      confirmingCreate = false;
      createError = null;
      await loadSummaries();
    } catch (e) {
      createError = e instanceof Error ? e.message : String(e);
      confirmingCreate = false;
    }
  }

  function retryFailed(failed: Tier3PollSummary) {
    proposalText = failed.proposalText;
    sortitionSize = failed.sortitionSize;
    // Keep current window/incentive — the proposer can tweak before resubmitting.
    confirmingCreate = false;
    // Scroll to top so the create form is visible.
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  onMount(() => {
    loadSummaries();
    unsubscribers.push(adapter.subscribeTier3PollCreated(() => loadSummaries()));
    unsubscribers.push(
      adapter.subscribeTier3SortitionComplete(() => {
        loadSummaries();
        refetchSelected();
      }),
    );
    unsubscribers.push(
      adapter.subscribeTier3DraftingOpen(() => {
        loadSummaries();
        refetchSelected();
      }),
    );
    unsubscribers.push(
      adapter.subscribeTier3RatificationOpen(() => {
        loadSummaries();
        refetchSelected();
      }),
    );
    unsubscribers.push(
      adapter.subscribeTier3Finalized(() => {
        loadSummaries();
        refetchSelected();
      }),
    );
  });

  onDestroy(() => {
    for (const u of unsubscribers) u();
    unsubscribers = [];
  });
</script>

<section class="tier3-panel">
  <h2>Constitutional Decisions (Tier 3)</h2>

  <form
    class="create-form"
    onsubmit={(e) => {
      e.preventDefault();
      if (proposalText.trim()) confirmingCreate = true;
    }}
  >
    <label>
      <span>Proposal text</span>
      <textarea
        bind:value={proposalText}
        rows="3"
        maxlength="2000"
        placeholder="Amend charter §3: require 2/3 supermajority for moderator dismissals"
        required
      ></textarea>
    </label>

    <div class="paired-input">
      <label for="sortition-size">Sortition size</label>
      <input
        id="sortition-size"
        type="range"
        min="20"
        max="300"
        step="1"
        bind:value={sortitionSize}
      />
      <input type="number" min="20" max="300" bind:value={sortitionSize} />
    </div>

    <div class="paired-input">
      <label for="deliberation-window">Deliberation window (days)</label>
      <input
        id="deliberation-window"
        type="range"
        min="1"
        max="30"
        step="1"
        value={Math.round(deliberationWindowSeconds / 86_400)}
        oninput={(e) => {
          deliberationWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
      <input
        type="number"
        min="1"
        max="30"
        value={Math.round(deliberationWindowSeconds / 86_400)}
        oninput={(e) => {
          deliberationWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
    </div>

    <div class="paired-input">
      <label for="drafting-window">Drafting window (days)</label>
      <input
        id="drafting-window"
        type="range"
        min="1"
        max="30"
        step="1"
        value={Math.round(draftingWindowSeconds / 86_400)}
        oninput={(e) => {
          draftingWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
      <input
        type="number"
        min="1"
        max="30"
        value={Math.round(draftingWindowSeconds / 86_400)}
        oninput={(e) => {
          draftingWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
    </div>

    <div class="paired-input">
      <label for="ratification-window">Ratification window (days)</label>
      <input
        id="ratification-window"
        type="range"
        min="1"
        max="30"
        step="1"
        value={Math.round(ratificationWindowSeconds / 86_400)}
        oninput={(e) => {
          ratificationWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
      <input
        type="number"
        min="1"
        max="30"
        value={Math.round(ratificationWindowSeconds / 86_400)}
        oninput={(e) => {
          ratificationWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
    </div>

    <label>
      <span>Incentive mode</span>
      <select bind:value={incentiveMode}>
        <option value="a">a — SoftExpectation</option>
        <option value="b">b — AutoPowerBoost</option>
        <option value="c">c — CompulsoryWithOptOut</option>
        <option value="d">d — DeclineWithBackupPool (default)</option>
      </select>
    </label>

    <button type="submit" disabled={!proposalText.trim()}>Create proposal</button>
    {#if createError}
      <p class="error">{createError}</p>
    {/if}
  </form>

  {#if confirmingCreate}
    <div class="confirm-modal" role="dialog" aria-modal="true" aria-label="Confirm new Tier 3 proposal">
      <p>Confirm new Tier 3 proposal</p>
      <p class="confirm-summary">
        “{proposalText.slice(0, 120)}{proposalText.length > 120 ? '…' : ''}”
      </p>
      <div class="confirm-actions">
        <button type="button" onclick={() => (confirmingCreate = false)}>Cancel</button>
        <button type="button" onclick={submitCreate}>Confirm</button>
      </div>
    </div>
  {/if}

  <h3 class="list-heading">Existing proposals</h3>
  {#if listError}
    <p class="error">{listError}</p>
  {/if}
  {#if summaries.length === 0}
    <p class="empty">No constitutional decisions in this community yet.</p>
  {:else}
    <ul class="poll-list">
      {#each summaries as s (s.pollId)}
        <li class="poll-row">
          <button
            type="button"
            class="poll-row-button"
            onclick={() => select(s.pollId)}
            class:selected={selectedPollId === s.pollId}
          >
            <span class="proposal-text">{s.proposalText}</span>
            <Tier3LifecycleStatus summary={s} />
          </button>
          {#if s.stage === 'fa' && s.proposer === myAddr}
            <button type="button" class="retry-btn" onclick={() => retryFailed(s)}>
              Retry
            </button>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}

  {#if selectedDetail}
    <section class="detail-pane">
      <h4>{selectedDetail.proposalText}</h4>
      <p class="stage-label">{tier3StageLabel(selectedDetail.stage)}</p>
      {#if detailError}
        <p class="error">{detailError}</p>
      {/if}

      {#if selectedDetail.stage === 'so'}
        <p>Awaiting sortition draw. The D-FROST committee must produce the VRF beacon before the mini-public is selected.</p>
      {:else if selectedDetail.stage === 'de' || selectedDetail.stage === 'dr' || selectedDetail.stage === 'ra' || selectedDetail.stage === 'fi'}
        <SortitionRevealView detail={selectedDetail} {myAddr} />
        {#if selectedDetail.myRole === 'mini_public' && (selectedDetail.stage === 'de' || selectedDetail.stage === 'dr')}
          <MiniPublicParticipationToggle detail={selectedDetail} {adapter} onDecline={refetchSelected} />
        {/if}
        {#if selectedDetail.stage === 'dr'}
          <DraftingPanel detail={selectedDetail} {adapter} {myAddr} onChange={refetchSelected} />
        {/if}
        {#if selectedDetail.stage === 'ra' || selectedDetail.stage === 'fi'}
          <StarRatificationBallot detail={selectedDetail} {adapter} onCast={refetchSelected} />
        {/if}
      {:else if selectedDetail.stage === 'fa'}
        <p class="failed-detail">
          Sortition failed — the backup pool was exhausted before the mini-public could be assembled.
        </p>
      {/if}
    </section>
  {/if}
</section>

<style>
  .tier3-panel { padding: 1rem; max-width: 880px; margin: 0 auto; }
  .create-form {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    padding: 1rem;
    background: var(--panel-bg, #1a1c24);
    border-radius: 8px;
    margin-bottom: 1.5rem;
  }
  .paired-input {
    display: grid;
    grid-template-columns: 1fr 3fr 80px;
    gap: 0.5rem;
    align-items: center;
  }
  textarea, select, input[type="number"] {
    background: var(--input-bg, #0e0f15);
    color: inherit;
    border: 1px solid #2a2c34;
    border-radius: 4px;
    padding: 0.4rem 0.5rem;
    font: inherit;
  }
  button[type="submit"] {
    align-self: flex-start;
    background: var(--accent, #4a9eff);
    color: #fff;
    border: 0;
    padding: 0.5rem 1rem;
    border-radius: 4px;
    cursor: pointer;
  }
  button[type="submit"]:disabled { opacity: 0.5; cursor: not-allowed; }
  .confirm-modal {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: grid;
    place-items: center;
    z-index: 100;
  }
  .confirm-modal > * {
    background: var(--panel-bg, #1a1c24);
    padding: 1rem 1.5rem;
    border-radius: 8px;
    margin: 0.25rem;
  }
  .confirm-actions { display: flex; gap: 0.5rem; }
  .confirm-actions button:last-child {
    background: var(--accent, #4a9eff);
    color: #fff;
  }
  .list-heading { margin-top: 1.5rem; font-size: 1rem; }
  .poll-list { list-style: none; padding: 0; }
  .poll-row { display: flex; gap: 0.5rem; align-items: center; padding: 0.5rem 0; border-bottom: 1px solid #2a2c34; }
  .poll-row-button {
    flex: 1;
    background: transparent;
    border: 0;
    color: inherit;
    text-align: left;
    cursor: pointer;
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.75rem;
    padding: 0.25rem 0.5rem;
    border-radius: 4px;
  }
  .poll-row-button.selected { background: rgba(74, 158, 255, 0.1); }
  .proposal-text { font-weight: 500; }
  .retry-btn {
    background: transparent;
    color: var(--accent, #4a9eff);
    border: 1px solid var(--accent, #4a9eff);
    padding: 0.25rem 0.6rem;
    border-radius: 4px;
    cursor: pointer;
  }
  .detail-pane {
    margin-top: 1.5rem;
    padding: 1rem;
    background: var(--panel-bg, #1a1c24);
    border-radius: 8px;
  }
  .stage-label { color: #8a8c95; font-size: 0.85rem; margin-top: -0.25rem; }
  .error { color: #d93838; }
  .empty { color: #8a8c95; }
  .failed-detail { color: #d93838; }
</style>
```

- [ ] **Step 3: Run the test + type check**

Run: `npx vitest run Tier3ProposalPanel && npx tsc --noEmit`
Expected: all 3 component tests PASS, type check clean.

NOTE: this test will fail until Tasks 7-10 (the 4 child components) are implemented because of the imports. Solution: stub the 4 child components as empty Svelte 5 components in this task (each just `<script lang="ts">let { ...props } = $props();</script>`), then implement each child in its own task. This is the prevailing pattern for top-down testing — the parent's shape is locked first; children fill in.

Actually a cleaner alternative: write the imports + sub-component invocations but mark them with `{#if false}` so they don't render in the panel's tests. Then enable each as it's implemented. Avoids stub churn.

Pick whichever feels cleaner. Either way, the panel's tests above should pass on their own without requiring the children to be functional yet.

- [ ] **Step 4: Commit**

```bash
git add src/lib/components/Tier3ProposalPanel.svelte src/lib/components/__tests__/Tier3ProposalPanel.test.ts
git commit -m "feat(zeb-311): Tier3ProposalPanel — create form + poll list + detail dispatch

Parent container for the Tier 3 tab. Click-confirm severity tier on
create (per feedback_severe_action_confirmation). Paired slider +
number-input on sortition_size + 3 windows (per feedback_slider_pair_with_number_input).
Subscribes to all 5 Tier 3 Tauri events; refetches on each. Failed
polls get a proposer-only Retry button that pre-fills the form.
"
```

---

### Task 7: `SortitionRevealView.svelte` + Vitest

**Files:**
- Create: `src/lib/components/SortitionRevealView.svelte`
- Create: `src/lib/components/__tests__/SortitionRevealView.test.ts`

Renders the kd=ss primary mini-public + backup pool. Highlights the current user if they're in primary or backup. Shows declined members.

- [ ] **Step 1: Write the failing test**

```typescript
import { render } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import SortitionRevealView from '../SortitionRevealView.svelte';
import type { Tier3PollExport } from '$lib/types/voting';

const baseDetail: Tier3PollExport = {
  pollId: 'aa'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Amend §3',
  proposer: 'pp'.repeat(32),
  stage: 'de',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  deliberationWindowSeconds: 1_209_600,
  draftingWindowSeconds: 604_800,
  ratificationWindowSeconds: 1_209_600,
  incentiveMode: 'd',
  miniPublic: ['aa'.repeat(32), 'bb'.repeat(32)],
  backupPool: ['cc'.repeat(32)],
  declined: [],
  draftCandidates: [],
  ratificationCandidates: [],
  myRole: 'observer',
  myDraftingApprovals: [],
  myRatificationScores: null,
  winnerEventHash: null,
  runnerUpEventHash: null,
};

describe('SortitionRevealView', () => {
  it('renders mini-public + backup with counts', () => {
    const { getByText } = render(SortitionRevealView, {
      props: { detail: baseDetail, myAddr: 'zz'.repeat(32) },
    });
    expect(getByText(/Mini-public \(2\)/)).toBeTruthy();
    expect(getByText(/Backup pool \(1\)/)).toBeTruthy();
  });

  it('highlights "You were selected!" when self in primary', () => {
    const { getByText } = render(SortitionRevealView, {
      props: { detail: { ...baseDetail, myRole: 'mini_public' }, myAddr: 'aa'.repeat(32) },
    });
    expect(getByText(/You were selected/i)).toBeTruthy();
  });

  it('shows declined members when present', () => {
    const { getByText } = render(SortitionRevealView, {
      props: {
        detail: { ...baseDetail, declined: [['bb'.repeat(32), 1_700_000_500_000]] },
        myAddr: 'zz'.repeat(32),
      },
    });
    expect(getByText(/Declined \(1\)/)).toBeTruthy();
  });
});
```

Run: `npx vitest run SortitionRevealView`
Expected: FAIL — component doesn't exist.

- [ ] **Step 2: Implement**

```svelte
<script lang="ts">
  /**
   * ZEB-311 — Renders the sortition draw result: primary mini-public
   * + backup pool + declines. Highlights the caller's membership if
   * any. Renders OwnerAddr as a short hex (first 8 + last 4 chars).
   *
   * Per ZEB-287 R4: every $props field destructured below.
   */
  import type { Tier3PollExport } from '../types/voting';

  let {
    detail,
    myAddr,
  }: {
    detail: Tier3PollExport;
    myAddr: string;
  } = $props();

  function shortAddr(hex: string): string {
    return hex.length > 16 ? `${hex.slice(0, 8)}…${hex.slice(-4)}` : hex;
  }

  let amInPrimary = $derived(detail.miniPublic.includes(myAddr));
  let amInBackup = $derived(detail.backupPool.includes(myAddr));
  let declinedSet = $derived(new Set(detail.declined.map(([owner]) => owner)));
</script>

{#if amInPrimary}
  <p class="selected-banner">🎯 You were selected for the mini-public!</p>
{:else if amInBackup}
  <p class="backup-banner">You're in the backup pool — you'll be promoted if a primary member declines.</p>
{/if}

<section class="sortition-reveal">
  <h5>Mini-public ({detail.miniPublic.length})</h5>
  <ul class="roster">
    {#each detail.miniPublic as addr (addr)}
      <li class:declined={declinedSet.has(addr)} class:self={addr === myAddr}>
        <code>{shortAddr(addr)}</code>
        {#if declinedSet.has(addr)}<span class="tag">declined</span>{/if}
        {#if addr === myAddr}<span class="tag">you</span>{/if}
      </li>
    {/each}
  </ul>

  {#if detail.backupPool.length > 0}
    <h5>Backup pool ({detail.backupPool.length})</h5>
    <ul class="roster">
      {#each detail.backupPool as addr (addr)}
        <li class:self={addr === myAddr}>
          <code>{shortAddr(addr)}</code>
          {#if addr === myAddr}<span class="tag">you</span>{/if}
        </li>
      {/each}
    </ul>
  {/if}

  {#if detail.declined.length > 0}
    <h5>Declined ({detail.declined.length})</h5>
  {/if}
</section>

<style>
  .selected-banner {
    background: var(--accent, #4a9eff);
    color: #fff;
    padding: 0.5rem 0.75rem;
    border-radius: 4px;
    font-weight: 500;
  }
  .backup-banner {
    background: rgba(74, 158, 255, 0.15);
    color: var(--accent, #4a9eff);
    padding: 0.5rem 0.75rem;
    border-radius: 4px;
  }
  .roster {
    list-style: none;
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    gap: 0.25rem 0.5rem;
  }
  .roster li {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    font-size: 0.85rem;
    padding: 0.15rem 0.4rem;
    background: var(--input-bg, #0e0f15);
    border-radius: 3px;
  }
  .roster li.declined code { text-decoration: line-through; opacity: 0.6; }
  .roster li.self { background: rgba(74, 158, 255, 0.1); }
  .tag {
    font-size: 0.7rem;
    color: #8a8c95;
    background: #2a2c34;
    padding: 0 0.35rem;
    border-radius: 2px;
  }
  h5 { margin: 1rem 0 0.25rem; font-size: 0.9rem; }
</style>
```

- [ ] **Step 3: Run + commit**

Run: `npx vitest run SortitionRevealView && npx tsc --noEmit`
Expected: PASS.

```bash
git add src/lib/components/SortitionRevealView.svelte src/lib/components/__tests__/SortitionRevealView.test.ts
git commit -m "feat(zeb-311): SortitionRevealView — mini-public + backup + declines

Highlights self-in-primary / self-in-backup with banner. Renders
declined members with strikethrough. Short-hex addresses
(first 8 + last 4) per existing addr-display conventions.
"
```

---

### Task 8: `MiniPublicParticipationToggle.svelte` + Vitest

**Files:**
- Create: `src/lib/components/MiniPublicParticipationToggle.svelte`
- Create: `src/lib/components/__tests__/MiniPublicParticipationToggle.test.ts`

Visible only when the caller's `myRole === 'mini_public'` and stage ∈ {Deliberation, Drafting}. Renders a Decline button → `adapter.declineSortition(pollId, reason?)`. Shows already-declined status if applicable.

- [ ] **Step 1: Write the failing test**

```typescript
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import MiniPublicParticipationToggle from '../MiniPublicParticipationToggle.svelte';
import { VotingAdapter } from '$lib/voting-adapter';
import type { Tier3PollExport } from '$lib/types/voting';

const detail: Tier3PollExport = {
  pollId: 'aa'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Amend §3',
  proposer: 'pp'.repeat(32),
  stage: 'de',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  deliberationWindowSeconds: 1_209_600,
  draftingWindowSeconds: 604_800,
  ratificationWindowSeconds: 1_209_600,
  incentiveMode: 'd',
  miniPublic: ['mm'.repeat(32)],
  backupPool: [],
  declined: [],
  draftCandidates: [],
  ratificationCandidates: [],
  myRole: 'mini_public',
  myDraftingApprovals: [],
  myRatificationScores: null,
  winnerEventHash: null,
  runnerUpEventHash: null,
};

describe('MiniPublicParticipationToggle', () => {
  it('renders Decline button when not yet declined', () => {
    const adapter = new VotingAdapter();
    const { getByText } = render(MiniPublicParticipationToggle, {
      props: { detail, adapter, onDecline: () => {} },
    });
    expect(getByText(/Decline mini-public role/i)).toBeTruthy();
  });

  it('invokes declineSortition with the pollId on click', async () => {
    const adapter = new VotingAdapter();
    vi.spyOn(adapter, 'declineSortition').mockResolvedValue();
    const onDecline = vi.fn();
    const { getByText } = render(MiniPublicParticipationToggle, {
      props: { detail, adapter, onDecline },
    });
    await fireEvent.click(getByText(/Decline mini-public role/i));
    await waitFor(() => expect(adapter.declineSortition).toHaveBeenCalledWith(detail.pollId, undefined));
    expect(onDecline).toHaveBeenCalled();
  });

  it('shows already-declined message when self is in declined set', () => {
    const declinedDetail = {
      ...detail,
      declined: [['mm'.repeat(32), 1_700_000_500_000]] as [string, number][],
    };
    const adapter = new VotingAdapter();
    const { getByText } = render(MiniPublicParticipationToggle, {
      props: { detail: declinedDetail, adapter, onDecline: () => {} },
    });
    expect(getByText(/You declined this role/i)).toBeTruthy();
  });
});
```

Run: `npx vitest run MiniPublicParticipationToggle`
Expected: FAIL.

- [ ] **Step 2: Implement**

```svelte
<script lang="ts">
  /**
   * ZEB-311 — Mini-public decline affordance.
   *
   * Shown only when myRole === 'mini_public' AND stage is
   * Deliberation or Drafting (the parent panel gates rendering).
   * One-shot decline; once declined, the button is hidden and a
   * confirmation message takes its place.
   *
   * Per ZEB-287 R4: every $props field destructured below.
   * Per Tauri error-extraction memory.
   */
  import type { Tier3PollExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';

  let {
    detail,
    adapter,
    onDecline,
  }: {
    detail: Tier3PollExport;
    adapter: VotingAdapter;
    onDecline: () => void;
  } = $props();

  let pending = $state(false);
  let error = $state<string | null>(null);

  let alreadyDeclined = $derived(
    detail.declined.some(([owner]) => owner === detail.miniPublic.find((p) => p === owner && detail.myRole === 'mini_public') && detail.myRole === 'mini_public'),
  );
  // Simpler: am I in the declined set? Use myDraftingApprovals as a proxy?
  // No — better: server tells us via declined directly. But we don't have a
  // myAddr prop here. Use the fact that we are in detail.miniPublic AND
  // myRole === 'mini_public' to confirm membership, then check declined
  // against the same membership-only entry.
  //
  // For simplicity, just check whether detail.miniPublic and detail.declined
  // share any owner. (Phase 4a-main MUST show declined regardless of which
  // member declined — and the only mini-public member viewing this is the
  // one whose decline matters. So intersection is sufficient for v1; a
  // myAddr-aware impl can come if we ever surface per-member state.)
  // Actually we should pass myAddr in. Add it.

  async function clickDecline() {
    if (pending) return;
    pending = true;
    error = null;
    try {
      await adapter.declineSortition(detail.pollId);
      onDecline();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      pending = false;
    }
  }
</script>

{#if alreadyDeclined}
  <p class="already-declined">You declined this role; a backup member is filling the slot.</p>
{:else}
  <div class="decline-affordance">
    <p>You're a member of the mini-public. Active participation in deliberation + drafting is expected.</p>
    <button type="button" onclick={clickDecline} disabled={pending}>
      {pending ? 'Declining…' : 'Decline mini-public role'}
    </button>
    {#if error}<p class="error">{error}</p>{/if}
  </div>
{/if}

<style>
  .decline-affordance { margin: 0.75rem 0; }
  .decline-affordance button {
    background: transparent;
    color: #d93838;
    border: 1px solid #d93838;
    padding: 0.35rem 0.8rem;
    border-radius: 4px;
    cursor: pointer;
  }
  .already-declined { color: #8a8c95; font-style: italic; }
  .error { color: #d93838; }
</style>
```

**IMPLEMENTER NOTE:** The `alreadyDeclined` derivation above is messy because the component doesn't receive `myAddr`. Add `myAddr: string` to `$props`, then `alreadyDeclined = $derived(detail.declined.some(([owner]) => owner === myAddr))`. Update the parent panel's invocation in Tier3ProposalPanel.svelte to pass `{myAddr}`.

- [ ] **Step 3: Run + commit**

```bash
git add src/lib/components/MiniPublicParticipationToggle.svelte src/lib/components/__tests__/MiniPublicParticipationToggle.test.ts src/lib/components/Tier3ProposalPanel.svelte
git commit -m "feat(zeb-311): MiniPublicParticipationToggle — decline affordance

One-shot decline button visible to mini-public members during
Deliberation and Drafting stages. Hidden once self appears in
detail.declined. Threads myAddr from the parent panel.
"
```

---

### Task 9: `DraftingPanel.svelte` + Vitest

**Files:**
- Create: `src/lib/components/DraftingPanel.svelte`
- Create: `src/lib/components/__tests__/DraftingPanel.test.ts`

Mini-public-restricted writable; observers read-only. Textarea (512-char limit) → `adapter.proposeDraftCandidate`. List of existing candidates with approval counts + Approve buttons (no Unapprove — kd=da is a one-shot positive event; backend handles dedup).

- [ ] **Step 1: Write the failing test**

```typescript
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import DraftingPanel from '../DraftingPanel.svelte';
import { VotingAdapter } from '$lib/voting-adapter';
import type { Tier3PollExport, DraftCandidateExport } from '$lib/types/voting';

const candidates: DraftCandidateExport[] = [
  { eventHash: 'aa'.repeat(32), text: 'Candidate A', proposer: 'pp'.repeat(32), approvalCount: 2 },
  { eventHash: 'bb'.repeat(32), text: 'Candidate B', proposer: 'qq'.repeat(32), approvalCount: 1 },
];

const baseDetail: Tier3PollExport = {
  pollId: 'aa'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Amend §3',
  proposer: 'pp'.repeat(32),
  stage: 'dr',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  deliberationWindowSeconds: 1_209_600,
  draftingWindowSeconds: 604_800,
  ratificationWindowSeconds: 1_209_600,
  incentiveMode: 'd',
  miniPublic: ['mm'.repeat(32)],
  backupPool: [],
  declined: [],
  draftCandidates: candidates,
  ratificationCandidates: [],
  myRole: 'mini_public',
  myDraftingApprovals: [],
  myRatificationScores: null,
  winnerEventHash: null,
  runnerUpEventHash: null,
};

describe('DraftingPanel', () => {
  it('lists candidates with approval counts', () => {
    const adapter = new VotingAdapter();
    const { getByText } = render(DraftingPanel, {
      props: { detail: baseDetail, adapter, myAddr: 'mm'.repeat(32), onChange: () => {} },
    });
    expect(getByText('Candidate A')).toBeTruthy();
    expect(getByText('Candidate B')).toBeTruthy();
    expect(getByText(/2 approval/)).toBeTruthy();
  });

  it('mini-public members can propose new candidate via textarea', async () => {
    const adapter = new VotingAdapter();
    vi.spyOn(adapter, 'proposeDraftCandidate').mockResolvedValue('cc'.repeat(32));
    const onChange = vi.fn();
    const { getByLabelText, getByText } = render(DraftingPanel, {
      props: { detail: baseDetail, adapter, myAddr: 'mm'.repeat(32), onChange },
    });
    await fireEvent.input(getByLabelText(/Propose candidate/i), { target: { value: 'New candidate' } });
    await fireEvent.click(getByText(/Submit candidate/i));
    await waitFor(() => expect(adapter.proposeDraftCandidate).toHaveBeenCalledWith(baseDetail.pollId, 'New candidate'));
    expect(onChange).toHaveBeenCalled();
  });

  it('renders read-only when myRole === observer', () => {
    const adapter = new VotingAdapter();
    const { queryByLabelText } = render(DraftingPanel, {
      props: {
        detail: { ...baseDetail, myRole: 'observer' },
        adapter,
        myAddr: 'zz'.repeat(32),
        onChange: () => {},
      },
    });
    expect(queryByLabelText(/Propose candidate/i)).toBeNull();
  });

  it('approve button is disabled when already approved', () => {
    const adapter = new VotingAdapter();
    const { getAllByText } = render(DraftingPanel, {
      props: {
        detail: { ...baseDetail, myDraftingApprovals: ['aa'.repeat(32)] },
        adapter,
        myAddr: 'mm'.repeat(32),
        onChange: () => {},
      },
    });
    const buttons = getAllByText(/Approved|Approve/);
    expect((buttons[0] as HTMLButtonElement).disabled).toBe(true);
  });
});
```

Run: `npx vitest run DraftingPanel`
Expected: FAIL.

- [ ] **Step 2: Implement**

```svelte
<script lang="ts">
  /**
   * ZEB-311 — Drafting stage: mini-public proposes & approves draft
   * candidates. Observers see a read-only list.
   *
   * Per ZEB-287 R4: every $props field destructured below.
   * Per Tauri error-extraction memory.
   */
  import type { Tier3PollExport } from '../types/voting';
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

  let candidateText = $state('');
  let submitting = $state(false);
  let submitError = $state<string | null>(null);
  let approvingHash = $state<string | null>(null);
  let approveError = $state<string | null>(null);

  let canPropose = $derived(detail.myRole === 'mini_public');
  let approvalSet = $derived(new Set(detail.myDraftingApprovals));

  function shortAddr(hex: string | null): string {
    if (!hex) return 'system';
    return hex.length > 16 ? `${hex.slice(0, 8)}…${hex.slice(-4)}` : hex;
  }

  async function submitCandidate() {
    if (!candidateText.trim() || submitting) return;
    submitting = true;
    submitError = null;
    try {
      await adapter.proposeDraftCandidate(detail.pollId, candidateText.trim());
      candidateText = '';
      onChange();
    } catch (e) {
      submitError = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  async function approveCandidate(eventHash: string) {
    if (approvingHash) return;
    approvingHash = eventHash;
    approveError = null;
    try {
      await adapter.approveDraftCandidate(detail.pollId, eventHash);
      onChange();
    } catch (e) {
      approveError = e instanceof Error ? e.message : String(e);
    } finally {
      approvingHash = null;
    }
  }
</script>

<section class="drafting-panel">
  <h5>Draft candidates ({detail.draftCandidates.length})</h5>

  {#if detail.draftCandidates.length === 0}
    <p class="empty">No candidates proposed yet.</p>
  {:else}
    <ul class="candidate-list">
      {#each detail.draftCandidates as c (c.eventHash)}
        <li class="candidate">
          <div class="candidate-body">
            <p class="text">{c.text}</p>
            <p class="proposer">— {shortAddr(c.proposer)}</p>
          </div>
          <div class="candidate-actions">
            <span class="approval-count">{c.approvalCount} approval{c.approvalCount === 1 ? '' : 's'}</span>
            {#if canPropose}
              {#if approvalSet.has(c.eventHash)}
                <button type="button" disabled>Approved</button>
              {:else}
                <button
                  type="button"
                  onclick={() => approveCandidate(c.eventHash)}
                  disabled={approvingHash !== null}
                >
                  {approvingHash === c.eventHash ? 'Approving…' : 'Approve'}
                </button>
              {/if}
            {/if}
          </div>
        </li>
      {/each}
    </ul>
    {#if approveError}<p class="error">{approveError}</p>{/if}
  {/if}

  {#if canPropose}
    <form
      class="propose-form"
      onsubmit={(e) => {
        e.preventDefault();
        submitCandidate();
      }}
    >
      <label>
        <span>Propose candidate</span>
        <textarea
          bind:value={candidateText}
          rows="2"
          maxlength="512"
          placeholder="Charter §3.1: Moderator dismissal requires 67% supermajority of voting members…"
        ></textarea>
      </label>
      <div class="form-footer">
        <span class="char-count">{candidateText.length}/512</span>
        <button type="submit" disabled={!candidateText.trim() || submitting}>
          {submitting ? 'Submitting…' : 'Submit candidate'}
        </button>
      </div>
      {#if submitError}<p class="error">{submitError}</p>{/if}
    </form>
  {/if}
</section>

<style>
  .drafting-panel { margin-top: 1rem; }
  .candidate-list { list-style: none; padding: 0; }
  .candidate {
    display: flex;
    justify-content: space-between;
    gap: 0.75rem;
    padding: 0.5rem 0.75rem;
    background: var(--input-bg, #0e0f15);
    border-radius: 4px;
    margin-bottom: 0.4rem;
  }
  .candidate-body { flex: 1; }
  .candidate-body .text { margin: 0; }
  .candidate-body .proposer { margin: 0.15rem 0 0; color: #8a8c95; font-size: 0.8rem; }
  .candidate-actions { display: flex; flex-direction: column; align-items: flex-end; gap: 0.25rem; }
  .approval-count { color: #8a8c95; font-size: 0.8rem; }
  .candidate-actions button {
    background: var(--accent, #4a9eff);
    color: #fff;
    border: 0;
    padding: 0.25rem 0.6rem;
    border-radius: 3px;
    cursor: pointer;
  }
  .candidate-actions button:disabled { opacity: 0.6; cursor: not-allowed; }
  .propose-form { margin-top: 0.75rem; display: flex; flex-direction: column; gap: 0.4rem; }
  .propose-form textarea {
    background: var(--input-bg, #0e0f15);
    color: inherit;
    border: 1px solid #2a2c34;
    border-radius: 4px;
    padding: 0.4rem 0.5rem;
    width: 100%;
  }
  .form-footer { display: flex; justify-content: space-between; align-items: center; }
  .char-count { color: #8a8c95; font-size: 0.75rem; }
  .form-footer button {
    background: var(--accent, #4a9eff);
    color: #fff;
    border: 0;
    padding: 0.3rem 0.8rem;
    border-radius: 3px;
    cursor: pointer;
  }
  .form-footer button:disabled { opacity: 0.5; cursor: not-allowed; }
  .empty { color: #8a8c95; }
  .error { color: #d93838; }
</style>
```

- [ ] **Step 3: Run + commit**

```bash
git add src/lib/components/DraftingPanel.svelte src/lib/components/__tests__/DraftingPanel.test.ts
git commit -m "feat(zeb-311): DraftingPanel — mini-public propose & approve candidates

Textarea capped at 512 chars per spec §6.3. Mini-public members
can propose + approve; observers see read-only list. Already-
approved candidates show a disabled 'Approved' button.
"
```

---

### Task 10: `StarRatificationBallot.svelte` + Vitest

**Files:**
- Create: `src/lib/components/StarRatificationBallot.svelte`
- Create: `src/lib/components/__tests__/StarRatificationBallot.test.ts`

For each candidate (including the synthesized status_quo), a 0-5 slider paired with a number input. "Cast ballot" button triggers click-confirm severity, then `adapter.castRatificationBallot`. Re-cast allowed (later kd=rb supersedes prior). After cast, the ballot fields stay editable but show "✓ submitted" indicator.

- [ ] **Step 1: Write the failing test**

```typescript
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StarRatificationBallot from '../StarRatificationBallot.svelte';
import { VotingAdapter } from '$lib/voting-adapter';
import type { Tier3PollExport } from '$lib/types/voting';

const detail: Tier3PollExport = {
  pollId: 'aa'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Amend §3',
  proposer: 'pp'.repeat(32),
  stage: 'ra',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  deliberationWindowSeconds: 1_209_600,
  draftingWindowSeconds: 604_800,
  ratificationWindowSeconds: 1_209_600,
  incentiveMode: 'd',
  miniPublic: ['mm'.repeat(32)],
  backupPool: [],
  declined: [],
  draftCandidates: [],
  ratificationCandidates: [
    { eventHash: 'aa'.repeat(32), text: 'Candidate A' },
    { eventHash: 'bb'.repeat(32), text: 'Candidate B' },
  ],
  myRole: 'observer',
  myDraftingApprovals: [],
  myRatificationScores: null,
  winnerEventHash: null,
  runnerUpEventHash: null,
};

describe('StarRatificationBallot', () => {
  it('renders one slider per ratification candidate', () => {
    const adapter = new VotingAdapter();
    const { getAllByRole } = render(StarRatificationBallot, {
      props: { detail, adapter, onCast: () => {} },
    });
    const sliders = getAllByRole('slider');
    expect(sliders).toHaveLength(2);
  });

  it('cast button opens confirm modal before invoking', async () => {
    const adapter = new VotingAdapter();
    vi.spyOn(adapter, 'castRatificationBallot').mockResolvedValue();
    const { getByText, findByText } = render(StarRatificationBallot, {
      props: { detail, adapter, onCast: () => {} },
    });
    await fireEvent.click(getByText(/Cast ballot/i));
    expect(await findByText(/Confirm ratification ballot/i)).toBeTruthy();
    expect(adapter.castRatificationBallot).not.toHaveBeenCalled();
    await fireEvent.click(await findByText(/^Confirm$/i));
    await waitFor(() => expect(adapter.castRatificationBallot).toHaveBeenCalledWith(detail.pollId, [0, 0]));
  });

  it('prefills sliders with myRatificationScores when present', () => {
    const adapter = new VotingAdapter();
    const { getAllByRole } = render(StarRatificationBallot, {
      props: { detail: { ...detail, myRatificationScores: [3, 5] }, adapter, onCast: () => {} },
    });
    const sliders = getAllByRole('slider') as HTMLInputElement[];
    expect(sliders[0].value).toBe('3');
    expect(sliders[1].value).toBe('5');
  });
});
```

Run: `npx vitest run StarRatificationBallot`
Expected: FAIL.

- [ ] **Step 2: Implement**

```svelte
<script lang="ts">
  /**
   * ZEB-311 — STAR ratification ballot. One 0-5 slider per candidate
   * (paired with a number input per accessibility memory). Cast goes
   * through click-confirm severity tier. Re-cast allowed.
   *
   * Per ZEB-287 R4: every $props field destructured below.
   * Per Tauri error-extraction memory.
   */
  import type { Tier3PollExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';

  let {
    detail,
    adapter,
    onCast,
  }: {
    detail: Tier3PollExport;
    adapter: VotingAdapter;
    onCast: () => void;
  } = $props();

  // Initial scores: prefill from server-side prior ballot, else all-zero.
  let scores = $state<number[]>(
    detail.myRatificationScores
      ? [...detail.myRatificationScores]
      : new Array(detail.ratificationCandidates.length).fill(0),
  );
  let confirming = $state(false);
  let casting = $state(false);
  let castError = $state<string | null>(null);
  let castSuccess = $state(false);

  // If detail changes (re-fetch on stage transition / new ballot from server),
  // sync our local scores with the server snapshot.
  $effect(() => {
    if (detail.myRatificationScores) {
      scores = [...detail.myRatificationScores];
    } else if (scores.length !== detail.ratificationCandidates.length) {
      scores = new Array(detail.ratificationCandidates.length).fill(0);
    }
  });

  function setScore(index: number, value: number) {
    const clamped = Math.max(0, Math.min(5, Math.round(value)));
    scores[index] = clamped;
  }

  async function confirmCast() {
    confirming = false;
    casting = true;
    castError = null;
    try {
      await adapter.castRatificationBallot(detail.pollId, scores);
      castSuccess = true;
      onCast();
    } catch (e) {
      castError = e instanceof Error ? e.message : String(e);
    } finally {
      casting = false;
    }
  }
</script>

<section class="ratification-ballot">
  <h5>STAR ratification ballot</h5>
  <p class="instructions">Score each candidate 0-5. Top two advance to runoff; the candidate scored higher on more ballots wins.</p>

  <ol class="candidate-list">
    {#each detail.ratificationCandidates as c, i (c.eventHash)}
      <li class="candidate">
        <div class="candidate-text">{c.text}</div>
        <div class="score-input">
          <input
            type="range"
            min="0"
            max="5"
            step="1"
            value={scores[i]}
            oninput={(e) => setScore(i, Number((e.target as HTMLInputElement).value))}
            aria-label={`Score for ${c.text}`}
          />
          <input
            type="number"
            min="0"
            max="5"
            value={scores[i]}
            oninput={(e) => setScore(i, Number((e.target as HTMLInputElement).value))}
            aria-label={`Score number for ${c.text}`}
          />
        </div>
      </li>
    {/each}
  </ol>

  <div class="ballot-footer">
    {#if castSuccess}
      <span class="success">✓ Ballot submitted</span>
    {/if}
    <button type="button" onclick={() => (confirming = true)} disabled={casting || detail.stage !== 'ra'}>
      {casting ? 'Casting…' : detail.myRatificationScores ? 'Re-cast ballot' : 'Cast ballot'}
    </button>
  </div>
  {#if castError}<p class="error">{castError}</p>{/if}
</section>

{#if confirming}
  <div class="confirm-modal" role="dialog" aria-modal="true" aria-label="Confirm ratification ballot">
    <p>Confirm ratification ballot</p>
    <ul class="ballot-summary">
      {#each detail.ratificationCandidates as c, i}
        <li><strong>{scores[i]}</strong> — {c.text}</li>
      {/each}
    </ul>
    <p class="caveat">You can re-cast later if the ratification window is still open.</p>
    <div class="confirm-actions">
      <button type="button" onclick={() => (confirming = false)}>Cancel</button>
      <button type="button" onclick={confirmCast}>Confirm</button>
    </div>
  </div>
{/if}

<style>
  .ratification-ballot { margin-top: 1rem; }
  .instructions { color: #8a8c95; font-size: 0.85rem; }
  .candidate-list { list-style: decimal; padding-left: 1.25rem; }
  .candidate {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 0.75rem;
    align-items: center;
    margin-bottom: 0.4rem;
  }
  .candidate-text { font-weight: 500; }
  .score-input { display: flex; gap: 0.4rem; align-items: center; }
  .score-input input[type="range"] { width: 140px; }
  .score-input input[type="number"] {
    width: 50px;
    background: var(--input-bg, #0e0f15);
    color: inherit;
    border: 1px solid #2a2c34;
    border-radius: 3px;
    padding: 0.2rem 0.3rem;
  }
  .ballot-footer { margin-top: 0.75rem; display: flex; justify-content: flex-end; gap: 0.75rem; align-items: center; }
  .ballot-footer button {
    background: var(--accent, #4a9eff);
    color: #fff;
    border: 0;
    padding: 0.35rem 0.9rem;
    border-radius: 3px;
    cursor: pointer;
  }
  .ballot-footer button:disabled { opacity: 0.5; cursor: not-allowed; }
  .success { color: var(--success, #4ad97a); font-size: 0.85rem; }
  .error { color: #d93838; }
  .confirm-modal {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: grid;
    place-items: center;
    z-index: 100;
  }
  .confirm-modal > * {
    background: var(--panel-bg, #1a1c24);
    padding: 1rem 1.5rem;
    border-radius: 8px;
    margin: 0.25rem;
  }
  .ballot-summary { list-style: none; padding: 0; }
  .ballot-summary li { padding: 0.1rem 0; }
  .caveat { color: #8a8c95; font-size: 0.8rem; }
  .confirm-actions { display: flex; gap: 0.5rem; }
  .confirm-actions button:last-child {
    background: var(--accent, #4a9eff);
    color: #fff;
  }
</style>
```

- [ ] **Step 3: Run + commit**

```bash
git add src/lib/components/StarRatificationBallot.svelte src/lib/components/__tests__/StarRatificationBallot.test.ts
git commit -m "feat(zeb-311): StarRatificationBallot — 0-5 sliders + click-confirm cast

Paired slider + number-input per candidate per
feedback_slider_pair_with_number_input. Click-confirm severity tier
per feedback_severe_action_confirmation (ballot is re-castable).
Prefills sliders with server-side prior ballot when present.
"
```

---

### Task 11: Wire `'tier3'` tab into `CommunityView.svelte`

**Files:**
- Modify: `src/lib/components/CommunityView.svelte`

Add a "Constitutional" tab to the existing view switcher. When active, render `Tier3ProposalPanel`. No new test file — `CommunityView.test.ts` (if present) gets one new test case asserting the tab mounts.

- [ ] **Step 1: Read the existing view switcher to know the pattern**

Run: `grep -nE "(activeView|view ==)" src/lib/components/CommunityView.svelte | head -20`

- [ ] **Step 2: Modify `CommunityView.svelte`**

Add the import:
```typescript
import Tier3ProposalPanel from './Tier3ProposalPanel.svelte';
```

Add the route block alongside `activeView === 'proposals'`:
```svelte
{:else if activeView === 'tier3' && votingAdapter}
  <Tier3ProposalPanel
    {communityId}
    adapter={votingAdapter}
    myAddr={ownAddress}
  />
```

Wire the tab button next to the existing Proposals nav (find wherever activeView gets set):
```svelte
<button class:active={activeView === 'tier3'} onclick={() => (activeView = 'tier3')}>
  Constitutional
</button>
```

- [ ] **Step 3: Add a CommunityView test (or update existing) asserting the route works**

If `CommunityView.test.ts` exists, append:
```typescript
it('renders Tier3ProposalPanel when activeView === tier3', () => {
  // ... mount with activeView='tier3', votingAdapter mocked
  // expect a heading containing "Constitutional Decisions"
});
```

If no test file exists, skip this step — manual verification via the smoke-test step in Task 12 will cover it.

- [ ] **Step 4: Run frontend gates**

Run: `npx tsc --noEmit && npx vitest run`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/CommunityView.svelte src/lib/components/__tests__/CommunityView.test.ts
git commit -m "feat(zeb-311): wire Tier 3 Constitutional tab into CommunityView

Adds 'Constitutional' tab to the view switcher; mounts
Tier3ProposalPanel when activeView === 'tier3'. Tier 2 Proposals
tab stays unchanged.
"
```

---

### Task 12: Final 5-gate sweep + push + PR creation

**Files:** none.

- [ ] **Step 1: Run all 5 backend + 2 frontend gates**

From `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

From repo root:
```bash
npx tsc --noEmit
npx vitest run
```

Expected: all PASS aside from the 28 pre-existing orphan failures.

- [ ] **Step 2: Push the branch**

```bash
git push -u origin zeb-311-tier3-ui
```

- [ ] **Step 3: Create the PR**

Use markdown-linked refs per `feedback_linear_pr_auto_close` (no bare `Closes ZEB-NNN`):

```bash
gh pr create --title "ZEB-311: Tier 3 governance UI + Tier3 getter IPCs" --body "$(cat <<'EOF'
## Summary

Ships the Phase 4a-main UI for Tier 3 constitutional governance ([ZEB-311](https://linear.app/zeblith/issue/ZEB-311), parent [ZEB-293](https://linear.app/zeblith/issue/ZEB-293)):

- **6 Svelte 5 components** (flat under `src/lib/components/`):
  - `Tier3ProposalPanel.svelte` — create form + list of polls + stage-dispatched detail pane
  - `Tier3LifecycleStatus.svelte` — 4-chip stage progression + Failed/Finalized states
  - `SortitionRevealView.svelte` — mini-public + backup pool + declines
  - `MiniPublicParticipationToggle.svelte` — decline affordance for selected members
  - `DraftingPanel.svelte` — candidate proposal + approval (mini-public-restricted)
  - `StarRatificationBallot.svelte` — 0-5 sliders with click-confirm cast
- **New "Constitutional" tab** in `CommunityView.svelte` mounts the panel.
- **Two new pull-style IPCs** added to fill the gap that [ZEB-310](https://linear.app/zeblith/issue/ZEB-310) deferred:
  - `voting_get_tier3_poll(poll_id) -> Tier3PollExport` — full state + caller-derived `my_role` / `my_*` fields.
  - `voting_list_tier3_polls(community_id) -> Vec<Tier3PollSummary>` — every Tier 3 poll regardless of lifecycle, ordered newest-first.
- **CBOR wire-format fixture** pins `Tier3PollExport` + `Tier3PollSummary` encoding.

Closes [ZEB-311](https://linear.app/zeblith/issue/ZEB-311).

## Test plan

- [x] `cargo fmt --all -- --check` (src-tauri)
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` (src-tauri)
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (src-tauri; 28 pre-existing orphan failures expected)
- [x] `npx tsc --noEmit` (repo root)
- [x] `npx vitest run` (repo root)
- [ ] Manual smoke: open the dev app, create a community, switch to the new "Constitutional" tab, attempt a proposal (it'll fail at sortition because no D-FROST committee is provisioned in dev mode — that's expected; the failure path is what the UI surfaces).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Capture the PR URL and report it to the user.

- [ ] **Step 4: Hand off to the autonomous bot-review monitoring loop**

Controller (NOT the implementer) takes over here. The plan is done; what follows is the standard bot-review cycle (CodeRabbit + Cursor Bugbot + CodeAnt + Qodo per `feedback_autonomous_pr_monitoring_loop`).

---

## Self-Review

### Spec coverage check

Going through ZEB-311's scope clause-by-clause:

1. ✅ Tier3ProposalPanel — Task 6
2. ✅ SortitionRevealView — Task 7
3. ✅ MiniPublicParticipationToggle — Task 8
4. ✅ DraftingPanel — Task 9
5. ✅ StarRatificationBallot — Task 10
6. ✅ Tier3LifecycleStatus — Task 5
7. ✅ Tauri error extraction — every implementation uses `e instanceof Error ? e.message : String(e)`
8. ✅ Svelte 5 `$props` destructuring — every component lists exactly the props it uses
9. ✅ Paired slider + number-input — sortition_size + 3 windows in Tier3ProposalPanel, 0-5 in StarRatificationBallot
10. ✅ Click-confirm severity tier — Create-proposal + Cast-ratification-ballot both gated
11. ✅ Vitest coverage — every component has its own `__tests__/*.test.ts` with happy-path + key edge-case coverage
12. ✅ SortitionFailed visible — Tier3LifecycleStatus renders the failed badge; ProposalPanel surfaces a Retry button for the proposer

### Placeholder scan

- No "TBD" / "implement later" / "Add appropriate error handling" / "Similar to Task N" anywhere.
- The one elision is the test harness implementation in Task 2 step 2 — the constructors (`with_poll_in_sortition_stage()` etc.) are described by their effect but not coded out, because the implementer should mirror the existing `community_voting_tier3_ipc_integration.rs` setup pattern. That's an integration-test scaffolding decision, not a contract decision.

### Type consistency check

- `Tier3PollExport` field names match between Task 1 (Rust struct with `#[serde(rename_all = "camelCase")]`), Task 4 (TS interface in camelCase), and every component (`detail.miniPublic`, `detail.draftCandidates`, etc.).
- `Tier3MyRole` variants `'proposer' | 'mini_public' | 'backup' | 'observer'` — Rust uses `#[serde(rename_all = "snake_case")]` so wire/JS match.
- `Tier3Stage` 2-char tags `'so' | 'de' | 'dr' | 'ra' | 'fi' | 'fa'` consistent in Rust `Tier3StageTag` (with `#[serde(rename = "...")]` per variant) + TS type alias.
- `adapter.getTier3Poll(pollId)` and `adapter.listTier3Polls(communityId)` use camelCased args that Tauri auto-converts to snake_case at the IPC boundary — matches the `#[tauri::command(rename_all = "snake_case")]` on the Rust side.

### Scope check

- ~13 tasks (Task 0 + 12 implementation tasks). Each is independently testable.
- Backend additions are ~250 LOC of Rust (one DTO module + two IPCs). Frontend additions are 6 Svelte components + 2 adapter methods + 6 TS types. ~1500-2000 LOC total.
- This is a single-PR-sized change. Splitting (backend / frontend) would add a review cycle without clarifying ownership.

Plan complete.
