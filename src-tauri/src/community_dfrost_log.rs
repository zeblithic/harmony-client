//! ZEB-301 Phase 4a-foundation: D-FROST committee event log.
//!
//! Parallels `community_voting_log::VotingLog` (ZEB-290 pattern). Holds
//! the signed committee events for a community plus the materialized
//! `CommitteeState` derived from them, in-memory only DKG/sign-session
//! secret packages, and a per-ceremony pending-state structure used to
//! gather contributions across the network before finalising a transition.
//!
//! Zenoh sync wiring lives in Phase 4a-main; this file is the pure
//! data-structure + apply-dispatch logic. Task 2 ships only the skeleton:
//! the type lattice + `apply()` dispatcher with stub handlers (all return
//! `Ok(())` except `apply_dkg_round` which checks pending-ceremony presence
//! so the dispatcher's "unknown ceremony" error path is exercised).

use std::collections::{BTreeMap, HashMap};

use frost_ristretto255::keys::{
    dkg::{round1 as dkg_r1, round2 as dkg_r2},
    KeyPackage, PublicKeyPackage,
};
use frost_ristretto255::round1::SigningNonces;
use frost_ristretto255::Identifier;
use serde::{Deserialize, Serialize};

use crate::community_dfrost_types::{DfrostEventKind, SignedCommitteeEvent};
use crate::owner_state_types::OwnerAddr;

/// All D-FROST committee events for a single community, plus the
/// materialized `CommitteeState`. Lives in
/// `NodeState.dfrost_logs: HashMap<SpaceId, Arc<Mutex<DfrostLog>>>` once
/// Task 9 wires the IPC surface.
#[derive(Debug, Default)]
pub struct DfrostLog {
    /// All accepted events, ordered by insert time (caller is expected
    /// to order by HLC at verify time before invoking `apply`).
    pub events: Vec<SignedCommitteeEvent>,

    /// Materialized state derived from `events`. Reset/rebuilt on
    /// deserialization via the `serde(from = "CommitteeStateRaw")` shim.
    pub committee_state: CommitteeState,

    /// In-memory DKG round-1 secret (this node's). Held only between the
    /// `dr(rn=1)` post and the `dr(rn=2)` send; never persisted to disk
    /// because it lets a recovered attacker reconstruct the local share.
    pub local_dkg_secret: Option<dkg_r1::SecretPackage>,

    /// In-memory DKG round-2 secret (this node's). Held only between the
    /// `dr(rn=2)` post and the local `dk` finalisation; never persisted.
    pub local_dkg_secret2: Option<dkg_r2::SecretPackage>,

    /// Local FROST `KeyPackage` (this node's signing share). Materialised
    /// at DKG completion or refresh completion. NOT persisted in Phase
    /// 4a-foundation (Phase 4b adds sealed-disk storage gated by the
    /// device-binding flow).
    pub local_key_package: Option<KeyPackage>,

    /// Local FROST `PublicKeyPackage` (joint verifying key + per-member
    /// verifying shares). Materialised alongside `local_key_package`.
    pub local_pub_key_package: Option<PublicKeyPackage>,

    /// Active threshold-sign nonce material, keyed by signing-ceremony
    /// id. Each `(nonces, commitments)` pair MUST be used exactly once
    /// (re-use leaks the signing share — see FROST spec §6.2). Cleared
    /// when the corresponding `vb` event lands or the ceremony is
    /// abandoned.
    pub local_signing_nonces: HashMap<[u8; 32], SigningNonces>,
}

/// Persisted (CBOR) committee state for a community. The
/// `identifier_map` is rebuilt deterministically from `members` on every
/// deserialization via the `serde(from = "CommitteeStateRaw")` route —
/// `frost_ristretto255::Identifier` is not itself `Serialize`-able and
/// the mapping is a pure function of `members`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(from = "CommitteeStateRaw")]
pub struct CommitteeState {
    /// `false` until the first `dk` event finalises the committee.
    pub active: bool,
    /// Bumped on every successful DKG or refresh completion. Starts at 0.
    pub current_epoch: u64,
    /// Compressed Ristretto point bytes for the joint verifying key.
    /// `None` until the first DKG completion.
    pub joint_verifying_key: Option<[u8; 32]>,
    /// Per-member verifying shares (compressed Ristretto point bytes).
    pub verifying_shares: BTreeMap<OwnerAddr, [u8; 32]>,
    /// Sorted committee member list. Sort order is canonical bytewise
    /// `OwnerAddr` ordering — `build_identifier_map` depends on this.
    pub members: Vec<OwnerAddr>,
    /// Threshold (`min_signers`) for both DKG and threshold signing.
    pub threshold: u16,
    /// `max_signers` (number of committee members at DKG time).
    pub max_signers: u16,
    /// Sorted-OwnerAddr → 1-indexed FROST identifier mapping. Derived
    /// from `members` via `build_identifier_map`; skipped on the wire
    /// because `Identifier` is not `Serialize`.
    #[serde(skip)]
    pub identifier_map: BTreeMap<OwnerAddr, Identifier>,
    /// In-flight DKG ceremony, if any. Cleared on successful completion.
    pub pending_dkg: Option<PendingCeremony>,
    /// In-flight threshold signing sessions, keyed by `ceremony_id`.
    pub pending_sign: BTreeMap<[u8; 32], PendingSignSession>,
    /// In-flight proactive-refresh ceremony, if any. Cleared on completion.
    pub pending_refresh: Option<PendingCeremony>,
}

/// Wire shape used solely as a `serde(from = ...)` shim: identical to
/// `CommitteeState` minus the derived `identifier_map`. `From` impl
/// below rebuilds the map deterministically on every load.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct CommitteeStateRaw {
    pub active: bool,
    pub current_epoch: u64,
    pub joint_verifying_key: Option<[u8; 32]>,
    pub verifying_shares: BTreeMap<OwnerAddr, [u8; 32]>,
    pub members: Vec<OwnerAddr>,
    pub threshold: u16,
    pub max_signers: u16,
    pub pending_dkg: Option<PendingCeremony>,
    pub pending_sign: BTreeMap<[u8; 32], PendingSignSession>,
    pub pending_refresh: Option<PendingCeremony>,
}

impl From<CommitteeStateRaw> for CommitteeState {
    fn from(raw: CommitteeStateRaw) -> Self {
        let identifier_map = CommitteeState::build_identifier_map(&raw.members);
        Self {
            active: raw.active,
            current_epoch: raw.current_epoch,
            joint_verifying_key: raw.joint_verifying_key,
            verifying_shares: raw.verifying_shares,
            members: raw.members,
            threshold: raw.threshold,
            max_signers: raw.max_signers,
            identifier_map,
            pending_dkg: raw.pending_dkg,
            pending_sign: raw.pending_sign,
            pending_refresh: raw.pending_refresh,
        }
    }
}

impl CommitteeState {
    /// Deterministic OwnerAddr → 1-indexed FROST `Identifier` map.
    ///
    /// Both sides of a DKG MUST agree on identifier assignment; we
    /// derive it purely from the (sorted-bytewise) member list so any
    /// pair of nodes that observes the same `members` set materialises
    /// the same map without further coordination.
    pub fn build_identifier_map(members: &[OwnerAddr]) -> BTreeMap<OwnerAddr, Identifier> {
        let mut sorted: Vec<OwnerAddr> = members.to_vec();
        sorted.sort();
        sorted.dedup();
        let mut map = BTreeMap::new();
        for (idx, addr) in sorted.into_iter().enumerate() {
            // 1-indexed — FROST disallows Identifier(0). We cast through
            // u16; the caller is responsible for staying within
            // `max_signers <= u16::MAX` (enforced upstream at DKG-initiate).
            let id = Identifier::try_from((idx as u16) + 1)
                .expect("idx+1 fits u16 and is non-zero by construction");
            map.insert(addr, id);
        }
        map
    }
}

/// Per-ceremony pending state shared between DKG and proactive-refresh
/// ceremonies. Both protocols accumulate round-1 packages, decrypted
/// round-2 packages, and per-member `dk` confirmations; only the
/// invariants enforced at finalisation differ (refresh requires `vk`
/// unchanged from the existing committee).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PendingCeremony {
    pub ceremony_id: [u8; 32],
    /// Round-1 package bytes per actor (broadcast-shaped).
    pub round1_packages: BTreeMap<OwnerAddr, Vec<u8>>,
    /// Decrypted round-2 package bytes per sender (this node's share).
    /// Populated only on the local node via `apply_with_identity`.
    pub round2_packages: BTreeMap<OwnerAddr, Vec<u8>>,
    /// Per-member `dk` confirmations: actor → claimed joint VK bytes.
    /// Conflict (≥2 distinct values) is a protocol abort.
    pub dk_confirmations: BTreeMap<OwnerAddr, [u8; 32]>,
    pub proposed_epoch: u64,
    pub members: Vec<OwnerAddr>,
    pub threshold: u16,
    pub max_signers: u16,
}

/// Per-signing-ceremony pending state. One entry per in-flight
/// threshold-sign + VRF-beacon ceremony, keyed by `ceremony_id`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PendingSignSession {
    /// VRF seed bytes (`derive_vrf_seed(poll_hash, epoch)`).
    pub message_hash: [u8; 32],
    /// Per-actor (commitments_bytes, share_bytes) contributions.
    pub contributions: BTreeMap<OwnerAddr, (Vec<u8>, Vec<u8>)>,
}

/// Errors surfaced by `DfrostLog::apply`. Verify-time checks (signature
/// validity, kind-specific decode, actor membership) are the caller's
/// responsibility; apply only fails on materialise-level invariants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    /// CBOR decode of the kind-specific payload failed.
    PayloadDecode,
    /// A `dr`/`ts`/`vb`/`rf` event referenced a `ceremony_id` for which
    /// no pending ceremony / sign session exists.
    UnknownCeremony,
    /// `apply_dkg_complete` (or refresh-complete) saw two distinct `vk`
    /// values across confirmations, or a refresh attempted to change the
    /// committee's joint verifying key.
    InvariantViolation,
    /// Event arrived with a kind that does not match this log's
    /// envelope tag (`tg != 'd'`) or whose `committee_tier` field is
    /// non-zero. Defence-in-depth — peers should not produce these.
    UnexpectedEnvelope,
}

impl DfrostLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a single committee event. Caller has already verified the
    /// Ed25519 envelope signature and any kind-specific membership /
    /// authorization rules. This function only handles the
    /// materialize-into-state half.
    ///
    /// Task 2 ships only the dispatch skeleton: `apply_dkg_round` checks
    /// for a pending ceremony (so the "orphan event" error path is
    /// exercised by the test suite), and the other four handlers are
    /// no-op `Ok(())` stubs to be fleshed out in Tasks 4–6.
    pub fn apply(&mut self, event: SignedCommitteeEvent) -> Result<(), ApplyError> {
        // Envelope sanity — never reachable from honest peers, but
        // these are cheap defence-in-depth checks given the dispatcher
        // already pattern-matches `event.kind`.
        if event.tag != 'd' || event.committee_tier != 0 {
            return Err(ApplyError::UnexpectedEnvelope);
        }

        let result = match event.kind {
            DfrostEventKind::DkgRound => self.apply_dkg_round(&event),
            DfrostEventKind::DkgComplete => self.apply_dkg_complete(&event),
            DfrostEventKind::ThresholdSign => self.apply_threshold_sign(&event),
            DfrostEventKind::VrfBeacon => self.apply_vrf_beacon(&event),
            DfrostEventKind::ProactiveRefresh => self.apply_proactive_refresh(&event),
        };
        result?;

        self.events.push(event);
        Ok(())
    }

    /// Apply a `dr` event (DKG round 1 broadcast or round 2 encrypted shares).
    ///
    /// Round 1 (broadcast): records the actor's `round1_package` bytes in
    /// the pending ceremony's `round1_packages` map. Idempotent — a duplicate
    /// from the same actor is silently ignored (HLC LWW deduped upstream).
    ///
    /// Round 2 (encrypted shares): bytes here are encrypted to per-recipient
    /// pubkeys, so the global `apply` path can't decrypt them. The local-
    /// node path (`apply_with_identity`, Task 5) handles round-2 decryption.
    fn apply_dkg_round(&mut self, event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        use crate::community_dfrost_types::DkgRoundPayload;

        let payload: DkgRoundPayload =
            ciborium::de::from_reader(&event.payload[..]).map_err(|_| ApplyError::PayloadDecode)?;

        let pending = self
            .committee_state
            .pending_dkg
            .as_mut()
            .ok_or(ApplyError::UnknownCeremony)?;
        if pending.ceremony_id != payload.ceremony_id {
            return Err(ApplyError::UnknownCeremony);
        }

        if payload.round_num == 1 {
            if let Some(pkg) = payload.round1_package {
                pending.round1_packages.entry(event.actor).or_insert(pkg);
            }
        }
        // Round 2: the broadcast log path is intentionally inert here.
        // Per-recipient decryption + share storage happens in
        // `apply_with_identity` (Task 5). Returning Ok keeps the dr(rn=2)
        // event flowing into `self.events` so non-committee replicas
        // still observe the protocol progress.
        Ok(())
    }

    /// Apply a `dk` (DKG complete) event. Records this actor's claimed
    /// joint verifying key into `dk_confirmations`; on quorum (count >=
    /// pending.threshold) and consensus (all confirmations equal), finalize
    /// the committee. Rejects with `InvariantViolation` when:
    ///
    /// * the committee is already active AND the dk's vk differs from the
    ///   active vk (a DKG cannot mutate the joint pubkey — that's what
    ///   proactive refresh is for, and the spec requires vk identity).
    /// * two dk confirmations disagree on the vk for the same ceremony.
    fn apply_dkg_complete(&mut self, event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        use crate::community_dfrost_types::DkgCompletePayload;

        let payload: DkgCompletePayload =
            ciborium::de::from_reader(&event.payload[..]).map_err(|_| ApplyError::PayloadDecode)?;

        // Reject any dk that would mutate an already-active joint vk.
        // (Tier 3a contract: DKG runs at most once per committee epoch
        // boundary; once `active`, refresh is the only path forward.)
        if self.committee_state.active {
            if let Some(existing_vk) = self.committee_state.joint_verifying_key {
                if existing_vk != payload.joint_verifying_key {
                    return Err(ApplyError::InvariantViolation);
                }
            }
        }

        let pending = self
            .committee_state
            .pending_dkg
            .as_mut()
            .ok_or(ApplyError::UnknownCeremony)?;
        if pending.ceremony_id != payload.ceremony_id {
            return Err(ApplyError::UnknownCeremony);
        }

        // Cross-confirmation consensus: any disagreement on the vk among
        // already-recorded confirmations aborts.
        for existing_vk in pending.dk_confirmations.values() {
            if *existing_vk != payload.joint_verifying_key {
                return Err(ApplyError::InvariantViolation);
            }
        }
        pending
            .dk_confirmations
            .insert(event.actor, payload.joint_verifying_key);

        // Quorum reached → promote the pending ceremony to the active
        // committee state. We re-read threshold/members from the just-
        // decoded `dk` payload (the proposer's snapshot) rather than from
        // `pending`, mirroring the invariant that on-the-wire payload is
        // authoritative for committee shape.
        if pending.dk_confirmations.len() >= payload.threshold as usize {
            let mut new_verifying_shares: BTreeMap<OwnerAddr, [u8; 32]> = BTreeMap::new();
            for mvs in &payload.verifying_shares {
                new_verifying_shares.insert(mvs.member, mvs.verifying_share);
            }
            let identifier_map = CommitteeState::build_identifier_map(&payload.members);

            self.committee_state.active = true;
            self.committee_state.current_epoch = payload.epoch;
            self.committee_state.joint_verifying_key = Some(payload.joint_verifying_key);
            self.committee_state.verifying_shares = new_verifying_shares;
            self.committee_state.members = payload.members.clone();
            self.committee_state.threshold = payload.threshold;
            self.committee_state.max_signers = payload.max_signers;
            self.committee_state.identifier_map = identifier_map;
            self.committee_state.pending_dkg = None;
        }

        Ok(())
    }

    /// Apply a `ts` event. Stub for Task 2; fleshed out in Task 6.
    fn apply_threshold_sign(&mut self, _event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        Ok(())
    }

    /// Apply a `vb` event. Stub for Task 2; fleshed out in Task 6.
    fn apply_vrf_beacon(&mut self, _event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        Ok(())
    }

    /// Apply an `rf` event. Stub for Task 2; fleshed out in Task 8.
    fn apply_proactive_refresh(&mut self, _event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::OwnerAddr;

    #[test]
    fn dfrost_log_starts_empty() {
        let log = DfrostLog::new();
        assert!(!log.committee_state.active);
        assert_eq!(log.committee_state.current_epoch, 0);
        assert!(log.committee_state.joint_verifying_key.is_none());
        assert!(log.events.is_empty());
    }

    #[test]
    fn build_identifier_map_uses_sorted_order() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        // alice < bob in byte order; even if passed unsorted, alice gets id=1
        let map = CommitteeState::build_identifier_map(&[bob, alice]);
        let alice_id = frost_ristretto255::Identifier::try_from(1u16).unwrap();
        let bob_id = frost_ristretto255::Identifier::try_from(2u16).unwrap();
        assert_eq!(map[&alice], alice_id);
        assert_eq!(map[&bob], bob_id);
    }

    #[test]
    fn apply_unknown_ceremony_returns_error() {
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgRoundPayload, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        // Post a dr(rn=1) event with no pending ceremony — should error.
        let payload = DkgRoundPayload {
            ceremony_id: [0x42u8; 32],
            round_num: 1,
            round1_package: Some(vec![0xde]),
            recipient_ciphertexts: None,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        let ev = SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgRound,
            hlc: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: OwnerAddr([0xaa; 16]),
            payload: pd,
            sig: vec![0u8; 64],
        };
        let mut log = DfrostLog::new();
        assert_eq!(log.apply(ev), Err(ApplyError::UnknownCeremony));
    }

    #[test]
    fn full_1of1_dkg_ceremony_finalizes() {
        // 1-of-1 committee: single member posts dr(rn=1) then dk → committee active.
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgCompletePayload, DkgRoundPayload, MemberVerifyingShare,
            SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let ceremony_id = [0x42u8; 32];
        let fake_vk = [0x55u8; 32];

        let mut log = DfrostLog::new();
        // Seed pending_dkg (normally done by initiate_dkg IPC).
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id,
            members: vec![alice],
            threshold: 1,
            max_signers: 1,
            proposed_epoch: 1,
            ..Default::default()
        });

        // Apply dr(rn=1)
        let r1_payload = DkgRoundPayload {
            ceremony_id,
            round_num: 1,
            round1_package: Some(vec![0xde, 0xad]),
            recipient_ciphertexts: None,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&r1_payload, &mut pd).unwrap();
        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgRound,
            hlc: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![0u8; 64],
        })
        .expect("apply dr rn=1");
        assert!(log
            .committee_state
            .pending_dkg
            .as_ref()
            .unwrap()
            .round1_packages
            .contains_key(&alice));

        // Apply dk
        let dk_payload = DkgCompletePayload {
            ceremony_id,
            joint_verifying_key: fake_vk,
            verifying_shares: vec![MemberVerifyingShare {
                member: alice,
                verifying_share: [0xaa; 32],
            }],
            epoch: 1,
            members: vec![alice],
            threshold: 1,
            max_signers: 1,
        };
        let mut pd2 = Vec::new();
        ciborium::into_writer(&dk_payload, &mut pd2).unwrap();
        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: Hlc {
                wall_ms: 2000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd2,
            sig: vec![0u8; 64],
        })
        .expect("apply dk");

        assert!(log.committee_state.active);
        assert_eq!(log.committee_state.current_epoch, 1);
        assert_eq!(log.committee_state.joint_verifying_key, Some(fake_vk));
        assert_eq!(log.committee_state.verifying_shares[&alice], [0xaa; 32]);
        assert!(log.committee_state.pending_dkg.is_none());
        assert_eq!(log.events.len(), 2);
    }

    #[test]
    fn dk_with_wrong_vk_after_active_returns_invariant_violation() {
        // After a committee is active, a dk with a different vk must be rejected.
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgCompletePayload, MemberVerifyingShare, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.joint_verifying_key = Some([0x11u8; 32]);
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id: [0xcc; 32],
            members: vec![OwnerAddr([0x01; 16])],
            threshold: 1,
            max_signers: 1,
            proposed_epoch: 2,
            ..Default::default()
        });

        let dk_payload = DkgCompletePayload {
            ceremony_id: [0xcc; 32],
            joint_verifying_key: [0x22u8; 32], // DIFFERENT from active [0x11]
            verifying_shares: vec![MemberVerifyingShare {
                member: OwnerAddr([0x01; 16]),
                verifying_share: [0; 32],
            }],
            epoch: 2,
            members: vec![OwnerAddr([0x01; 16])],
            threshold: 1,
            max_signers: 1,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&dk_payload, &mut pd).unwrap();

        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: Hlc {
                wall_ms: 3000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: OwnerAddr([0x01; 16]),
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
    }
}
