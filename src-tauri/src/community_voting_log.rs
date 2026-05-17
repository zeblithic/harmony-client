//! ZEB-290 Phase 1: per-community voting event log.
//!
//! Parallels `community_channel_log.rs` (ZEB-248 pattern). Holds all
//! `SignedVotingEvent`s for a community plus the materialized per-poll
//! state map. Zenoh sync wiring lives in Task 12; this file is the
//! pure data structure + apply/materialize logic.

use std::collections::HashMap;

use crate::community_voting_approval::{validate_poll_config, Tier1PollConfig};
use crate::community_voting_core::{
    derive_poll_id, next_lifecycle, Eligibility, Lifecycle, MembershipSnapshot, PollEventKindCode,
    PollId, PollMeta, SignedVotingEvent, Tier,
};
use crate::owner_state_types::Hlc;

/// All voting events for a single community, plus the materialized
/// per-poll state derived from them.
///
/// Stored in `NodeState` keyed by community SpaceId. Synced via Zenoh
/// topic `harmony/community/{id}/voting` (Task 12).
#[derive(Debug, Default, Clone)]
pub struct VotingLog {
    /// All accepted events, ordered by (hlc, event_hash) at insert time.
    pub events: Vec<SignedVotingEvent>,
    /// Materialized per-poll state, keyed by PollId.
    pub polls: HashMap<PollId, PollState>,
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

/// Tier-specific tally state. Replaced by `Tier1TallyState` from
/// `community_voting_approval.rs` in Task 8.
#[derive(Debug, Clone)]
pub enum TierState {
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    SigningBytesError,
    MissingPollIdRef,
    IllegalTransition,
    EventBeforePollCreate,
    PayloadDecode,
    PayloadValidate,
}

impl VotingLog {
    pub fn new() -> Self {
        Self::default()
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
    /// Stored on the new `PollState` when `event.kind == PollCreate`
    /// and `event.tier == Tier::Approval`. Ignored otherwise.
    pub fn apply_with_snapshot(
        &mut self,
        event: SignedVotingEvent,
        community_id: &crate::owner_state_types::SpaceId,
        snapshot: Option<MembershipSnapshot>,
    ) -> Result<PollId, ApplyError> {
        // PollCreate derives PollId from H(community_id || signing_bytes);
        // every other kind references an existing PollId via a `{ "pi": ... }`
        // map in the payload (encoded by tier modules; Task 7 covers Tier 1).
        let poll_id = match event.kind {
            PollEventKindCode::PollCreate => {
                let sb = event
                    .signing_bytes()
                    .map_err(|_| ApplyError::SigningBytesError)?;
                derive_poll_id(community_id, &sb)
            }
            _ => decode_poll_id_ref(&event.payload).ok_or(ApplyError::MissingPollIdRef)?,
        };

        // For non-create events, require an existing poll. We check this
        // *before* the lifecycle transition so the failure surfaces as
        // EventBeforePollCreate (more specific) rather than the generic
        // IllegalTransition that the Draft state machine would otherwise emit.
        let existing_lifecycle = self.polls.get(&poll_id).map(|p| p.meta.lifecycle);
        if existing_lifecycle.is_none() && event.kind != PollEventKindCode::PollCreate {
            return Err(ApplyError::EventBeforePollCreate);
        }

        let current = existing_lifecycle.unwrap_or(Lifecycle::Draft);
        let next =
            next_lifecycle(current, event.kind).map_err(|_| ApplyError::IllegalTransition)?;

        if let Some(state) = self.polls.get_mut(&poll_id) {
            state.meta.lifecycle = next;
            state.events.push(event.clone());
        } else if event.kind == PollEventKindCode::PollCreate {
            // Tier-1 PollCreate: deserialize and validate Tier1PollConfig
            // from the payload, then populate PollMeta fully from it.
            // Tier 2/3 land in their respective phases — until then,
            // PollCreate events with non-Tier 1 tier values populate a
            // minimal PollMeta with default Eligibility (closes_at = hlc).
            let (meta, tier1_cfg) = if event.tier == Tier::Approval {
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
                };
                (meta, Some(cfg))
            } else {
                let meta = PollMeta {
                    poll_id,
                    community_id: *community_id,
                    creator: event.actor,
                    tier: event.tier,
                    eligibility: Eligibility {
                        min_power: 0,
                        min_vouching_depth: None,
                        sortition_size: None,
                    },
                    lifecycle: next,
                    created_at: event.hlc.clone(),
                    opens_at: event.hlc.clone(),
                    closes_at: event.hlc.clone(),
                    extends_at: None,
                    channel_id: None,
                };
                (meta, None)
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
                    tier_state: TierState::Empty,
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
}

/// Decode a `{ "pi": <PollId> }` map from `pd` bytes. Used by all
/// non-PollCreate events to identify which poll they belong to.
fn decode_poll_id_ref(pd: &[u8]) -> Option<PollId> {
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
        for (pid, state) in self.polls.iter_mut() {
            if state.meta.lifecycle != Lifecycle::Finalized {
                continue;
            }
            let finalized_at = state
                .events
                .iter()
                .find(|e| e.kind == PollEventKindCode::PollResult)
                .map(|e| e.hlc.wall_ms);
            let Some(fin_at) = finalized_at else {
                continue;
            };
            if now_wall_ms.saturating_sub(fin_at) > NINETY_DAYS_MS {
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
        archived
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;
    use crate::community_voting_core::Tier;
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
        };
        log.polls.insert(
            pid,
            PollState {
                meta,
                events,
                tier_state: TierState::Empty,
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
        };
        log.polls.insert(
            pid,
            PollState {
                meta,
                events: vec![],
                tier_state: TierState::Empty,
                tier1_cfg: None,
                tier1_snapshot: None,
            },
        );
        let archived = log.archive_finalized_polls(999 * 24 * 60 * 60 * 1000);
        assert!(archived.is_empty());
    }
}
