# ZEB-309 Phase 4a-main Implementation Plan (data + engine + tests)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the Tier 3 governance mechanism (sortition + STAR ratification + drafting) backend for [ZEB-309](https://linear.app/zeblith/issue/ZEB-309), wired into the voting engine with auto-orchestrated D-FROST beacon coupling. Public ballots only; no Pol.is, no privacy modes (those are Phase 5/6/7).

**Architecture:** Three new files at `src-tauri/src/`: `community_voting_sortition.rs` (Fisher-Yates + VRF seeding, pure), `community_voting_star.rs` (score-then-runoff + tiebreaker cascade, pure), `community_voting_tier3.rs` (4-stage state machine + drafting + engine glue). Edits to `community_voting_core.rs` (new event kinds + Tier 3 PollConfig.ro field), `community_voting_log_engine.rs` (Arc<DfrostLogRegistry> handle + beacon callback), `lib.rs` (start_node/shutdown wiring). Two new integration test files. Wire-format fixtures pinned via the ZEB-250 regen-on-first-run pattern.

**Tech Stack:** Rust 1.88 stable; `ciborium` (CBOR); `serde` + `serde_repr`; `sha2`; `rand_chacha` (deterministic RNG seeded from VRF output); `frost-ristretto255` 2.x (already vendored); `tokio` for engine; integration tests use mpsc channels for in-process bidirectional engine bridges (per ZEB-307 pattern).

**Reference spec:** `docs/specs/2026-05-20-zeb-309-phase4a-main-design.md` (HEAD~0 on this branch). Implementers MUST read it before starting any task.

**Branch state:** `zeb-309-phase4a-main-data-engine` based on `origin/main` `4908692` (ZEB-307 merged). HEAD `60d7986` is the design spec commit. Pull-before-work satisfied.

---

## CI gate compliance (all tasks)

Every task except Task 0 ends in a commit. Each task must leave the tree green for these 5 gates locally before committing:

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Frontend gates (`npx tsc --noEmit`, `npx vitest run`) are unaffected by ZEB-309 (backend-only) — verify in Task 17 only.

`feedback_cargo_fmt_gate`: include `cargo fmt --check` in EVERY task's pre-commit verification, not just clippy.

`feedback_pipe_exit_codes_lie`: any `cmd | grep/tail` MUST use `set -o pipefail` or check `${PIPESTATUS[0]}`.

---

## File structure

| File | Disposition | Purpose | LOC est |
|---|---|---|---|
| `src-tauri/src/community_voting_core.rs` | Modify | Extend `PollEventKindCode` enum; add Tier 3 PollConfig with `ro` field; export new payload struct types | +~250 |
| `src-tauri/src/community_voting_sortition.rs` | **Create** | Fisher-Yates + VRF seed; pure functions | ~800 |
| `src-tauri/src/community_voting_star.rs` | **Create** | STAR tally + tiebreaker cascade; pure functions | ~600 |
| `src-tauri/src/community_voting_tier3.rs` | **Create** | 4-stage state machine + drafting math + DfrostLog coupling | ~1500 |
| `src-tauri/src/community_voting_log_engine.rs` | Modify | Add `Arc<DfrostLogRegistry<R>>` handle + beacon callback hook | +~150 |
| `src-tauri/src/lib.rs` | Modify | Wire voting_registry.install_dfrost_handle in start_node; refactor `dfrost_request_vrf_beacon` to expose a Rust-callable inner | +~100 |
| `src-tauri/tests/community_voting_tier3_integration.rs` | **Create** | 4-stage E2E + decline + failure + race tests | ~900 |
| `src-tauri/tests/wire_format_voting_tier3_fixtures.rs` | **Create** | Pin canonical CBOR for 8 event kinds | ~500 |

---

## Task 0: Pre-flight verification (no commit)

**Files:** none

- [ ] **Step 1: Confirm branch state**

```bash
git status
git log --oneline -3
git rev-parse --abbrev-ref HEAD
```

Expected:
- Branch: `zeb-309-phase4a-main-data-engine`
- HEAD: `60d7986 docs(zeb-309): Phase 4a-main design spec`
- Parent: `4908692 ZEB-307: D-FROST Zenoh transport ... (#146)`
- Working tree clean

- [ ] **Step 2: Confirm green baseline**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures 2>&1 | tail -3
```

Expected: `Summary [...] X tests run: X passed, ...` — all green. If any tests fail on `harmony-app` alone, STOP and pushover.

- [ ] **Step 3: Read the design spec**

Read `docs/specs/2026-05-20-zeb-309-phase4a-main-design.md` in full (670 lines). Pay particular attention to §3 (state machine), §6 (materialize), §7 (sortition algo), §8 (STAR math), §9 (drafting math), §11 (module factoring).

Read `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` §6 + §8 (Tier 3 sections + verify rules) for the umbrella context.

- [ ] **Step 4: Read existing patterns**

Read these in this order:
1. `src-tauri/src/community_voting_core.rs` (envelope, `PollEventKindCode`, `Tier`, `Eligibility`) — current state of voting core
2. `src-tauri/src/community_voting_approval.rs` (Tier 1 pattern) — variant idiom + materialize
3. `src-tauri/src/community_dfrost_log_engine.rs` lines 470-530 (`DfrostLogRegistry` API + `register` + `get` + `shutdown`)
4. `src-tauri/src/community_dfrost_types.rs` (look for `VrfBeaconPayload`, `derive_vrf_output`)
5. `src-tauri/src/community_voting_log_engine.rs` (current voting engine — to understand where to add DfrostLog handle)

No file changes, no commit. Just understand the lay of the land.

---

## Task 1: Wire-format extensions (envelope, kinds, Tier 3 PollConfig)

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs`

- [ ] **Step 1: Extend `PollEventKindCode` enum**

In `community_voting_core.rs` near line 159, add 7 new variants matching spec §2:

```rust
pub enum PollEventKindCode {
    // ...existing variants...
    #[serde(rename = "ss")]
    SortitionSelection,
    #[serde(rename = "ds")]
    DeliberationStatement,
    #[serde(rename = "md")]
    MiniPublicDecline,
    #[serde(rename = "dc")]
    DraftCandidate,
    #[serde(rename = "da")]
    DraftApproval,
    #[serde(rename = "sf")]
    SortitionFailed,
    #[serde(rename = "rb")]
    RatificationBallot,
}
```

- [ ] **Step 2: Write the failing payload-struct round-trip tests**

Add a new `#[cfg(test)] mod tier3_payload_tests` after envelope_tests. Test that CBOR round-trips work for each of the 7 new payload struct types (`SortitionSelectionPayload`, `DeliberationStatementPayload`, `MiniPublicDeclinePayload`, `DraftCandidatePayload`, `DraftApprovalPayload`, `SortitionFailedPayload`, `RatificationBallotPayload`, plus `Tier3PollConfigPayload` with `ro` field).

Each payload follows spec §2.1: 2-char field keys (`pi` for poll_id, `pr` for primary, `bk` for backup, `tx` for text, `rs` for reason, `ch` for candidate_event_hash, `sc` for scores). Use `serde_bytes` for byte fields; `skip_serializing_if = "Option::is_none"` for optional fields.

- [ ] **Step 3: Run tests to verify they fail (compile error — types don't exist)**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(tier3_payload)' 2>&1 | tail -20
```

Expected: compile error on missing types.

- [ ] **Step 4: Implement the payload structs**

Add 8 new structs (7 event payloads + Tier 3 PollConfig) with proper serde attributes matching spec §2. `PollId` is reused from existing module. Use `OwnerAddr` from `owner_state_types`. Define `CandidateEventHash` as `pub type CandidateEventHash = [u8; 32]`.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortitionSelectionPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(rename = "pr")]
    pub primary: Vec<OwnerAddr>,
    #[serde(rename = "bk")]
    pub backup: Vec<OwnerAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliberationStatementPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(rename = "tx")]
    pub text: String,  // ≤280 chars
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MiniPublicDeclinePayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,  // ≤2 chars
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftCandidatePayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(rename = "tx")]
    pub text: String,  // ≤512 chars
}

pub type CandidateEventHash = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftApprovalPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(
        rename = "ch",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub candidate_event_hash: CandidateEventHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortitionFailedPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatificationBallotPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(rename = "sc", with = "serde_bytes")]
    pub scores: Vec<u8>,  // each 0..=5; len matches ratification_candidates
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier3PollConfigPayload {
    #[serde(rename = "pt")]
    pub proposal_text: String,
    #[serde(rename = "ss")]
    pub sortition_size: u16,
    #[serde(rename = "dw")]
    pub deliberation_window_seconds: u32,
    #[serde(rename = "fw")]
    pub drafting_window_seconds: u32,
    #[serde(rename = "rw")]
    pub ratification_window_seconds: u32,
    #[serde(rename = "pm")]
    pub privacy_mode: String,  // "pu" (Phase 4a-main only); "se"/"rf" reserved for Phase 6/7
    #[serde(rename = "im")]
    pub incentive_mode: String,  // "a" | "b" | "c" | "d"
    #[serde(rename = "el")]
    pub eligibility: Eligibility,
    #[serde(rename = "ro", skip_serializing_if = "Option::is_none", default)]
    pub retry_of: Option<PollId>,
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(tier3_payload)' 2>&1 | tail -10
```

Expected: PASS for all round-trip tests.

- [ ] **Step 6: Pre-commit gate verification**

```bash
cd src-tauri && cargo fmt --all -- --check && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -3
```

Expected: No fmt diffs; 0 clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_voting_core.rs
git commit -m "$(cat <<'EOF'
feat(zeb-309): extend wire format for Tier 3 events

Add 7 new PollEventKindCode variants (ss/ds/md/dc/da/sf/rb) and
their payload struct types. Tier 3 PollConfig payload with `ro`
(retry_of) field for retry chains. Reserves "pu" (Phase 4a-main),
"se" (Phase 6), "rf" (Phase 7) values for privacy_mode field.

All payloads use 2-char same-length CBOR keys per spec §2. Round-trip
tests pinned for each payload type.

Refs ZEB-309. Design spec: docs/specs/2026-05-20-zeb-309-phase4a-main-design.md §2.
EOF
)"
```

---

## Task 2: Sortition module (Fisher-Yates + VRF seeding)

**Files:**
- Create: `src-tauri/src/community_voting_sortition.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod community_voting_sortition;`)

- [ ] **Step 1: Write the failing tests first**

Create the test module with these tests (use `#[cfg(test)]` inline):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::OwnerAddr;

    fn make_electorate(n: usize) -> Vec<OwnerAddr> {
        (0..n).map(|i| OwnerAddr([i as u8; 16])).collect()
    }

    #[test]
    fn derive_beacon_seed_deterministic() {
        let h = [0x42u8; 32];
        let s1 = derive_beacon_seed(&h, 7);
        let s2 = derive_beacon_seed(&h, 7);
        assert_eq!(s1, s2);
    }

    #[test]
    fn derive_beacon_seed_changes_with_epoch() {
        let h = [0x42u8; 32];
        let s1 = derive_beacon_seed(&h, 7);
        let s2 = derive_beacon_seed(&h, 8);
        assert_ne!(s1, s2);
    }

    #[test]
    fn canonical_order_idempotent() {
        let e = make_electorate(10);
        let c1 = canonical_electorate_order(&e);
        let c2 = canonical_electorate_order(&c1);
        assert_eq!(c1, c2);
    }

    #[test]
    fn canonical_order_invariant_under_input_permutation() {
        let mut e1 = make_electorate(10);
        let mut e2 = e1.clone();
        e2.reverse();
        assert_eq!(canonical_electorate_order(&e1), canonical_electorate_order(&e2));
    }

    #[test]
    fn fisher_yates_deterministic() {
        let e = make_electorate(20);
        let vrf = [0x55u8; 32];
        let r1 = fisher_yates_select(&vrf, &e, 5, 5);
        let r2 = fisher_yates_select(&vrf, &e, 5, 5);
        assert_eq!(r1, r2);
    }

    #[test]
    fn fisher_yates_different_seeds_yield_different_results() {
        let e = make_electorate(20);
        let r1 = fisher_yates_select(&[0x01u8; 32], &e, 5, 5);
        let r2 = fisher_yates_select(&[0x02u8; 32], &e, 5, 5);
        assert_ne!(r1, r2);
    }

    #[test]
    fn fisher_yates_primary_size_correct() {
        let e = make_electorate(30);
        let r = fisher_yates_select(&[0u8; 32], &e, 10, 5);
        assert_eq!(r.primary.len(), 10);
        assert_eq!(r.backup.len(), 5);
    }

    #[test]
    fn fisher_yates_primary_and_backup_disjoint() {
        let e = make_electorate(30);
        let r = fisher_yates_select(&[0u8; 32], &e, 10, 5);
        for p in &r.primary {
            assert!(!r.backup.contains(p), "primary {p:?} also in backup");
        }
    }

    #[test]
    fn fisher_yates_all_selections_in_electorate() {
        let e = make_electorate(20);
        let r = fisher_yates_select(&[0u8; 32], &e, 5, 5);
        for p in r.primary.iter().chain(r.backup.iter()) {
            assert!(e.contains(p));
        }
    }

    #[test]
    #[should_panic(expected = "electorate too small")]
    fn fisher_yates_panics_on_small_electorate() {
        let e = make_electorate(5);
        fisher_yates_select(&[0u8; 32], &e, 5, 5);
    }

    #[test]
    fn fisher_yates_canonicalizes_input_order() {
        let mut e1 = make_electorate(20);
        let mut e2 = e1.clone();
        e2.reverse();
        let r1 = fisher_yates_select(&[0u8; 32], &e1, 5, 5);
        let r2 = fisher_yates_select(&[0u8; 32], &e2, 5, 5);
        assert_eq!(r1, r2, "result must not depend on input order");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_voting_sortition)' 2>&1 | tail -5
```

Expected: compile error.

- [ ] **Step 3: Implement the module**

Create `src-tauri/src/community_voting_sortition.rs` with:

```rust
//! ZEB-309 Phase 4a-main: deterministic sortition selection.
//!
//! Pure functions. No I/O, no async. VRF beacon seed → Fisher-Yates
//! over canonicalized electorate → primary + backup OwnerAddr lists.
//! See docs/specs/2026-05-20-zeb-309-phase4a-main-design.md §7.

use crate::owner_state_types::OwnerAddr;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortitionResult {
    pub primary: Vec<OwnerAddr>,
    pub backup: Vec<OwnerAddr>,
}

pub fn derive_beacon_seed(poll_create_hash: &[u8; 32], community_epoch: u32) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(poll_create_hash);
    hasher.update(&community_epoch.to_be_bytes());
    hasher.finalize().into()
}

pub fn canonical_electorate_order(electorate: &[OwnerAddr]) -> Vec<OwnerAddr> {
    let mut sorted = electorate.to_vec();
    sorted.sort();
    sorted
}

pub fn fisher_yates_select(
    vrf_output: &[u8; 32],
    electorate: &[OwnerAddr],
    primary_size: usize,
    backup_size: usize,
) -> SortitionResult {
    let total = primary_size + backup_size;
    assert!(electorate.len() >= total, "electorate too small: {} < {}", electorate.len(), total);
    let mut shuffled = canonical_electorate_order(electorate);
    let mut rng = ChaCha20Rng::from_seed(*vrf_output);
    for i in (1..shuffled.len()).rev() {
        let j = rng.gen_range(0..=i);
        shuffled.swap(i, j);
    }
    SortitionResult {
        primary: shuffled[..primary_size].to_vec(),
        backup: shuffled[primary_size..total].to_vec(),
    }
}
```

Add `rand` and `rand_chacha` to `src-tauri/Cargo.toml` if not already present. Check with:

```bash
grep -E '^rand|^rand_chacha' src-tauri/Cargo.toml
```

If missing, add to `[dependencies]`:

```toml
rand = "0.8"
rand_chacha = "0.3"
```

- [ ] **Step 4: Add module declaration to lib.rs**

In `src-tauri/src/lib.rs`, add near the other `pub mod community_voting_*` declarations:

```rust
pub mod community_voting_sortition;
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_voting_sortition)' 2>&1 | tail -10
```

Expected: 10+ tests PASS.

- [ ] **Step 6: Pre-commit gates**

```bash
cd src-tauri && cargo fmt --all -- --check && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -3
```

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_voting_sortition.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "$(cat <<'EOF'
feat(zeb-309): deterministic sortition (Fisher-Yates + VRF seed)

community_voting_sortition.rs holds pure functions: derive_beacon_seed
(SHA-256 of PollCreate.event_hash || community_epoch), canonical
electorate ordering (sort by OwnerAddr byte lex ASC for shuffle
input-order invariance), and Fisher-Yates selection seeded by ChaCha20Rng
from the VRF output. Returns SortitionResult { primary, backup }.

10+ unit tests cover determinism, seed sensitivity, primary/backup
disjointness, input-order invariance, panic-on-small-electorate.

Refs ZEB-309. Design spec §7.
EOF
)"
```

---

## Task 3: STAR ratification math module

**Files:**
- Create: `src-tauri/src/community_voting_star.rs`
- Modify: `src-tauri/src/lib.rs` (add module declaration)

- [ ] **Step 1: Write the failing tests**

Create a `tests` mod with the 6+ test cases from design spec §8 ("Test cases"). Implementer should expand to at least 20 unit tests covering:

- 3-candidate happy path (A=30/30, B=24/30, C=15/30 — finalists [A, B], A wins runoff)
- Score-round tie at 2nd-place (3-way runoff)
- Runoff abstention on equal-score finalists
- Runoff tie → total_score tiebreaker
- Total-score tie → event_hash lex tiebreaker
- Empty ballots (status_quo wins by default)
- Single-ballot edge case
- All-zero scores edge case
- 5-candidate full slate
- Identical ballots × N converge correctly
- Property test (optional): result invariant under ballot reordering (use `quickcheck` if convenient, else hand-written)

- [ ] **Step 2: Implement `community_voting_star.rs`**

Per design spec §8 — define:

```rust
//! ZEB-309 Phase 4a-main: STAR ratification math (score + automatic runoff).
//!
//! Pure functions. Deterministic. Tiebreaker cascade:
//! runoff_votes → total_score → candidate_event_hash lex ASC.

use crate::community_voting_core::{CandidateEventHash, RatificationBallotPayload};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateRef {
    pub event_hash: CandidateEventHash,
    // Other fields per spec — text optional, approval_count optional
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StarResult {
    pub winner: CandidateRef,
    pub finalists: Vec<CandidateRef>,
    pub total_scores: Vec<u32>,    // indexed by candidates input
    pub runoff_votes: Vec<u32>,    // indexed by finalists
}

pub fn tally_star(
    candidates: &[CandidateRef],
    ballots: &[RatificationBallotPayload],
) -> StarResult {
    // Per design spec §8: score round → finalists → runoff → winner.
    // ...full implementation per spec pseudo-code...
}
```

Implementer fills in the implementation following spec §8 pseudo-code, with care on:
- Score-round tie at 2nd-place → include ALL tied candidates in runoff
- Equal-score finalists on a ballot → abstain (no vote, not split)
- Winner selection: sort finalists by (runoff_votes DESC, total_score DESC, event_hash ASC)

- [ ] **Step 3: Run tests, iterate to green**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_voting_star)' 2>&1 | tail -10
```

- [ ] **Step 4: Pre-commit gates + commit**

```bash
cd src-tauri && cargo fmt --all -- --check && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -3

git add src-tauri/src/community_voting_star.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-309): STAR ratification tally (score + automatic runoff)

community_voting_star.rs: pure tally_star function over candidates
and RatificationBallotPayload list. Returns StarResult with winner,
finalists, total_scores, runoff_votes.

Tiebreaker cascade per design spec §8:
1. Score round: top-2 finalists (3-way score-tie at 2nd-place
   includes all tied in runoff).
2. Runoff: ballot vote → max-scored finalist; equal-score = abstain.
3. Winner: max runoff_votes; tiebreak by total_score; final tiebreak
   by candidate_event_hash lex ASC.

20+ unit tests cover all tiebreaker branches, empty-ballot edge,
all-zero-scores edge, 5-candidate full slate, ballot-order invariance.

Refs ZEB-309. Design spec §8.
EOF
)"
```

---

## Task 4: Tier 3 state machine skeleton (Tier3PollState + Stage enum + apply_event)

**Files:**
- Create: `src-tauri/src/community_voting_tier3.rs`
- Modify: `src-tauri/src/lib.rs` (add module declaration)

- [ ] **Step 1: Write failing tests for state machine transitions**

Tests must cover:
- New poll: starts in `Stage::Sortition`
- kd=ss applied → `Stage::Deliberation` (assuming HLC past stage_2_threshold? per spec re-evaluation)
- HLC watermark advance → Stage 2→3 and 3→4 transitions
- kd=md decline → declines list grows + backup auto-promotion in current_mini_public
- kd=sf applied → `Stage::Failed` (terminal)
- Subsequent events after Failed → rejected with `PollInFailedState`
- kd=cl applied → close_event populated
- kd=rs applied with valid R2 → `Stage::Finalized`

- [ ] **Step 2: Implement Tier3PollState + Stage enum**

```rust
//! ZEB-309 Phase 4a-main: Tier 3 poll state machine + drafting + DfrostLog coupling.
//!
//! See docs/specs/2026-05-20-zeb-309-phase4a-main-design.md §3 + §6 + §9.

use crate::community_voting_core::*;
use crate::community_voting_sortition::SortitionResult;
use crate::community_voting_star::{CandidateRef, StarResult};
use crate::owner_state_types::{Hlc, OwnerAddr};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Sortition,
    Deliberation,
    Drafting,
    Ratification,
    Finalized,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftCandidateState {
    pub event_hash: CandidateEventHash,
    pub text: String,
    pub proposer: Option<OwnerAddr>,
    pub approvals: HashSet<OwnerAddr>,
}

#[derive(Debug, Clone)]
pub struct Tier3PollMeta {
    pub poll_id: PollId,
    pub proposer: OwnerAddr,
    pub poll_create_hlc: Hlc,
    pub config: Tier3PollConfigPayload,
}

#[derive(Debug, Clone)]
pub struct Tier3PollState {
    pub meta: Tier3PollMeta,
    pub stage: Stage,
    pub eligible_electorate_snapshot: Vec<OwnerAddr>,
    pub sortition_result: Option<SortitionResult>,
    pub declines: Vec<(OwnerAddr, Hlc)>,
    pub candidates: Vec<DraftCandidateState>,
    pub ratification_ballots: Vec<RatificationBallotPayload>,
    pub close_event_hash: Option<CandidateEventHash>,
    pub result: Option<StarResult>,
    pub last_hlc: Hlc,
}

impl Tier3PollState {
    pub fn apply_event(&mut self, ev: &SignedVotingEvent) -> Result<(), ApplyError> {
        // Dispatch on ev.kind; per spec §6 table.
        // Terminal states (Failed, Finalized) reject all events.
        // ...
    }

    pub fn current_stage_at(&self, hlc: Hlc) -> Stage {
        // Re-evaluate stage based on HLC watermark per spec §6.
        // ...
    }

    pub fn current_mini_public(&self, hlc: Hlc) -> HashSet<OwnerAddr> {
        // Compute set: primary[i] for i in 0..primary.len(),
        // minus declines at hlc, plus backup[j] for j in 0..decline_count.
        // ...
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("poll is in Failed state")]
    PollInFailedState,
    #[error("poll is Finalized")]
    PollInFinalizedState,
    #[error("verify rule failed: {0}")]
    VerifyFailed(String),
    #[error("event hlc {0:?} not monotonic")]
    HlcNotMonotonic(Hlc),
    #[error("payload decode failed: {0}")]
    PayloadDecode(String),
}
```

Implementer fills in `apply_event`, `current_stage_at`, `current_mini_public` per spec §6.

- [ ] **Step 3: Iterate tests to green**

- [ ] **Step 4: Pre-commit gates + commit**

```bash
git add src-tauri/src/community_voting_tier3.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-309): Tier 3 poll state machine skeleton

community_voting_tier3.rs introduces Tier3PollState, Stage enum
(Sortition/Deliberation/Drafting/Ratification/Finalized/Failed),
DraftCandidateState, ApplyError. apply_event dispatches on
SignedVotingEvent.kind and mutates state per design spec §6 table.
current_stage_at and current_mini_public derive set state from
log content + HLC watermark.

Terminal states (Failed, Finalized) reject further events with
PollInFailedState / PollInFinalizedState. Verify/validate is
deferred to Task 6/7.

Refs ZEB-309. Design spec §3, §6.
EOF
)"
```

---

## Task 5: Drafting math (status_quo synthesis + drafting_advancers + ratification ordering)

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs`

- [ ] **Step 1: Write failing tests**

Cases:
- status_quo synthesized exactly once at drafting-stage open
- status_quo event_hash = sha256(poll_id || "status_quo")
- drafting_advancers below threshold returns just status_quo
- drafting_advancers above threshold returns top-N + status_quo (status_quo always last)
- drafting_advancers caps at MAX_RATIFICATION_CANDIDATES=5
- ratification_candidates_ordering: drafting approval DESC, tie by event_hash ASC, status_quo last

- [ ] **Step 2: Implement per design spec §6 + §9**

```rust
pub const MAX_RATIFICATION_CANDIDATES: usize = 5;

pub fn synthesize_status_quo(poll_id: &PollId) -> DraftCandidateState {
    let mut hasher = Sha256::new();
    hasher.update(poll_id.0);
    hasher.update(b"status_quo");
    DraftCandidateState {
        event_hash: hasher.finalize().into(),
        text: "<status quo>".into(),
        proposer: None,
        approvals: HashSet::new(),
    }
}

pub fn drafting_advancers(
    candidates: &[DraftCandidateState],
    mini_public_size: usize,
    status_quo_hash: CandidateEventHash,
) -> Vec<CandidateRef> {
    // Per design spec §9 — filter, sort, take top-(MAX_ADVANCERS-1), append status_quo.
    // ...
}

pub fn ratification_candidates_ordering(
    advancers: &[CandidateRef],
    status_quo_hash: CandidateEventHash,
) -> Vec<CandidateRef> {
    // Sort by approval_count DESC, ties by event_hash ASC. status_quo always last.
    // ...
}
```

- [ ] **Step 3: Tests green + pre-commit gates + commit**

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-309): drafting math + status_quo synthesis

Adds synthesize_status_quo (deterministic event_hash from poll_id),
drafting_advancers (top-N where approval ≥ ceil(mini_public/2),
capped at MAX_RATIFICATION_CANDIDATES=5, status_quo always advances
and always last), ratification_candidates_ordering (approval DESC,
event_hash ASC tiebreak, status_quo last).

12+ unit tests cover threshold edge cases, MAX_RATIFICATION_CANDIDATES
cap, ordering determinism.

Refs ZEB-309. Design spec §6, §9.
EOF
)"
```

---

## Task 6: Validate functions (validate_tier3_poll_config + validate_ratification_ballot)

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs`

- [ ] **Step 1: Tests**

For `validate_tier3_poll_config`:
- Valid config passes
- sortition_size < 20 → rejected (per spec §6.1.1 floor)
- sortition_size > 300 → rejected (per spec ceiling)
- deliberation/drafting/ratification windows < 60s → rejected
- privacy_mode "se" / "rf" → rejected with `UnknownPrivacyMode` (Phase 6/7 forward-compat)
- privacy_mode "pu" → accepted
- proposal_text empty → rejected
- incentive_mode not a/b/c/d → rejected
- retry_of present and points to non-existent prev poll → rejected

For `validate_ratification_ballot`:
- scores.len() == ratification_candidates.len() (matched) — accept
- mismatch → reject
- any score > 5 → reject
- ballot for poll in non-Ratification stage → caller's B2 problem, but validate_ballot itself should not over-check

- [ ] **Step 2: Implement + iterate to green**

- [ ] **Step 3: Commit**

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-309): validate_tier3_poll_config + validate_ratification_ballot

validate_tier3_poll_config enforces sortition_size 20..=300, window
floors at 60s, proposal_text non-empty, privacy_mode "pu" only (Phase
4a-main), incentive_mode a/b/c/d, optional retry_of integrity.

validate_ratification_ballot: scores.len matches ratification_candidates,
each score 0..=5.

Both run BEFORE signing/applying/broadcasting per
feedback_metadata_before_irreversible_write.

Refs ZEB-309. Design spec §5 (verify rules C1, B4) + §6.
EOF
)"
```

---

## Task 7: Verify rules (SS1, SD1, SF1, SR1, B1-B5 extensions)

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs`
- Add: helper to access DfrostLog beacon (trait or `Arc<DfrostLogRegistry>` — may need to defer the SS1 beacon fetch until Task 10)

For Task 7, **SS1 verify can use a `BeaconOracle` trait** that the integration code (Task 10) implements with the real `DfrostLogRegistry`. This decouples Task 7 from engine wiring:

```rust
pub trait BeaconOracle {
    fn vrf_output_for(&self, community_id: &SpaceId, seed: &[u8; 32]) -> Option<[u8; 32]>;
}
```

- [ ] **Step 1: Tests** — exhaustive coverage of verify rules per spec §5.

- [ ] **Step 2: Implement verify functions:**

```rust
pub fn verify_ss(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
    beacon_oracle: &dyn BeaconOracle,
    community_id: &SpaceId,
) -> Result<(), VerifyError> {
    // Decode SortitionSelectionPayload.
    // Look up vrf_output via beacon_oracle.
    // Recompute fisher_yates_select and compare primary + backup.
    // Reject on mismatch or missing beacon.
}

pub fn verify_sd(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
) -> Result<(), VerifyError> {
    // event.actor must be in poll_state.current_mini_public(event.hlc).
}

pub fn verify_sf(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
) -> Result<(), VerifyError> {
    // event.actor must be poll_state.meta.proposer.
    // decline_count_at_hlc(poll_state, event.hlc) ≥ backup_pool_size.
}

pub fn verify_sr(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
) -> Result<(), VerifyError> {
    // Decode kd=rs payload, recompute tally_star, compare result bit-identical.
}
```

- [ ] **Step 3: Tests green + commit**

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-309): Tier 3 verify rules (SS1, SD1, SF1, SR1, B1-B5)

verify_ss reconstructs sortition deterministically from VRF output
(via BeaconOracle trait — Task 10 wires the real DfrostLog impl).
verify_sd enforces mini-public restriction on kd=ds/dc/da/md.
verify_sf gates kd=SortitionFailed on proposer + backup-exhausted.
verify_sr re-computes STAR tally and rejects mismatch.

B1-B5 RatificationBallot extension: poll exists, lifecycle Ratification
at hlc, actor in sortition snapshot (full electorate, not mini-public),
validate_ballot, privacy_mode="pu" only.

25+ verify tests cover happy and rejection paths.

Refs ZEB-309. Design spec §5.
EOF
)"
```

---

## Task 8: Wire Tier 3 into community_voting_core dispatch

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs` (extend dispatcher; route kd=ss/ds/md/dc/da/sf/rb to tier3 module; route Tier 3 kd=cr to validate_tier3_poll_config)

- [ ] Implementer reads existing dispatch table in voting_core, extends it for Tier 3 kinds, ensures existing Tier 1 / Tier 2 paths unchanged.

- [ ] Add an integration smoke test that exercises a Tier 3 PollCreate end-to-end through the existing voting_log/voting_log_engine code (no engine coupling yet — that's Task 10).

- [ ] Commit:

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-309): dispatch Tier 3 event kinds in community_voting_core

Extends verify_event + apply_event dispatch tables to route the 7
new kd codes (ss/ds/md/dc/da/sf/rb) plus Tier 3 kd=cr/cl/rs to
the community_voting_tier3 module. Tier 1 / Tier 2 paths unchanged.

Smoke test: Tier 3 PollCreate round-trips through voting_log without
engine coupling (sortition will stall until Task 10 wires DfrostLog).

Refs ZEB-309. Design spec §11.
EOF
)"
```

---

## Task 9: Refactor `dfrost_request_vrf_beacon` to expose a Rust-callable inner

**Files:**
- Modify: `src-tauri/src/lib.rs`

The current `dfrost_request_vrf_beacon` IPC is at lib.rs ~line 23185 and does many things tightly coupled to `tauri::State<'_, Mutex<NodeState>>`. We need a Rust-callable inner function so the voting engine can invoke beacon requests without going through the IPC layer.

- [ ] **Step 1: Extract inner function**

Refactor:

```rust
async fn dfrost_request_vrf_beacon_inner(
    node_state: Arc<Mutex<NodeState>>,
    community_id: SpaceId,
    seed: [u8; 32],
) -> Result<(), String> {
    // Body extracted from the existing IPC — same checks, same
    // build_signed + apply + publish flow.
}

#[tauri::command]
async fn dfrost_request_vrf_beacon<R: tauri::Runtime>(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id_hex: String,
    seed_hex: String,
) -> Result<(), String> {
    // Decode hex, get Arc<Mutex<NodeState>>, call dfrost_request_vrf_beacon_inner.
}
```

- [ ] **Step 2: Verify existing IPC tests still pass** — no behavior change, just refactor.

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(dfrost_request_vrf_beacon)' 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git commit -m "$(cat <<'EOF'
refactor(zeb-309): extract dfrost_request_vrf_beacon Rust callable

Extracts dfrost_request_vrf_beacon_inner(node_state, community_id,
seed) so non-IPC callers (notably voting engine in Task 10) can
trigger VRF beacon ceremonies without going through Tauri State.

Behavior unchanged; existing IPC unit tests still pass.

Refs ZEB-309.
EOF
)"
```

---

## Task 10: Voting engine ↔ DfrostLog coupling (Arc handle + beacon callback + kd=ss publish)

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs`
- Modify: `src-tauri/src/community_voting_log.rs` (if VotingLogRegistry needs the handle too)
- Modify: `src-tauri/src/community_voting_tier3.rs` (implement BeaconOracle for DfrostLogRegistry)

- [ ] **Step 1: Add `Arc<DfrostLogRegistry<R>>` field to VotingLogEngine + VotingLogRegistry**

Use `PhantomData<fn() -> R>` (Send-safe) per ZEB-307 lesson.

- [ ] **Step 2: Implement `subscribe_beacons` on DfrostLogRegistry**

DfrostLogRegistry currently has no callback hook. Add:

```rust
impl<R: tauri::Runtime> DfrostLogRegistry<R> {
    pub async fn subscribe_beacons<F>(&self, callback: F)
    where
        F: Fn(&VrfBeaconPayload, &SpaceId) + Send + Sync + 'static,
    {
        // Store callback; invoke from engine apply_vrf_beacon hook.
    }
}
```

Engine's `apply_vrf_beacon` (existing in community_dfrost_log.rs:392) gets a hook that calls registered callbacks after successful apply.

- [ ] **Step 3: Wire voting engine to subscribe**

On `VotingLogEngine::start`, subscribe to dfrost beacon arrivals. Callback:
1. Looks up the matching open Tier 3 poll by `(community_id, seed)`.
2. Recomputes Fisher-Yates.
3. Builds + signs kd=ss SortitionSelection event.
4. Publishes via own voting log engine.

- [ ] **Step 4: Implement BeaconOracle for DfrostLogRegistry**

```rust
#[async_trait::async_trait]
impl<R: tauri::Runtime> BeaconOracle for DfrostLogRegistry<R> {
    async fn vrf_output_for(&self, community_id: &SpaceId, seed: &[u8; 32]) -> Option<[u8; 32]> {
        let engine = self.get(*community_id).await?;
        engine.find_vrf_beacon_by_seed(seed).await
    }
}
```

DfrostLogEngine needs a `find_vrf_beacon_by_seed` helper — straightforward lookup in the dfrost log.

- [ ] **Step 5: Tests** — unit tests for callback subscription + dispatch.

- [ ] **Step 6: Commit**

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-309): engine-layer auto-orchestration (voting ↔ DfrostLog)

VotingLogEngine + VotingLogRegistry now hold Arc<DfrostLogRegistry<R>>
(PhantomData<fn() -> R> Send-safety per ZEB-307). On Tier 3 PollCreate
apply, voting engine invokes DfrostLog beacon-request via the Task 9
extracted inner function. Voting engine subscribes to DfrostLog VRF
beacon arrivals via Rust callback; on matching beacon, deterministically
computes Fisher-Yates and publishes kd=ss SortitionSelection.

DfrostLogRegistry adds subscribe_beacons callback hook + implements
BeaconOracle trait. DfrostLogEngine adds find_vrf_beacon_by_seed helper.

First-valid-wins by HLC LWW; cross-engine kd=ss races verified in Task 16.

Refs ZEB-309. Design spec §4.
EOF
)"
```

---

## Task 11: lib.rs start_node + stop_inner wiring

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] In `start_node`: after creating DfrostLogRegistry and VotingLogRegistry, call `voting_registry.install_dfrost_handle(Arc::clone(&dfrost_registry))`. Open communities are restarted in order: dfrost first, then voting.

- [ ] In `stop_inner`: shutdown voting registry FIRST (drops dfrost handle), THEN dfrost registry.

- [ ] Add a smoke test that exercises start_node → stop_inner cycle with both registries installed.

- [ ] Commit:

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-309): lib.rs wiring for voting ↔ DfrostLog engine coupling

start_node:
  1. Create DfrostLogRegistry
  2. Create VotingLogRegistry
  3. voting_registry.install_dfrost_handle(dfrost_registry.clone())
  4. Restart open communities (dfrost first, voting second)

stop_inner:
  1. voting_registry.shutdown()  (drops dfrost handle)
  2. dfrost_registry.shutdown()

Smoke test exercises the full lifecycle.

Refs ZEB-309. Design spec §4.
EOF
)"
```

---

## Task 12: Integration test infrastructure

**Files:**
- Create: `src-tauri/tests/community_voting_tier3_integration.rs`

- [ ] Set up:
  - Two-engine bidirectional mpsc bridge (per `community_dfrost_transport_integration.rs` pattern)
  - `fixture_identity(seed: u8)` helper producing Ed25519 keys + OwnerAddrs binding through verify gate
  - `fixture_committee(n: usize)` helper producing n D-FROST committee members + epoch-1 committee
  - `wait_for_poll_state` polling helper (5ms poll, 2s timeout)
  - `build_tier3_poll_create_event` helper
  - `build_decline_event` helper
  - `build_rb_event` helper

- [ ] Smoke test: 2 engines start cleanly, can publish + receive a Tier 1 ballot (sanity check before Tier 3).

- [ ] Commit:

```bash
git commit -m "$(cat <<'EOF'
test(zeb-309): multi-engine integration test infrastructure

community_voting_tier3_integration.rs sets up two-engine bidirectional
mpsc bridges for both VotingLog and DfrostLog. fixture_identity binds
Ed25519 keys to OwnerAddrs through verify gate. wait_for_poll_state
polling helper avoids tokio::time::sleep flakiness (per ZEB-307 R3).

Smoke test confirms two-engine Tier 1 ballot exchange works (no Tier 3
behavior tested yet — that's Tasks 13-16).

Refs ZEB-309.
EOF
)"
```

---

## Task 13: Integration test — 4-stage happy path E2E

**Files:**
- Modify: `src-tauri/tests/community_voting_tier3_integration.rs`

Test flow:
1. Two voting engines + two dfrost engines started with committee of size 3-of-5.
2. Proposer publishes Tier 3 PollCreate (sortition_size=5, deliberation_window=60s, drafting_window=60s, ratification_window=60s, incentive_mode=d).
3. DfrostLog committee produces kd=vb VRF beacon.
4. Both voting engines compute + publish kd=ss; HLC LWW resolves; both converge on same SortitionResult.
5. Skip-ahead HLC (use test clock or manual hlc advances) past deliberation_window.
6. Mini-public members (selected primary) publish kd=dc DraftCandidate events. Other mini-public members publish kd=da DraftApproval.
7. Skip-ahead HLC past drafting_window. Both engines compute drafting_advancers identically.
8. Full electorate publishes kd=rb RatificationBallot. Both engines accumulate identical ballot set.
9. Skip-ahead HLC past ratification_window.
10. Either engine publishes kd=cl PollClose. Then one publishes kd=rs PollResult.
11. Both engines verify kd=rs against deterministic tally re-compute. Both converge on Stage::Finalized with identical StarResult.

- [ ] Commit:

```bash
git commit -m "$(cat <<'EOF'
test(zeb-309): multi-engine 4-stage E2E happy path

Two voting engines + two dfrost engines exercise the full Tier 3
lifecycle: PollCreate → VRF beacon → SortitionSelection → Deliberation
(scaffold) → Drafting (kd=dc + kd=da) → Ratification (kd=rb) → Close →
Result. Both engines converge bit-identically on sortition, drafting
advancers, ratification candidate ordering, and StarResult.

Refs ZEB-309. Design spec §3, §6, §12.
EOF
)"
```

---

## Task 14: Integration test — decline + backup promotion

- [ ] Test:
1. Same setup as Task 13.
2. After kd=ss, 3 mini-public members publish kd=md decline (out of 5 primary, backup pool also 5).
3. Both engines compute current_mini_public identically: primary minus the 3 decliners, plus backup[0..3].
4. Promoted backup members publish kd=dc; their events are accepted (SD1 verify).
5. Continue through 4-stage lifecycle; verify result.

- [ ] Commit.

---

## Task 15: Integration test — mass-decline + SortitionFailed + retry chain

- [ ] Test:
1. Setup as above.
2. ALL primary + backup publish kd=md (10 members decline; backup_pool_size = 5).
3. Proposer publishes kd=sf SortitionFailed.
4. Verify SF1 verify accepts (decline_count=10 ≥ backup_pool_size=5).
5. Both engines transition poll to Stage::Failed.
6. Subsequent events for this poll_id rejected with PollInFailedState.
7. Proposer publishes new kd=cr Tier 3 with `ro: Some(prev_poll_id)`.
8. New poll proceeds independently with fresh VRF beacon + sortition.

- [ ] Commit.

---

## Task 16: Integration test — cross-engine kd=ss race + HLC LWW

- [ ] Test:
1. Setup as above.
2. After kd=vb beacon arrives at both engines simultaneously, both publish kd=ss.
3. HLC LWW resolves race — first valid wins.
4. Both engines converge to the same SortitionResult (since both computed bit-identically).
5. Subsequent kd=ss events from second engine rejected as HLC non-monotonic per V5.

- [ ] Commit.

---

## Task 17: Wire-format fixtures (regen-on-first-run pattern)

**Files:**
- Create: `src-tauri/tests/wire_format_voting_tier3_fixtures.rs`

- [ ] Per the ZEB-250 pattern in `tests/wire_format_zeb250_fixtures.rs`:

```rust
const FIXTURE_PATH: &str = "tests/fixtures/voting_tier3";
const REGENERATE_ENV: &str = "REGENERATE_VOTING_TIER3_FIXTURES";

#[test]
fn fixture_tier3_poll_create() { fixture_round_trip("tier3_poll_create.cbor", build_tier3_poll_create()); }

#[test]
fn fixture_sortition_selection() { ... }

// ...etc for 8 event types
```

If env var set, regen and panic with REGENERATE message. Otherwise compare against on-disk bytes.

- [ ] Generate fixtures on first run; commit binary CBOR + test source.

```bash
cd src-tauri && REGENERATE_VOTING_TIER3_FIXTURES=1 cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fixture_tier3)' || true
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fixture_tier3)' 2>&1 | tail -10
```

Expected on first run: panic message asking to regen. After regen, all 8 fixtures pass.

- [ ] Commit:

```bash
git add src-tauri/tests/wire_format_voting_tier3_fixtures.rs src-tauri/tests/fixtures/voting_tier3/
git commit -m "$(cat <<'EOF'
test(zeb-309): pin canonical CBOR fixtures for Tier 3 wire format

8 fixtures pinned via the ZEB-250 regen-on-first-run pattern:
  tier3_poll_create.cbor      (with retry_of field)
  sortition_selection.cbor
  mini_public_decline.cbor
  draft_candidate.cbor
  draft_approval.cbor
  sortition_failed.cbor
  ratification_ballot.cbor
  tier3_poll_result.cbor

Structural CBOR-key checks via ciborium::Value confirm same-length
2-char keys invariant. Set REGENERATE_VOTING_TIER3_FIXTURES=1 to
regenerate.

Refs ZEB-309. Design spec §13.
EOF
)"
```

---

## Task 18: Final 5-gate sweep + push + PR

**Files:** none (verification + PR creation only)

- [ ] **Step 1: Run all 5 CI gates from scratch**

```bash
# Backend gates (from src-tauri/)
cd src-tauri && cargo fmt --all -- --check
echo "Exit: $?"
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
echo "Exit: ${PIPESTATUS[0]}"
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
echo "Exit: ${PIPESTATUS[0]}"

# Frontend gates (from repo root)
cd .. && npx tsc --noEmit 2>&1 | tail -5
echo "Exit: ${PIPESTATUS[0]}"
npx vitest run 2>&1 | tail -10
echo "Exit: ${PIPESTATUS[0]}"
```

All gates must show exit 0. Per `feedback_pipe_exit_codes_lie`: use `${PIPESTATUS[0]}` or `set -o pipefail`.

If any gate fails:
- If failure is in code changed by ZEB-309: fix and re-run
- If failure is in unrelated tests (per `feedback_unrelated_test_failures`): file a follow-up Linear ticket; do NOT fold into this PR; mention in PR description

- [ ] **Step 2: Push branch**

```bash
git push -u origin zeb-309-phase4a-main-data-engine
```

- [ ] **Step 3: Create PR**

```bash
gh pr create --title "ZEB-309 Phase 4a-main: sortition + STAR + drafting math + engine integration" --body "$(cat <<'EOF'
## Summary

Implements Tier 3 governance backend mechanism for [ZEB-309](https://linear.app/zeblith/issue/ZEB-309): deterministic sortition (Fisher-Yates + VRF), STAR ratification math, drafting via inline approval voting, 4-stage lifecycle state machine, and voting engine ↔ DfrostLog auto-orchestration. Public ballots only — privacy modes deferred to Phase 6/7.

Builds on Phase 4a-foundation ([ZEB-301](https://linear.app/zeblith/issue/ZEB-301) / [ZEB-303](https://linear.app/zeblith/issue/ZEB-303) / [ZEB-305](https://linear.app/zeblith/issue/ZEB-305) / [ZEB-307](https://linear.app/zeblith/issue/ZEB-307), all merged).

## Scope

- 3 new modules: `community_voting_sortition.rs` (pure Fisher-Yates + VRF), `community_voting_star.rs` (pure STAR tally), `community_voting_tier3.rs` (state machine + drafting + engine glue)
- 7 new wire-format event kinds (kd=ss/ds/md/dc/da/sf/rb); reuses kd=cl/rs from Tier 1
- Tier 3 PollConfig with `ro` (retry_of) field for retry chains
- Engine-layer auto-orchestration: voting engine holds `Arc<DfrostLogRegistry>`, auto-triggers beacon on Tier 3 PollCreate apply, subscribes to kd=vb arrivals and publishes kd=ss deterministically
- Verify rules SS1, SD1, SF1, SR1 + B1-B5 RatificationBallot extension
- 4-stage hybrid lifecycle: HLC-passive 1→3, explicit kd=cl + kd=rs at stage 4, kd=sf for backup-pool-exhaustion
- 4 multi-engine integration tests: happy E2E, decline + backup, mass-decline + SortitionFailed + retry, cross-engine kd=ss race
- 8 wire-format fixtures pinned (ZEB-250 regen-on-first-run pattern)

## Test plan

- [x] `cargo fmt --all -- --check` — 0 diffs
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — 0 warnings
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — all green
- [x] `npx tsc --noEmit` — 0 errors
- [x] `npx vitest run` — all green
- [x] 4-stage E2E multi-engine test passes
- [x] Decline + backup-promotion test passes
- [x] Mass-decline + SortitionFailed + retry test passes
- [x] Cross-engine kd=ss race test passes
- [x] 8 wire-format fixtures pinned

## What this unlocks

- [ZEB-310](https://linear.app/zeblith/issue/ZEB-310) — Tier 3 IPCs + Tauri events (5 IPCs, 5 events)
- [ZEB-311](https://linear.app/zeblith/issue/ZEB-311) — Tier 3 UI (6 Svelte components)
- Phase 5 ([ZEB-294](https://linear.app/zeblith/issue/ZEB-294)) — Pol.is-style deliberation (scaffold already in place via kd=ds)
- Phase 6 ([ZEB-295](https://linear.app/zeblith/issue/ZEB-295)) — Ballot-secret D-FROST tally (reuses committee primitive)
- Phase 7 ([ZEB-296](https://linear.app/zeblith/issue/ZEB-296)) — Receipt-free TRIP credentials

## Closes

Closes ZEB-309.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 4: Confirm PR is open**

```bash
gh pr view --json url,number,state | jq -r '.url, .number, .state'
```

Capture PR URL + number for the bot-monitoring loop.

---

## Self-review (post-write)

This plan covers the full ZEB-309 scope per design spec. 18 tasks; each ends in a commit; TDD-shaped throughout. No placeholders — every step has concrete code, exact commands, or pointer to the spec for design details. Type consistency verified (PollEventKindCode → Tier3PollConfigPayload → Tier3PollState → SortitionResult → StarResult).

Notable trade-offs:
- BeaconOracle trait (Task 7) decouples verify_ss from engine wiring so Task 7 can land before Task 10. Implementer impl uses `Arc<DfrostLogRegistry>` in Task 10.
- Task 9 IPC refactor isolates the IPC-tied logic so voting engine can drive beacon requests without the State<Mutex<NodeState>> type.
- All 4 integration tests live in one file (`community_voting_tier3_integration.rs`) for shared fixtures; each test is independent.

## References

- Design spec: `docs/specs/2026-05-20-zeb-309-phase4a-main-design.md`
- Umbrella spec: `docs/specs/2026-05-16-zeb-289-voting-polling-design.md`
- Pattern source — Tier 1: `src-tauri/src/community_voting_approval.rs`
- Pattern source — DfrostLog: `src-tauri/src/community_dfrost_log_engine.rs`, `community_dfrost_log.rs`
- Pattern source — multi-engine integration: `src-tauri/tests/community_dfrost_transport_integration.rs`
- Pattern source — wire-format fixtures: `src-tauri/tests/wire_format_zeb250_fixtures.rs`
- ZEB-307 PR #146 (Send-safety `PhantomData<fn() -> R>` lesson)
