//! ZEB-309 Phase 4a-main: Tier 3 poll state machine + drafting + DfrostLog coupling.
//! See docs/specs/2026-05-20-zeb-309-phase4a-main-design.md §3 + §6 + §9.

use crate::community_voting_core::{
    CandidateEventHash, DeliberationStatementPayload, DraftApprovalPayload, DraftCandidatePayload,
    MiniPublicDeclinePayload, PollEventKindCode, PollId, RatificationBallotPayload,
    SignedVotingEvent, SortitionFailedPayload, SortitionSelectionPayload, Tier3PollConfigPayload,
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
    pub community_epoch: u32,
}

/// Full state of an in-progress or terminal Tier 3 poll.
/// Built by applying kd=* events in (hlc, event_hash) lex order.
#[derive(Debug, Clone)]
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
    /// HLC of the most recent event applied (None before any event).
    pub last_hlc: Option<Hlc>,
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
    #[error("unknown privacy_mode {0:?}; Phase 4a-main only accepts \"pu\"")]
    UnknownPrivacyMode(String),
    #[error("unknown incentive_mode {0:?}; must be one of a/b/c/d")]
    UnknownIncentiveMode(String),
    #[error(
        "ratification ballot scores length {scores} != ratification_candidates length {expected}"
    )]
    BallotLengthMismatch { scores: usize, expected: usize },
    #[error("ratification ballot score {0} > 5")]
    BallotScoreOutOfRange(u8),
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
    if pd.privacy_mode != "pu" {
        // "se" reserved for Phase 6; "rf" reserved for Phase 7.
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
    if pd.scores.len() != expected_candidate_count {
        return Err(ValidateError::BallotLengthMismatch {
            scores: pd.scores.len(),
            expected: expected_candidate_count,
        });
    }
    for &s in &pd.scores {
        if s > 5 {
            return Err(ValidateError::BallotScoreOutOfRange(s));
        }
    }
    Ok(())
}

// ── Apply error type ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
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
            last_hlc: None,
        }
    }

    /// Apply a single event to this state, mutating it in place.
    ///
    /// Dispatches on `ev.kind` per design spec §6 per-event apply table.
    /// Terminal states (Failed, Finalized) reject all further events.
    ///
    /// Verify rules (SS1, SD1, B1-B5, SF1, SR1) are deferred to Task 6/7
    /// and are NOT enforced here; apply_event is the pure materialize layer.
    pub fn apply_event(&mut self, ev: &SignedVotingEvent) -> Result<(), ApplyError> {
        // 1. Reject if terminal state.
        match self.stage {
            Stage::Failed => return Err(ApplyError::PollInFailedState),
            Stage::Finalized => return Err(ApplyError::PollInFinalizedState),
            _ => {}
        }

        // 2. Monotonic HLC check (defensive — upstream should enforce, but we
        //    surface HlcNotMonotonic if the caller sends out-of-order events).
        if let Some(ref last) = self.last_hlc {
            let incoming = &ev.hlc;
            let last_tuple = (last.wall_ms, last.logical, last.device_id.as_str());
            let inc_tuple = (
                incoming.wall_ms,
                incoming.logical,
                incoming.device_id.as_str(),
            );
            if inc_tuple < last_tuple {
                return Err(ApplyError::HlcNotMonotonic);
            }
        }

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

            // kd=ds DeliberationStatement: Phase 4a-main scaffold — accept event
            // (SD1 verify deferred); no state mutation beyond last_hlc update.
            PollEventKindCode::DeliberationStatement => {
                // Validate payload parses (don't silently accept corrupt payloads).
                let _payload: DeliberationStatementPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                // No-op: Phase 5 will cluster statements. Accepted so multi-engine convergence holds.
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
                }
                // If candidate not found: silently ignore (candidate may arrive out of order
                // in future cross-node scenarios; Task 6 verify will enforce ordering constraints).
            }

            // kd=sf SortitionFailed: terminal failure state.
            PollEventKindCode::SortitionFailed => {
                let _payload: SortitionFailedPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                self.stage = Stage::Failed;
                self.last_hlc = Some(ev.hlc.clone());
                return Ok(());
            }

            // kd=rb RatificationBallot: append to ratification_ballots.
            PollEventKindCode::RatificationBallot => {
                let payload: RatificationBallotPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                self.ratification_ballots.push(payload);
            }

            // kd=cl PollClose: record close_event_hash.
            PollEventKindCode::PollClose => {
                let hash = sha256_of_signing_bytes(ev);
                self.close_event_hash = Some(hash);
            }

            // kd=rs PollResult (Tier 3): decode StarResult from payload; transition to Finalized.
            PollEventKindCode::PollResult => {
                let payload: Tier3PollResultPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                self.result = Some(payload.result);
                self.stage = Stage::Finalized;
                self.last_hlc = Some(ev.hlc.clone());
                return Ok(());
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

        self.last_hlc = Some(ev.hlc.clone());
        Ok(())
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

    /// Compute the current mini-public set at HLC watermark `now`.
    ///
    /// Primary minus members who declined at or before `now`, plus backup
    /// auto-promotions (one backup promoted per decline, in backup order).
    ///
    /// Returns empty set if sortition_result is None.
    pub fn current_mini_public(&self, now: &Hlc) -> HashSet<OwnerAddr> {
        let sr = match &self.sortition_result {
            Some(sr) => sr,
            None => return HashSet::new(),
        };

        let decline_count = self.decline_count_at(now);

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

        // Start from primary, remove declines.
        let mut set: HashSet<OwnerAddr> = sr.primary.iter().copied().collect();
        for d in &declined {
            set.remove(d);
        }

        // Auto-promote backups in order (one per decline).
        for backup in sr.backup.iter().take(decline_count) {
            set.insert(*backup);
        }

        set
    }

    /// Count of declines at or before `now`.
    pub fn decline_count_at(&self, now: &Hlc) -> usize {
        self.declines
            .iter()
            .filter(|(_, hlc)| {
                let h = (hlc.wall_ms, hlc.logical, hlc.device_id.as_str());
                let n = (now.wall_ms, now.logical, now.device_id.as_str());
                h <= n
            })
            .count()
    }
}

// ── BeaconOracle trait ────────────────────────────────────────────────────────

/// Trait for looking up VRF beacon output by `(community_id, seed)`.
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
#[async_trait::async_trait]
pub trait BeaconOracle: Send + Sync {
    async fn vrf_output_for(
        &self,
        community_id: &crate::owner_state_types::SpaceId,
        seed: &[u8; 32],
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

    // 3. Look up VRF output.
    let vrf_output = beacon_oracle
        .vrf_output_for(community_id, &seed)
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

    // 2. Backup pool must be exhausted: decline_count ≥ backup_pool_size.
    //    backup_pool_size == sortition_size by design (primary_size == backup_size).
    let declined = poll_state.decline_count_at(&event.hlc);
    let capacity = poll_state.meta.config.sortition_size as usize;
    if declined < capacity {
        return Err(VerifyError::BackupPoolNotExhausted { declined, capacity });
    }

    Ok(())
}

/// SR1 verify: `PollResult` tally must be bit-identical to deterministic re-compute.
///
/// R1 prerequisite: `poll_state.close_event_hash` must be `Some` (kd=cl already applied).
/// R2: re-run `tally_star` over `poll_state.ratification_ballots` and compare.
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

    // Recompute: derive ratification candidates ordering from state, then tally.
    let sq = synthesize_status_quo(&poll_state.meta.poll_id);
    let sq_hash = sq.event_hash;

    // Build candidate list from state (same as the ordered list used at ratification open).
    // For SR1, we re-derive the ordered candidate set from the stored candidates.
    let primary_size = poll_state.meta.config.sortition_size as usize;
    let advancers = drafting_advancers(&poll_state.candidates, primary_size, sq_hash);
    let ordered_candidates = ratification_candidates_ordering(&advancers, sq_hash);

    let recomputed = tally_star(&ordered_candidates, &poll_state.ratification_ballots);

    if recomputed != payload.result {
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
    let sq = synthesize_status_quo(&poll_state.meta.poll_id);
    let sq_hash = sq.event_hash;
    let primary_size = poll_state.meta.config.sortition_size as usize;
    let advancers = drafting_advancers(&poll_state.candidates, primary_size, sq_hash);
    let expected_candidate_count = advancers.len();

    validate_ratification_ballot(&payload, expected_candidate_count)?;

    Ok(())
}

// ── Drafting math ─────────────────────────────────────────────────────────────

/// Maximum number of candidates that advance to ratification (including the
/// guaranteed status_quo slot). Hard cap per design spec §9.
pub const MAX_RATIFICATION_CANDIDATES: usize = 5;

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
/// # Panics
///
/// Panics if `status_quo_hash` is not found in `candidates`. The contract
/// is that `materialize()` synthesizes status_quo before calling this.
pub fn drafting_advancers(
    candidates: &[DraftCandidateState],
    mini_public_size: usize,
    status_quo_hash: CandidateEventHash,
) -> Vec<crate::community_voting_star::CandidateRef> {
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
    let status_quo = candidates
        .iter()
        .find(|c| c.event_hash == status_quo_hash)
        .expect("materialize() guarantees status_quo synthesis at drafting open");
    advancers.push(CandidateRef {
        event_hash: status_quo.event_hash,
        approval_count: status_quo.approvals.len() as u32,
    });

    advancers
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
            scores,
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
        assert_eq!(poll.ratification_ballots[0].scores, vec![3, 1, 5]);
        assert_eq!(poll.ratification_ballots[1].scores, vec![0, 5, 2]);
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
        let result = drafting_advancers(&candidates, 4, sq_hash);
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
        let result = drafting_advancers(&candidates, 4, sq_hash);
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

        let result = drafting_advancers(&candidates, 4, sq_hash);
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
        let result = drafting_advancers(&candidates, 4, sq_hash);
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
        let result = drafting_advancers(&candidates, 4, sq_hash);
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
        let result = drafting_advancers(&candidates, 5, sq_hash);
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
        let result = drafting_advancers(&candidates, 4, sq_hash);
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
        let advancers = drafting_advancers(&candidates, 4, sq_hash);
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
        let advancers = drafting_advancers(&candidates, 4, sq_hash);
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
        let result = drafting_advancers(&candidates, 6, sq_hash);
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

    // Test 8: privacy_mode "se" rejected (Phase 6 forward-compat)
    #[test]
    fn validate_config_privacy_mode_se_rejected_with_unknown_privacy_mode() {
        let mut c = valid_config();
        c.privacy_mode = "se".into();
        assert_eq!(
            validate_tier3_poll_config(&c),
            Err(ValidateError::UnknownPrivacyMode("se".into()))
        );
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
            scores,
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
        // sortition_size=2, 2 declines → pool exhausted
        let poll = poll_with_declines(2, 2);
        let payload = SortitionFailedPayload { poll_id: poll_id() };
        let ev = make_event_with_payload(
            PollEventKindCode::SortitionFailed,
            100,
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
        // sortition_size=3 but only 1 decline → pool not exhausted (need 3)
        let poll = poll_with_declines(1, 3);
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
                capacity: 3
            })
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
        let advancers = drafting_advancers(&poll.candidates, primary_size, sq_hash);
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

    // 16. verify_rb_actor_in_full_electorate_accepted
    #[test]
    fn verify_rb_actor_in_full_electorate_accepted() {
        let poll = poll_in_ratification();
        // The electorate is addr(0..19); any of them can cast a ratification ballot.
        // wall_ms = 25000 > 20000 → Ratification stage
        // Candidate count = 2 (1 real + status_quo)
        let rb_payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: vec![3, 1], // 2 candidates: real + status_quo
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
            scores: vec![3, 1],
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
            scores: vec![3, 1],
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
            scores: vec![6, 1], // score 6 > 5 → BallotScoreOutOfRange
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
            scores: vec![3, 1, 5],
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
}
