//! ZEB-290 Phase 1: per-community voting event log.
//!
//! Parallels `community_channel_log.rs` (ZEB-248 pattern). Holds all
//! `SignedVotingEvent`s for a community plus the materialized per-poll
//! state map. Zenoh sync wiring lives in Task 12; this file is the
//! pure data structure + apply/materialize logic.

use std::collections::HashMap;

use crate::community_voting_approval::{validate_poll_config, Tier1PollConfig, Tier1TallyState};
use crate::community_voting_conviction::{
    DelegatePayload, DelegationGraph, SignalPayload, Tier2PollConfig, Tier2ProposalState,
    UndelegatePayload,
};
use crate::community_voting_core::{
    derive_poll_id, next_lifecycle, Eligibility, Lifecycle, MembershipSnapshot, PollEventKindCode,
    PollId, PollMeta, SignedVotingEvent, Tier,
};
use crate::owner_state_types::{Hlc, OwnerAddr};

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
    /// Per-community delegation graph for Tier 2 conviction voting (spec §5).
    /// Delegation is community-wide (NOT per-poll): a single
    /// `delegator → delegate` edge applies to every Tier 2 proposal in the
    /// community. Maintained via `Delegate`/`Undelegate` events; HLC-LWW
    /// resolves concurrent updates. Empty for communities with no Tier 2
    /// activity yet.
    pub delegation_graph: DelegationGraph,
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
/// `Tier1`; Phase 2 adds `Tier2`.
#[derive(Debug, Clone)]
pub enum TierState {
    Tier1(Tier1TallyState),
    Tier2(Tier2ProposalState),
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
        // threshold_conviction. The Signal apply path just updates the
        // per-voter state and appends to the event log.
        if event.kind == PollEventKindCode::Signal && event.tier == Tier::Conviction {
            let payload: SignalPayload = ciborium::de::from_reader(&event.payload[..])
                .map_err(|_| ApplyError::PayloadDecode)?;
            let state = self
                .polls
                .get_mut(&poll_id)
                .ok_or(ApplyError::EventBeforePollCreate)?;
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
            state.events.push(event.clone());
            self.events.push(event);
            return Ok(poll_id);
        }

        // ---- Tier 2 Delegate: graph mutation; NO lifecycle ----
        if event.kind == PollEventKindCode::Delegate && event.tier == Tier::Conviction {
            let payload: DelegatePayload = ciborium::de::from_reader(&event.payload[..])
                .map_err(|_| ApplyError::PayloadDecode)?;
            // Wire `to` is the 16-byte OwnerAddr (see
            // `DelegatePayload` doc for why this is not a raw pubkey).
            if payload.to.len() != 16 {
                return Err(ApplyError::PayloadValidate);
            }
            let mut to_bytes = [0u8; 16];
            to_bytes.copy_from_slice(&payload.to);
            self.delegation_graph
                .apply_delegate(event.actor, OwnerAddr(to_bytes), event.hlc.wall_ms as i128)
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
                .apply_undelegate(event.actor, event.hlc.wall_ms as i128);
            self.events.push(event);
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
            // Other tiers fall through to a minimal Tier1-shaped placeholder
            // — they'll be replaced in their respective phases.
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
                };
                let tally = Tier1TallyState::empty(cfg.options.len());
                (meta, Some(cfg), TierState::Tier1(tally))
            } else if event.tier == Tier::Conviction {
                let cfg: Tier2PollConfig = ciborium::de::from_reader(&event.payload[..])
                    .map_err(|_| ApplyError::PayloadDecode)?;
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
                };
                let proposal_state = Tier2ProposalState::new(cfg, total_supply);
                (meta, None, TierState::Tier2(proposal_state))
            } else {
                // Sortition (Tier 3) and future tiers: minimal placeholder.
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
                (meta, None, TierState::Tier1(Tier1TallyState::empty(0)))
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
            to: to.to_vec(),
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
            let Some(fin_at) = state
                .events
                .iter()
                .find(|e| e.kind == PollEventKindCode::PollResult)
                .map(|e| e.hlc.wall_ms)
            else {
                continue;
            };
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
