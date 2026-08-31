//! ZEB-290 Phase 1: per-community voting event log.
//!
//! Parallels `community_channel_log.rs` (ZEB-248 pattern). Holds all
//! `SignedVotingEvent`s for a community plus the materialized per-poll
//! state map. Zenoh sync wiring lives in Task 12; this file is the
//! pure data structure + apply/materialize logic.

use sha2::Digest;
use std::collections::HashMap;

use crate::community_voting_approval::{validate_poll_config, Tier1PollConfig, Tier1TallyState};
use crate::community_voting_conviction::{
    DelegatePayload, DelegationGraph, SignalPayload, Tier2PollConfig, Tier2ProposalState,
    UndelegatePayload,
};
use crate::community_voting_core::{
    derive_poll_id, next_lifecycle, Lifecycle, MembershipSnapshot, PollEventKindCode, PollId,
    PollMeta, SignedVotingEvent, Tier, Tier3PollConfigPayload,
};
use crate::community_voting_sortition::canonical_electorate_order;
use crate::community_voting_tier3::{Tier3PollMeta, Tier3PollState};
use crate::owner_state_types::Hlc;

/// Resolves the community's membership snapshot at a specific HLC,
/// used by `process_inbound` for `PollCreate` events. Non-`PollCreate`
/// inbound events reuse the snapshot frozen on the poll's state at
/// create time. Production impl reads from `community_registry` +
/// `crdt_state` via `NodeState`; tests use a fixed snapshot.
#[async_trait::async_trait]
pub trait MembershipSnapshotResolver: Send + Sync {
    /// Resolve the per-community membership snapshot at (or as of) `hlc`.
    /// Returns `Err` if the community is not loaded locally (e.g. we
    /// never joined). Apply layer treats this as "reject the inbound
    /// event" rather than "accept anyway".
    async fn snapshot_at(
        &self,
        community_id: crate::owner_state_types::SpaceId,
        hlc: &crate::owner_state_types::Hlc,
    ) -> Result<crate::community_voting_core::MembershipSnapshot, SnapshotResolverError>;

    /// ZEB-1031 §5.1/§6.1: resolve the FULL materialized membership
    /// state — including `reset_proposals`, not just the narrow
    /// `MembershipSnapshot` projection `snapshot_at` returns — strictly
    /// BEFORE `hlc` (same at-event-HLC discipline as `snapshot_at`, via
    /// `prior_state_at_hlc`). Consumed by the D-FROST engine's
    /// `verify_reset_marker_admissible` (RS-M3/M4/M5), which must
    /// evaluate reset-proposal phase/digest/actor-power at the marker
    /// event's OWN envelope HLC so the verdict is deterministic across
    /// replicas regardless of arrival order.
    ///
    /// Default: unsupported (`Err`) — only the production
    /// `NodeStateMembershipResolver` overrides this; the voting/channel-
    /// log resolvers this trait already serves have no use for it.
    async fn reset_membership_at(
        &self,
        _community_id: crate::owner_state_types::SpaceId,
        _hlc: &crate::owner_state_types::Hlc,
    ) -> Result<crate::community_membership::MaterializedMembership, SnapshotResolverError> {
        Err(SnapshotResolverError::BackendError(
            "reset membership evidence not supported by this resolver (ZEB-1031)".into(),
        ))
    }

    /// ZEB-1031 §6.1: resolve the CURRENT (at-HEAD) materialized
    /// membership state. Used by `adopt_initial_quorum`/
    /// `adopt_refresh_quorum` call sites to compute
    /// `dfrost_reset_rejected_vks` against THIS replica's own live
    /// view — deliberately never a peer-supplied HLC, since a stale
    /// event's HLC could hide a reset authorized after it, defeating
    /// the exact replay this gate exists to close.
    ///
    /// Default: unsupported (`Err`).
    async fn reset_membership_now(
        &self,
        _community_id: crate::owner_state_types::SpaceId,
    ) -> Result<crate::community_membership::MaterializedMembership, SnapshotResolverError> {
        Err(SnapshotResolverError::BackendError(
            "reset membership evidence not supported by this resolver (ZEB-1031)".into(),
        ))
    }
}

/// Why `MembershipSnapshotResolver::snapshot_at` failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotResolverError {
    #[error("community {0:?} not loaded locally")]
    CommunityNotLoaded(crate::owner_state_types::SpaceId),
    #[error("failed to read membership state: {0}")]
    BackendError(String),
}

/// All voting events for a single community, plus the materialized
/// per-poll state derived from them.
///
/// Stored in `NodeState` keyed by community SpaceId. Synced via Zenoh
/// topic `harmony/community/{id}/voting` (Task 12).
#[derive(Debug, Default, Clone)]
pub struct VotingLog {
    /// All accepted events, in arrival (apply) order — `apply_with_snapshot`
    /// pushes; it does NOT insert-sort. Canonical `(hlc, event_hash)` order is
    /// (re)established by `rebuild_from_events`
    /// (`sort_by_cached_key(canonical_key)`), which is what keeps live state
    /// byte-equal to boot-restore (ZEB-860/867). Wire/replay order is not
    /// correctness-bearing here: each event is self-contained and re-applied
    /// through coordinate-dedup.
    pub events: Vec<SignedVotingEvent>,
    /// Materialized per-poll state, keyed by PollId.
    pub polls: HashMap<PollId, PollState>,
    /// Per-community delegation graph for Tier 2 conviction voting (spec §5).
    /// Delegation is community-wide (NOT per-poll): a single
    /// `delegator → delegate` edge applies to every Tier 2 proposal in the
    /// community. Maintained via `Delegate`/`Undelegate` events; HLC-LWW
    /// resolves concurrent updates. Empty for communities with no Tier 2
    /// activity yet.
    pub delegation_graph: DelegationGraph,
    /// ZEB-298: community-scoped voting policy. Mutated via IPC (not
    /// via signed event). Default = all-fields-false so existing
    /// communities preserve pre-policy behavior.
    policy: crate::community_voting_conviction::CommunityVotingPolicy,
}

/// Materialized state for a single poll.
#[derive(Debug, Clone)]
pub struct PollState {
    pub meta: PollMeta,
    /// All events belonging to this poll, ordered by HLC.
    pub events: Vec<SignedVotingEvent>,
    /// Tier-specific tally state, opaque to voting_core. Phase 1 ships
    /// only `Tier1`; Phase 2/4+ add variants. Using an enum (rather
    /// than `Box<dyn Any>`) keeps the code monomorphic and trivially
    /// Clone'able for fork/persist.
    pub tier_state: TierState,
    /// Tier 1 deserialized config, populated at PollCreate-apply time.
    /// Cached so ballot validation can fail fast without re-decoding
    /// the PollCreate payload on every ballot. None for non-Tier 1.
    pub tier1_cfg: Option<Tier1PollConfig>,
    /// Frozen eligibility snapshot captured at PollCreate-apply time
    /// (spec §7 — eligibility is evaluated against community state at
    /// the poll's create HLC, not at ballot-cast HLC). The local
    /// IPC creator passes its own computed snapshot via `apply_with_snapshot`;
    /// peer-received PollCreate events leave this `None` until Task 12
    /// wires the materialize-at-HLC path. None for non-Tier 1.
    pub tier1_snapshot: Option<MembershipSnapshot>,
}

/// Tier-specific tally state. Each variant holds the materialized
/// per-tier aggregate; the apply path picks the right variant at
/// `PollCreate` time based on `event.tier`. Phase 1 ships only
/// `Tier1`; Phase 2 adds `Tier2`; Phase 4a-main adds `Tier3`.
///
/// `Tier3` is boxed to avoid inflating every enum value to the size of the
/// largest variant — `Tier3PollState` is significantly larger than `Tier1` +
/// `Tier2` combined (clippy::large_enum_variant).
#[derive(Debug, Clone)]
pub enum TierState {
    Tier1(Tier1TallyState),
    Tier2(Tier2ProposalState),
    Tier3(Box<Tier3PollState>),
}

impl TierState {
    pub fn as_tier1(&self) -> Option<&Tier1TallyState> {
        match self {
            TierState::Tier1(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_tier1_mut(&mut self) -> Option<&mut Tier1TallyState> {
        match self {
            TierState::Tier1(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_tier2(&self) -> Option<&Tier2ProposalState> {
        match self {
            TierState::Tier2(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_tier2_mut(&mut self) -> Option<&mut Tier2ProposalState> {
        match self {
            TierState::Tier2(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_tier3(&self) -> Option<&Tier3PollState> {
        match self {
            TierState::Tier3(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_tier3_mut(&mut self) -> Option<&mut Tier3PollState> {
        match self {
            TierState::Tier3(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    SigningBytesError,
    MissingPollIdRef,
    IllegalTransition,
    EventBeforePollCreate,
    PayloadDecode,
    PayloadValidate,
    /// Tier 2 Signal/Delegate/Undelegate applied to a poll whose
    /// `tier_state` is not `Tier2` (mis-routed event — caller should have
    /// rejected at verify-time).
    WrongTierForEvent,
    /// Tier 2 Delegate event rejected by `DelegationGraph::apply_delegate`
    /// (cycle in the graph or HLC-LWW stale).
    DelegationRejected,
    /// Tier 3 event kind not valid for the Tier 3 state machine
    /// (e.g., BallotCast, PollOpen, PollExtend on a Sortition poll).
    InvalidKindForTier3,
    /// Tier 3 PollCreate with `retry_of = Some(prev)` but `prev` does not
    /// exist in this log.
    RetryOfPollNotFound,
    /// Tier 3 PollCreate with `retry_of = Some(prev)` but `prev` is not in
    /// Stage::Failed (only failed polls may be retried).
    RetryOfPollNotFailed,
    /// A Tier 3 event arrived for a poll that is not a Tier 3 poll (the
    /// tier_state is not `Tier3`).
    WrongTierStateForTier3Event,
    /// Tier 3 PollCreate with `retry_of = Some(prev)` but `prev` is a non-Tier3
    /// poll (the tier_state is not `Tier3`). Distinct from `RetryOfPollNotFailed`
    /// which applies when the predecessor is Tier 3 but not in Failed stage.
    RetryOfPollNotTier3,
    /// ZEB-1031 Task 7: event targeted a Tier 3 poll voided by a committee
    /// reset (spec §7). Distinct from `IllegalTransition` (the ordinary
    /// Failed/Finalized terminal rejections) so callers can surface a
    /// reset-specific message and prompt a relaunch instead of a generic
    /// "poll closed" error.
    PollVoided,
}

/// ZEB-860 / Cluster K: mirror a Tier-3 poll's terminal `stage` into the
/// generic `PollMeta.lifecycle` (+ `finalized_at_ms`). Runs after every Tier-3
/// apply and again after an out-of-order canonical rebuild (which can move the
/// stage), so `archive_finalized_polls()` always sees a consistent lifecycle.
///
/// Behavior mirrors the inline match it replaces. `finalized_at_ms` is stamped
/// from the finalizing (kd=rs) event's wall_ms, read off `last_hlc`: a kd=rs
/// finalize always accepts (advances `last_hlc`), so at the post-apply call
/// site `last_hlc.wall_ms` equals the `event.hlc.wall_ms` the inline match
/// previously read; at the post-rebuild call site it is the canonical
/// finalizer's wall. Non-terminal stages are a no-op (lifecycle stays Open);
/// no-op if the poll is not Tier-3.
fn sync_lifecycle_from_stage(state: &mut PollState) {
    let Some(stage) = state.tier_state.as_tier3().map(|t3| t3.stage) else {
        return;
    };
    match stage {
        crate::community_voting_tier3::Stage::Finalized => {
            state.meta.lifecycle = Lifecycle::Finalized;
            // Stamp finalized_at_ms so archive_finalized_polls() can age it.
            if let Some(wall_ms) = state
                .tier_state
                .as_tier3()
                .and_then(|t3| t3.last_hlc.as_ref())
                .map(|h| h.wall_ms)
            {
                state.meta.finalized_at_ms = Some(wall_ms);
            }
        }
        crate::community_voting_tier3::Stage::Failed => {
            // No Lifecycle::Failed variant; Closed is the closest match
            // and prevents the poll from appearing Open after failure.
            state.meta.lifecycle = Lifecycle::Closed;
        }
        _ => {} // Non-terminal stages: lifecycle stays Open.
    }
}

impl VotingLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn policy(&self) -> &crate::community_voting_conviction::CommunityVotingPolicy {
        &self.policy
    }

    pub fn set_policy(
        &mut self,
        policy: crate::community_voting_conviction::CommunityVotingPolicy,
    ) {
        self.policy = policy;
    }

    /// Returns true if any poll with this PollId is currently tracked.
    /// Used by `voting_resolve_community_for_poll` (lib.rs) to locate
    /// the owning SpaceId for an IPC that only knows the poll_id.
    pub fn has_poll(&self, pid: &crate::community_voting_core::PollId) -> bool {
        self.polls.contains_key(pid)
    }

    /// Return the count of ratification candidates for a Tier 3 poll
    /// currently in `Stage::Ratification` (HLC-aware). Returns `None` if:
    /// - the poll does not exist,
    /// - the poll is not Tier 3,
    /// - the poll has no `last_hlc` (no events applied yet — definitely
    ///   not in Ratification), or
    /// - the poll is not in `Stage::Ratification` at `last_hlc`.
    ///
    /// Used by `voting_cast_ratification_ballot` for pre-flight ballot
    /// validation against the canonical candidate ordering.
    ///
    /// ## Why a local status_quo synthesis
    ///
    /// `synthesize_status_quo` is NEVER written into `t3.candidates` by
    /// `apply` — status_quo is materialized only on a local clone at
    /// orchestration time (see `verify_sr`, `verify_ratification_ballot`,
    /// the engine-auto kd=rs trigger in
    /// `community_voting_log_engine::maybe_trigger_engine_auto_orchestration`,
    /// and the ratification-open Tauri-event branch in the post-apply
    /// hook). If we called `drafting_advancers(&t3.candidates, ...)`
    /// directly, it would return `None` for every production poll
    /// because status_quo wouldn't be in the slice — and the IPC's
    /// pre-flight would always reject valid ballots.
    ///
    /// This helper mirrors the engine-auto kd=rs pattern: clone
    /// `t3.candidates`, push synthesized status_quo, then run
    /// `drafting_advancers` + `ratification_candidates_ordering`. The
    /// returned count includes status_quo.
    ///
    /// `now` is the HLC the caller intends to use as "now" — typically the
    /// HLC just reserved for the new ratification ballot. Gating on
    /// `t3.last_hlc` here would be incorrect: that is the HLC of the last
    /// applied event on this poll, which may pre-date the ratification
    /// window if no events have been applied since deliberation closed.
    pub fn tier3_ratification_candidate_count(
        &self,
        pid: &crate::community_voting_core::PollId,
        now: &crate::owner_state_types::Hlc,
    ) -> Option<usize> {
        let state = self.polls.get(pid)?;
        let t3 = state.tier_state.as_tier3()?;

        // Gate on Ratification stage using the caller-provided "now"
        // (typically the HLC reserved for the new ballot). Using
        // t3.last_hlc would be wrong — that's the HLC of the latest
        // applied event on this poll, which can lag the ratification
        // window when no events have been applied since deliberation
        // closed.
        if !matches!(
            t3.current_stage_at(now),
            crate::community_voting_tier3::Stage::Ratification
        ) {
            return None;
        }

        // Mirror the engine-auto kd=rs computation: synthesize status_quo
        // locally and push onto a temp candidates slice so
        // `drafting_advancers` returns Some.
        let sq = crate::community_voting_tier3::synthesize_status_quo(&t3.meta.poll_id);
        let sq_hash = sq.event_hash;
        let mut all_candidates = t3.candidates.clone();
        all_candidates.push(sq);
        let primary_size = t3.meta.config.sortition_size as usize;
        let advancers = crate::community_voting_tier3::drafting_advancers(
            &all_candidates,
            primary_size,
            sq_hash,
        )?;
        let ordering =
            crate::community_voting_tier3::ratification_candidates_ordering(&advancers, sq_hash);
        Some(ordering.len())
    }

    /// Apply a new event to the log. Caller has already done verify
    /// (V1-V6, kind-specific) — this function only handles materialize
    /// (lifecycle transition + tier-specific apply).
    ///
    /// Returns Ok(poll_id) if applied; Err if lifecycle transition
    /// is illegal (which indicates a verify-rule violation by the caller).
    ///
    /// For locally-created PollCreate events, prefer `apply_with_snapshot`
    /// — the IPC path knows the membership snapshot at create-HLC and
    /// caches it on the inserted `PollState` for cheap ballot re-checks.
    pub fn apply(
        &mut self,
        event: SignedVotingEvent,
        community_id: &crate::owner_state_types::SpaceId,
    ) -> Result<PollId, ApplyError> {
        self.apply_with_snapshot(event, community_id, None)
    }

    /// Apply with an optional caller-supplied eligibility snapshot.
    /// Stored on the new `PollState` when `event.kind == PollCreate`.
    /// For Tier 1 (`Tier::Approval`) it caches as `tier1_snapshot`; for
    /// Tier 2 (`Tier::Conviction`) it's used to derive `total_supply` for
    /// the new `Tier2ProposalState`.
    pub fn apply_with_snapshot(
        &mut self,
        event: SignedVotingEvent,
        community_id: &crate::owner_state_types::SpaceId,
        snapshot: Option<MembershipSnapshot>,
    ) -> Result<PollId, ApplyError> {
        // PollCreate derives PollId from H(community_id || signing_bytes);
        // every other kind references an existing PollId via a `{ "pi": ... }`
        // map in the payload — except Tier 2 Signal/Delegate/Undelegate which
        // use their own canonical payload shapes (proposal_id field for Signal,
        // delegator-implicit for Delegate/Undelegate). Decoded below.
        let poll_id = match event.kind {
            PollEventKindCode::PollCreate => {
                let sb = event
                    .signing_bytes()
                    .map_err(|_| ApplyError::SigningBytesError)?;
                derive_poll_id(community_id, &sb)
            }
            PollEventKindCode::Signal => {
                let p: SignalPayload = ciborium::de::from_reader(&event.payload[..])
                    .map_err(|_| ApplyError::PayloadDecode)?;
                p.proposal_id
            }
            // Delegate/Undelegate are NOT bound to a specific poll — they
            // mutate the community-wide delegation graph. We still need a
            // poll_id return value for the IPC layer; for now route via
            // a sentinel zero PollId. (Tier 2 IPC/UI in Task 18 may stop
            // calling apply for these and route the graph mutation
            // directly; for Task 9 we keep the apply call site uniform.)
            PollEventKindCode::Delegate | PollEventKindCode::Undelegate => PollId([0u8; 32]),
            _ => decode_poll_id_ref(&event.payload).ok_or(ApplyError::MissingPollIdRef)?,
        };

        // ---- Tier 2 Signal: mutate per-voter conviction; NO lifecycle ----
        // Per spec §5 + Task 9: Signal events alone do NOT drive lifecycle
        // transitions. Threshold-cross / threshold-drop transitions are
        // owned by the Task 15 tick which inspects total_conviction vs
        // threshold_conviction. The Signal apply path updates the per-
        // voter state, appends to the event log, AND — when an unsignal
        // arrives while the proposal is in ThresholdReached — stamps
        // `last_unsignal_after_threshold_ms` so the tick's 24h
        // contestability window resets. Without that stamp, a fresh
        // contest would finalize against the original
        // `threshold_reached_at_ms` and slip through inside the original
        // window (CR R3 Major).
        if event.kind == PollEventKindCode::Signal && event.tier == Tier::Conviction {
            let payload: SignalPayload = ciborium::de::from_reader(&event.payload[..])
                .map_err(|_| ApplyError::PayloadDecode)?;
            let state = self
                .polls
                .get_mut(&poll_id)
                .ok_or(ApplyError::EventBeforePollCreate)?;
            // Lifecycle gate (Cursor R6 Medium): Signal events are
            // only valid while the proposal is still actively
            // collecting conviction. A Signal arriving on a Finalized,
            // Closed, or Archived proposal would otherwise still
            // mutate `per_voter` conviction state and stamp
            // `last_unsignal_after_threshold_ms` — corrupting the
            // historical record of a terminal proposal.
            if !matches!(
                state.meta.lifecycle,
                Lifecycle::Open | Lifecycle::ThresholdReached
            ) {
                return Err(ApplyError::IllegalTransition);
            }
            let in_threshold_reached = state.meta.lifecycle == Lifecycle::ThresholdReached;
            let proposal_state = state
                .tier_state
                .as_tier2_mut()
                .ok_or(ApplyError::WrongTierForEvent)?;
            let hl_ms = (proposal_state.config.half_life_seconds as i128) * 1000;
            proposal_state
                .per_voter
                .entry(event.actor)
                .or_default()
                .apply_signal(
                    payload.support,
                    event.hlc.wall_ms as i128,
                    event.hlc.logical,
                    hl_ms,
                );
            if !payload.support && in_threshold_reached {
                proposal_state.last_unsignal_after_threshold_ms = Some(event.hlc.wall_ms as i128);
            }
            state.events.push(event.clone());
            self.events.push(event);
            return Ok(poll_id);
        }

        // ---- Tier 2 Delegate: graph mutation; NO lifecycle ----
        if event.kind == PollEventKindCode::Delegate && event.tier == Tier::Conviction {
            // The 16-byte length invariant is now enforced at decode by
            // `DelegatePayload.to: OwnerAddr` (CR R5 Major). Decode-time
            // rejection means malformed peer events never reach the
            // apply path with an unconstrained Vec<u8>.
            let payload: DelegatePayload = ciborium::de::from_reader(&event.payload[..])
                .map_err(|_| ApplyError::PayloadDecode)?;
            self.delegation_graph
                .apply_delegate(
                    event.actor,
                    payload.to,
                    (event.hlc.wall_ms, event.hlc.logical),
                )
                .map_err(|_| ApplyError::DelegationRejected)?;
            self.events.push(event);
            return Ok(poll_id);
        }

        // ---- Tier 2 Undelegate: graph mutation; NO lifecycle ----
        if event.kind == PollEventKindCode::Undelegate && event.tier == Tier::Conviction {
            // Payload is the empty `UndelegatePayload {}`; we decode
            // defensively to surface PayloadDecode on a malformed input
            // rather than silently accepting whatever bytes arrived.
            let _payload: UndelegatePayload = ciborium::de::from_reader(&event.payload[..])
                .map_err(|_| ApplyError::PayloadDecode)?;
            self.delegation_graph
                .apply_undelegate(event.actor, (event.hlc.wall_ms, event.hlc.logical));
            self.events.push(event);
            return Ok(poll_id);
        }

        // ---- Tier 3 non-create events: route to Tier3PollState::apply_event ----
        //
        // Tier 3 polls have their own 4-stage state machine (Stage::Sortition /
        // Deliberation / Drafting / Ratification / Finalized / Failed) that
        // does NOT use the generic Lifecycle + next_lifecycle path.  All Tier 3
        // event kinds except PollCreate are routed here and delegated to
        // `Tier3PollState::apply_event`.
        if event.tier == Tier::Sortition && event.kind != PollEventKindCode::PollCreate {
            let state = self
                .polls
                .get_mut(&poll_id)
                .ok_or(ApplyError::EventBeforePollCreate)?;
            let tier3_state = state
                .tier_state
                .as_tier3_mut()
                .ok_or(ApplyError::WrongTierStateForTier3Event)?;

            // ZEB-860: snapshot the out-of-order watermark BEFORE apply — the
            // apply advances max_applied, so it must be captured first — along
            // with this event's canonical key. `trigger_kind` is captured here
            // too since `event` is moved into `self.events` below.
            let prev_max = tier3_state.max_applied.clone();
            let ev_key3 = (
                event.hlc.wall_ms,
                event.hlc.logical,
                event.hlc.device_id.clone(),
            );
            let trigger_kind = matches!(
                event.kind,
                PollEventKindCode::SortitionSelection
                    | PollEventKindCode::MiniPublicDecline
                    | PollEventKindCode::DeliberationStatement
                    | PollEventKindCode::DeliberationVote
            );

            // ZEB-867 (Component 2): decide up front (tier3_state is borrowed here
            // and `event` is moved below) whether a post-finalize arrival is a
            // backdated ratification ballot to refold. The gate compares the ballot's
            // OWN canonical key against the poll's FINALIZE key — the kd=rs event's
            // key (the min if more than one was ever recorded) — NOT the global
            // `max_applied` watermark. `max_applied` can exceed the finalize key (a
            // ballot may apply pre-finalize with a higher key), so a watermark gate
            // could admit a ballot that sorts AFTER the finalize and is then dropped
            // by canonical replay (recorded-but-absent from the projection).
            // Comparing against the actual finalize key is exact: an admitted ballot
            // always sorts before the finalize, so replay always folds it in.
            // (CodeAnt, PR #593.) pu-gated: se keeps today's drop-on-finalize
            // behavior (se finalize is Lagrange-invariant).
            let finalize_key = state
                .events
                .iter()
                .filter(|e| e.kind == PollEventKindCode::PollResult)
                .map(|e| (e.hlc.wall_ms, e.hlc.logical, e.hlc.device_id.clone()))
                .min();
            let refold_backdated_ballot = tier3_state.meta.config.privacy_mode == "pu"
                && event.kind == PollEventKindCode::RatificationBallot
                && finalize_key.as_ref().is_some_and(|rk| ev_key3 < *rk);

            // ZEB-867 (Component 2): such a ballot arriving AFTER the pu poll
            // finalized is rejected by apply_event's terminal guard
            // (PollInFinalizedState). Instead of dropping it, RECORD it and
            // re-materialize in canonical order so the late ballot folds into the
            // tally before the finalize and re-finalizes — preserving live ==
            // boot-restore. A refolded ballot always sorts before the finalize, so it
            // is always reflected in the rebuilt projection (never
            // persisted-but-absent). Genuinely post-close events (key at/after the
            // finalize) fail the gate and keep today's drop behavior. Failed is never
            // loosened; the terminal guard runs before any field write, so the
            // rejected apply leaves the projection untouched and the rebuild is the
            // sole mutation.
            let outcome = match tier3_state.apply_event(&event) {
                Ok(o) => o,
                Err(crate::community_voting_tier3::ApplyError::PollInFinalizedState)
                    if refold_backdated_ballot =>
                {
                    state.events.push(event.clone());
                    self.events.push(event);
                    let state = self
                        .polls
                        .get_mut(&poll_id)
                        .expect("poll present (just appended)");
                    let events = std::mem::take(&mut state.events);
                    if let Some(t3) = state.tier_state.as_tier3_mut() {
                        t3.rebuild_from_events(&events);
                    }
                    state.events = events;
                    sync_lifecycle_from_stage(state);
                    return Ok(poll_id);
                }
                Err(e) => {
                    return Err(match e {
                        crate::community_voting_tier3::ApplyError::InvalidKindForTier3(_) => {
                            ApplyError::InvalidKindForTier3
                        }
                        // Terminal-state rejections map to IllegalTransition.
                        crate::community_voting_tier3::ApplyError::PollInFailedState
                        | crate::community_voting_tier3::ApplyError::PollInFinalizedState
                        | crate::community_voting_tier3::ApplyError::HlcNotMonotonic => {
                            ApplyError::IllegalTransition
                        }
                        crate::community_voting_tier3::ApplyError::PayloadDecode(_) => {
                            ApplyError::PayloadDecode
                        }
                        crate::community_voting_tier3::ApplyError::PollVoided => {
                            ApplyError::PollVoided
                        }
                    });
                }
            };

            // Cluster K fix: sync PollMeta.lifecycle from tier3 stage after a
            // terminal transition (Finalized / Failed).  Must happen BEFORE
            // pushing the event so archive_finalized_polls() sees the synced state.
            sync_lifecycle_from_stage(state);

            state.events.push(event.clone());
            self.events.push(event);

            // ZEB-860: an out-of-order arrival of an order-dependent
            // Deliberation-family event that WAS applied can retroactively
            // change other events' outcomes (a late kd=ds unblocks a dropped
            // kd=dv; a backdated kd=dv must be re-dropped). When the trigger
            // holds, re-materialize this poll's projection as a deterministic
            // fold of its per-poll events in canonical order.
            //
            // Trigger = ALL of: (1) the event was out-of-order — its key is
            // <= the watermark captured BEFORE this apply; (2) apply accepted
            // it (Applied, not silently Dropped — a dropped stranger event must
            // never force a rebuild, a DoS guard); (3) the kind is
            // order-dependent (ss / md / ds / dv, captured above).
            //
            // Two bounded cost properties (neither is a divergence — the rebuild
            // reproduces exactly what boot-restore computes):
            //   * `max_applied` advances on DROPS too (load-bearing: a late ds must
            //     lift the watermark so a still-dropped dv is reconsidered). So a
            //     future-dated *dropped* trigger-kind event could poison the
            //     watermark and make later honest events rebuild — but both
            //     peer-facing ingest paths reject wall > now + MAX_FORWARD_SKEW_MS
            //     (ZEB-846), capping the poison to a self-healing ~5-min window.
            //   * ds/dv self-gate to Deliberation (dropped elsewhere ⇒ not Applied),
            //     but ss/md do NOT, so a backdated ss/md arriving in Ratification can
            //     rebuild and re-run rb/ts crypto — insider-only, bounded by event
            //     count, ZEB-846-limited. Trigger-tightening tracked in ZEB-868.
            let out_of_order = prev_max.as_ref().is_some_and(|m| ev_key3 <= *m);
            if out_of_order
                && outcome == crate::community_voting_tier3::ApplyOutcome::Applied
                && trigger_kind
            {
                let state = self
                    .polls
                    .get_mut(&poll_id)
                    .expect("poll present (just appended)");
                // Split-borrow: `events` (immut) and `tier_state` (mut) are
                // disjoint fields. `mem::take` sidesteps the borrow conflict
                // between `&state.events` and the `&mut` rebuild without cloning
                // the whole Vec; the events already contain the just-appended
                // triggering event AND the applied ss.
                let events = std::mem::take(&mut state.events);
                if let Some(t3) = state.tier_state.as_tier3_mut() {
                    t3.rebuild_from_events(&events);
                }
                state.events = events;
                // The rebuild can move the stage (e.g. a canonical re-fold that
                // finalizes) — re-sync the lifecycle from the rebuilt stage.
                sync_lifecycle_from_stage(state);
            }
            return Ok(poll_id);
        }

        // ---- All other event kinds: existing lifecycle-driven path ----

        // For non-create events, require an existing poll. We check this
        // *before* the lifecycle transition so the failure surfaces as
        // EventBeforePollCreate (more specific) rather than the generic
        // IllegalTransition that the Draft state machine would otherwise emit.
        let existing_lifecycle = self.polls.get(&poll_id).map(|p| p.meta.lifecycle);
        if existing_lifecycle.is_none() && event.kind != PollEventKindCode::PollCreate {
            return Err(ApplyError::EventBeforePollCreate);
        }

        let current = existing_lifecycle.unwrap_or(Lifecycle::Draft);
        let next = next_lifecycle(current, event.kind, event.tier)
            .map_err(|_| ApplyError::IllegalTransition)?;

        if let Some(state) = self.polls.get_mut(&poll_id) {
            state.meta.lifecycle = next;
            state.events.push(event.clone());
        } else if event.kind == PollEventKindCode::PollCreate {
            // PollCreate dispatch: Tier 1 (Approval) decodes Tier1PollConfig
            // and seeds a `Tier1(Tier1TallyState)` tier_state. Tier 2
            // (Conviction) decodes Tier2PollConfig, computes total_supply
            // from the caller-supplied snapshot (filtered by the
            // config's Eligibility), and seeds a `Tier2(Tier2ProposalState)`.
            // Tier 3 (Sortition) decodes Tier3PollConfigPayload, validates it,
            // checks retry_of predecessor (if any), and seeds a
            // `Tier3(Tier3PollState)` with the supplied electorate snapshot.
            let (meta, tier1_cfg, tier_state) = if event.tier == Tier::Approval {
                let cfg: Tier1PollConfig = ciborium::de::from_reader(&event.payload[..])
                    .map_err(|_| ApplyError::PayloadDecode)?;
                validate_poll_config(&cfg).map_err(|_| ApplyError::PayloadValidate)?;
                let closes_at = Hlc {
                    wall_ms: event.hlc.wall_ms + (cfg.window_seconds as u64 * 1000),
                    logical: 0,
                    device_id: event.hlc.device_id.clone(),
                };
                let meta = PollMeta {
                    poll_id,
                    community_id: *community_id,
                    creator: event.actor,
                    tier: event.tier,
                    eligibility: cfg.eligibility,
                    lifecycle: next,
                    created_at: event.hlc.clone(),
                    opens_at: event.hlc.clone(),
                    closes_at,
                    extends_at: None,
                    channel_id: Some(cfg.channel_id),
                    finalized_at_ms: None,
                };
                let tally = Tier1TallyState::empty(cfg.options.len());
                (meta, Some(cfg), TierState::Tier1(tally))
            } else if event.tier == Tier::Conviction {
                let cfg: Tier2PollConfig = ciborium::de::from_reader(&event.payload[..])
                    .map_err(|_| ApplyError::PayloadDecode)?;
                // Defensive apply-layer validation (CR R6 nit): peer-
                // received configs bypass the IPC layer's invariant
                // check. Inverted thresholds (T_min > T_max) flip the
                // band sign, producing an "easier-to-finalize-at-low-
                // participation" curve. A zero half-life would
                // degenerate the conviction math. Mirrors the Tier 1
                // `validate_poll_config` discipline that runs above.
                if cfg.threshold_min_q32 > cfg.threshold_max_q32 || cfg.half_life_seconds == 0 {
                    return Err(ApplyError::PayloadValidate);
                }
                // total_supply = count of members in the caller-supplied
                // snapshot who pass the Tier 2 config's Eligibility. If
                // no snapshot was supplied (peer-received PollCreate path
                // pending Task 12 wiring), default to the snapshot member
                // count or 0; downstream `Tier2ProposalState` guards
                // against total_supply=0 in `threshold_conviction_at`.
                let total_supply = if let Some(snap) = &snapshot {
                    snap.members
                        .iter()
                        .filter(|(addr, _)| {
                            crate::community_voting_core::check_eligibility(
                                snap,
                                addr,
                                &cfg.eligibility,
                            )
                            .is_ok()
                        })
                        .count() as u32
                } else {
                    0
                };
                let meta = PollMeta {
                    poll_id,
                    community_id: *community_id,
                    creator: event.actor,
                    tier: event.tier,
                    eligibility: cfg.eligibility,
                    lifecycle: next,
                    created_at: event.hlc.clone(),
                    opens_at: event.hlc.clone(),
                    // Tier 2 polls have no fixed close window — they
                    // finalize via threshold-cross + 24h contestability.
                    // Mirror created_at as a benign default; the tick
                    // never reads this for Tier 2.
                    closes_at: event.hlc.clone(),
                    extends_at: None,
                    channel_id: None,
                    finalized_at_ms: None,
                };
                let proposal_state = Tier2ProposalState::new(cfg, total_supply);
                (meta, None, TierState::Tier2(proposal_state))
            } else {
                // Tier 3 (Sortition) PollCreate: decode Tier3PollConfigPayload,
                // validate it, check retry_of predecessor, build Tier3PollState.
                debug_assert_eq!(event.tier, Tier::Sortition);
                let cfg: Tier3PollConfigPayload = ciborium::de::from_reader(&event.payload[..])
                    .map_err(|_| ApplyError::PayloadDecode)?;
                crate::community_voting_tier3::validate_tier3_poll_config(&cfg)
                    .map_err(|_| ApplyError::PayloadValidate)?;

                // retry_of validation: if Some(prev_poll_id), the predecessor
                // must exist and be in Stage::Failed. Predecessor may be in this
                // log (local) or will be wired in Task 12 (peer-received path).
                // For Phase 4a-main we validate against the local log only.
                //
                // TODO(ZEB-309 Phase 4b): out-of-order arrival — if the retry
                // event arrives before its predecessor (e.g. gossip reorder), the
                // apply currently returns `RetryOfPollNotFound` and the event is
                // permanently lost. The correct fix is a pending-retries map:
                //   `pending_retries: HashMap<PollId, Vec<VotingEvent>>` keyed by
                //   `retry_of` predecessor id. When a poll reaches Stage::Failed,
                //   drain any pending retries for it and re-apply in HLC order.
                // Phase 4a conservative stance: Phase 4a topology is single-node
                // (no peer gossip yet); the predecessor always lands first via the
                // initiating node. Peer-received ordering becomes relevant when
                // Task 12 (peer-received path) is wired in Phase 4b.
                if let Some(prev_id) = cfg.retry_of {
                    let prev_state = self
                        .polls
                        .get(&prev_id)
                        .ok_or(ApplyError::RetryOfPollNotFound)?;
                    // Check that the predecessor is a Tier 3 poll in Failed stage.
                    let prev_t3 = prev_state
                        .tier_state
                        .as_tier3()
                        .ok_or(ApplyError::RetryOfPollNotTier3)?;
                    if prev_t3.stage != crate::community_voting_tier3::Stage::Failed {
                        return Err(ApplyError::RetryOfPollNotFailed);
                    }
                }

                // Derive the poll_create_event_hash from signing bytes.
                let sb = event
                    .signing_bytes()
                    .map_err(|_| ApplyError::SigningBytesError)?;
                let mut hasher = sha2::Sha256::new();
                hasher.update(&sb);
                let poll_create_event_hash: [u8; 32] = hasher.finalize().into();

                // Electorate snapshot: callers pass it via `snapshot`. For peer-
                // received PollCreate events pending Task 12, default to empty.
                // The verify layer (Task 6) and engine (Task 10) supply the real
                // snapshot for locally-originated events.
                //
                // Cluster 3 fix (Qodo bug #2): filter by eligibility predicate
                // so only members who pass `check_eligibility` are included.
                // Without filtering, ineligible members could appear in the
                // snapshot and pass `verify_ratification_ballot`'s authz check.
                //
                // Cluster C fix (CodeRabbit major, R2 bot review): HashMap iteration is
                // non-deterministic. Sorting via canonical_electorate_order (OwnerAddr lex ASC)
                // guarantees that both engines derive identical eligible_electorate_snapshot from
                // the same beacon, so fisher_yates_select produces identical SortitionResult.
                let eligible_electorate_snapshot: Vec<crate::owner_state_types::OwnerAddr> =
                    snapshot
                        .as_ref()
                        .map(|snap| {
                            let filtered: Vec<_> = snap
                                .members
                                .keys()
                                .copied()
                                .filter(|addr| {
                                    crate::community_voting_core::check_eligibility(
                                        snap,
                                        addr,
                                        &cfg.eligibility,
                                    )
                                    .is_ok()
                                })
                                .collect();
                            canonical_electorate_order(&filtered)
                        })
                        .unwrap_or_default();

                let tier3_meta = Tier3PollMeta {
                    poll_id,
                    proposer: event.actor,
                    poll_create_hlc: event.hlc.clone(),
                    config: cfg.clone(),
                    poll_create_event_hash,
                    // community_epoch is set to 0 here; the engine calls
                    // `set_tier3_poll_epoch` immediately after apply to store
                    // the real epoch read from DfrostLogRegistry (Cluster 1 fix).
                    community_epoch: 0,
                };
                let tier3_state =
                    Tier3PollState::new_from_create(tier3_meta, eligible_electorate_snapshot);

                let meta = PollMeta {
                    poll_id,
                    community_id: *community_id,
                    creator: event.actor,
                    tier: event.tier,
                    eligibility: cfg.eligibility,
                    lifecycle: next,
                    created_at: event.hlc.clone(),
                    opens_at: event.hlc.clone(),
                    // Tier 3 has a ratification window; closes_at is set to
                    // `opens_at + dw + fw + rw` for display. The engine tick
                    // does not use closes_at for Tier 3 (it uses Stage watermarks).
                    closes_at: Hlc {
                        wall_ms: event.hlc.wall_ms
                            + (cfg.deliberation_window_seconds as u64 * 1000)
                            + (cfg.drafting_window_seconds as u64 * 1000)
                            + (cfg.ratification_window_seconds as u64 * 1000),
                        logical: 0,
                        device_id: event.hlc.device_id.clone(),
                    },
                    extends_at: None,
                    channel_id: None,
                    finalized_at_ms: None,
                };
                (meta, None, TierState::Tier3(Box::new(tier3_state)))
            };
            // Snapshot is only meaningful for Tier 1 in Phase 1; other
            // tiers have their own eligibility paths and we discard.
            let tier1_snapshot = if event.tier == Tier::Approval {
                snapshot
            } else {
                None
            };
            self.polls.insert(
                poll_id,
                PollState {
                    meta,
                    events: vec![event.clone()],
                    tier_state,
                    tier1_cfg,
                    tier1_snapshot,
                },
            );
        } else {
            return Err(ApplyError::EventBeforePollCreate);
        }

        self.events.push(event);
        Ok(poll_id)
    }

    /// Patch the `community_epoch` on an existing Tier 3 poll.
    ///
    /// Called by `VotingLogEngine::publish_event` immediately after a successful
    /// Tier 3 PollCreate apply to store the real D-FROST epoch read from
    /// `DfrostLogRegistry` (Cluster 1 fix, R1 bot review).
    ///
    /// Reading epoch BEFORE apply and storing here is atomic within a single
    /// engine task — there is no TOCTOU risk for `community_epoch` because the
    /// epoch is only used as a seed input to `derive_beacon_seed`, and the beacon
    /// request is issued after this call with the value that was read once.
    ///
    /// Returns `false` if the poll does not exist or is not Tier 3 (no-op).
    pub fn set_tier3_poll_epoch(&mut self, poll_id: &PollId, epoch: u64) -> bool {
        match self.polls.get_mut(poll_id) {
            Some(ps) => match ps.tier_state.as_tier3_mut() {
                Some(t3) => {
                    t3.meta.community_epoch = epoch;
                    true
                }
                None => false,
            },
            None => false,
        }
    }
}

/// Decode a `{ "pi": <PollId> }` map from `pd` bytes. Used by all
/// non-PollCreate events to identify which poll they belong to.
///
/// `pub(crate)` so the engine's `previous_stage_for_emit` snapshot path
/// (in both `publish_event` and `process_inbound_dispatch`) can resolve
/// the affected poll using the same logic as `apply_with_snapshot` —
/// signing-bytes-derivation only matches for `PollCreate` and gives the
/// wrong PollId for every other Tier 3 event kind (Qodo R1).
pub(crate) fn decode_poll_id_ref(pd: &[u8]) -> Option<PollId> {
    #[derive(serde::Deserialize)]
    struct Ref {
        #[serde(rename = "pi")]
        pi: PollId,
    }
    ciborium::de::from_reader::<Ref, _>(pd).ok().map(|r| r.pi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::ChannelId;
    use crate::community_voting_approval::Tier1PollConfig;
    use crate::community_voting_core::Eligibility;
    use crate::owner_state_types::OwnerAddr;
    use crate::owner_state_types::SpaceId;

    fn signing_bytes_of(ev: &SignedVotingEvent) -> Vec<u8> {
        ev.signing_bytes().expect("signing bytes")
    }

    fn good_poll_config() -> Tier1PollConfig {
        Tier1PollConfig {
            options: vec!["A".into(), "B".into(), "C".into()],
            window_seconds: 3600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            channel_id: ChannelId([0x11; 16]),
        }
    }

    fn poll_create_event(creator: OwnerAddr) -> SignedVotingEvent {
        let mut payload = Vec::new();
        ciborium::into_writer(&good_poll_config(), &mut payload).expect("encode cfg");
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind: PollEventKindCode::PollCreate,
            hlc: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "a".into(),
            },
            actor: creator,
            payload,
            sig: vec![0u8; 64],
        }
    }

    #[derive(serde::Serialize)]
    struct PollIdRefHelper {
        #[serde(rename = "pi")]
        pi: PollId,
    }

    fn ballot_event(poll_id: PollId, hlc_ms: u64, voter: OwnerAddr) -> SignedVotingEvent {
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&PollIdRefHelper { pi: poll_id }, &mut payload).unwrap();
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind: PollEventKindCode::BallotCast,
            hlc: Hlc {
                wall_ms: hlc_ms,
                logical: 0,
                device_id: "a".into(),
            },
            actor: voter,
            payload,
            sig: vec![0u8; 64],
        }
    }

    #[test]
    fn apply_poll_create_inserts_state() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x11; 16]);
        let ev = poll_create_event(OwnerAddr([0xaa; 16]));
        let pid = log.apply(ev.clone(), &cid).expect("apply");

        let expected_pid = derive_poll_id(&cid, &signing_bytes_of(&ev));
        assert_eq!(pid, expected_pid);
        assert_eq!(log.polls.len(), 1);
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Open);
    }

    #[test]
    fn apply_ballot_before_create_rejected() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x22; 16]);
        let phantom_pid = PollId([0x99; 32]);
        let ev = ballot_event(phantom_pid, 2000, OwnerAddr([0xbb; 16]));
        assert_eq!(log.apply(ev, &cid), Err(ApplyError::EventBeforePollCreate));
    }

    #[test]
    fn apply_ballot_against_existing_poll_appended() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x33; 16]);
        let create_ev = poll_create_event(OwnerAddr([0xaa; 16]));
        let pid = log.apply(create_ev, &cid).expect("apply create");

        let ballot = ballot_event(pid, 2000, OwnerAddr([0xbb; 16]));
        log.apply(ballot, &cid).expect("apply ballot");

        assert_eq!(log.polls[&pid].events.len(), 2);
    }

    // ────────────────────────────────────────────────────────────────────
    // Tier 2 apply-path tests (ZEB-291 Task 9)
    // ────────────────────────────────────────────────────────────────────

    use crate::community_voting_conviction::{
        AutoExecAction, DelegatePayload, SignalPayload, Tier2PollConfig, UndelegatePayload, Q32,
    };
    use crate::community_voting_core::{MemberAttrs, MembershipSnapshot};

    fn tier2_config() -> Tier2PollConfig {
        Tier2PollConfig {
            proposal_text: "promote".into(),
            half_life_seconds: 86_400,
            threshold_min_q32: Q32,
            threshold_max_q32: 100 * Q32,
            beta: 2,
            delegation_allowed: true,
            auto_exec: AutoExecAction::None,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
        }
    }

    fn tier2_poll_create_event(creator: OwnerAddr) -> SignedVotingEvent {
        let mut payload = Vec::new();
        ciborium::into_writer(&tier2_config(), &mut payload).expect("encode tier2 cfg");
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Conviction,
            kind: PollEventKindCode::PollCreate,
            hlc: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "a".into(),
            },
            actor: creator,
            payload,
            sig: vec![0u8; 64],
        }
    }

    fn signal_event(
        poll_id: PollId,
        actor: OwnerAddr,
        support: bool,
        hlc_ms: u64,
    ) -> SignedVotingEvent {
        let payload_obj = SignalPayload {
            proposal_id: poll_id,
            support,
        };
        let mut payload = Vec::new();
        ciborium::into_writer(&payload_obj, &mut payload).expect("encode signal");
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Conviction,
            kind: PollEventKindCode::Signal,
            hlc: Hlc {
                wall_ms: hlc_ms,
                logical: 0,
                device_id: "a".into(),
            },
            actor,
            payload,
            sig: vec![0u8; 64],
        }
    }

    fn delegate_event(actor: OwnerAddr, to: [u8; 16], hlc_ms: u64) -> SignedVotingEvent {
        // Wire `to` is the 16-byte OwnerAddr.
        let payload_obj = DelegatePayload {
            to: OwnerAddr(to),
            scope: "all".into(),
        };
        let mut payload = Vec::new();
        ciborium::into_writer(&payload_obj, &mut payload).expect("encode delegate");
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Conviction,
            kind: PollEventKindCode::Delegate,
            hlc: Hlc {
                wall_ms: hlc_ms,
                logical: 0,
                device_id: "a".into(),
            },
            actor,
            payload,
            sig: vec![0u8; 64],
        }
    }

    fn undelegate_event(actor: OwnerAddr, hlc_ms: u64) -> SignedVotingEvent {
        let payload_obj = UndelegatePayload {};
        let mut payload = Vec::new();
        ciborium::into_writer(&payload_obj, &mut payload).expect("encode undelegate");
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Conviction,
            kind: PollEventKindCode::Undelegate,
            hlc: Hlc {
                wall_ms: hlc_ms,
                logical: 0,
                device_id: "a".into(),
            },
            actor,
            payload,
            sig: vec![0u8; 64],
        }
    }

    fn snapshot_of(addrs: &[OwnerAddr]) -> MembershipSnapshot {
        let mut members = HashMap::new();
        for a in addrs {
            members.insert(
                *a,
                MemberAttrs {
                    power: 10,
                    vouching_depth: 0,
                },
            );
        }
        MembershipSnapshot { members }
    }

    #[test]
    fn tier2_pollcreate_creates_tier2_state() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x55; 16]);
        let creator = OwnerAddr([0xaa; 16]);
        let voter1 = OwnerAddr([0xb1; 16]);
        let voter2 = OwnerAddr([0xb2; 16]);
        let ev = tier2_poll_create_event(creator);
        let pid = log
            .apply_with_snapshot(ev, &cid, Some(snapshot_of(&[creator, voter1, voter2])))
            .expect("apply tier2 create");
        let state = &log.polls[&pid];
        assert_eq!(state.meta.lifecycle, Lifecycle::Open);
        assert_eq!(state.meta.tier, Tier::Conviction);
        let t2 = state.tier_state.as_tier2().expect("tier2 state");
        assert_eq!(t2.total_supply, 3);
        assert!(t2.per_voter.is_empty());
    }

    #[test]
    fn tier2_signal_updates_voter_state() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x55; 16]);
        let creator = OwnerAddr([0xaa; 16]);
        let voter = OwnerAddr([0xb1; 16]);
        let pid = log
            .apply_with_snapshot(
                tier2_poll_create_event(creator),
                &cid,
                Some(snapshot_of(&[creator, voter])),
            )
            .expect("create");
        log.apply(signal_event(pid, voter, true, 2000), &cid)
            .expect("signal");
        let t2 = log.polls[&pid].tier_state.as_tier2().unwrap();
        let v = t2.per_voter.get(&voter).expect("voter state");
        assert!(v.is_supporting);
        assert_eq!(v.support_started_at_ms, 2000);
        // Lifecycle stays Open — Signal does NOT drive lifecycle (Task 15
        // tick owns that path).
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Open);
    }

    #[test]
    fn tier2_signal_toggle_on_off_accumulates_conviction() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x55; 16]);
        let creator = OwnerAddr([0xaa; 16]);
        let voter = OwnerAddr([0xb1; 16]);
        let pid = log
            .apply_with_snapshot(
                tier2_poll_create_event(creator),
                &cid,
                Some(snapshot_of(&[creator, voter])),
            )
            .expect("create");
        // Signal on at t=1_000_000, off at t=1_086_400_000 (=24h later, == 1 half-life).
        log.apply(signal_event(pid, voter, true, 1_000_000), &cid)
            .expect("on");
        log.apply(
            signal_event(pid, voter, false, 1_000_000 + 86_400_000),
            &cid,
        )
        .expect("off");
        let v = log.polls[&pid]
            .tier_state
            .as_tier2()
            .unwrap()
            .per_voter
            .get(&voter)
            .unwrap();
        assert!(!v.is_supporting);
        // After one half-life of continuous support, accumulated conviction
        // is the charge function value — strictly positive.
        assert!(
            v.accumulated_conviction_q32 > 0,
            "accumulated conviction must be > 0 after full support session, got {}",
            v.accumulated_conviction_q32
        );
    }

    #[test]
    fn tier2_delegate_updates_delegation_graph() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x55; 16]);
        let creator = OwnerAddr([0xaa; 16]);
        let alice = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        log.apply_with_snapshot(
            tier2_poll_create_event(creator),
            &cid,
            Some(snapshot_of(&[creator, alice, bob])),
        )
        .expect("create");
        log.apply(delegate_event(alice, bob.0, 2000), &cid)
            .expect("delegate");
        assert_eq!(log.delegation_graph.delegate_of(alice), Some(bob));
        assert_eq!(log.delegation_graph.delegator_count(bob), 1);
    }

    #[test]
    fn tier2_delegate_cycle_rejected() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x55; 16]);
        let creator = OwnerAddr([0xaa; 16]);
        let alice = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        log.apply_with_snapshot(
            tier2_poll_create_event(creator),
            &cid,
            Some(snapshot_of(&[creator, alice, bob])),
        )
        .expect("create");
        // alice → bob succeeds; bob → alice would close a cycle and is
        // rejected by DelegationGraph::apply_delegate.
        log.apply(delegate_event(alice, bob.0, 2000), &cid)
            .expect("alice → bob");
        let err = log
            .apply(delegate_event(bob, alice.0, 3000), &cid)
            .expect_err("bob → alice must be rejected");
        assert_eq!(err, ApplyError::DelegationRejected);
        assert_eq!(log.delegation_graph.delegate_of(bob), None);
    }

    #[test]
    fn tier2_undelegate_clears_edge() {
        let mut log = VotingLog::new();
        let cid = SpaceId([0x55; 16]);
        let creator = OwnerAddr([0xaa; 16]);
        let alice = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        log.apply_with_snapshot(
            tier2_poll_create_event(creator),
            &cid,
            Some(snapshot_of(&[creator, alice, bob])),
        )
        .expect("create");
        log.apply(delegate_event(alice, bob.0, 2000), &cid)
            .expect("delegate");
        assert_eq!(log.delegation_graph.delegate_of(alice), Some(bob));
        log.apply(undelegate_event(alice, 3000), &cid)
            .expect("undelegate");
        assert_eq!(log.delegation_graph.delegate_of(alice), None);
    }

    #[test]
    fn tier1_apply_path_still_works() {
        // Regression guard for Task 9's TierState extension: Tier 1
        // PollCreate + BallotCast must still flow through and seed a
        // `Tier1(Tier1TallyState)` instead of the old `Empty` variant.
        let mut log = VotingLog::new();
        let cid = SpaceId([0x33; 16]);
        let create_ev = poll_create_event(OwnerAddr([0xaa; 16]));
        let pid = log.apply(create_ev, &cid).expect("apply create");
        let ballot = ballot_event(pid, 2000, OwnerAddr([0xbb; 16]));
        log.apply(ballot, &cid).expect("apply ballot");
        let state = &log.polls[&pid];
        assert_eq!(state.events.len(), 2);
        assert_eq!(state.meta.lifecycle, Lifecycle::Open);
        let t1 = state.tier_state.as_tier1().expect("tier1 state");
        // good_poll_config() has 3 options.
        assert_eq!(t1.counts.len(), 3);
    }
}

const NINETY_DAYS_MS: u64 = 90 * 24 * 60 * 60 * 1000;

impl VotingLog {
    /// Sweep polls finalized > 90 days ago (per spec §2). Drop per-ballot
    /// events but retain `PollCreate` + `PollResult` so the audit record
    /// stays intact forever. Transition lifecycle to `Archived`.
    /// Idempotent. Returns the `PollId`s that were archived this sweep.
    ///
    /// Caller responsibility (deferred to a follow-up that wires this into
    /// the periodic tick in `lib.rs`): invoke daily across every entry in
    /// `NodeState.voting_logs`.
    pub fn archive_finalized_polls(&mut self, now_wall_ms: u64) -> Vec<PollId> {
        let mut archived = Vec::new();
        // Collect the set of (poll_id) to archive in a first pass so we can
        // also rewrite the top-level `events` vector below without holding
        // a mutable borrow on `self.polls`.
        let mut to_archive: Vec<PollId> = Vec::new();
        for (pid, state) in self.polls.iter() {
            if state.meta.lifecycle != Lifecycle::Finalized {
                continue;
            }
            // Tier 1 emits a terminal `PollResult` event whose HLC is the
            // canonical finalize timestamp. Tier 2 has no terminal event
            // (the tick flips lifecycle directly), so we stamp
            // `meta.finalized_at_ms` on the lifecycle transition and
            // consult that here as a fallback. Without the fallback,
            // Tier 2 finalized polls would never archive — CR R3 Major.
            let fin_at = state
                .events
                .iter()
                .find(|e| e.kind == PollEventKindCode::PollResult)
                .map(|e| e.hlc.wall_ms)
                .or(state.meta.finalized_at_ms);
            let Some(fin_at) = fin_at else { continue };
            if now_wall_ms.saturating_sub(fin_at) > NINETY_DAYS_MS {
                to_archive.push(*pid);
            }
        }

        if to_archive.is_empty() {
            return archived;
        }
        let archive_set: std::collections::HashSet<PollId> = to_archive.iter().copied().collect();

        // Per-poll retain + lifecycle transition.
        for pid in &to_archive {
            if let Some(state) = self.polls.get_mut(pid) {
                state.events.retain(|e| {
                    matches!(
                        e.kind,
                        PollEventKindCode::PollCreate | PollEventKindCode::PollResult
                    )
                });
                state.meta.lifecycle = Lifecycle::Archived;
                archived.push(*pid);
            }
        }

        // Top-level events vector also needs to drop the same ballots
        // (apply pushes into both locations; without this, the global
        // log grows unboundedly even after archival — spec §2 says
        // the archive sweep bounds disk use for a community's lifetime).
        // Cursor #130 round-3 catch.
        self.events.retain(|ev| {
            // PollCreate of an archived poll is always retained (audit);
            // we can't easily re-derive its PollId here without the
            // community_id, but we don't need to — the per-poll retain
            // above kept the PollCreate for archived polls too, and
            // dropping a PollCreate would break R2 reproducibility on
            // the still-archived PollResult.
            if ev.kind == PollEventKindCode::PollCreate {
                return true;
            }
            // Non-create events carry their poll-id reference in the
            // payload. If the poll is in the archive set, retain only
            // PollResult; otherwise (active poll, or undecodable payload
            // we're being defensive about), keep the event.
            let Some(pid) = decode_poll_id_ref(&ev.payload) else {
                return true;
            };
            if !archive_set.contains(&pid) {
                return true;
            }
            ev.kind == PollEventKindCode::PollResult
        });

        archived
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;
    use crate::community_voting_core::{Eligibility, Tier};
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    fn make_event(kind: PollEventKindCode, wall_ms: u64) -> SignedVotingEvent {
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind,
            hlc: Hlc {
                wall_ms,
                logical: 0,
                device_id: "a".into(),
            },
            actor: OwnerAddr([0xaa; 16]),
            payload: vec![],
            sig: vec![0u8; 64],
        }
    }

    /// Build a PollState by direct construction — bypasses the full
    /// signed-event chain so archive tests stay focused on the sweep
    /// semantics rather than re-exercising every other layer.
    fn make_finalized_log(finalized_at_ms: u64, n_ballots: usize) -> (VotingLog, PollId) {
        let mut log = VotingLog::new();
        let pid = PollId([0x77; 32]);
        let cid = SpaceId([0xcc; 16]);
        let create_ev = make_event(PollEventKindCode::PollCreate, 0);
        let result_ev = make_event(PollEventKindCode::PollResult, finalized_at_ms);
        let mut events = vec![create_ev.clone()];
        for i in 0..n_ballots {
            events.push(make_event(
                PollEventKindCode::BallotCast,
                (i as u64 + 1) * 100,
            ));
        }
        events.push(make_event(
            PollEventKindCode::PollClose,
            finalized_at_ms.saturating_sub(1),
        ));
        events.push(result_ev);
        let meta = PollMeta {
            poll_id: pid,
            community_id: cid,
            creator: OwnerAddr([0xaa; 16]),
            tier: Tier::Approval,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            lifecycle: Lifecycle::Finalized,
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "a".into(),
            },
            opens_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "a".into(),
            },
            closes_at: Hlc {
                wall_ms: finalized_at_ms,
                logical: 0,
                device_id: "a".into(),
            },
            extends_at: None,
            channel_id: None,
            finalized_at_ms: None,
        };
        log.polls.insert(
            pid,
            PollState {
                meta,
                events,
                tier_state: TierState::Tier1(Tier1TallyState::empty(0)),
                tier1_cfg: None,
                tier1_snapshot: None,
            },
        );
        (log, pid)
    }

    #[test]
    fn old_finalized_poll_archived() {
        let (mut log, pid) = make_finalized_log(0, 10);
        let now_ms = 91 * 24 * 60 * 60 * 1000;
        let archived = log.archive_finalized_polls(now_ms);
        assert_eq!(archived, vec![pid]);
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Archived);
        assert_eq!(log.polls[&pid].events.len(), 2);
    }

    #[test]
    fn archive_sweep_prunes_top_level_events_vector() {
        // Build a Finalized poll with real `{ "pi": PollId }` payloads
        // on non-create events so `decode_poll_id_ref` can route them
        // in the top-level prune step. The shape-less make_event helper
        // used by the other tests produces empty payloads; defensively
        // those just stay in `log.events`, which is the safe-but-no-op
        // path of the prune. This test exercises the actually-prunes
        // path Cursor flagged on PR #130.
        #[derive(serde::Serialize)]
        struct PiRef {
            #[serde(rename = "pi")]
            pi: PollId,
        }
        let pid = PollId([0x99; 32]);
        let mk = |kind: PollEventKindCode, wall_ms: u64| -> SignedVotingEvent {
            let payload = if matches!(kind, PollEventKindCode::PollCreate) {
                vec![]
            } else {
                let mut buf = Vec::new();
                ciborium::into_writer(&PiRef { pi: pid }, &mut buf).unwrap();
                buf
            };
            SignedVotingEvent {
                tag: 'p',
                version: 1,
                tier: Tier::Approval,
                kind,
                hlc: Hlc {
                    wall_ms,
                    logical: 0,
                    device_id: "a".into(),
                },
                actor: OwnerAddr([0xaa; 16]),
                payload,
                sig: vec![0u8; 64],
            }
        };

        let create_ev = mk(PollEventKindCode::PollCreate, 0);
        let close_ev = mk(PollEventKindCode::PollClose, 200);
        let result_ev = mk(PollEventKindCode::PollResult, 300);
        let ballots: Vec<SignedVotingEvent> = (0..5)
            .map(|i| mk(PollEventKindCode::BallotCast, 100 + i))
            .collect();

        let mut log = VotingLog::new();
        log.events.push(create_ev.clone());
        for b in &ballots {
            log.events.push(b.clone());
        }
        log.events.push(close_ev.clone());
        log.events.push(result_ev.clone());

        let mut per_poll_events = vec![create_ev.clone()];
        per_poll_events.extend(ballots.iter().cloned());
        per_poll_events.push(close_ev);
        per_poll_events.push(result_ev);

        let meta = PollMeta {
            poll_id: pid,
            community_id: SpaceId([0xcc; 16]),
            creator: OwnerAddr([0xaa; 16]),
            tier: Tier::Approval,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            lifecycle: Lifecycle::Finalized,
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "a".into(),
            },
            opens_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "a".into(),
            },
            closes_at: Hlc {
                wall_ms: 300,
                logical: 0,
                device_id: "a".into(),
            },
            extends_at: None,
            channel_id: None,
            finalized_at_ms: None,
        };
        log.polls.insert(
            pid,
            PollState {
                meta,
                events: per_poll_events,
                tier_state: TierState::Tier1(Tier1TallyState::empty(0)),
                tier1_cfg: None,
                tier1_snapshot: None,
            },
        );

        let before_total = log.events.len();
        assert_eq!(
            before_total, 8,
            "8 events: create + 5 ballots + close + result"
        );

        let now_ms = 91 * 24 * 60 * 60 * 1000;
        let archived = log.archive_finalized_polls(now_ms);
        assert_eq!(archived, vec![pid]);
        assert_eq!(
            log.events.len(),
            2,
            "top-level events vector pruned to PollCreate + PollResult"
        );
        assert_eq!(
            log.events
                .iter()
                .filter(|e| e.kind == PollEventKindCode::PollCreate)
                .count(),
            1
        );
        assert_eq!(
            log.events
                .iter()
                .filter(|e| e.kind == PollEventKindCode::PollResult)
                .count(),
            1
        );
    }

    #[test]
    fn young_finalized_poll_kept() {
        let (mut log, pid) = make_finalized_log(0, 10);
        let now_ms = 89 * 24 * 60 * 60 * 1000;
        let archived = log.archive_finalized_polls(now_ms);
        assert!(archived.is_empty());
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Finalized);
    }

    #[test]
    fn archive_is_idempotent() {
        let (mut log, _pid) = make_finalized_log(0, 10);
        let now_ms = 100 * 24 * 60 * 60 * 1000;
        let first = log.archive_finalized_polls(now_ms);
        let second = log.archive_finalized_polls(now_ms);
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
    }

    #[test]
    fn open_poll_not_archived() {
        let mut log = VotingLog::new();
        let pid = PollId([0x88; 32]);
        let meta = PollMeta {
            poll_id: pid,
            community_id: SpaceId([0xcc; 16]),
            creator: OwnerAddr([0xaa; 16]),
            tier: Tier::Approval,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            lifecycle: Lifecycle::Open,
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "a".into(),
            },
            opens_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "a".into(),
            },
            closes_at: Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: "a".into(),
            },
            extends_at: None,
            channel_id: None,
            finalized_at_ms: None,
        };
        log.polls.insert(
            pid,
            PollState {
                meta,
                events: vec![],
                tier_state: TierState::Tier1(Tier1TallyState::empty(0)),
                tier1_cfg: None,
                tier1_snapshot: None,
            },
        );
        let archived = log.archive_finalized_polls(999 * 24 * 60 * 60 * 1000);
        assert!(archived.is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────────────────
// Task 8: Tier 3 dispatch smoke tests
// ────────────────────────────────────────────────────────────────────────────────
//
// These tests verify that Tier 3 event kinds route correctly through
// VotingLog::apply (the dispatch surface) and that the resulting Tier3PollState
// holds the expected materialized state. Tier 1 and Tier 2 regression guards
// are also included.
//
// verify_ss is async and needs a BeaconOracle. The NoBeaconOracle stub always
// returns BeaconNotYetAvailable — that path is exercised in
// `dispatch_tier3_kd_ss_without_beacon_returns_beacon_not_yet_available` below
// directly via the verify_ss function (not via dispatch). The apply path for
// kd=ss (materialize only) is exercised via Tier3PollState::apply_event, which
// is what the dispatch routes to.
#[cfg(test)]
mod tier3_dispatch_tests {
    use super::*;
    use crate::community_voting_core::{
        DeliberationStatementPayload, DeliberationVotePayload, Eligibility,
        MiniPublicDeclinePayload, SortitionFailedPayload, SortitionSelectionPayload, Tier,
        Tier3PollConfigPayload,
    };
    use crate::community_voting_tier3::{NoBeaconOracle, Stage, VerifyError};
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    // Local helper for encoding a pi-keyed CBOR map (mirrors the one in `mod tests`).
    #[derive(serde::Serialize)]
    struct PollIdRefHelper {
        #[serde(rename = "pi")]
        pi: PollId,
    }

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

    fn cid() -> SpaceId {
        SpaceId([0xcc; 16])
    }

    fn encode<T: serde::Serialize>(v: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(v, &mut buf).expect("encode");
        buf
    }

    fn tier3_config() -> Tier3PollConfigPayload {
        Tier3PollConfigPayload {
            proposal_text: "Charter amendment".into(),
            sortition_size: 20,
            deliberation_window_seconds: 100,
            drafting_window_seconds: 100,
            ratification_window_seconds: 100,
            privacy_mode: "pu".into(),
            incentive_mode: "a".into(),
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            retry_of: None,
            predecessor: None,
        }
    }

    fn tier3_create_event(
        creator: OwnerAddr,
        config: &Tier3PollConfigPayload,
    ) -> SignedVotingEvent {
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::PollCreate,
            hlc: hlc(1000),
            actor: creator,
            payload: encode(config),
            sig: vec![0u8; 64],
        }
    }

    fn tier3_event_with_payload(
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

    // ── Test 1: Tier 3 PollCreate creates a Tier3PollState ──────────────────────

    #[test]
    fn dispatch_tier3_poll_create_creates_tier3_poll_state() {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cfg = tier3_config();
        let ev = tier3_create_event(creator, &cfg);
        let pid = log.apply(ev, &cid()).expect("apply tier3 create");

        let state = &log.polls[&pid];
        assert_eq!(state.meta.tier, Tier::Sortition);
        assert_eq!(state.meta.lifecycle, Lifecycle::Open);
        let t3 = state
            .tier_state
            .as_tier3()
            .expect("tier_state must be Tier3 variant");
        assert_eq!(t3.stage, Stage::Sortition);
        assert!(t3.sortition_result.is_none());
        assert!(t3.candidates.is_empty());
        assert!(t3.declines.is_empty());
    }

    // ── Test 2: kd=ss routes to Tier3PollState and sets sortition_result ────────

    #[test]
    fn dispatch_tier3_kd_ss_routes_to_tier3_poll_state() {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cfg = tier3_config();
        let create_ev = tier3_create_event(creator, &cfg);
        let pid = log.apply(create_ev, &cid()).expect("tier3 create");

        let primary = vec![addr(1), addr(2)];
        let backup = vec![addr(3), addr(4)];
        let ss_payload = SortitionSelectionPayload {
            poll_id: pid,
            primary: primary.clone(),
            backup: backup.clone(),
        };
        let ss_ev = tier3_event_with_payload(
            PollEventKindCode::SortitionSelection,
            2000,
            addr(0xfe),
            encode(&ss_payload),
        );
        log.apply(ss_ev, &cid()).expect("apply kd=ss");

        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        let sr = t3.sortition_result.as_ref().expect("sortition_result set");
        assert_eq!(sr.primary, primary);
        assert_eq!(sr.backup, backup);
    }

    // ── Test 3: verify_ss without oracle returns BeaconNotYetAvailable ──────────
    //
    // This test calls verify_ss directly (it's async) rather than through
    // dispatch (which is sync and does not call verify_ss — verify is a
    // separate concern from apply per the architecture).

    #[test]
    fn dispatch_tier3_kd_ss_without_beacon_returns_beacon_not_yet_available() {
        // verify_ss is async; run it in a sync test via block_on.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        rt.block_on(async {
            use crate::community_voting_tier3::{verify_ss, Tier3PollMeta, Tier3PollState};

            let pid = PollId([0x01; 32]);
            let meta = Tier3PollMeta {
                poll_id: pid,
                proposer: addr(0xff),
                poll_create_hlc: hlc(1000),
                config: Tier3PollConfigPayload {
                    proposal_text: "test".into(),
                    sortition_size: 5,
                    deliberation_window_seconds: 100,
                    drafting_window_seconds: 100,
                    ratification_window_seconds: 100,
                    privacy_mode: "pu".into(),
                    incentive_mode: "a".into(),
                    eligibility: Eligibility {
                        min_power: 0,
                        min_vouching_depth: None,
                        sortition_size: None,
                    },
                    retry_of: None,
                    predecessor: None,
                },
                poll_create_event_hash: [0xaa; 32],
                community_epoch: 0,
            };
            let electorate: Vec<OwnerAddr> = (0u8..10).map(|b| OwnerAddr([b; 16])).collect();
            let poll_state = Tier3PollState::new_from_create(meta, electorate.clone());

            let ss_payload = SortitionSelectionPayload {
                poll_id: pid,
                primary: vec![addr(0), addr(1)],
                backup: vec![addr(2), addr(3)],
            };
            let ss_ev = SignedVotingEvent {
                tag: 'p',
                version: 1,
                tier: Tier::Sortition,
                kind: PollEventKindCode::SortitionSelection,
                hlc: hlc(2000),
                actor: addr(0xfe),
                payload: encode(&ss_payload),
                sig: vec![0u8; 64],
            };

            let oracle = NoBeaconOracle;
            let community_id = cid();
            let result = verify_ss(&ss_ev, &poll_state, &oracle, &community_id).await;
            assert_eq!(result, Err(VerifyError::BeaconNotYetAvailable));
        });
    }

    // ── Test 4: kd=md routes to apply_event and appends to declines ─────────────

    #[test]
    fn dispatch_tier3_kd_md_routes_to_apply_event() {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cfg = tier3_config();
        let create_ev = tier3_create_event(creator, &cfg);
        let pid = log.apply(create_ev, &cid()).expect("tier3 create");

        // Apply kd=ss so sortition_result is set (not strictly needed for decline).
        let ss_payload = SortitionSelectionPayload {
            poll_id: pid,
            primary: vec![addr(1)],
            backup: vec![addr(2)],
        };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::SortitionSelection,
                1500,
                addr(0xfe),
                encode(&ss_payload),
            ),
            &cid(),
        )
        .expect("kd=ss");

        let decline_payload = MiniPublicDeclinePayload {
            poll_id: pid,
            reason: None,
        };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::MiniPublicDecline,
                2000,
                addr(1),
                encode(&decline_payload),
            ),
            &cid(),
        )
        .expect("kd=md");

        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        assert_eq!(t3.declines.len(), 1);
        assert_eq!(t3.declines[0].0, addr(1));
    }

    // ── Test 5: kd=sf transitions to Stage::Failed ───────────────────────────────

    #[test]
    fn dispatch_tier3_kd_sf_transitions_to_failed() {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cfg = tier3_config();
        let create_ev = tier3_create_event(creator, &cfg);
        let pid = log.apply(create_ev, &cid()).expect("tier3 create");

        let sf_payload = SortitionFailedPayload { poll_id: pid };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::SortitionFailed,
                2000,
                creator,
                encode(&sf_payload),
            ),
            &cid(),
        )
        .expect("kd=sf");

        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        assert_eq!(t3.stage, Stage::Failed);
    }

    // ── Test 5a: Cluster K regression — kd=rs sets lifecycle=Finalized ──────────
    //
    // After kd=rs is applied to a Tier 3 poll, state.meta.lifecycle must be
    // Lifecycle::Finalized (not stuck at Lifecycle::Open). This is required so
    // archive_finalized_polls() can identify and archive the poll.

    #[test]
    fn dispatch_tier3_kd_rs_syncs_lifecycle_to_finalized() {
        use crate::community_voting_star::{CandidateRef, StarResult};
        use crate::community_voting_tier3::{Stage, Tier3PollResultPayload};

        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cfg = tier3_config();
        let create_ev = tier3_create_event(creator, &cfg);
        let pid = log.apply(create_ev, &cid()).expect("tier3 create");

        // Lifecycle starts Open.
        assert_eq!(
            log.polls[&pid].meta.lifecycle,
            Lifecycle::Open,
            "lifecycle must start Open"
        );

        // Build a minimal Tier3PollResultPayload (apply doesn't re-verify tally).
        let dummy_hash = [0x42u8; 32];
        let dummy_candidate = CandidateRef {
            event_hash: dummy_hash,
            approval_count: 0,
        };
        let star_result = StarResult {
            winner: dummy_candidate.clone(),
            finalists: vec![dummy_candidate.clone()],
            total_scores: vec![0],
            runoff_votes: vec![1],
        };
        let rs_payload = Tier3PollResultPayload {
            poll_id: pid,
            result: star_result,
        };

        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::PollResult,
                5000,
                creator,
                encode(&rs_payload),
            ),
            &cid(),
        )
        .expect("kd=rs apply must succeed");

        // Cluster K fix: lifecycle must be synced to Finalized.
        assert_eq!(
            log.polls[&pid].meta.lifecycle,
            Lifecycle::Finalized,
            "lifecycle must be Finalized after kd=rs"
        );
        // Tier 3 stage also Finalized.
        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        assert_eq!(t3.stage, Stage::Finalized, "tier3 stage must be Finalized");
        // finalized_at_ms must be set (kd=rs hlc.wall_ms = 5000).
        assert_eq!(
            log.polls[&pid].meta.finalized_at_ms,
            Some(5000),
            "finalized_at_ms must be set to kd=rs event wall_ms"
        );
    }

    // ── Test 5b: Cluster K regression — kd=sf sets lifecycle=Closed ─────────────
    //
    // After kd=sf, lifecycle must be Closed (not Open). Failed polls cannot be
    // archived the same way as finalized ones, but they must leave the Open state.

    #[test]
    fn dispatch_tier3_kd_sf_syncs_lifecycle_to_closed() {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cfg = tier3_config();
        let create_ev = tier3_create_event(creator, &cfg);
        let pid = log.apply(create_ev, &cid()).expect("tier3 create");

        let sf_payload = SortitionFailedPayload { poll_id: pid };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::SortitionFailed,
                3000,
                creator,
                encode(&sf_payload),
            ),
            &cid(),
        )
        .expect("kd=sf");

        // Cluster K fix: lifecycle must be Closed (not Open) after failure.
        assert_eq!(
            log.polls[&pid].meta.lifecycle,
            Lifecycle::Closed,
            "lifecycle must be Closed after kd=sf (poll failed)"
        );
    }

    // ── Test 6: invalid kind for Tier 3 is rejected at dispatch ─────────────────

    #[test]
    fn dispatch_tier3_invalid_kind_for_tier3_rejected() {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cfg = tier3_config();
        let create_ev = tier3_create_event(creator, &cfg);
        let pid = log.apply(create_ev, &cid()).expect("tier3 create");

        // PollOpen is a Tier 1 only kind — invalid for Tier 3.
        let open_ev = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::PollOpen,
            hlc: hlc(2000),
            actor: creator,
            // kd=op payload is a { "pi": ... } ref — encode with the pid.
            payload: encode(&PollIdRefHelper { pi: pid }),
            sig: vec![0u8; 64],
        };
        let err = log
            .apply(open_ev, &cid())
            .expect_err("PollOpen on Tier3 must fail");
        assert_eq!(err, ApplyError::InvalidKindForTier3);
    }

    // ── Test 7: Tier 1 path still works after adding Tier 3 ─────────────────────

    #[test]
    fn dispatch_tier1_paths_unchanged() {
        let mut log = VotingLog::new();
        let cid = cid();
        let creator = addr(0xaa);

        // Use the Tier 1 helpers from the existing tests module.
        use crate::community_membership::ChannelId;
        use crate::community_voting_approval::Tier1PollConfig;

        let t1_cfg = Tier1PollConfig {
            options: vec!["A".into(), "B".into()],
            window_seconds: 3600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            channel_id: ChannelId([0x11; 16]),
        };
        let mut payload = Vec::new();
        ciborium::into_writer(&t1_cfg, &mut payload).expect("encode t1 cfg");

        let create_ev = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind: PollEventKindCode::PollCreate,
            hlc: hlc(1000),
            actor: creator,
            payload,
            sig: vec![0u8; 64],
        };
        let pid = log.apply(create_ev, &cid).expect("tier1 create");
        let state = &log.polls[&pid];
        assert_eq!(state.meta.tier, Tier::Approval);
        assert_eq!(state.meta.lifecycle, Lifecycle::Open);
        assert!(state.tier_state.as_tier1().is_some());
        assert!(state.tier_state.as_tier3().is_none());
    }

    // ── Test 8: retry_of with existing Failed poll accepted ──────────────────────

    #[test]
    fn dispatch_retry_of_existing_failed_poll_accepted() {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);

        // Create the first poll (predecessor).
        let cfg1 = tier3_config();
        let create_ev1 = tier3_create_event(creator, &cfg1);
        let prev_pid = log.apply(create_ev1, &cid()).expect("first create");

        // Apply kd=sf to fail it.
        let sf_payload = SortitionFailedPayload { poll_id: prev_pid };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::SortitionFailed,
                2000,
                creator,
                encode(&sf_payload),
            ),
            &cid(),
        )
        .expect("kd=sf");

        // Retry poll references the failed predecessor.
        let mut cfg2 = tier3_config();
        cfg2.retry_of = Some(prev_pid);
        let create_ev2 = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::PollCreate,
            hlc: hlc(3000),
            actor: creator,
            payload: encode(&cfg2),
            sig: vec![0u8; 64],
        };
        let retry_pid = log.apply(create_ev2, &cid()).expect("retry create");
        // Should be a different poll_id.
        assert_ne!(retry_pid, prev_pid);
        // The retry poll should be in Sortition stage.
        let t3 = log.polls[&retry_pid].tier_state.as_tier3().unwrap();
        assert_eq!(t3.stage, Stage::Sortition);
    }

    // ── Test 9: retry_of with nonexistent poll rejected ──────────────────────────

    #[test]
    fn dispatch_retry_of_nonexistent_poll_rejected() {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let phantom_pid = PollId([0x99; 32]);

        let mut cfg = tier3_config();
        cfg.retry_of = Some(phantom_pid);
        let create_ev = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::PollCreate,
            hlc: hlc(1000),
            actor: creator,
            payload: encode(&cfg),
            sig: vec![0u8; 64],
        };
        let err = log
            .apply(create_ev, &cid())
            .expect_err("retry_of nonexistent must fail");
        assert_eq!(err, ApplyError::RetryOfPollNotFound);
    }

    // ── Test 10: retry_of with non-failed poll rejected ───────────────────────────

    #[test]
    fn dispatch_retry_of_non_failed_poll_rejected() {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);

        // Create the predecessor poll but do NOT fail it (leave it in Sortition stage).
        let cfg1 = tier3_config();
        let create_ev1 = tier3_create_event(creator, &cfg1);
        let prev_pid = log.apply(create_ev1, &cid()).expect("first create");

        // Retry poll references the still-active predecessor.
        let mut cfg2 = tier3_config();
        cfg2.retry_of = Some(prev_pid);
        let create_ev2 = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::PollCreate,
            hlc: hlc(2000),
            actor: creator,
            payload: encode(&cfg2),
            sig: vec![0u8; 64],
        };
        let err = log
            .apply(create_ev2, &cid())
            .expect_err("retry_of non-failed must fail");
        assert_eq!(err, ApplyError::RetryOfPollNotFailed);
    }

    // ── Test 11: retry_of with non-Tier3 poll returns RetryOfPollNotTier3 (Cluster 10 nit) ──

    #[test]
    fn dispatch_retry_of_non_tier3_poll_returns_not_tier3() {
        use crate::community_membership::ChannelId;
        use crate::community_voting_approval::Tier1PollConfig;

        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cid = cid();

        // Create a Tier 1 poll to serve as the (wrong-tier) predecessor.
        let t1_cfg = Tier1PollConfig {
            options: vec!["A".into(), "B".into()],
            window_seconds: 3600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            channel_id: ChannelId([0x11; 16]),
        };
        let mut t1_payload = Vec::new();
        ciborium::into_writer(&t1_cfg, &mut t1_payload).expect("encode t1");
        let t1_create = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind: PollEventKindCode::PollCreate,
            hlc: hlc(1000),
            actor: creator,
            payload: t1_payload,
            sig: vec![0u8; 64],
        };
        let t1_pid = log.apply(t1_create, &cid).expect("t1 create");

        // Try a Tier 3 retry_of pointing at the Tier 1 poll.
        let mut cfg = tier3_config();
        cfg.retry_of = Some(t1_pid);
        let retry_ev = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::PollCreate,
            hlc: hlc(2000),
            actor: creator,
            payload: encode(&cfg),
            sig: vec![0u8; 64],
        };
        let err = log
            .apply(retry_ev, &cid)
            .expect_err("retry_of non-Tier3 must fail");
        assert_eq!(
            err,
            ApplyError::RetryOfPollNotTier3,
            "retry_of referencing a non-Tier3 poll must return RetryOfPollNotTier3"
        );
    }

    // ── Test 12 (Cluster G): retry_of out-of-order arrival — known limitation ──
    //
    // Verifies the current behaviour (RetryOfPollNotFound) and pins it so
    // any future implementation of the pending-retries map (see TODO in
    // apply) knows exactly what to change.

    #[test]
    fn dispatch_retry_of_out_of_order_arrival_returns_not_found() {
        // Both the predecessor poll and the retry poll are built upfront.
        // We apply the RETRY before the PREDECESSOR to simulate an out-of-order
        // gossip delivery. The current Phase 4a implementation returns
        // RetryOfPollNotFound because the predecessor is not yet in the log.
        //
        // TODO(ZEB-309 Phase 4b): when the pending-retries map is added, this
        // test should be updated: the retry must pend until the predecessor
        // arrives as Stage::Failed, then apply cleanly.
        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let c = cid();

        // Build the predecessor poll (not yet applied to the log).
        let pred_cfg = tier3_config();
        let pred_ev = tier3_create_event(creator, &pred_cfg);
        // Derive the predecessor poll_id using the same derivation apply uses:
        // sha256(community_id || signing_bytes).
        let pred_signing = pred_ev.signing_bytes().expect("signing_bytes for pred_ev");
        let pred_poll_id = crate::community_voting_core::derive_poll_id(&c, &pred_signing);

        // Build the retry poll referencing the not-yet-applied predecessor.
        let mut retry_cfg = tier3_config();
        retry_cfg.retry_of = Some(pred_poll_id);
        let retry_ev = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::PollCreate,
            hlc: hlc(2000),
            actor: creator,
            payload: encode(&retry_cfg),
            sig: vec![0u8; 64],
        };

        // Apply retry FIRST — predecessor not yet in log → RetryOfPollNotFound.
        let err = log
            .apply(retry_ev, &c)
            .expect_err("retry before predecessor must fail");
        assert_eq!(
            err,
            ApplyError::RetryOfPollNotFound,
            "out-of-order retry must return RetryOfPollNotFound (Phase 4a limitation)"
        );
    }

    // ── Test 13: Cluster 3 regression — eligibility filter in electorate snapshot ──

    #[test]
    fn dispatch_tier3_electorate_snapshot_filters_ineligible_members() {
        use crate::community_voting_core::{MemberAttrs, MembershipSnapshot};

        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cid = cid();

        // Build a config requiring min_power=10.
        let mut cfg = tier3_config();
        cfg.eligibility = Eligibility {
            min_power: 10,
            min_vouching_depth: None,
            sortition_size: None,
        };
        let create_ev = tier3_create_event(creator, &cfg);

        // Snapshot: eligible member (power=100) + ineligible member (power=5).
        let eligible = addr(0xE0);
        let ineligible = addr(0xE1);
        let mut members = std::collections::HashMap::new();
        members.insert(
            eligible,
            MemberAttrs {
                power: 100,
                vouching_depth: 0,
            },
        );
        members.insert(
            ineligible,
            MemberAttrs {
                power: 5,
                vouching_depth: 0,
            },
        );
        let snapshot = MembershipSnapshot { members };

        let pid = log
            .apply_with_snapshot(create_ev, &cid, Some(snapshot))
            .expect("apply with snapshot");

        let ps = log.polls.get(&pid).expect("poll");
        let t3 = ps.tier_state.as_tier3().expect("tier3 state");

        // Only the eligible member should be in the electorate.
        assert_eq!(
            t3.eligible_electorate_snapshot.len(),
            1,
            "only members meeting eligibility must be included in electorate snapshot"
        );
        assert!(
            t3.eligible_electorate_snapshot.contains(&eligible),
            "eligible member must be in snapshot"
        );
        assert!(
            !t3.eligible_electorate_snapshot.contains(&ineligible),
            "ineligible member must NOT be in snapshot (Cluster 3 fix)"
        );
    }

    // ── Test 13: Cluster 4 regression — decline_count_at deduplicates actors ──

    #[test]
    fn decline_count_at_deduplicates_same_actor_repeated_declines() {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cfg = tier3_config();
        let create_ev = tier3_create_event(creator, &cfg);
        let pid = log.apply(create_ev, &cid()).expect("tier3 create");

        // Apply kd=ss so sortition_result is set (required for decline to be meaningful).
        let ss_payload = SortitionSelectionPayload {
            poll_id: pid,
            primary: vec![addr(1), addr(2)],
            backup: vec![addr(3), addr(4)],
        };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::SortitionSelection,
                2000,
                addr(0xfe),
                encode(&ss_payload),
            ),
            &cid(),
        )
        .expect("apply kd=ss");

        // Same actor declines TWICE — should only count as 1 unique decliner.
        let same_actor = addr(1);
        let md_payload = crate::community_voting_core::MiniPublicDeclinePayload {
            poll_id: pid,
            reason: None,
        };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::MiniPublicDecline,
                3000,
                same_actor,
                encode(&md_payload),
            ),
            &cid(),
        )
        .expect("first decline");
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::MiniPublicDecline,
                4000,
                same_actor,
                encode(&md_payload),
            ),
            &cid(),
        )
        .expect("second decline (same actor)");

        let ps = log.polls.get(&pid).expect("poll");
        let t3 = ps.tier_state.as_tier3().expect("tier3");

        // Two kd=md events from same actor → unique count is 1, not 2.
        let now = Hlc {
            wall_ms: 99_999,
            logical: 0,
            device_id: "test".into(),
        };
        assert_eq!(
            t3.decline_count_at(&now),
            1,
            "decline_count_at must deduplicate same-actor repeat declines (Cluster 4 fix)"
        );

        // The mini-public set: walk primary||backup, skip declined, fill to sortition_size.
        // primary=[1,2], backup=[3,4]; sortition_size=20; addr(1) declined.
        // Walk: addr(1) declined → skip; addr(2) → add; addr(3) → add; addr(4) → add.
        // All non-declined pool members are collected (pool_size < sortition_size).
        let mp = t3.current_mini_public(&now);
        assert!(mp.contains(&addr(2)), "non-decliner stays in mini-public");
        assert!(
            mp.contains(&addr(3)),
            "backup[0] fills the vacant slot from addr(1) declining"
        );
        assert!(
            !mp.contains(&addr(1)),
            "addr(1) declined — must not be in mini-public"
        );
        // Note: addr(4) also enters set because sortition_size(20) > available non-declined(3).
        // The key invariant is: decline_count_at correctly deduplicates the same actor (tested above).
    }

    // ── Cluster C regression test ────────────────────────────────────────────

    // C1: two engines applying the same PollCreate event with members in different
    // HashMap iteration orders must produce identical eligible_electorate_snapshots.
    // Without canonical sort, fisher_yates_select would produce different SortitionResults.
    #[test]
    fn eligible_electorate_snapshot_is_deterministically_sorted_regardless_of_hashmap_order() {
        use crate::community_voting_core::{MemberAttrs, MembershipSnapshot};
        use std::collections::HashMap;

        // Build two snapshots with the same members but inserted in different orders.
        let member_a = addr(0x05);
        let member_b = addr(0x01);
        let member_c = addr(0x03);

        let make_snapshot = |order: &[u8]| {
            let mut members = HashMap::new();
            for &b in order {
                members.insert(
                    addr(b),
                    MemberAttrs {
                        power: 10,
                        vouching_depth: 0,
                    },
                );
            }
            MembershipSnapshot { members }
        };

        let cfg = tier3_config();

        // Snapshot 1: members inserted in order [0x05, 0x01, 0x03].
        let snap1 = make_snapshot(&[0x05, 0x01, 0x03]);
        // Snapshot 2: same members in reversed order [0x03, 0x01, 0x05].
        let snap2 = make_snapshot(&[0x03, 0x01, 0x05]);

        // Apply the same PollCreate event through two separate VotingLogs with different snapshots.
        let cid = cid();
        let creator = addr(0xaa);
        let create_ev = tier3_create_event(creator, &cfg);

        let mut log1 = VotingLog::new();
        let pid1 = log1
            .apply_with_snapshot(create_ev.clone(), &cid, Some(snap1))
            .expect("log1 apply");

        let mut log2 = VotingLog::new();
        let pid2 = log2
            .apply_with_snapshot(create_ev.clone(), &cid, Some(snap2))
            .expect("log2 apply");

        assert_eq!(pid1, pid2, "poll_id must be identical");

        let t3_1 = log1.polls[&pid1]
            .tier_state
            .as_tier3()
            .expect("tier3 in log1");
        let t3_2 = log2.polls[&pid2]
            .tier_state
            .as_tier3()
            .expect("tier3 in log2");

        // Both snapshots contain the same 3 members, both should produce the same sorted snapshot.
        assert_eq!(
            t3_1.eligible_electorate_snapshot, t3_2.eligible_electorate_snapshot,
            "eligible_electorate_snapshot must be identical regardless of HashMap insertion order"
        );

        // Expected: lex-sorted by OwnerAddr bytes → [addr(0x01), addr(0x03), addr(0x05)]
        let expected: Vec<_> = vec![member_b, member_c, member_a];
        assert_eq!(
            t3_1.eligible_electorate_snapshot, expected,
            "eligible_electorate_snapshot must be sorted OwnerAddr lex ASC"
        );
    }

    // ── tier3_ratification_candidate_count regression test ──────────────────
    //
    // Pre-fix bug: the helper called `drafting_advancers(&t3.candidates, ...)`
    // directly, which returns `None` because status_quo is never inserted into
    // `t3.candidates` by `apply()` — it's only synthesized on a local clone at
    // orchestration time. Net effect: `voting_cast_ratification_ballot` always
    // failed its pre-flight in production.
    //
    // Fix: mirror the engine-auto kd=rs computation — clone t3.candidates,
    // push synthesized status_quo, then derive advancers + ordering.
    //
    // This test drives a Tier 3 poll into Ratification with one above-threshold
    // draft candidate and confirms the helper returns Some(2) (= 1 real
    // candidate + status_quo) — NOT None, which is what it returned pre-fix.

    #[test]
    fn tier3_ratification_candidate_count_synthesizes_status_quo() {
        use crate::community_voting_core::{
            DraftApprovalPayload, DraftCandidatePayload, RatificationBallotPayload,
            SortitionSelectionPayload,
        };
        use crate::community_voting_tier3::event_hash_of;

        // Use the standard tier3_config() — sortition_size=20, dw=fw=rw=100s.
        // Drafting threshold = ceil(20/2) = 10 approvals.
        // Ratification reached at wall ≥ create_wall + (dw+fw)*1000 = 200_000ms.
        let cfg = tier3_config();

        let mut log = VotingLog::new();
        let creator = addr(0xaa);

        // PollCreate at t=1000 (tier3_create_event uses wall_ms=1000).
        let create_ev = tier3_create_event(creator, &cfg);
        let pid = log.apply(create_ev, &cid()).expect("tier3 create");

        // "now" well past the deadline (create + dw + fw = 201_000ms;
        // ratification window closes at 301_000ms). Use a wall_ms inside
        // Ratification for the success-case assertions; for stage-gate
        // negative cases, use a `now` inside the corresponding stage.
        let now_ratification = hlc(250_000);
        let now_sortition = hlc(1_500);
        let now_deliberation = hlc(50_000);
        let now_drafting = hlc(150_000);

        // Before any further events apply, the helper still returns None
        // because we haven't reached Ratification stage at `now_sortition`.
        // Pre-fix this relied on `last_hlc.as_ref()?` short-circuiting; now
        // it relies on `current_stage_at(now_sortition) != Ratification`.
        assert_eq!(
            log.tier3_ratification_candidate_count(&pid, &now_sortition),
            None,
            "now in Sortition stage must return None"
        );

        // kd=ss SortitionSelection — mini-public primary = [addr(1)..addr(20)].
        let primary: Vec<_> = (1..=20u8).map(addr).collect();
        let ss_payload = SortitionSelectionPayload {
            poll_id: pid,
            primary: primary.clone(),
            backup: vec![],
        };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::SortitionSelection,
                2000,
                addr(0xfe),
                encode(&ss_payload),
            ),
            &cid(),
        )
        .expect("apply kd=ss");

        // Still pre-Ratification at `now_deliberation` (wall=50_000 is
        // inside the deliberation window).
        assert_eq!(
            log.tier3_ratification_candidate_count(&pid, &now_deliberation),
            None,
            "Deliberation stage must return None"
        );

        // kd=dc DraftCandidate by addr(1) during Drafting window
        // (wall ≥ 1000+100_000 = 101_000ms, wall < 1000+200_000 = 201_000ms).
        let dc_payload = DraftCandidatePayload {
            poll_id: pid,
            text: "proposal A".into(),
        };
        let dc_ev = tier3_event_with_payload(
            PollEventKindCode::DraftCandidate,
            110_000,
            addr(1),
            encode(&dc_payload),
        );
        let candidate_hash = event_hash_of(&dc_ev);
        log.apply(dc_ev, &cid()).expect("apply kd=dc");

        // 9 additional approvers (addr(2)..addr(10)) — combined with the
        // implicit self-approval from addr(1), the candidate reaches the
        // threshold of 10.
        for (i, approver_byte) in (2u8..=10).enumerate() {
            let da_payload = DraftApprovalPayload {
                poll_id: pid,
                candidate_event_hash: candidate_hash,
            };
            log.apply(
                tier3_event_with_payload(
                    PollEventKindCode::DraftApproval,
                    110_000 + 1 + i as u64,
                    addr(approver_byte),
                    encode(&da_payload),
                ),
                &cid(),
            )
            .unwrap_or_else(|e| panic!("apply kd=da for addr({approver_byte}): {e:?}"));
        }

        // Drafting stage at `now_drafting` (wall=150_000 is between
        // dw (101_000) and dw+fw (201_000) → Drafting).
        assert_eq!(
            log.tier3_ratification_candidate_count(&pid, &now_drafting),
            None,
            "Drafting stage must return None"
        );

        // Push past the dw+fw threshold via a kd=rb RatificationBallot at
        // wall=210_000 (≥ 201_000 → Ratification). apply_event does not
        // validate ballot score length (verify does, but tests bypass verify),
        // so any score vector works.
        // Crucially: status_quo is NOT in t3.candidates — apply never writes
        // it. This is the bug condition the helper must handle.
        let rb_payload = RatificationBallotPayload {
            poll_id: pid,
            scores: Some(vec![5, 0]),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::RatificationBallot,
                210_000,
                addr(5),
                encode(&rb_payload),
            ),
            &cid(),
        )
        .expect("apply kd=rb");

        // Sanity: t3.candidates has exactly 1 entry (the draft). status_quo
        // is NOT in the slice — the apply path never writes it.
        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        assert_eq!(
            t3.candidates.len(),
            1,
            "status_quo is NOT in t3.candidates (apply path never writes it)"
        );
        assert_eq!(
            t3.candidates[0].approvals.len(),
            10,
            "draft candidate must have 10 approvals (threshold for sortition_size=20)"
        );
        assert_eq!(
            t3.last_hlc.as_ref().map(|h| h.wall_ms),
            Some(210_000),
            "last_hlc must reflect the kd=rb apply"
        );

        // The helper must now return Some(2) — 1 above-threshold draft +
        // synthesized status_quo. Pre-fix this returned None because
        // `drafting_advancers(&t3.candidates, ...)` couldn't find status_quo
        // in the candidate slice.
        assert_eq!(
            log.tier3_ratification_candidate_count(&pid, &now_ratification),
            Some(2),
            "Ratification stage with 1 above-threshold draft must return \
             Some(2) (= 1 draft + status_quo). Pre-fix this was None."
        );

        // The new caller-provided `now` is load-bearing: gating on
        // `t3.last_hlc` (= 210_000, Ratification) would also have worked
        // here, but for a poll where no events were applied after
        // deliberation closed, `last_hlc` would be earlier than the
        // ratification window and gating on it would incorrectly reject
        // valid ballots. This `now_ratification` is well past the
        // deadline (250_000 > 201_000) and represents the HLC the caller
        // would have just reserved for the new ballot.
        assert!(now_ratification.wall_ms > 201_000);

        // Unknown poll_id returns None.
        let missing_pid = PollId([0xff; 32]);
        assert_eq!(
            log.tier3_ratification_candidate_count(&missing_pid, &now_ratification),
            None,
            "Unknown poll_id must return None"
        );
    }

    // ── ZEB-860 Task 3: out-of-order canonical rebuild trigger ──────────────────
    //
    // These drive order-dependent Deliberation-family events (kd=ds, kd=dv)
    // through `apply` (which delegates to `apply_with_snapshot`) and assert the
    // out-of-order rebuild trigger fires iff the event was applied AND arrived at
    // or below the poll's pre-apply `max_applied` watermark AND is in the
    // order-dependent kind set. A cross-lane out-of-order arrival (a late kd=ds
    // from actor A while actor B's later-stamped kd=dv already dispatched) passes
    // the per-(actor,device) monotonic guard, so it reaches this trigger — the
    // exact divergence case the canonical rebuild converges.

    /// Build a Tier-3 poll driven into Deliberation with mini-public
    /// {A=addr(1), B=addr(2)}. Poll is created at wall=1000 with
    /// deliberation_window_seconds=100, so the deliberation window is
    /// [2000, 101_000): any event with wall_ms in that range sees
    /// `Stage::Deliberation`.
    fn tier3_poll_in_deliberation() -> (VotingLog, PollId) {
        let mut log = VotingLog::new();
        let creator = addr(0xaa);
        let cfg = tier3_config();
        let create_ev = tier3_create_event(creator, &cfg);
        let pid = log.apply(create_ev, &cid()).expect("tier3 create");

        // kd=ss puts A and B in the primary mini-public → poll enters Deliberation.
        let ss_payload = SortitionSelectionPayload {
            poll_id: pid,
            primary: vec![addr(1), addr(2)],
            backup: vec![],
        };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::SortitionSelection,
                2000,
                addr(0xfe),
                encode(&ss_payload),
            ),
            &cid(),
        )
        .expect("apply kd=ss");
        (log, pid)
    }

    /// Build a kd=ds DeliberationStatement event by `author` at `wall_ms`.
    fn ds_event(pid: PollId, author: OwnerAddr, wall_ms: u64, text: &str) -> SignedVotingEvent {
        let payload = DeliberationStatementPayload {
            poll_id: pid,
            text: text.into(),
        };
        tier3_event_with_payload(
            PollEventKindCode::DeliberationStatement,
            wall_ms,
            author,
            encode(&payload),
        )
    }

    /// Build a kd=dv DeliberationVote event by `voter` at `wall_ms` on the
    /// statement whose signing-bytes hash is `statement_hash` (vote=0, Agree).
    fn dv_event(
        pid: PollId,
        voter: OwnerAddr,
        wall_ms: u64,
        statement_hash: [u8; 32],
    ) -> SignedVotingEvent {
        let payload = DeliberationVotePayload {
            poll_id: pid,
            statement_event_hash: statement_hash,
            vote: 0, // BridgingVoteCode::Agree
        };
        tier3_event_with_payload(
            PollEventKindCode::DeliberationVote,
            wall_ms,
            voter,
            encode(&payload),
        )
    }

    /// The `statement_event_hash` a kd=dv must reference: SHA-256 of the kd=ds
    /// event's signing bytes. Read off `canonical_key`'s 4th tuple element, which
    /// IS `sha256_of_signing_bytes(ev)` — guaranteeing it matches the hash
    /// `apply_event` computes internally.
    fn statement_hash_of(ev: &SignedVotingEvent) -> [u8; 32] {
        crate::community_voting_tier3::canonical_key(ev).3
    }

    #[test]
    fn live_out_of_order_vote_is_rebuilt() {
        let (mut log, pid) = tier3_poll_in_deliberation();
        let ds = ds_event(pid, addr(1), 3000, "let us deliberate");
        let s_hash = statement_hash_of(&ds);
        let dv = dv_event(pid, addr(2), 4000, s_hash);

        // Deliver dv BEFORE ds: dv finds no target statement → Dropped. Not
        // out-of-order (4000 > max_applied 2000) and dropped anyway → no rebuild.
        log.apply(dv, &cid()).expect("apply dv (dropped, still Ok)");
        {
            let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
            assert!(
                !t3.deliberation.votes.contains_key(&(addr(2), s_hash)),
                "vote must be absent before its statement arrives"
            );
            assert_eq!(t3.rebuild_count, 0, "dropped dv must not rebuild");
        }

        // ds now arrives out of order (wall 3000 ≤ max_applied 4000) and applies
        // → triggers the canonical rebuild, which re-folds [ss, ds, dv] in HLC
        // order and lands dv against its now-present statement.
        log.apply(ds, &cid())
            .expect("apply ds (out-of-order, applied)");

        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        assert!(
            t3.deliberation.votes.contains_key(&(addr(2), s_hash)),
            "vote rebuilt live once its statement arrives"
        );
        assert_eq!(t3.rebuild_count, 1);
    }

    #[test]
    fn in_order_delivery_does_not_rebuild() {
        let (mut log, pid) = tier3_poll_in_deliberation();
        let ds = ds_event(pid, addr(1), 3000, "let us deliberate");
        let s_hash = statement_hash_of(&ds);
        let dv = dv_event(pid, addr(2), 4000, s_hash);

        // HLC order: ds (3000) then dv (4000). Each is fresh vs max_applied, so
        // both apply incrementally and neither triggers a rebuild.
        log.apply(ds, &cid()).expect("apply ds");
        log.apply(dv, &cid()).expect("apply dv");

        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        assert!(t3.deliberation.votes.contains_key(&(addr(2), s_hash)));
        assert_eq!(t3.rebuild_count, 0, "in-order fast path must not rebuild");
    }

    #[test]
    fn outsider_dropped_vote_does_not_rebuild() {
        let (mut log, pid) = tier3_poll_in_deliberation();
        let ds = ds_event(pid, addr(1), 5000, "let us deliberate");
        let s_hash = statement_hash_of(&ds);
        // addr(9) is NOT in the mini-public {addr(1), addr(2)}.
        let outsider_dv = dv_event(pid, addr(9), 3000, s_hash);

        // ds first raises max_applied to 5000...
        log.apply(ds, &cid()).expect("apply ds");
        // ...then the outsider dv arrives out of order (3000 ≤ 5000) but is
        // dropped (actor not in mini-public) → outcome Dropped → NO rebuild
        // (DoS guard: a stranger's backdated event must not force a rebuild).
        log.apply(outsider_dv, &cid())
            .expect("apply outsider dv (dropped, still Ok)");

        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        assert!(
            !t3.deliberation.votes.contains_key(&(addr(9), s_hash)),
            "outsider vote must never land"
        );
        assert_eq!(
            t3.rebuild_count, 0,
            "dropped outsider vote must not rebuild"
        );
    }

    #[test]
    fn byzantine_backdated_vote_is_dropped_after_rebuild() {
        let (mut log, pid) = tier3_poll_in_deliberation();
        let ds = ds_event(pid, addr(1), 5000, "let us deliberate");
        let s_hash = statement_hash_of(&ds);
        // dv is backdated BEFORE ds (3000 < 5000) but delivered [ds, dv].
        let dv = dv_event(pid, addr(2), 3000, s_hash);

        // ds applies in order. Then dv applies INCREMENTALLY (its statement is
        // present at delivery time), but is out-of-order vs max_applied
        // (3000 ≤ 5000) → triggers a rebuild whose canonical order
        // [dv(3000), ds(5000)] re-drops dv (it precedes its own statement) → the
        // vote converges to ABSENT despite the incremental accept.
        log.apply(ds, &cid()).expect("apply ds");
        log.apply(dv, &cid()).expect("apply dv (backdated)");

        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        assert!(
            !t3.deliberation.votes.contains_key(&(addr(2), s_hash)),
            "backdated vote must be dropped after canonical rebuild"
        );
        assert_eq!(t3.rebuild_count, 1);
    }

    // ── ZEB-867: canonical-fold pu finalize ─────────────────────────────────────

    /// Drive a pu poll (tier3_config) through Drafting into Ratification with one
    /// above-threshold draft candidate + status_quo (ratification set = 2), apply
    /// the given `(wall_ms, actor_byte, scores)` ballots, then kd=cl at 310_000.
    /// Returns the log at the closed (pre-finalize) stage.
    fn pu_poll_closed_with_ballots(ballots: &[(u64, u8, Vec<u8>)]) -> (VotingLog, PollId) {
        use crate::community_voting_core::{
            DraftApprovalPayload, DraftCandidatePayload, PollClosePayload,
            SortitionSelectionPayload,
        };
        use crate::community_voting_tier3::event_hash_of;

        let mut log = VotingLog::new();
        let pid = log
            .apply(tier3_create_event(addr(0xaa), &tier3_config()), &cid())
            .expect("tier3 create");

        let ss_payload = SortitionSelectionPayload {
            poll_id: pid,
            primary: (1..=20u8).map(addr).collect(),
            backup: vec![],
        };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::SortitionSelection,
                2000,
                addr(0xfe),
                encode(&ss_payload),
            ),
            &cid(),
        )
        .expect("apply kd=ss");

        let dc_payload = DraftCandidatePayload {
            poll_id: pid,
            text: "proposal A".into(),
        };
        let dc_ev = tier3_event_with_payload(
            PollEventKindCode::DraftCandidate,
            110_000,
            addr(1),
            encode(&dc_payload),
        );
        let candidate_hash = event_hash_of(&dc_ev);
        log.apply(dc_ev, &cid()).expect("apply kd=dc");

        for (i, approver_byte) in (2u8..=10).enumerate() {
            let da_payload = DraftApprovalPayload {
                poll_id: pid,
                candidate_event_hash: candidate_hash,
            };
            log.apply(
                tier3_event_with_payload(
                    PollEventKindCode::DraftApproval,
                    110_001 + i as u64,
                    addr(approver_byte),
                    encode(&da_payload),
                ),
                &cid(),
            )
            .expect("apply kd=da");
        }

        for (wall_ms, actor_byte, scores) in ballots {
            log.apply(
                rb_event_pu(pid, *wall_ms, addr(*actor_byte), scores.clone()),
                &cid(),
            )
            .expect("apply kd=rb");
        }

        let cl_payload = PollClosePayload { poll_id: pid };
        log.apply(
            tier3_event_with_payload(
                PollEventKindCode::PollClose,
                310_000,
                addr(0xff),
                encode(&cl_payload),
            ),
            &cid(),
        )
        .expect("apply kd=cl");

        (log, pid)
    }

    fn rb_event_pu(
        pid: PollId,
        wall_ms: u64,
        author: OwnerAddr,
        scores: Vec<u8>,
    ) -> SignedVotingEvent {
        tier3_event_with_payload(
            PollEventKindCode::RatificationBallot,
            wall_ms,
            author,
            encode(&crate::community_voting_core::RatificationBallotPayload {
                poll_id: pid,
                scores: Some(scores),
                ciphertexts_scores: None,
                ciphertexts_indicators: None,
                proof: None,
            }),
        )
    }

    fn rs_event(
        pid: PollId,
        wall_ms: u64,
        author: OwnerAddr,
        result: crate::community_voting_star::StarResult,
    ) -> SignedVotingEvent {
        tier3_event_with_payload(
            PollEventKindCode::PollResult,
            wall_ms,
            author,
            encode(&crate::community_voting_tier3::Tier3PollResultPayload {
                poll_id: pid,
                result,
            }),
        )
    }

    fn dummy_star_result() -> crate::community_voting_star::StarResult {
        crate::community_voting_star::StarResult {
            winner: crate::community_voting_star::CandidateRef {
                event_hash: [0u8; 32],
                approval_count: 0,
            },
            finalists: vec![],
            total_scores: vec![],
            runoff_votes: vec![],
        }
    }

    // A backdated pu kd=rb (canonically pre-finalize) arriving after finalize is
    // RECORDED and re-folded (Component 2), so the finalized tally reflects it and
    // live == boot-restore.
    #[test]
    fn pu_backdated_ballot_after_finalize_refolds() {
        let (mut log, pid) = pu_poll_closed_with_ballots(&[(210_000, 5, vec![5, 0])]);
        log.apply(
            rs_event(pid, 311_000, addr(0xfd), dummy_star_result()),
            &cid(),
        )
        .expect("apply kd=rs (finalize)");
        let before = log.polls[&pid]
            .tier_state
            .as_tier3()
            .unwrap()
            .result
            .clone();
        assert!(before.is_some(), "poll finalized");

        // Backdated ballot (wall 205_000 < finalize, in the Ratification window)
        // arrives after finalize; apply_event rejects it (terminal guard), then
        // Component 2 records it + rebuilds so it folds into the tally.
        let r = log.apply(rb_event_pu(pid, 205_000, addr(6), vec![3, 1]), &cid());
        assert!(
            r.is_ok(),
            "backdated pu ballot recorded + rebuilt, not rejected: {r:?}"
        );

        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        assert_eq!(t3.stage, crate::community_voting_tier3::Stage::Finalized);
        assert_eq!(t3.ratification_ballots.len(), 2, "late ballot folded in");
        assert_ne!(t3.result, before, "re-finalized tally changed");
        assert!(t3.rebuild_count >= 1, "a canonical rebuild ran");

        // live projection == a fresh canonical rebuild (ZEB-860 boot-restore parity).
        let events = log.polls[&pid].events.clone();
        let live = log.polls[&pid].tier_state.as_tier3().unwrap().clone();
        let mut restored = live.clone();
        restored.rebuild_from_events(&events);
        assert_eq!(restored.result, live.result, "live == boot-restore");
        assert_eq!(restored.ratification_ballots, live.ratification_ballots);
    }

    // A ballot whose key sorts AT/AFTER the finalize (320_000 > the kd=rs at
    // 311_000) is canonically post-finalize, so Component 2's finalize-key gate is
    // false → it stays dropped (today's behavior); the finalized tally is unchanged.
    #[test]
    fn pu_post_close_higher_hlc_ballot_excluded() {
        let (mut log, pid) = pu_poll_closed_with_ballots(&[(210_000, 5, vec![5, 0])]);
        log.apply(
            rs_event(pid, 311_000, addr(0xfd), dummy_star_result()),
            &cid(),
        )
        .expect("finalize");
        let before = log.polls[&pid]
            .tier_state
            .as_tier3()
            .unwrap()
            .result
            .clone();

        let r = log.apply(rb_event_pu(pid, 320_000, addr(6), vec![3, 1]), &cid());
        assert!(
            r.is_err(),
            "genuine post-close (higher-HLC) ballot stays dropped"
        );

        let t3 = log.polls[&pid].tier_state.as_tier3().unwrap();
        assert_eq!(t3.result, before, "result unchanged");
        assert_eq!(
            t3.ratification_ballots.len(),
            1,
            "post-close ballot not folded"
        );
    }

    // se polls keep today's exact behavior: a late ballot after finalize is
    // dropped — Component 2 is pu-gated.
    #[test]
    fn se_late_ballot_after_finalize_is_unaffected() {
        let mut cfg = tier3_config();
        cfg.privacy_mode = "se".into();
        let mut log = VotingLog::new();
        let pid = log
            .apply(tier3_create_event(addr(0xaa), &cfg), &cid())
            .expect("se create");
        // se finalize stores payload.result verbatim (no recompute, no committee).
        let claimed = crate::community_voting_star::StarResult {
            winner: crate::community_voting_star::CandidateRef {
                event_hash: [0x22; 32],
                approval_count: 3,
            },
            finalists: vec![crate::community_voting_star::CandidateRef {
                event_hash: [0x22; 32],
                approval_count: 3,
            }],
            total_scores: vec![7],
            runoff_votes: vec![7],
        };
        log.apply(rs_event(pid, 311_000, addr(0xfd), claimed.clone()), &cid())
            .expect("se finalize (verbatim)");
        assert_eq!(
            log.polls[&pid]
                .tier_state
                .as_tier3()
                .unwrap()
                .result
                .as_ref(),
            Some(&claimed),
            "se stored the claim verbatim"
        );

        let r = log.apply(rb_event_pu(pid, 205_000, addr(6), vec![3, 1]), &cid());
        assert!(r.is_err(), "se late ballot dropped (pu-gate)");
        assert_eq!(
            log.polls[&pid]
                .tier_state
                .as_tier3()
                .unwrap()
                .result
                .as_ref(),
            Some(&claimed),
            "se result unchanged"
        );
    }

    // Two replicas fed the same event set in different orders (one in-order, one
    // with a ballot delivered after finalize) converge to the same finalized tally.
    #[test]
    fn pu_finalize_converges_under_reordered_delivery() {
        let (mut a, pid) =
            pu_poll_closed_with_ballots(&[(205_000, 6, vec![3, 1]), (210_000, 5, vec![5, 0])]);
        a.apply(
            rs_event(pid, 311_000, addr(0xfd), dummy_star_result()),
            &cid(),
        )
        .expect("A finalize");

        let (mut b, _pid_b) = pu_poll_closed_with_ballots(&[(210_000, 5, vec![5, 0])]);
        b.apply(
            rs_event(pid, 311_000, addr(0xfd), dummy_star_result()),
            &cid(),
        )
        .expect("B finalize");
        b.apply(rb_event_pu(pid, 205_000, addr(6), vec![3, 1]), &cid())
            .expect("B late ballot recorded + rebuilt");

        let a_result = a.polls[&pid].tier_state.as_tier3().unwrap().result.clone();
        let b_result = b.polls[&pid].tier_state.as_tier3().unwrap().result.clone();
        assert_eq!(
            a_result, b_result,
            "replicas converge to the same finalized tally"
        );
        assert_eq!(
            a.polls[&pid].tier_state.as_tier3().unwrap().stage,
            crate::community_voting_tier3::Stage::Finalized
        );
    }
}
