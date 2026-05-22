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
    /// Phase 5 (Tier 3b) deliberation projection. Populated by `kd=ds`
    /// and `kd=dv` apply paths. Default empty until Task 3/4 wire apply.
    pub deliberation: DeliberationState,
    /// HLC of the most recent **accepted** event (None before any accept).
    /// Used as the `now` watermark by `current_stage_at` and `current_mini_public`
    /// via `build_tier3_export`. Drop paths must NOT advance this (ZEB-320).
    pub last_hlc: Option<Hlc>,
    /// HLC of the most recent **dispatched** event, accepted *or* silently
    /// dropped. Used for the monotonic-HLC guard at the top of `apply_event`
    /// so out-of-order peer delivery is still surfaced as `HlcNotMonotonic`
    /// even when the previous event was dropped — without this, a dropped
    /// event followed by an earlier-HLC event that would change drop decisions
    /// (e.g., the kd=ds whose existence is the prerequisite for a previously
    /// target-missing kd=dv) would silently bypass the guard and never be
    /// re-materialized, causing replica divergence (Qodo PR #154 finding).
    pub last_received_hlc: Option<Hlc>,
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
            deliberation: DeliberationState::default(),
            last_hlc: None,
            last_received_hlc: None,
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
        //    Reads `last_received_hlc` (advances on every dispatch, accept or
        //    drop) rather than `last_hlc` (advances on accepts only) so the
        //    guard still catches out-of-order delivery after a dropped event.
        if let Some(ref last) = self.last_received_hlc {
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

            // kd=rb RatificationBallot: append to ratification_ballots.
            PollEventKindCode::RatificationBallot => {
                let payload: RatificationBallotPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                self.ratification_ballots.push(payload);
            }

            // kd=ts TallyShare (ZEB-295 Phase 6): committee member's partial
            // decryption share + DLEQ proof. Task 1 (wire format) only — apply
            // behavior is wired in Task 7 (secret-tally state). Silent drop
            // here keeps the projection unchanged; last_hlc must NOT advance
            // (ZEB-320) so a future re-apply pass after Task 7 lands can
            // process the same event.
            PollEventKindCode::TallyShare => {
                let _payload: crate::community_voting_core::TallySharePayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                advance_last_hlc = false;
                tracing::debug!(
                    poll_id = %hex::encode(self.meta.poll_id.0),
                    actor = %hex::encode(ev.actor.0),
                    "kd=ts drop: TallyShare apply not yet implemented (ZEB-295 Task 7)"
                );
            }

            // kd=cl PollClose: record close_event_hash.
            PollEventKindCode::PollClose => {
                let hash = sha256_of_signing_bytes(ev);
                self.close_event_hash = Some(hash);
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

        // last_received_hlc always advances on Ok return — including silent
        // drops — so the monotonic guard keeps catching out-of-order delivery
        // after a drop. last_hlc only advances on accepts (ZEB-320).
        self.last_received_hlc = Some(ev.hlc.clone());
        if advance_last_hlc {
            self.last_hlc = Some(ev.hlc.clone());
        }
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
    // If status_quo is not yet present, the poll hasn't reached Drafting/Ratification
    // stage — the PollResult event is premature; reject with StatusQuoNotSynthesized.
    let primary_size = poll_state.meta.config.sortition_size as usize;
    let advancers = drafting_advancers(&poll_state.candidates, primary_size, sq_hash)
        .ok_or(VerifyError::StatusQuoNotSynthesized)?;
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
    // If status_quo is not yet present, the poll hasn't reached Drafting/Ratification
    // stage — a RatificationBallot arriving now is premature.
    let sq = synthesize_status_quo(&poll_state.meta.poll_id);
    let sq_hash = sq.event_hash;
    let primary_size = poll_state.meta.config.sortition_size as usize;
    let advancers = drafting_advancers(&poll_state.candidates, primary_size, sq_hash)
        .ok_or(VerifyError::StatusQuoNotSynthesized)?;
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

    // A1: verify_sr on a Stage 2 (Deliberation) poll with no status_quo synthesized
    //     must NOT panic — must return StatusQuoNotSynthesized.
    #[test]
    fn verify_sr_on_stage_2_poll_rejects_with_status_quo_not_synthesized() {
        // Poll in Stage::Deliberation (sortition done, dw not elapsed).
        // status_quo has NOT been synthesized into candidates yet.
        let mut poll = new_poll(0); // create at 0, dw=10s, fw=10s
        poll.apply_event(&ss_event(10, vec![addr(1), addr(2), addr(3)], vec![]))
            .expect("ss");
        // Apply kd=cl so NotInClosedStage is bypassed (we want to hit StatusQuoNotSynthesized).
        let cl_ev = make_event(PollEventKindCode::PollClose, 5000, addr(0xff));
        poll.apply_event(&cl_ev).expect("cl");
        // No status_quo in candidates — poll hasn't reached Drafting yet.
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
        // Must not panic; must return StatusQuoNotSynthesized.
        let result = verify_sr(&ev, &poll);
        assert_eq!(
            result,
            Err(VerifyError::StatusQuoNotSynthesized),
            "expected StatusQuoNotSynthesized, got {result:?}"
        );
    }

    // A2: verify_ratification_ballot on a Stage 1 (Sortition) poll with no status_quo
    //     must NOT panic — must return StatusQuoNotSynthesized.
    #[test]
    fn verify_ratification_ballot_on_stage_1_poll_rejects_with_status_quo_not_synthesized() {
        // Poll in Stage::Ratification time window, but status_quo NOT synthesized.
        let mut meta = meta_at(0);
        meta.config.sortition_size = 3;
        let mut poll = Tier3PollState::new_from_create(meta, electorate(20));
        poll.apply_event(&ss_event(10, vec![addr(1), addr(2), addr(3)], vec![]))
            .expect("ss");
        // Wall_ms=25000 → past dw+fw threshold (20000) → Ratification stage.
        // But no candidates / status_quo synthesized.
        assert!(poll.candidates.is_empty());

        let rb_payload = RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(vec![3, 1]), // 2 scores — but no status_quo yet
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
        // Must not panic; must return StatusQuoNotSynthesized.
        let result = verify_ratification_ballot(&ev, &poll);
        assert_eq!(
            result,
            Err(VerifyError::StatusQuoNotSynthesized),
            "expected StatusQuoNotSynthesized, got {result:?}"
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
            poll.last_received_hlc.as_ref().unwrap().wall_ms,
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
