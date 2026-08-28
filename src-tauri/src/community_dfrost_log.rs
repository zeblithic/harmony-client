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

    /// Index of completed VRF beacons: `message_hash → vrf_output`.
    ///
    /// Populated in `apply_vrf_beacon` so that `find_vrf_beacon_output_by_seed`
    /// can answer oracle lookups without re-scanning `events`. The key is
    /// the `VrfBeaconPayload.message_hash` field (= `derive_vrf_seed(seed, epoch)`).
    /// Given a poll's beacon seed and the committee's epoch, callers compute
    /// the expected `message_hash = derive_vrf_seed(seed, epoch)` and look it up here.
    ///
    /// Not persisted (same as `local_key_package`): rebuilt on replay.
    /// Task 10: consulted by `DfrostBeaconOracle<R>` for `verify_ss`.
    pub beacon_index: HashMap<[u8; 32], [u8; 32]>,
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
            // R2 (Cursor Low): delegate to `identifier_for_index` to
            // avoid duplicated index→Identifier conversion logic. Both
            // functions assign 1-indexed identifiers from sorted member
            // order; using one canonical implementation ensures
            // overflow handling (u16::try_from + checked_add) stays
            // consistent across both call sites.
            let id = crate::community_dfrost_crypto::identifier_for_index(idx);
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
    /// ZEB-1022: the actor whose `di` (CeremonyInit) event seeded this
    /// ceremony. The orchestration layer uses it to decide which node
    /// owns the deadline-abort + re-initiate responsibility (only the
    /// initiator restarts a stalled ceremony; peers wait for a
    /// replacement `di`). `None` for ceremonies seeded by legacy /
    /// test paths that set `pending_dkg` directly, and for
    /// `pending_refresh` (refresh has no initiator-driven recovery).
    #[serde(default)]
    pub initiator: Option<OwnerAddr>,
    /// Round-1 package bytes per actor (broadcast-shaped). Public —
    /// these are commitments to the secret polynomial, not the
    /// secrets themselves; safe to persist.
    pub round1_packages: BTreeMap<OwnerAddr, Vec<u8>>,
    /// Decrypted round-2 package bytes per sender (this node's share).
    /// Populated only on the local node via `apply_with_identity`.
    ///
    /// R4 (CodeRabbit Critical): `#[serde(skip, default)]` — these are
    /// the decrypted secret share material. Persisting them to disk
    /// would leak signing-share inputs across restarts and any state
    /// export. If restart recovery for in-flight DKG is needed later,
    /// the device should re-request round-2 share distribution from
    /// the committee (or abort + restart the ceremony); we MUST NOT
    /// silently snapshot decrypted secrets onto the disk substrate.
    #[serde(skip, default)]
    pub round2_packages: BTreeMap<OwnerAddr, Vec<u8>>,
    /// Per-member `dk` confirmations: actor → claimed joint VK bytes.
    /// Conflict (≥2 distinct values) is a protocol abort.
    pub dk_confirmations: BTreeMap<OwnerAddr, [u8; 32]>,
    /// R4 (Cursor Medium): cross-confirmation consensus on per-member
    /// verifying shares. Set on the first dk that arrives for this
    /// ceremony and enforced as identical on every subsequent dk —
    /// any divergence is an InvariantViolation. On promote, this map
    /// (NOT the just-decoded payload) is the source of truth for the
    /// active committee's verifying_shares. Without this check, the
    /// dk event that happens to push confirmations to quorum can
    /// substitute incorrect per-member shares.
    ///
    /// Empty until the first dk lands. Public data (verifying shares
    /// are pubkeys, not secrets); safe to serialize.
    pub consensus_verifying_shares: BTreeMap<OwnerAddr, [u8; 32]>,
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
    /// Local node's secret FROST signing nonces (CBOR-encoded
    /// `frost::round1::SigningNonces`). Populated by
    /// `dfrost_request_vrf_beacon` (which calls `frost::round1::commit`
    /// to produce both the public commitments + the secret nonces);
    /// consumed by `dfrost_contribute_threshold_sign` (which feeds them
    /// into `frost::round2::sign`).
    ///
    /// ZEB-305 security: `#[serde(skip, default)]` — these are the
    /// local node's secret nonces. Persisting them to disk would leak
    /// signing inputs across restarts. Same security justification as
    /// `PendingDkg::round2_packages`. Restart recovery for in-flight
    /// threshold-sign ceremonies requires re-requesting (re-running
    /// `dfrost_request_vrf_beacon`); we MUST NOT silently snapshot
    /// secret nonces onto the disk substrate.
    #[serde(skip, default)]
    pub local_nonces: Option<Vec<u8>>,
}

/// Which pending-ceremony slot a `dk` event resolves to. R1 fix: refresh
/// completes via the same `dk` event kind as initial DKG, so the dispatch
/// has to inspect both slots before either rejecting (UnknownCeremony)
/// or proceeding.
#[derive(Debug, Clone, Copy)]
enum PendingSlot {
    Dkg,
    Refresh,
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
    /// ZEB-1022: a `di` (CeremonyInit) event arrived while a DIFFERENT
    /// ceremony already occupies the `pending_dkg` slot. The log never
    /// replaces an in-flight ceremony on its own — the engine's
    /// stale-replace policy decides whether to `abort_pending_dkg()`
    /// first (quiet ceremony) or drop the newcomer (fresh ceremony).
    CeremonyInFlight,
}

/// Errors surfaced by `verify_signed_committee_event`. Mirrors the
/// channel-log `ChannelEventError` shape (subset) for the envelope
/// verify chain — every variant maps to a structurally identical
/// channel-log error case (`UnknownAuthor`, `AuthorPubkeyMismatch`,
/// `BadSignature`). Decode + replay + apply errors are NOT carried
/// here — those live one level up in `process_inbound`.
#[derive(thiserror::Error, Debug)]
pub enum DfrostVerifyError {
    /// Identity resolution failed for `event.actor`. Mirrors
    /// `ChannelEventError::UnknownAuthor`.
    #[error("identity not resolvable for actor {0:?}")]
    UnknownActor(OwnerAddr),
    /// Resolver returned bytes that failed `Identity::from_public_bytes`
    /// (wrong length, or `VerifyingKey::from_bytes` rejected — non-
    /// canonical Ed25519 point). Folds into `AuthorPubkeyMismatch` on
    /// the channel-log side. Distinct here because the resolver shape
    /// is shared with `community_state_sync` and we want a clear log
    /// at the verify boundary.
    #[error("resolver returned bytes that do not parse as a 64-byte identity composite")]
    BadIdentityBytes,
    /// `identity.address_hash != event.actor.0` — the resolver
    /// returned the wrong identity for this actor (cache substitution,
    /// stale entry, or malicious resolver). Mirrors
    /// `ChannelEventError::AuthorPubkeyMismatch`.
    #[error("identity-pubkey-to-actor binding mismatch")]
    ActorAddressMismatch,
    /// `event.sig` was not 64 bytes (Ed25519 signature width).
    /// Decoder accepts arbitrary-length `Vec<u8>`; this is the first
    /// place the length is enforced.
    #[error("signature is not 64 bytes")]
    BadSignatureBytes,
    /// `signing_bytes()` re-encode failed. Should be unreachable in
    /// practice (the same struct just round-tripped through ciborium
    /// to decode), but propagated for completeness.
    #[error("signing_bytes re-encode failed: {0}")]
    SigningBytesEncode(String),
    /// Ed25519 `verify_strict` rejected the signature. Mirrors
    /// `ChannelEventError::BadSignature`.
    #[error("signature verify failed")]
    SignatureVerifyFailed,
}

/// Verify the Ed25519 envelope signature on a `SignedCommitteeEvent`.
/// Mirrors `community_channel_log::verify_channel_event` lines 628-698
/// — the resolver-shape, address-hash-binding, and `verify_strict`
/// posture are identical, modulo the channel-log-specific misroute /
/// replay / authorization steps which live one level up here (replay
/// in `DfrostReplayTracker`, apply-level membership/ceremony checks
/// in `DfrostLog::apply`).
///
/// On Ok the event is wire-valid + identity-valid + signature-valid.
/// The caller (Phase 4a engine) is responsible for the
/// replay-then-apply chain.
pub async fn verify_signed_committee_event(
    event: &SignedCommitteeEvent,
    resolver: &dyn crate::community_state_sync::IdentityResolver,
) -> Result<(), DfrostVerifyError> {
    // 1. Resolve actor → 64-byte identity composite.
    let identity_pub = resolver
        .resolve(&event.actor)
        .await
        .ok_or(DfrostVerifyError::UnknownActor(event.actor))?;

    // 2. Parse → Identity; verify address-hash binding (defence vs
    //    a buggy/compromised resolver that pairs an OwnerAddr with
    //    the wrong key — same threat model as channel-log Step 4b).
    let identity = harmony_identity::Identity::from_public_bytes(&identity_pub)
        .map_err(|_| DfrostVerifyError::BadIdentityBytes)?;
    if identity.address_hash != event.actor.0 {
        return Err(DfrostVerifyError::ActorAddressMismatch);
    }

    // 3. Parse signature into a fixed 64-byte array. `Signature::from_bytes`
    //    in `ed25519-dalek` 2.x is infallible given a `&[u8; 64]`, so the
    //    length-gate sits at the `try_into` boundary.
    let sig_bytes: [u8; 64] = event
        .sig
        .as_slice()
        .try_into()
        .map_err(|_| DfrostVerifyError::BadSignatureBytes)?;
    let signature = ed25519_dalek::Signature::from_bytes(&sig_bytes);

    // 4. `verify_strict` over canonical signing bytes (RFC 8032 strict
    //    subset — rejects non-canonical S and small-order R points,
    //    matching channel-log Step 5 and community_membership::
    //    verify_signature posture).
    let signing_bytes = event
        .signing_bytes()
        .map_err(|e| DfrostVerifyError::SigningBytesEncode(e.to_string()))?;
    identity
        .verifying_key
        .verify_strict(&signing_bytes, &signature)
        .map_err(|_| DfrostVerifyError::SignatureVerifyFailed)?;

    Ok(())
}

impl DfrostLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up the VRF output for a completed beacon identified by its beacon
    /// seed and committee epoch.
    ///
    /// Computes `message_hash = derive_vrf_seed(seed, epoch)` and checks
    /// `beacon_index[message_hash]`. Returns `Some(vrf_output)` if a
    /// beacon with that message_hash was previously applied (and indexed by
    /// `apply_vrf_beacon`), `None` otherwise.
    ///
    /// Called by `DfrostBeaconOracle<R>::vrf_output_for` to service SS1
    /// verify (Task 10). The oracle derives `epoch` from the engine's
    /// `committee_state.current_epoch` — correct for recent beacons; an
    /// epoch change (committee refresh) would cause the oracle to miss old
    /// beacons, which is the correct security posture (old-committee beacons
    /// should not drive new-committee sortition).
    pub fn find_vrf_beacon_output_by_seed(&self, seed: &[u8; 32], epoch: u64) -> Option<[u8; 32]> {
        use crate::community_dfrost_types::derive_vrf_seed;
        let message_hash = derive_vrf_seed(seed, epoch);
        self.beacon_index.get(&message_hash).copied()
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
            DfrostEventKind::CeremonyInit => self.apply_ceremony_init(&event),
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

    /// ZEB-1022: apply a `di` (CeremonyInit) event — seed `pending_dkg`
    /// with the committee shape the initiator announced.
    ///
    /// Structural validation only (this log is sans-I/O): the engine's
    /// ingest layer is responsible for the ceremony-id binding recompute
    /// (`derive_dkg_ceremony_id` needs the community's `SpaceId`, which
    /// the log does not hold) and for membership-snapshot validation
    /// (async resolver). What IS enforced here:
    ///
    /// * committee not already `active` (a fresh DKG on an active
    ///   committee would mint a new joint VK; the forward path is
    ///   proactive refresh — same rule as `dfrost_initiate_dkg`).
    /// * members sorted bytewise ascending + deduplicated (load-bearing
    ///   for `build_identifier_map` determinism), at least 2 of them,
    ///   `max_signers == members.len()`, `2 <= threshold <= max_signers`.
    /// * the initiator (`event.actor`) is itself a committee member.
    /// * `epoch == current_epoch + 1` (the one epoch a fresh DKG may
    ///   propose from this replica's view).
    ///
    /// Slot semantics: empty slot → seed (`initiator = Some(actor)`);
    /// same `ceremony_id` already pending → idempotent no-op (re-mint /
    /// re-broadcast tolerance); DIFFERENT ceremony pending →
    /// `CeremonyInFlight` (the engine's stale-replace policy calls
    /// `abort_pending_dkg()` first when it decides to admit the newcomer).
    fn apply_ceremony_init(&mut self, event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        use crate::community_dfrost_types::CeremonyInitPayload;

        let payload: CeremonyInitPayload =
            ciborium::de::from_reader(&event.payload[..]).map_err(|_| ApplyError::PayloadDecode)?;

        if self.committee_state.active {
            return Err(ApplyError::InvariantViolation);
        }

        // Shape validation.
        let mut sorted = payload.members.clone();
        sorted.sort();
        sorted.dedup();
        if sorted != payload.members || payload.members.len() < 2 {
            return Err(ApplyError::InvariantViolation);
        }
        let member_count =
            u16::try_from(payload.members.len()).map_err(|_| ApplyError::InvariantViolation)?;
        if payload.max_signers != member_count {
            return Err(ApplyError::InvariantViolation);
        }
        if payload.threshold < 2 || payload.threshold > payload.max_signers {
            return Err(ApplyError::InvariantViolation);
        }
        if !payload.members.contains(&event.actor) {
            return Err(ApplyError::InvariantViolation);
        }
        if payload.epoch != self.committee_state.current_epoch + 1 {
            return Err(ApplyError::InvariantViolation);
        }

        match self.committee_state.pending_dkg.as_ref() {
            None => {
                self.committee_state.pending_dkg = Some(PendingCeremony {
                    ceremony_id: payload.ceremony_id,
                    initiator: Some(event.actor),
                    members: payload.members,
                    threshold: payload.threshold,
                    max_signers: payload.max_signers,
                    proposed_epoch: payload.epoch,
                    ..Default::default()
                });
                Ok(())
            }
            Some(p) if p.ceremony_id == payload.ceremony_id => {
                // Idempotent re-application (initiator re-mint with a
                // fresh HLC, or a duplicate delivery). No state change —
                // in particular the accumulated round1_packages /
                // dk_confirmations survive.
                Ok(())
            }
            Some(_) => Err(ApplyError::CeremonyInFlight),
        }
    }

    /// ZEB-1022: abort the in-flight DKG ceremony, clearing the pending
    /// slot AND this node's in-memory ceremony secrets (which are bound
    /// to the aborted ceremony's polynomial and MUST NOT leak into a
    /// successor ceremony's part2/part3 inputs — FROST would reject the
    /// mismatched transcript, but only after wasted rounds).
    ///
    /// Returns the aborted `ceremony_id`, or `None` if no DKG was
    /// pending. Callers: the engine's initiator deadline-abort, its
    /// peer-side stale-replace admission, and the initiate path's
    /// rollback-on-apply-failure.
    pub fn abort_pending_dkg(&mut self) -> Option<[u8; 32]> {
        let aborted = self.committee_state.pending_dkg.take()?;
        self.local_dkg_secret = None;
        self.local_dkg_secret2 = None;
        Some(aborted.ceremony_id)
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

        // R2 (CodeRabbit Major): reject out-of-range round_num values
        // and require the per-round-required field shape. Without these
        // checks, a malformed `dr` event with round_num=99 or rn=1 with
        // no round1_package gets appended to the log carrying no usable
        // protocol state.
        match payload.round_num {
            1 => {
                let pkg = payload
                    .round1_package
                    .ok_or(ApplyError::InvariantViolation)?;
                pending.round1_packages.entry(event.actor).or_insert(pkg);
            }
            2 => {
                // Round 2 is per-recipient encrypted; the broadcast log
                // path stores nothing. Local node decryption happens in
                // `apply_with_identity`. Require at least the
                // recipient_ciphertexts vector to be Some — an rn=2 dr
                // with no ciphertexts carries no protocol state.
                if payload.recipient_ciphertexts.is_none() {
                    return Err(ApplyError::InvariantViolation);
                }
            }
            _ => return Err(ApplyError::InvariantViolation),
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

        // R1 (CodeRabbit Critical + Qodo Bug): a `dk` event can finalize
        // EITHER an in-flight DKG OR an in-flight proactive refresh —
        // both flows complete via the same `dk` event kind per the spec
        // (refresh = "DKG that preserves the joint vk"). Look up which
        // pending slot the ceremony_id binds to.
        let pending_slot = if self
            .committee_state
            .pending_dkg
            .as_ref()
            .map(|p| p.ceremony_id == payload.ceremony_id)
            .unwrap_or(false)
        {
            PendingSlot::Dkg
        } else if self
            .committee_state
            .pending_refresh
            .as_ref()
            .map(|p| p.ceremony_id == payload.ceremony_id)
            .unwrap_or(false)
        {
            PendingSlot::Refresh
        } else {
            return Err(ApplyError::UnknownCeremony);
        };

        // R5 (CodeRabbit Major): once the committee is active, refresh is
        // the only forward path. A stale or accidentally-seeded
        // pending_dkg (corrupted state, race condition, future code
        // change) MUST NOT be allowed to finalize a `dk` — that would
        // let it rewrite members / threshold / max_signers / current_epoch
        // under the existing joint VK, silently swapping the committee
        // shape. Reject loudly to surface the protocol bug.
        if self.committee_state.active && matches!(pending_slot, PendingSlot::Dkg) {
            return Err(ApplyError::InvariantViolation);
        }

        // Borrow the matching pending ceremony mutably. The branches are
        // identical except for which slot we read; collapse via a small
        // helper closure so the rest of the logic is one path.
        let pending = match pending_slot {
            PendingSlot::Dkg => self.committee_state.pending_dkg.as_mut().expect("checked"),
            PendingSlot::Refresh => self
                .committee_state
                .pending_refresh
                .as_mut()
                .expect("checked"),
        };

        // R1 (CodeRabbit Critical + Cursor High): committee shape MUST
        // match the pending ceremony's pre-declared shape. A malicious
        // member cannot redefine threshold / members / max_signers via
        // the `dk` payload — those are pinned at ceremony initiation in
        // the proposer's signed initial event.
        if payload.threshold != pending.threshold
            || payload.max_signers != pending.max_signers
            || payload.members != pending.members
            || payload.epoch != pending.proposed_epoch
        {
            return Err(ApplyError::InvariantViolation);
        }

        // R2 (CodeRabbit Major): EVERY dk event's verifying_shares MUST
        // be a 1:1 match for pending.members — no missing, duplicate, or
        // non-member entries. Validating up-front (before quorum check
        // or dk_confirmations.insert) means a malformed dk is rejected
        // before being recorded as a confirmation. Builds the
        // `new_verifying_shares` map up-front so the promote branch
        // below just consumes the pre-validated map.
        let pending_member_set: std::collections::BTreeSet<OwnerAddr> =
            pending.members.iter().copied().collect();
        let mut new_verifying_shares: BTreeMap<OwnerAddr, [u8; 32]> = BTreeMap::new();
        for mvs in &payload.verifying_shares {
            if !pending_member_set.contains(&mvs.member) {
                return Err(ApplyError::InvariantViolation);
            }
            // Duplicate member entries: BTreeMap::insert returns the
            // previous value, so a Some(_) result means we just
            // overwrote — reject loudly.
            if new_verifying_shares
                .insert(mvs.member, mvs.verifying_share)
                .is_some()
            {
                return Err(ApplyError::InvariantViolation);
            }
        }
        if new_verifying_shares.len() != pending_member_set.len() {
            // Missing entries: every pending member must have a share.
            return Err(ApplyError::InvariantViolation);
        }

        // Cross-confirmation consensus: any disagreement on the vk among
        // already-recorded confirmations aborts.
        for existing_vk in pending.dk_confirmations.values() {
            if *existing_vk != payload.joint_verifying_key {
                return Err(ApplyError::InvariantViolation);
            }
        }

        // R4 (Cursor Medium): cross-confirmation consensus on per-member
        // verifying shares. First dk sets the consensus; every subsequent
        // dk MUST claim identical shares. Without this, a malicious
        // committee member whose dk happens to trigger quorum can
        // substitute incorrect per-member verifying_shares and poison
        // CommitteeState.verifying_shares.
        if pending.consensus_verifying_shares.is_empty() {
            pending.consensus_verifying_shares = new_verifying_shares.clone();
        } else if pending.consensus_verifying_shares != new_verifying_shares {
            return Err(ApplyError::InvariantViolation);
        }

        pending
            .dk_confirmations
            .insert(event.actor, payload.joint_verifying_key);

        // Quorum reached → promote the pending ceremony to the active
        // committee state. Threshold + members + max_signers come from
        // `pending` (set at ceremony init), NOT the payload (R1 fix).
        // verifying_shares + joint_verifying_key + epoch come from the
        // payload because they're the OUTPUT of the ceremony — those
        // can only be known after the protocol runs.
        if pending.dk_confirmations.len() >= pending.threshold as usize {
            let identifier_map = CommitteeState::build_identifier_map(&pending.members);
            let members = pending.members.clone();
            let threshold = pending.threshold;
            let max_signers = pending.max_signers;
            // R4 (Cursor Medium): activate using the consensus shares
            // (set on first dk and enforced as identical on every
            // subsequent dk), NOT the just-decoded payload's shares.
            // The two are guaranteed equal by the consensus check
            // above, but reading from `pending` makes the source-of-
            // truth explicit and prevents a future refactor of the
            // payload-shape check from accidentally letting a divergent
            // share map through.
            let promoted_shares = pending.consensus_verifying_shares.clone();

            self.committee_state.active = true;
            self.committee_state.current_epoch = payload.epoch;
            self.committee_state.joint_verifying_key = Some(payload.joint_verifying_key);
            self.committee_state.verifying_shares = promoted_shares;
            self.committee_state.members = members;
            self.committee_state.threshold = threshold;
            self.committee_state.max_signers = max_signers;
            self.committee_state.identifier_map = identifier_map;
            // Clear whichever pending slot we just promoted.
            match pending_slot {
                PendingSlot::Dkg => self.committee_state.pending_dkg = None,
                PendingSlot::Refresh => self.committee_state.pending_refresh = None,
            }
        }

        Ok(())
    }

    /// Apply a `ts` event: per-member threshold-sign contribution.
    ///
    /// Verify-side guarantees (signature + actor-is-member) are upstream;
    /// here we accumulate `(commitment_bytes, share_bytes)` per actor in
    /// `pending_sign[ceremony_id].contributions`, creating the session on
    /// first contribution.
    ///
    /// Upsert semantics — the IPC pair
    /// `dfrost_request_vrf_beacon` → `dfrost_contribute_threshold_sign`
    /// emits TWO `ts` events per actor on the local node:
    ///   1. round-1: empty `share_bytes`, populated `commitment_bytes`
    ///   2. round-2: filled `share_bytes`, SAME `commitment_bytes`
    ///
    /// A naive `or_insert` would keep the first and silently drop the
    /// second — losing the signature share. We therefore upsert:
    ///   * no existing contribution → insert.
    ///   * existing empty-share + new filled share + matching commitment
    ///     → update the share_bytes (the canonical fill-in path).
    ///   * existing filled share + new filled share → `InvariantViolation`
    ///     (FROST signature-share reuse attempt is security-critical;
    ///     allowing this would let a malicious peer swap shares mid-ceremony
    ///     after the first share committed the aggregator to one transcript).
    ///   * new empty share when an entry already exists → idempotent no-op
    ///     (peer re-broadcast of the round-1 ts; HLC LWW dedupes upstream
    ///     but a single-process / late-arrival path may still hand it here).
    ///   * existing entry with different `commitment_bytes` → `InvariantViolation`
    ///     (peer attempted to swap commitments after the first round-1 ts).
    fn apply_threshold_sign(&mut self, event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        use crate::community_dfrost_types::ThresholdSignPayload;

        let payload: ThresholdSignPayload =
            ciborium::de::from_reader(&event.payload[..]).map_err(|_| ApplyError::PayloadDecode)?;

        // A `ts` event before the committee is active is malformed.
        if !self.committee_state.active {
            return Err(ApplyError::InvariantViolation);
        }

        let session = self
            .committee_state
            .pending_sign
            .entry(payload.ceremony_id)
            .or_insert_with(|| PendingSignSession {
                message_hash: payload.message_hash,
                contributions: BTreeMap::new(),
                local_nonces: None,
            });
        // First-write-wins on message_hash — if a later `ts` claims a
        // different message for the same ceremony, that's an invariant
        // violation (the ceremony shape is set when the first contribution
        // lands).
        if session.message_hash != payload.message_hash {
            return Err(ApplyError::InvariantViolation);
        }

        match session.contributions.get(&event.actor) {
            None => {
                // First contribution from this actor — insert as-is.
                session
                    .contributions
                    .insert(event.actor, (payload.commitment_bytes, payload.share_bytes));
            }
            Some((existing_commit, existing_share)) => {
                // Commitment must match across both round-1 (empty share)
                // and round-2 (filled share) ts events from the same actor.
                // A peer that swaps commitment_bytes mid-ceremony is
                // malformed.
                if existing_commit != &payload.commitment_bytes {
                    return Err(ApplyError::InvariantViolation);
                }
                match (existing_share.is_empty(), payload.share_bytes.is_empty()) {
                    (true, false) => {
                        // Canonical empty → filled upsert: round-2 ts
                        // arriving after round-1. Update share_bytes
                        // in place; commitment is already equal.
                        session
                            .contributions
                            .insert(event.actor, (payload.commitment_bytes, payload.share_bytes));
                    }
                    (false, false) => {
                        // Existing filled share + new filled share.
                        // Idempotent if byte-identical (peer re-broadcast
                        // of the round-2 ts after HLC LWW failed to
                        // dedupe upstream); otherwise a signature-share
                        // reuse attempt that we MUST reject — accepting a
                        // second distinct share would let a malicious
                        // peer fork the transcript after the aggregator
                        // committed to the first.
                        if existing_share != &payload.share_bytes {
                            return Err(ApplyError::InvariantViolation);
                        }
                        // byte-identical → no-op
                    }
                    (true, true) | (false, true) => {
                        // Either the round-1 ts re-arrived (true, true) or
                        // the round-2 ts re-arrived and is now followed
                        // by a stale round-1 (false, true). Both are
                        // idempotent no-ops; never downgrade a filled
                        // share back to empty.
                    }
                }
            }
        }
        Ok(())
    }

    /// Apply a `vb` event: the committee's aggregated VRF beacon.
    ///
    /// Verifies that the payload's `vrf_output` matches the
    /// `derive_vrf_output(R_compressed)` recomputation. `R_compressed` is
    /// the first 32 bytes of the Schnorr signature (the R component). If
    /// the binding fails, reject — a malformed beacon must not poison
    /// downstream consumers.
    fn apply_vrf_beacon(&mut self, event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        use crate::community_dfrost_types::{derive_vrf_output, VrfBeaconPayload};

        let payload: VrfBeaconPayload =
            ciborium::de::from_reader(&event.payload[..]).map_err(|_| ApplyError::PayloadDecode)?;

        if !self.committee_state.active {
            return Err(ApplyError::InvariantViolation);
        }

        // R1 (CodeRabbit Major): a `vb` event MUST reference a known
        // pending sign session AND its claimed message_hash MUST match
        // the session's pinned message_hash. Without these checks, a
        // malicious peer can broadcast a `vb` for an unknown ceremony
        // or for a different message than the committee actually
        // signed, and the log silently accepts it.
        let session = self
            .committee_state
            .pending_sign
            .get(&payload.ceremony_id)
            .ok_or(ApplyError::UnknownCeremony)?;
        if session.message_hash != payload.message_hash {
            return Err(ApplyError::InvariantViolation);
        }

        // Schnorr signature is 64 bytes: R (32) || s (32). Anything else
        // is malformed.
        if payload.signature.len() != 64 {
            return Err(ApplyError::InvariantViolation);
        }
        let mut r_compressed = [0u8; 32];
        r_compressed.copy_from_slice(&payload.signature[..32]);
        if derive_vrf_output(&r_compressed) != payload.vrf_output {
            return Err(ApplyError::InvariantViolation);
        }

        // R2 (CodeRabbit Critical): verify the full Schnorr signature
        // against the committee's joint verifying key. Without this,
        // any 64-byte blob whose first 32 bytes hash to `vrf_output`
        // would be accepted — the VRF-output binding check alone is
        // not sufficient to prove the committee actually produced a
        // valid threshold signature on the agreed message.
        let joint_vk = self
            .committee_state
            .joint_verifying_key
            .ok_or(ApplyError::InvariantViolation)?;
        crate::community_dfrost_crypto::verify_schnorr_signature(
            &joint_vk,
            &payload.message_hash,
            &payload.signature,
        )
        .map_err(|_| ApplyError::InvariantViolation)?;

        // Index the completed beacon so `find_vrf_beacon_output_by_seed` can
        // answer oracle lookups without re-scanning `events`. Key is
        // `message_hash` (= `derive_vrf_seed(seed_bytes, epoch)`); value is
        // the verified `vrf_output`. Inserted before clearing pending_sign.
        self.beacon_index
            .insert(payload.message_hash, payload.vrf_output);

        // Clear the pending sign session — the ceremony is now finalised
        // on this replica.
        self.committee_state
            .pending_sign
            .remove(&payload.ceremony_id);
        Ok(())
    }

    /// Apply an `rf` event: proactive committee refresh round.
    ///
    /// rn=1: initialise `pending_refresh` with `proposed_epoch =
    ///   current_epoch + 1`. Recipient ciphertext storage is reserved for
    ///   the local node's apply path (`apply_with_identity`).
    /// rn=2: store the broadcast round-2 package alongside the pending
    ///   refresh state.
    ///
    /// The refresh ceremony FINISHES via a `dk` event whose
    /// `joint_verifying_key` MUST equal the existing active vk
    /// (`apply_dkg_complete`'s invariant check enforces this). The dk's
    /// epoch must be `current_epoch + 1`.
    fn apply_proactive_refresh(&mut self, event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        use crate::community_dfrost_types::RefreshRoundPayload;

        let payload: RefreshRoundPayload =
            ciborium::de::from_reader(&event.payload[..]).map_err(|_| ApplyError::PayloadDecode)?;

        if !self.committee_state.active {
            return Err(ApplyError::InvariantViolation);
        }

        // R2 (CodeRabbit Major): exhaustive round_num match + per-round
        // payload-shape validation. Out-of-range round_num is rejected
        // with InvariantViolation rather than silently appended.
        match payload.round_num {
            1 => {
                // rn=1 of refresh requires recipient_ciphertexts — the
                // proposer is distributing new shares.
                if payload.recipient_ciphertexts.is_none() {
                    return Err(ApplyError::InvariantViolation);
                }
                // Initialise the pending refresh once on the first rn=1
                // event. Subsequent rn=1 events (from other actors)
                // reuse the existing pending_refresh — only the local
                // node's per-recipient ciphertext decode happens in
                // `apply_with_identity`.
                if self.committee_state.pending_refresh.is_none() {
                    self.committee_state.pending_refresh = Some(PendingCeremony {
                        ceremony_id: payload.ceremony_id,
                        members: self.committee_state.members.clone(),
                        threshold: self.committee_state.threshold,
                        max_signers: self.committee_state.max_signers,
                        proposed_epoch: self.committee_state.current_epoch + 1,
                        ..Default::default()
                    });
                }
                // Subsequent enforcement: ceremony_id must match. A
                // divergent ceremony_id within the same refresh epoch
                // indicates a forked proposer; reject to surface the
                // protocol bug rather than silently apply.
                if let Some(pr) = &self.committee_state.pending_refresh {
                    if pr.ceremony_id != payload.ceremony_id {
                        return Err(ApplyError::InvariantViolation);
                    }
                }
            }
            2 => {
                // Round-2 is broadcast in the spec's wire shape but the
                // actual share material is per-recipient encrypted via
                // recipient_ciphertexts; the global apply path stores
                // nothing here. Local decryption happens in
                // `apply_with_identity`. Require the round2 package
                // field shape to be present even if we don't process
                // it here, so a wholly-malformed rn=2 is rejected.
                let pr = self
                    .committee_state
                    .pending_refresh
                    .as_ref()
                    .ok_or(ApplyError::UnknownCeremony)?;
                if pr.ceremony_id != payload.ceremony_id {
                    return Err(ApplyError::UnknownCeremony);
                }
                if payload.round2_package.is_none() && payload.recipient_ciphertexts.is_none() {
                    return Err(ApplyError::InvariantViolation);
                }
            }
            _ => return Err(ApplyError::InvariantViolation),
        }
        Ok(())
    }

    /// Local-node apply path. Identical to `apply` except DkgRound rn=2
    /// (and ProactiveRefresh rn=1 with `recipient_ciphertexts`) decrypt
    /// the per-recipient sealed package matching `self_addr` and store
    /// the plaintext bytes in `pending_dkg.round2_packages[event.actor]`
    /// (or `pending_refresh.round2_packages[event.actor]`).
    ///
    /// `self_x25519_priv` is this node's X25519 private key derived from
    /// the device identity (see `dm_signing::derive_x25519_priv`). The
    /// non-local broadcast apply path (`apply`) cannot decrypt these
    /// ciphertexts, so this method is the only path that materialises the
    /// local share material needed for DKG part2/part3.
    pub fn apply_with_identity(
        &mut self,
        event: SignedCommitteeEvent,
        self_addr: &OwnerAddr,
        self_x25519_priv: &[u8; 32],
    ) -> Result<(), ApplyError> {
        use crate::community_dfrost_types::{DkgRoundPayload, RefreshRoundPayload};
        use crate::dm_signing;

        // Envelope checks duplicated from `apply` so the early-bail paths
        // surface the same diagnostics.
        if event.tag != 'd' || event.committee_tier != 0 {
            return Err(ApplyError::UnexpectedEnvelope);
        }

        match event.kind {
            DfrostEventKind::DkgRound => {
                let payload: DkgRoundPayload = ciborium::de::from_reader(&event.payload[..])
                    .map_err(|_| ApplyError::PayloadDecode)?;
                if payload.round_num == 2 {
                    let pending = self
                        .committee_state
                        .pending_dkg
                        .as_mut()
                        .ok_or(ApplyError::UnknownCeremony)?;
                    if pending.ceremony_id != payload.ceremony_id {
                        return Err(ApplyError::UnknownCeremony);
                    }
                    // R3 (Cursor Medium): rn=2 MUST carry
                    // recipient_ciphertexts. The broadcast `apply` path
                    // (`apply_dkg_round`) already rejects rn=2 without
                    // ciphertexts as InvariantViolation; this local path
                    // must apply the same rule, otherwise an attacker
                    // can broadcast a malformed rn=2 that lands on the
                    // local replica (via this path) but is rejected by
                    // every peer (via the broadcast path) — event-log
                    // divergence.
                    let cts = payload
                        .recipient_ciphertexts
                        .ok_or(ApplyError::InvariantViolation)?;
                    for ct in cts {
                        if ct.recipient == *self_addr {
                            let plaintext =
                                dm_signing::open_from_owner(self_x25519_priv, &ct.sealed)
                                    .map_err(|_| ApplyError::PayloadDecode)?;
                            pending
                                .round2_packages
                                .entry(event.actor)
                                .or_insert(plaintext);
                            break;
                        }
                    }
                    self.events.push(event);
                    return Ok(());
                }
                // For round_num != 2, fall back to the broadcast apply path.
                self.apply(event)
            }
            DfrostEventKind::ProactiveRefresh => {
                let payload: RefreshRoundPayload = ciborium::de::from_reader(&event.payload[..])
                    .map_err(|_| ApplyError::PayloadDecode)?;
                if payload.round_num == 1 {
                    // R3 (CodeRabbit Major): stage the local decrypt
                    // BEFORE mutating pending_refresh. Previously
                    // `apply_proactive_refresh(&event)` was called
                    // first (which initializes pending_refresh), then
                    // decrypt was attempted — if decrypt failed, the
                    // event would be rejected but pending_refresh had
                    // already advanced, leaving the materialized state
                    // out of sync with the (rejected) event log.
                    //
                    // The broadcast `apply_proactive_refresh` already
                    // requires recipient_ciphertexts.is_some() for rn=1,
                    // so we can take that requirement here too — and
                    // do the decrypt early so a decrypt failure short-
                    // circuits before mutation.
                    let cts = payload
                        .recipient_ciphertexts
                        .as_ref()
                        .ok_or(ApplyError::InvariantViolation)?;
                    let decrypted_for_self: Option<Vec<u8>> = cts
                        .iter()
                        .find(|ct| ct.recipient == *self_addr)
                        .map(|ct| dm_signing::open_from_owner(self_x25519_priv, &ct.sealed))
                        .transpose()
                        .map_err(|_| ApplyError::PayloadDecode)?;

                    // Decrypt succeeded (or no ciphertext targeted self
                    // — non-committee replicas in this refresh round
                    // legitimately have nothing to decrypt). Safe to
                    // mutate state now.
                    self.apply_proactive_refresh(&event)?;
                    if let Some(plaintext) = decrypted_for_self {
                        let pending = self
                            .committee_state
                            .pending_refresh
                            .as_mut()
                            .ok_or(ApplyError::UnknownCeremony)?;
                        pending
                            .round2_packages
                            .entry(event.actor)
                            .or_insert(plaintext);
                    }
                    self.events.push(event);
                    return Ok(());
                }
                self.apply(event)
            }
            _ => self.apply(event),
        }
    }
}

/// Build a fully-signed `SignedCommitteeEvent` for a D-FROST event, ready
/// to broadcast / apply locally. Mirrors `community_voting_core::
/// build_signed_poll_create_tier1` — encodes the kind-specific payload
/// via ciborium, builds the envelope with a placeholder signature,
/// computes `signing_bytes()`, signs with the supplied Ed25519 key, and
/// returns the completed event.
///
/// The envelope `tag` is hardcoded to `'d'` (D-FROST) and `committee_tier`
/// to `0`, matching the wire-format invariants enforced by
/// `DfrostLog::apply` (which rejects any other tag / non-zero tier as
/// `ApplyError::UnexpectedEnvelope`).
pub fn build_signed_dfrost_event<P: serde::Serialize>(
    keypair: &ed25519_dalek::SigningKey,
    actor: crate::owner_state_types::OwnerAddr,
    kind: crate::community_dfrost_types::DfrostEventKind,
    payload: &P,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_dfrost_types::SignedCommitteeEvent, String> {
    use ed25519_dalek::Signer;
    let mut payload_bytes = Vec::new();
    ciborium::ser::into_writer(payload, &mut payload_bytes)
        .map_err(|e| format!("encode payload: {e}"))?;
    let mut ev = crate::community_dfrost_types::SignedCommitteeEvent {
        tag: 'd',
        version: 1,
        committee_tier: 0,
        kind,
        hlc,
        actor,
        payload: payload_bytes,
        sig: vec![0u8; 64],
    };
    let sb = ev
        .signing_bytes()
        .map_err(|e| format!("signing_bytes: {e}"))?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// ZEB-1022: re-mint an existing self-authored event with a fresh HLC +
/// signature, leaving the payload bytes untouched. Used by the ceremony
/// re-broadcast path: peers' `DfrostReplayTracker` keys `(actor,
/// device_id) → max HLC`, so re-publishing the ORIGINAL bytes is dropped
/// as a replay by any peer that has already seen a newer event from this
/// actor — exactly the late-subscriber case re-broadcast exists to heal.
/// A fresh HLC clears the tracker while the structural first-wins maps
/// (`round1_packages` `.or_insert`, `dk_confirmations` insert, `di`
/// same-id no-op) keep the re-application idempotent on peers that
/// already applied the original.
pub fn resign_dfrost_event_with_fresh_hlc(
    event: &crate::community_dfrost_types::SignedCommitteeEvent,
    fresh_hlc: crate::owner_state_types::Hlc,
    keypair: &ed25519_dalek::SigningKey,
) -> Result<crate::community_dfrost_types::SignedCommitteeEvent, String> {
    use ed25519_dalek::Signer;
    let mut ev = event.clone();
    ev.hlc = fresh_hlc;
    let sb = ev
        .signing_bytes()
        .map_err(|e| format!("signing_bytes: {e}"))?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
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
    fn apply_with_identity_decrypts_round2_package() {
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgRoundPayload, SignedCommitteeEvent,
        };
        use crate::community_membership::RecipientCiphertext;
        use crate::dm_signing;
        use crate::owner_state_types::Hlc;
        use x25519_dalek::{PublicKey, StaticSecret};

        let alice = OwnerAddr([0x01; 16]);
        let alice_priv = [0x42u8; 32];
        let alice_x25519_pub = *PublicKey::from(&StaticSecret::from(alice_priv)).as_bytes();

        let fake_r2_pkg_bytes = vec![0xca, 0xfe, 0xba, 0xbe];
        let sealed =
            dm_signing::seal_to_owner(&alice_x25519_pub, &fake_r2_pkg_bytes).expect("seal");

        let payload = DkgRoundPayload {
            ceremony_id: [0x42u8; 32],
            round_num: 2,
            round1_package: None,
            recipient_ciphertexts: Some(vec![RecipientCiphertext {
                recipient: alice,
                sealed,
            }]),
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        let ev = SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgRound,
            hlc: Hlc {
                wall_ms: 3000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: OwnerAddr([0x02; 16]),
            payload: pd,
            sig: vec![0u8; 64],
        };

        let mut log = DfrostLog::new();
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id: [0x42u8; 32],
            members: vec![alice, OwnerAddr([0x02; 16])],
            threshold: 1,
            max_signers: 2,
            proposed_epoch: 1,
            ..Default::default()
        });

        log.apply_with_identity(ev, &alice, &alice_priv)
            .expect("apply with identity");

        let r2_pkgs = &log
            .committee_state
            .pending_dkg
            .as_ref()
            .unwrap()
            .round2_packages;
        assert_eq!(
            r2_pkgs.get(&OwnerAddr([0x02; 16])),
            Some(&fake_r2_pkg_bytes)
        );
    }

    #[test]
    fn ts_contributions_accumulate_in_pending_sign() {
        use crate::community_dfrost_types::{
            DfrostEventKind, SignedCommitteeEvent, ThresholdSignPayload,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let ceremony_id = [0xcc; 32];
        let msg_hash = [0xde; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.members = vec![alice];

        let payload = ThresholdSignPayload {
            ceremony_id,
            message_hash: msg_hash,
            commitment_bytes: vec![0x01, 0x02],
            share_bytes: vec![0x03, 0x04],
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();

        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms: 4000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![0u8; 64],
        })
        .expect("apply ts");

        let session = log.committee_state.pending_sign.get(&ceremony_id).unwrap();
        assert_eq!(session.message_hash, msg_hash);
        let (cm, sh) = session.contributions.get(&alice).unwrap();
        assert_eq!(cm, &vec![0x01u8, 0x02]);
        assert_eq!(sh, &vec![0x03u8, 0x04]);
    }

    /// R1 (round-1 bot-review CRITICAL): `apply_threshold_sign` must
    /// UPSERT the share when the share-bearing round-2 `ts` arrives after
    /// the empty-share round-1 `ts` from the same actor. Previously the
    /// `or_insert` pattern dropped the second event silently, losing the
    /// signature share and breaking the threshold ceremony.
    #[test]
    fn ts_round2_filled_share_upserts_over_round1_empty_share() {
        use crate::community_dfrost_types::{
            DfrostEventKind, SignedCommitteeEvent, ThresholdSignPayload,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let ceremony_id = [0xcc; 32];
        let msg_hash = [0xde; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.members = vec![alice];

        // Round 1: empty share + populated commitment.
        let p1 = ThresholdSignPayload {
            ceremony_id,
            message_hash: msg_hash,
            commitment_bytes: vec![0xaa, 0xbb],
            share_bytes: Vec::new(),
        };
        let mut pd1 = Vec::new();
        ciborium::into_writer(&p1, &mut pd1).unwrap();
        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms: 4000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd1,
            sig: vec![0u8; 64],
        })
        .expect("apply round-1 ts");

        // Round 2: SAME commitment, populated share.
        let p2 = ThresholdSignPayload {
            ceremony_id,
            message_hash: msg_hash,
            commitment_bytes: vec![0xaa, 0xbb],
            share_bytes: vec![0x11, 0x22, 0x33],
        };
        let mut pd2 = Vec::new();
        ciborium::into_writer(&p2, &mut pd2).unwrap();
        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms: 5000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd2,
            sig: vec![0u8; 64],
        })
        .expect("apply round-2 ts (upsert)");

        let session = log.committee_state.pending_sign.get(&ceremony_id).unwrap();
        let (cm, sh) = session.contributions.get(&alice).unwrap();
        assert_eq!(
            cm,
            &vec![0xaau8, 0xbb],
            "commitment preserved across upsert"
        );
        assert_eq!(
            sh,
            &vec![0x11u8, 0x22, 0x33],
            "share_bytes filled by round-2 upsert"
        );
    }

    /// R1 CRITICAL: two distinct filled shares from the same actor must
    /// be rejected — FROST signature-share reuse is security-critical and
    /// allowing a swap would let a malicious peer fork the transcript
    /// after the aggregator committed to the first share.
    #[test]
    fn ts_second_filled_share_with_different_bytes_rejected() {
        use crate::community_dfrost_types::{
            DfrostEventKind, SignedCommitteeEvent, ThresholdSignPayload,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let ceremony_id = [0xcc; 32];
        let msg_hash = [0xde; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.members = vec![alice];

        for (wall, share) in [(4000u64, vec![0x11u8, 0x22]), (5000, vec![0x99, 0xaa])]
            .iter()
            .cloned()
        {
            let p = ThresholdSignPayload {
                ceremony_id,
                message_hash: msg_hash,
                commitment_bytes: vec![0xaa, 0xbb],
                share_bytes: share,
            };
            let mut pd = Vec::new();
            ciborium::into_writer(&p, &mut pd).unwrap();
            let r = log.apply(SignedCommitteeEvent {
                tag: 'd',
                version: 1,
                committee_tier: 0,
                kind: DfrostEventKind::ThresholdSign,
                hlc: Hlc {
                    wall_ms: wall,
                    logical: 0,
                    device_id: "t".into(),
                },
                actor: alice,
                payload: pd,
                sig: vec![0u8; 64],
            });
            if wall == 4000 {
                r.expect("first filled-share ts accepted");
            } else {
                assert_eq!(
                    r,
                    Err(ApplyError::InvariantViolation),
                    "second distinct filled share must be rejected"
                );
            }
        }
    }

    /// R1 CRITICAL: a `ts` event whose `commitment_bytes` differ from
    /// the actor's existing contribution must be rejected — a peer that
    /// swaps commitments mid-ceremony is malformed and accepting it
    /// would silently invalidate the SigningPackage built from peer
    /// commitments.
    #[test]
    fn ts_with_mismatched_commitment_bytes_rejected() {
        use crate::community_dfrost_types::{
            DfrostEventKind, SignedCommitteeEvent, ThresholdSignPayload,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let ceremony_id = [0xcc; 32];
        let msg_hash = [0xde; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.members = vec![alice];

        // Round 1 ts.
        let p1 = ThresholdSignPayload {
            ceremony_id,
            message_hash: msg_hash,
            commitment_bytes: vec![0xaa, 0xbb],
            share_bytes: Vec::new(),
        };
        let mut pd1 = Vec::new();
        ciborium::into_writer(&p1, &mut pd1).unwrap();
        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms: 4000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd1,
            sig: vec![0u8; 64],
        })
        .expect("apply round-1 ts");

        // Round 2 with DIFFERENT commitment_bytes.
        let p2 = ThresholdSignPayload {
            ceremony_id,
            message_hash: msg_hash,
            commitment_bytes: vec![0xcc, 0xdd],
            share_bytes: vec![0x11, 0x22],
        };
        let mut pd2 = Vec::new();
        ciborium::into_writer(&p2, &mut pd2).unwrap();
        let r = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms: 5000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd2,
            sig: vec![0u8; 64],
        });
        assert_eq!(r, Err(ApplyError::InvariantViolation));
    }

    /// R1: an empty-share `ts` event arriving AFTER a filled-share `ts`
    /// is an idempotent no-op — must never downgrade the actor's
    /// recorded share back to empty (which would silently break
    /// aggregation).
    #[test]
    fn ts_late_empty_share_does_not_downgrade_existing_filled_share() {
        use crate::community_dfrost_types::{
            DfrostEventKind, SignedCommitteeEvent, ThresholdSignPayload,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let ceremony_id = [0xcc; 32];
        let msg_hash = [0xde; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.members = vec![alice];

        // Round 2 lands first (e.g., HLC LWW ordering quirk).
        let p_filled = ThresholdSignPayload {
            ceremony_id,
            message_hash: msg_hash,
            commitment_bytes: vec![0xaa, 0xbb],
            share_bytes: vec![0x11, 0x22, 0x33],
        };
        let mut pd_f = Vec::new();
        ciborium::into_writer(&p_filled, &mut pd_f).unwrap();
        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms: 4000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd_f,
            sig: vec![0u8; 64],
        })
        .expect("filled-share ts accepted");

        // Late-arriving empty-share ts (peer rebroadcast / log replay).
        let p_empty = ThresholdSignPayload {
            ceremony_id,
            message_hash: msg_hash,
            commitment_bytes: vec![0xaa, 0xbb],
            share_bytes: Vec::new(),
        };
        let mut pd_e = Vec::new();
        ciborium::into_writer(&p_empty, &mut pd_e).unwrap();
        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms: 5000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd_e,
            sig: vec![0u8; 64],
        })
        .expect("late empty-share ts is idempotent no-op");

        // Share MUST still be the filled one — never downgraded.
        let session = log.committee_state.pending_sign.get(&ceremony_id).unwrap();
        let (_, sh) = session.contributions.get(&alice).unwrap();
        assert_eq!(
            sh,
            &vec![0x11u8, 0x22, 0x33],
            "late empty-share ts must not downgrade filled share"
        );
    }

    #[test]
    fn vb_with_wrong_vrf_output_rejected() {
        use crate::community_dfrost_types::{
            derive_vrf_output, DfrostEventKind, SignedCommitteeEvent, VrfBeaconPayload,
        };
        use crate::owner_state_types::Hlc;

        let ceremony_id = [0xcc; 32];
        let msg_hash = [0xde; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        // R1 update (CodeRabbit Major): seed a matching pending_sign
        // session so the new orphan-ceremony check passes and we
        // exercise the VRF-output binding check specifically (this
        // test's original target).
        log.committee_state.pending_sign.insert(
            ceremony_id,
            PendingSignSession {
                message_hash: msg_hash,
                contributions: BTreeMap::new(),
                local_nonces: None,
            },
        );

        let sig_bytes = vec![0xaau8; 64];
        let correct_vrf = derive_vrf_output(&sig_bytes[..32].try_into().unwrap());
        let wrong_vrf = [0xff; 32];
        assert_ne!(correct_vrf, wrong_vrf);

        let payload = VrfBeaconPayload {
            ceremony_id,
            message_hash: msg_hash,
            signature: sig_bytes,
            vrf_output: wrong_vrf,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();

        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::VrfBeacon,
            hlc: Hlc {
                wall_ms: 5000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: OwnerAddr([0x01; 16]),
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
    }

    #[test]
    fn rf_rn1_event_starts_pending_refresh() {
        use crate::community_dfrost_types::{
            DfrostEventKind, RefreshRoundPayload, SignedCommitteeEvent,
        };
        use crate::community_membership::RecipientCiphertext;
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let ceremony_id = [0x77u8; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = vec![alice];
        log.committee_state.threshold = 1;
        log.committee_state.max_signers = 1;

        let payload = RefreshRoundPayload {
            ceremony_id,
            round_num: 1,
            recipient_ciphertexts: Some(vec![RecipientCiphertext {
                recipient: alice,
                sealed: vec![0xde, 0xad],
            }]),
            round2_package: None,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();

        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ProactiveRefresh,
            hlc: Hlc {
                wall_ms: 6000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![0u8; 64],
        })
        .expect("apply rf rn=1");

        assert!(log.committee_state.pending_refresh.is_some());
        let pr = log.committee_state.pending_refresh.as_ref().unwrap();
        assert_eq!(pr.ceremony_id, ceremony_id);
        assert_eq!(pr.proposed_epoch, 2);
    }

    /// R1 (CodeRabbit Critical): refresh completion routes through
    /// `pending_refresh`, NOT `pending_dkg`. The `dk` event kind is
    /// shared between initial DKG and proactive refresh; the slot
    /// resolution happens by ceremony_id lookup. Previously this test
    /// (wrongly) seeded `pending_dkg` for refresh — that worked because
    /// the bug under review was that ONLY `pending_dkg` was ever
    /// consulted. Now we exercise the real refresh path.
    #[test]
    fn refresh_completion_preserves_joint_vk() {
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgCompletePayload, MemberVerifyingShare, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let existing_vk = [0x11u8; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.joint_verifying_key = Some(existing_vk);
        log.committee_state.members = vec![alice];
        log.committee_state.threshold = 1;
        log.committee_state.max_signers = 1;
        // R1 fix: refresh uses pending_refresh, not pending_dkg.
        log.committee_state.pending_refresh = Some(PendingCeremony {
            ceremony_id: [0x88; 32],
            members: vec![alice],
            threshold: 1,
            max_signers: 1,
            proposed_epoch: 2,
            ..Default::default()
        });

        let dk_payload = DkgCompletePayload {
            ceremony_id: [0x88; 32],
            joint_verifying_key: existing_vk,
            verifying_shares: vec![MemberVerifyingShare {
                member: alice,
                verifying_share: [0xbb; 32],
            }],
            epoch: 2,
            members: vec![alice],
            threshold: 1,
            max_signers: 1,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&dk_payload, &mut pd).unwrap();

        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: Hlc {
                wall_ms: 7000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![0u8; 64],
        })
        .expect("refresh dk accepted");

        assert_eq!(log.committee_state.current_epoch, 2);
        assert_eq!(log.committee_state.joint_verifying_key, Some(existing_vk));
        assert!(
            log.committee_state.pending_refresh.is_none(),
            "completed refresh clears its pending slot"
        );
    }

    /// R1 (CodeRabbit Critical / Cursor High): `dk` payload MUST NOT
    /// redefine committee shape. A malicious member sending threshold=1
    /// against a pending ceremony with threshold=2 would otherwise
    /// finalize the committee on a single confirmation. Reject as
    /// InvariantViolation.
    #[test]
    fn dk_with_payload_threshold_mismatch_rejected() {
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgCompletePayload, MemberVerifyingShare, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let mut log = DfrostLog::new();
        // 2-of-2 ceremony pending.
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id: [0xab; 32],
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
            proposed_epoch: 1,
            ..Default::default()
        });

        // Attacker dk claims threshold=1 against the threshold=2 ceremony.
        let dk_payload = DkgCompletePayload {
            ceremony_id: [0xab; 32],
            joint_verifying_key: [0x99u8; 32],
            verifying_shares: vec![
                MemberVerifyingShare {
                    member: alice,
                    verifying_share: [0xa1; 32],
                },
                MemberVerifyingShare {
                    member: bob,
                    verifying_share: [0xb2; 32],
                },
            ],
            epoch: 1,
            members: vec![alice, bob],
            threshold: 1, // ← MISMATCH against pending.threshold=2
            max_signers: 2,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&dk_payload, &mut pd).unwrap();

        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: Hlc {
                wall_ms: 8000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
        assert!(
            !log.committee_state.active,
            "rejected dk must not promote the committee"
        );
    }

    /// R1 (CodeRabbit Major): `vb` event for an unknown sign ceremony
    /// is rejected. Previously such events silently appended to the log,
    /// allowing a malicious peer to inject phantom beacons.
    #[test]
    fn vb_with_unknown_ceremony_rejected() {
        use crate::community_dfrost_types::{
            derive_vrf_output, DfrostEventKind, SignedCommitteeEvent, VrfBeaconPayload,
        };
        use crate::owner_state_types::Hlc;

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        // No pending_sign session — beacon should be rejected.

        let r_compressed = [0x77u8; 32];
        let mut sig = vec![0u8; 64];
        sig[..32].copy_from_slice(&r_compressed);
        let payload = VrfBeaconPayload {
            ceremony_id: [0xee; 32],
            message_hash: [0xde; 32],
            signature: sig,
            vrf_output: derive_vrf_output(&r_compressed),
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();

        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::VrfBeacon,
            hlc: Hlc {
                wall_ms: 9000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: OwnerAddr([0x01; 16]),
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::UnknownCeremony));
    }

    /// R2 (CodeRabbit Critical): `vb` event with bogus signature bytes
    /// that happen to pass the VRF-output derivation check is now
    /// rejected by the Schnorr verify step against the joint
    /// verifying key. Pre-R2 fix, any 64-byte blob whose first 32
    /// bytes hash to `payload.vrf_output` was accepted; that's a
    /// trivial forgery primitive for VRF beacons.
    #[test]
    fn vb_with_bogus_signature_bytes_rejected_at_schnorr_verify() {
        use crate::community_dfrost_types::{
            derive_vrf_output, DfrostEventKind, SignedCommitteeEvent, VrfBeaconPayload,
        };
        use crate::owner_state_types::Hlc;

        let ceremony_id = [0xcc; 32];
        let msg_hash = [0xde; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        // Seed a joint_verifying_key so the Schnorr-verify step has a
        // key to deserialise. We use [0x55; 32] which is unlikely to
        // be a valid Ristretto compressed point — `VerifyingKey::
        // deserialize` returns Err, the wrapper returns Err, and the
        // apply path maps Err to InvariantViolation. Either way, the
        // bogus signature is rejected before clearing pending_sign.
        log.committee_state.joint_verifying_key = Some([0x55u8; 32]);
        log.committee_state.pending_sign.insert(
            ceremony_id,
            PendingSignSession {
                message_hash: msg_hash,
                contributions: BTreeMap::new(),
                local_nonces: None,
            },
        );

        // Construct a payload where the VRF-output derivation check
        // PASSES (vrf_output is the legitimate hash of R), but the
        // Schnorr verify step MUST reject. Pre-R2, this would have
        // landed the beacon.
        let r_compressed = [0x77u8; 32];
        let mut sig = vec![0u8; 64];
        sig[..32].copy_from_slice(&r_compressed);
        let payload = VrfBeaconPayload {
            ceremony_id,
            message_hash: msg_hash,
            signature: sig,
            vrf_output: derive_vrf_output(&r_compressed),
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();

        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::VrfBeacon,
            hlc: Hlc {
                wall_ms: 11_000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: OwnerAddr([0x01; 16]),
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
        // Pending sign session MUST NOT be cleared on rejection — it
        // remains in-flight so a legitimate beacon can still land.
        assert!(log.committee_state.pending_sign.contains_key(&ceremony_id));
    }

    /// R5 (CodeRabbit Major): once the committee is active, a stale or
    /// accidentally-seeded `pending_dkg` MUST NOT be allowed to finalize.
    /// Only `pending_refresh` is a legitimate forward path after
    /// activation. Without this guard, a stale initial-DKG dk that
    /// happens to share the active joint vk could rewrite members /
    /// threshold / max_signers under the existing key, silently
    /// swapping committee shape.
    #[test]
    fn dk_against_pending_dkg_after_activation_rejected() {
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgCompletePayload, MemberVerifyingShare, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let active_vk = [0x11u8; 32];

        let mut log = DfrostLog::new();
        // Committee is already active (e.g., initial DKG completed).
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.joint_verifying_key = Some(active_vk);
        log.committee_state.members = vec![alice];
        log.committee_state.threshold = 1;
        log.committee_state.max_signers = 1;

        // Simulated bug / corruption: pending_dkg is somehow non-None
        // post-activation. The IPC layer should never produce this state,
        // but defence-in-depth means apply must reject.
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id: [0x77; 32],
            members: vec![alice],
            threshold: 1,
            max_signers: 1,
            proposed_epoch: 2,
            ..Default::default()
        });

        // A dk targeting the stale pending_dkg, claiming the same vk
        // (so the vk-mutation check doesn't catch it).
        let dk_payload = DkgCompletePayload {
            ceremony_id: [0x77; 32],
            joint_verifying_key: active_vk,
            verifying_shares: vec![MemberVerifyingShare {
                member: alice,
                verifying_share: [0xaa; 32],
            }],
            epoch: 2,
            members: vec![alice],
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
                wall_ms: 16_000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
        // Active committee state MUST remain unchanged.
        assert_eq!(log.committee_state.current_epoch, 1);
    }

    /// R4 (Cursor Medium): two dk events for the same ceremony MUST
    /// claim identical verifying_shares for every member. A malicious
    /// committee member whose dk happens to push confirmation count
    /// to quorum cannot substitute incorrect per-member shares —
    /// the second dk's diverging shares get rejected as
    /// InvariantViolation. The previously-recorded consensus
    /// (set on the first dk) is the source of truth.
    #[test]
    fn dk_with_divergent_verifying_shares_across_confirmations_rejected() {
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgCompletePayload, MemberVerifyingShare, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);

        let mut log = DfrostLog::new();
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id: [0xab; 32],
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
            proposed_epoch: 1,
            ..Default::default()
        });

        let joint_vk = [0x99u8; 32];

        // Alice's dk: claims alice→[0xa1; 32], bob→[0xb2; 32].
        let alice_dk = DkgCompletePayload {
            ceremony_id: [0xab; 32],
            joint_verifying_key: joint_vk,
            verifying_shares: vec![
                MemberVerifyingShare {
                    member: alice,
                    verifying_share: [0xa1; 32],
                },
                MemberVerifyingShare {
                    member: bob,
                    verifying_share: [0xb2; 32],
                },
            ],
            epoch: 1,
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&alice_dk, &mut pd).unwrap();
        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: Hlc {
                wall_ms: 14_000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![0u8; 64],
        })
        .expect("alice's dk sets consensus");

        // Bob's dk: claims alice→[0xa1; 32], bob→[0xFF; 32] (different!).
        let bob_dk = DkgCompletePayload {
            ceremony_id: [0xab; 32],
            joint_verifying_key: joint_vk,
            verifying_shares: vec![
                MemberVerifyingShare {
                    member: alice,
                    verifying_share: [0xa1; 32],
                },
                MemberVerifyingShare {
                    member: bob,
                    verifying_share: [0xFF; 32], // ← DIVERGES from alice's claim
                },
            ],
            epoch: 1,
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
        };
        let mut pd2 = Vec::new();
        ciborium::into_writer(&bob_dk, &mut pd2).unwrap();
        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: Hlc {
                wall_ms: 15_000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: bob,
            payload: pd2,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
        // Committee MUST NOT promote on the divergent second dk.
        assert!(
            !log.committee_state.active,
            "divergent dk must not promote committee"
        );
    }

    /// R2 (CodeRabbit Major): `dk` payload with `verifying_shares` that
    /// don't 1:1 match `pending.members` is rejected. Covers missing
    /// entries (member without share), non-member entries (share for an
    /// unknown OwnerAddr), and duplicate-member entries.
    #[test]
    fn dk_with_mismatched_verifying_shares_rejected() {
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgCompletePayload, MemberVerifyingShare, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let stranger = OwnerAddr([0x03; 16]);

        // Test 1: missing member share (only alice's, bob is in pending).
        let mut log = DfrostLog::new();
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id: [0xab; 32],
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
            proposed_epoch: 1,
            ..Default::default()
        });
        let dk_payload = DkgCompletePayload {
            ceremony_id: [0xab; 32],
            joint_verifying_key: [0x99u8; 32],
            verifying_shares: vec![MemberVerifyingShare {
                member: alice,
                verifying_share: [0xa1; 32],
            }], // ← bob missing
            epoch: 1,
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&dk_payload, &mut pd).unwrap();
        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: Hlc {
                wall_ms: 12_000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(
            result,
            Err(ApplyError::InvariantViolation),
            "missing member share must reject"
        );

        // Test 2: stranger entry (share for a non-member).
        let mut log = DfrostLog::new();
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id: [0xab; 32],
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
            proposed_epoch: 1,
            ..Default::default()
        });
        let dk_payload = DkgCompletePayload {
            ceremony_id: [0xab; 32],
            joint_verifying_key: [0x99u8; 32],
            verifying_shares: vec![
                MemberVerifyingShare {
                    member: alice,
                    verifying_share: [0xa1; 32],
                },
                MemberVerifyingShare {
                    member: stranger, // ← not in pending.members
                    verifying_share: [0xff; 32],
                },
            ],
            epoch: 1,
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&dk_payload, &mut pd).unwrap();
        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: Hlc {
                wall_ms: 12_000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(
            result,
            Err(ApplyError::InvariantViolation),
            "stranger share must reject"
        );
    }

    /// R2 (CodeRabbit Major): `dr` event with out-of-range round_num
    /// (e.g., 99) is rejected. Pre-R2 the malformed event was silently
    /// appended to `events` despite carrying no usable protocol state.
    #[test]
    fn dr_with_invalid_round_num_rejected() {
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgRoundPayload, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let mut log = DfrostLog::new();
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id: [0x42; 32],
            members: vec![OwnerAddr([0x01; 16])],
            threshold: 1,
            max_signers: 1,
            proposed_epoch: 1,
            ..Default::default()
        });
        let payload = DkgRoundPayload {
            ceremony_id: [0x42; 32],
            round_num: 99, // ← out of range
            round1_package: Some(vec![0xde]),
            recipient_ciphertexts: None,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgRound,
            hlc: Hlc {
                wall_ms: 13_000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: OwnerAddr([0x01; 16]),
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
        assert!(
            log.events.is_empty(),
            "rejected event must NOT append to log"
        );
    }

    /// R1 (CodeRabbit Major): `vb` event whose `message_hash` differs
    /// from the pinned pending-sign-session message is rejected. Without
    /// this, a malicious peer could broadcast a beacon for a different
    /// message than the committee actually agreed to sign.
    #[test]
    fn vb_with_mismatched_message_hash_rejected() {
        use crate::community_dfrost_types::{
            derive_vrf_output, DfrostEventKind, SignedCommitteeEvent, VrfBeaconPayload,
        };
        use crate::owner_state_types::Hlc;

        let ceremony_id = [0xcc; 32];
        let agreed_msg = [0xaau8; 32];
        let attacker_msg = [0xbbu8; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        // Seed the pending sign session with the agreed message hash.
        log.committee_state.pending_sign.insert(
            ceremony_id,
            PendingSignSession {
                message_hash: agreed_msg,
                contributions: BTreeMap::new(),
                local_nonces: None,
            },
        );

        let r_compressed = [0x77u8; 32];
        let mut sig = vec![0u8; 64];
        sig[..32].copy_from_slice(&r_compressed);
        let payload = VrfBeaconPayload {
            ceremony_id,
            message_hash: attacker_msg, // ← MISMATCH
            signature: sig,
            vrf_output: derive_vrf_output(&r_compressed),
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();

        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::VrfBeacon,
            hlc: Hlc {
                wall_ms: 10_000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: OwnerAddr([0x01; 16]),
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
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

    #[test]
    fn pending_sign_session_local_nonces_serde_skipped() {
        // local_nonces holds the local node's secret FROST signing nonces
        // between dfrost_request_vrf_beacon and dfrost_contribute_threshold_sign.
        // It MUST be marked #[serde(skip)] — persisting decrypted secret nonce
        // material across restarts leaks signing inputs (same security
        // posture as PendingDkg::round2_packages).
        let session = PendingSignSession {
            local_nonces: Some(vec![0xAA; 64]),
            message_hash: [0xBB; 32],
            ..Default::default()
        };

        // CBOR-encode → decode; local_nonces MUST round-trip as None.
        let mut buf = Vec::new();
        ciborium::into_writer(&session, &mut buf).expect("encode");
        let decoded: PendingSignSession = ciborium::from_reader(&buf[..]).expect("decode");
        assert_eq!(
            decoded.local_nonces, None,
            "local_nonces must be skipped during serialization (security)"
        );
        assert_eq!(decoded.message_hash, [0xBB; 32], "public fields preserved");
    }

    #[test]
    fn build_signed_dfrost_event_round_trips_and_signs() {
        use crate::community_dfrost_types::{DfrostEventKind, DkgRoundPayload};
        use crate::owner_state_types::Hlc;
        use ed25519_dalek::{SigningKey, Verifier};
        use rand::rngs::OsRng;

        let mut csprng = OsRng;
        let keypair = SigningKey::generate(&mut csprng);
        let actor = OwnerAddr([0xab; 16]);
        let payload = DkgRoundPayload {
            ceremony_id: [0x42u8; 32],
            round_num: 1,
            round1_package: Some(vec![0xde, 0xad, 0xbe, 0xef]),
            recipient_ciphertexts: None,
        };
        let hlc = Hlc {
            wall_ms: 1234,
            logical: 0,
            device_id: "t".into(),
        };

        let ev = build_signed_dfrost_event(
            &keypair,
            actor,
            DfrostEventKind::DkgRound,
            &payload,
            hlc.clone(),
        )
        .expect("build signed event");

        // Envelope shape: tag='d', committee_tier=0, kind/actor/hlc passthrough.
        assert_eq!(ev.tag, 'd');
        assert_eq!(ev.version, 1);
        assert_eq!(ev.committee_tier, 0);
        assert_eq!(ev.kind, DfrostEventKind::DkgRound);
        assert_eq!(ev.actor, actor);
        assert_eq!(ev.hlc, hlc);

        // sig is non-zero (not the placeholder) AND is a valid Ed25519
        // signature over signing_bytes() under the supplied keypair.
        assert!(
            ev.sig.iter().any(|b| *b != 0),
            "sig must be the real Ed25519 signature, not the placeholder"
        );
        let sb = ev.signing_bytes().expect("signing bytes");
        let sig_bytes: [u8; 64] = ev.sig.clone().try_into().expect("sig len");
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        keypair
            .verifying_key()
            .verify(&sb, &sig)
            .expect("sig verifies under signer's pubkey");

        // Payload round-trips through ciborium.
        let decoded: DkgRoundPayload =
            ciborium::de::from_reader(&ev.payload[..]).expect("decode payload");
        assert_eq!(decoded, payload);
    }
}
