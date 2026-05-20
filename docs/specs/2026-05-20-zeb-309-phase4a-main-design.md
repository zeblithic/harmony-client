# ZEB-293 Phase 4a-main Design: Sortition + STAR Ratification (Public Ballots)

**Status:** Brainstormed 2026-05-20; refines [ZEB-289 umbrella spec](2026-05-16-zeb-289-voting-polling-design.md) §6.
**Tickets:** [ZEB-309](https://linear.app/zeblith/issue/ZEB-309) (this PR — data + engine), [ZEB-310](https://linear.app/zeblith/issue/ZEB-310) (IPCs + events), [ZEB-311](https://linear.app/zeblith/issue/ZEB-311) (UI).
**Foundation:** Phase 4a-foundation ([ZEB-301](https://linear.app/zeblith/issue/ZEB-301) / [ZEB-303](https://linear.app/zeblith/issue/ZEB-303) / [ZEB-305](https://linear.app/zeblith/issue/ZEB-305) / [ZEB-307](https://linear.app/zeblith/issue/ZEB-307)) — D-FROST committee + Zenoh transport — merged.

## Summary

This document settles the *implementation-level* open questions for Tier 3 (sortition + STAR ratification with PUBLIC ballots only). The umbrella spec settled the mechanism (Fisher-Yates VRF sortition, STAR runoff math, 4-stage lifecycle, verify rules SS1/SD1/B1-B5). This refinement settles the *event-kind taxonomy*, *engine orchestration*, *materialize state machine*, *drafting math*, *retry semantics*, and *module factoring* — everything plan-writers and implementer subagents need to dispatch work.

Privacy modes (D-FROST ballot-secret in Phase 6, TRIP receipt-free in Phase 7) are *deliberately deferred*. The wire format is designed additive — Phase 6 will extend `kd=rb` with a privacy_mode field and add `kd=ts` TallyShare; no breaking changes.

## 1. Goals & non-goals

### Goals

1. **End-to-end Tier 3 governance lands across 3 PRs** — backend mechanism + math + engine integration ([ZEB-309](https://linear.app/zeblith/issue/ZEB-309)); IPCs + Tauri events ([ZEB-310](https://linear.app/zeblith/issue/ZEB-310)); UI components ([ZEB-311](https://linear.app/zeblith/issue/ZEB-311)).
2. **Determinism across all nodes** — sortition selection (Fisher-Yates from VRF), drafting advancers, STAR runoff result all bit-identical regardless of node ordering, partition heal timing, or who first publishes optimistic events.
3. **Censorship-resistance** — anyone with the log can recompute kd=ss (SS1 verify) and kd=rs (R2 verify); no single signer bottleneck.
4. **Engine orchestration in Rust** — voting engine ↔ DfrostLog coupling at the Rust layer; works whether frontend is open or closed; survives the same Wry-on-MacOS Send constraints we fixed in [ZEB-307](https://linear.app/zeblith/issue/ZEB-307).
5. **Backwards-compat with Phase 1 / Phase 4a-foundation** — additive wire-format additions only; existing Tier 1 / Tier 2 / DfrostLog code paths unchanged.

### Non-goals (this phase)

1. [Pol.is](http://Pol.is)-style deliberation algorithm — Phase 5.
2. Ballot-secret encryption (D-FROST threshold-decryption of ballots) — Phase 6.
3. Receipt-free TRIP credentials — Phase 7.
4. Auto-exec for Tier 3 (always `none` — admin enacts constitutional decisions manually).
5. Dynamic sortition size (`SqrtN` formula) — fixed default 100 in this phase.
6. AutoPowerBoost cross-CRDT enactment — voting state exposes 'serving' set, but power-CRDT writes deferred to a future ticket.

## 2. Wire format extensions

All additions follow the same-length 2-char-keys invariant from spec §3.

### New event kinds (`kd` codes)

| `kd` | Name | Payload (`pd`) | Notes |
|---|---|---|---|
| `ss` | SortitionSelection | `{ pi: PollId, pr: [OwnerAddr; sortition_size], bk: [OwnerAddr; sortition_size] }` | First valid wins by HLC LWW. SS1 verify recomputes Fisher-Yates and rejects mismatch. |
| `ds` | DeliberationStatement | `{ pi: PollId, tx: String (≤280) }` | Scaffold for Phase 5. SD1 verify gates on mini-public membership. No clustering in this phase. |
| `md` | MiniPublicDecline | `{ pi: PollId, rs: Option<String> (≤2 chars) }` | SD1 verify (actor must be in mini-public). |
| `dc` | DraftCandidate | `{ pi: PollId, tx: String (≤512) }` | Publisher implicitly approves own candidate. SD1 verify. |
| `da` | DraftApproval | `{ pi: PollId, ch: CandidateEventHash }` | SD1 verify. |
| `sf` | SortitionFailed | `{ pi: PollId }` | Proposer-signed. Deterministic gate: `decline_count_at_hlc ≥ backup_pool_size`. |
| `rb` | RatificationBallot | `{ pi: PollId, sc: Vec<u8> (each 0..=5) }` | B1-B5 verify. Length matches `ratification_candidates.len()`. |

### Reused event kinds (verbatim from Tier 1)

| `kd` | Name | Phase 4a-main usage |
|---|---|---|
| `cr` | PollCreate | Tier 3 PollConfig payload (see §2.1); auto-exec always `none`. |
| `cl` | PollClose | Closes Stage 4 ratification window; L1+L2 verify. |
| `rs` | PollResult | Anyone signs once tally is bit-identical to deterministic re-compute over kd=rb events present at hlc; R1+R2 verify. |

### Tier 3 PollConfig (`pd` for `kd="cr"`, Tier 3)

Extends spec §6.0 with `ro` (retry_of):

```text
{
  "pt": "Amend charter §3...",  # proposal_text
  "ss": 100,                      # sortition_size
  "dw": 1209600,                  # deliberation_window seconds (14d default)
  "fw": 604800,                   # drafting_window seconds (7d default)
  "rw": 1209600,                  # ratification_window seconds (14d default)
  "pm": "pu",                     # privacy_mode: "pu"=public (Phase 4a-main),
                                  #               "se"=ballot-secret (Phase 6),
                                  #               "rf"=receipt-free (Phase 7)
  "im": "d",                      # incentive_mode: a/b/c/d per §6.1.2
  "ro": Option<PollId>            # retry_of — links to predecessor poll if this is a retry
}
```

`pm="pu"` is the only privacy_mode accepted in this phase; verify rule rejects `pm="se"` or `pm="rf"` with `UnknownPrivacyMode` (fail-soft to forward-compat for Phase 6/7).

## 3. Four-stage lifecycle state machine

```text
Stage 1: Proposal + Sortition Selection
  ┌─ Triggered by: kd=cr Tier 3 publish
  ├─ Engine auto-orchestration:
  │    voting engine apply(kd=cr) → DfrostLog::request_beacon
  │    → committee threshold-VRF → kd=vb in DfrostLog
  │    → voting engine on_beacon_callback → publish kd=ss
  ├─ Transition to Stage 2: kd=ss apply
  └─ Failure branch: kd=sf valid when decline_count ≥ backup_pool_size

Stage 2: Deliberation (mini-public only)
  ┌─ Mini-public publishes kd=ds (scaffold — Phase 5 wires clustering)
  ├─ kd=md (decline) accepted → backup auto-promotion
  └─ Transition to Stage 3: HLC ≥ PollCreate.hlc + deliberation_window

Stage 3: Drafting (mini-public only)
  ┌─ Mini-public publishes kd=dc DraftCandidate (implicit self-approval)
  ├─ Mini-public publishes kd=da DraftApproval for others' candidates
  ├─ Status quo synthesized in materialize() at drafting open
  └─ Transition to Stage 4: HLC ≥ above + drafting_window AND ≥1 DraftCandidate

Stage 4: STAR Ratification (FULL electorate)
  ┌─ Full electorate publishes kd=rb RatificationBallot (scores 0-5 per candidate)
  ├─ Transition to Finalized:
  │    kd=cl PollClose (L2: HLC ≥ above + ratification_window)
  │    then kd=rs PollResult (R2: tally bit-identical to re-compute)
  └─ Anyone can sign kd=rs once tally is deterministic

Failed (terminal — kd=sf was applied)
  ├─ No further events apply to this poll_id
  └─ Retry: proposer files new kd=cr with `ro: Some(prev_poll_id)`
```

### Stage-transition source-of-truth

| Transition | Triggered by | Verify rule |
|---|---|---|
| 1 → 2 | apply of `kd=ss` | SS1 (sortition deterministic re-compute) |
| 1 → Failed | apply of `kd=sf` | gate: `decline_count_at_hlc ≥ backup_pool_size` AND actor == proposer |
| 2 → 3 | materialize at HLC watermark | n/a (passive) |
| 3 → 4 | materialize at HLC watermark AND ≥1 kd=dc applied | n/a (passive) |
| 4 → Finalized | apply of `kd=cl` then `kd=rs` | L1+L2 then R1+R2 |

## 4. Engine orchestration

### voting engine ↔ DfrostLog coupling

```text
src-tauri/src/lib.rs:
  NodeState {
    ...,
    voting_log_registry: Option<Arc<VotingLogRegistry<tauri::Wry>>>,
    dfrost_log_registry: Option<Arc<DfrostLogRegistry<tauri::Wry>>>,
  }

  start_node():
    ...wire both registries...
    voting_registry.set_dfrost_handle(Arc::clone(&dfrost_registry))
```

`VotingLogEngine` holds an `Arc<DfrostLogRegistry<R>>`. On `apply()` of a Tier 3 `kd=cr`:

```rust
// In community_voting_tier3.rs
fn on_apply_tier3_poll_create(
    &self,
    community_id: &SpaceId,
    poll_create: &SignedVotingEvent,
) -> Result<(), ApplyError> {
    let seed = derive_beacon_seed(
        &poll_create.event_hash(),
        self.community_epoch(community_id)?,
    );
    self.dfrost_handle
        .request_beacon(community_id, seed)
        .map_err(ApplyError::DfrostBeaconRequestFailed)?;
    Ok(())
}
```

`request_beacon` returns immediately (publishes `kd=dr` Round 0 into DfrostLog; committee asynchronously produces `kd=vb`).

The voting engine subscribes to DfrostLog `kd=vb` arrivals via a callback registered at construction:

```rust
// In community_voting_log_engine.rs
impl<R: tauri::Runtime> VotingLogEngine<R> {
    pub fn install_dfrost_beacon_callback(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.dfrost_handle
            .subscribe_beacons(Box::new(move |beacon: &VrfBeaconEvent| {
                if let Some(engine) = weak.upgrade() {
                    engine.on_dfrost_beacon(beacon);
                }
            }));
    }
}
```

On `on_dfrost_beacon`, the engine:
1. Looks up which open Tier 3 polls have matching `(community_id, seed)`.
2. For each match, computes `SortitionResult` deterministically.
3. Publishes `kd=ss` (first-valid-wins by HLC LWW; SS1 verify rejects mismatch).

**Send-safety**: per [ZEB-307](https://linear.app/zeblith/issue/ZEB-307) lesson, `PhantomData<fn() -> R>` (not `PhantomData<R>`) to avoid inheriting Wry's non-Send-ness.

### Lifecycle (start_node / stop_inner)

Mirror DfrostLog wiring from `lib.rs::start_node()`:

```rust
// Install/shutdown order:
// start_node:
//   1. Create DfrostLogRegistry
//   2. Create VotingLogRegistry  (no dfrost handle yet)
//   3. voting_registry.install_dfrost_handle(dfrost_registry.clone())
//   4. Restart open communities for both registries
//
// stop_inner:
//   1. voting_registry.shutdown()  (drops dfrost handle)
//   2. dfrost_registry.shutdown()
```

## 5. Verify rules (delta from umbrella spec §8)

All generic V1-V6 + C1-C2 rules apply to Tier 3 events.

### New Tier 3 verify rules

**SS1** (SortitionSelection): the `pr` + `bk` arrays in `kd=ss` must be bit-identical to `fisher_yates_select(vrf_output, electorate_snapshot, sortition_size, sortition_size)`. Mismatch → reject with `SortitionMismatch`. The `vrf_output` is fetched from DfrostLog at the corresponding `(community_id, seed)`; if the beacon isn't present at `event.hlc`, reject with `BeaconNotYetAvailable`.

**SD1** (mini-public restriction): for `kd=ds`/`dc`/`da`/`md`, `event.actor` must be in `kd=ss.pr` (primary) OR have been promoted from `kd=ss.bk` via prior `kd=md` events. Materialize maintains a `current_mini_public(poll_id, hlc)` set; SD1 checks set membership.

**B1-B5** (RatificationBallot — extension of Tier 1):
- B1: poll_id exists as Tier 3 `kd=cr` event.
- B2: poll state is Stage 4 (Ratification) at `event.hlc`.
- B3: `event.actor` is in eligible-electorate snapshot at PollCreate.hlc (NOT mini-public — full electorate).
- B4: `validate_ballot(pd, poll_meta)` — `scores.len() == ratification_candidates.len()`; each score in `0..=5`.
- B5: ballot encoding matches `privacy_mode` — for Phase 4a-main, only `pm="pu"` accepted.

**SF1** (SortitionFailed): `event.actor == proposer`; `decline_count_in_log_at_hlc(poll_id, event.hlc) ≥ backup_pool_size`. Otherwise reject with `BackupPoolNotExhausted`.

**SR1** (PollResult, Tier 3 specialization of R2): tally bit-identical to STAR runoff over `kd=rb` events present at `event.hlc`, using deterministic `ratification_candidates` ordering (see §7).

### Forward-compat verify

- `pm="se"` or `pm="rf"` in PollCreate.pd → reject with `UnknownPrivacyMode` (fail-soft per spec §11). Phase 6/7 will lift these to accepted values.
- Unknown event kinds (`kd=??` not in §2) → drop with `UnknownKind` per spec V4.

## 6. Materialize rules

Materialize walks the voting log by `(hlc, event_hash)` lex order. For each Tier 3 poll, materialize maintains:

```rust
pub struct Tier3PollState {
    pub meta: Tier3PollMeta,                       // from kd=cr
    pub stage: Stage,                              // Sortition / Deliberation / Drafting / Ratification / Finalized / Failed
    pub eligible_electorate_snapshot: Vec<OwnerAddr>,  // from PollCreate.hlc
    pub sortition_result: Option<SortitionResult>, // from kd=ss
    pub declines: Vec<(OwnerAddr, Hlc)>,           // from kd=md
    pub current_mini_public: HashSet<OwnerAddr>,   // derived: primary minus declines + backup promotions
    pub candidates: Vec<DraftCandidateState>,      // from kd=dc + status_quo synthesized
    pub approvals: HashMap<CandidateEventHash, HashSet<OwnerAddr>>, // from kd=da + implicit self-approvals
    pub ratification_ballots: Vec<RatificationBallotEvent>,
    pub close_event: Option<SignedPollEvent>,      // from kd=cl
    pub result: Option<StarResult>,                // from kd=rs
}

pub enum Stage {
    Sortition,    // 1 — waiting for kd=ss
    Deliberation, // 2 — HLC ≥ PollCreate.hlc + deliberation_window after kd=ss applied
    Drafting,     // 3 — HLC ≥ above + drafting_window; ≥1 kd=dc
    Ratification, // 4 — HLC ≥ above + drafting_window
    Finalized,
    Failed,
}
```

### Per-event apply rules

| Event | Materialize action |
|---|---|
| `kd=cr` Tier 3 | Create PollState with `stage=Sortition`. Snapshot electorate at HLC. Engine layer triggers DfrostLog beacon request (see §4). |
| `kd=ss` | Validate SS1. If valid: set `sortition_result`. Transition `stage = Deliberation` IFF HLC ≥ PollCreate.hlc; otherwise stage remains `Sortition` until HLC catches up. |
| `kd=md` | Validate SD1. Append to `declines`. If `bk[declines.len()-1]` exists: auto-promote into `current_mini_public`. If `declines.len() ≥ bk.len()`: poll may receive a `kd=sf` (deterministic gate becomes valid). |
| `kd=sf` | Validate SF1. Set `stage = Failed`. No further events apply. |
| `kd=ds` | Validate SD1. Scaffold-only — store statement; no clustering. |
| `kd=dc` | Validate SD1. Append candidate. Implicit self-approval: add `actor` to `approvals[event_hash]`. |
| `kd=da` | Validate SD1. Add `actor` to `approvals[candidate_event_hash]`. |
| `kd=rb` | Validate B1-B5. Append to `ratification_ballots`. |
| `kd=cl` | Validate L1+L2. Set `close_event`. |
| `kd=rs` | Validate R1+SR1. Set `result`. Transition `stage = Finalized`. |

### Stage-transition watermark check

On every apply (and on a periodic tick from `community_voting_tick.rs`), materialize re-evaluates stage transitions for all open Tier 3 polls:

```rust
fn recompute_stage(state: &mut Tier3PollState, now_hlc: Hlc) {
    if matches!(state.stage, Stage::Failed | Stage::Finalized) {
        return; // terminal
    }
    let create_hlc = state.meta.poll_create_hlc;
    let dw = state.meta.deliberation_window;
    let fw = state.meta.drafting_window;
    let stage_2_threshold = create_hlc + dw;
    let stage_3_threshold = stage_2_threshold + fw;
    if state.sortition_result.is_none() {
        return; // still Stage 1
    }
    // sortition done — advance
    if now_hlc < stage_2_threshold {
        state.stage = Stage::Deliberation;
    } else if now_hlc < stage_3_threshold {
        state.stage = Stage::Drafting;
    } else if !state.candidates_for_ratification().is_empty() {
        state.stage = Stage::Ratification;
    } else {
        // No candidates — degenerate path. Status quo always advances per drafting
        // synthesis, so this branch should be unreachable in practice.
        state.stage = Stage::Ratification;
    }
}
```

### Status quo synthesis

On entering Stage 3 (Drafting) for the first time, materialize injects a synthetic status_quo candidate:

```rust
let status_quo_hash = sha256(format!("{}|status_quo", poll_id).as_bytes());
state.candidates.push(DraftCandidateState {
    event_hash: status_quo_hash,
    text: "<status quo>".to_string(),
    proposer: None,  // synthetic
    approvals: HashSet::new(),  // no implicit self-approval
});
```

Status quo is always included in ratification (even if it doesn't meet the approval threshold).

## 7. Sortition algorithm

### VRF seed derivation

```rust
pub fn derive_beacon_seed(poll_create_hash: &[u8; 32], community_epoch: u32) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(poll_create_hash);
    hasher.update(&community_epoch.to_be_bytes());
    hasher.finalize().into()
}
```

The seed is passed to `DfrostLogRegistry::request_beacon`. The D-FROST committee produces a VRF output via threshold-Schnorr; `vrf_output` is a 64-byte (R, s) pair, hashed to 32 bytes for Fisher-Yates seeding.

### Fisher-Yates selection

```rust
pub fn fisher_yates_select(
    vrf_output: &[u8; 32],
    electorate: &[OwnerAddr],
    primary_size: usize,
    backup_size: usize,
) -> SortitionResult {
    let total_size = primary_size + backup_size;
    assert!(electorate.len() >= total_size, "electorate too small");

    let mut shuffled = electorate.to_vec();
    let mut rng = ChaCha20Rng::from_seed(*vrf_output);
    for i in (1..shuffled.len()).rev() {
        let j = rng.gen_range(0..=i);
        shuffled.swap(i, j);
    }

    SortitionResult {
        primary: shuffled[0..primary_size].to_vec(),
        backup: shuffled[primary_size..total_size].to_vec(),
    }
}
```

Note: ChaCha20Rng is deterministic + cross-platform-stable. Fisher-Yates over a sorted-canonicalized electorate guarantees bit-identical output for the same VRF seed.

### Electorate canonicalization

Before Fisher-Yates: sort electorate by `OwnerAddr` byte lex ASC. Ensures the same set of members yields the same shuffle.

## 8. STAR ratification math

```rust
pub struct StarResult {
    pub winner: CandidateRef,
    pub finalists: Vec<CandidateRef>,         // 2 typically; 3+ if score-tie at 2nd
    pub total_scores: Vec<u32>,               // indexed by candidates
    pub runoff_votes: Vec<u32>,               // indexed by finalists
}

pub fn tally_star(
    candidates: &[CandidateRef],
    ballots: &[RatificationBallotEvent],
) -> StarResult {
    let n = candidates.len();
    // Score round
    let mut total_scores = vec![0u32; n];
    for b in ballots {
        for (i, &score) in b.scores.iter().enumerate() {
            total_scores[i] += score as u32;
        }
    }

    // Find top 2 with tie-handling
    let mut indexed: Vec<(usize, u32)> = total_scores.iter().enumerate().map(|(i, &s)| (i, s)).collect();
    indexed.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| candidates[a.0].event_hash.cmp(&candidates[b.0].event_hash))
    });

    let top_score = indexed[0].1;
    let second_score = indexed.get(1).map(|(_, s)| *s).unwrap_or(0);
    let finalists: Vec<usize> = indexed.iter()
        .filter(|(_, s)| *s == top_score || *s == second_score)
        .map(|(i, _)| *i)
        .collect();

    // Runoff round
    let mut runoff_votes = vec![0u32; finalists.len()];
    for b in ballots {
        let scores_for_finalists: Vec<u8> = finalists.iter().map(|&i| b.scores[i]).collect();
        let max = *scores_for_finalists.iter().max().unwrap_or(&0);
        let winners: Vec<usize> = scores_for_finalists.iter().enumerate()
            .filter(|(_, &s)| s == max)
            .map(|(i, _)| i)
            .collect();
        if winners.len() == 1 {
            runoff_votes[winners[0]] += 1;
        }
        // else: abstain (equal-score finalists)
    }

    // Winner = max runoff_votes, tiebreaker = max total_score, then candidate_event_hash lex
    let winner_idx_in_finalists = (0..finalists.len()).max_by(|&a, &b| {
        runoff_votes[a].cmp(&runoff_votes[b])
            .then_with(|| total_scores[finalists[a]].cmp(&total_scores[finalists[b]]))
            .then_with(|| candidates[finalists[b]].event_hash.cmp(&candidates[finalists[a]].event_hash))
    }).unwrap();

    StarResult {
        winner: candidates[finalists[winner_idx_in_finalists]].clone(),
        finalists: finalists.iter().map(|&i| candidates[i].clone()).collect(),
        total_scores,
        runoff_votes,
    }
}
```

### Test cases (unit)

- **Baseline 3-candidate happy path**: A=10/10, B=8/10, C=5/10 → finalists [A, B]; runoff A wins.
- **Score-round tie at 2nd-place**: A=20, B=15, C=15 → 3-way runoff [A, B, C].
- **Runoff-equal-score abstention**: ballot scores [5, 5, 0] on 3-finalist runoff → no vote.
- **Runoff tie → total_score tiebreak**: 2 finalists tied on runoff, A has higher total → A wins.
- **Total-score tie → event_hash tiebreak**: both finalists fully tied → lex ASC on event_hash.
- **Empty ballots**: zero kd=rb events → status_quo wins by default (only candidate with non-zero total once filtered).

## 9. Drafting algorithm

### Approval counting

```rust
pub fn drafting_advancers(
    candidates: &[DraftCandidateState],   // includes synthesized status_quo
    mini_public_size: usize,
    status_quo_hash: CandidateEventHash,
) -> Vec<CandidateRef> {
    const MAX_ADVANCERS: usize = 5;
    let threshold = (mini_public_size + 1) / 2;  // ceil(N/2) — "at least half"

    // Step 1: filter non-status-quo candidates by threshold.
    let mut threshold_passers: Vec<&DraftCandidateState> = candidates.iter()
        .filter(|c| c.event_hash != status_quo_hash)
        .filter(|c| c.approvals.len() >= threshold)
        .collect();

    // Step 2: sort by approval_count DESC, ties by event_hash ASC.
    threshold_passers.sort_by(|a, b| {
        b.approvals.len().cmp(&a.approvals.len())
            .then_with(|| a.event_hash.cmp(&b.event_hash))
    });

    // Step 3: take top (MAX_ADVANCERS - 1) — leave room for guaranteed status_quo.
    let mut advancers: Vec<CandidateRef> = threshold_passers.into_iter()
        .take(MAX_ADVANCERS - 1)
        .map(|c| c.to_candidate_ref())
        .collect();

    // Step 4: status_quo always advances, always last.
    let status_quo = candidates.iter()
        .find(|c| c.event_hash == status_quo_hash)
        .expect("materialize() guarantees status_quo synthesis at drafting open");
    advancers.push(status_quo.to_candidate_ref());

    advancers
}
```

### Ratification candidate ordering

The order in which candidates appear in the `kd=rb` `scores` array MUST be deterministic:

```rust
pub fn ratification_candidates_ordering(
    advancers: &[CandidateRef],
) -> Vec<CandidateRef> {
    let mut ordered = advancers.to_vec();
    // Sort by approval_count DESC, ties by event_hash ASC; status quo's approval = 0 → last
    ordered.sort_by(|a, b| {
        b.approval_count.cmp(&a.approval_count)
            .then_with(|| a.event_hash.cmp(&b.event_hash))
    });
    ordered
}
```

This ordering is part of `Tier3PollState` and locked at Stage 3 → Stage 4 transition (so all kd=rb ballots reference the same ordering).

## 10. Failure modes

### Backup pool exhausted

When `kd=md` count reaches `backup_pool_size`, the proposer SHOULD publish `kd=sf`. Materialize transitions `stage = Failed`. Anyone observing the log can verify this gate.

### Mass-decline edge case

If all `sortition_size + backup_size` selected members decline (full pool exhausted), `kd=sf` is the only valid event after that point. Other event kinds for this poll are dropped with `PollInFailedState`.

### Retry

Proposer creates a new `kd=cr` Tier 3 PollCreate. New poll has a fresh `event_hash`, hence a fresh VRF beacon seed, hence a fresh sortition. `pd.ro` (retry_of) links to the failed predecessor for audit clarity.

There is no protocol-layer rate-limit on retries — community policy may enforce limits via admin enacted Tier 1/2 polls.

### Stale beacon

If `kd=ss` cannot be computed because the beacon hasn't arrived in DfrostLog yet, voting engine retries on each subsequent `kd=vb` apply until a matching seed shows up. If the committee fails (no kd=vb produced within deliberation_window/2), the poll effectively stalls in Stage 1 — proposer's responsibility to file a new poll if the committee is broken (community admin issue).

### Cross-engine convergence

Two engines may race to publish `kd=ss` from the same beacon. Both compute bit-identical content (Fisher-Yates is deterministic). HLC LWW on `(hlc, event_hash)` resolves the race — only one applies, the other is rejected by HLC monotonicity V5.

### Eligibility snapshot timing

Per spec §10, eligibility snapshot is taken at `PollCreate.hlc`. If a member joins after PollCreate.hlc, they are NOT in the sortition pool nor in the ratification electorate. If a member is kicked between PollCreate.hlc and ratification close, their `kd=rb` ballots ARE still counted (snapshot is locked at PollCreate.hlc).

## 11. Module factoring

```text
src-tauri/src/
  community_voting_sortition.rs   [NEW, ~800 LOC]
    pub struct SortitionResult { primary, backup }
    pub fn derive_beacon_seed(poll_create_hash, community_epoch) -> [u8; 32]
    pub fn fisher_yates_select(vrf_output, electorate, primary_size, backup_size) -> SortitionResult
    pub fn canonical_electorate_order(electorate: &[OwnerAddr]) -> Vec<OwnerAddr>
    + 30+ unit tests

  community_voting_star.rs        [NEW, ~600 LOC]
    pub struct StarResult { winner, finalists, total_scores, runoff_votes }
    pub fn tally_star(candidates, ballots) -> StarResult
    + 25+ unit tests covering tie-breaker cascade

  community_voting_tier3.rs       [NEW, ~1500 LOC]
    pub struct Tier3PollState { meta, stage, ... }
    impl Tier3PollState {
      pub fn apply_event(&mut self, ev: &SignedVotingEvent) -> Result<(), ApplyError>
      pub fn current_stage_at(&self, hlc: Hlc) -> Stage
      pub fn current_mini_public(&self, hlc: Hlc) -> HashSet<OwnerAddr>
      pub fn drafting_advancers(&self) -> Vec<CandidateRef>
      pub fn ratification_candidates(&self) -> Vec<CandidateRef>
      pub fn tally(&self) -> Option<StarResult>
    }
    pub fn validate_tier3_poll_config(pd: &PollConfig) -> Result<(), ValidateError>
    pub fn validate_ratification_ballot(pd: &BallotPayload, state: &Tier3PollState) -> Result<(), ValidateError>
    pub fn verify_ss(event: &SignedVotingEvent, state: &Tier3PollState, dfrost_handle: &DfrostLogRegistry) -> Result<(), VerifyError>
    pub fn verify_sd(event: &SignedVotingEvent, state: &Tier3PollState) -> Result<(), VerifyError>
    pub fn verify_sf(event: &SignedVotingEvent, state: &Tier3PollState) -> Result<(), VerifyError>
    + integration with VotingLogEngine (Arc<DfrostLogRegistry>)

  community_voting_log_engine.rs  [EDIT, +~150 LOC]
    Add dfrost_handle: Arc<DfrostLogRegistry<R>> field
    Add install_dfrost_beacon_callback()
    Add on_dfrost_beacon() handler

  community_voting_core.rs        [EDIT, +~250 LOC]
    Extend EventKind enum with: SortitionSelection, DeliberationStatement, MiniPublicDecline,
      DraftCandidate, DraftApproval, SortitionFailed, RatificationBallot
    Extend Tier 3 PollConfig with `ro: Option<PollId>` (retry_of)
    Tier 3 dispatch table to voting_tier3 module

  lib.rs                          [EDIT, ~20 LOC]
    Wire voting_registry.install_dfrost_handle in start_node()

  tests/community_voting_tier3_integration.rs  [NEW, ~800 LOC]
    Multi-engine 4-stage E2E
    Decline scenario
    Failure + retry scenario

  tests/wire_format_voting_tier3_fixtures.rs   [NEW, ~400 LOC]
    Pin canonical CBOR fixtures for each new event kind
```

## 12. Sub-ticket map

| Ticket | Scope | LOC est |
|---|---|---|
| [ZEB-309](https://linear.app/zeblith/issue/ZEB-309) | Wire format + sortition + STAR + drafting math + state machine + engine coupling + multi-engine integration tests + fixtures | ~2000 (this PR) |
| [ZEB-310](https://linear.app/zeblith/issue/ZEB-310) | 5 Tauri commands + 5 Tauri events + frontend lib utilities + IPC tests | ~600 |
| [ZEB-311](https://linear.app/zeblith/issue/ZEB-311) | 6 Svelte components + vitest coverage | ~1500-2000 |

Each ticket is independently mergeable + reviewable; the bot fleet (CodeRabbit / Cursor / CodeAnt / Qodo) can converge on each PR within ~1 day.

## 13. Test plan (this PR — ZEB-309)

### Unit tests (in each new module)

- `community_voting_sortition.rs`:
  - Deterministic given same seed
  - Different seeds produce different selections
  - Electorate canonicalization (different input orderings → same output)
  - Edge cases: electorate size == primary+backup; electorate size < primary+backup (error)
- `community_voting_star.rs`:
  - 6+ scenarios from §8 ("Test cases")
  - Tiebreaker cascade exhaustively tested
  - Property test: result invariant under ballot reordering
- `community_voting_tier3.rs`:
  - State machine transitions (all 5 stage edges + Failed)
  - Backup auto-promotion on kd=md
  - Status quo synthesis at drafting open
  - Drafting advancers respect threshold + cap
  - Ratification candidate ordering deterministic

### Integration tests (in `tests/community_voting_tier3_integration.rs`)

- **4-stage E2E happy path**: 2+ engines each running voting + DfrostLog; proposer creates Tier 3 poll; engines converge on sortition; mini-public deliberates + drafts + approves; ratification; final result identical across engines.
- **Decline + backup promotion**: 3 declines during stage 1; backup promotions visible across engines.
- **Mass decline → SortitionFailed**: all primary + backup decline; proposer publishes kd=sf; retry poll links via retry_of.
- **Cross-engine kd=ss race**: 2 engines publish kd=ss simultaneously; HLC LWW resolves; both converge on the winner.

### Wire-format fixtures (in `tests/wire_format_voting_tier3_fixtures.rs`)

Following ZEB-250 pattern (regen-on-first-run via `panic!("REGENERATE...")`):
- `fixture_tier3_poll_create.cbor`
- `fixture_sortition_selection.cbor`
- `fixture_mini_public_decline.cbor`
- `fixture_draft_candidate.cbor`
- `fixture_draft_approval.cbor`
- `fixture_sortition_failed.cbor`
- `fixture_ratification_ballot.cbor`
- `fixture_tier3_poll_result.cbor`

Structural CBOR-key checks via `ciborium::Value` (same-length 2-char keys verified).

### CI gate compliance

All 5 gates green:
- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- `npx tsc --noEmit`
- `npx vitest run`

## 14. Implementation order (for plan-writers)

Implementer subagents should expect plans broken into roughly these phases, with TDD-shaped tasks each ending in a commit:

1. **Wire format scaffolding** — extend EventKind enum, add empty struct shells for each new payload, ensure SerDe round-trip works.
2. **Sortition module** — `community_voting_sortition.rs` pure functions + unit tests.
3. **STAR module** — `community_voting_star.rs` pure functions + unit tests.
4. **Tier 3 module skeleton** — state machine + apply_event dispatch + materialize rules (in-memory only).
5. **Status quo synthesis** + drafting math.
6. **Verify rules** — SS1, SD1, SF1, SR1, B1-B5 extensions, validate_tier3_poll_config, validate_ratification_ballot.
7. **Engine coupling** — voting_log_engine ↔ DfrostLog handle + beacon callback.
8. **Multi-engine integration tests** — 4-stage E2E + decline + failure + cross-engine race.
9. **Wire-format fixtures** — pin canonical CBOR for each new kind.
10. **Final 5-gate sweep + push + PR creation** — per `feedback_cargo_fmt_gate` + `feedback_pipe_exit_codes_lie`.

## References

- Umbrella spec: [`docs/specs/2026-05-16-zeb-289-voting-polling-design.md`](2026-05-16-zeb-289-voting-polling-design.md)
- D-FROST primitives: `src-tauri/src/community_dfrost_*.rs` (Phase 4a-foundation)
- Tier 1 reference pattern: `src-tauri/src/community_voting_approval.rs`
- Wire-format fixture pattern: `src-tauri/tests/wire_format_zeb250_fixtures.rs`
- Send-safety lesson: [ZEB-307](https://linear.app/zeblith/issue/ZEB-307) PR #146 (PhantomData<fn() -> R>)
- Cross-CRDT engine coupling lesson: `src-tauri/src/community_dfrost_log_engine.rs` (Arc<Registry> pattern)
