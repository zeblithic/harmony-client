//! ZEB-309 Phase 4a-main: Tier 3 poll state machine + drafting + DfrostLog coupling.
//! See docs/specs/2026-05-20-zeb-309-phase4a-main-design.md §3 + §6 + §9.

use crate::community_voting_core::{
    CandidateEventHash, DeliberationStatementPayload, DeliberationVotePayload,
    DraftApprovalPayload, DraftCandidatePayload, MiniPublicDeclinePayload, PollEventKindCode,
    PollId, RatificationBallotPayload, SignedVotingEvent, SortitionFailedPayload,
    SortitionSelectionPayload, Tier3PollConfigPayload,
};
use crate::community_voting_sortition::{derive_beacon_seed, fisher_yates_select, SortitionResult};
use crate::community_voting_star::{tally_star, StarResult};
use crate::owner_state_types::{Hlc, OwnerAddr};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

// ── Stage enum ────────────────────────────────────────────────────────────────

/// The four lifecycle stages of a Tier 3 poll plus terminal states.
/// See design spec §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Stage 1: Awaiting VRF beacon + kd=ss SortitionSelection.
    Sortition,
    /// Stage 2: Mini-public deliberates (kd=ds). HLC-gated.
    Deliberation,
    /// Stage 3: Mini-public proposes drafts (kd=dc/da). HLC-gated.
    Drafting,
    /// Stage 4: Full electorate casts STAR ballots (kd=rb). HLC-gated + ≥1 candidate.
    Ratification,
    /// Terminal: kd=rs PollResult applied.
    Finalized,
    /// Terminal: kd=sf SortitionFailed applied.
    Failed,
}

// ── Supporting state types ────────────────────────────────────────────────────

/// State for a single draft candidate (kd=dc event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftCandidateState {
    /// SHA-256 of the signing bytes of the originating kd=dc event.
    pub event_hash: CandidateEventHash,
    /// Proposal text from the DraftCandidatePayload.
    pub text: String,
    /// OwnerAddr of the proposer, or None for the synthesized status_quo.
    pub proposer: Option<OwnerAddr>,
    /// Set of members who have approved this candidate (includes implicit self-approval).
    pub approvals: HashSet<OwnerAddr>,
}

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
/// determinism (spec §4.7 point 2) depends on this.
#[derive(Debug, Clone, Default)]
pub struct DeliberationState {
    pub statements: std::collections::BTreeMap<[u8; 32], DeliberationStatement>,
    pub votes: std::collections::BTreeMap<(OwnerAddr, [u8; 32]), DeliberationVoteEntry>,
    pub statements_per_author: std::collections::BTreeMap<OwnerAddr, u8>,
}

/// Immutable metadata derived from the kd=cr PollCreate event.
#[derive(Debug, Clone)]
pub struct Tier3PollMeta {
    pub poll_id: PollId,
    pub proposer: OwnerAddr,
    pub poll_create_hlc: Hlc,
    pub config: Tier3PollConfigPayload,
    /// SHA-256 of the signing bytes of the kd=cr PollCreate event.
    /// Used by verify_ss to derive the beacon seed deterministically.
    /// Populated by the caller (Task 8 dispatch) at apply-create time.
    pub poll_create_event_hash: [u8; 32],
    /// Community epoch at PollCreate time, used to derive the beacon seed
    /// via `derive_beacon_seed(poll_create_event_hash, community_epoch)`.
    /// Populated by the caller (Task 8 dispatch) at apply-create time;
    /// Task 10 wires the real epoch from DfrostLogRegistry.
    /// `u64` to match `DfrostLogEngine::current_epoch()` return type.
    pub community_epoch: u64,
}

// ── ZEB-295 Phase 6: ballot-secret committee oracle + tally state ────────────

/// Public-side view of the committee at a single DKG epoch, exposed by a
/// `CommitteeOracle`. Carries everything the tier3 apply path needs to
/// verify a `kd=ts` payload — the joint verifying key (= ElGamal Y), per-
/// member verifying shares (= DLEQ basis points Y_i), and the threshold.
#[derive(Debug, Clone)]
pub struct CommitteePublicState {
    pub epoch: u64,
    pub joint_verifying_key: [u8; 32],
    pub verifying_shares: std::collections::BTreeMap<OwnerAddr, [u8; 32]>,
    pub threshold: u16,
}

/// Read-side handle into the per-community DKG history. The Tier 3 apply
/// path consults this to verify `kd=ts` payloads against the committee
/// that was active at the poll's PollCreate epoch (or — for cross-epoch
/// shares — the matching `epoch` in the payload).
///
/// `Send + Sync` because `Tier3PollState` is stored inside per-community
/// engines that the IPC layer hands out behind locks; the oracle must be
/// safe to share across those locks.
pub trait CommitteeOracle: Send + Sync + std::fmt::Debug {
    fn committee_at_epoch(&self, epoch: u64) -> Option<CommitteePublicState>;
    fn latest_epoch(&self) -> Option<u64>;
}

/// Trivial oracle that knows about no committees — used as the default
/// before Task 10 wires the real `DfrostLogRegistry`-backed implementation.
/// All Tier 3c apply paths that consult the oracle MUST tolerate `None`
/// returns (silent drop with no state mutation, matching the existing
/// Phase 4a "silent drop for missing prerequisites" convention).
#[derive(Debug, Default)]
pub struct NullCommitteeOracle;

impl CommitteeOracle for NullCommitteeOracle {
    fn committee_at_epoch(&self, _epoch: u64) -> Option<CommitteePublicState> {
        None
    }
    fn latest_epoch(&self) -> Option<u64> {
        None
    }
}

/// Single LWW-stamped tally-share submission held as the value of
/// `SecretTallyState::tally_shares`. Tagged with the submitting event's
/// `(hlc, event_hash)` so the apply path can compare against an incoming
/// re-submission from the same `(actor, committee_epoch)` key and
/// overwrite when the incoming pair is strictly greater (Cursor PR #155
/// #3 + Qodo PR #155 #4 LWW fix).
///
/// `entries` mirrors the original `TallySharePayload.entries` Vec: one
/// `TallyShareEntry` per ciphertext aggregate (score-sum slots first,
/// then indicator-pair slots in canonical order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TallyShareRecord {
    pub entries: Vec<crate::community_voting_core::TallyShareEntry>,
    pub last_update_hlc: Hlc,
    pub last_update_event_hash: [u8; 32],
}

/// Ballot-secret tally projection. `tally_shares` is LWW-upserted by
/// `(actor, committee_epoch)` keyed on `(hlc.wall_ms, hlc.logical,
/// hlc.device_id, event_hash)` lex order. Later submissions from the same
/// committee member at the same epoch overwrite earlier ones — required
/// per Qodo PR #155 #4 because kd=ts DLEQ verification is performed
/// against aggregates computed from the poll's CURRENT accepted ballots,
/// and a late-arriving valid ballot (lower HLC than already-applied
/// shares) changes those aggregates and invalidates earlier shares for
/// decryption; the committee member must be able to resubmit with the
/// updated aggregates and have the new submission win.
#[derive(Debug, Clone, Default)]
pub struct SecretTallyState {
    pub tally_shares: std::collections::BTreeMap<(OwnerAddr, u64), TallyShareRecord>,
    /// Populated once enough shares are collected to Lagrange-combine and
    /// BSGS-decode every ciphertext. Mirrors the pu-mode `result` field
    /// shape so the IPC read path can render either uniformly.
    pub decrypted_result: Option<crate::community_voting_star::StarResult>,
}

/// Full state of an in-progress or terminal Tier 3 poll.
/// Built by applying kd=* events in (hlc, event_hash) lex order.
///
/// `Debug` is hand-rolled because `Arc<dyn CommitteeOracle>` does derive
/// Debug (the trait carries `+ Debug`) but we prefer to print a short
/// `<oracle>` placeholder rather than risk dumping committee internals.
#[derive(Clone)]
pub struct Tier3PollState {
    pub meta: Tier3PollMeta,
    pub stage: Stage,
    /// Eligible full electorate snapshotted at PollCreate.hlc.
    pub eligible_electorate_snapshot: Vec<OwnerAddr>,
    /// Set by the first valid kd=ss event (SS1 verify deferred to Task 6).
    pub sortition_result: Option<SortitionResult>,
    /// Decline records from kd=md events: (actor, hlc).
    pub declines: Vec<(OwnerAddr, Hlc)>,
    /// Draft candidates from kd=dc events (and synthesized status_quo).
    pub candidates: Vec<DraftCandidateState>,
    /// Ratification ballots from kd=rb events.
    pub ratification_ballots: Vec<RatificationBallotPayload>,
    /// SHA-256 of signing_bytes of the kd=cl PollClose event, if applied.
    pub close_event_hash: Option<[u8; 32]>,
    /// Set by kd=rs PollResult event (StarResult decoded from payload).
    pub result: Option<StarResult>,
    /// Phase 5 (Tier 3b) deliberation projection. Populated by `kd=ds`
    /// and `kd=dv` apply paths. Default empty until Task 3/4 wire apply.
    pub deliberation: DeliberationState,
    /// HLC of the most recent **accepted** event (None before any accept).
    /// Used as the `now` watermark by `current_stage_at` and `current_mini_public`
    /// via `build_tier3_export`. Drop paths must NOT advance this (ZEB-320).
    pub last_hlc: Option<Hlc>,
    /// Per-`(actor, device_id)` receive-watermark lanes (ZEB-850, mirrors the
    /// channel-log `WatermarkVector`, ZEB-585). Each lane holds the highest
    /// `(wall_ms, logical)` dispatched on that lane — accepted OR silently
    /// dropped (the ZEB-320 property, now per-lane). Keying per lane stops one
    /// sibling's future-stamped event from freezing every other member's
    /// events (the T-VOTE-LANE GRIEF-LOCKOUT). Rebuilt by replay; never
    /// serialized (community_voting_persist.rs).
    pub last_received_hlc: std::collections::BTreeMap<(OwnerAddr, String), (u64, u32)>,
    /// ZEB-860: highest `(wall_ms, logical, device_id)` key dispatched to this
    /// poll (accepted OR silently dropped). Read by the apply layer to detect
    /// out-of-order arrival and trigger a canonical-order rebuild. Never
    /// serialized (like `last_received_hlc`).
    pub(crate) max_applied: Option<(u64, u32, String)>,
    /// ZEB-860: cumulative count of canonical-order rebuilds performed on this
    /// projection via `rebuild_from_events`. Preserved across each rebuild's
    /// reset and incremented, so it counts total rebuilds over the poll's life
    /// (not resets to 1). Read by tests/telemetry; never serialized.
    pub(crate) rebuild_count: u64,
    /// ZEB-295 Phase 6: ballot-secret tally projection. Populated by
    /// `kd=ts` apply (Task 7) and the decrypted-result derivation
    /// (Task 8). Empty in pu-mode polls.
    pub secret_tally: SecretTallyState,
    /// ZEB-295 Phase 6: read-side handle into the per-community committee
    /// history, used at `kd=ts` apply to verify DLEQ proofs against the
    /// correct epoch's verifying shares. Defaults to `NullCommitteeOracle`
    /// (returns `None` for every query); Task 10 wires the real registry-
    /// backed implementation.
    pub committee_oracle: std::sync::Arc<dyn CommitteeOracle>,
}

// Hand-rolled Debug because `Arc<dyn CommitteeOracle>` is awkward to format
// and the oracle internals aren't useful in poll-state dumps. Every other
// field passes through unchanged.
impl std::fmt::Debug for Tier3PollState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tier3PollState")
            .field("meta", &self.meta)
            .field("stage", &self.stage)
            .field(
                "eligible_electorate_snapshot",
                &self.eligible_electorate_snapshot,
            )
            .field("sortition_result", &self.sortition_result)
            .field("declines", &self.declines)
            .field("candidates", &self.candidates)
            .field("ratification_ballots", &self.ratification_ballots)
            .field("close_event_hash", &self.close_event_hash)
            .field("result", &self.result)
            .field("deliberation", &self.deliberation)
            .field("last_hlc", &self.last_hlc)
            .field("last_received_hlc", &self.last_received_hlc)
            .field("max_applied", &self.max_applied)
            .field("rebuild_count", &self.rebuild_count)
            .field("secret_tally", &self.secret_tally)
            .field("committee_oracle", &"<oracle>")
            .finish()
    }
}

// ── Validate error type ───────────────────────────────────────────────────────

/// Validation errors for Tier 3 PollCreate config and RatificationBallot payloads.
/// Returned by [`validate_tier3_poll_config`] and [`validate_ratification_ballot`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidateError {
    #[error("proposal_text is empty")]
    EmptyProposalText,
    #[error("sortition_size {0} out of range [20, 300]")]
    SortitionSizeOutOfRange(u16),
    #[error("deliberation_window_seconds {0} below floor 60")]
    DeliberationWindowTooSmall(u32),
    #[error("drafting_window_seconds {0} below floor 60")]
    DraftingWindowTooSmall(u32),
    #[error("ratification_window_seconds {0} below floor 60")]
    RatificationWindowTooSmall(u32),
    #[error("unknown privacy_mode {0:?}; accepts \"pu\" or \"se\" (\"rf\" reserved for Phase 7)")]
    UnknownPrivacyMode(String),
    #[error("unknown incentive_mode {0:?}; must be one of a/b/c/d")]
    UnknownIncentiveMode(String),
    #[error(
        "ratification ballot scores length {scores} != ratification_candidates length {expected}"
    )]
    BallotLengthMismatch { scores: usize, expected: usize },
    #[error("ratification ballot score {0} > 5")]
    BallotScoreOutOfRange(u8),
    #[error("decline reason must be exactly 2 ASCII alphanumeric chars")]
    BadDeclineReason,
}

// ── Validate functions ────────────────────────────────────────────────────────

/// Validate Tier 3 PollCreate config payload BEFORE signing/applying/broadcasting.
///
/// Per design spec §5 (verify rule C1) and feedback_metadata_before_irreversible_write:
/// read-only validation precedes irreversible writes.
///
/// Note: `retry_of` integrity (predecessor must exist and be in Failed state)
/// cannot be validated here without state context; the caller (community_voting_core
/// dispatch in Task 8) handles that check.
pub fn validate_tier3_poll_config(pd: &Tier3PollConfigPayload) -> Result<(), ValidateError> {
    if pd.proposal_text.is_empty() {
        return Err(ValidateError::EmptyProposalText);
    }
    if !(20..=300).contains(&pd.sortition_size) {
        return Err(ValidateError::SortitionSizeOutOfRange(pd.sortition_size));
    }
    if pd.deliberation_window_seconds < 60 {
        return Err(ValidateError::DeliberationWindowTooSmall(
            pd.deliberation_window_seconds,
        ));
    }
    if pd.drafting_window_seconds < 60 {
        return Err(ValidateError::DraftingWindowTooSmall(
            pd.drafting_window_seconds,
        ));
    }
    if pd.ratification_window_seconds < 60 {
        return Err(ValidateError::RatificationWindowTooSmall(
            pd.ratification_window_seconds,
        ));
    }
    if !["pu", "se"].contains(&pd.privacy_mode.as_str()) {
        // ZEB-295 Phase 6: "se" (ballot-secret threshold-ElGamal) is now
        // accepted; "rf" (receipt-free) remains reserved for Phase 7.
        return Err(ValidateError::UnknownPrivacyMode(pd.privacy_mode.clone()));
    }
    if !["a", "b", "c", "d"].contains(&pd.incentive_mode.as_str()) {
        return Err(ValidateError::UnknownIncentiveMode(
            pd.incentive_mode.clone(),
        ));
    }
    Ok(())
}

/// Validate a Tier 3 RatificationBallot payload against the poll's
/// ratification candidate set. Per design spec §5 (verify rule B4).
///
/// `expected_candidate_count` is looked up by the caller from
/// `poll_state.ratification_candidate_count()` (Task 8).
pub fn validate_ratification_ballot(
    pd: &RatificationBallotPayload,
    expected_candidate_count: usize,
) -> Result<(), ValidateError> {
    // ZEB-295 Phase 6: `scores` is now Option<Vec<u8>> on the wire-format
    // type (Some in "pu" mode, None in "se" mode). This validator is only
    // called from the pu-mode IPC path, so we treat None as a length-0 ballot
    // for the mismatch error path. The se-mode path bypasses this validator.
    let scores_len = pd.scores.as_ref().map(|s| s.len()).unwrap_or(0);
    if scores_len != expected_candidate_count {
        return Err(ValidateError::BallotLengthMismatch {
            scores: scores_len,
            expected: expected_candidate_count,
        });
    }
    if let Some(scores) = pd.scores.as_ref() {
        for &s in scores {
            if s > 5 {
                return Err(ValidateError::BallotScoreOutOfRange(s));
            }
        }
    }
    Ok(())
}

/// Validate the optional `reason` field on a `kd=md` MiniPublicDecline event.
///
/// Per spec §3 same-length-keys invariant: if `Some`, the reason must be
/// exactly 2 ASCII alphanumeric chars (e.g., `"u1"`, `"co"`). `None` is
/// always accepted (signals decline without a coded reason).
pub fn validate_decline_reason(reason: &Option<String>) -> Result<(), ValidateError> {
    if let Some(r) = reason {
        if r.len() != 2 || !r.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(ValidateError::BadDeclineReason);
        }
    }
    Ok(())
}

// ── Apply error type ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApplyError {
    #[error("poll is in Failed state")]
    PollInFailedState,
    #[error("poll is Finalized")]
    PollInFinalizedState,
    #[error("payload decode failed: {0}")]
    PayloadDecode(String),
    #[error("event kind {0:?} not valid for Tier 3")]
    InvalidKindForTier3(PollEventKindCode),
    #[error("event hlc not monotonic")]
    HlcNotMonotonic,
}

// ── Apply outcome + canonical order key ───────────────────────────────────────

/// Whether an `apply_event` call changed the projection (`Applied`) or hit a
/// silent-drop branch (`Dropped`). Used by the apply layer's out-of-order
/// rebuild trigger to tell a state-changing accept from a no-op drop.
///
/// `pub` (not `pub(crate)`) because `apply_event` is a `pub fn` on a `pub`
/// struct in a `pub mod`: returning a `pub(crate)` type from it would trip the
/// `private_interfaces` lint under `-D warnings`. Mirrors the sibling
/// `ApplyError`, which is `pub` for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Dropped,
}

/// Canonical total order for replay/rebuild: `(wall_ms, logical, device_id,
/// event_hash)`. `event_hash` (SHA-256 of signing bytes) is the final
/// tiebreaker — a strict total order with no ties even if a device reuses an
/// HLC. Matches the `dv`/`ts` LWW tiebreak.
///
/// ZEB-860 Task 2 wires the production consumer: `rebuild_from_events` sorts a
/// poll's events by this key before the canonical-order re-fold, so the helper
/// is live in the non-test lib build (no `allow(dead_code)` needed).
pub(crate) fn canonical_key(ev: &SignedVotingEvent) -> (u64, u32, String, [u8; 32]) {
    (
        ev.hlc.wall_ms,
        ev.hlc.logical,
        ev.hlc.device_id.clone(),
        sha256_of_signing_bytes(ev),
    )
}

// ── Tier3PollState impl ───────────────────────────────────────────────────────

impl Tier3PollState {
    /// Create a fresh Tier3PollState from a parsed Tier 3 PollCreate event meta
    /// and the electorate snapshot taken at PollCreate.hlc.
    ///
    /// `poll_create_event_hash`: SHA-256 of the signing bytes of the kd=cr event.
    /// `community_epoch`: epoch value for beacon seed derivation (Task 10 wires real value).
    ///
    /// Starts in Stage::Sortition — awaiting kd=ss.
    pub fn new_from_create(
        meta: Tier3PollMeta,
        eligible_electorate_snapshot: Vec<OwnerAddr>,
    ) -> Self {
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
            last_received_hlc: std::collections::BTreeMap::new(),
            max_applied: None,
            rebuild_count: 0,
            secret_tally: SecretTallyState::default(),
            committee_oracle: std::sync::Arc::new(NullCommitteeOracle),
        }
    }

    /// ZEB-295 Phase 6: install a committee oracle. Used by the engine
    /// setup path (Task 10) to bind the per-community
    /// `DfrostLogRegistry` view; tests can install a fixture oracle
    /// to short-circuit DKG ceremony setup.
    pub fn install_committee_oracle(&mut self, oracle: std::sync::Arc<dyn CommitteeOracle>) {
        self.committee_oracle = oracle;
    }

    /// ZEB-860: re-materialize this poll's projection as a deterministic fold of
    /// `events` in canonical order. Resets only replay-derived fields (via the
    /// canonical `new_from_create`), preserving `meta` (incl. `community_epoch`),
    /// `eligible_electorate_snapshot`, and the installed `committee_oracle`.
    /// Synchronous: `apply_event` reads only stored poll state, so no membership
    /// resolver is needed. Each `Err` on re-fold is a terminal/monotonic
    /// rejection, ignored exactly as boot replay ignores un-replayable events.
    ///
    /// ZEB-860 Task 2 lands the method + its tests; Task 3 wires the production
    /// trigger in `VotingLog::apply_with_snapshot`, which calls this on
    /// out-of-order arrival of an order-dependent Deliberation-family event —
    /// so the helper (and its `canonical_key` callee) is now live in the
    /// non-test lib build.
    pub(crate) fn rebuild_from_events(&mut self, events: &[SignedVotingEvent]) {
        let meta = self.meta.clone();
        let electorate = std::mem::take(&mut self.eligible_electorate_snapshot);
        let oracle = self.committee_oracle.clone();
        let rebuilds = self.rebuild_count;
        *self = Tier3PollState::new_from_create(meta, electorate);
        self.committee_oracle = oracle;
        self.rebuild_count = rebuilds + 1;

        let mut sorted: Vec<&SignedVotingEvent> = events.iter().collect();
        // `sort_by_cached_key` over `sort_by(|a,b| key(a).cmp(&key(b)))`:
        // `canonical_key` hashes the event (SHA-256 signing bytes), so caching one
        // key per element beats recomputing it on every comparison. Also satisfies
        // clippy's `unnecessary_sort_by`.
        sorted.sort_by_cached_key(|a| canonical_key(a));
        for ev in sorted {
            let _ = self.apply_event(ev);
        }
    }

    /// Apply a single event to this state, mutating it in place.
    ///
    /// Dispatches on `ev.kind` per design spec §6 per-event apply table.
    /// Terminal states (Failed, Finalized) reject all further events.
    ///
    /// Verify rules (SS1, SD1, B1-B5, SF1, SR1) are deferred to Task 6/7
    /// and are NOT enforced here; apply_event is the pure materialize layer.
    pub fn apply_event(&mut self, ev: &SignedVotingEvent) -> Result<ApplyOutcome, ApplyError> {
        // 1. Reject if terminal state.
        match self.stage {
            Stage::Failed => return Err(ApplyError::PollInFailedState),
            Stage::Finalized => return Err(ApplyError::PollInFinalizedState),
            _ => {}
        }

        // 2. Monotonic HLC check (defensive — upstream should enforce, but we
        //    surface HlcNotMonotonic if the caller sends out-of-order events).
        //    Reads `last_received_hlc` (advances on every dispatch, accept or
        //    drop) rather than `last_hlc` (advances on accepts only) so the
        //    guard still catches out-of-order delivery after a dropped event.
        // ZEB-850: per-(actor, device) monotonic guard. Compare the incoming
        // event only against the high-water mark of ITS OWN lane. device_id is
        // constant within a lane, so comparing (wall_ms, logical) is equivalent
        // to the old (wall_ms, logical, device_id) 3-tuple compare. A future
        // event on one lane no longer blocks another lane's honest events.
        let lane = (ev.actor, ev.hlc.device_id.clone());
        if let Some(&(w, l)) = self.last_received_hlc.get(&lane) {
            if (ev.hlc.wall_ms, ev.hlc.logical) < (w, l) {
                return Err(ApplyError::HlcNotMonotonic);
            }
        }

        // ZEB-320: silent-drop paths must NOT advance the projection watermark
        // `last_hlc` (read by `current_stage_at` and `current_mini_public` via
        // `build_tier3_export`); advancing it on filtered/invalid peer events
        // would let an adversary push the projected stage forward without any
        // accepted state change. Each drop branch below sets this to false.
        // `last_received_hlc` always advances at the tail regardless.
        let mut advance_last_hlc = true;

        // 3. Dispatch on event kind.
        match ev.kind {
            // kd=ss SortitionSelection: set sortition_result.
            // Stage advance happens via current_stage_at / recompute_stage (not here).
            PollEventKindCode::SortitionSelection => {
                let payload: SortitionSelectionPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                self.sortition_result = Some(SortitionResult {
                    primary: payload.primary,
                    backup: payload.backup,
                });
            }

            // kd=md MiniPublicDecline: append (actor, hlc) to declines.
            PollEventKindCode::MiniPublicDecline => {
                // Decode payload to validate it parses correctly; reason field unused at materialize level.
                let _payload: MiniPublicDeclinePayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                self.declines.push((ev.actor, ev.hlc.clone()));
            }

            // kd=ds DeliberationStatement: materialize per spec §2.3 apply rules.
            // Apply-time rules (silent drop on failure; falls through to post-match last_hlc update).
            PollEventKindCode::DeliberationStatement => {
                let payload: DeliberationStatementPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                let event_hash = sha256_of_signing_bytes(ev);

                // Rule 1: stage must be Deliberation at ev.hlc.
                if self.current_stage_at(&ev.hlc) != Stage::Deliberation {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        actor = %hex::encode(ev.actor.0),
                        "kd=ds drop: not in Deliberation stage"
                    );
                // Rule 2: actor must be in the current mini-public at ev.hlc.
                } else if !self.current_mini_public(&ev.hlc).contains(&ev.actor) {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        actor = %hex::encode(ev.actor.0),
                        "kd=ds drop: actor not in current mini-public"
                    );
                // Rule 3: text must be 1..=280 chars (non-empty after trim).
                } else if payload.text.chars().count() > 280 || payload.text.trim().is_empty() {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        actor = %hex::encode(ev.actor.0),
                        "kd=ds drop: text length out of range or whitespace-only"
                    );
                } else {
                    // Rule 4: per-actor spam cap — at most 5 statements per poll.
                    let prior_count = self
                        .deliberation
                        .statements_per_author
                        .get(&ev.actor)
                        .copied()
                        .unwrap_or(0);
                    if prior_count >= 5 {
                        advance_last_hlc = false;
                        tracing::debug!(
                            poll_id = %hex::encode(self.meta.poll_id.0),
                            actor = %hex::encode(ev.actor.0),
                            prior_count,
                            "kd=ds drop: per-actor 5-statement spam cap reached"
                        );
                    } else {
                        // Accept: insert into deliberation projection. Statement events are
                        // keyed by event_hash (sha256 of signing bytes), so replays of the
                        // exact same event are idempotent on the map. Only bump the per-author
                        // spam counter when the insert actually added a new statement —
                        // otherwise repeated apply of the same event would falsely advance
                        // the cap toward the 5-statement limit.
                        let inserted = self
                            .deliberation
                            .statements
                            .insert(
                                event_hash,
                                DeliberationStatement {
                                    event_hash,
                                    author: ev.actor,
                                    text: payload.text,
                                    created_at_hlc: ev.hlc.clone(),
                                },
                            )
                            .is_none();
                        if inserted {
                            *self
                                .deliberation
                                .statements_per_author
                                .entry(ev.actor)
                                .or_insert(0) += 1;
                        }
                    }
                }
            }

            // kd=dv DeliberationVote: LWW upsert keyed by (voter, statement_event_hash).
            // Apply-time rules per spec §2.3 (silent drop on failure; falls through to
            // post-match last_hlc update).
            PollEventKindCode::DeliberationVote => {
                let payload: DeliberationVotePayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                let event_hash = sha256_of_signing_bytes(ev);

                let stage_ok = self.current_stage_at(&ev.hlc) == Stage::Deliberation;
                let mp_ok = stage_ok && self.current_mini_public(&ev.hlc).contains(&ev.actor);
                let target = if mp_ok {
                    self.deliberation
                        .statements
                        .get(&payload.statement_event_hash)
                } else {
                    None
                };
                let self_vote = target.is_some_and(|s| s.author == ev.actor);
                let vote_code =
                    crate::community_voting_core::BridgingVoteCode::from_u8(payload.vote);

                if !stage_ok {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        actor = %hex::encode(ev.actor.0),
                        "kd=dv drop: not in Deliberation stage"
                    );
                } else if !mp_ok {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        actor = %hex::encode(ev.actor.0),
                        "kd=dv drop: actor not in current mini-public"
                    );
                } else if target.is_none() {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        actor = %hex::encode(ev.actor.0),
                        statement = %hex::encode(payload.statement_event_hash),
                        "kd=dv drop: target statement not in projection"
                    );
                } else if self_vote {
                    // Spec §2.3: deliberation votes are for *another* member's statement;
                    // self-votes would distort statement totals and bridging scores.
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        actor = %hex::encode(ev.actor.0),
                        statement = %hex::encode(payload.statement_event_hash),
                        "kd=dv drop: voter is author of target statement"
                    );
                } else if let Some(vote_code) = vote_code {
                    // LWW: insert only if (ev.hlc, event_hash) > existing entry's (hlc, hash).
                    let key = (ev.actor, payload.statement_event_hash);
                    let should_insert = match self.deliberation.votes.get(&key) {
                        None => true,
                        Some(existing) => {
                            let incoming = (
                                ev.hlc.wall_ms,
                                ev.hlc.logical,
                                ev.hlc.device_id.as_str(),
                                event_hash,
                            );
                            let current = (
                                existing.last_update_hlc.wall_ms,
                                existing.last_update_hlc.logical,
                                existing.last_update_hlc.device_id.as_str(),
                                existing.last_update_event_hash,
                            );
                            incoming > current
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
                    } else {
                        // Stale LWW: incoming (hlc, event_hash) ≤ existing entry's;
                        // silent drop, watermark must not advance (ZEB-320).
                        advance_last_hlc = false;
                    }
                } else {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        actor = %hex::encode(ev.actor.0),
                        vote_byte = payload.vote,
                        "kd=dv drop: vote byte out of range"
                    );
                }
            }

            // kd=dc DraftCandidate: append new candidate with implicit self-approval.
            PollEventKindCode::DraftCandidate => {
                let payload: DraftCandidatePayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                let event_hash = sha256_of_signing_bytes(ev);
                let mut approvals = HashSet::new();
                approvals.insert(ev.actor); // implicit self-approval per spec §6
                self.candidates.push(DraftCandidateState {
                    event_hash,
                    text: payload.text,
                    proposer: Some(ev.actor),
                    approvals,
                });
            }

            // kd=da DraftApproval: add actor to the named candidate's approvals (idempotent).
            PollEventKindCode::DraftApproval => {
                let payload: DraftApprovalPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                if let Some(candidate) = self
                    .candidates
                    .iter_mut()
                    .find(|c| c.event_hash == payload.candidate_event_hash)
                {
                    candidate.approvals.insert(ev.actor); // HashSet = idempotent
                } else {
                    // Candidate not found: silent drop (candidate may arrive out of
                    // order in cross-node scenarios; Task 6 verify will enforce
                    // ordering). Watermark must not advance (ZEB-320).
                    advance_last_hlc = false;
                }
            }

            // kd=sf SortitionFailed: terminal failure state. Falls through
            // to the unified tail (advance_last_hlc stays true → both
            // watermarks update; equivalent to the prior explicit set+return).
            PollEventKindCode::SortitionFailed => {
                let _payload: SortitionFailedPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                self.stage = Stage::Failed;
            }

            // kd=rb RatificationBallot. Phase 4: append the pu-mode ballot.
            // Phase 6 (ZEB-295): B5 = encoding-matches-privacy-mode +
            // ciphertext-shape + NIZK verify (se-mode only). Failure on any
            // sub-check → silent-drop per spec §3.2; advance_last_hlc = false
            // so the projection watermark does not advance for filtered events
            // (ZEB-320).
            PollEventKindCode::RatificationBallot => {
                let payload: RatificationBallotPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                let mode = self.meta.config.privacy_mode.as_str();
                // Qodo PR #155 #2 (ZEB-295 Phase 6): derive `n` from the
                // canonical ratification-candidate ordering — the same
                // computation the IPC path (`tier3_ratification_candidate_count`)
                // and engine-auto kd=rs path use. Using `self.candidates.len() + 1`
                // is incorrect when only a subset of kd=dc candidates pass the
                // approval threshold + primary_size cap, because then
                // `ratification_candidates_ordering.len() < self.candidates.len() + 1`
                // and se-mode ballots minted by the IPC at the correct length
                // would be silently dropped here on shape mismatch.
                //
                // If `drafting_advancers` returns None, we're pre-Drafting
                // (status_quo not yet synthesized); silent-drop per spec §3.2.
                let sq = synthesize_status_quo(&self.meta.poll_id);
                let sq_hash = sq.event_hash;
                let mut all_candidates = self.candidates.clone();
                all_candidates.push(sq);
                let primary_size = self.meta.config.sortition_size as usize;
                let n = match drafting_advancers(&all_candidates, primary_size, sq_hash) {
                    Some(advancers) => ratification_candidates_ordering(&advancers, sq_hash).len(),
                    None => {
                        advance_last_hlc = false;
                        tracing::debug!(
                            poll_id = %hex::encode(self.meta.poll_id.0),
                            actor = %hex::encode(ev.actor.0),
                            "kd=rb drop: pre-Drafting stage (status_quo not synthesized)"
                        );
                        // Skip downstream shape/NIZK checks; flow into the
                        // unified tail. Use a no-op block to keep the
                        // `if !b5_ok …` branches reachable but inert.
                        0
                    }
                };
                let pair_count = n * (n - 1) / 2;
                let b5_ok = match mode {
                    "pu" => {
                        payload.scores.is_some()
                            && payload.ciphertexts_scores.is_none()
                            && payload.ciphertexts_indicators.is_none()
                            && payload.proof.is_none()
                    }
                    "se" => {
                        payload.scores.is_none()
                            && payload
                                .ciphertexts_scores
                                .as_ref()
                                .is_some_and(|v| v.len() == n)
                            && payload
                                .ciphertexts_indicators
                                .as_ref()
                                .is_some_and(|v| v.len() == pair_count)
                            && payload.proof.as_ref().is_some_and(|p| {
                                p.range_proofs.len()
                                == crate::community_voting_tier3_nizk::Range5Proof::SIZE * n
                                && p.consistency_proofs.len()
                                    == crate::community_voting_tier3_nizk::ConsistencyProof::SIZE
                                        * pair_count
                            })
                    }
                    _ => false,
                };
                if !b5_ok {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        actor = %hex::encode(ev.actor.0),
                        mode,
                        "kd=rb drop: B5 encoding-matches-privacy-mode/shape failed"
                    );
                } else if mode == "se" {
                    // NIZK verify against committee Y at latest known epoch.
                    let nizk_ok = match self
                        .committee_oracle
                        .latest_epoch()
                        .and_then(|e| self.committee_oracle.committee_at_epoch(e))
                    {
                        Some(cs) => match crate::community_voting_tier3_crypto::decompress_point(
                            &cs.joint_verifying_key,
                        ) {
                            Some(y_point) => {
                                let proof_ref = payload.proof.as_ref().unwrap();
                                let proof_struct =
                                    crate::community_voting_tier3_nizk::BallotBundleProof {
                                        range_proofs: proof_ref.range_proofs.clone(),
                                        consistency_proofs: proof_ref.consistency_proofs.clone(),
                                    };
                                crate::community_voting_tier3_nizk::verify_ballot_bundle(
                                    &y_point,
                                    payload.ciphertexts_scores.as_ref().unwrap(),
                                    payload.ciphertexts_indicators.as_ref().unwrap(),
                                    &proof_struct,
                                )
                            }
                            None => false,
                        },
                        None => false,
                    };
                    if !nizk_ok {
                        advance_last_hlc = false;
                        tracing::debug!(
                            poll_id = %hex::encode(self.meta.poll_id.0),
                            actor = %hex::encode(ev.actor.0),
                            "kd=rb se-mode drop: NIZK verify failed"
                        );
                    } else {
                        self.ratification_ballots.push(payload);
                    }
                } else {
                    self.ratification_ballots.push(payload);
                }
            }

            // kd=ts TallyShare (ZEB-295 Phase 6, spec §3.3): committee member's
            // partial decryption share + DLEQ proof. Silent-drop per sub-check
            // on failure; advance_last_hlc=false on every drop branch so the
            // projection watermark does not advance (ZEB-320).
            PollEventKindCode::TallyShare => {
                let payload: crate::community_voting_core::TallySharePayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;

                if self.meta.config.privacy_mode != "se" {
                    advance_last_hlc = false;
                    tracing::debug!(
                        poll_id = %hex::encode(self.meta.poll_id.0),
                        "kd=ts drop: poll is not se-mode"
                    );
                } else {
                    let ratification_end_ms = self.meta.poll_create_hlc.wall_ms
                        + (self.meta.config.deliberation_window_seconds as u64
                            + self.meta.config.drafting_window_seconds as u64
                            + self.meta.config.ratification_window_seconds as u64)
                            * 1000;
                    if ev.hlc.wall_ms < ratification_end_ms {
                        advance_last_hlc = false;
                        tracing::debug!(
                            poll_id = %hex::encode(self.meta.poll_id.0),
                            wall_ms = ev.hlc.wall_ms,
                            ratification_end_ms,
                            "kd=ts drop: too early (ratification window not closed)"
                        );
                    } else {
                        // Qodo PR #155 #2: derive `n` from the canonical
                        // ratification ordering, matching the IPC + engine-auto
                        // kd=rs paths. See the analogous comment in the
                        // RatificationBallot arm.
                        let sq = synthesize_status_quo(&self.meta.poll_id);
                        let sq_hash = sq.event_hash;
                        let mut all_candidates = self.candidates.clone();
                        all_candidates.push(sq);
                        let primary_size = self.meta.config.sortition_size as usize;
                        let n = match drafting_advancers(&all_candidates, primary_size, sq_hash) {
                            Some(advancers) => {
                                ratification_candidates_ordering(&advancers, sq_hash).len()
                            }
                            None => {
                                advance_last_hlc = false;
                                tracing::debug!(
                                    poll_id = %hex::encode(self.meta.poll_id.0),
                                    "kd=ts drop: pre-Drafting (status_quo not synthesized)"
                                );
                                0
                            }
                        };
                        let expected = n + n * (n - 1) / 2;
                        if payload.entries.len() != expected {
                            advance_last_hlc = false;
                            tracing::debug!(
                                poll_id = %hex::encode(self.meta.poll_id.0),
                                expected,
                                actual = payload.entries.len(),
                                "kd=ts drop: shape mismatch"
                            );
                        } else {
                            let oracle_state = self
                                .committee_oracle
                                .committee_at_epoch(payload.committee_epoch);
                            let t1_ok = oracle_state
                                .as_ref()
                                .is_some_and(|cs| cs.verifying_shares.contains_key(&ev.actor));
                            if !t1_ok {
                                advance_last_hlc = false;
                                tracing::debug!(
                                    poll_id = %hex::encode(self.meta.poll_id.0),
                                    actor = %hex::encode(ev.actor.0),
                                    epoch = payload.committee_epoch,
                                    "kd=ts drop: T1 actor not in committee at epoch"
                                );
                            } else {
                                // T2: per-entry DLEQ verify against (G, Y_i,
                                // c1_agg[idx], share). c1_agg comes from the
                                // homomorphic aggregate of accepted ballots.
                                let cs = oracle_state.unwrap();
                                let y_i_opt =
                                    crate::community_voting_tier3_crypto::decompress_point(
                                        &cs.verifying_shares[&ev.actor],
                                    );
                                let aggregates =
                                    aggregate_se_ballots(&self.ratification_ballots, n);
                                let all_dleq_ok = match (y_i_opt, aggregates) {
                                    (Some(y_i), Some(aggregates)) => {
                                        let g_point =
                                            curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
                                        let mut ok = true;
                                        for (idx, entry) in payload.entries.iter().enumerate() {
                                            let c1_agg = match crate::community_voting_tier3_crypto::decompress_point(&aggregates[idx].c1) {
                                                Some(p) => p,
                                                None => { ok = false; break; }
                                            };
                                            let d_i = match crate::community_voting_tier3_crypto::decompress_point(&entry.share) {
                                                Some(p) => p,
                                                None => { ok = false; break; }
                                            };
                                            let proof = match crate::community_voting_tier3_nizk::DleqProof::from_bytes(&entry.dleq_proof) {
                                                Some(p) => p,
                                                None => { ok = false; break; }
                                            };
                                            if !crate::community_voting_tier3_nizk::dleq_verify(
                                                &g_point, &y_i, &c1_agg, &d_i, &proof,
                                            ) {
                                                ok = false;
                                                break;
                                            }
                                        }
                                        ok
                                    }
                                    _ => false,
                                };
                                if !all_dleq_ok {
                                    advance_last_hlc = false;
                                    tracing::debug!(
                                        poll_id = %hex::encode(self.meta.poll_id.0),
                                        actor = %hex::encode(ev.actor.0),
                                        "kd=ts drop: T2 DLEQ verify failed (or aggregate decode failed)"
                                    );
                                } else {
                                    // Cursor PR #155 #3 + Qodo PR #155 #4:
                                    // true LWW upsert on (actor, committee_epoch).
                                    // Compare incoming (hlc, event_hash) against the
                                    // existing record's; overwrite only when strictly
                                    // greater. Stale arrivals are silent-dropped
                                    // (advance_last_hlc=false so the projection
                                    // watermark does not advance, ZEB-320).
                                    //
                                    // First-wins was incorrect because kd=ts DLEQ
                                    // proofs verify against the homomorphic aggregate
                                    // of all currently-accepted ballots; if a valid
                                    // ballot with an earlier HLC arrives after some
                                    // shares were applied, the aggregate changes and
                                    // those earlier shares no longer decrypt the new
                                    // aggregate, so the committee member must resubmit
                                    // and the resubmission must win.
                                    let event_hash = sha256_of_signing_bytes(ev);
                                    let key = (ev.actor, payload.committee_epoch);
                                    let should_insert =
                                        match self.secret_tally.tally_shares.get(&key) {
                                            None => true,
                                            Some(existing) => {
                                                let incoming = (
                                                    ev.hlc.wall_ms,
                                                    ev.hlc.logical,
                                                    ev.hlc.device_id.as_str(),
                                                    event_hash,
                                                );
                                                let current = (
                                                    existing.last_update_hlc.wall_ms,
                                                    existing.last_update_hlc.logical,
                                                    existing.last_update_hlc.device_id.as_str(),
                                                    existing.last_update_event_hash,
                                                );
                                                incoming > current
                                            }
                                        };
                                    if should_insert {
                                        self.secret_tally.tally_shares.insert(
                                            key,
                                            TallyShareRecord {
                                                entries: payload.entries.clone(),
                                                last_update_hlc: ev.hlc.clone(),
                                                last_update_event_hash: event_hash,
                                            },
                                        );
                                    } else {
                                        // Stale LWW: incoming (hlc, event_hash) ≤
                                        // existing; silent-drop watermark hold.
                                        advance_last_hlc = false;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // kd=cl PollClose: record close_event_hash.
            PollEventKindCode::PollClose => {
                // ZEB-859/858: first-close-wins. close_event_hash is the second half of
                // the verify_sr memo key; overwriting it on every applied kd=cl would let
                // an insider churn the key (distinct closes all pass verify_cl in the
                // pre-finalize window) and bypass the se-mode decrypt DoS bound. Freezing
                // on the first close per node is consistent with the engine-auto trigger's
                // `close_event_hash.is_some()` short-circuit and the documented
                // "close_event_hash may legitimately differ across replicas" tolerance
                // (each node freezes on its own first-observed close).
                if self.close_event_hash.is_none() {
                    self.close_event_hash = Some(sha256_of_signing_bytes(ev));
                }
            }

            // kd=rs PollResult (Tier 3): decode StarResult from payload;
            // transition to Finalized. Falls through to the unified tail.
            PollEventKindCode::PollResult => {
                let payload: Tier3PollResultPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                self.result = Some(payload.result);
                self.stage = Stage::Finalized;
            }

            // Tier 1 / Tier 2 only kinds — not valid for Tier 3 state machine.
            kind @ (PollEventKindCode::PollCreate
            | PollEventKindCode::PollOpen
            | PollEventKindCode::PollExtend
            | PollEventKindCode::BallotCast
            | PollEventKindCode::Signal
            | PollEventKindCode::Delegate
            | PollEventKindCode::Undelegate) => {
                return Err(ApplyError::InvalidKindForTier3(kind));
            }
        }

        // Raise this event's (actor, device) lane to max((wall_ms, logical)),
        // mirroring channel-log raise_watermark. Advances on every Ok (accept
        // or silent drop) — the ZEB-320 property, per lane — so the monotonic
        // guard keeps catching out-of-order delivery after a drop. last_hlc
        // only advances on accepts (ZEB-320).
        let lane = (ev.actor, ev.hlc.device_id.clone());
        let cand = (ev.hlc.wall_ms, ev.hlc.logical);
        let entry = self.last_received_hlc.entry(lane).or_insert((0, 0));
        if cand > *entry {
            *entry = cand;
        }
        // ZEB-860: advance the canonical out-of-order watermark on every dispatch
        // (accept or silent drop), beside the per-lane receive-watermark.
        let key3 = (ev.hlc.wall_ms, ev.hlc.logical, ev.hlc.device_id.clone());
        if self.max_applied.as_ref().is_none_or(|m| key3 > *m) {
            self.max_applied = Some(key3);
        }
        if advance_last_hlc {
            self.last_hlc = Some(ev.hlc.clone());
        }
        Ok(if advance_last_hlc {
            ApplyOutcome::Applied
        } else {
            ApplyOutcome::Dropped
        })
    }

    /// Highest `(wall_ms, logical)` across all per-(actor,device) receive
    /// lanes, as an `Hlc` with an empty `device_id`, or `None` before any
    /// dispatch. The engine-auto kd=rs mint floor needs a floor strictly above
    /// EVERY received event regardless of lane; `engine_auto_hlc_from_base`
    /// synthesizes its own device_id, so the empty one here is never used.
    pub fn max_received_hlc(&self) -> Option<Hlc> {
        // ZEB-861: O(1). The `(wall_ms, logical)` prefix of `max_applied` is
        // identical to the max over all per-lane `(wall_ms, logical)` in
        // `last_received_hlc` — both advance on every dispatch at the same tail,
        // and `max_applied` orders by `(wall_ms, logical)` first, so its prefix
        // is the global max. `device_id` is documented-unused by the sole
        // consumer (the kd=rs mint floor synthesizes its own), so it stays empty.
        self.max_applied
            .as_ref()
            .map(|(wall_ms, logical, _device)| Hlc {
                wall_ms: *wall_ms,
                logical: *logical,
                device_id: String::new(),
            })
    }

    /// Compute the effective Stage at HLC watermark `now`.
    ///
    /// Implements the `recompute_stage` logic from design spec §6:
    ///
    /// - Sortition → (no advance while sortition_result is None)
    /// - Deliberation: once sortition_result is Some AND now.wall_ms < create + dw_ms
    /// - Drafting: once now.wall_ms ≥ create + dw_ms (AND sortition done)
    /// - Ratification: once now.wall_ms ≥ create + dw_ms + fw_ms AND candidates non-empty
    /// - Finalized / Failed: terminal, returned as-is.
    pub fn current_stage_at(&self, now: &Hlc) -> Stage {
        // Terminal states are permanent regardless of HLC.
        match self.stage {
            Stage::Failed => return Stage::Failed,
            Stage::Finalized => return Stage::Finalized,
            _ => {}
        }

        // If no sortition result yet → still Stage 1.
        if self.sortition_result.is_none() {
            return Stage::Sortition;
        }

        let create_wall_ms = self.meta.poll_create_hlc.wall_ms;
        let dw_ms = self.meta.config.deliberation_window_seconds as u64 * 1000;
        let fw_ms = self.meta.config.drafting_window_seconds as u64 * 1000;

        let stage_2_threshold_ms = create_wall_ms.saturating_add(dw_ms);
        let stage_3_threshold_ms = stage_2_threshold_ms.saturating_add(fw_ms);

        if now.wall_ms < stage_2_threshold_ms {
            // Sortition done but deliberation window not yet elapsed.
            Stage::Deliberation
        } else if now.wall_ms < stage_3_threshold_ms {
            // Deliberation window elapsed but drafting window not yet elapsed.
            Stage::Drafting
        } else {
            // Both windows elapsed — Ratification if ≥1 candidate (status_quo counts).
            // Per spec §6 degenerate path: always Ratification (status_quo always present).
            Stage::Ratification
        }
    }

    /// ZEB-859: the deterministic engine-auto close condition for a `kd=cl`
    /// (PollClose) event, evaluated at a caller-supplied wall-clock ms.
    ///
    /// This is the SINGLE shared predicate for both:
    ///   * the engine-auto close trigger (`trigger_kd_cl` in
    ///     `community_voting_log_engine.rs`), which passes its receiver-clamped
    ///     `last_hlc.wall_ms`, and
    ///   * `verify_cl`, which passes the receiver-clamped wall of an INCOMING
    ///     peer close event.
    ///
    /// Keeping the two callers on this one predicate is load-bearing: any drift
    /// would let a node reject its own legitimately-triggered close (the trigger
    /// mint would satisfy a condition the verifier then rejects). Do NOT inline
    /// a second copy of this logic.
    ///
    /// The condition is: the poll is in `Ratification` stage at `at_wall_ms` AND
    /// the full lifecycle window (deliberation + drafting + ratification) has
    /// elapsed relative to `poll_create_hlc.wall_ms`. Callers are responsible
    /// for clamping `at_wall_ms` to the receiver's own clock (never a raw peer
    /// wall) before calling — this method trusts `at_wall_ms` as given.
    pub fn close_condition_met(&self, at_wall_ms: u64) -> bool {
        // Mirror the HLC the engine trigger builds: only `wall_ms` is
        // significant to `current_stage_at`; `logical`/`device_id` are inert.
        let at_hlc = Hlc {
            wall_ms: at_wall_ms,
            logical: 0,
            device_id: String::new(),
        };
        if !matches!(self.current_stage_at(&at_hlc), Stage::Ratification) {
            return false;
        }
        // Total window = deliberation + drafting + ratification.
        let total_window_ms: u64 = (self.meta.config.deliberation_window_seconds as u64
            + self.meta.config.drafting_window_seconds as u64
            + self.meta.config.ratification_window_seconds as u64)
            * 1000;
        let created_wall = self.meta.poll_create_hlc.wall_ms;
        at_wall_ms >= created_wall.saturating_add(total_window_ms)
    }

    /// Compute the current mini-public set at HLC watermark `now`.
    ///
    /// The mini-public is the first `sortition_size` non-declined members from the
    /// ordered concat of `primary || backup`. This naturally handles cascading declines
    /// (Cluster B fix, R2 bot review): if a promoted backup member also declines, the
    /// next backup takes their slot — both are skipped in the same walk.
    ///
    /// Returns empty set if sortition_result is None.
    pub fn current_mini_public(&self, now: &Hlc) -> HashSet<OwnerAddr> {
        let sr = match &self.sortition_result {
            Some(sr) => sr,
            None => return HashSet::new(),
        };

        // The target mini-public size is the length of the primary slice (which equals
        // sortition_size in a well-formed sortition). Using sr.primary.len() directly means
        // the logic is correct even in tests that construct a primary slice shorter than
        // meta.config.sortition_size.
        let target_size = sr.primary.len();

        // Collect declined actors (those who declined up to `now`).
        let declined: HashSet<OwnerAddr> = self
            .declines
            .iter()
            .filter(|(_, hlc)| {
                let h = (hlc.wall_ms, hlc.logical, hlc.device_id.as_str());
                let n = (now.wall_ms, now.logical, now.device_id.as_str());
                h <= n
            })
            .map(|(addr, _)| *addr)
            .collect();

        // Walk primary || backup in order; collect non-declined until we have target_size.
        // Handles promoted-then-declined backups correctly (Cluster B fix, R2 bot review):
        // both declined primaries and declined backups are skipped in a single unified pass.
        // A backup that was promoted (because a primary declined) and then itself declined
        // is simply skipped in the walk — the next backup takes the slot.
        let mut set = HashSet::new();
        for member in sr.primary.iter().chain(sr.backup.iter()) {
            if set.len() >= target_size {
                break;
            }
            if !declined.contains(member) {
                set.insert(*member);
            }
        }

        set
    }

    /// Build the (statements, votes, mini_public) tuple that the bridging
    /// algorithm consumes. Computed once per bridging IPC call so the
    /// authoritative-set snapshot is fixed for the entire computation
    /// (spec §4.7 determinism guarantee 4).
    #[allow(clippy::type_complexity)]
    pub fn bridging_inputs(
        &self,
        eval_hlc: &Hlc,
    ) -> (
        &std::collections::BTreeMap<[u8; 32], DeliberationStatement>,
        &std::collections::BTreeMap<(OwnerAddr, [u8; 32]), DeliberationVoteEntry>,
        std::collections::HashSet<OwnerAddr>,
    ) {
        let mini_public = self.current_mini_public(eval_hlc);
        (
            &self.deliberation.statements,
            &self.deliberation.votes,
            mini_public,
        )
    }

    /// Count of UNIQUE actors who declined at or before `now`.
    ///
    /// Cluster 4 fix (Qodo bug #3, R1 bot review): counts unique decliners
    /// rather than decline events. Without deduplication, repeated kd=md events
    /// from the same actor inflate the decline count, causing `current_mini_public`
    /// to over-promote backups (`.take(decline_count)` promotes one extra backup per
    /// repeat) and `verify_sf`'s backup-exhaustion check to fire prematurely.
    pub fn decline_count_at(&self, now: &Hlc) -> usize {
        self.declines
            .iter()
            .filter(|(_, hlc)| {
                let h = (hlc.wall_ms, hlc.logical, hlc.device_id.as_str());
                let n = (now.wall_ms, now.logical, now.device_id.as_str());
                h <= n
            })
            .map(|(addr, _)| addr)
            .collect::<HashSet<_>>()
            .len()
    }
}

// ── BeaconOracle trait ────────────────────────────────────────────────────────

/// Trait for looking up VRF beacon output by `(community_id, seed, epoch)`.
///
/// SS1 verify reconstructs sortition deterministically from VRF output, so
/// `verify_ss` must query the local DfrostLog state. This trait decouples
/// Task 7 verify logic from the actual `DfrostLogRegistry` wiring (Task 10).
///
/// Implementations:
/// - Production (Task 10): `Arc<DfrostLogRegistry>` looks up the committee's
///   `kd=vb` event in the local dfrost log.
/// - Tests (this task): `MockBeaconOracle` returns canned outputs.
///
/// `community_id` is the community space-id (not the poll). `seed` is the
/// seed value derived from `PollCreate.event_hash + community_epoch`.
/// `epoch` is the POLL'S community epoch at PollCreate time — NOT the live
/// engine epoch (Cluster D fix, R2 bot review). Using the poll's stored epoch
/// prevents a CHURP committee refresh between beacon creation and verify_ss
/// invocation from causing a message_hash mismatch and stalling the poll.
#[async_trait::async_trait]
pub trait BeaconOracle: Send + Sync {
    async fn vrf_output_for(
        &self,
        community_id: &crate::owner_state_types::SpaceId,
        seed: &[u8; 32],
        epoch: u64,
    ) -> Option<[u8; 32]>;
}

// ── VerifyError ───────────────────────────────────────────────────────────────

/// Errors returned by the `verify_*` functions in this module.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("sortition selection mismatch: recomputed differs from claimed")]
    SortitionMismatch,
    #[error("VRF beacon not yet available for this poll")]
    BeaconNotYetAvailable,
    #[error("actor {0:?} not in current mini-public set")]
    NotInMiniPublic(crate::owner_state_types::OwnerAddr),
    #[error("actor {0:?} not in eligible electorate")]
    NotInEligibleElectorate(crate::owner_state_types::OwnerAddr),
    #[error("SortitionFailed: actor {0:?} is not the proposer")]
    SfActorNotProposer(crate::owner_state_types::OwnerAddr),
    #[error(
        "SortitionFailed: backup pool not yet exhausted (declines {declined}, backup {capacity})"
    )]
    BackupPoolNotExhausted { declined: usize, capacity: usize },
    #[error("PollResult tally mismatch: recomputed differs from claimed")]
    TallyMismatch,
    /// se-mode `verify_sr`: fewer than `threshold` committee tally shares have
    /// been applied on this node, so the secret tally cannot yet be recomputed.
    /// Distinct from `TallyMismatch` — the claim is *unconfirmable here yet*, not
    /// forged (a later share may make it verifiable). Fail-closed, but retryable.
    #[error("tally shares not yet available to confirm result (below threshold)")]
    TallySharesNotReady,
    #[error("payload decode failed: {0}")]
    PayloadDecode(String),
    #[error("ballot validation failed: {0}")]
    BallotInvalid(#[from] ValidateError),
    #[error("ratification stage not active at event.hlc")]
    NotInRatificationStage,
    #[error("DraftApproval references unknown candidate")]
    UnknownCandidate,
    #[error("poll lifecycle not Closed at HLC (PollResult R1)")]
    NotInClosedStage,
    /// verify_sr / verify_ratification_ballot called before status_quo synthesized.
    /// This means the poll has not yet reached the Drafting/Ratification stage,
    /// so a PollResult or RatificationBallot event arriving now is premature.
    #[error("status_quo not yet synthesized (poll not in Drafting/Ratification stage)")]
    StatusQuoNotSynthesized,
    /// verify_cl (ZEB-859): a PollClose event whose (receiver-clamped) HLC wall
    /// does not satisfy the engine's deterministic close condition — the poll is
    /// not in Ratification at that wall, or the total window has not elapsed.
    #[error("close condition not met at event.hlc (premature PollClose)")]
    CloseConditionNotMet,
    /// verify_sr (ZEB-858): a kd=rs PollResult targeting a poll already in the
    /// terminal `Stage::Finalized`. The early-out rejects it BEFORE the se-mode
    /// threshold-decrypt (`recover_secret_tally`) runs — a finalized poll's
    /// result is immutable, so any further kd=rs is spurious or a forgery.
    #[error("poll already finalized; kd=rs is terminal-rejected")]
    PollAlreadyFinalized,
}

// ── Verify functions ──────────────────────────────────────────────────────────

/// SS1 verify: `SortitionSelection` recomputes deterministically from VRF beacon.
///
/// Per spec §5: the claimed `primary` + `backup` arrays must be bit-identical to
/// `fisher_yates_select(vrf_output, electorate_snapshot, sortition_size, sortition_size)`.
///
/// The beacon seed is derived from `meta.poll_create_event_hash` and `meta.community_epoch`
/// via `derive_beacon_seed`. The VRF output is fetched from the `BeaconOracle`; if the
/// beacon isn't present yet, returns `BeaconNotYetAvailable`.
pub async fn verify_ss(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
    beacon_oracle: &dyn BeaconOracle,
    community_id: &crate::owner_state_types::SpaceId,
) -> Result<(), VerifyError> {
    // 1. Decode SortitionSelectionPayload from event.payload.
    let payload: SortitionSelectionPayload =
        decode_payload(&event.payload).map_err(VerifyError::PayloadDecode)?;

    // 2. Derive beacon seed from poll_create_event_hash + community_epoch.
    let seed = derive_beacon_seed(
        &poll_state.meta.poll_create_event_hash,
        poll_state.meta.community_epoch,
    );

    // 3. Look up VRF output using the POLL'S epoch (Cluster D fix, R2 bot review).
    // Using the poll's stored epoch rather than the live engine epoch prevents a
    // CHURP committee refresh between beacon creation and verify_ss invocation from
    // causing a message_hash mismatch (live epoch != poll's epoch → lookup fails).
    let vrf_output = beacon_oracle
        .vrf_output_for(community_id, &seed, poll_state.meta.community_epoch)
        .await
        .ok_or(VerifyError::BeaconNotYetAvailable)?;

    // 4. Deterministically recompute sortition.
    let primary_size = poll_state.meta.config.sortition_size as usize;
    let backup_size = primary_size;
    let recomputed = fisher_yates_select(
        &vrf_output,
        &poll_state.eligible_electorate_snapshot,
        primary_size,
        backup_size,
    );

    // 5. Compare bit-identical.
    if recomputed.primary != payload.primary || recomputed.backup != payload.backup {
        return Err(VerifyError::SortitionMismatch);
    }

    Ok(())
}

/// SD1 verify: `event.actor` must be in the current mini-public set at `event.hlc`.
///
/// Applies to `kd=ds`, `kd=dc`, `kd=da`, `kd=md`.
pub fn verify_sd(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
) -> Result<(), VerifyError> {
    let mini_public = poll_state.current_mini_public(&event.hlc);
    if !mini_public.contains(&event.actor) {
        return Err(VerifyError::NotInMiniPublic(event.actor));
    }
    Ok(())
}

/// DA additional verify: the referenced candidate must exist in `poll_state.candidates`.
///
/// Call this in addition to `verify_sd` when processing `kd=da` DraftApproval events.
pub fn verify_da_candidate_exists(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
) -> Result<(), VerifyError> {
    let payload: DraftApprovalPayload =
        decode_payload(&event.payload).map_err(VerifyError::PayloadDecode)?;
    let exists = poll_state
        .candidates
        .iter()
        .any(|c| c.event_hash == payload.candidate_event_hash);
    if !exists {
        return Err(VerifyError::UnknownCandidate);
    }
    Ok(())
}

/// SF1 verify: `SortitionFailed` must be proposer-signed AND the backup pool must be
/// fully exhausted at `event.hlc` (i.e., `decline_count ≥ backup_pool_size`).
pub fn verify_sf(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
) -> Result<(), VerifyError> {
    // 1. Actor must be the poll proposer.
    if event.actor != poll_state.meta.proposer {
        return Err(VerifyError::SfActorNotProposer(event.actor));
    }

    // 2. Backup pool must be exhausted: decline_count ≥ primary_size + backup_size.
    //    Total pool = primary + backup = 2 × sortition_size by design.
    //    Read the actual pool lengths from sortition_result if available (covers
    //    any future asymmetric sizes); fall back to 2 × sortition_size from meta.
    let declined = poll_state.decline_count_at(&event.hlc);
    let capacity = poll_state
        .sortition_result
        .as_ref()
        .map(|sr| sr.primary.len() + sr.backup.len())
        .unwrap_or(2 * poll_state.meta.config.sortition_size as usize);
    if declined < capacity {
        return Err(VerifyError::BackupPoolNotExhausted { declined, capacity });
    }

    Ok(())
}

/// CL1 verify (ZEB-859): a peer-supplied `kd=cl` (PollClose) event is legitimate
/// only if it satisfies the SAME deterministic close condition the engine-auto
/// close trigger uses — the poll is in `Ratification` and the full lifecycle
/// window has elapsed. Because `kd=cl` was previously ungated at ingest (any
/// member with a valid signature could inject a forged early close), a forged
/// close could prematurely satisfy `verify_sr`'s R1 precondition; this verifier
/// closes that hole.
///
/// The incoming event's `hlc.wall_ms` is a peer-controlled value, so it is
/// clamped to the receiver's own clock (`receiver_now_ms + MAX_FORWARD_SKEW_MS`)
/// before the condition is checked — a future-stamped close cannot jump the
/// window. This mirrors the receiver-now clamp the engine trigger already
/// applies to `last_hlc.wall_ms` (ZEB-846 Layer 2).
///
/// Note: this validates the close *condition*, never a specific
/// `close_event_hash` — that hash may legitimately diverge across replicas under
/// reordering and is not in any cross-peer state-root.
pub fn verify_cl(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
    receiver_now_ms: u64,
) -> Result<(), VerifyError> {
    // Never trust the raw peer wall: clamp the event's HLC wall down to at most
    // receiver_now + MAX_FORWARD_SKEW_MS (a past/present wall passes unchanged).
    let clamped_wall = crate::clock_trust::clamp_future(
        event.hlc.wall_ms,
        receiver_now_ms,
        crate::clock_trust::MAX_FORWARD_SKEW_MS,
    );
    if poll_state.close_condition_met(clamped_wall) {
        Ok(())
    } else {
        Err(VerifyError::CloseConditionNotMet)
    }
}

/// ZEB-858: shared poll-state → expected-result core, used by BOTH [`verify_sr`]
/// and the memoized `kd=rs` ingest path (`community_voting_log_engine`).
///
/// Builds the deterministic ratification candidate ordering (status_quo synth +
/// `drafting_advancers` + `ratification_candidates_ordering`) and recomputes the
/// expected STAR result via [`recompute_expected_result`]. This is exactly the
/// part BOTH callers share; the R1 (close-applied) precondition and the
/// `payload.result` comparison stay in the callers.
///
/// The expensive work lives here: in se-mode the recompute is a threshold
/// decrypt (`recover_secret_tally` → Lagrange-combine + BSGS dlog). The result
/// is a **pure function of `poll_state`** (candidates + committee shares +
/// ratification ballots), so this takes no `event` — which is exactly what lets
/// the ingest path memoize it on `(poll_id, close_event_hash)` and skip the
/// recompute on a flood of distinct-signed kd=rs for the same closed poll.
///
/// Errors mirror `verify_sr`'s (post-decode) failure modes:
/// - `StatusQuoNotSynthesized` — poll not yet at/past Drafting open.
/// - `TallySharesNotReady` — se-mode, fewer than `threshold` committee shares
///   applied on this node yet (the claim is *unconfirmable here yet*, retryable).
pub(crate) fn expected_result_from_state(
    poll_state: &Tier3PollState,
) -> Result<crate::community_voting_star::StarResult, VerifyError> {
    // Build candidate list from state (same as the ordered list used at
    // ratification open). ZEB-850 (Task 2 completion): status_quo is synthesized
    // on demand and never persisted into `candidates`, so mirror the apply-time
    // locals (`Tier3PollState::apply_event`, community_voting_tier3.rs:742-743)
    // and include it before ranking — otherwise `drafting_advancers` returns None
    // and the caller rejects every legitimate peer kd=rs. Prematurity is caught by
    // the caller's R1 close-applied / stage checks.
    let sq = synthesize_status_quo(&poll_state.meta.poll_id);
    let sq_hash = sq.event_hash;
    let primary_size = poll_state.meta.config.sortition_size as usize;
    let mut all_candidates = poll_state.candidates.clone();
    all_candidates.push(sq);
    let advancers = drafting_advancers(&all_candidates, primary_size, sq_hash)
        .ok_or(VerifyError::StatusQuoNotSynthesized)?;
    let ordered_candidates = ratification_candidates_ordering(&advancers, sq_hash);

    // ZEB-850 (Task 2 completion): recompute the result the SAME way the engine
    // produces it, per privacy mode. se-mode is a threshold-decrypted tally
    // (`recover_secret_tally`, mirroring `try_finalize_secret_tally`), NOT a
    // plaintext STAR tally — `tally_star` skips se-mode ciphertext ballots and
    // would return an all-zero result. `recover_secret_tally` is deterministic
    // across replicas (Lagrange invariance), so this is a real authz check.
    //
    // ZEB-858: `None` (se-mode, fewer than `threshold` shares applied here) means
    // this node cannot YET confirm the claim → `TallySharesNotReady` (retryable),
    // distinct from a forged claim (which the caller maps to `TallyMismatch`).
    recompute_expected_result(poll_state, &ordered_candidates)
        .ok_or(VerifyError::TallySharesNotReady)
}

/// ZEB-858: decode the claimed `StarResult` from a `kd=rs` event payload,
/// mapping a decode failure to `PayloadDecode` (matching [`verify_sr`]). Exposed
/// for the memoized ingest path, which compares the claim against the memoized
/// expected result WITHOUT re-running the recompute.
pub(crate) fn decode_poll_result_claim(
    event: &SignedVotingEvent,
) -> Result<crate::community_voting_star::StarResult, VerifyError> {
    let payload: Tier3PollResultPayload =
        decode_payload(&event.payload).map_err(VerifyError::PayloadDecode)?;
    Ok(payload.result)
}

/// SR1 verify: `PollResult` tally must be bit-identical to deterministic re-compute.
///
/// R1 prerequisite: `poll_state.close_event_hash` must be `Some` (kd=cl already applied).
/// R2: re-run the per-privacy-mode recompute over `poll_state` and compare.
///
/// ZEB-858: the R2 recompute is now shared with the memoized ingest path via
/// [`expected_result_from_state`]; this verifier's observable behavior is
/// unchanged (R1 → payload decode → StatusQuoNotSynthesized/TallySharesNotReady
/// → TallyMismatch, in that order).
pub fn verify_sr(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
) -> Result<(), VerifyError> {
    // R1: PollClose must have been applied.
    if poll_state.close_event_hash.is_none() {
        return Err(VerifyError::NotInClosedStage);
    }

    // Decode the claimed result from the payload.
    let payload: Tier3PollResultPayload =
        decode_payload(&event.payload).map_err(VerifyError::PayloadDecode)?;

    // R2: recompute via the shared core and compare bit-for-bit.
    let expected = expected_result_from_state(poll_state)?;
    if expected != payload.result {
        return Err(VerifyError::TallyMismatch);
    }

    Ok(())
}

/// B1-B5 verify for `kd=rb` RatificationBallot (Tier 3 extension of Tier 1 B1-B5).
///
/// - B2: poll must be in `Stage::Ratification` at `event.hlc`.
/// - B3: `event.actor` must be in `eligible_electorate_snapshot` (full electorate, NOT mini-public).
/// - B4: `validate_ratification_ballot` — score length and range.
/// - B5: `privacy_mode == "pu"` (enforced at PollCreate via validate_tier3_poll_config,
///   but checked defensively here).
pub fn verify_ratification_ballot(
    event: &SignedVotingEvent,
    poll_state: &Tier3PollState,
) -> Result<(), VerifyError> {
    // B2: must be in Ratification stage at event.hlc.
    if poll_state.current_stage_at(&event.hlc) != Stage::Ratification {
        return Err(VerifyError::NotInRatificationStage);
    }

    // B3: actor must be in the full eligible electorate (NOT just mini-public).
    if !poll_state
        .eligible_electorate_snapshot
        .contains(&event.actor)
    {
        return Err(VerifyError::NotInEligibleElectorate(event.actor));
    }

    // B4: validate ballot payload (score length + range).
    let payload: RatificationBallotPayload =
        decode_payload(&event.payload).map_err(VerifyError::PayloadDecode)?;

    // Compute the ratification candidate count from state.
    // ZEB-850 (Task 2 completion): status_quo is synthesized on demand and never
    // persisted into `candidates`, so mirror the apply-time locals
    // (`Tier3PollState::apply_event`, community_voting_tier3.rs:742-743) and include
    // it before ranking — otherwise `drafting_advancers` returns None and this
    // verifier rejects every legitimate peer kd=rb. Prematurity is already caught
    // by the B2 Ratification-stage check above.
    let sq = synthesize_status_quo(&poll_state.meta.poll_id);
    let sq_hash = sq.event_hash;
    let primary_size = poll_state.meta.config.sortition_size as usize;
    let mut all_candidates = poll_state.candidates.clone();
    all_candidates.push(sq);
    let advancers = drafting_advancers(&all_candidates, primary_size, sq_hash)
        .ok_or(VerifyError::StatusQuoNotSynthesized)?;
    let expected_candidate_count = advancers.len();

    validate_ratification_ballot(&payload, expected_candidate_count)?;

    Ok(())
}

// ── Drafting math ─────────────────────────────────────────────────────────────

/// Maximum number of candidates that advance to ratification (including the
/// guaranteed status_quo slot). Hard cap per design spec §9.
pub const MAX_RATIFICATION_CANDIDATES: usize = 5;

/// Homomorphic aggregate of accepted se-mode ballots. Returns a
/// `Vec<EncCiphertext>` of length `n + C(n,2)` — `n` score-sum aggregates
/// first, then `C(n,2)` indicator-sum aggregates in unordered-pair
/// lexicographic order. Pure point-addition over decompressed Ristretto
/// points; spec §3.4 step 4.
///
/// Returns `None` if `ballots.is_empty()`, if any ballot has the wrong
/// ciphertext shape, or if any compressed point fails to decode.
///
/// Per-actor LWW dedup is the caller's responsibility — for Task 7
/// apply-time verification, `self.ratification_ballots` is the accept-stream
/// (each kd=rb event is filtered through B5 + NIZK verify before being
/// appended), so all entries are valid; spec §3.4 step 4 cross-references
/// the engine's `recover_secret_tally` for the production lww_dedup step.
pub fn aggregate_se_ballots(
    ballots: &[crate::community_voting_core::RatificationBallotPayload],
    n: usize,
) -> Option<Vec<crate::community_voting_core::EncCiphertext>> {
    use crate::community_voting_tier3_crypto::{compress_point, decompress_point};
    use curve25519_dalek::ristretto::RistrettoPoint;
    if ballots.is_empty() {
        return None;
    }
    let pair_count = n * (n - 1) / 2;
    let mut sums_score: Vec<(RistrettoPoint, RistrettoPoint)> =
        vec![(RistrettoPoint::default(), RistrettoPoint::default()); n];
    let mut sums_ind: Vec<(RistrettoPoint, RistrettoPoint)> =
        vec![(RistrettoPoint::default(), RistrettoPoint::default()); pair_count];
    for b in ballots {
        let cs = b.ciphertexts_scores.as_ref()?;
        let ci = b.ciphertexts_indicators.as_ref()?;
        if cs.len() != n || ci.len() != pair_count {
            return None;
        }
        for (i, ec) in cs.iter().enumerate() {
            sums_score[i].0 += decompress_point(&ec.c1)?;
            sums_score[i].1 += decompress_point(&ec.c2)?;
        }
        for (i, ec) in ci.iter().enumerate() {
            sums_ind[i].0 += decompress_point(&ec.c1)?;
            sums_ind[i].1 += decompress_point(&ec.c2)?;
        }
    }
    let mut out = Vec::with_capacity(n + pair_count);
    for (c1, c2) in sums_score {
        out.push(crate::community_voting_core::EncCiphertext {
            c1: compress_point(&c1),
            c2: compress_point(&c2),
        });
    }
    for (c1, c2) in sums_ind {
        out.push(crate::community_voting_core::EncCiphertext {
            c1: compress_point(&c1),
            c2: compress_point(&c2),
        });
    }
    Some(out)
}

/// ZEB-295 Phase 6: recover the aggregate STAR result from accumulated
/// `kd=ts` tally shares. Returns `None` if no epoch yet has ≥ threshold
/// valid shares whose Lagrange-combine + BSGS decode cleanly. Spec §3.4
/// + §5.3.
///
/// Iterates epochs in descending order (latest first); falls through to
/// earlier epochs on insufficient-quorum / decryption-failure. Multi-
/// engine deterministic: any size-`threshold` subset of `(i, x_i)` pairs
/// from the SAME polynomial reconstructs the same secret (Lagrange
/// invariance), so all replicas converge on the same plaintext result.
///
/// `ordered_candidates`: the deterministic ratification ordering produced
/// by `ratification_candidates_ordering`. Length = `n` (score-sum
/// aggregate count).
pub fn recover_secret_tally(
    poll: &Tier3PollState,
    ordered_candidates: &[crate::community_voting_star::CandidateRef],
) -> Option<crate::community_voting_star::StarResult> {
    use crate::community_voting_tier3_crypto::{bsgs, combine_shares, decompress_point};
    use std::collections::BTreeMap;

    let n = ordered_candidates.len();
    if n == 0 {
        return None;
    }
    let pair_count = n * (n - 1) / 2;

    // Group received tally_shares by epoch. tally_shares is keyed by
    // (OwnerAddr, epoch) → TallyShareRecord; we want
    // epoch → [(addr, &entries)…] for Lagrange combination below.
    let mut by_epoch: BTreeMap<
        u64,
        Vec<(
            OwnerAddr,
            &Vec<crate::community_voting_core::TallyShareEntry>,
        )>,
    > = BTreeMap::new();
    for ((addr, epoch), record) in &poll.secret_tally.tally_shares {
        by_epoch
            .entry(*epoch)
            .or_default()
            .push((*addr, &record.entries));
    }

    // LWW-dedup of ratification ballots by actor — last (HLC, event_hash)
    // wins. For Phase 6 v1 we treat each accepted payload as already
    // canonical (the apply path enforces 1-per-eligible-electorate via
    // verify rule + monotonic-HLC discipline). See §3.1 — LWW dedup at
    // tally time is a tally-side concern; for the current apply layer
    // this is a no-op.
    let lww_ballots = lww_dedup_se_ballots(&poll.ratification_ballots);
    let ballot_count = lww_ballots.len() as u64;

    // Try epochs newest-first. Fall through on failure.
    for (epoch, shares) in by_epoch.iter().rev() {
        let cs = match poll.committee_oracle.committee_at_epoch(*epoch) {
            Some(s) => s,
            None => continue,
        };
        if shares.len() < cs.threshold as usize {
            continue;
        }

        let aggregates = match aggregate_se_ballots(&lww_ballots, n) {
            Some(a) => a,
            None => continue,
        };
        if aggregates.len() != n + pair_count {
            continue;
        }

        // Build the OwnerAddr → 1-indexed FROST identifier map. The
        // committee oracle's verifying_shares BTreeMap is sorted by
        // OwnerAddr lex ASC (BTreeMap natural order); identifiers are
        // 1-indexed in that sorted order — same derivation as
        // `community_dfrost_crypto::identifier_for_index` over the
        // committee's sorted member list.
        let addr_to_id: BTreeMap<OwnerAddr, u16> = cs
            .verifying_shares
            .keys()
            .copied()
            .enumerate()
            .map(|(i, addr)| (addr, (i + 1) as u16))
            .collect();

        // Lagrange-combine + BSGS-decode every aggregate. Any per-
        // aggregate failure falls through to the next-older epoch.
        let mut score_sums: Vec<u64> = Vec::with_capacity(n);
        let mut ind_sums: Vec<u64> = Vec::with_capacity(pair_count);
        let electorate_size = poll.eligible_electorate_snapshot.len() as u64;
        let mut decryption_ok = true;

        // Take the first `threshold` shares (sorted by OwnerAddr ASC since
        // BTreeMap iteration order is canonical). All replicas pick the
        // same subset given the same shares; even if subsets differ
        // across replicas, Lagrange invariance guarantees the same
        // plaintext (see spec §3.4 multi-engine determinism note).
        let selected: Vec<(
            OwnerAddr,
            &Vec<crate::community_voting_core::TallyShareEntry>,
        )> = {
            let mut s: Vec<_> = shares.iter().map(|(a, e)| (*a, *e)).collect();
            s.sort_by_key(|(a, _)| *a);
            s.truncate(cs.threshold as usize);
            s
        };

        for idx in 0..(n + pair_count) {
            let c1_agg = match decompress_point(&aggregates[idx].c1) {
                Some(p) => p,
                None => {
                    decryption_ok = false;
                    break;
                }
            };
            let c2_agg = match decompress_point(&aggregates[idx].c2) {
                Some(p) => p,
                None => {
                    decryption_ok = false;
                    break;
                }
            };

            // Build the partial-share map for this aggregate, keyed by
            // FROST identifier.
            let mut partial: BTreeMap<u16, curve25519_dalek::ristretto::RistrettoPoint> =
                BTreeMap::new();
            let mut ok = true;
            for (addr, entries) in &selected {
                let id = match addr_to_id.get(addr) {
                    Some(i) => *i,
                    None => {
                        ok = false;
                        break;
                    }
                };
                // Shape gate: each member's entries vec must match
                // n + pair_count.
                if entries.len() != n + pair_count {
                    ok = false;
                    break;
                }
                let share_pt = match decompress_point(&entries[idx].share) {
                    Some(p) => p,
                    None => {
                        ok = false;
                        break;
                    }
                };
                partial.insert(id, share_pt);
            }
            if !ok {
                decryption_ok = false;
                break;
            }

            let d_agg = match combine_shares(&c1_agg, &partial) {
                Some(d) => d,
                None => {
                    decryption_ok = false;
                    break;
                }
            };
            let m_point = c2_agg - d_agg;

            // Bounds per spec §4.6: score-sum bound = electorate × 5,
            // indicator-sum bound = electorate.
            let bound = if idx < n {
                electorate_size.saturating_mul(5)
            } else {
                electorate_size
            };
            let m = match bsgs(&m_point, bound) {
                Some(m) => m,
                None => {
                    decryption_ok = false;
                    break;
                }
            };
            if idx < n {
                score_sums.push(m);
            } else {
                ind_sums.push(m);
            }
        }

        if !decryption_ok {
            continue;
        }

        return Some(crate::community_voting_star::compute_star_from_sums(
            ordered_candidates,
            score_sums,
            ind_sums,
            ballot_count,
        ));
    }
    None
}

/// Recompute the PollResult a legitimate producer would claim, per privacy mode.
///
/// This is the single deterministic tally-recompute shared by `verify_sr` (and,
/// from ZEB-857/859, the ingest arm). It mirrors the engine's own result
/// production:
/// - `se`: threshold-decrypted secret tally (`recover_secret_tally`). Returns
///   `None` when fewer than `threshold` committee shares have been applied on
///   this node — the tally is not yet recoverable here (retryable), which the
///   caller must distinguish from a genuine mismatch.
/// - otherwise (`pu`): plaintext STAR tally (`tally_star`), always `Some`.
///
/// `ordered_candidates` must be the canonical ratification ordering
/// (`ratification_candidates_ordering`); its length is the spec's `n`.
fn recompute_expected_result(
    poll_state: &Tier3PollState,
    ordered_candidates: &[crate::community_voting_star::CandidateRef],
) -> Option<crate::community_voting_star::StarResult> {
    match poll_state.meta.config.privacy_mode.as_str() {
        "se" => recover_secret_tally(poll_state, ordered_candidates),
        _ => Some(tally_star(
            ordered_candidates,
            &poll_state.ratification_ballots,
        )),
    }
}

/// LWW-dedup helper for se-mode ratification ballots. Phase 6 v1 ships a
/// pass-through: the apply path already enforces 1-per-actor via
/// `current_mini_public` membership + monotonic-HLC discipline. A real
/// dedup would walk `ratification_ballots` newest-first by HLC and keep
/// the latest entry per actor — but the current Vec preserves arrival
/// order with no actor tag (the actor is on the SignedVotingEvent, not
/// the payload). v1 treats accepted payloads as canonical.
fn lww_dedup_se_ballots(
    ballots: &[crate::community_voting_core::RatificationBallotPayload],
) -> Vec<crate::community_voting_core::RatificationBallotPayload> {
    ballots.to_vec()
}

/// Deterministic synthetic status_quo candidate.
///
/// `event_hash = sha256(poll_id.0 || b"status_quo")`.
///
/// Inserted into the candidates list by `materialize()` at drafting open.
/// The hash is stable across nodes because it depends only on the poll_id
/// bytes, not on any event order.
pub fn synthesize_status_quo(poll_id: &PollId) -> DraftCandidateState {
    let mut hasher = Sha256::new();
    hasher.update(poll_id.0);
    hasher.update(b"status_quo");
    DraftCandidateState {
        event_hash: hasher.finalize().into(),
        text: "<status quo>".into(),
        proposer: None,
        approvals: std::collections::HashSet::new(),
    }
}

/// Top-N drafting advancers (status_quo always last) per design spec §9.
///
/// Threshold: `ceil(mini_public_size / 2)` = `(mini_public_size + 1) / 2`.
/// Cap: `MAX_RATIFICATION_CANDIDATES = 5` (status_quo counts toward the cap,
/// so at most 4 non-status-quo candidates can advance).
/// Status quo always advances regardless of threshold.
/// Non-status-quo candidates are filtered by threshold, sorted by
/// `approval_count DESC`, ties by `candidate_event_hash lex ASC`.
///
/// Returns `None` if `status_quo_hash` is not present in `candidates`.
/// This happens when the poll has not yet reached the Drafting stage
/// (status_quo is synthesized by `materialize()` at drafting open). Callers
/// that require status_quo — e.g. `verify_sr` and `verify_ratification_ballot`
/// — must return an appropriate error (`VerifyError::StatusQuoNotSynthesized`)
/// rather than panicking.
pub fn drafting_advancers(
    candidates: &[DraftCandidateState],
    mini_public_size: usize,
    status_quo_hash: CandidateEventHash,
) -> Option<Vec<crate::community_voting_star::CandidateRef>> {
    use crate::community_voting_star::CandidateRef;

    let threshold = mini_public_size.div_ceil(2); // ceil(N/2)

    // Step 1: filter non-status-quo candidates by approval threshold.
    let mut threshold_passers: Vec<&DraftCandidateState> = candidates
        .iter()
        .filter(|c| c.event_hash != status_quo_hash)
        .filter(|c| c.approvals.len() >= threshold)
        .collect();

    // Step 2: sort by approval_count DESC, ties by event_hash ASC.
    threshold_passers.sort_by(|a, b| {
        b.approvals
            .len()
            .cmp(&a.approvals.len())
            .then_with(|| a.event_hash.cmp(&b.event_hash))
    });

    // Step 3: take top (MAX_RATIFICATION_CANDIDATES - 1) — leave room for status_quo.
    let mut advancers: Vec<CandidateRef> = threshold_passers
        .into_iter()
        .take(MAX_RATIFICATION_CANDIDATES - 1)
        .map(|c| CandidateRef {
            event_hash: c.event_hash,
            approval_count: c.approvals.len() as u32,
        })
        .collect();

    // Step 4: status_quo always advances, always last.
    // Return None if not present (poll not yet at Drafting stage).
    let status_quo = candidates
        .iter()
        .find(|c| c.event_hash == status_quo_hash)?;
    advancers.push(CandidateRef {
        event_hash: status_quo.event_hash,
        approval_count: status_quo.approvals.len() as u32,
    });

    Some(advancers)
}

/// Collect all RatificationBallot ballots applied to a Tier 3 poll.
///
/// Returns a reference to the underlying `ratification_ballots` slice,
/// which is appended to in event-arrival order — equivalent to HLC order
/// because `Tier3PollState::apply_event` enforces a monotonic-HLC check
/// before mutating state.
///
/// Used by ZEB-310 Task 11's engine-auto kd=rs orchestration: the hook
/// calls this helper to get the ballot slice, then re-runs `tally_star`
/// over it to produce a deterministic STAR result that converges
/// bit-identically across all engines.
///
/// Note: this helper is intentionally a `&[RatificationBallotPayload]`
/// reference rather than a copy. `tally_star` takes a borrowed slice;
/// no allocation needed.
pub fn collect_ratification_ballots(
    t3: &Tier3PollState,
) -> &[crate::community_voting_core::RatificationBallotPayload] {
    &t3.ratification_ballots
}

/// Final ratification candidate ordering: `approval_count DESC`,
/// ties by `candidate_event_hash lex ASC`.
///
/// Status quo has `approval_count = 0` (no approvals by design), so it
/// naturally sorts last unless a real candidate also has zero approvals —
/// in which case lex on `event_hash` breaks the tie deterministically.
///
/// The result is what `kd=rb RatificationBallot.scores` arrays index against.
/// Call this function once at Stage 3 → Stage 4 transition and cache the result
/// so all kd=rb events reference the same ordering.
///
/// Input `advancers` are the `CandidateRef`s that `drafting_advancers`
/// returned (threshold filter + status_quo inclusion + cap already applied).
pub fn ratification_candidates_ordering(
    advancers: &[crate::community_voting_star::CandidateRef],
    _status_quo_hash: CandidateEventHash,
) -> Vec<crate::community_voting_star::CandidateRef> {
    let mut ordered = advancers.to_vec();
    // Sort by approval_count DESC, ties by event_hash ASC.
    // Status quo's approval_count == 0 → naturally last unless other zero-approval
    // candidates exist, in which case lex ASC tiebreaks.
    ordered.sort_by(|a, b| {
        b.approval_count
            .cmp(&a.approval_count)
            .then_with(|| a.event_hash.cmp(&b.event_hash))
    });
    ordered
}

// ── NoBeaconOracle stub ───────────────────────────────────────────────────────

/// A `BeaconOracle` that always returns `None` (beacon not yet available).
///
/// Used in Phase 4a-main tests and in the dispatch layer before Task 10 wires
/// the real `DfrostLogRegistry` handle. `verify_ss` returns
/// `VerifyError::BeaconNotYetAvailable` when this oracle is in use.
pub struct NoBeaconOracle;

#[async_trait::async_trait]
impl BeaconOracle for NoBeaconOracle {
    async fn vrf_output_for(
        &self,
        _community_id: &crate::owner_state_types::SpaceId,
        _seed: &[u8; 32],
        _epoch: u64,
    ) -> Option<[u8; 32]> {
        None
    }
}

// ── DfrostBeaconOracle ────────────────────────────────────────────────────────

/// Production `BeaconOracle` backed by a `DfrostLogRegistry<R>`. Looks up
/// the VRF output for a beacon seed by querying the engine's `beacon_index`.
///
/// Cluster D fix (R2 bot review): `vrf_output_for` now takes `epoch` from the
/// CALLER (i.e., the poll's stored epoch at PollCreate time) rather than
/// querying `engine.current_epoch()`. This prevents a CHURP committee refresh
/// between beacon creation and verify_ss invocation from causing the oracle to
/// look up the beacon under the wrong epoch → message_hash mismatch → poll stalls.
///
/// Requires `cfg(not(test))` to be `pub` — in tests, `MockBeaconOracle` is
/// used directly. Both are `pub` at module level for integration use.
///
/// Task 10 architectural decision: DfrostBeaconOracle lives in
/// community_voting_tier3.rs alongside BeaconOracle so all oracle impls are
/// co-located with the trait definition.
pub struct DfrostBeaconOracle<R: tauri::Runtime> {
    pub registry: std::sync::Arc<crate::community_dfrost_log_engine::DfrostLogRegistry<R>>,
}

#[async_trait::async_trait]
impl<R: tauri::Runtime> BeaconOracle for DfrostBeaconOracle<R> {
    async fn vrf_output_for(
        &self,
        community_id: &crate::owner_state_types::SpaceId,
        seed: &[u8; 32],
        epoch: u64,
    ) -> Option<[u8; 32]> {
        let engine = self.registry.get(*community_id).await?;
        // Use the caller-supplied epoch (the poll's stored community_epoch at PollCreate time),
        // NOT engine.current_epoch(). This is the Cluster D fix: if a CHURP refresh happens
        // between beacon creation and verify_ss, the live epoch != poll's epoch; using the
        // poll's epoch ensures we look up the beacon under the epoch it was originally indexed.
        engine.find_vrf_beacon_output_by_seed(seed, epoch).await
    }
}

// ── Tier 3 PollResult payload ─────────────────────────────────────────────────

/// Payload for kd=rs PollResult when tier == Tier::Sortition.
/// Carries the computed StarResult so any node can verify SR1 by re-running
/// tally_star over the kd=rb events at event.hlc and comparing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Tier3PollResultPayload {
    /// PollId this result is for.
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    /// The STAR tally result.
    #[serde(rename = "rs")]
    pub result: StarResult,
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Decode a CBOR payload bytes into T, mapping ciborium error to a String.
fn decode_payload<T>(bytes: &[u8]) -> Result<T, String>
where
    T: for<'de> serde::Deserialize<'de>,
{
    ciborium::from_reader(bytes).map_err(|e| e.to_string())
}

/// SHA-256 of `signing_bytes` for the event. Public wrapper around
/// `sha256_of_signing_bytes` exposed for the IPC layer (Tasks 4-8) so
/// callers can return the event_hash hex for chaining (DraftApproval
/// references DraftCandidate hashes; PollClose references statement hashes).
pub fn event_hash_of(event: &SignedVotingEvent) -> [u8; 32] {
    sha256_of_signing_bytes(event)
}

/// Compute SHA-256 of the signing bytes of a SignedVotingEvent.
/// Used to derive event_hash for kd=dc DraftCandidate and kd=cl PollClose.
fn sha256_of_signing_bytes(ev: &SignedVotingEvent) -> [u8; 32] {
    match ev.signing_bytes() {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            hasher.finalize().into()
        }
        Err(_) => {
            // Fallback: hash the raw payload bytes. This branch should be
            // unreachable in practice (signing_bytes only fails on IO error
            // when writing to a Vec, which never fails).
            let mut hasher = Sha256::new();
            hasher.update(&ev.payload);
            hasher.finalize().into()
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_voting_core::{Eligibility, Tier};
    use crate::community_voting_star::CandidateRef;

    // ── Test fixtures ─────────────────────────────────────────────────────────

    fn hlc(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn addr(byte: u8) -> OwnerAddr {
        OwnerAddr([byte; 16])
    }

    fn poll_id() -> PollId {
        PollId([0x01; 32])
    }

    fn default_config() -> Tier3PollConfigPayload {
        Tier3PollConfigPayload {
            proposal_text: "Amend charter §3".into(),
            sortition_size: 5,
            deliberation_window_seconds: 10, // 10s deliberation
            drafting_window_seconds: 10,     // 10s drafting
            ratification_window_seconds: 10,
            privacy_mode: "pu".into(),
            incentive_mode: "d".into(),
            eligibility: Eligibility {
                min_power: 1,
                min_vouching_depth: None,
                sortition_size: None,
            },
            retry_of: None,
        }
    }

    fn meta_at(wall_ms: u64) -> Tier3PollMeta {
        Tier3PollMeta {
            poll_id: poll_id(),
            proposer: addr(0xff),
            poll_create_hlc: hlc(wall_ms),
            config: default_config(),
            poll_create_event_hash: [0xaa; 32],
            community_epoch: 1,
        }
    }

    fn electorate(n: u8) -> Vec<OwnerAddr> {
        (0..n).map(addr).collect()
    }

    fn new_poll(create_wall_ms: u64) -> Tier3PollState {
        Tier3PollState::new_from_create(meta_at(create_wall_ms), electorate(20))
    }

    fn make_event(kind: PollEventKindCode, wall_ms: u64, actor: OwnerAddr) -> SignedVotingEvent {
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind,
            hlc: hlc(wall_ms),
            actor,
            payload: vec![],
            sig: vec![0u8; 64],
        }
    }

    fn make_event_with_payload(
        kind: PollEventKindCode,
        wall_ms: u64,
        actor: OwnerAddr,
        payload: Vec<u8>,
    ) -> SignedVotingEvent {
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind,
            hlc: hlc(wall_ms),
            actor,
            payload,
            sig: vec![0u8; 64],
        }
    }

    fn encode<T: serde::Serialize>(v: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(v, &mut buf).expect("encode");
        buf
    }

    fn ss_event(
        wall_ms: u64,
        primary: Vec<OwnerAddr>,
        backup: Vec<OwnerAddr>,
    ) -> SignedVotingEvent {
        let payload = SortitionSelectionPayload {
            poll_id: poll_id(),
            primary,
            backup,
        };
        make_event_with_payload(
            PollEventKindCode::SortitionSelection,
            wall_ms,
            addr(0xfe),
            encode(&payload),
        )
    }

    fn md_event(wall_ms: u64, actor: OwnerAddr) -> SignedVotingEvent {
        let payload = MiniPublicDeclinePayload {
            poll_id: poll_id(),
            reason: None,
        };
        make_event_with_payload(
            PollEventKindCode::MiniPublicDecline,
            wall_ms,
            actor,
            encode(&payload),
        )
    }

    fn dc_event(wall_ms: u64, actor: OwnerAddr, text: &str) -> SignedVotingEvent {
        let payload = DraftCandidatePayload {
            poll_id: poll_id(),
            text: text.into(),
        };
        make_event_with_payload(
            PollEventKindCode::DraftCandidate,
            wall_ms,
            actor,
            encode(&payload),
        )
    }

    fn da_event(
        wall_ms: u64,
        actor: OwnerAddr,
        candidate_hash: CandidateEventHash,
    ) -> SignedVotingEvent {
        let payload = DraftApprovalPayload {
            poll_id: poll_id(),
            candidate_event_hash: candidate_hash,
        };
        make_event_with_payload(
            PollEventKindCode::DraftApproval,
            wall_ms,
            actor,
            encode(&payload),
        )
    }

    fn sf_event(wall_ms: u64) -> SignedVotingEvent {
        let payload = SortitionFailedPayload { poll_id: poll_id() };
        make_event_with_payload(
            PollEventKindCode::SortitionFailed,
            wall_ms,
            addr(0xff),
            encode(&payload),
        )
    }

    fn rb_event(wall_ms: u64, actor: OwnerAddr, scores: Vec<u8>) -> SignedVotingEvent {
        let payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(scores),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        make_event_with_payload(
            PollEventKindCode::RatificationBallot,
            wall_ms,
            actor,
            encode(&payload),
        )
    }

    fn ds_event(wall_ms: u64, actor: OwnerAddr) -> SignedVotingEvent {
        let payload = DeliberationStatementPayload {
            poll_id: poll_id(),
            text: "I think we should consider option A.".into(),
        };
        make_event_with_payload(
            PollEventKindCode::DeliberationStatement,
            wall_ms,
            actor,
            encode(&payload),
        )
    }

    fn star_result() -> StarResult {
        StarResult {
            winner: CandidateRef {
                event_hash: [0xab; 32],
                approval_count: 0,
            },
            finalists: vec![
                CandidateRef {
                    event_hash: [0xab; 32],
                    approval_count: 0,
                },
                CandidateRef {
                    event_hash: [0xcd; 32],
                    approval_count: 0,
                },
            ],
            total_scores: vec![30, 20],
            runoff_votes: vec![3, 1],
        }
    }

    fn rs_event(wall_ms: u64) -> SignedVotingEvent {
        let payload = Tier3PollResultPayload {
            poll_id: poll_id(),
            result: star_result(),
        };
        make_event_with_payload(
            PollEventKindCode::PollResult,
            wall_ms,
            addr(0xfe),
            encode(&payload),
        )
    }

    // ── Test 1: new poll starts in Sortition stage ─────────────────────────────

    #[test]
    fn new_poll_starts_in_sortition_stage() {
        let poll = new_poll(1000);
        assert_eq!(poll.stage, Stage::Sortition);
        assert!(poll.sortition_result.is_none());
        assert!(poll.candidates.is_empty());
        assert!(poll.declines.is_empty());
        assert!(poll.last_hlc.is_none());
    }

    // ── Test 2: apply kd=ss sets sortition_result ─────────────────────────────
    // Stage advance is via current_stage_at, not apply_event directly.

    #[test]
    fn apply_kd_ss_sets_sortition_result_and_advances_via_current_stage_at() {
        let mut poll = new_poll(0);
        let primary = vec![addr(1), addr(2)];
        let backup = vec![addr(3)];
        let ev = ss_event(500, primary.clone(), backup.clone());
        poll.apply_event(&ev).expect("apply ss");

        assert!(
            poll.sortition_result.is_some(),
            "sortition_result must be set"
        );
        let sr = poll.sortition_result.as_ref().unwrap();
        assert_eq!(sr.primary, primary);
        assert_eq!(sr.backup, backup);

        // current_stage_at should now return Deliberation (within dw window).
        let stage = poll.current_stage_at(&hlc(500));
        assert_eq!(stage, Stage::Deliberation);
    }

    // ── Test 3: current_stage_at before sortition returns Sortition ───────────

    #[test]
    fn current_stage_at_before_sortition_returns_sortition() {
        let poll = new_poll(0);
        assert_eq!(poll.current_stage_at(&hlc(1_000_000)), Stage::Sortition);
    }

    // ── Test 4: current_stage_at during deliberation window returns Deliberation

    #[test]
    fn current_stage_at_during_dw_returns_deliberation() {
        let mut poll = new_poll(0); // create at wall_ms=0, dw=10s=10000ms, fw=10s
        let ev = ss_event(100, vec![addr(1)], vec![addr(2)]);
        poll.apply_event(&ev).expect("apply ss");
        // now=5000ms < create(0) + dw(10000ms) = 10000ms → Deliberation
        assert_eq!(poll.current_stage_at(&hlc(5_000)), Stage::Deliberation);
    }

    // ── Test 5: current_stage_at after dw+fw with candidates returns Ratification

    #[test]
    fn current_stage_at_after_full_window_with_candidates_returns_ratification() {
        let mut poll = new_poll(0); // dw=10s, fw=10s → ratification after 20000ms
        let ev = ss_event(100, vec![addr(1)], vec![addr(2)]);
        poll.apply_event(&ev).expect("apply ss");

        // Add a candidate so we're not degenerate.
        let dc = dc_event(200, addr(1), "proposal A");
        poll.apply_event(&dc).expect("apply dc");

        // now=25000ms ≥ 0 + 10000 + 10000 = 20000ms → Ratification
        assert_eq!(poll.current_stage_at(&hlc(25_000)), Stage::Ratification);
    }

    // ── Test 4b: current_stage_at between dw and dw+fw returns Drafting ────────

    #[test]
    fn current_stage_at_between_dw_and_fw_returns_drafting() {
        let mut poll = new_poll(0); // dw=10000ms, fw=10000ms
        let ev = ss_event(100, vec![addr(1)], vec![addr(2)]);
        poll.apply_event(&ev).expect("apply ss");
        // now=15000ms: ≥ dw threshold (10000) but < dw+fw threshold (20000) → Drafting
        assert_eq!(poll.current_stage_at(&hlc(15_000)), Stage::Drafting);
    }

    // ── Test 6: apply kd=md appends to declines ────────────────────────────────

    #[test]
    fn apply_kd_md_appends_to_declines() {
        let mut poll = new_poll(0);
        // Apply ss first so poll has sortition result (not strictly required for declines).
        poll.apply_event(&ss_event(100, vec![addr(1)], vec![addr(2)]))
            .expect("ss");
        poll.apply_event(&md_event(200, addr(1))).expect("md");
        assert_eq!(poll.declines.len(), 1);
        assert_eq!(poll.declines[0].0, addr(1));
        assert_eq!(poll.declines[0].1.wall_ms, 200);
    }

    // ── Test 7: multiple kd=md promote backups in order ────────────────────────

    #[test]
    fn multiple_kd_md_promote_backups_in_order() {
        let mut poll = new_poll(0);
        // primary=[1,2,3], backup=[10,11]
        let ev = ss_event(
            100,
            vec![addr(1), addr(2), addr(3)],
            vec![addr(10), addr(11)],
        );
        poll.apply_event(&ev).expect("ss");

        // Member 1 declines.
        poll.apply_event(&md_event(200, addr(1))).expect("md1");
        let mp = poll.current_mini_public(&hlc(300));
        // primary=[2,3] + backup[0]=10
        assert!(mp.contains(&addr(2)));
        assert!(mp.contains(&addr(3)));
        assert!(mp.contains(&addr(10)));
        assert!(!mp.contains(&addr(1)));
        assert_eq!(mp.len(), 3);

        // Member 2 declines.
        poll.apply_event(&md_event(400, addr(2))).expect("md2");
        let mp2 = poll.current_mini_public(&hlc(500));
        // primary=[3] + backup[0]=10 + backup[1]=11
        assert!(mp2.contains(&addr(3)));
        assert!(mp2.contains(&addr(10)));
        assert!(mp2.contains(&addr(11)));
        assert!(!mp2.contains(&addr(1)));
        assert!(!mp2.contains(&addr(2)));
        assert_eq!(mp2.len(), 3);
    }

    // ── Test 8: apply kd=dc appends candidate with implicit self-approval ──────

    #[test]
    fn apply_kd_dc_appends_candidate_with_implicit_self_approval() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(100, vec![addr(1)], vec![]))
            .expect("ss");
        poll.apply_event(&dc_event(200, addr(1), "my great proposal"))
            .expect("dc");
        assert_eq!(poll.candidates.len(), 1);
        let c = &poll.candidates[0];
        assert_eq!(c.text, "my great proposal");
        assert_eq!(c.proposer, Some(addr(1)));
        assert!(
            c.approvals.contains(&addr(1)),
            "implicit self-approval missing"
        );
        assert_eq!(c.approvals.len(), 1);
    }

    // ── Test 9: apply kd=da adds actor to named candidate's approvals ──────────

    #[test]
    fn apply_kd_da_adds_actor_to_approvals_of_named_candidate() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(100, vec![addr(1), addr(2)], vec![]))
            .expect("ss");
        let dc = dc_event(200, addr(1), "proposal");
        poll.apply_event(&dc).expect("dc");

        // Get the hash of the dc event.
        let candidate_hash = sha256_of_signing_bytes(&dc);
        poll.apply_event(&da_event(300, addr(2), candidate_hash))
            .expect("da");

        let c = &poll.candidates[0];
        assert!(c.approvals.contains(&addr(1)), "self-approval present");
        assert!(c.approvals.contains(&addr(2)), "addr(2) approval present");
        assert_eq!(c.approvals.len(), 2);
    }

    // ── Test 10: apply kd=da is idempotent on repeated actor ──────────────────

    #[test]
    fn apply_kd_da_is_idempotent_on_repeated_actor() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(100, vec![addr(1), addr(2)], vec![]))
            .expect("ss");
        let dc = dc_event(200, addr(1), "proposal");
        poll.apply_event(&dc).expect("dc");
        let candidate_hash = sha256_of_signing_bytes(&dc);

        // Apply da from addr(2) twice.
        poll.apply_event(&da_event(300, addr(2), candidate_hash))
            .expect("da1");
        poll.apply_event(&da_event(400, addr(2), candidate_hash))
            .expect("da2 (idempotent)");

        let c = &poll.candidates[0];
        assert_eq!(c.approvals.len(), 2, "should still be 2 (idempotent)");
    }

    // ── Test 11: apply kd=sf transitions to Failed (terminal) ─────────────────

    #[test]
    fn apply_kd_sf_transitions_to_failed_terminal() {
        let mut poll = new_poll(0);
        poll.apply_event(&sf_event(100)).expect("sf");
        assert_eq!(poll.stage, Stage::Failed);
    }

    // ── Test 12: apply after Failed returns PollInFailedState ─────────────────

    #[test]
    fn apply_after_failed_returns_poll_in_failed_state() {
        let mut poll = new_poll(0);
        poll.apply_event(&sf_event(100)).expect("sf");

        let err = poll
            .apply_event(&md_event(200, addr(1)))
            .expect_err("should fail");
        assert!(
            matches!(err, ApplyError::PollInFailedState),
            "expected PollInFailedState, got {err:?}"
        );
    }

    // ── Test 13: apply kd=rs transitions to Finalized (terminal) ──────────────

    #[test]
    fn apply_kd_rs_transitions_to_finalized_terminal() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(100, vec![addr(1)], vec![]))
            .expect("ss");
        poll.apply_event(&rs_event(500)).expect("rs");
        assert_eq!(poll.stage, Stage::Finalized);
        assert!(poll.result.is_some());
    }

    // ── Test 14: apply after Finalized returns PollInFinalizedState ───────────

    #[test]
    fn apply_after_finalized_returns_poll_in_finalized_state() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(100, vec![addr(1)], vec![]))
            .expect("ss");
        poll.apply_event(&rs_event(500)).expect("rs");

        let err = poll
            .apply_event(&md_event(600, addr(1)))
            .expect_err("should fail");
        assert!(
            matches!(err, ApplyError::PollInFinalizedState),
            "expected PollInFinalizedState, got {err:?}"
        );
    }

    // ── Test 15: Tier 1 kinds return InvalidKindForTier3 ──────────────────────

    #[test]
    fn apply_kd_bl_or_other_tier1_kinds_returns_invalid_kind_for_tier3() {
        let mut poll = new_poll(0);

        for kind in &[
            PollEventKindCode::BallotCast,
            PollEventKindCode::PollOpen,
            PollEventKindCode::PollExtend,
            PollEventKindCode::Signal,
            PollEventKindCode::Delegate,
            PollEventKindCode::Undelegate,
        ] {
            let ev = make_event(*kind, 100, addr(1));
            let err = poll.apply_event(&ev).expect_err("should be invalid");
            assert!(
                matches!(err, ApplyError::InvalidKindForTier3(_)),
                "expected InvalidKindForTier3 for {kind:?}, got {err:?}"
            );
        }
    }

    // ── Test 16: apply kd=rb appends to ratification_ballots ─────────────────

    #[test]
    fn apply_kd_rb_appends_to_ratification_ballots() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(100, vec![addr(1)], vec![]))
            .expect("ss");
        poll.apply_event(&rb_event(200, addr(1), vec![3, 1, 5]))
            .expect("rb1");
        poll.apply_event(&rb_event(300, addr(2), vec![0, 5, 2]))
            .expect("rb2");
        assert_eq!(poll.ratification_ballots.len(), 2);
        assert_eq!(poll.ratification_ballots[0].scores, Some(vec![3, 1, 5]));
        assert_eq!(poll.ratification_ballots[1].scores, Some(vec![0, 5, 2]));
    }

    // ── Test 17: apply kd=ds is scaffold no-op (accepts event) ───────────────

    #[test]
    fn apply_kd_ds_is_scaffold_noop_accepts_event() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(100, vec![addr(1)], vec![]))
            .expect("ss");
        poll.apply_event(&ds_event(200, addr(1)))
            .expect("ds should be accepted as no-op scaffold");
        // No state change beyond last_hlc.
        assert!(poll.candidates.is_empty());
        assert!(poll.ratification_ballots.is_empty());
        assert_eq!(poll.last_hlc.as_ref().map(|h| h.wall_ms), Some(200));
    }

    // ── Test 18: decline_count_at filters by HLC ──────────────────────────────

    #[test]
    fn decline_count_at_filters_by_hlc() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(100, vec![addr(1), addr(2), addr(3)], vec![]))
            .expect("ss");
        poll.apply_event(&md_event(200, addr(1))).expect("md1");
        poll.apply_event(&md_event(400, addr(2))).expect("md2");
        poll.apply_event(&md_event(600, addr(3))).expect("md3");

        assert_eq!(poll.decline_count_at(&hlc(0)), 0);
        assert_eq!(poll.decline_count_at(&hlc(200)), 1);
        assert_eq!(poll.decline_count_at(&hlc(300)), 1);
        assert_eq!(poll.decline_count_at(&hlc(400)), 2);
        assert_eq!(poll.decline_count_at(&hlc(1000)), 3);
    }

    // ── Additional: apply kd=cr (PollCreate) returns InvalidKindForTier3 ──────

    #[test]
    fn apply_kd_cr_returns_invalid_kind() {
        let mut poll = new_poll(0);
        let ev = make_event(PollEventKindCode::PollCreate, 100, addr(1));
        let err = poll.apply_event(&ev).expect_err("should be invalid");
        assert!(matches!(err, ApplyError::InvalidKindForTier3(_)));
    }

    // ── Additional: current_stage_at with Failed state returns Failed ─────────

    #[test]
    fn current_stage_at_failed_returns_failed() {
        let mut poll = new_poll(0);
        poll.apply_event(&sf_event(100)).expect("sf");
        assert_eq!(poll.current_stage_at(&hlc(999_999)), Stage::Failed);
    }

    // ── Additional: current_stage_at with Finalized state returns Finalized ───

    #[test]
    fn current_stage_at_finalized_returns_finalized() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(100, vec![addr(1)], vec![]))
            .expect("ss");
        poll.apply_event(&rs_event(200)).expect("rs");
        assert_eq!(poll.current_stage_at(&hlc(999_999)), Stage::Finalized);
    }

    // ── Additional: current_mini_public returns empty before sortition ─────────

    #[test]
    fn current_mini_public_empty_before_sortition() {
        let poll = new_poll(0);
        assert!(poll.current_mini_public(&hlc(100)).is_empty());
    }

    // ── Task 2: new Tier3PollState has empty deliberation projection ──────────

    #[test]
    fn new_tier3_poll_state_has_empty_deliberation() {
        let meta = meta_at(0);
        let poll =
            Tier3PollState::new_from_create(meta, vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])]);
        assert!(poll.deliberation.statements.is_empty());
        assert!(poll.deliberation.votes.is_empty());
        assert!(poll.deliberation.statements_per_author.is_empty());
    }

    // ── ZEB-295 Task 6: secret tally projection + committee oracle ─────────

    #[test]
    fn tier3_poll_state_initializes_with_empty_secret_tally() {
        let meta = meta_at(0);
        let state = Tier3PollState::new_from_create(meta, vec![]);
        assert!(state.secret_tally.tally_shares.is_empty());
        assert!(state.secret_tally.decrypted_result.is_none());
    }

    #[test]
    fn null_committee_oracle_returns_none() {
        let oracle = NullCommitteeOracle;
        assert!(oracle.committee_at_epoch(7).is_none());
        assert!(oracle.latest_epoch().is_none());
    }

    // ── Additional: da for unknown candidate is silently ignored ──────────────

    #[test]
    fn apply_kd_da_for_unknown_candidate_is_ignored() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(100, vec![addr(1), addr(2)], vec![]))
            .expect("ss");
        let unknown_hash = [0xde; 32];
        // Should not error even if candidate is unknown.
        poll.apply_event(&da_event(200, addr(2), unknown_hash))
            .expect("da for unknown candidate");
        assert!(poll.candidates.is_empty());
    }

    // ── Drafting math tests ───────────────────────────────────────────────────

    // Helper: build a DraftCandidateState with a given hash and approval set.
    fn make_candidate(hash_byte: u8, approvers: &[u8]) -> DraftCandidateState {
        let mut approvals = std::collections::HashSet::new();
        for &b in approvers {
            approvals.insert(addr(b));
        }
        DraftCandidateState {
            event_hash: [hash_byte; 32],
            text: format!("proposal {hash_byte}"),
            proposer: Some(addr(hash_byte)),
            approvals,
        }
    }

    // 1. synthesize_status_quo: same poll_id → same event_hash
    #[test]
    fn synthesize_status_quo_deterministic() {
        let pid = poll_id();
        let sq1 = synthesize_status_quo(&pid);
        let sq2 = synthesize_status_quo(&pid);
        assert_eq!(sq1.event_hash, sq2.event_hash);
        assert_eq!(sq1.text, "<status quo>");
        assert!(sq1.proposer.is_none());
        assert!(sq1.approvals.is_empty());
    }

    // 2. synthesize_status_quo: different polls → different hashes
    #[test]
    fn synthesize_status_quo_different_polls_different_hashes() {
        let pid1 = PollId([0x01; 32]);
        let pid2 = PollId([0x02; 32]);
        let sq1 = synthesize_status_quo(&pid1);
        let sq2 = synthesize_status_quo(&pid2);
        assert_ne!(sq1.event_hash, sq2.event_hash);
    }

    // 3. drafting_advancers: all below threshold → only status_quo returned
    #[test]
    fn drafting_advancers_below_threshold_returns_only_status_quo() {
        let sq = synthesize_status_quo(&poll_id());
        let sq_hash = sq.event_hash;
        // mini_public_size=4, threshold=2; candidates have 1 approval each (below 2)
        let candidates = vec![make_candidate(0x01, &[1]), make_candidate(0x02, &[2]), sq];
        let result =
            drafting_advancers(&candidates, 4, sq_hash).expect("status_quo is in candidates");
        assert_eq!(result.len(), 1, "only status_quo should advance");
        assert_eq!(result[0].event_hash, sq_hash);
    }

    // 4. drafting_advancers: candidates at/above threshold advance + status_quo
    #[test]
    fn drafting_advancers_above_threshold_returns_top_n_plus_status_quo() {
        let sq = synthesize_status_quo(&poll_id());
        let sq_hash = sq.event_hash;
        // mini_public_size=4, threshold=2; candidate 0x01 has 3 approvals (above), 0x02 has 1 (below)
        let candidates = vec![
            make_candidate(0x01, &[1, 2, 3]),
            make_candidate(0x02, &[2]),
            sq,
        ];
        let result =
            drafting_advancers(&candidates, 4, sq_hash).expect("status_quo is in candidates");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].event_hash, [0x01; 32]);
        assert_eq!(result[1].event_hash, sq_hash, "status_quo must be last");
    }

    // 5. drafting_advancers: 10 candidates all above threshold → capped at 5 total
    #[test]
    fn drafting_advancers_caps_at_max_ratification_candidates_5() {
        let sq = synthesize_status_quo(&poll_id());
        let sq_hash = sq.event_hash;
        // 10 regular candidates all with 3 approvals; threshold=2 (mini_public=4)
        let mut candidates: Vec<DraftCandidateState> =
            (1u8..=10).map(|b| make_candidate(b, &[1, 2, 3])).collect();
        candidates.push(sq);

        let result =
            drafting_advancers(&candidates, 4, sq_hash).expect("status_quo is in candidates");
        assert_eq!(
            result.len(),
            MAX_RATIFICATION_CANDIDATES,
            "output must be capped at MAX_RATIFICATION_CANDIDATES=5"
        );
        // Last entry must be status_quo
        assert_eq!(
            result.last().unwrap().event_hash,
            sq_hash,
            "status_quo must be last"
        );
    }

    // 6. drafting_advancers: status_quo always last
    #[test]
    fn drafting_advancers_status_quo_always_last() {
        let sq = synthesize_status_quo(&poll_id());
        let sq_hash = sq.event_hash;
        let candidates = vec![
            make_candidate(0x10, &[1, 2, 3, 4]),
            make_candidate(0x20, &[1, 2, 3]),
            sq,
        ];
        let result =
            drafting_advancers(&candidates, 4, sq_hash).expect("status_quo is in candidates");
        assert!(result.len() >= 2);
        assert_eq!(
            result.last().unwrap().event_hash,
            sq_hash,
            "status_quo must always be last"
        );
    }

    // 7. drafting_advancers: sort by approval DESC, ties by hash ASC
    #[test]
    fn drafting_advancers_sort_by_approval_desc_then_hash_asc() {
        let sq = synthesize_status_quo(&poll_id());
        let sq_hash = sq.event_hash;
        // 0x30: 3 approvals, 0x10: 3 approvals, 0x20: 2 approvals
        // Tie between 0x10 and 0x30 at 3 approvals → hash ASC: 0x10 < 0x30
        let candidates = vec![
            make_candidate(0x30, &[1, 2, 3]),
            make_candidate(0x10, &[1, 2, 3]),
            make_candidate(0x20, &[1, 2]),
            sq,
        ];
        // mini_public=4, threshold=2; all three pass
        let result =
            drafting_advancers(&candidates, 4, sq_hash).expect("status_quo is in candidates");
        // Expected order: 0x10 (3 approvals, lower hash), 0x30 (3 approvals, higher hash),
        // 0x20 (2 approvals), status_quo (last)
        assert_eq!(result[0].event_hash, [0x10; 32]);
        assert_eq!(result[1].event_hash, [0x30; 32]);
        assert_eq!(result[2].event_hash, [0x20; 32]);
        assert_eq!(result[3].event_hash, sq_hash);
    }

    // 8. drafting_advancers: ceil threshold for odd mini_public (N=5, threshold=3)
    #[test]
    fn drafting_advancers_ceil_threshold_for_odd_mini_public() {
        let sq = synthesize_status_quo(&poll_id());
        let sq_hash = sq.event_hash;
        // N=5, threshold = ceil(5/2) = 3
        // candidate with 2 approvals: below threshold
        // candidate with 3 approvals: at threshold (passes)
        let candidates = vec![
            make_candidate(0x01, &[1, 2]),    // 2 approvals — below threshold=3
            make_candidate(0x02, &[1, 2, 3]), // 3 approvals — at threshold=3
            sq,
        ];
        let result =
            drafting_advancers(&candidates, 5, sq_hash).expect("status_quo is in candidates");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].event_hash, [0x02; 32], "0x02 should advance");
        assert_eq!(result[1].event_hash, sq_hash, "status_quo last");
    }

    // 9. drafting_advancers: ceil threshold for even mini_public (N=4, threshold=2)
    #[test]
    fn drafting_advancers_ceil_threshold_for_even_mini_public() {
        let sq = synthesize_status_quo(&poll_id());
        let sq_hash = sq.event_hash;
        // N=4, threshold = ceil(4/2) = 2
        // candidate with 1 approval: below threshold
        // candidate with 2 approvals: at threshold (passes)
        let candidates = vec![
            make_candidate(0x01, &[1]),    // 1 approval — below threshold=2
            make_candidate(0x02, &[1, 2]), // 2 approvals — at threshold=2
            sq,
        ];
        let result =
            drafting_advancers(&candidates, 4, sq_hash).expect("status_quo is in candidates");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].event_hash, [0x02; 32], "0x02 should advance");
        assert_eq!(result[1].event_hash, sq_hash, "status_quo last");
    }

    // 10. ratification_candidates_ordering: deterministic (same input → same output)
    #[test]
    fn ratification_candidates_ordering_deterministic() {
        let sq = synthesize_status_quo(&poll_id());
        let sq_hash = sq.event_hash;
        let candidates = vec![
            make_candidate(0x10, &[1, 2, 3]),
            make_candidate(0x20, &[1, 2]),
            sq,
        ];
        let advancers =
            drafting_advancers(&candidates, 4, sq_hash).expect("status_quo is in candidates");
        let order1 = ratification_candidates_ordering(&advancers, sq_hash);
        let order2 = ratification_candidates_ordering(&advancers, sq_hash);
        assert_eq!(order1, order2, "ordering must be deterministic");
    }

    // 11. ratification_candidates_ordering: status_quo last unless tied at zero
    #[test]
    fn ratification_candidates_ordering_status_quo_always_last_unless_tied() {
        let sq = synthesize_status_quo(&poll_id());
        let sq_hash = sq.event_hash;
        // Three candidates: 0x10 with 3 approvals, 0x20 with 2 approvals, status_quo (0 approvals)
        // Expected ordering: 0x10 (3), 0x20 (2), status_quo (0)
        let candidates = vec![
            make_candidate(0x20, &[1, 2]),
            make_candidate(0x10, &[1, 2, 3]),
            sq,
        ];
        let advancers =
            drafting_advancers(&candidates, 4, sq_hash).expect("status_quo is in candidates");
        let ordered = ratification_candidates_ordering(&advancers, sq_hash);
        assert_eq!(ordered[0].event_hash, [0x10; 32], "highest approvals first");
        assert_eq!(ordered[1].event_hash, [0x20; 32]);
        assert_eq!(
            ordered.last().unwrap().event_hash,
            sq_hash,
            "status_quo last"
        );
    }

    // 12. drafting_advancers: excludes below-threshold candidates
    #[test]
    fn drafting_advancers_excludes_below_threshold_candidates() {
        let sq = synthesize_status_quo(&poll_id());
        let sq_hash = sq.event_hash;
        // N=6, threshold = ceil(6/2) = 3
        // 0x01: 2 approvals (excluded), 0x02: 3 approvals (included), 0x03: 4 approvals (included)
        let candidates = vec![
            make_candidate(0x01, &[1, 2]),       // 2 — below
            make_candidate(0x02, &[1, 2, 3]),    // 3 — at threshold
            make_candidate(0x03, &[1, 2, 3, 4]), // 4 — above
            sq,
        ];
        let result =
            drafting_advancers(&candidates, 6, sq_hash).expect("status_quo is in candidates");
        let hashes: Vec<u8> = result.iter().map(|c| c.event_hash[0]).collect();
        assert!(
            !hashes.contains(&0x01),
            "below-threshold candidate must be excluded"
        );
        assert!(
            hashes.contains(&0x02),
            "at-threshold candidate must advance"
        );
        assert!(
            hashes.contains(&0x03),
            "above-threshold candidate must advance"
        );
        assert_eq!(
            result.last().unwrap().event_hash,
            sq_hash,
            "status_quo last"
        );
    }

    // ── validate_tier3_poll_config tests ──────────────────────────────────────

    fn valid_config() -> Tier3PollConfigPayload {
        use crate::community_voting_core::Eligibility;
        Tier3PollConfigPayload {
            proposal_text: "Amend charter §3 to allow remote governance".into(),
            sortition_size: 100,
            deliberation_window_seconds: 3600,
            drafting_window_seconds: 3600,
            ratification_window_seconds: 3600,
            privacy_mode: "pu".into(),
            incentive_mode: "a".into(),
            eligibility: Eligibility {
                min_power: 1,
                min_vouching_depth: None,
                sortition_size: None,
            },
            retry_of: None,
        }
    }

    // Test 1: happy path — all fields valid → Ok
    #[test]
    fn validate_config_happy_path() {
        assert_eq!(validate_tier3_poll_config(&valid_config()), Ok(()));
    }

    // Test 2: empty proposal_text rejected
    #[test]
    fn validate_config_empty_proposal_text_rejected() {
        let mut c = valid_config();
        c.proposal_text = String::new();
        assert_eq!(
            validate_tier3_poll_config(&c),
            Err(ValidateError::EmptyProposalText)
        );
    }

    // Test 3: sortition_size below 20 rejected
    #[test]
    fn validate_config_sortition_size_below_20_rejected() {
        let mut c = valid_config();
        c.sortition_size = 19;
        assert_eq!(
            validate_tier3_poll_config(&c),
            Err(ValidateError::SortitionSizeOutOfRange(19))
        );
    }

    // Test 4: sortition_size above 300 rejected
    #[test]
    fn validate_config_sortition_size_above_300_rejected() {
        let mut c = valid_config();
        c.sortition_size = 301;
        assert_eq!(
            validate_tier3_poll_config(&c),
            Err(ValidateError::SortitionSizeOutOfRange(301))
        );
    }

    // Test 5: sortition_size = 20 accepted (lower boundary)
    #[test]
    fn validate_config_sortition_size_20_accepted() {
        let mut c = valid_config();
        c.sortition_size = 20;
        assert_eq!(validate_tier3_poll_config(&c), Ok(()));
    }

    // Test 6: sortition_size = 300 accepted (upper boundary)
    #[test]
    fn validate_config_sortition_size_300_accepted() {
        let mut c = valid_config();
        c.sortition_size = 300;
        assert_eq!(validate_tier3_poll_config(&c), Ok(()));
    }

    // Test 7: window below 60s rejected — tests all three windows
    #[test]
    fn validate_config_window_below_60s_rejected() {
        let mut c = valid_config();
        c.deliberation_window_seconds = 59;
        assert_eq!(
            validate_tier3_poll_config(&c),
            Err(ValidateError::DeliberationWindowTooSmall(59))
        );

        let mut c = valid_config();
        c.drafting_window_seconds = 0;
        assert_eq!(
            validate_tier3_poll_config(&c),
            Err(ValidateError::DraftingWindowTooSmall(0))
        );

        let mut c = valid_config();
        c.ratification_window_seconds = 1;
        assert_eq!(
            validate_tier3_poll_config(&c),
            Err(ValidateError::RatificationWindowTooSmall(1))
        );
    }

    // Test 8: privacy_mode "se" accepted (ZEB-295 Phase 6)
    #[test]
    fn validate_config_privacy_mode_se_accepted() {
        let mut c = valid_config();
        c.privacy_mode = "se".into();
        assert_eq!(validate_tier3_poll_config(&c), Ok(()));
    }

    // Test 9: privacy_mode "rf" rejected (Phase 7 forward-compat)
    #[test]
    fn validate_config_privacy_mode_rf_rejected_with_unknown_privacy_mode() {
        let mut c = valid_config();
        c.privacy_mode = "rf".into();
        assert_eq!(
            validate_tier3_poll_config(&c),
            Err(ValidateError::UnknownPrivacyMode("rf".into()))
        );
    }

    // Test 10: privacy_mode "pu" accepted
    #[test]
    fn validate_config_privacy_mode_pu_accepted() {
        let mut c = valid_config();
        c.privacy_mode = "pu".into();
        assert_eq!(validate_tier3_poll_config(&c), Ok(()));
    }

    // Test 11: unknown incentive_mode rejected
    #[test]
    fn validate_config_incentive_mode_unknown_rejected() {
        let mut c = valid_config();
        c.incentive_mode = "z".into();
        assert_eq!(
            validate_tier3_poll_config(&c),
            Err(ValidateError::UnknownIncentiveMode("z".into()))
        );
    }

    // Test 12: all four valid incentive modes accepted (a/b/c/d)
    #[test]
    fn validate_config_all_four_incentive_modes_accepted() {
        for mode in ["a", "b", "c", "d"] {
            let mut c = valid_config();
            c.incentive_mode = mode.into();
            assert_eq!(
                validate_tier3_poll_config(&c),
                Ok(()),
                "incentive_mode {mode:?} should be accepted"
            );
        }
    }

    // ── validate_ratification_ballot tests ────────────────────────────────────

    fn ballot(scores: Vec<u8>) -> RatificationBallotPayload {
        RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(scores),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        }
    }

    // Test 13: ballot length matches expected → Ok
    #[test]
    fn validate_ballot_length_matches_accepted() {
        let b = ballot(vec![3, 0, 5, 1, 2]);
        assert_eq!(validate_ratification_ballot(&b, 5), Ok(()));
    }

    // Test 14: ballot length mismatch rejected
    #[test]
    fn validate_ballot_length_mismatch_rejected() {
        let b = ballot(vec![1, 2]);
        assert_eq!(
            validate_ratification_ballot(&b, 3),
            Err(ValidateError::BallotLengthMismatch {
                scores: 2,
                expected: 3,
            })
        );
    }

    // Test 15: score above 5 rejected
    #[test]
    fn validate_ballot_score_above_5_rejected() {
        let b = ballot(vec![3, 6, 1]);
        assert_eq!(
            validate_ratification_ballot(&b, 3),
            Err(ValidateError::BallotScoreOutOfRange(6))
        );
    }

    // Test 16: score = 5 accepted (upper boundary)
    #[test]
    fn validate_ballot_score_5_accepted() {
        let b = ballot(vec![5, 5, 5]);
        assert_eq!(validate_ratification_ballot(&b, 3), Ok(()));
    }

    // Test 17: score = 0 accepted (lower boundary)
    #[test]
    fn validate_ballot_score_0_accepted() {
        let b = ballot(vec![0, 0, 0]);
        assert_eq!(validate_ratification_ballot(&b, 3), Ok(()));
    }

    // ── MockBeaconOracle + verify_ss tests ────────────────────────────────────

    use crate::community_voting_sortition::derive_beacon_seed;
    use crate::owner_state_types::SpaceId;

    struct MockBeaconOracle {
        // Maps (community_id bytes, seed) → vrf_output
        outputs: Vec<([u8; 16], [u8; 32], [u8; 32])>,
    }

    impl MockBeaconOracle {
        fn new() -> Self {
            MockBeaconOracle {
                outputs: Vec::new(),
            }
        }

        fn with(mut self, community_id: [u8; 16], seed: [u8; 32], vrf: [u8; 32]) -> Self {
            self.outputs.push((community_id, seed, vrf));
            self
        }
    }

    #[async_trait::async_trait]
    impl BeaconOracle for MockBeaconOracle {
        async fn vrf_output_for(
            &self,
            community_id: &SpaceId,
            seed: &[u8; 32],
            _epoch: u64, // tests match on (community_id, seed) only; epoch is caller-supplied
        ) -> Option<[u8; 32]> {
            self.outputs
                .iter()
                .find(|(cid, s, _)| cid == &community_id.0 && s == seed)
                .map(|(_, _, vrf)| *vrf)
        }
    }

    fn community_id() -> SpaceId {
        SpaceId([0x01; 16])
    }

    /// Build a poll state with `sortition_size` members in the electorate
    /// so fisher_yates_select can draw primary_size + backup_size from it.
    fn poll_state_with_electorate(create_wall_ms: u64, electorate_size: u8) -> Tier3PollState {
        let mut meta = meta_at(create_wall_ms);
        meta.config.sortition_size = 2; // primary=2, backup=2 → need ≥4 members
        Tier3PollState::new_from_create(meta, electorate(electorate_size))
    }

    /// Compute the seed a poll with the default meta_at hash + epoch=1 would produce.
    fn default_seed() -> [u8; 32] {
        derive_beacon_seed(&[0xaa; 32], 1)
    }

    /// Use fisher_yates_select to compute the correct SortitionSelectionPayload for a poll.
    fn correct_ss_payload(poll: &Tier3PollState, vrf: &[u8; 32]) -> SortitionSelectionPayload {
        use crate::community_voting_sortition::fisher_yates_select;
        let size = poll.meta.config.sortition_size as usize;
        let sr = fisher_yates_select(vrf, &poll.eligible_electorate_snapshot, size, size);
        SortitionSelectionPayload {
            poll_id: poll.meta.poll_id,
            primary: sr.primary,
            backup: sr.backup,
        }
    }

    // 1. verify_ss_happy_path_recompute_matches
    #[tokio::test]
    async fn verify_ss_happy_path_recompute_matches() {
        let poll = poll_state_with_electorate(0, 10);
        let vrf = [0x55u8; 32];
        let seed = default_seed();
        let oracle = MockBeaconOracle::new().with([0x01; 16], seed, vrf);
        let payload = correct_ss_payload(&poll, &vrf);
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionSelection,
            100,
            addr(0xfe),
            encode(&payload),
        );
        let result = verify_ss(&ev, &poll, &oracle, &community_id()).await;
        assert_eq!(result, Ok(()));
    }

    // 2. verify_ss_mismatched_primary_rejected_SortitionMismatch
    #[tokio::test]
    async fn verify_ss_mismatched_primary_rejected_sortition_mismatch() {
        let poll = poll_state_with_electorate(0, 10);
        let vrf = [0x55u8; 32];
        let seed = default_seed();
        let oracle = MockBeaconOracle::new().with([0x01; 16], seed, vrf);
        let mut payload = correct_ss_payload(&poll, &vrf);
        payload.primary[0] = addr(0xde); // corrupt primary
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionSelection,
            100,
            addr(0xfe),
            encode(&payload),
        );
        let result = verify_ss(&ev, &poll, &oracle, &community_id()).await;
        assert_eq!(result, Err(VerifyError::SortitionMismatch));
    }

    // 3. verify_ss_mismatched_backup_rejected_SortitionMismatch
    #[tokio::test]
    async fn verify_ss_mismatched_backup_rejected_sortition_mismatch() {
        let poll = poll_state_with_electorate(0, 10);
        let vrf = [0x55u8; 32];
        let seed = default_seed();
        let oracle = MockBeaconOracle::new().with([0x01; 16], seed, vrf);
        let mut payload = correct_ss_payload(&poll, &vrf);
        payload.backup[0] = addr(0xde); // corrupt backup
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionSelection,
            100,
            addr(0xfe),
            encode(&payload),
        );
        let result = verify_ss(&ev, &poll, &oracle, &community_id()).await;
        assert_eq!(result, Err(VerifyError::SortitionMismatch));
    }

    // 4. verify_ss_missing_beacon_rejected_BeaconNotYetAvailable
    #[tokio::test]
    async fn verify_ss_missing_beacon_rejected_beacon_not_yet_available() {
        let poll = poll_state_with_electorate(0, 10);
        let oracle = MockBeaconOracle::new(); // empty — no outputs registered
        let payload = SortitionSelectionPayload {
            poll_id: poll_id(),
            primary: vec![],
            backup: vec![],
        };
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionSelection,
            100,
            addr(0xfe),
            encode(&payload),
        );
        let result = verify_ss(&ev, &poll, &oracle, &community_id()).await;
        assert_eq!(result, Err(VerifyError::BeaconNotYetAvailable));
    }

    // 5. verify_ss_decode_failure_rejected_PayloadDecode
    #[tokio::test]
    async fn verify_ss_decode_failure_rejected_payload_decode() {
        let poll = poll_state_with_electorate(0, 10);
        let seed = default_seed();
        let oracle = MockBeaconOracle::new().with([0x01; 16], seed, [0x55; 32]);
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionSelection,
            100,
            addr(0xfe),
            vec![0xff, 0xfe], // garbage CBOR
        );
        let result = verify_ss(&ev, &poll, &oracle, &community_id()).await;
        assert!(matches!(result, Err(VerifyError::PayloadDecode(_))));
    }

    // ── verify_sd tests ───────────────────────────────────────────────────────

    /// Build a poll state already past sortition, in Deliberation, with a known primary + backup.
    fn poll_with_sortition(
        create_wall_ms: u64,
        primary: Vec<OwnerAddr>,
        backup: Vec<OwnerAddr>,
    ) -> Tier3PollState {
        let electorate_size = (primary.len() + backup.len() + 5) as u8; // padding
        let mut meta = meta_at(create_wall_ms);
        meta.config.sortition_size = primary.len() as u16;
        let mut poll = Tier3PollState::new_from_create(meta, electorate(electorate_size));
        let ev = ss_event(create_wall_ms + 10, primary, backup);
        poll.apply_event(&ev).expect("apply ss in fixture");
        poll
    }

    // 6. verify_sd_actor_in_primary_accepted
    #[test]
    fn verify_sd_actor_in_primary_accepted() {
        let poll = poll_with_sortition(0, vec![addr(1), addr(2), addr(3)], vec![addr(10)]);
        let ev = make_event(PollEventKindCode::DeliberationStatement, 50, addr(1));
        assert_eq!(verify_sd(&ev, &poll), Ok(()));
    }

    // 7. verify_sd_actor_in_promoted_backup_accepted (primary member declined → backup promoted)
    #[test]
    fn verify_sd_actor_in_promoted_backup_accepted() {
        let mut poll =
            poll_with_sortition(0, vec![addr(1), addr(2), addr(3)], vec![addr(10), addr(11)]);
        // addr(1) declines → addr(10) is promoted as backup[0]
        poll.apply_event(&md_event(60, addr(1))).expect("md");
        let ev = make_event(PollEventKindCode::DeliberationStatement, 80, addr(10));
        assert_eq!(verify_sd(&ev, &poll), Ok(()));
    }

    // 8. verify_sd_actor_not_in_set_rejected_NotInMiniPublic
    #[test]
    fn verify_sd_actor_not_in_set_rejected_not_in_mini_public() {
        let poll = poll_with_sortition(0, vec![addr(1), addr(2)], vec![addr(10)]);
        let ev = make_event(PollEventKindCode::DeliberationStatement, 50, addr(99));
        assert_eq!(
            verify_sd(&ev, &poll),
            Err(VerifyError::NotInMiniPublic(addr(99)))
        );
    }

    // 9. verify_sd_actor_declined_no_longer_in_set_rejected
    #[test]
    fn verify_sd_actor_declined_no_longer_in_set_rejected() {
        let mut poll = poll_with_sortition(0, vec![addr(1), addr(2), addr(3)], vec![addr(10)]);
        // addr(1) declines at wall_ms=60
        poll.apply_event(&md_event(60, addr(1))).expect("md");
        // Now at wall_ms=80, addr(1) tries to act — should be rejected (no longer in mini-public)
        let ev = make_event(PollEventKindCode::DeliberationStatement, 80, addr(1));
        assert_eq!(
            verify_sd(&ev, &poll),
            Err(VerifyError::NotInMiniPublic(addr(1)))
        );
    }

    // ── verify_sf tests ───────────────────────────────────────────────────────

    /// Build a poll with `n_declines` pre-recorded declines. sortition_size=n_backup_capacity.
    fn poll_with_declines(n_declines: usize, backup_capacity: usize) -> Tier3PollState {
        let total = (n_declines + backup_capacity + 5) as u8;
        let primary: Vec<OwnerAddr> = (0..n_declines as u8).map(addr).collect();
        let backup: Vec<OwnerAddr> = (100..100 + backup_capacity as u8).map(addr).collect();
        let mut meta = meta_at(0);
        meta.config.sortition_size = backup_capacity as u16;
        let mut poll = Tier3PollState::new_from_create(meta, electorate(total));
        let ev = ss_event(10, primary.clone(), backup);
        poll.apply_event(&ev).expect("ss");
        for (i, actor) in primary.into_iter().enumerate() {
            poll.apply_event(&md_event(20 + i as u64 * 10, actor))
                .expect("md");
        }
        poll
    }

    // 10. verify_sf_proposer_with_exhausted_pool_accepted
    #[test]
    fn verify_sf_proposer_with_exhausted_pool_accepted() {
        // sortition_size=2: primary=2, backup=2 → total capacity=4.
        // All 4 (2 primary + 2 backup) decline → pool fully exhausted → accepted.
        let poll = poll_with_split_declines(2, 2, 2);
        let payload = SortitionFailedPayload { poll_id: poll_id() };
        // wall_ms=10000 ensures all backup declines (HLCs ~140-150) are included in
        // decline_count_at's filter (decline_count_at filters by ≤ event.hlc).
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionFailed,
            10000,
            addr(0xff), // proposer = addr(0xff) from meta_at
            encode(&payload),
        );
        assert_eq!(verify_sf(&ev, &poll), Ok(()));
    }

    // 11. verify_sf_non_proposer_rejected_SfActorNotProposer
    #[test]
    fn verify_sf_non_proposer_rejected_sf_actor_not_proposer() {
        let poll = poll_with_declines(3, 2);
        let payload = SortitionFailedPayload { poll_id: poll_id() };
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionFailed,
            100,
            addr(0x01), // NOT the proposer (which is addr(0xff))
            encode(&payload),
        );
        assert_eq!(
            verify_sf(&ev, &poll),
            Err(VerifyError::SfActorNotProposer(addr(0x01)))
        );
    }

    // 12. verify_sf_pool_not_exhausted_rejected_BackupPoolNotExhausted
    #[test]
    fn verify_sf_pool_not_exhausted_rejected_backup_pool_not_exhausted() {
        // sortition_size=3: primary=3, backup=3 → total capacity=6.
        // Only 1 decline → pool not exhausted (need 6).
        let poll = poll_with_split_declines(3, 1, 0);
        let payload = SortitionFailedPayload { poll_id: poll_id() };
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionFailed,
            100,
            addr(0xff), // proposer
            encode(&payload),
        );
        assert_eq!(
            verify_sf(&ev, &poll),
            Err(VerifyError::BackupPoolNotExhausted {
                declined: 1,
                capacity: 6
            })
        );
    }

    // 13a. Cluster J regression: verify_sf with sortition_size=20
    //      - 20 declines (primary only) → still rejected (10 backup remain)
    //      - 30 declines → still rejected (10 non-declined remain)
    //      - 40 declines (full pool) → accepted
    //
    // Uses a dedicated helper that can inject backup declines as well.
    /// Build a poll with `n_primary_declines` primary declines and `n_backup_declines`
    /// backup declines. Both pools have `sortition_size` members.
    fn poll_with_split_declines(
        sortition_size: usize,
        n_primary_declines: usize,
        n_backup_declines: usize,
    ) -> Tier3PollState {
        assert!(n_primary_declines <= sortition_size);
        assert!(n_backup_declines <= sortition_size);
        let total = (sortition_size * 2 + 5) as u8;
        let primary: Vec<OwnerAddr> = (0..sortition_size as u8).map(addr).collect();
        let backup: Vec<OwnerAddr> = (100..100 + sortition_size as u8).map(addr).collect();
        let mut meta = meta_at(0);
        meta.config.sortition_size = sortition_size as u16;
        let mut poll = Tier3PollState::new_from_create(meta, electorate(total));
        let ev = ss_event(10, primary.clone(), backup.clone());
        poll.apply_event(&ev).expect("ss");
        for (i, actor) in primary.into_iter().take(n_primary_declines).enumerate() {
            poll.apply_event(&md_event(20 + i as u64 * 10, actor))
                .expect("primary md");
        }
        let base = 20 + (n_primary_declines as u64) * 10 + 100;
        for (i, actor) in backup.into_iter().take(n_backup_declines).enumerate() {
            poll.apply_event(&md_event(base + i as u64 * 10, actor))
                .expect("backup md");
        }
        poll
    }

    #[test]
    fn verify_sf_20_of_40_declines_rejected_j_regression() {
        // sortition_size=20: primary=20, backup=20 → total capacity=40.
        // 20 declines → not exhausted (20 < 40).
        let poll = poll_with_split_declines(20, 20, 0);
        let payload = SortitionFailedPayload { poll_id: poll_id() };
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionFailed,
            5000,
            addr(0xff),
            encode(&payload),
        );
        assert_eq!(
            verify_sf(&ev, &poll),
            Err(VerifyError::BackupPoolNotExhausted {
                declined: 20,
                capacity: 40
            }),
            "20 declines out of 40 must be rejected"
        );
    }

    #[test]
    fn verify_sf_30_of_40_declines_rejected_j_regression() {
        // sortition_size=20: 30 declines (20 primary + 10 backup) → still 10 available.
        let poll = poll_with_split_declines(20, 20, 10);
        let payload = SortitionFailedPayload { poll_id: poll_id() };
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionFailed,
            5000,
            addr(0xff),
            encode(&payload),
        );
        assert_eq!(
            verify_sf(&ev, &poll),
            Err(VerifyError::BackupPoolNotExhausted {
                declined: 30,
                capacity: 40
            }),
            "30 declines out of 40 must be rejected"
        );
    }

    #[test]
    fn verify_sf_40_of_40_declines_accepted_j_regression() {
        // sortition_size=20: all 40 declined → pool fully exhausted → accepted.
        let poll = poll_with_split_declines(20, 20, 20);
        let payload = SortitionFailedPayload { poll_id: poll_id() };
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionFailed,
            5000,
            addr(0xff),
            encode(&payload),
        );
        assert_eq!(
            verify_sf(&ev, &poll),
            Ok(()),
            "40 declines out of 40 must be accepted"
        );
    }

    // ── verify_sr tests ───────────────────────────────────────────────────────

    /// Build a poll state in Ratification with some ballots and a kd=cl close applied.
    ///
    /// Uses sortition_size=3 (matching primary=[addr(1),2,3]) so threshold=ceil(3/2)=2
    /// and a candidate with 2 approvals passes. Simulates materialize() synthesizing
    /// status_quo into candidates at drafting open.
    fn poll_at_closed_with_ballots(ballots: &[(u64, OwnerAddr, Vec<u8>)]) -> Tier3PollState {
        let mut meta = meta_at(0);
        meta.config.sortition_size = 3; // threshold = ceil(3/2)=2; dw=10s, fw=10s
        let mut poll = Tier3PollState::new_from_create(meta, electorate(20));
        // Apply sortition
        poll.apply_event(&ss_event(10, vec![addr(1), addr(2), addr(3)], vec![]))
            .expect("ss");
        // Add a candidate with enough approvals (threshold = ceil(3/2) = 2)
        let dc = dc_event(200, addr(1), "proposal A");
        poll.apply_event(&dc).expect("dc");
        let candidate_hash = sha256_of_signing_bytes(&dc);
        poll.apply_event(&da_event(300, addr(2), candidate_hash))
            .expect("da");
        // Simulate materialize() synthesizing status_quo at drafting open.
        let sq = synthesize_status_quo(&poll.meta.poll_id);
        poll.candidates.push(sq);
        // Apply ballots (at ratification time, wall_ms ≥ 20000)
        for (wall_ms, actor, scores) in ballots {
            poll.apply_event(&rb_event(*wall_ms, *actor, scores.clone()))
                .expect("rb");
        }
        // Apply kd=cl PollClose
        let cl_ev = make_event(PollEventKindCode::PollClose, 40000, addr(0xff));
        poll.apply_event(&cl_ev).expect("cl");
        poll
    }

    // 13. verify_sr_happy_path_matching_tally_accepted
    #[test]
    fn verify_sr_happy_path_matching_tally_accepted() {
        let poll = poll_at_closed_with_ballots(&[(25000, addr(5), vec![5, 3])]);
        // Recompute expected result manually to build the correct payload.
        let sq = synthesize_status_quo(&poll.meta.poll_id);
        let sq_hash = sq.event_hash;
        let primary_size = poll.meta.config.sortition_size as usize;
        let advancers = drafting_advancers(&poll.candidates, primary_size, sq_hash)
            .expect("status_quo is in candidates (poll_at_closed_with_ballots synthesizes it)");
        let ordered = ratification_candidates_ordering(&advancers, sq_hash);
        let expected_result = tally_star(&ordered, &poll.ratification_ballots);
        let payload = Tier3PollResultPayload {
            poll_id: poll_id(),
            result: expected_result,
        };
        let ev = make_event_with_payload(
            PollEventKindCode::PollResult,
            41000,
            addr(0xfe),
            encode(&payload),
        );
        assert_eq!(verify_sr(&ev, &poll), Ok(()));
    }

    // 14. verify_sr_no_kd_cl_applied_rejected_NotInClosedStage
    #[test]
    fn verify_sr_no_kd_cl_applied_rejected_not_in_closed_stage() {
        // A poll without kd=cl applied
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(10, vec![addr(1)], vec![]))
            .expect("ss");
        let payload = Tier3PollResultPayload {
            poll_id: poll_id(),
            result: star_result(),
        };
        let ev = make_event_with_payload(
            PollEventKindCode::PollResult,
            500,
            addr(0xfe),
            encode(&payload),
        );
        assert_eq!(verify_sr(&ev, &poll), Err(VerifyError::NotInClosedStage));
    }

    // 15. verify_sr_tally_mismatch_rejected_TallyMismatch
    #[test]
    fn verify_sr_tally_mismatch_rejected_tally_mismatch() {
        let poll = poll_at_closed_with_ballots(&[(25000, addr(5), vec![5, 3])]);
        // Construct a deliberately wrong result
        let wrong_result = StarResult {
            winner: crate::community_voting_star::CandidateRef {
                event_hash: [0x00; 32],
                approval_count: 0,
            },
            finalists: vec![],
            total_scores: vec![0, 0],
            runoff_votes: vec![0],
        };
        let payload = Tier3PollResultPayload {
            poll_id: poll_id(),
            result: wrong_result,
        };
        let ev = make_event_with_payload(
            PollEventKindCode::PollResult,
            41000,
            addr(0xfe),
            encode(&payload),
        );
        assert_eq!(verify_sr(&ev, &poll), Err(VerifyError::TallyMismatch));
    }

    // 15b. ZEB-858: se-mode poll whose committee tally shares are below
    // threshold — `recover_secret_tally` returns `None`, so this node cannot
    // yet confirm the claimed result. verify_sr must surface the DISTINCT
    // `TallySharesNotReady` (retryable), NOT `TallyMismatch` (forgery).
    #[test]
    fn verify_sr_shares_not_ready_returns_distinct_error() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        // One real ballot + one real tally share. Committee threshold is 2,
        // so a single share leaves the secret tally unrecoverable (None).
        let ballot = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let rb_ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            committee.member_addrs[0],
            &ballot,
        );
        state.apply_event(&rb_ev).expect("rb ok");
        let share = ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        let ts_ev = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS,
            committee.member_addrs[0],
            &share,
        );
        state.apply_event(&ts_ev).expect("ts apply");
        // Satisfy R1: a kd=cl on its own (proposer) lane records close_event_hash.
        let cl_ev = make_event(
            PollEventKindCode::PollClose,
            RATIFICATION_END_MS + 1000,
            addr(0xff),
        );
        state.apply_event(&cl_ev).expect("cl");
        // Any claimed result — the recompute is unavailable, so it is unconfirmable.
        let payload = Tier3PollResultPayload {
            poll_id: poll_id(),
            result: star_result(),
        };
        let ev = make_event_with_payload(
            PollEventKindCode::PollResult,
            RATIFICATION_END_MS + 2000,
            addr(0xfe),
            encode(&payload),
        );
        assert_eq!(
            verify_sr(&ev, &state),
            Err(VerifyError::TallySharesNotReady)
        );
    }

    // 15c. ZEB-858: se-mode poll with a FULLY recoverable tally (threshold
    // shares applied) but a deliberately wrong claimed result. The recompute
    // succeeds and differs from the claim → genuine forgery → `TallyMismatch`
    // (the split must NOT weaken forgery detection).
    #[test]
    fn verify_sr_forgery_returns_tally_mismatch() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        // Three ballots so the recompute yields non-zero score sums.
        let ballots = [[5u64, 1, 0], [5, 2, 0], [5, 3, 0]];
        for (i, scores) in ballots.iter().enumerate() {
            let payload = ts_apply_helpers::build_real_se_ballot_payload(&committee, scores);
            let ev = ts_apply_helpers::rb_event_at_with_actor(
                RATIFICATION_OPEN_MS + i as u64,
                committee.member_addrs[i],
                &payload,
            );
            state.apply_event(&ev).expect("rb apply");
        }
        // Two valid shares (threshold = 2-of-3) → recover_secret_tally is Some.
        for member_idx in [0, 1] {
            let ts_payload =
                ts_apply_helpers::build_real_tally_share_payload(&state, &committee, member_idx, 0);
            let ev = ts_apply_helpers::ts_event_at_with_actor(
                RATIFICATION_END_MS + member_idx as u64,
                committee.member_addrs[member_idx],
                &ts_payload,
            );
            state.apply_event(&ev).expect("ts apply");
        }
        // Satisfy R1.
        let cl_ev = make_event(
            PollEventKindCode::PollClose,
            RATIFICATION_END_MS + 1000,
            addr(0xff),
        );
        state.apply_event(&cl_ev).expect("cl");
        // Deliberately wrong result: real recompute yields non-zero total_scores.
        let wrong_result = StarResult {
            winner: crate::community_voting_star::CandidateRef {
                event_hash: [0x00; 32],
                approval_count: 0,
            },
            finalists: vec![],
            total_scores: vec![0, 0, 0],
            runoff_votes: vec![0],
        };
        let payload = Tier3PollResultPayload {
            poll_id: poll_id(),
            result: wrong_result,
        };
        let ev = make_event_with_payload(
            PollEventKindCode::PollResult,
            RATIFICATION_END_MS + 2000,
            addr(0xfe),
            encode(&payload),
        );
        assert_eq!(verify_sr(&ev, &state), Err(VerifyError::TallyMismatch));
    }

    // ── verify_ratification_ballot tests ─────────────────────────────────────

    /// Build a poll that is in Stage::Ratification with status_quo + 1 real candidate.
    /// electorate = addr(0..19), mini_public primary = [addr(1), addr(2), addr(3)],
    /// sortition_size=3 (so threshold = ceil(3/2) = 2 and 2 approvals suffice).
    ///
    /// Simulates materialize() by synthesizing status_quo into candidates at drafting open.
    fn poll_in_ratification() -> Tier3PollState {
        // Use sortition_size=3 so threshold = ceil(3/2)=2 matches the actual primary size.
        let mut meta = meta_at(0);
        meta.config.sortition_size = 3; // dw=10s, fw=10s; threshold = ceil(3/2)=2
        let mut poll = Tier3PollState::new_from_create(meta, electorate(20));
        // primary = [addr(1), addr(2), addr(3)], backup = [] so no backup needed
        poll.apply_event(&ss_event(10, vec![addr(1), addr(2), addr(3)], vec![]))
            .expect("ss");
        // Add 1 candidate with 2 approvals (threshold = ceil(3/2)=2 → passes)
        let dc = dc_event(200, addr(1), "proposal A");
        poll.apply_event(&dc).expect("dc");
        let candidate_hash = sha256_of_signing_bytes(&dc);
        poll.apply_event(&da_event(300, addr(2), candidate_hash))
            .expect("da");
        // Simulate materialize() synthesizing status_quo at drafting open.
        let sq = synthesize_status_quo(&poll.meta.poll_id);
        poll.candidates.push(sq);
        poll
    }

    // ── ZEB-859: verify_cl + close_condition_met ─────────────────────────────
    //
    // `poll_in_ratification()` builds a `pu`-mode poll with create_wall=0 and
    // deliberation/drafting/ratification windows of 10s each, so:
    //   * total_window_ms       = (10+10+10)*1000 = 30_000
    //   * Ratification stage     opens at wall >= dw+fw = 20_000
    //   * close condition (kd=cl) is met at wall >= create + total = 30_000

    /// A close event whose (clamped) wall is exactly `create + total_window`
    /// and whose stage is Ratification is a legitimate engine-auto close → Ok.
    #[test]
    fn verify_cl_accepts_legitimate_close() {
        let poll = poll_in_ratification();
        let close_wall = 30_000; // create(0) + total_window(30_000)
        let ev = make_event(PollEventKindCode::PollClose, close_wall, addr(7));
        // receiver_now == event wall ⇒ clamp is a no-op.
        assert_eq!(verify_cl(&ev, &poll, close_wall), Ok(()));
    }

    /// A close event whose stage is still before Ratification (here Drafting,
    /// at create + deliberation) must be rejected regardless of the window math.
    #[test]
    fn verify_cl_rejects_premature_stage() {
        let poll = poll_in_ratification();
        let drafting_wall = 10_000; // create(0) + deliberation(10_000) ⇒ Drafting
        let ev = make_event(PollEventKindCode::PollClose, drafting_wall, addr(7));
        assert_eq!(
            verify_cl(&ev, &poll, drafting_wall),
            Err(VerifyError::CloseConditionNotMet)
        );
    }

    /// Stage is Ratification but the ratification window has NOT fully elapsed
    /// (25_000 is past the 20_000 Ratification boundary but below the 30_000
    /// close threshold) → rejected.
    #[test]
    fn verify_cl_rejects_window_not_elapsed() {
        let poll = poll_in_ratification();
        let mid_ratification_wall = 25_000; // Ratification (>=20_000) but < 30_000
        let ev = make_event(PollEventKindCode::PollClose, mid_ratification_wall, addr(7));
        assert_eq!(
            verify_cl(&ev, &poll, mid_ratification_wall),
            Err(VerifyError::CloseConditionNotMet)
        );
    }

    /// A future-stamped close event (peer wall = create + total + 10h) whose
    /// receiver clock says almost no time has passed must be rejected: the
    /// receiver-now clamp (`event.wall.min(now + MAX_FORWARD_SKEW)`) pulls the
    /// effective wall below the close threshold. Requires total_window >
    /// MAX_FORWARD_SKEW so the clamp is decisive, hence the enlarged windows.
    #[test]
    fn verify_cl_rejects_future_stamped_wall() {
        let mut poll = poll_in_ratification();
        // total_window = (60+60+600)*1000 = 720_000 ms (12 min) > MAX_FORWARD_SKEW (5 min).
        poll.meta.config.deliberation_window_seconds = 60;
        poll.meta.config.drafting_window_seconds = 60;
        poll.meta.config.ratification_window_seconds = 600;
        let total_window_ms: u64 = 720_000;
        let ten_hours_ms: u64 = 10 * 60 * 60 * 1000;
        let future_wall = total_window_ms + ten_hours_ms; // peer claims window elapsed
        let receiver_now = poll.meta.poll_create_hlc.wall_ms; // == 0: real clock ≈ create
        let ev = make_event(PollEventKindCode::PollClose, future_wall, addr(7));
        // Sanity: an UNCLAMPED evaluation at the raw peer wall WOULD pass — proving
        // the clamp (not the stage/window predicate) is what rejects the forgery.
        assert!(poll.close_condition_met(future_wall));
        assert_eq!(
            verify_cl(&ev, &poll, receiver_now),
            Err(VerifyError::CloseConditionNotMet)
        );
    }

    /// Parity guard: `close_condition_met` fires exactly at the wall where the
    /// engine's `trigger_kd_cl` predicate fires (create + total_window) and not
    /// one millisecond before. Any drift here would make a node reject its own
    /// legitimate engine-auto close.
    #[test]
    fn verify_cl_matches_trigger_predicate() {
        let poll = poll_in_ratification();
        let close_threshold = 30_000; // create(0) + total_window(30_000)
        assert!(!poll.close_condition_met(close_threshold - 1));
        assert!(poll.close_condition_met(close_threshold));
    }

    /// ZEB-859/858: `apply_event`'s PollClose arm is first-close-wins. The
    /// `close_event_hash` (the second half of the verify_sr memo key) freezes on
    /// the first applied kd=cl; a SECOND distinct kd=cl on a different lane must
    /// NOT overwrite it. Without this an insider could spam distinct closes (each
    /// legitimate in the pre-finalize window) to churn the memo key, force memo
    /// misses, and re-run the expensive se-mode threshold-decrypt — bypassing the
    /// DoS bound. The post-match watermark tail must STILL run for the second
    /// event (only the hash assignment is gated), so the second apply returns Ok.
    #[test]
    fn apply_kd_cl_first_close_wins_hash_stable() {
        let mut poll = poll_in_ratification();

        // First close (lane = addr(7)) freezes close_event_hash.
        let first_cl = make_event(PollEventKindCode::PollClose, 30_000, addr(7));
        poll.apply_event(&first_cl).expect("first cl applies");
        let first_hash = poll.close_event_hash.expect("first close records a hash");
        assert_eq!(
            first_hash,
            sha256_of_signing_bytes(&first_cl),
            "close_event_hash is the first close's signing-bytes hash",
        );

        // A SECOND distinct close (different signer + wall ⇒ different signing
        // bytes ⇒ different hash) on its own lane. Fixture sanity: it hashes
        // differently, so an overwrite would be observable.
        let second_cl = make_event(PollEventKindCode::PollClose, 31_000, addr(8));
        assert_ne!(
            sha256_of_signing_bytes(&second_cl),
            first_hash,
            "fixture sanity: the second close hashes differently",
        );
        // The apply itself succeeds — the (actor, device) watermark tail still
        // runs for the second event even though the hash assignment is gated.
        poll.apply_event(&second_cl)
            .expect("second cl applies (watermark tail still runs)");
        assert_eq!(
            poll.close_event_hash,
            Some(first_hash),
            "first-close-wins: close_event_hash unchanged by the second kd=cl",
        );
        // And the second event's lane watermark was raised (tail ran).
        let lane = (second_cl.actor, second_cl.hlc.device_id.clone());
        assert_eq!(
            poll.last_received_hlc.get(&lane).copied(),
            Some((31_000, 0)),
            "post-match watermark tail ran for the gated second close",
        );
    }

    // 16. verify_rb_actor_in_full_electorate_accepted
    #[test]
    fn verify_rb_actor_in_full_electorate_accepted() {
        let poll = poll_in_ratification();
        // The electorate is addr(0..19); any of them can cast a ratification ballot.
        // wall_ms = 25000 > 20000 → Ratification stage
        // Candidate count = 2 (1 real + status_quo)
        let rb_payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(vec![3, 1]), // 2 candidates: real + status_quo
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        let ev = make_event_with_payload(
            PollEventKindCode::RatificationBallot,
            25000,
            addr(7), // in electorate (electorate(20) covers addr(0)..addr(19))
            encode(&rb_payload),
        );
        assert_eq!(verify_ratification_ballot(&ev, &poll), Ok(()));
    }

    // 17. verify_rb_actor_not_in_electorate_rejected
    #[test]
    fn verify_rb_actor_not_in_electorate_rejected() {
        let poll = poll_in_ratification();
        let rb_payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(vec![3, 1]),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        let ev = make_event_with_payload(
            PollEventKindCode::RatificationBallot,
            25000,
            addr(200), // NOT in electorate (electorate is addr(0)..addr(19))
            encode(&rb_payload),
        );
        assert_eq!(
            verify_ratification_ballot(&ev, &poll),
            Err(VerifyError::NotInEligibleElectorate(addr(200)))
        );
    }

    // 18. verify_rb_wrong_stage_rejected_NotInRatificationStage
    #[test]
    fn verify_rb_wrong_stage_rejected_not_in_ratification_stage() {
        let poll = poll_in_ratification();
        let rb_payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(vec![3, 1]),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        // wall_ms = 5000 → still in Deliberation stage (dw not elapsed yet)
        let ev = make_event_with_payload(
            PollEventKindCode::RatificationBallot,
            5000,
            addr(7),
            encode(&rb_payload),
        );
        assert_eq!(
            verify_ratification_ballot(&ev, &poll),
            Err(VerifyError::NotInRatificationStage)
        );
    }

    // 19. verify_rb_score_above_5_rejected_via_BallotInvalid
    #[test]
    fn verify_rb_score_above_5_rejected_via_ballot_invalid() {
        let poll = poll_in_ratification();
        let rb_payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(vec![6, 1]), // score 6 > 5 → BallotScoreOutOfRange
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        let ev = make_event_with_payload(
            PollEventKindCode::RatificationBallot,
            25000,
            addr(7),
            encode(&rb_payload),
        );
        let result = verify_ratification_ballot(&ev, &poll);
        assert!(
            matches!(
                result,
                Err(VerifyError::BallotInvalid(
                    ValidateError::BallotScoreOutOfRange(6)
                ))
            ),
            "expected BallotInvalid(BallotScoreOutOfRange(6)), got {result:?}"
        );
    }

    // 20. verify_rb_wrong_length_rejected_via_BallotInvalid
    #[test]
    fn verify_rb_wrong_length_rejected_via_ballot_invalid() {
        let poll = poll_in_ratification();
        // Expected 2 candidates (1 real + status_quo); provide 3 scores → mismatch
        let rb_payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(vec![3, 1, 5]),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        let ev = make_event_with_payload(
            PollEventKindCode::RatificationBallot,
            25000,
            addr(7),
            encode(&rb_payload),
        );
        let result = verify_ratification_ballot(&ev, &poll);
        assert!(
            matches!(
                result,
                Err(VerifyError::BallotInvalid(
                    ValidateError::BallotLengthMismatch {
                        scores: 3,
                        expected: 2
                    }
                ))
            ),
            "expected BallotLengthMismatch, got {result:?}"
        );
    }

    // ── verify_da tests ───────────────────────────────────────────────────────

    // 21. verify_da_known_candidate_accepted
    #[test]
    fn verify_da_known_candidate_accepted() {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(10, vec![addr(1), addr(2)], vec![]))
            .expect("ss");
        let dc = dc_event(200, addr(1), "my proposal");
        poll.apply_event(&dc).expect("dc");
        let candidate_hash = sha256_of_signing_bytes(&dc);

        let payload = DraftApprovalPayload {
            poll_id: poll_id(),
            candidate_event_hash: candidate_hash,
        };
        let ev = make_event_with_payload(
            PollEventKindCode::DraftApproval,
            300,
            addr(2),
            encode(&payload),
        );
        assert_eq!(verify_da_candidate_exists(&ev, &poll), Ok(()));
    }

    // 22. verify_da_unknown_candidate_rejected_UnknownCandidate
    #[test]
    fn verify_da_unknown_candidate_rejected_unknown_candidate() {
        let poll = new_poll(0);
        // No candidates registered yet
        let payload = DraftApprovalPayload {
            poll_id: poll_id(),
            candidate_event_hash: [0xde; 32], // unknown
        };
        let ev = make_event_with_payload(
            PollEventKindCode::DraftApproval,
            300,
            addr(2),
            encode(&payload),
        );
        assert_eq!(
            verify_da_candidate_exists(&ev, &poll),
            Err(VerifyError::UnknownCandidate)
        );
    }

    // ── Cluster A regression tests ────────────────────────────────────────────

    // A1: verify_sr on a candidate-less poll (kd=ss + kd=cl applied, no kd=dc) must
    //     NOT panic. ZEB-850 Task 5: status_quo is now ALWAYS synthesized into the
    //     ranking set (verify_sr mirrors the engine's apply-time locals), so the
    //     poll is never "status_quo not synthesized" — the ratification set is the
    //     degenerate [status_quo]. The claimed result here (a 2-finalist
    //     star_result()) cannot match the recompute over [status_quo] with zero
    //     ballots, so verify_sr correctly rejects with TallyMismatch.
    #[test]
    fn verify_sr_candidateless_poll_result_mismatch_rejected_tally_mismatch() {
        // Poll in Stage::Deliberation (sortition done, dw not elapsed).
        let mut poll = new_poll(0); // create at 0, dw=10s, fw=10s
        poll.apply_event(&ss_event(10, vec![addr(1), addr(2), addr(3)], vec![]))
            .expect("ss");
        // Apply kd=cl so the R1 close-applied check is satisfied.
        let cl_ev = make_event(PollEventKindCode::PollClose, 5000, addr(0xff));
        poll.apply_event(&cl_ev).expect("cl");
        // No kd=dc applied — candidates empty; ranking set is just [status_quo].
        assert!(poll.candidates.is_empty());

        let payload = Tier3PollResultPayload {
            poll_id: poll_id(),
            result: star_result(),
        };
        let ev = make_event_with_payload(
            PollEventKindCode::PollResult,
            5500,
            addr(0xfe),
            encode(&payload),
        );
        // No panic; the fabricated result != the recompute over [status_quo]
        // (zero ballots) → TallyMismatch.
        let result = verify_sr(&ev, &poll);
        assert_eq!(
            result,
            Err(VerifyError::TallyMismatch),
            "expected TallyMismatch, got {result:?}"
        );
    }

    // A2: verify_ratification_ballot on a candidate-less poll (no kd=dc) in the
    //     Ratification window must NOT panic. ZEB-850 Task 5: status_quo is now
    //     always synthesized, so the degenerate ratification set is [status_quo] =
    //     1 candidate. A ballot carrying 2 scores is legitimately length-mismatched
    //     against that single-candidate set → BallotLengthMismatch.
    #[test]
    fn verify_ratification_ballot_candidateless_ratification_poll_rejects_length_mismatch() {
        // Poll in the Ratification time window, but no kd=dc → candidates empty.
        let mut meta = meta_at(0);
        meta.config.sortition_size = 3;
        let mut poll = Tier3PollState::new_from_create(meta, electorate(20));
        poll.apply_event(&ss_event(10, vec![addr(1), addr(2), addr(3)], vec![]))
            .expect("ss");
        // Wall_ms=25000 → past dw+fw threshold (20000) → Ratification stage.
        assert!(poll.candidates.is_empty());

        let rb_payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(vec![3, 1]), // 2 scores, but the degenerate set is [status_quo] = 1
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        let ev = make_event_with_payload(
            PollEventKindCode::RatificationBallot,
            25000,
            addr(5), // in electorate
            encode(&rb_payload),
        );
        // No panic; 2 scores vs the 1-candidate [status_quo] set → length mismatch.
        let result = verify_ratification_ballot(&ev, &poll);
        assert_eq!(
            result,
            Err(VerifyError::BallotInvalid(
                ValidateError::BallotLengthMismatch {
                    scores: 2,
                    expected: 1,
                }
            )),
            "expected BallotLengthMismatch (2 vs 1), got {result:?}"
        );
    }

    // ── Cluster B regression test ────────────────────────────────────────────

    // B1: primary[i] declines → backup[0] promoted; then backup[0] declines →
    //     backup[1] takes their slot; set size stays = sortition_size and
    //     backup[0] is NOT in the set.
    #[test]
    fn current_mini_public_promoted_backup_decline_cascades_correctly() {
        // primary=[addr(1), addr(2)], backup=[addr(10), addr(11), addr(12)]
        // sortition_size=2 (so mini-public should always be 2 members)
        let mut meta = meta_at(0);
        meta.config.sortition_size = 2;
        let electorate_addrs = vec![
            addr(1),
            addr(2),
            addr(10),
            addr(11),
            addr(12),
            addr(20),
            addr(21),
        ];
        let mut poll = Tier3PollState::new_from_create(meta, electorate_addrs);
        poll.apply_event(&ss_event(
            10,
            vec![addr(1), addr(2)],
            vec![addr(10), addr(11), addr(12)],
        ))
        .expect("ss");

        // Step 1: addr(1) (primary) declines → addr(10) (backup[0]) promoted.
        poll.apply_event(&md_event(20, addr(1)))
            .expect("md addr(1)");
        let mp = poll.current_mini_public(&hlc(30));
        assert_eq!(mp.len(), 2, "mini-public size must stay 2");
        assert!(mp.contains(&addr(2)), "addr(2) still primary");
        assert!(mp.contains(&addr(10)), "addr(10) promoted from backup");
        assert!(!mp.contains(&addr(1)), "addr(1) declined");

        // Step 2: addr(10) (promoted backup) declines → addr(11) (backup[1]) takes slot.
        poll.apply_event(&md_event(40, addr(10)))
            .expect("md addr(10)");
        let mp2 = poll.current_mini_public(&hlc(50));
        assert_eq!(
            mp2.len(),
            2,
            "mini-public size must stay 2 after second decline"
        );
        assert!(mp2.contains(&addr(2)), "addr(2) still primary");
        assert!(
            mp2.contains(&addr(11)),
            "addr(11) must be promoted (backup[1])"
        );
        assert!(
            !mp2.contains(&addr(10)),
            "addr(10) declined — must not be in mini-public"
        );
        assert!(!mp2.contains(&addr(1)), "addr(1) still declined");
    }

    // ── Cluster D regression test ────────────────────────────────────────────

    // D1: verify_ss uses the poll's stored epoch, not a live epoch from the oracle.
    // Simulates a committee refresh between beacon creation and verify_ss invocation:
    // the MockBeaconOracle is indexed by (community_id, seed) only, ignoring epoch,
    // confirming that verify_ss passes the poll's epoch (not a stale live epoch) to
    // the oracle.
    #[tokio::test]
    async fn verify_ss_uses_poll_epoch_not_live_epoch() {
        // Build an EpochTrackingOracle that records the epoch it receives.
        struct EpochTrackingOracle {
            seed: [u8; 32],
            vrf: [u8; 32],
            received_epoch: std::sync::Arc<tokio::sync::Mutex<Option<u64>>>,
        }

        #[async_trait::async_trait]
        impl BeaconOracle for EpochTrackingOracle {
            async fn vrf_output_for(
                &self,
                _community_id: &crate::owner_state_types::SpaceId,
                seed: &[u8; 32],
                epoch: u64,
            ) -> Option<[u8; 32]> {
                if seed == &self.seed {
                    *self.received_epoch.lock().await = Some(epoch);
                    Some(self.vrf)
                } else {
                    None
                }
            }
        }

        let poll = poll_state_with_electorate(0, 10);
        // poll.meta.community_epoch = 1 (set by meta_at).
        let expected_epoch = poll.meta.community_epoch;
        assert_eq!(expected_epoch, 1, "fixture epoch is 1");

        let seed = default_seed();
        let vrf = [0x55u8; 32];
        let received = std::sync::Arc::new(tokio::sync::Mutex::new(None::<u64>));
        let oracle = EpochTrackingOracle {
            seed,
            vrf,
            received_epoch: std::sync::Arc::clone(&received),
        };

        let payload = correct_ss_payload(&poll, &vrf);
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionSelection,
            100,
            addr(0xfe),
            encode(&payload),
        );
        let result = verify_ss(&ev, &poll, &oracle, &community_id()).await;
        assert_eq!(result, Ok(()), "verify_ss must succeed");

        // Confirm that the epoch passed to the oracle was the POLL's epoch (1),
        // not any other value.
        let epoch_seen = received.lock().await;
        assert_eq!(
            *epoch_seen,
            Some(expected_epoch),
            "verify_ss must pass poll's stored epoch to BeaconOracle::vrf_output_for"
        );
    }

    // ── Task 3: kd=ds apply_event tests ──────────────────────────────────────

    /// Build a poll in Deliberation stage with primary=[addr(1), addr(2), addr(3)]
    /// and backup=[addr(10), addr(11)].  Poll created at wall_ms=0, dw=10000ms.
    /// Sortition event applied at wall_ms=50.  Any event HLC in 51..9999 is
    /// within the Deliberation window.
    fn build_poll_in_deliberation_stage() -> Tier3PollState {
        let mut poll = new_poll(0);
        poll.apply_event(&ss_event(
            50,
            vec![addr(1), addr(2), addr(3)],
            vec![addr(10), addr(11)],
        ))
        .expect("ss");
        poll
    }

    /// Build a kd=ds event with a custom text string.
    fn ds_event_with_text(wall_ms: u64, actor: OwnerAddr, text: &str) -> SignedVotingEvent {
        let payload = DeliberationStatementPayload {
            poll_id: poll_id(),
            text: text.into(),
        };
        make_event_with_payload(
            PollEventKindCode::DeliberationStatement,
            wall_ms,
            actor,
            encode(&payload),
        )
    }

    // Task 3 — Test 1: happy path accepts mini-public member in deliberation window.
    #[test]
    fn apply_ds_accepts_mini_public_member_in_window() {
        let mut poll = build_poll_in_deliberation_stage();
        let author = addr(1); // primary member
        let ev = ds_event_with_text(100, author, "Hello world");
        poll.apply_event(&ev).expect("apply");
        assert_eq!(poll.deliberation.statements.len(), 1);
        assert_eq!(
            poll.deliberation.statements_per_author[&author], 1,
            "statements_per_author must be incremented"
        );
    }

    // Task 3 — Test 2: drops event from actor not in mini-public.
    #[test]
    fn apply_ds_drops_non_mini_public_actor() {
        let mut poll = build_poll_in_deliberation_stage();
        // addr(99) is not in primary or backup.
        let ev = ds_event_with_text(100, addr(99), "Hello from outsider");
        poll.apply_event(&ev)
            .expect("apply returns Ok (silent drop)");
        assert!(
            poll.deliberation.statements.is_empty(),
            "non-mini-public actor must be silently dropped"
        );
    }

    // Task 3 — Test 3: drops event with text longer than 280 chars.
    #[test]
    fn apply_ds_drops_281_char_text() {
        let mut poll = build_poll_in_deliberation_stage();
        let long_text = "x".repeat(281);
        let ev = ds_event_with_text(100, addr(1), &long_text);
        poll.apply_event(&ev)
            .expect("apply returns Ok (silent drop)");
        assert!(
            poll.deliberation.statements.is_empty(),
            "281-char text must be silently dropped"
        );
    }

    // Task 3 — Test 4: drops event whose text is whitespace-only.
    #[test]
    fn apply_ds_drops_whitespace_only_text() {
        let mut poll = build_poll_in_deliberation_stage();
        let ev = ds_event_with_text(100, addr(1), "   \t\n  ");
        poll.apply_event(&ev)
            .expect("apply returns Ok (silent drop)");
        assert!(
            poll.deliberation.statements.is_empty(),
            "whitespace-only text must be silently dropped"
        );
    }

    // Task 3 — Test 5: enforces per-actor 5-statement spam cap.
    #[test]
    fn apply_ds_enforces_5_statement_spam_cap() {
        let mut poll = build_poll_in_deliberation_stage();
        let author = addr(2); // primary member

        // Apply 6 statements with distinct text + monotonically increasing HLCs.
        for i in 0u64..6 {
            let text = format!("Statement number {i}");
            let ev = ds_event_with_text(100 + i, author, &text);
            poll.apply_event(&ev).expect("apply returns Ok");
        }

        assert_eq!(
            poll.deliberation.statements.len(),
            5,
            "only 5 statements must be accepted (spam cap)"
        );
        assert_eq!(
            poll.deliberation.statements_per_author[&author], 5,
            "statements_per_author must cap at 5"
        );
    }

    // Task 3 — Test 6: drops event with HLC outside the deliberation window.
    #[test]
    fn apply_ds_drops_event_outside_deliberation_window() {
        let mut poll = build_poll_in_deliberation_stage();
        // dw=10s → deliberation ends at wall_ms=10000.  wall_ms=15000 is in Drafting.
        let ev = ds_event_with_text(15_000, addr(1), "Too late");
        poll.apply_event(&ev)
            .expect("apply returns Ok (silent drop)");
        assert!(
            poll.deliberation.statements.is_empty(),
            "event outside deliberation window must be silently dropped"
        );
    }

    // ── Task 4: kd=dv apply_event tests ──────────────────────────────────────

    /// Build a kd=dv event with a specific vote byte (raw u8, so we can test invalid values).
    fn dv_event_raw(
        wall_ms: u64,
        actor: OwnerAddr,
        statement_hash: [u8; 32],
        vote_byte: u8,
    ) -> SignedVotingEvent {
        let payload = DeliberationVotePayload {
            poll_id: poll_id(),
            statement_event_hash: statement_hash,
            vote: vote_byte,
        };
        make_event_with_payload(
            PollEventKindCode::DeliberationVote,
            wall_ms,
            actor,
            encode(&payload),
        )
    }

    /// Build a kd=dv event with a valid BridgingVoteCode.
    fn dv_event(
        wall_ms: u64,
        actor: OwnerAddr,
        statement_hash: [u8; 32],
        vote: crate::community_voting_core::BridgingVoteCode,
    ) -> SignedVotingEvent {
        dv_event_raw(wall_ms, actor, statement_hash, vote.as_u8())
    }

    // Task 4 — Test 1: happy path — mini-public member votes agree on existing statement.
    #[test]
    fn apply_dv_accepts_valid_vote_from_mini_public() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();
        // addr(1) posts a statement.
        let ds_ev = ds_event_with_text(100, addr(1), "Statement from addr(1)");
        poll.apply_event(&ds_ev).expect("ds apply");
        let stmt_hash = sha256_of_signing_bytes(&ds_ev);

        // addr(2) votes agree on that statement.
        let dv_ev = dv_event(200, addr(2), stmt_hash, BridgingVoteCode::Agree);
        poll.apply_event(&dv_ev).expect("dv apply");

        let key = (addr(2), stmt_hash);
        let entry = poll
            .deliberation
            .votes
            .get(&key)
            .expect("vote entry must exist");
        assert_eq!(entry.vote, BridgingVoteCode::Agree, "vote must be Agree");
        assert_eq!(entry.voter, addr(2));
        assert_eq!(entry.statement_event_hash, stmt_hash);
    }

    // Task 4 — Test 2: drops vote referencing a non-existent statement.
    #[test]
    fn apply_dv_drops_non_existent_statement_reference() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();
        let fake_hash = [0x99u8; 32];
        let dv_ev = dv_event(100, addr(1), fake_hash, BridgingVoteCode::Agree);
        poll.apply_event(&dv_ev)
            .expect("apply returns Ok (silent drop)");
        assert!(
            poll.deliberation.votes.is_empty(),
            "vote targeting non-existent statement must be silently dropped"
        );
    }

    // Task 4 — Test 3: drops vote from actor not in mini-public.
    #[test]
    fn apply_dv_drops_non_mini_public_voter() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();
        // addr(1) posts a statement.
        let ds_ev = ds_event_with_text(100, addr(1), "Some statement");
        poll.apply_event(&ds_ev).expect("ds apply");
        let stmt_hash = sha256_of_signing_bytes(&ds_ev);

        // addr(99) is not in primary or backup.
        let dv_ev = dv_event(200, addr(99), stmt_hash, BridgingVoteCode::Agree);
        poll.apply_event(&dv_ev)
            .expect("apply returns Ok (silent drop)");
        assert!(
            poll.deliberation.votes.is_empty(),
            "vote from non-mini-public actor must be silently dropped"
        );
    }

    // Task 4 — Test 4: LWW — later HLC wins (revote replaces earlier vote).
    #[test]
    fn apply_dv_revote_lww_later_hlc_wins() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();
        let ds_ev = ds_event_with_text(100, addr(1), "A statement");
        poll.apply_event(&ds_ev).expect("ds");
        let stmt_hash = sha256_of_signing_bytes(&ds_ev);

        // First vote: agree at hlc=200.
        let v1 = dv_event(200, addr(2), stmt_hash, BridgingVoteCode::Agree);
        poll.apply_event(&v1).expect("v1");

        // Second vote: disagree at hlc=300 (later). LWW: disagree wins.
        let v2 = dv_event(300, addr(2), stmt_hash, BridgingVoteCode::Disagree);
        poll.apply_event(&v2).expect("v2");

        let entry = poll
            .deliberation
            .votes
            .get(&(addr(2), stmt_hash))
            .expect("entry must exist");
        assert_eq!(
            entry.vote,
            BridgingVoteCode::Disagree,
            "later vote must win"
        );
    }

    // Task 4 — Test 5: LWW — earlier HLC dropped (monotonic check may reject first).
    #[test]
    fn apply_dv_revote_lww_earlier_hlc_dropped() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();
        let ds_ev = ds_event_with_text(100, addr(1), "A statement");
        poll.apply_event(&ds_ev).expect("ds");
        let stmt_hash = sha256_of_signing_bytes(&ds_ev);

        // First vote: disagree at hlc=200 (later HLC applied first).
        let v1 = dv_event(200, addr(2), stmt_hash, BridgingVoteCode::Disagree);
        poll.apply_event(&v1).expect("v1");

        // Second attempt: agree at hlc=100 (earlier). apply_event's monotonic HLC check
        // rejects this with HlcNotMonotonic — the existing Disagree entry stays either way.
        let _ = poll.apply_event(&dv_event(100, addr(2), stmt_hash, BridgingVoteCode::Agree));

        let entry = poll
            .deliberation
            .votes
            .get(&(addr(2), stmt_hash))
            .expect("entry must still exist");
        assert_eq!(
            entry.vote,
            BridgingVoteCode::Disagree,
            "earlier vote must not overwrite later vote"
        );
    }

    // Task 4 — Test 6: drops vote with invalid vote byte (3).
    #[test]
    fn apply_dv_drops_vote_byte_3() {
        let mut poll = build_poll_in_deliberation_stage();
        let ds_ev = ds_event_with_text(100, addr(1), "A statement");
        poll.apply_event(&ds_ev).expect("ds");
        let stmt_hash = sha256_of_signing_bytes(&ds_ev);

        // Build vote with raw byte=3 (out of range).
        let dv_ev = dv_event_raw(200, addr(2), stmt_hash, 3);
        poll.apply_event(&dv_ev)
            .expect("apply returns Ok (silent drop)");
        assert!(
            poll.deliberation.votes.is_empty(),
            "vote byte=3 (out of range) must be silently dropped"
        );
    }

    // Bot-review #1 fix (CodeRabbit): the apply layer must reject self-votes
    // because deliberation votes are for *another* member's statement (spec
    // §2.3). Counting self-votes would distort statement totals and bridging
    // scores.
    #[test]
    fn apply_dv_drops_self_vote_on_own_statement() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();
        let author = addr(1); // primary mini-public member
        let ds_ev = ds_event_with_text(100, author, "My own statement");
        poll.apply_event(&ds_ev).expect("ds apply");
        let stmt_hash = sha256_of_signing_bytes(&ds_ev);

        // Same actor tries to vote on their own statement.
        let dv_ev = dv_event(200, author, stmt_hash, BridgingVoteCode::Agree);
        poll.apply_event(&dv_ev)
            .expect("apply returns Ok (silent drop)");

        assert!(
            poll.deliberation.votes.is_empty(),
            "self-vote on own statement must be silently dropped"
        );
        // The other mini-public member can still vote on it normally.
        let other_ev = dv_event(300, addr(2), stmt_hash, BridgingVoteCode::Agree);
        poll.apply_event(&other_ev).expect("other apply");
        assert_eq!(
            poll.deliberation.votes.len(),
            1,
            "non-self votes on the same statement remain accepted"
        );
    }

    // Bot-review #2 fix (CodeAnt): replaying the same kd=ds event must be
    // idempotent — the BTreeMap insert is idempotent on event_hash, but the
    // per-author spam counter must NOT bump on replay, otherwise rehydrating
    // a log with duplicate events would falsely advance toward the 5-cap
    // and block legitimate later statements.
    //
    // The HLC monotonic guard (apply_event step 2) rejects strictly-less
    // HLCs but accepts equal HLCs, so applying the exact same
    // SignedVotingEvent twice exercises both the BTreeMap idempotency and
    // the apply_event counter-skip guard end-to-end.
    #[test]
    fn apply_ds_replay_is_idempotent_for_spam_cap() {
        let mut poll = build_poll_in_deliberation_stage();
        let author = addr(1);
        let ev = ds_event_with_text(100, author, "Same statement");

        // First apply: counter must bump to 1.
        poll.apply_event(&ev).expect("first apply");
        assert_eq!(
            poll.deliberation.statements_per_author[&author], 1,
            "first apply must increment counter to 1"
        );
        assert_eq!(
            poll.deliberation.statements.len(),
            1,
            "first apply must insert exactly one statement"
        );

        // Replay the exact same SignedVotingEvent. Equal HLCs pass the
        // monotonic guard; the BTreeMap insert returns the existing entry
        // (no-op); the counter MUST NOT bump.
        poll.apply_event(&ev).expect("replay apply");
        assert_eq!(
            poll.deliberation.statements_per_author[&author], 1,
            "replay must NOT bump the per-author counter"
        );
        assert_eq!(
            poll.deliberation.statements.len(),
            1,
            "replay must NOT add a new statement entry"
        );

        // After the replay, four more distinct statements should fit under
        // the 5-cap, and a sixth distinct one is rejected.
        for i in 1u64..=4 {
            let next = ds_event_with_text(100 + i, author, &format!("Statement {i}"));
            poll.apply_event(&next).expect("apply");
        }
        assert_eq!(
            poll.deliberation.statements_per_author[&author], 5,
            "exactly 5 distinct statements must be counted after replay"
        );
        let sixth = ds_event_with_text(200, author, "Sixth");
        poll.apply_event(&sixth).expect("apply");
        assert_eq!(
            poll.deliberation.statements_per_author[&author], 5,
            "6th distinct statement must be dropped (cap reached)"
        );
    }

    // ── ZEB-320: drop paths must not advance the materialize watermark ──────
    //
    // `last_hlc` feeds `current_stage_at` and `current_mini_public` via
    // `build_tier3_export`. Advancing it on silent-dropped events would let
    // peer-originated invalid events push the projected stage forward without
    // any accepted state change. Each test below applies a baseline-accepting
    // event, then an event the apply layer silently drops, and asserts the
    // watermark stayed at the baseline.

    #[test]
    fn dropped_ds_past_deliberation_window_does_not_advance_last_hlc() {
        let mut poll = build_poll_in_deliberation_stage();
        // Baseline: build_poll_in_deliberation_stage applies ss at wall_ms=50.
        assert_eq!(poll.last_hlc.as_ref().unwrap().wall_ms, 50);

        // dw_ms = 10_000 (default test config); at wall_ms = 15_000 the
        // projected stage is Drafting, so the kd=ds Rule 1 check drops it.
        let drop_ev = ds_event_with_text(15_000, addr(1), "Late statement");
        poll.apply_event(&drop_ev).expect("ok (silent drop)");

        assert_eq!(
            poll.last_hlc.as_ref().unwrap().wall_ms,
            50,
            "dropped kd=ds must not advance last_hlc past the baseline"
        );
        // Visible consequence: stage projection at the watermark stays in
        // Deliberation. With the buggy unconditional advance, last_hlc would
        // be 15_000 and current_stage_at would erroneously report Drafting.
        assert_eq!(
            poll.current_stage_at(poll.last_hlc.as_ref().unwrap()),
            Stage::Deliberation,
            "stage watermark must stay in Deliberation after dropped event"
        );
        assert!(
            poll.deliberation.statements.is_empty(),
            "no statement should have been materialized"
        );
    }

    #[test]
    fn dropped_dv_self_vote_does_not_advance_last_hlc() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();
        let author = addr(1);
        let ds_ev = ds_event_with_text(100, author, "Statement");
        poll.apply_event(&ds_ev).expect("ds apply");
        let stmt_hash = sha256_of_signing_bytes(&ds_ev);
        // Baseline after accepted kd=ds: last_hlc.wall_ms = 100.
        assert_eq!(poll.last_hlc.as_ref().unwrap().wall_ms, 100);

        // Self-vote on own statement is silently dropped by apply_event.
        let drop_ev = dv_event(500, author, stmt_hash, BridgingVoteCode::Agree);
        poll.apply_event(&drop_ev).expect("ok (silent drop)");

        assert_eq!(
            poll.last_hlc.as_ref().unwrap().wall_ms,
            100,
            "self-vote drop must not advance last_hlc"
        );
        assert!(
            poll.deliberation.votes.is_empty(),
            "no vote should have been materialized"
        );
    }

    #[test]
    fn dropped_dv_unknown_target_does_not_advance_last_hlc() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();
        let ds_ev = ds_event_with_text(100, addr(1), "Statement");
        poll.apply_event(&ds_ev).expect("ds apply");
        // Baseline after accepted kd=ds: last_hlc.wall_ms = 100.
        assert_eq!(poll.last_hlc.as_ref().unwrap().wall_ms, 100);

        // Reference a statement_event_hash that doesn't exist in the projection.
        let phantom_hash: [u8; 32] = [0x77; 32];
        let drop_ev = dv_event(500, addr(2), phantom_hash, BridgingVoteCode::Agree);
        poll.apply_event(&drop_ev).expect("ok (silent drop)");

        assert_eq!(
            poll.last_hlc.as_ref().unwrap().wall_ms,
            100,
            "unknown-target dv drop must not advance last_hlc"
        );
        assert!(
            poll.deliberation.votes.is_empty(),
            "no vote should have been materialized"
        );
    }

    #[test]
    fn dropped_da_unknown_candidate_does_not_advance_last_hlc() {
        let mut poll = build_poll_in_deliberation_stage();
        // Baseline after ss: last_hlc.wall_ms = 50. No candidates exist yet.
        assert_eq!(poll.last_hlc.as_ref().unwrap().wall_ms, 50);
        assert!(poll.candidates.is_empty());

        // Approve a phantom candidate — silent drop via candidates lookup miss.
        let phantom_hash: [u8; 32] = [0x88; 32];
        let drop_ev = da_event(500, addr(1), phantom_hash);
        poll.apply_event(&drop_ev).expect("ok (silent drop)");

        assert_eq!(
            poll.last_hlc.as_ref().unwrap().wall_ms,
            50,
            "unknown-candidate da drop must not advance last_hlc"
        );
    }

    // Qodo PR #154 bot-pass-1 finding: with Option A alone, `last_hlc` was
    // doing two jobs (projection watermark + monotonic-HLC guard), and
    // not advancing it on drops weakened the guard. An earlier-HLC event
    // arriving after a dropped later-HLC event would silently bypass the
    // guard and never be re-materialized when its prerequisites later
    // arrived (e.g., a kd=dv whose target ds had not yet been delivered).
    //
    // Option B splits the watermarks: `last_received_hlc` advances on every
    // dispatched event (accept or drop) so the guard still surfaces
    // HlcNotMonotonic on out-of-order delivery; `last_hlc` only advances on
    // accepts so the projection is unaffected by silent drops.
    #[test]
    fn guard_still_rejects_earlier_hlc_after_dropped_event() {
        let mut poll = build_poll_in_deliberation_stage();
        // Baseline: last_hlc = 50 (ss accepted).
        assert_eq!(poll.last_hlc.as_ref().unwrap().wall_ms, 50);

        // Drop a ds at wall=15000 (past deliberation window → Rule 1 drop).
        let drop_ev = ds_event_with_text(15_000, addr(1), "Late");
        poll.apply_event(&drop_ev).expect("ok (silent drop)");
        assert_eq!(
            poll.last_hlc.as_ref().unwrap().wall_ms,
            50,
            "drop did not advance projection watermark"
        );
        // But last_received_hlc DID advance — the guard sees the late event.
        assert_eq!(
            poll.max_received_hlc().unwrap().wall_ms,
            15_000,
            "drop advanced received watermark (guard)"
        );

        // Now deliver an honest earlier event. Without the dual watermark,
        // it would slip past `last_hlc=50` and silently apply, leaving the
        // dropped event un-replayed and the replica divergent.
        let earlier_ev = ds_event_with_text(100, addr(1), "Earlier");
        let err = poll
            .apply_event(&earlier_ev)
            .expect_err("guard must reject earlier-HLC delivery");
        assert!(matches!(err, ApplyError::HlcNotMonotonic));
    }

    // ZEB-850 T-VOTE-LANE: a far-future stamp on ONE (actor, device) lane must
    // not freeze another lane's honest events. Under the OLD single global
    // watermark, device A's hour-in-the-future ds raised the lone watermark and
    // every later honest event from device B was rejected HlcNotMonotonic — the
    // GRIEF-LOCKOUT. Per-lane watermarks isolate the two.
    #[test]
    fn future_event_on_one_lane_does_not_stall_another_lane() {
        let mut poll = build_poll_in_deliberation_stage();

        // Device A (actor addr(1)): stamps an hour into the future. Past the 10s
        // deliberation window, so it silently drops — but still advances A's lane.
        let far_future = 3_600_000;
        let a_ev = ds_event_with_text(far_future, addr(1), "from the future");
        poll.apply_event(&a_ev)
            .expect("A dispatch is Ok (silent drop)");

        // Device B (actor addr(2)): two honest statements at now / now+1. Under
        // the OLD global watermark the first B apply would be HlcNotMonotonic
        // (100 < 3_600_000); per-lane, B's lane is untouched by A's future stamp.
        let b1 = ds_event_with_text(100, addr(2), "honest one");
        poll.apply_event(&b1)
            .expect("B@now must apply — A's future stamp is on a different lane");
        let b2 = ds_event_with_text(101, addr(2), "honest two");
        poll.apply_event(&b2)
            .expect("B@now+1 must apply — B's own lane advances monotonically");

        // Both B statements materialized (proves accept, not merely Ok).
        assert_eq!(
            poll.deliberation.statements_per_author[&addr(2)],
            2,
            "both honest B statements must be accepted"
        );
    }

    // ZEB-850 T-VOTE-LANE (CodeRabbit): the receive-watermark lane key is the
    // FULL `(OwnerAddr, device_id)` tuple, not just the actor. The same member
    // on two devices must occupy two lanes — a future stamp from device "dev-a"
    // must not stall an honest event from the SAME actor's device "dev-b". If
    // the key collapsed to actor-only, dev-a's future stamp would advance the
    // shared lane and dev-b's now-event would be rejected HlcNotMonotonic.
    #[test]
    fn future_event_on_one_device_does_not_stall_same_actor_other_device() {
        let mut poll = build_poll_in_deliberation_stage();
        let author = addr(1); // in the mini-public primary set

        // Device "dev-a": an hour into the future → silent drop (past the 10s
        // deliberation window), but advances the (author, "dev-a") lane.
        let mut a_ev = ds_event_with_text(3_600_000, author, "dev-a from the future");
        a_ev.hlc.device_id = "dev-a".into();
        poll.apply_event(&a_ev)
            .expect("dev-a dispatch is Ok (silent drop)");

        // Device "dev-b" (SAME actor): honest statement at wall=100. Its lane
        // (author, "dev-b") is untouched by dev-a's future stamp, so it applies.
        let mut b_ev = ds_event_with_text(100, author, "dev-b honest");
        b_ev.hlc.device_id = "dev-b".into();
        poll.apply_event(&b_ev)
            .expect("dev-b@now must apply — a different device is a different lane");

        // Exactly the dev-b statement materialized (dev-a's dropped).
        assert_eq!(
            poll.deliberation.statements_per_author[&author], 1,
            "only the dev-b statement is accepted; the future dev-a stamp dropped"
        );
        // The dev-a lane watermark did advance (max over lanes reflects it),
        // so a within-lane earlier event on dev-a would still be rejected.
        assert_eq!(
            poll.max_received_hlc().unwrap().wall_ms,
            3_600_000,
            "dev-a lane watermark advanced on its silent drop"
        );
    }

    // ZEB-850 / ZEB-320 / Qodo #154: per-lane must NOT weaken the within-lane
    // guard. On a single (actor, device) lane, an earlier-HLC event delivered
    // after a later event that silently dropped must still be rejected — the
    // dropped event advanced the lane watermark. Neutralizing the guard (or
    // reading last_hlc, which drops don't advance) lets the earlier event slip
    // through and this test fails.
    #[test]
    fn within_lane_earlier_hlc_still_rejected_after_dropped_event() {
        let mut poll = build_poll_in_deliberation_stage();

        // Same lane (addr(1), "test"). A later ds that drops (past the 10s
        // deliberation window) but still advances the lane watermark to 15_000.
        let later = ds_event_with_text(15_000, addr(1), "later, dropped");
        poll.apply_event(&later).expect("Ok (silent drop)");

        // An earlier-HLC ds on the SAME lane must be rejected.
        let earlier = ds_event_with_text(100, addr(1), "earlier");
        let err = poll
            .apply_event(&earlier)
            .expect_err("earlier-HLC on same lane must be rejected");
        assert!(matches!(err, ApplyError::HlcNotMonotonic));
    }

    // ZEB-850: max_received_hlc() is the max (wall_ms, logical) across ALL
    // lanes, independent of BTreeMap lane order. Here the HIGHER watermark sits
    // on the lower-sorted lane (addr(1)), so a naive "last lane wins" would
    // return 100 — the max-over-lanes contract pins 200. The kd=rs mint floor
    // depends on this max.
    #[test]
    fn max_received_hlc_is_max_over_lanes() {
        let mut poll = build_poll_in_deliberation_stage();

        // Lane (addr(1), "test") @ wall=200; lane (addr(2), "test") @ wall=100.
        poll.apply_event(&ds_event_with_text(200, addr(1), "high"))
            .expect("apply high");
        poll.apply_event(&ds_event_with_text(100, addr(2), "low"))
            .expect("apply low");

        assert_eq!(
            poll.max_received_hlc().unwrap().wall_ms,
            200,
            "max_received_hlc must be the max wall_ms across all lanes"
        );
    }

    // ZEB-861 T2: the O(1) `max_received_hlc()` (reads `max_applied`) must equal
    // the old O(lanes) fold over `last_received_hlc` — across accepts,
    // silent-drops, multiple lanes, and empty — because both watermarks advance
    // on every dispatch. Pins the equivalence so the swap is behavior-preserving
    // and the two watermarks can't silently drift in a future edit.
    #[test]
    fn max_received_hlc_equals_max_over_lanes_fold() {
        // Empty poll: no dispatch → both sides None.
        let empty = Tier3PollState::new_from_create(meta_at(1_000), electorate(20));
        let old_empty = empty
            .last_received_hlc
            .values()
            .copied()
            .max();
        assert_eq!(
            empty.max_received_hlc().map(|h| (h.wall_ms, h.logical)),
            old_empty,
            "empty: O(1) must equal the fold (both None)"
        );
        assert!(empty.max_received_hlc().is_none());

        // Populated: accepts on several lanes plus a silently-dropped 6th ds at
        // the HIGHEST wall (the 5-cap drops it, but its lane watermark still
        // advances — the ZEB-320/850 property), so the max must reflect it
        // identically on both sides.
        let mut poll = build_poll_in_deliberation_stage();
        let a1 = addr(1);
        for i in 0..5 {
            poll.apply_event(&ds_event_with_text(100 + i, a1, &format!("s{i}")))
                .expect("apply");
        }
        poll.apply_event(&ds_event_with_text(300, a1, "sixth"))
            .expect("Ok (silent drop by 5-cap)");
        poll.apply_event(&ds_event_with_text(150, addr(2), "other lane"))
            .expect("apply");

        let old = poll
            .last_received_hlc
            .values()
            .copied()
            .max();
        let got = poll.max_received_hlc().map(|h| (h.wall_ms, h.logical));
        assert_eq!(
            got, old,
            "O(1) max_received_hlc must equal the max-over-lanes fold"
        );
        assert_eq!(
            got.unwrap().0,
            300,
            "silently-dropped 6th (wall=300) must still set the max watermark"
        );
        assert_eq!(poll.max_received_hlc().unwrap().device_id, String::new());
    }

    // CodeRabbit PR #154 bot-pass-1 nitpick: cover the remaining gated drop
    // branches with explicit watermark assertions (kd=ds spam-cap; kd=dv
    // stage-mismatch / mp-not-member / invalid-vote-byte). LWW-stale is
    // unreachable in single-device single-thread tests because the HLC
    // guard catches strictly-earlier delivery first; the implementation
    // still gates it (see the `else { advance_last_hlc = false; }` branch
    // in the LWW match).
    #[test]
    fn remaining_drop_branches_do_not_advance_last_hlc() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();

        // Set up: author1 fills the 5-statement spam cap; author2 posts one
        // statement so dv has a valid target to point at.
        let author1 = addr(1);
        let author2 = addr(2);
        for i in 0..5 {
            let ev = ds_event_with_text(100 + i, author1, &format!("Stmt {i}"));
            poll.apply_event(&ev).expect("apply");
        }
        let target_ev = ds_event_with_text(200, author2, "Target statement");
        poll.apply_event(&target_ev).expect("apply target");
        let target_hash = sha256_of_signing_bytes(&target_ev);

        // Snapshot the projection watermark after the last accept.
        let baseline = poll.last_hlc.clone().expect("baseline last_hlc");
        assert_eq!(baseline.wall_ms, 200);

        // Each case: an event the apply layer silently drops. HLCs are
        // strictly increasing across cases to satisfy the monotonic guard
        // (which now reads last_received_hlc, not last_hlc).
        let dv_other = addr(3); // mini-public member who hasn't voted yet
        let drop_cases: Vec<(SignedVotingEvent, &str)> = vec![
            (
                ds_event_with_text(300, author1, "Sixth"),
                "kd=ds spam-cap (6th statement)",
            ),
            (
                dv_event_raw(400, dv_other, target_hash, 99),
                "kd=dv invalid vote byte",
            ),
            (
                dv_event(500, addr(99), target_hash, BridgingVoteCode::Agree),
                "kd=dv actor not in mini-public",
            ),
            (
                dv_event(15_000, dv_other, target_hash, BridgingVoteCode::Agree),
                "kd=dv past deliberation window (stage mismatch)",
            ),
        ];

        for (ev, label) in drop_cases {
            poll.apply_event(&ev).unwrap_or_else(|e| {
                panic!("apply must return Ok (silent drop) for {label}: {e:?}")
            });
            assert_eq!(
                poll.last_hlc.as_ref().unwrap(),
                &baseline,
                "{label}: last_hlc must remain at baseline ({baseline:?})"
            );
        }

        // Sanity: 5 from author1 + 1 from author2; no votes accepted.
        assert_eq!(poll.deliberation.statements.len(), 6);
        assert!(poll.deliberation.votes.is_empty());
    }

    // ── ZEB-295 Task 7: kd=rb se-mode + kd=ts apply tests ─────────────────────
    //
    // These tests cover the 10 silent-drop/accept branches per spec §3.2 (B5)
    // and §3.3 (T1/T2). Helpers below construct a real 2-of-3 ElGamal committee
    // + a MockCommitteeOracle so we can exercise NIZK + DLEQ proof verification
    // end-to-end.
    mod ts_apply_helpers {
        use curve25519_dalek::{
            constants::RISTRETTO_BASEPOINT_POINT, ristretto::RistrettoPoint, scalar::Scalar,
        };
        use frost_ristretto255::rand_core::OsRng;
        use std::collections::BTreeMap;

        /// A 2-of-3 ElGamal committee built by sampling a random polynomial of
        /// degree 1: f(0) = x (joint secret), f(i+1) = x_i (per-member share).
        /// Joint Y = G·x; per-member Y_i = G·x_i.
        pub struct MockCommittee {
            #[allow(dead_code)]
            pub secret: Scalar,
            pub joint_y: RistrettoPoint,
            pub member_addrs: Vec<crate::owner_state_types::OwnerAddr>,
            /// FROST identifier (1-based) → secret share scalar.
            pub shares: BTreeMap<u16, Scalar>,
            /// FROST identifier (1-based) → verifying share point Y_i = G·x_i.
            pub verifying_shares: BTreeMap<u16, RistrettoPoint>,
        }

        pub fn fake_committee_2_of_3() -> MockCommittee {
            let coeffs: Vec<Scalar> = (0..2).map(|_| Scalar::random(&mut OsRng)).collect();
            let secret = coeffs[0];
            let joint_y = RISTRETTO_BASEPOINT_POINT * secret;
            let mut shares = BTreeMap::new();
            let mut verifying_shares = BTreeMap::new();
            for i in 1u16..=3u16 {
                let id = Scalar::from(i as u64);
                let mut acc = Scalar::ZERO;
                let mut id_pow = Scalar::ONE;
                for c in &coeffs {
                    acc += c * id_pow;
                    id_pow *= id;
                }
                shares.insert(i, acc);
                verifying_shares.insert(i, RISTRETTO_BASEPOINT_POINT * acc);
            }
            // Deterministic per-member OwnerAddrs (0x10, 0x11, 0x12).
            let member_addrs = vec![
                crate::owner_state_types::OwnerAddr([0x10; 16]),
                crate::owner_state_types::OwnerAddr([0x11; 16]),
                crate::owner_state_types::OwnerAddr([0x12; 16]),
            ];
            MockCommittee {
                secret,
                joint_y,
                member_addrs,
                shares,
                verifying_shares,
            }
        }

        /// Snapshot of a MockCommittee suitable for CommitteeOracle queries.
        /// Cloned once per oracle install — the oracle returns identical state
        /// for every epoch query.
        #[derive(Debug, Clone)]
        pub struct MockOracleState {
            pub joint_verifying_key: [u8; 32],
            pub verifying_shares: BTreeMap<crate::owner_state_types::OwnerAddr, [u8; 32]>,
            pub threshold: u16,
        }

        pub fn snapshot_of(c: &MockCommittee) -> MockOracleState {
            use crate::community_voting_tier3_crypto::compress_point;
            let joint_verifying_key = compress_point(&c.joint_y);
            let mut verifying_shares = BTreeMap::new();
            for (i, addr) in c.member_addrs.iter().enumerate() {
                let id = (i + 1) as u16;
                verifying_shares.insert(*addr, compress_point(&c.verifying_shares[&id]));
            }
            MockOracleState {
                joint_verifying_key,
                verifying_shares,
                threshold: 2,
            }
        }

        #[derive(Debug)]
        pub struct MockCommitteeOracle {
            pub state: MockOracleState,
            pub latest: u64,
        }

        impl super::super::CommitteeOracle for MockCommitteeOracle {
            fn committee_at_epoch(&self, epoch: u64) -> Option<super::super::CommitteePublicState> {
                Some(super::super::CommitteePublicState {
                    epoch,
                    joint_verifying_key: self.state.joint_verifying_key,
                    verifying_shares: self.state.verifying_shares.clone(),
                    threshold: self.state.threshold,
                })
            }
            fn latest_epoch(&self) -> Option<u64> {
                Some(self.latest)
            }
        }

        /// Construct a se-mode Tier3PollState in (effective) Ratification
        /// stage with a real 2-of-3 committee installed.
        ///
        /// `n_total` is the spec's `n` (score-aggregate count): explicit
        /// candidates = `n_total - 1` (status_quo accounts for the missing
        /// one). e.g. n_total=3 → 2 explicit kd=dc candidates are appended.
        ///
        /// The poll is created at wall_ms=0 with the default 10s/10s/10s
        /// window config. After this helper, `last_hlc` is past the
        /// drafting threshold (20s) — `current_stage_at(now=20000+)` returns
        /// `Ratification`.
        pub fn arrange_se_poll_in_ratification_stage_with_real_committee(
            n_total: usize,
        ) -> (super::super::Tier3PollState, MockCommittee) {
            assert!(n_total >= 1, "n must be >= 1 (status_quo counts)");
            let committee = fake_committee_2_of_3();
            // Use a se-mode meta; default windows are 10s/10s/10s.
            let mut meta = super::meta_at(0);
            meta.config.privacy_mode = "se".into();
            let electorate: Vec<crate::owner_state_types::OwnerAddr> =
                committee.member_addrs.to_vec();
            let mut state = super::super::Tier3PollState::new_from_create(meta, electorate);
            // Install oracle.
            let oracle_state = snapshot_of(&committee);
            state.install_committee_oracle(std::sync::Arc::new(MockCommitteeOracle {
                state: oracle_state,
                latest: 0,
            }));
            // Append `n_total - 1` synthetic candidates so the post-fix
            // ratification ordering (drafting_advancers → ratification_candidates_ordering)
            // has length n_total. Each candidate needs ≥ ceil(sortition_size/2)
            // approvals to pass the drafting threshold; the default config's
            // sortition_size=5 → threshold=3, exactly matching the 3 committee
            // members. We approve every synthetic candidate by every committee
            // member so all advance (deterministic test setup).
            let all_approvals: std::collections::HashSet<crate::owner_state_types::OwnerAddr> =
                committee.member_addrs.iter().copied().collect();
            for i in 0..(n_total - 1) {
                state
                    .candidates
                    .push(crate::community_voting_tier3::DraftCandidateState {
                        event_hash: [0xC0 | (i as u8); 32],
                        text: format!("candidate {i}"),
                        proposer: Some(committee.member_addrs[0]),
                        approvals: all_approvals.clone(),
                    });
            }
            // Apply a kd=ss at wall_ms=10 so the stage projection has a
            // sortition_result (current_stage_at requires it to leave
            // Sortition). last_hlc=10 after this.
            let ss_ev = super::ss_event(10, committee.member_addrs.clone(), Vec::new());
            state.apply_event(&ss_ev).expect("ss apply");
            (state, committee)
        }

        /// Build a real se-mode RatificationBallotPayload via the bundled
        /// NIZK prover. `scores.len()` must equal n.
        pub fn build_real_se_ballot_payload(
            committee: &MockCommittee,
            scores: &[u64],
        ) -> super::super::RatificationBallotPayload {
            use crate::community_voting_tier3_nizk::prove_ballot_bundle_with_outputs;
            let (bundle, cs, ci) = prove_ballot_bundle_with_outputs(&committee.joint_y, scores)
                .expect("test scores are well-formed");
            super::super::RatificationBallotPayload {
                poll_id: super::poll_id(),
                scores: None,
                ciphertexts_scores: Some(cs),
                ciphertexts_indicators: Some(ci),
                proof: Some(crate::community_voting_core::BallotNIZKProof {
                    range_proofs: bundle.range_proofs,
                    consistency_proofs: bundle.consistency_proofs,
                }),
            }
        }

        /// Compute a real kd=ts payload for member at index `member_idx` (0-based).
        /// Aggregates the accepted ballots from `state.ratification_ballots`,
        /// produces per-entry DLEQ proofs against the member's share x_i.
        ///
        /// `committee_epoch` is the epoch this share is being published for.
        pub fn build_real_tally_share_payload(
            state: &super::super::Tier3PollState,
            committee: &MockCommittee,
            member_idx: usize,
            committee_epoch: u64,
        ) -> crate::community_voting_core::TallySharePayload {
            use crate::community_voting_tier3_crypto::{
                compress_point, decompress_point, partial_decrypt_share,
            };
            use crate::community_voting_tier3_nizk::dleq_prove;
            // Derive n from the canonical ratification ordering, matching the
            // apply path's n derivation (Qodo PR #155 #2 fix). Using
            // `state.candidates.len() + 1` is only correct when every kd=dc
            // candidate passes threshold + cap, which the new
            // n-derivation regression tests deliberately violate.
            let sq = super::super::synthesize_status_quo(&state.meta.poll_id);
            let sq_hash = sq.event_hash;
            let mut all_candidates = state.candidates.clone();
            all_candidates.push(sq);
            let primary_size = state.meta.config.sortition_size as usize;
            let advancers =
                super::super::drafting_advancers(&all_candidates, primary_size, sq_hash)
                    .expect("ratification ordering reachable");
            let n = super::super::ratification_candidates_ordering(&advancers, sq_hash).len();
            let aggregates = super::super::aggregate_se_ballots(&state.ratification_ballots, n)
                .expect("aggregates");
            let frost_id = (member_idx + 1) as u16;
            let x_i = committee.shares[&frost_id];
            let y_i = committee.verifying_shares[&frost_id];
            let g = RISTRETTO_BASEPOINT_POINT;
            let entries: Vec<crate::community_voting_core::TallyShareEntry> = aggregates
                .iter()
                .map(|agg| {
                    let c1_agg = decompress_point(&agg.c1).expect("c1");
                    let share_pt = partial_decrypt_share(&c1_agg, &x_i);
                    let proof = dleq_prove(&g, &y_i, &c1_agg, &share_pt, &x_i);
                    crate::community_voting_core::TallyShareEntry {
                        share: compress_point(&share_pt),
                        dleq_proof: proof.to_bytes(),
                    }
                })
                .collect();
            crate::community_voting_core::TallySharePayload {
                poll_id: super::poll_id(),
                committee_epoch,
                entries,
            }
        }

        /// Encode a payload into a SignedVotingEvent at a custom wall_ms +
        /// actor (used by kd=ts tests where actor identity drives T1).
        pub fn rb_event_at_with_actor<T: serde::Serialize>(
            wall_ms: u64,
            actor: crate::owner_state_types::OwnerAddr,
            payload: &T,
        ) -> crate::community_voting_core::SignedVotingEvent {
            super::make_event_with_payload(
                super::super::PollEventKindCode::RatificationBallot,
                wall_ms,
                actor,
                super::encode(payload),
            )
        }

        pub fn ts_event_at_with_actor<T: serde::Serialize>(
            wall_ms: u64,
            actor: crate::owner_state_types::OwnerAddr,
            payload: &T,
        ) -> crate::community_voting_core::SignedVotingEvent {
            super::make_event_with_payload(
                super::super::PollEventKindCode::TallyShare,
                wall_ms,
                actor,
                super::encode(payload),
            )
        }
    }

    // The default config has dw=10/fw=10/rw=10 (seconds), so ratification
    // window opens at wall_ms=20_000 and closes at wall_ms=30_000.
    const RATIFICATION_OPEN_MS: u64 = 20_000;
    const RATIFICATION_END_MS: u64 = 30_000;

    // ── B5: encoding-matches-privacy-mode ────────────────────────────────────

    #[test]
    fn kd_rb_b5_pu_mode_payload_in_se_mode_poll_silent_drops() {
        // se-mode poll; sending pu-mode payload (scores Some, others None).
        let (mut state, _committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        let pu_payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(vec![5, 3, 1]),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        let prev_last_hlc = state.last_hlc.clone();
        let ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            state.eligible_electorate_snapshot[0],
            &pu_payload,
        );
        state.apply_event(&ev).expect("apply ok");
        assert_eq!(
            state.ratification_ballots.len(),
            0,
            "se-mode poll must reject pu-mode payload"
        );
        assert_eq!(
            state.last_hlc, prev_last_hlc,
            "drop must not advance last_hlc (ZEB-320)"
        );
        assert_eq!(
            state.max_received_hlc().map(|h| h.wall_ms),
            Some(RATIFICATION_OPEN_MS),
            "last_received_hlc must advance on every dispatch"
        );
    }

    #[test]
    fn kd_rb_b5_se_mode_payload_in_pu_mode_poll_silent_drops() {
        // pu-mode poll; sending se-mode payload.
        let mut state = Tier3PollState::new_from_create(meta_at(0), electorate(20));
        // Apply ss to leave Sortition.
        state
            .apply_event(&ss_event(10, vec![addr(1)], vec![]))
            .expect("ss apply");
        let n = state.candidates.len() + 1; // 1
        let _pair_count = n * (n - 1) / 2;
        // Construct a se-mode shape correctly for the n=1 case.
        let se_payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: None,
            ciphertexts_scores: Some(vec![
                crate::community_voting_core::EncCiphertext {
                    c1: [0u8; 32],
                    c2: [0u8; 32],
                };
                n
            ]),
            ciphertexts_indicators: Some(vec![]),
            proof: Some(crate::community_voting_core::BallotNIZKProof {
                range_proofs: vec![0u8; crate::community_voting_tier3_nizk::Range5Proof::SIZE * n],
                consistency_proofs: vec![],
            }),
        };
        let prev_last_hlc = state.last_hlc.clone();
        let ev =
            ts_apply_helpers::rb_event_at_with_actor(RATIFICATION_OPEN_MS, addr(1), &se_payload);
        state.apply_event(&ev).expect("apply ok");
        assert_eq!(state.ratification_ballots.len(), 0);
        assert_eq!(state.last_hlc, prev_last_hlc);
    }

    #[test]
    fn kd_rb_se_mode_wrong_ciphertext_shape_silent_drops() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        let mut payload = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        // Wrong shape: pop one indicator → ci.len() = pair_count - 1.
        payload.ciphertexts_indicators.as_mut().unwrap().pop();
        let prev_last_hlc = state.last_hlc.clone();
        let ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            state.eligible_electorate_snapshot[0],
            &payload,
        );
        state.apply_event(&ev).expect("apply ok");
        assert_eq!(state.ratification_ballots.len(), 0);
        assert_eq!(state.last_hlc, prev_last_hlc);
    }

    #[test]
    fn kd_rb_se_mode_invalid_nizk_silent_drops() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        let mut payload = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        // Bit-flip one byte of the range proof bundle.
        payload.proof.as_mut().unwrap().range_proofs[0] ^= 0x01;
        let prev_last_hlc = state.last_hlc.clone();
        let ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            state.eligible_electorate_snapshot[0],
            &payload,
        );
        state.apply_event(&ev).expect("apply ok");
        assert_eq!(state.ratification_ballots.len(), 0);
        assert_eq!(state.last_hlc, prev_last_hlc);
    }

    #[test]
    fn kd_rb_se_mode_valid_ballot_accepted() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        let payload = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            state.eligible_electorate_snapshot[0],
            &payload,
        );
        state.apply_event(&ev).expect("apply ok");
        assert_eq!(state.ratification_ballots.len(), 1, "valid ballot accepted");
        assert_eq!(
            state.last_hlc.as_ref().map(|h| h.wall_ms),
            Some(RATIFICATION_OPEN_MS),
            "accept must advance last_hlc"
        );
    }

    // Qodo PR #155 #2: regression for apply-time n derivation. Build a poll
    // where `self.candidates.len() + 1` (the old buggy n) differs from the
    // canonical ratification-ordering length. Asserts a se-mode ballot built
    // for the canonical n is accepted — under the buggy code it would have
    // been silently dropped by the b5 shape check.
    #[test]
    fn kd_rb_se_mode_n_derived_from_ratification_ordering_not_candidate_count() {
        // Helper provisions 2 synthetic candidates with full committee approvals
        // (post-fix helper). After the fix to the apply path, n = ordering length;
        // we extend `state.candidates` with sub-threshold candidates that bring
        // `self.candidates.len() + 1` to 6 while the canonical ordering remains 3
        // (2 passing + status_quo). The IPC path sizes ballots at the canonical
        // ordering length, so under the buggy `n = candidates.len() + 1` the
        // ballot would be silently dropped by the b5 shape gate.
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        // Push 3 extra candidates with ZERO approvals → they fail the
        // drafting threshold (ceil(sortition_size=5 / 2) = 3) and are
        // filtered out by `drafting_advancers`. After these pushes:
        //   - self.candidates.len() = 5
        //   - self.candidates.len() + 1 = 6  (old buggy n)
        //   - ratification ordering length = 2 (passing) + 1 (status_quo) = 3
        //     (the correct n).
        for i in 0..3u8 {
            state
                .candidates
                .push(crate::community_voting_tier3::DraftCandidateState {
                    event_hash: [0xD0 | i; 32],
                    text: format!("sub-threshold {i}"),
                    proposer: Some(committee.member_addrs[0]),
                    approvals: std::collections::HashSet::new(),
                });
        }
        assert_eq!(state.candidates.len(), 5, "test setup precondition");

        // Build a real se-mode ballot for n=3 (3 scores → 3 score-aggregates,
        // 3 indicator-pair aggregates).
        let payload = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            state.eligible_electorate_snapshot[0],
            &payload,
        );
        state.apply_event(&ev).expect("apply ok");
        assert_eq!(
            state.ratification_ballots.len(),
            1,
            "se-mode ballot sized for ratification ordering n=3 must be accepted; \
             under the buggy n = candidates.len()+1 = 6, the b5 shape check would have dropped it",
        );
        assert_eq!(
            state.last_hlc.as_ref().map(|h| h.wall_ms),
            Some(RATIFICATION_OPEN_MS),
            "accept must advance last_hlc",
        );
    }

    // Qodo PR #155 #2: same bug surface for the kd=ts shape check.
    // A kd=ts payload sized to the canonical ratification ordering n must be
    // accepted; the buggy path used `self.candidates.len() + 1` and would
    // reject any payload sized for a smaller n.
    #[test]
    fn kd_ts_n_derived_from_ratification_ordering_not_candidate_count() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        // Same trick as above: extend `state.candidates` with sub-threshold
        // entries so `self.candidates.len() + 1 = 6 ≠ 3 = ordering length`.
        for i in 0..3u8 {
            state
                .candidates
                .push(crate::community_voting_tier3::DraftCandidateState {
                    event_hash: [0xD0 | i; 32],
                    text: format!("sub-threshold {i}"),
                    proposer: Some(committee.member_addrs[0]),
                    approvals: std::collections::HashSet::new(),
                });
        }
        // Submit a valid ballot first so `aggregate_se_ballots` has input;
        // built for n=3 (the correct ordering length).
        let ballot = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let rb_ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            committee.member_addrs[0],
            &ballot,
        );
        state.apply_event(&rb_ev).expect("rb apply");
        // Build a kd=ts payload for n=3 (per the helper, which reads
        // `aggregate_se_ballots(..., n=3)` internally — see helper for
        // n derivation).
        let payload = ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        let ev = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS,
            committee.member_addrs[0],
            &payload,
        );
        state.apply_event(&ev).expect("apply ok");
        assert_eq!(
            state.secret_tally.tally_shares.len(),
            1,
            "kd=ts payload sized for ratification ordering n=3 must be accepted; \
             under the buggy n = candidates.len()+1 = 6 the shape check would have dropped it",
        );
    }

    // ── kd=ts: T1 / T2 / timing / mode ───────────────────────────────────────

    #[test]
    fn kd_ts_t1_non_committee_actor_silent_drops() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        // Submit at least one valid ballot so aggregates exist for the kd=ts
        // shape check to reach T1 (otherwise an empty-ballots path would
        // confound the test). The aggregates are not needed for the T1 check
        // itself — T1 only consults the oracle's verifying_shares map.
        let ballot = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let rb_ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            committee.member_addrs[0],
            &ballot,
        );
        state.apply_event(&rb_ev).expect("rb ok");
        // Actor 0xFF is not in the committee.
        let bogus_actor = OwnerAddr([0xFF; 16]);
        let n = state.candidates.len() + 1;
        let payload = crate::community_voting_core::TallySharePayload {
            poll_id: poll_id(),
            committee_epoch: 0,
            entries: vec![
                crate::community_voting_core::TallyShareEntry {
                    share: [0; 32],
                    dleq_proof: [0; 64],
                };
                n + n * (n - 1) / 2
            ],
        };
        let prev_last_hlc = state.last_hlc.clone();
        let ev =
            ts_apply_helpers::ts_event_at_with_actor(RATIFICATION_END_MS, bogus_actor, &payload);
        state.apply_event(&ev).expect("apply ok");
        assert!(state.secret_tally.tally_shares.is_empty());
        assert_eq!(state.last_hlc, prev_last_hlc);
    }

    #[test]
    fn kd_ts_t2_invalid_dleq_silent_drops() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        let ballot = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let rb_ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            committee.member_addrs[0],
            &ballot,
        );
        state.apply_event(&rb_ev).expect("rb ok");
        let mut payload =
            ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        // Bit-flip one byte of the first entry's DLEQ proof.
        payload.entries[0].dleq_proof[0] ^= 0x01;
        let prev_last_hlc = state.last_hlc.clone();
        let ev = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS,
            committee.member_addrs[0],
            &payload,
        );
        state.apply_event(&ev).expect("apply ok");
        assert!(state.secret_tally.tally_shares.is_empty());
        assert_eq!(state.last_hlc, prev_last_hlc);
    }

    #[test]
    fn kd_ts_too_early_silent_drops() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        let ballot = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let rb_ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            committee.member_addrs[0],
            &ballot,
        );
        state.apply_event(&rb_ev).expect("rb ok");
        let payload = ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        // wall_ms < ratification_end_ms (which is 30_000).
        let prev_last_hlc = state.last_hlc.clone();
        let ev = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS - 1,
            committee.member_addrs[0],
            &payload,
        );
        state.apply_event(&ev).expect("apply ok");
        assert!(state.secret_tally.tally_shares.is_empty());
        assert_eq!(state.last_hlc, prev_last_hlc);
    }

    #[test]
    fn kd_ts_in_pu_mode_poll_silent_drops() {
        let mut state = Tier3PollState::new_from_create(meta_at(0), electorate(20));
        state
            .apply_event(&ss_event(10, vec![addr(1)], vec![]))
            .expect("ss apply");
        let payload = crate::community_voting_core::TallySharePayload {
            poll_id: poll_id(),
            committee_epoch: 0,
            entries: vec![],
        };
        let prev_last_hlc = state.last_hlc.clone();
        let ev = ts_apply_helpers::ts_event_at_with_actor(RATIFICATION_END_MS, addr(1), &payload);
        state.apply_event(&ev).expect("apply ok");
        assert!(state.secret_tally.tally_shares.is_empty());
        assert_eq!(state.last_hlc, prev_last_hlc);
    }

    #[test]
    fn kd_ts_valid_share_accepted() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        let ballot = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let rb_ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            committee.member_addrs[0],
            &ballot,
        );
        state.apply_event(&rb_ev).expect("rb ok");
        let payload = ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        let ev = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS,
            committee.member_addrs[0],
            &payload,
        );
        state.apply_event(&ev).expect("apply ok");
        assert_eq!(
            state.secret_tally.tally_shares.len(),
            1,
            "valid (T1+T2+timing+shape) share accepted"
        );
    }

    // Cursor PR #155 #3 + Qodo PR #155 #4: kd=ts is LWW per
    // (actor, committee_epoch), keyed on (hlc.wall_ms, hlc.logical,
    // hlc.device_id, event_hash) lex order. Two follow-on tests cover the
    // "later wins" and "byte-identical replay is no-op" cases; a third
    // covers the same-HLC-different-event-hash lex tiebreak.
    //
    // The earlier-HLC-second case (an old share arriving after a newer one)
    // is intercepted by the apply_event monotonic-HLC guard *in single-
    // replica flow*, so the LWW lex comparison is exercised through
    // `incoming == existing` (replay) and same-HLC-different-hash branches
    // here. Cross-replica convergence is exercised by integration tests
    // that drive events through `apply_event` in canonical (hlc, hash) order.
    #[test]
    fn kd_ts_lww_later_hlc_with_different_entries_overwrites() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        // Two ballots, then re-prove two share payloads against the same
        // current aggregate. Each `build_real_tally_share_payload` call
        // freshly samples DLEQ randomness, so entries differ between calls
        // even though the underlying aggregate is identical. The test goal
        // is to confirm that a later-HLC kd=ts overwrites an earlier-HLC one
        // for the same (actor, epoch), regardless of payload differences.
        let ballot_a = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let rb_a = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            committee.member_addrs[0],
            &ballot_a,
        );
        state.apply_event(&rb_a).expect("rb apply A");
        let ballot_b = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[1, 2, 3]);
        let rb_b = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS + 1,
            committee.member_addrs[1],
            &ballot_b,
        );
        state.apply_event(&rb_b).expect("rb apply B");

        let payload_first =
            ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        let payload_second =
            ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        // Fresh DLEQ randomness → different entries; both are valid against
        // the same aggregate (verified by the apply path's T2 DLEQ check).
        assert_ne!(
            payload_first.entries, payload_second.entries,
            "test precondition: two re-proves at the same aggregate differ",
        );

        let ev_first = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS,
            committee.member_addrs[0],
            &payload_first,
        );
        state.apply_event(&ev_first).expect("apply first");
        let after_first = state
            .secret_tally
            .tally_shares
            .get(&(committee.member_addrs[0], 0))
            .expect("share first applied")
            .clone();

        let ev_second = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS + 1,
            committee.member_addrs[0],
            &payload_second,
        );
        state.apply_event(&ev_second).expect("apply second");
        let after_second = state
            .secret_tally
            .tally_shares
            .get(&(committee.member_addrs[0], 0))
            .expect("share second applied")
            .clone();

        assert_eq!(
            state.secret_tally.tally_shares.len(),
            1,
            "(actor, epoch) LWW: still a single record",
        );
        assert_eq!(
            after_second.entries, payload_second.entries,
            "later-HLC submission with different entries must overwrite (LWW)",
        );
        assert_ne!(
            after_second.entries, after_first.entries,
            "record was overwritten, not retained",
        );
        assert!(
            after_second.last_update_hlc.wall_ms > after_first.last_update_hlc.wall_ms,
            "record carries the newer event's HLC",
        );
    }

    #[test]
    fn kd_ts_lww_byte_identical_replay_is_noop() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        let ballot = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let rb_ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            committee.member_addrs[0],
            &ballot,
        );
        state.apply_event(&rb_ev).expect("rb apply");
        let payload = ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        let ev = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS,
            committee.member_addrs[0],
            &payload,
        );
        state.apply_event(&ev).expect("apply 1");
        let record_after_first = state
            .secret_tally
            .tally_shares
            .get(&(committee.member_addrs[0], 0))
            .expect("share applied")
            .clone();
        let last_hlc_after_first = state.last_hlc.clone();

        // True byte-identical replay: same event passed through apply_event
        // again. The monotonic-HLC guard uses strict `<`, so equal HLCs
        // pass; the LWW comparison then reports `incoming == existing`
        // (not strictly greater) → silent stale-drop.
        state.apply_event(&ev).expect("apply 2");
        assert_eq!(
            state.secret_tally.tally_shares.len(),
            1,
            "LWW: still a single record",
        );
        let record_after_second = state
            .secret_tally
            .tally_shares
            .get(&(committee.member_addrs[0], 0))
            .expect("share still present")
            .clone();
        assert_eq!(
            record_after_first, record_after_second,
            "byte-identical replay must not change the record (LWW idempotent)",
        );
        assert_eq!(
            state.last_hlc, last_hlc_after_first,
            "stale LWW drop must not advance last_hlc watermark (ZEB-320)",
        );
    }

    #[test]
    fn kd_ts_lww_same_hlc_event_hash_tiebreak_is_deterministic() {
        // Same HLC, two events with different payloads → different event_hash.
        // The monotonic guard uses strict `<` so equal HLCs pass; the LWW
        // comparison falls through to the event_hash tiebreak. We construct
        // both events, sort them by event_hash to know which is "later",
        // apply the "earlier" one first, then the "later" one — assert the
        // later one wins.
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        let ballot = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let rb_ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            committee.member_addrs[0],
            &ballot,
        );
        state.apply_event(&rb_ev).expect("rb apply");

        // Two payloads with different DLEQ-proof randomness — same aggregate,
        // but the proofs use fresh `OsRng` so entries differ between calls.
        let payload_a = ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        let payload_b = ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        assert_ne!(
            payload_a.entries, payload_b.entries,
            "test precondition: two distinct re-proves at the same aggregate",
        );

        let ev_a = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS,
            committee.member_addrs[0],
            &payload_a,
        );
        let ev_b = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS,
            committee.member_addrs[0],
            &payload_b,
        );
        let hash_a = event_hash_of(&ev_a);
        let hash_b = event_hash_of(&ev_b);
        // Order events so we apply the lower-hash one first, then the
        // higher-hash one; LWW lex order says the higher hash wins.
        let (lower, higher, higher_entries) = if hash_a < hash_b {
            (&ev_a, &ev_b, &payload_b.entries)
        } else {
            (&ev_b, &ev_a, &payload_a.entries)
        };
        state.apply_event(lower).expect("apply lower");
        let last_hlc_after_lower = state.last_hlc.clone();
        state.apply_event(higher).expect("apply higher");

        assert_eq!(
            state.secret_tally.tally_shares.len(),
            1,
            "still a single record",
        );
        let record = &state.secret_tally.tally_shares[&(committee.member_addrs[0], 0)];
        assert_eq!(
            &record.entries, higher_entries,
            "higher event_hash must win the same-HLC lex tiebreak",
        );
        // Both events accepted → watermark advances on each (equal HLC,
        // but `advance_last_hlc=true` for the second). last_hlc stays at
        // ev's HLC.
        assert_eq!(
            state.last_hlc.as_ref().map(|h| h.wall_ms),
            Some(RATIFICATION_END_MS),
        );
        // Sanity: last_hlc_after_lower was already RATIFICATION_END_MS.
        assert_eq!(
            last_hlc_after_lower.as_ref().map(|h| h.wall_ms),
            Some(RATIFICATION_END_MS),
        );
    }

    // ── ZEB-295 Phase 6 Task 8: recover_secret_tally end-to-end ──────────

    #[test]
    fn recover_secret_tally_no_shares_returns_none() {
        let (state, _committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        let ordered = vec![
            crate::community_voting_star::CandidateRef {
                event_hash: [0xC0; 32],
                approval_count: 0,
            },
            crate::community_voting_star::CandidateRef {
                event_hash: [0xC1; 32],
                approval_count: 0,
            },
            crate::community_voting_star::CandidateRef {
                event_hash: [0xC2; 32],
                approval_count: 0,
            },
        ];
        assert!(super::recover_secret_tally(&state, &ordered).is_none());
    }

    #[test]
    fn recover_secret_tally_below_threshold_returns_none() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        // One ballot.
        let ballot = ts_apply_helpers::build_real_se_ballot_payload(&committee, &[5, 3, 1]);
        let rb_ev = ts_apply_helpers::rb_event_at_with_actor(
            RATIFICATION_OPEN_MS,
            committee.member_addrs[0],
            &ballot,
        );
        state.apply_event(&rb_ev).expect("rb ok");
        // One share (committee threshold = 2).
        let payload = ts_apply_helpers::build_real_tally_share_payload(&state, &committee, 0, 0);
        let ev = ts_apply_helpers::ts_event_at_with_actor(
            RATIFICATION_END_MS,
            committee.member_addrs[0],
            &payload,
        );
        state.apply_event(&ev).expect("ts apply");
        let ordered = vec![
            crate::community_voting_star::CandidateRef {
                event_hash: [0xC0; 32],
                approval_count: 0,
            },
            crate::community_voting_star::CandidateRef {
                event_hash: [0xC1; 32],
                approval_count: 0,
            },
            crate::community_voting_star::CandidateRef {
                event_hash: [0xC2; 32],
                approval_count: 0,
            },
        ];
        assert!(
            super::recover_secret_tally(&state, &ordered).is_none(),
            "1 share with threshold=2 must not recover"
        );
    }

    #[test]
    fn recover_secret_tally_threshold_shares_recover_plaintext() {
        let (mut state, committee) =
            ts_apply_helpers::arrange_se_poll_in_ratification_stage_with_real_committee(3);
        // Cast 3 ballots so the runoff has clear winner-decisive indicators.
        // n=3 (2 explicit candidates + status_quo); pair_count = 3.
        // Scores indexed by candidate position [c0, c1, c2 = status_quo].
        // Ballots:
        //   voter0 [5, 1, 0]: c0 dominates
        //   voter1 [5, 2, 0]: c0 dominates
        //   voter2 [5, 3, 0]: c0 dominates
        // score_sums = [15, 6, 0]
        // c0 wins with absolute dominance. No per-ballot ties.
        let ballots = [[5u64, 1, 0], [5, 2, 0], [5, 3, 0]];
        for (i, scores) in ballots.iter().enumerate() {
            let payload = ts_apply_helpers::build_real_se_ballot_payload(&committee, scores);
            let ev = ts_apply_helpers::rb_event_at_with_actor(
                RATIFICATION_OPEN_MS + i as u64,
                committee.member_addrs[i],
                &payload,
            );
            state.apply_event(&ev).expect("rb apply");
        }
        // Two valid shares (threshold = 2-of-3).
        for member_idx in [0, 1] {
            let ts_payload =
                ts_apply_helpers::build_real_tally_share_payload(&state, &committee, member_idx, 0);
            let ev = ts_apply_helpers::ts_event_at_with_actor(
                RATIFICATION_END_MS + member_idx as u64,
                committee.member_addrs[member_idx],
                &ts_payload,
            );
            state.apply_event(&ev).expect("ts apply");
        }
        assert_eq!(state.secret_tally.tally_shares.len(), 2);

        // Build ordered candidates matching state.candidates order. The
        // helper appends synthetic candidates with event_hashes
        // 0xC0..=0xC1 then status_quo would be added by ratification
        // path; here we use the same hashes as build_real_*.
        let ordered: Vec<crate::community_voting_star::CandidateRef> = state
            .candidates
            .iter()
            .map(|c| crate::community_voting_star::CandidateRef {
                event_hash: c.event_hash,
                approval_count: 0,
            })
            .chain(std::iter::once(
                crate::community_voting_star::CandidateRef {
                    event_hash: synthesize_status_quo(&state.meta.poll_id).event_hash,
                    approval_count: 0,
                },
            ))
            .collect();
        let result = super::recover_secret_tally(&state, &ordered).expect("must recover");

        // c0 score_sum = 15, c1 score_sum = 6, c2 score_sum = 0.
        assert_eq!(result.total_scores, vec![15, 6, 0]);
        // Finalists: c0 (15) and c1 (6) — c2 (0) excluded.
        assert_eq!(result.finalists.len(), 2);
        assert_eq!(result.winner.event_hash, ordered[0].event_hash);
    }

    // ── ZEB-860 Task 1: ApplyOutcome / max_applied / canonical_key ────────────

    #[test]
    fn apply_event_reports_applied_vs_dropped() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage(); // sets mini-public
                                                           // An accepted statement from a mini-public author (well inside the
                                                           // deliberation window [0, 10_000)):
        let s = ds_event_with_text(5_000, addr(1), "hello world statement");
        assert_eq!(poll.apply_event(&s), Ok(ApplyOutcome::Applied));
        // A dv from a mini-public member referencing a NON-existent statement is
        // silently dropped (still inside the window so the drop reason is the
        // missing statement, not an out-of-stage filter):
        let missing = [0u8; 32];
        let v = dv_event(6_000, addr(2), missing, BridgingVoteCode::Agree);
        assert_eq!(poll.apply_event(&v), Ok(ApplyOutcome::Dropped));
    }

    #[test]
    fn max_applied_advances_on_accept_and_drop() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();
        let s = ds_event_with_text(5_000, addr(1), "a statement here");
        let _ = poll.apply_event(&s).unwrap();
        let after_accept = poll.max_applied.clone();
        assert!(after_accept.is_some());
        // A dropped event with a HIGHER key still advances max_applied:
        let dropped = dv_event(20_000, addr(2), [0u8; 32], BridgingVoteCode::Agree); // dropped (no statement)
        assert_eq!(poll.apply_event(&dropped), Ok(ApplyOutcome::Dropped));
        assert!(
            poll.max_applied > after_accept,
            "drop path must still advance max_applied"
        );
    }

    #[test]
    fn canonical_key_orders_by_hlc_then_hash() {
        let a = ds_event_with_text(10_000, addr(1), "first statement text");
        let b = ds_event_with_text(20_000, addr(1), "second statement text");
        assert!(
            canonical_key(&a) < canonical_key(&b),
            "earlier wall_ms sorts first"
        );
        // Same (wall,logical,device), different payload → event_hash breaks the
        // tie deterministically.
        let c = ds_event_with_text(10_000, addr(1), "zzz different text same time");
        let (ka, kc) = (canonical_key(&a), canonical_key(&c));
        assert_eq!((ka.0, ka.1, ka.2.clone()), (kc.0, kc.1, kc.2.clone()));
        assert_ne!(ka.3, kc.3, "distinct events get distinct hash tiebreakers");
    }

    // ── ZEB-860 Task 2: rebuild_from_events (canonical mini-restore) ───────────

    #[test]
    fn rebuild_rematerializes_dropped_vote() {
        use crate::community_voting_core::BridgingVoteCode;

        // addr(1), addr(2) are in the mini-public; window is [0, 10_000).
        let mut poll = build_poll_in_deliberation_stage();
        // The sortition event that lifts the poll into Deliberation is part of the
        // poll's own event set; rebuild resets `sortition_result` via
        // `new_from_create`, so it must be in the replay slice (matching the
        // production per-poll `events` Vec) or every ds/dv drops as out-of-stage.
        let ss = ss_event(
            50,
            vec![addr(1), addr(2), addr(3)],
            vec![addr(10), addr(11)],
        );
        let s = ds_event_with_text(5_000, addr(1), "the statement being voted on");
        let s_hash = sha256_of_signing_bytes(&s);
        let v = dv_event(6_000, addr(2), s_hash, BridgingVoteCode::Agree); // vote by addr(2) on s
                                                                           // Deliver the VOTE first (out of order): it drops because the statement is absent.
        assert_eq!(poll.apply_event(&v), Ok(ApplyOutcome::Dropped));
        assert_eq!(poll.apply_event(&s), Ok(ApplyOutcome::Applied));
        assert!(
            !poll.deliberation.votes.contains_key(&(addr(2), s_hash)),
            "arrival-order fold drops the early vote"
        );
        // Rebuild from the same set → canonical order (ss@50 < s@5000 < v@6000)
        // re-materializes the vote.
        poll.rebuild_from_events(&[v.clone(), s.clone(), ss.clone()]);
        assert!(
            poll.deliberation.votes.contains_key(&(addr(2), s_hash)),
            "canonical rebuild applies the vote (ds precedes dv by HLC)"
        );
        assert_eq!(poll.rebuild_count, 1);
    }

    #[test]
    fn rebuild_preserves_non_replay_state() {
        let mut poll = build_poll_in_deliberation_stage();
        let epoch_before = poll.meta.community_epoch;
        let electorate_before = poll.eligible_electorate_snapshot.clone();
        // Install a non-null oracle. MockCommitteeOracle answers (latest_epoch =>
        // Some) where the constructor's default NullCommitteeOracle returns None.
        let oracle_state =
            ts_apply_helpers::snapshot_of(&ts_apply_helpers::fake_committee_2_of_3());
        poll.install_committee_oracle(std::sync::Arc::new(ts_apply_helpers::MockCommitteeOracle {
            state: oracle_state,
            latest: 0,
        }));
        let s = ds_event_with_text(5_000, addr(1), "some statement text here");
        poll.rebuild_from_events(&[s]);
        assert_eq!(
            poll.meta.community_epoch, epoch_before,
            "community_epoch preserved"
        );
        assert_eq!(
            poll.eligible_electorate_snapshot, electorate_before,
            "electorate preserved"
        );
        // Oracle preserved: a MockCommitteeOracle answers where NullCommitteeOracle
        // returns None. `latest_epoch().is_some()` proves it is NOT the default
        // NullCommitteeOracle that `new_from_create` would otherwise install.
        assert!(
            poll.committee_oracle.latest_epoch().is_some(),
            "installed committee_oracle survived the rebuild reset"
        );
    }

    #[test]
    fn rebuild_is_monotone_additive_for_in_order_set() {
        use crate::community_voting_core::BridgingVoteCode;

        let mut poll = build_poll_in_deliberation_stage();
        // ss (sortition) event is part of the poll's replay set — see the note in
        // `rebuild_rematerializes_dropped_vote`.
        let ss = ss_event(
            50,
            vec![addr(1), addr(2), addr(3)],
            vec![addr(10), addr(11)],
        );
        let s = ds_event_with_text(5_000, addr(1), "statement one text");
        let s_hash = sha256_of_signing_bytes(&s);
        let v = dv_event(6_000, addr(2), s_hash, BridgingVoteCode::Agree);
        poll.apply_event(&s).unwrap();
        poll.apply_event(&v).unwrap();
        let votes_before = poll.deliberation.votes.clone();
        let statements_before = poll.deliberation.statements.clone();
        poll.rebuild_from_events(&[ss, s, v]);
        assert_eq!(
            poll.deliberation.votes, votes_before,
            "in-order set unchanged by rebuild"
        );
        assert_eq!(poll.deliberation.statements, statements_before);
    }

    // ── ZEB-860 spec item 7: 5-cap determinism under canonical rebuild ─────────
    //
    // The kd=ds apply arm caps statements at 5 per AUTHOR
    // (`statements_per_author >= 5 → drop`). WHICH five survive depends on
    // delivery order: under arrival order the first-5-DELIVERED win; under
    // canonical order — exactly what `rebuild_from_events` produces — the 5
    // HLC-EARLIEST win. When delivery is out of order these two sets DIFFER, so
    // a previously-Applied statement can be retroactively EVICTED by the cap
    // recomputation after a canonical rebuild (and a previously-dropped,
    // HLC-earlier statement re-materialized). CodeRabbit (PR #590) flagged this
    // subtlety plus the "5-cap determinism" test that testing-strategy item 7
    // committed to but never landed. This pins BOTH eviction directions and the
    // "two delivery orders converge" guarantee.
    //
    // Each of addr(1)'s 6 statements sits on its OWN device lane (dev-0..dev-5)
    // so the per-(actor, device) monotonic guard (ZEB-850) does not reject the
    // deliberately-descending arrival order — the same actor across six devices
    // still shares ONE per-author 5-cap.
    #[test]
    fn rebuild_five_cap_keeps_hlc_earliest_deterministically() {
        // ss seats addr(1) in the mini-public and lifts the poll into
        // Deliberation; rebuild resets sortition_result via new_from_create, so
        // the replay slice MUST include ss (see rebuild_rematerializes_dropped_vote).
        let ss = ss_event(
            50,
            vec![addr(1), addr(2), addr(3)],
            vec![addr(10), addr(11)],
        );

        // 6 statements from addr(1) at distinct ascending walls (5000..=5005),
        // each on its own device lane. Capture each event_hash AFTER stamping
        // device_id — the id is part of the signing bytes.
        let mut stmts: Vec<SignedVotingEvent> = Vec::new();
        let mut hashes: Vec<[u8; 32]> = Vec::new();
        for i in 0u64..6 {
            let mut ev = ds_event_with_text(5_000 + i, addr(1), &format!("cap statement {i}"));
            ev.hlc.device_id = format!("dev-{i}");
            hashes.push(sha256_of_signing_bytes(&ev));
            stmts.push(ev);
        }

        // ── Arrival order: deliver DESCENDING [s5,s4,s3,s2,s1,s0]. Distinct
        // lanes ⇒ no monotonic rejection; the per-author cap keeps the first 5
        // DELIVERED {s5,s4,s3,s2,s1} and drops s0 — the HLC-EARLIEST, i.e. the
        // WRONG five relative to canonical order.
        let mut poll = build_poll_in_deliberation_stage();
        for ev in stmts.iter().rev() {
            poll.apply_event(ev)
                .expect("apply is Ok (cap-drop is a silent drop, not an error)");
        }
        assert_eq!(
            poll.deliberation.statements_per_author[&addr(1)],
            5,
            "arrival-order cap holds at 5"
        );
        assert!(
            poll.deliberation.statements.contains_key(&hashes[5]),
            "arrival order keeps s5 (HLC-latest, delivered first)"
        );
        assert!(
            !poll.deliberation.statements.contains_key(&hashes[0]),
            "arrival order drops s0 (HLC-earliest, delivered last) — the wrong five"
        );

        // ── Canonical rebuild: re-fold the SAME set (ss + all 6, scrambled —
        // rebuild sorts canonically regardless). Canonical order re-derives the
        // cap survivors as the 5 HLC-EARLIEST {s0..s4} and EVICTS s5. Assert
        // BOTH directions so the test fails if the rebuild did not re-canonicalize.
        let slice = vec![
            ss.clone(),
            stmts[3].clone(),
            stmts[0].clone(),
            stmts[5].clone(),
            stmts[2].clone(),
            stmts[4].clone(),
            stmts[1].clone(),
        ];
        poll.rebuild_from_events(&slice);

        assert!(
            poll.deliberation.statements.contains_key(&hashes[0]),
            "rebuild RE-MATERIALIZES s0 (HLC-earliest) that arrival order had dropped"
        );
        assert!(
            !poll.deliberation.statements.contains_key(&hashes[5]),
            "rebuild EVICTS s5 (HLC-latest) that arrival order had kept — retroactive eviction"
        );
        assert_eq!(
            poll.deliberation.statements_per_author[&addr(1)],
            5,
            "cap still holds at 5 after rebuild"
        );
        let survivors: std::collections::BTreeSet<[u8; 32]> =
            poll.deliberation.statements.keys().copied().collect();
        let expected: std::collections::BTreeSet<[u8; 32]> = hashes[0..5].iter().copied().collect();
        assert_eq!(
            survivors, expected,
            "canonical survivor set is exactly the 5 HLC-earliest {{s0..s4}}"
        );

        // ── Two delivery orders converge (spec item 7): fold a DIFFERENT
        // arrival order (ascending [s0..s5]) into a fresh poll, rebuild, and
        // assert the canonical survivor set is IDENTICAL — the rebuild's result
        // is delivery-order-independent.
        let mut other = build_poll_in_deliberation_stage();
        for ev in stmts.iter() {
            other
                .apply_event(ev)
                .expect("apply is Ok (cap-drop is a silent drop, not an error)");
        }
        let mut other_slice = vec![ss.clone()];
        other_slice.extend(stmts.iter().cloned());
        other.rebuild_from_events(&other_slice);
        let survivors_other: std::collections::BTreeSet<[u8; 32]> =
            other.deliberation.statements.keys().copied().collect();
        assert_eq!(
            survivors_other, survivors,
            "two delivery orders converge to the identical canonical survivor set"
        );
    }

    // ── ZEB-860 Task 4: order-invariance sweep for the non-trigger kinds ───────
    //
    // The canonical rebuild trigger (community_voting_log::apply_with_snapshot)
    // fires only for the order-DEPENDENT Deliberation family {ss, md, ds, dv}.
    // The drafting-family kinds kd=dc (append a candidate) and kd=da (idempotent
    // set-insert into a candidate's approvals) are order-INDEPENDENT: their
    // projection is identical regardless of delivery order (every downstream
    // consumer — `drafting_advancers` — re-sorts, so the raw append order of
    // `candidates` never reaches a result). Caveat for kd=da: this holds because
    // the ingest gate (`verify_da_candidate_exists`, run on the local-publish,
    // inbound, and backfill paths) guarantees a da's referenced dc is applied
    // (hence appended) BEFORE the da — so a da-before-dc is rejected-before-append
    // and never sits stranded in the log. This test keeps da's candidate present
    // in both orders to exercise that in-gate reality. This guards that contract: fold a
    // representative dc/dc/da set in two delivery orders — one delivering the
    // lower-HLC dc LAST, i.e. genuinely out of order — and assert (a) the
    // semantic candidate/approval projection is identical, and (b) neither poll
    // self-triggered a rebuild (`rebuild_count` stays 0; `apply_event` is the
    // pure materialize layer and never routes through `rebuild_from_events`, so
    // this also pins that these kinds can never enter the rebuild path).
    //
    // kd=rb (append to `ratification_ballots`) and kd=ts (LWW-keyed, exactly the
    // class as kd=dv within its own key) are order-independent by those SAME
    // append / last-writer-wins classes; reaching them needs a Ratification-stage
    // poll plus committee/NIZK crypto setup, so they are covered by class here
    // rather than duplicating that heavy fixture — they too are absent from the
    // {ss, md, ds, dv} trigger set.
    #[test]
    fn order_independent_kinds_converge_without_rebuild() {
        // Semantic projection of the drafting family: candidate identity
        // (event_hash) → its approver-address set. Delivery-order-independent by
        // construction (BTree containers), unlike the arrival-ordered
        // `candidates` Vec.
        fn candidate_projection(
            poll: &Tier3PollState,
        ) -> std::collections::BTreeMap<[u8; 32], std::collections::BTreeSet<[u8; 16]>> {
            poll.candidates
                .iter()
                .map(|c| (c.event_hash, c.approvals.iter().map(|a| a.0).collect()))
                .collect()
        }

        // Two DraftCandidates by distinct proposers + one DraftApproval by a
        // third actor on the FIRST candidate. HLCs: dc_a@200 < dc_b@300 < da@400.
        let dc_a = dc_event(200, addr(1), "alpha proposal");
        let dc_b = dc_event(300, addr(2), "beta proposal");
        let dc_a_hash = sha256_of_signing_bytes(&dc_a);
        let da = da_event(400, addr(3), dc_a_hash);

        // Order 1 (in HLC order): dc_a, dc_b, da.
        let mut poll_in_order = new_poll(0);
        for ev in [&dc_a, &dc_b, &da] {
            poll_in_order.apply_event(ev).expect("apply (in order)");
        }

        // Order 2 (out of order: the lower-HLC dc_a delivered AFTER dc_b). da is
        // still delivered after its candidate dc_a in BOTH orders, so it Applies
        // per the ingest rule "approval requires its candidate present". Each
        // event is on a distinct (actor, device) lane, so the per-lane monotonic
        // guard never rejects the reorder.
        let mut poll_reordered = new_poll(0);
        for ev in [&dc_b, &dc_a, &da] {
            poll_reordered.apply_event(ev).expect("apply (reordered)");
        }

        // (a) The semantic projection converges despite the reorder.
        assert_eq!(
            candidate_projection(&poll_in_order),
            candidate_projection(&poll_reordered),
            "dc/da projection must be identical regardless of delivery order"
        );
        // Concretely: candidate A carries {addr(1) self-approval, addr(3) da};
        // candidate B carries {addr(2) self-approval}.
        let proj = candidate_projection(&poll_in_order);
        let expected_a: std::collections::BTreeSet<[u8; 16]> =
            [addr(1).0, addr(3).0].into_iter().collect();
        assert_eq!(
            proj.get(&dc_a_hash),
            Some(&expected_a),
            "candidate A approved by its proposer and the da actor"
        );

        // (b) Neither poll self-rebuilt: dc/da are absent from the
        // {ss, md, ds, dv} trigger set, so folding them — even out of order —
        // must never trigger a canonical rebuild.
        assert_eq!(
            poll_in_order.rebuild_count, 0,
            "in-order dc/da fold must not rebuild"
        );
        assert_eq!(
            poll_reordered.rebuild_count, 0,
            "out-of-order dc/da delivery must NOT trigger a canonical rebuild"
        );
    }
}

#[cfg(test)]
mod validate_decline_reason_tests {
    use super::*;

    #[test]
    fn none_accepted() {
        assert!(validate_decline_reason(&None).is_ok());
    }

    #[test]
    fn two_char_alphanumeric_accepted() {
        assert!(validate_decline_reason(&Some("u1".into())).is_ok());
        assert!(validate_decline_reason(&Some("co".into())).is_ok());
    }

    #[test]
    fn empty_rejected() {
        assert!(matches!(
            validate_decline_reason(&Some(String::new())),
            Err(ValidateError::BadDeclineReason)
        ));
    }

    #[test]
    fn three_chars_rejected() {
        assert!(matches!(
            validate_decline_reason(&Some("abc".into())),
            Err(ValidateError::BadDeclineReason)
        ));
    }

    #[test]
    fn non_ascii_rejected() {
        assert!(matches!(
            validate_decline_reason(&Some("é!".into())),
            Err(ValidateError::BadDeclineReason)
        ));
    }
}
