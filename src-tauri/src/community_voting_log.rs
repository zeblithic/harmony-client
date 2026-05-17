//! ZEB-290 Phase 1: per-community voting event log.
//!
//! Parallels `community_channel_log.rs` (ZEB-248 pattern). Holds all
//! `SignedVotingEvent`s for a community plus the materialized per-poll
//! state map. Zenoh sync wiring lives in Task 12; this file is the
//! pure data structure + apply/materialize logic.

use std::collections::HashMap;

use crate::community_voting_core::{
    derive_poll_id, next_lifecycle, Eligibility, Lifecycle, PollEventKindCode, PollId, PollMeta,
    SignedVotingEvent,
};

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
    pub fn apply(
        &mut self,
        event: SignedVotingEvent,
        community_id: &crate::owner_state_types::SpaceId,
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
            // Stub PollMeta — populated fully in Task 7 once Tier 1
            // PollConfig deserialization lands.
            let stub = PollMeta {
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
            self.polls.insert(
                poll_id,
                PollState {
                    meta: stub,
                    events: vec![event.clone()],
                    tier_state: TierState::Empty,
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
    use crate::community_voting_core::Tier;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    fn signing_bytes_of(ev: &SignedVotingEvent) -> Vec<u8> {
        ev.signing_bytes().expect("signing bytes")
    }

    fn poll_create_event(creator: OwnerAddr) -> SignedVotingEvent {
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
            payload: vec![],
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
