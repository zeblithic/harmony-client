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
//!
//! ZEB-753: the accepted-event set is backed by the core
//! `VerifiedLog<DfrostEventPolicy>` engine (`harmony-crdt-sync`), the
//! second production adopter after community-membership's
//! `MembershipPolicy`. The adoption is EVENT-SET-shaped, deliberately
//! narrower than membership's: the policy's `verify` carries only the
//! envelope gate, and `State = ()` — deep verification stays fused in
//! the six apply handlers because (a) signature verification awaits an
//! async `IdentityResolver` (`LogPolicy::verify` is sync), (b) `di`
//! admission is partly ENGINE context (the stale-replace policy binds
//! to wall-clock ceremony quiet time, so replaying the log through the
//! handlers would reject its own history), and (c) `apply_with_identity`
//! materialises decrypt-derived secret state no pure fold over the
//! event set can reproduce. What the core engine buys here: exact-
//! duplicate applies are structural no-ops (id dedup), iteration is
//! HLC-ordered by construction (discharging the old "caller is expected
//! to order by HLC" doc obligation), and the trusted
//! `from_verified_events` restore path is the substrate for the sealed
//! on-disk snapshot (`community_dfrost_persist`). Supersession is
//! unused (`SupersessionKey = ()`): re-mint lineages are NOT
//! materialize-neutral (the first-arrived event is the one that shaped
//! first-wins state), so compacting them would violate the core's
//! neutrality contract.

use std::collections::{BTreeMap, HashMap};

use frost_ristretto255::keys::{
    dkg::{round1 as dkg_r1, round2 as dkg_r2},
    KeyPackage, PublicKeyPackage,
};
use frost_ristretto255::round1::SigningNonces;
use frost_ristretto255::Identifier;
use serde::{Deserialize, Serialize};

use harmony_crdt_sync::verified_log::{InsertOutcome as CoreInsertOutcome, LogPolicy, VerifiedLog};

use crate::community_dfrost_types::{DfrostEventKind, SignedCommitteeEvent};
use crate::owner_state_types::OwnerAddr;

/// Synthesized dedup/sort key for a `SignedCommitteeEvent` (ZEB-753).
///
/// The wire envelope carries no id field (and must not grow one — the
/// zeb303 fixtures byte-pin the 8-key map), so the id is derived from
/// the envelope: HLC-major, so `VerifiedLog`'s id-ordered iteration IS
/// HLC order. `sig` is included so two DISTINCT events can never
/// compare equal (an Ed25519 signature binds the whole signing-bytes
/// image; two events differing anywhere else differ in `sig`), which
/// also makes each re-minted re-broadcast (fresh HLC + fresh sig) a
/// DISTINCT event — the transport's healing re-mints must never dedup
/// against their originals.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DfrostEventId {
    wall_ms: u64,
    logical: u32,
    device_id: String,
    actor: OwnerAddr,
    sig: Vec<u8>,
}

/// Extract the synthesized [`DfrostEventId`] for an event.
pub fn dfrost_event_id(event: &SignedCommitteeEvent) -> DfrostEventId {
    DfrostEventId {
        wall_ms: event.hlc.wall_ms,
        logical: event.hlc.logical,
        device_id: event.hlc.device_id.clone(),
        actor: event.actor,
        sig: event.sig.clone(),
    }
}

/// Envelope gate shared by BOTH apply paths and the log policy's
/// `verify` (ZEB-753 single-sourcing of the check that `apply` and
/// `apply_with_identity` previously duplicated by hand). Never
/// reachable from honest peers — cheap defence-in-depth.
pub(crate) fn check_envelope(event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
    if event.tag != 'd' || event.committee_tier != 0 {
        return Err(ApplyError::UnexpectedEnvelope);
    }
    Ok(())
}

/// `LogPolicy` adopter for the D-FROST committee log (ZEB-753).
///
/// EVENT-SET-shaped (see the module doc for why this is deliberately
/// narrower than membership's `MembershipPolicy`): `verify` is the
/// envelope gate only, `State = ()`. The strict total order is the
/// synthesized HLC-major id itself.
pub(crate) struct DfrostEventPolicy;

impl LogPolicy for DfrostEventPolicy {
    type Event = SignedCommitteeEvent;
    type EventId = DfrostEventId;
    type State = ();
    type Context = ();
    type Error = ApplyError;
    type SupersessionKey = ();

    fn event_id(e: &SignedCommitteeEvent) -> DfrostEventId {
        dfrost_event_id(e)
    }

    fn cmp(a: &SignedCommitteeEvent, b: &SignedCommitteeEvent) -> std::cmp::Ordering {
        dfrost_event_id(a).cmp(&dfrost_event_id(b))
    }

    fn verify(e: &SignedCommitteeEvent, _prior: &(), _ctx: &()) -> Result<(), ApplyError> {
        check_envelope(e)
    }

    fn materialize(_events: &[&SignedCommitteeEvent], _ctx: &()) {}
}

/// All D-FROST committee events for a single community, plus the
/// materialized `CommitteeState`. Lives in
/// `NodeState.dfrost_logs: HashMap<SpaceId, Arc<Mutex<DfrostLog>>>` once
/// Task 9 wires the IPC surface.
#[derive(Default)]
pub struct DfrostLog {
    /// All accepted events, backed by `VerifiedLog` (ZEB-753): deduped
    /// by the synthesized HLC-major [`DfrostEventId`] and iterated in
    /// id (= HLC) order. Private — all access goes through the
    /// accessors below so the backing engine stays swappable.
    log: VerifiedLog<DfrostEventPolicy>,

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
    /// at DKG completion, refresh completion, or repair completion.
    ///
    /// Persistence (ZEB-1029, revising ZEB-753's original exclusion):
    /// the signing-share SCALAR is sealed at rest EMBEDDED in the
    /// per-community `dfrost.cbor` snapshot (one atomic rename — state
    /// and share can never skew on disk; #777 round 2) — Jake's
    /// 2026-08-29 product call closing the full-committee-restart dead
    /// end, where repair (needs ≥ t live share-holders) and refresh
    /// (needs every member's old share) are both structurally
    /// unreachable. On restore the share is installed only after
    /// `install_restored_share` re-derives `G·x` and matches it against
    /// the committee's consensus verifying-share entry; any mismatch
    /// falls back to ZEB-1027's RTS repair. The in-memory
    /// identity-switch teardown contract is unchanged (the `dfrost_logs`
    /// map is still cleared; the snapshot is per-identity-dir and sealed
    /// under that identity's derived key, and `reset_local_identity`
    /// moves it into the reset backup), and every OTHER secret on this
    /// struct remains never-persisted.
    pub local_key_package: Option<KeyPackage>,

    /// Local FROST `PublicKeyPackage` (joint verifying key + per-member
    /// verifying shares). Materialised alongside `local_key_package`.
    pub local_pub_key_package: Option<PublicKeyPackage>,

    /// ZEB-1027 (#775 round 2, Qodo #8): ROTATED key material produced
    /// by refresh finalization (part 3), staged here until the
    /// ceremony's `dk` quorum PROMOTES the new epoch
    /// (`apply_dkg_complete` installs it after the consensus check).
    /// Installing at promotion — not at part 3 — keeps the old,
    /// still-valid share in `local_key_package` through the dk-quorum
    /// window: a rotated share only matches the POST-promotion
    /// verifying shares, so installing it early would poison every
    /// threshold-sign contribution this node makes in that window.
    /// SECRET (contains the rotated signing share); in-memory only,
    /// never serialized — a crash in the window loses it, and share
    /// repair is the standing recovery.
    pub pending_rotated: Option<(KeyPackage, PublicKeyPackage)>,

    /// Active threshold-sign nonce material, keyed by signing-ceremony
    /// id. Each `(nonces, commitments)` pair MUST be used exactly once
    /// (re-use leaks the signing share — see FROST spec §6.2). Cleared
    /// when the corresponding `vb` event lands or the ceremony is
    /// abandoned.
    pub local_signing_nonces: HashMap<[u8; 32], SigningNonces>,

    /// ZEB-1022 (CI stall on #771): per-community publication-order
    /// lock. Every path that RESERVES an HLC for a dfrost event and
    /// then publishes it (initiate core, contribute core, re-broadcast
    /// core) holds this across the whole reserve→sign→apply→publish
    /// span, so events from this node enter the publisher channel in
    /// nondecreasing HLC order. Without it, a re-broadcast task's
    /// re-mints (fresh, HIGHER HLCs) can reach the wire BEFORE a
    /// concurrently-built original event (lower HLC), and peers'
    /// max-HLC replay trackers then drop the original as a replay —
    /// observed on CI as a peer missing the initiator's `dk` forever.
    /// Not serialized: `Arc` shared via the `dfrost_logs` map so IPC-
    /// and driver-built handle bundles converge on one lock.
    pub publish_order: std::sync::Arc<tokio::sync::Mutex<()>>,

    /// Index of completed VRF beacons: `message_hash → vrf_output`.
    ///
    /// Populated in `apply_vrf_beacon` so that `find_vrf_beacon_output_by_seed`
    /// can answer oracle lookups without re-scanning `events`. The key is
    /// the `VrfBeaconPayload.message_hash` field (= `derive_vrf_seed(seed, epoch)`).
    /// Given a poll's beacon seed and the committee's epoch, callers compute
    /// the expected `message_hash = derive_vrf_seed(seed, epoch)` and look it up here.
    ///
    /// ZEB-753: persisted in the sealed snapshot (`community_dfrost_persist`)
    /// so completed beacons survive a restart — `DfrostBeaconOracle`
    /// lookups for already-minted beacons must not go dark on reboot.
    /// Task 10: consulted by `DfrostBeaconOracle<R>` for `verify_ss`.
    pub beacon_index: HashMap<[u8; 32], [u8; 32]>,

    /// ZEB-753: durability dirty signal. `notify_one` fires on every
    /// successful apply (both apply paths route through
    /// `insert_applied`), regardless of WHICH holder of the shared
    /// `Arc<Mutex<DfrostLog>>` applied — engine ingest and the IPC/
    /// driver cores alike. The engine's debounced save task awaits it.
    /// `Notify` stores a permit, so an apply landing while the save
    /// task is mid-write is not lost. Not serialized: `Arc` shared via
    /// the `dfrost_logs` map, same pattern as `publish_order`.
    pub dirty: std::sync::Arc<tokio::sync::Notify>,

    /// ZEB-753 (#774 round 2): snapshot-WRITE-order lock. Every
    /// `dfrost.cbor` writer (an engine's debounce task, a teardown /
    /// replace `flush_persist`) acquires this FIRST, then takes the log
    /// lock only long enough to snapshot, releases it, and performs the
    /// sealed write while still holding this lock. That keeps rename
    /// order equal to snapshot order (a slower older write can never
    /// clobber a newer one — CodeRabbit/CodeAnt round 1) WITHOUT
    /// holding the protocol-state lock across fsyncs (Qodo round 2:
    /// inbound apply and IPC paths must not stall on storage latency).
    /// Lives here rather than on the engine so the old and new engine
    /// of a registry replace — which share this log — serialize on the
    /// same lock. Not serialized: `Arc` shared via the `dfrost_logs`
    /// map, same pattern as `publish_order`.
    pub persist_order: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for DfrostLog {
    /// Hand-written because `VerifiedLog` does not derive `Debug` —
    /// and, deliberately, the secret fields render as presence flags
    /// only (the old derive printed the FROST secret-package structs
    /// into any debug log that formatted the struct).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DfrostLog")
            .field("events", &self.log.events().collect::<Vec<_>>())
            .field("committee_state", &self.committee_state)
            .field("local_dkg_secret", &self.local_dkg_secret.is_some())
            .field("local_dkg_secret2", &self.local_dkg_secret2.is_some())
            .field("local_key_package", &self.local_key_package.is_some())
            .field(
                "local_pub_key_package",
                &self.local_pub_key_package.is_some(),
            )
            .field(
                "local_signing_nonces",
                &self.local_signing_nonces.keys().collect::<Vec<_>>(),
            )
            .field("beacon_index", &self.beacon_index)
            .finish_non_exhaustive()
    }
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
    /// ZEB-1027: in-flight RTS share repair, if any. Cleared on
    /// completion (participant), on full sigma distribution (helpers),
    /// and on refresh promotion (epoch moved ⇒ ceremony void).
    /// `serde(default)` so pre-ZEB-1027 `dfrost.cbor` snapshots (which
    /// lack the key) still load.
    #[serde(default)]
    pub pending_repair: Option<PendingRepair>,
    /// ZEB-1031 §5.2: append-only lineage of retired committees, one
    /// entry per applied `rs` reset marker. `#[serde(default)]` +
    /// `skip_serializing_if` (empty ⇒ key omitted) so pre-ZEB-1031
    /// `dfrost.cbor` snapshots — which never resets have touched — load
    /// AND re-save byte-identical to their pre-1031 form. Growth is
    /// bounded by resets, which are rare (mirrors `pending_repair`'s
    /// snapshot-compat pattern).
    #[serde(rename = "vh", default, skip_serializing_if = "Vec::is_empty")]
    pub vk_history: Vec<VkLineageEntry>,
    /// ZEB-1031 §5.2: the successor committee's pinned shape, set by
    /// `apply_reset_marker` and cleared on that successor's `dk`
    /// promotion (Task 4). `Some` gates `check_ceremony_init_admissible`
    /// to accept ONLY a `di` claiming exactly this shape — see spec
    /// §5.3. `#[serde(default)]` + `skip_serializing_if` for the same
    /// pre-ZEB-1031 snapshot-compat reason as `vk_history`.
    #[serde(rename = "pr", default, skip_serializing_if = "Option::is_none")]
    pub pending_reset: Option<PendingReset>,
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
    #[serde(default)]
    pub pending_repair: Option<PendingRepair>,
    #[serde(rename = "vh", default, skip_serializing_if = "Vec::is_empty")]
    pub vk_history: Vec<VkLineageEntry>,
    #[serde(rename = "pr", default, skip_serializing_if = "Option::is_none")]
    pub pending_reset: Option<PendingReset>,
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
            pending_repair: raw.pending_repair,
            vk_history: raw.vk_history,
            pending_reset: raw.pending_reset,
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
    /// ZEB-1028 (refresh slot only; always 0 for DKG): the deadline-
    /// retry counter this ceremony's id was derived with. A later rn=1
    /// carrying a STRICTLY higher attempt displaces this ceremony
    /// (max-attempt-wins — a semilattice, so replicas converge on the
    /// same incumbent from the same event set in any arrival order);
    /// it also serves as the globally-convergent retry budget (the
    /// engine stops re-proposing once `attempt` reaches its cap).
    /// `serde(default)` so pre-ZEB-1028 snapshots load as attempt 0.
    #[serde(default)]
    pub attempt: u32,
}

/// ZEB-1031 Task 6: what a `ts`/`vb` sign ceremony is FOR. `Beacon` (the
/// pre-1031 default — VRF beacon draws for tier-3 sortition) completes
/// by minting a `vb` event and advancing the beacon index, exactly as
/// before this field existed. `ResetResponse` ceremonies never mint
/// `vb` — the aggregate Schnorr signature is instead handed to
/// `dfrost_contribute_threshold_sign`'s reset-response completion arm,
/// which authors a `DfrostResetResponse` MEMBERSHIP event carrying it
/// (spec §3.3). `#[serde(default)]` on the carrying field is load-
/// bearing: every `PendingSignSession` persisted before this field
/// existed decodes as `Beacon` — the only purpose that existed then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SignPurpose {
    #[default]
    Beacon,
    ResetResponse {
        proposal_id: crate::community_membership::EventId,
        verdict: crate::community_membership::ResetVerdict,
    },
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
    /// ZEB-1031 Task 6: what this ceremony's completion authors. Set by
    /// the initiating core (`initiate_reset_response_ceremony`'s driver
    /// impl mutates it right after `apply_with_identity` creates the
    /// session — the same post-apply-mutate pattern `local_nonces` uses)
    /// for reset-response ceremonies; left at the `Beacon` default for
    /// VRF-beacon ceremonies started via `dfrost_request_vrf_beacon`.
    /// `#[serde(default)]`: pre-1031 persisted sessions decode as
    /// `Beacon`.
    #[serde(default)]
    pub purpose: SignPurpose,
}

/// ZEB-1027: in-flight RTS share-repair ceremony state (the `rp` event
/// family). One at a time per community — repair restores exactly one
/// member's lost share at the current epoch.
///
/// Public halves (who requested, who has contributed which round) are
/// observable by every replica; the decrypted delta/sigma material is
/// local-only and `serde(skip)`ped for exactly the reasons
/// `PendingCeremony::round2_packages` is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRepair {
    pub ceremony_id: [u8; 32],
    /// The member whose share is being repaired. Structurally the rn=1
    /// event's actor — a member can only ever request repair of its OWN
    /// share (sigmas reconstruct a share the participant is entitled
    /// to; nothing leaks that it could not already know).
    pub participant: OwnerAddr,
    /// Committee epoch the repair targets. A refresh completion bumps
    /// the epoch and clears this slot — shares from different epochs
    /// must never mix into one reconstruction.
    pub epoch: u64,
    /// Declared helper set (sorted, ⊆ members ∖ {participant}, len ≥
    /// threshold). RTS Lagrange coefficients are computed over exactly
    /// this set, so EVERY listed helper must contribute rounds 2–3.
    pub helpers: Vec<OwnerAddr>,
    /// Payload-carried mint stamp (wall half). Together with
    /// `minted_logical` this feeds the COMMUTATIVE arbitration between
    /// racing rn=1 requests (see `apply_repair_round`): the winner must
    /// be a pure function of the request SET, never of arrival order,
    /// or replicas diverge on which ceremony helpers should serve.
    /// Public data (already broadcast in the rn=1 payload).
    #[serde(default)]
    pub minted_wall_ms: u64,
    /// Payload-carried mint stamp (logical half). See `minted_wall_ms`.
    #[serde(default)]
    pub minted_logical: u32,
    /// Helpers whose rn=2 (delta distribution) has been observed.
    /// Public protocol progress — safe to persist (though restarts
    /// clear the whole slot via `from_restored`).
    pub round2_seen: std::collections::BTreeSet<OwnerAddr>,
    /// Helpers whose rn=3 (sigma to participant) has been observed.
    pub round3_seen: std::collections::BTreeSet<OwnerAddr>,
    /// LOCAL (helper) only: decrypted RTS delta bytes per sender,
    /// populated via `apply_with_identity`. SECRET — a full delta set
    /// plus a helper's share reconstructs nothing extra, but deltas are
    /// blinding material and must never touch disk. `serde(skip)`.
    #[serde(skip, default)]
    pub deltas: BTreeMap<OwnerAddr, Vec<u8>>,
    /// LOCAL (participant) only: decrypted RTS sigma bytes per helper.
    /// SECRET — the sigma sum IS the signing share. `serde(skip)`.
    #[serde(skip, default)]
    pub sigmas: BTreeMap<OwnerAddr, Vec<u8>>,
}

impl PendingRepair {
    /// Fresh ceremony state (no rounds observed, no local material).
    pub fn new(
        ceremony_id: [u8; 32],
        participant: OwnerAddr,
        epoch: u64,
        helpers: Vec<OwnerAddr>,
        minted_wall_ms: u64,
        minted_logical: u32,
    ) -> Self {
        Self {
            ceremony_id,
            participant,
            epoch,
            helpers,
            minted_wall_ms,
            minted_logical,
            round2_seen: Default::default(),
            round3_seen: Default::default(),
            deltas: Default::default(),
            sigmas: Default::default(),
        }
    }

    /// Total-order rank for rn=1 arbitration (#775 round 2 —
    /// Greptile P1 / Qodo #1). Smaller rank wins. The order is
    /// (participant ASC, mint stamp DESC, ceremony id ASC):
    ///
    /// * participant first — racing requests from DIFFERENT members
    ///   resolve to the smaller address on every replica, so helpers
    ///   can never split between two ceremonies that then both starve;
    /// * newer mint stamp beats older for the SAME participant — a
    ///   retry (fresh stamp) supersedes the participant's own earlier
    ///   request no matter which order the two arrive in;
    /// * ceremony id last, purely to make the order total.
    ///
    /// Because the winner over any request SET is its rank-minimum —
    /// independent of arrival order — replicas that see the same
    /// requests converge on the same incumbent (min is commutative,
    /// associative, and idempotent).
    fn rank(&self) -> (OwnerAddr, std::cmp::Reverse<(u64, u32)>, [u8; 32]) {
        Self::rank_key(
            self.participant,
            self.minted_wall_ms,
            self.minted_logical,
            self.ceremony_id,
        )
    }

    /// ZEB-1028: the same total-order key computed from loose parts —
    /// the engine's stale-replace admission ranks an INCOMING rn=1
    /// (not yet materialised as a `PendingRepair`) against the
    /// incumbent before deciding whether a quiet incumbent must yield.
    pub(crate) fn rank_key(
        participant: OwnerAddr,
        minted_wall_ms: u64,
        minted_logical: u32,
        ceremony_id: [u8; 32],
    ) -> (OwnerAddr, std::cmp::Reverse<(u64, u32)>, [u8; 32]) {
        (
            participant,
            std::cmp::Reverse((minted_wall_ms, minted_logical)),
            ceremony_id,
        )
    }
}

/// ZEB-1031 §5.2: one retired committee's lineage record, appended to
/// `CommitteeState.vk_history` by `apply_reset_marker`. Public data (a
/// retired joint verifying key plus the reset that retired it) — safe
/// to persist and to serve on catch-up (spec §6.3 chain-link healing).
///
/// All 5 keys are 2 characters (same-length-keys invariant, matching
/// `ResetMarkerPayload`'s field naming — `old_vk`/`old_epoch`/
/// `reset_id`/`digest` mirror `ov`/`oe`/`ri`/`dg` of the marker that
/// produced this entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VkLineageEntry {
    #[serde(rename = "ov", with = "serde_bytes")]
    pub old_vk: [u8; 32],
    #[serde(rename = "oe")]
    pub old_epoch: u64,
    #[serde(
        rename = "ri",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub reset_id: crate::community_membership::EventId,
    #[serde(rename = "dg", with = "serde_bytes")]
    pub digest: [u8; 32],
    #[serde(rename = "at")]
    pub at: crate::owner_state_types::Hlc,
}

/// ZEB-1031 §5.2/§5.3: the successor committee's pinned shape, set by
/// `apply_reset_marker` and consulted by `check_ceremony_init_admissible`
/// (Task 4) to constrain the post-reset `di` to exactly this shape.
/// Public data (member list + threshold, not secret material) — safe to
/// persist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReset {
    #[serde(
        rename = "ri",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub reset_id: crate::community_membership::EventId,
    #[serde(rename = "nm")]
    pub new_members: Vec<OwnerAddr>,
    #[serde(rename = "nt")]
    pub new_threshold: u16,
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
    /// committee's joint verifying key. Also covers `apply_reset_marker`'s
    /// RS-M1 (wrong `sp`) and RS-M2 (a marker for a state this log never
    /// held — see `ResetMarkerApplied::AlreadyMoved` for the genuine
    /// re-delivery case, which is NOT an error).
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

/// Outcome of a successful `DfrostLog::apply_reset_marker` call (ZEB-1031
/// §5.2). Both variants are `Ok` — `AlreadyMoved` is RS-M6's benign
/// no-op for a duplicate/late marker, not a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMarkerApplied {
    /// The marker deactivated the held committee. `old_epoch`/`reset_id`
    /// echo the applied marker's `oe`/`ri` for the caller's bookkeeping
    /// (e.g. the engine's void-polls hook, spec §7).
    Applied {
        old_epoch: u64,
        reset_id: crate::community_membership::EventId,
    },
    /// RS-M6: the committee had already moved past this marker (a prior
    /// application of the SAME `ri` already deactivated it, or a
    /// successor has since promoted) and `vk_history` confirms this is a
    /// genuine re-delivery, not a defect. Catch-up replay legitimately
    /// re-delivers markers; this is the idempotent no-op response.
    AlreadyMoved,
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

    /// ZEB-1032: deterministic, commutative beacon-index tie-break — the
    /// bytewise-minimum `vrf_output` wins, on EVERY indexing path (live
    /// `apply_vrf_beacon` and catch-up `adopt_beacons`).
    ///
    /// Two concurrent threshold-sign ceremonies over the same seed
    /// produce two independently-valid beacons for one `message_hash`
    /// (distinct ceremony ids, fresh nonces → distinct `R` → distinct
    /// `vrf_output`, both Schnorr-valid under the joint vk). The old
    /// postures — last-wins on the live path, first-wins on the adopt
    /// path — both indexed whichever beacon a replica happened to see
    /// first/last, so replicas diverged on the value driving sortition
    /// (`find_vrf_beacon_output_by_seed`). Min is order-independent, so
    /// every replica converges on the same value regardless of arrival
    /// order — and downgrade-capable, so a replica that indexed the
    /// larger output self-heals the moment the smaller one is re-applied
    /// or re-served through catch-up.
    ///
    /// Returns `true` when the index changed (fresh insert or downgrade).
    fn index_beacon_min_wins(&mut self, message_hash: [u8; 32], vrf_output: [u8; 32]) -> bool {
        match self.beacon_index.entry(message_hash) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(vrf_output);
                true
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if vrf_output < *slot.get() {
                    slot.insert(vrf_output);
                    true
                } else {
                    false
                }
            }
        }
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
        // cheap defence-in-depth given the dispatcher already
        // pattern-matches `event.kind`. Single-sourced with the log
        // policy's `verify` (ZEB-753).
        check_envelope(&event)?;

        // ZEB-753: an EXACT duplicate (same synthesized id ⇒ same
        // bytes) is a structural no-op. Previously it re-ran the
        // (idempotent, first-wins) handler AND pushed a second copy
        // onto the Vec; now neither happens. Re-minted re-broadcasts
        // carry a fresh HLC + sig ⇒ a distinct id, so they still reach
        // the handlers.
        if self.log.contains(&dfrost_event_id(&event)) {
            return Ok(());
        }

        let result = match event.kind {
            DfrostEventKind::CeremonyInit => self.apply_ceremony_init(&event),
            DfrostEventKind::DkgRound => self.apply_dkg_round(&event),
            DfrostEventKind::DkgComplete => self.apply_dkg_complete(&event),
            DfrostEventKind::ThresholdSign => self.apply_threshold_sign(&event),
            DfrostEventKind::VrfBeacon => self.apply_vrf_beacon(&event),
            DfrostEventKind::ProactiveRefresh => self.apply_proactive_refresh(&event),
            DfrostEventKind::RepairShare => self.apply_repair_round(&event),
            // ZEB-1031: `rs` carries only the marker payload (`ri`/`dg`/
            // `ov`/`oe`/`sp`) — the successor pin (`new_members`/
            // `new_threshold`) is resolved by the engine from membership
            // state (spec §5, keeping this log membership-blind) and is
            // NOT on the wire, so this single-event dispatcher has
            // nothing to supply it with. `apply_reset_marker` is the
            // sole entry point for `rs`; the engine calls it directly
            // (mirroring how `adopt_initial_quorum`/`adopt_refresh_quorum`
            // bypass this dispatcher for the same reason — external
            // context the payload alone can't carry). Reaching this arm
            // means an `rs` event was routed through the generic
            // single-event path in error.
            DfrostEventKind::ResetMarker => Err(ApplyError::InvariantViolation),
        };
        result?;

        self.insert_applied(event);
        Ok(())
    }

    /// Store a handler-accepted event in the backing `VerifiedLog` and
    /// fire the durability dirty signal (ZEB-753). Both apply paths
    /// funnel here.
    ///
    /// The insert is infallible by construction — the envelope gate
    /// (the policy's whole `verify`) already passed and the id was
    /// checked absent before the handler ran — so anything but
    /// `Inserted` indicates a logic drift worth a loud log line.
    fn insert_applied(&mut self, event: SignedCommitteeEvent) {
        let outcome = self.log.insert(event, &());
        if !matches!(outcome, CoreInsertOutcome::Inserted) {
            tracing::warn!(
                "dfrost log insert of a handler-accepted event did not report Inserted; \
                 apply-path dedup and policy verify are out of sync"
            );
        }
        self.dirty.notify_one();
    }

    /// Iterate accepted events in synthesized-id order — which is
    /// HLC-major order by construction (ZEB-753). This discharges the
    /// old `events: Vec` doc obligation ("caller is expected to order
    /// by HLC") structurally.
    pub fn events(&self) -> impl Iterator<Item = &SignedCommitteeEvent> {
        self.log.events()
    }

    /// Number of accepted events.
    pub fn event_count(&self) -> usize {
        self.log.len()
    }

    /// Whether the log holds no events.
    pub fn events_is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// Whether an event with this exact synthesized id was accepted.
    pub fn contains_event(&self, id: &DfrostEventId) -> bool {
        self.log.contains(id)
    }

    /// Snapshot the accepted-event set for persistence (ZEB-753).
    /// Ordered by synthesized id (= HLC order).
    pub(crate) fn export_events(&self) -> Vec<SignedCommitteeEvent> {
        self.log.events().cloned().collect()
    }

    /// Test-only seeding of retained history for catch-up tests (ZEB-1030).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn insert_event_for_test(&mut self, ev: SignedCommitteeEvent) {
        self.insert_applied(ev);
    }

    /// Rebuild a log from a trusted persisted snapshot (ZEB-753).
    ///
    /// Events restore through `VerifiedLog::from_verified_events` — no
    /// re-verification (they were verified when first applied), no
    /// handler replay (replaying through the handlers would REJECT
    /// history the live engine admitted under engine-only context,
    /// e.g. a stale-replace `di` — see the module doc). The four
    /// pending slots are CLEARED: interactive ceremony rounds do not
    /// survive a restart by design ("a node that missed a round cannot
    /// retroactively join"), their secret halves were never persisted,
    /// and a restored zombie `pending_dkg` would wedge new ceremonies
    /// until the engine's stale-replace deadline. Local secret fields
    /// start empty, exactly as the identity-switch teardown contract
    /// requires (lib.rs `dfrost_logs` doc).
    pub(crate) fn from_restored(
        events: Vec<SignedCommitteeEvent>,
        mut committee_state: CommitteeState,
        beacon_index: HashMap<[u8; 32], [u8; 32]>,
    ) -> Self {
        committee_state.pending_dkg = None;
        committee_state.pending_sign.clear();
        committee_state.pending_refresh = None;
        committee_state.pending_repair = None;
        Self {
            log: VerifiedLog::from_verified_events(events),
            committee_state,
            beacon_index,
            ..Self::default()
        }
    }

    /// ZEB-1029: install a signing share restored from the sealed
    /// `dfrost_share.cbor` sidecar into a freshly-restored log.
    ///
    /// The stored artifact is ONLY the 32-byte signing-share scalar plus
    /// the epoch it was minted at — everything else is rebuilt from the
    /// restored PUBLIC consensus state, which makes the install
    /// self-authenticating: `G·x` is recomputed from the secret and must
    /// equal the committee's consensus `verifying_shares[self]` entry
    /// (the same check `settle_repair_after_round3` runs on
    /// reconstructed shares). A share that missed a refresh (epoch or
    /// consensus mismatch), belongs to a different member, or rotted on
    /// disk fails closed — the caller discards the file and the node
    /// proceeds shareless, exactly where ZEB-1027's RTS repair picks up.
    ///
    /// Errors never leave partial state: both `local_key_package` and
    /// `local_pub_key_package` are written together at the end.
    pub fn install_restored_share(
        &mut self,
        self_addr: &OwnerAddr,
        stored_epoch: u64,
        signing_share_bytes: &[u8; 32],
    ) -> Result<(), String> {
        if !self.committee_state.active {
            return Err("committee is not active — a stored share has nothing to sign for".into());
        }
        if stored_epoch != self.committee_state.current_epoch {
            return Err(format!(
                "stored share epoch {} != committee epoch {} — the committee refreshed past \
                 this share; RTS repair is the recovery path",
                stored_epoch, self.committee_state.current_epoch
            ));
        }
        let consensus = self
            .committee_state
            .verifying_shares
            .get(self_addr)
            .ok_or("self has no consensus verifying share — not a committee member")?;
        let derived =
            crate::community_dfrost_crypto::derive_verifying_share_bytes(signing_share_bytes)?;
        if derived != *consensus {
            return Err(
                "stored share's derived verifying share does not match the committee's \
                 consensus entry — stale (missed refresh), foreign, or corrupt"
                    .to_string(),
            );
        }
        let joint_vk = self
            .committee_state
            .joint_verifying_key
            .as_ref()
            .ok_or("no joint verifying key on an active committee")?;
        let self_id = *self
            .committee_state
            .identifier_map
            .get(self_addr)
            .ok_or("self not in identifier_map")?;
        let mut shares_by_id = BTreeMap::new();
        for (addr, bytes) in &self.committee_state.verifying_shares {
            let id = *self
                .committee_state
                .identifier_map
                .get(addr)
                .ok_or("verifying-share holder not in identifier_map")?;
            shares_by_id.insert(id, *bytes);
        }
        let pub_pkg = crate::community_dfrost_crypto::pub_key_package_from_bytes(
            &shares_by_id,
            joint_vk,
            self.committee_state.threshold,
        )?;
        let kp = crate::community_dfrost_crypto::key_package_from_parts(
            self_id,
            signing_share_bytes,
            consensus,
            joint_vk,
            self.committee_state.threshold,
        )?;
        self.local_key_package = Some(kp);
        self.local_pub_key_package = Some(pub_pkg);
        Ok(())
    }

    /// ZEB-1030: adopt a later epoch from ≥ threshold verbatim signed `dk`
    /// events (vk-anchored — see spec §2). Caller (engine) has already
    /// envelope-verified every event. Returns the adopted epoch.
    ///
    /// Unlike the live `apply_dkg_complete` path (which requires a
    /// matching `pending_dkg`/`pending_refresh` slot and accumulates
    /// confirmations one at a time), this is a straight evidence check:
    /// the caller hands over a quorum of already-signed `dk` events for
    /// the SAME later epoch, and this function verifies they agree with
    /// each other and with the held committee shape before committing.
    /// No partial state is written on any rejection — every check runs
    /// before the commit block below.
    ///
    /// The epoch jump is deliberately unbounded (`payload.epoch` need
    /// only be `> current_epoch`, not `== current_epoch + 1`): a
    /// straggler that missed N sequential refreshes must be able to
    /// jump N epochs in one adoption, and that is exactly the feature
    /// this method exists to provide. This adds no attack surface: a
    /// forged quorum requires ≥ `threshold` colluding CURRENT committee
    /// members signing a fabricated `dk`, which is full committee
    /// compromise — and under that compromise, the same colluders could
    /// instead just run `threshold`-many sequential legitimate refreshes
    /// and reach any target epoch anyway. Bounding the jump size would
    /// therefore buy no additional security while breaking the catch-up
    /// feature for anyone far enough behind.
    ///
    /// ZEB-1034 (`expected_space`): a payload that CARRIES a community
    /// binding must name this community; a payload without one (legacy
    /// pre-ZEB-1034 event) is still accepted HERE because the held-vk
    /// pin above already blocks cross-community replay on this path — a
    /// foreign quorum would have to share this community's joint vk.
    /// Contrast `adopt_initial_quorum`, where no held state exists and
    /// the binding is REQUIRED.
    pub fn adopt_refresh_quorum(
        &mut self,
        events: &[SignedCommitteeEvent],
        expected_space: &crate::owner_state_types::SpaceId,
    ) -> Result<u64, String> {
        use crate::community_dfrost_types::DkgCompletePayload;

        if !self.committee_state.active {
            return Err(
                "adopt_refresh_quorum: committee is not active — nothing to refresh".into(),
            );
        }
        let held_vk = self
            .committee_state
            .joint_verifying_key
            .ok_or("adopt_refresh_quorum: no held joint verifying key")?;
        if events.is_empty() {
            return Err("adopt_refresh_quorum: no events supplied".into());
        }

        let mut payloads: Vec<DkgCompletePayload> = Vec::with_capacity(events.len());
        for ev in events {
            if ev.kind != DfrostEventKind::DkgComplete {
                return Err("adopt_refresh_quorum: event is not a DkgComplete (dk) event".into());
            }
            let payload: DkgCompletePayload = ciborium::de::from_reader(&ev.payload[..])
                .map_err(|e| format!("adopt_refresh_quorum: payload decode failed: {e}"))?;
            if let Some(sp) = payload.space_id {
                if sp != *expected_space {
                    return Err(
                        "adopt_refresh_quorum: dk payload is bound to a different community \
                         (ZEB-1034)"
                            .into(),
                    );
                }
            }
            payloads.push(payload);
        }

        let first = &payloads[0];
        for p in &payloads[1..] {
            if p.ceremony_id != first.ceremony_id
                || p.epoch != first.epoch
                || p.joint_verifying_key != first.joint_verifying_key
                || p.members != first.members
                || p.threshold != first.threshold
                || p.max_signers != first.max_signers
                || p.verifying_shares != first.verifying_shares
            {
                return Err(
                    "adopt_refresh_quorum: dk events disagree on the ceremony payload".into(),
                );
            }
        }

        if first.joint_verifying_key != held_vk {
            return Err(
                "adopt_refresh_quorum: payload joint verifying key does not match the held vk"
                    .into(),
            );
        }
        if first.epoch <= self.committee_state.current_epoch {
            return Err(
                "adopt_refresh_quorum: payload epoch is not newer than the current epoch".into(),
            );
        }
        if first.members != self.committee_state.members {
            return Err(
                "adopt_refresh_quorum: payload members differ from the held member set".into(),
            );
        }
        if first.threshold != self.committee_state.threshold {
            return Err(
                "adopt_refresh_quorum: payload threshold differs from the held threshold".into(),
            );
        }
        if first.max_signers != self.committee_state.max_signers {
            return Err(
                "adopt_refresh_quorum: payload max_signers differs from the held max_signers"
                    .into(),
            );
        }

        let mut actors: std::collections::BTreeSet<OwnerAddr> = std::collections::BTreeSet::new();
        for ev in events {
            if !self.committee_state.members.contains(&ev.actor) {
                return Err(
                    "adopt_refresh_quorum: event actor is not a held committee member".into(),
                );
            }
            actors.insert(ev.actor);
        }
        if actors.len() < self.committee_state.threshold as usize {
            return Err(
                "adopt_refresh_quorum: fewer than threshold distinct actors confirmed".into(),
            );
        }

        let member_set: std::collections::BTreeSet<OwnerAddr> =
            self.committee_state.members.iter().copied().collect();
        let mut new_verifying_shares: BTreeMap<OwnerAddr, [u8; 32]> = BTreeMap::new();
        for mvs in &first.verifying_shares {
            if !member_set.contains(&mvs.member) {
                return Err("adopt_refresh_quorum: verifying_shares entry for a non-member".into());
            }
            if new_verifying_shares
                .insert(mvs.member, mvs.verifying_share)
                .is_some()
            {
                return Err("adopt_refresh_quorum: duplicate verifying_shares entry".into());
            }
        }
        if new_verifying_shares.len() != member_set.len() {
            return Err("adopt_refresh_quorum: verifying_shares missing a member".into());
        }

        // Commit — mirrors `apply_dkg_complete`'s refresh-promotion block
        // (1369-1470): members/threshold/max_signers/identifier_map are
        // unchanged (a refresh preserves the member set).
        self.committee_state.current_epoch = first.epoch;
        self.committee_state.verifying_shares = new_verifying_shares;
        self.committee_state.pending_dkg = None;
        self.committee_state.pending_refresh = None;
        self.committee_state.pending_repair = None;
        self.committee_state.pending_sign.clear();
        self.local_dkg_secret = None;
        self.local_dkg_secret2 = None;

        // Mirrors `apply_dkg_complete`'s promotion block: install the
        // STAGED rotated key material (produced by this node's own live
        // refresh part3, then orphaned when quorum landed via peer dks
        // before this node's own) iff it matches the ADOPTED consensus
        // for its identifier. That is exactly the material a node that
        // finalized part3 and then partitioned before the quorum
        // arrived would hold — installing it here skips a needless
        // ZEB-1027 repair round. A mismatch (staged material for a
        // different, un-adopted ceremony) is dropped with a warn.
        if let Some((kp, pkp)) = self.pending_rotated.take() {
            let matches_consensus = self
                .committee_state
                .identifier_map
                .iter()
                .any(|(addr, id)| {
                    id == kp.identifier()
                        && self.committee_state.verifying_shares.get(addr)
                            == Some(&crate::community_dfrost_crypto::verifying_share_to_bytes(
                                kp.verifying_share(),
                            ))
                });
            if matches_consensus {
                self.local_key_package = Some(kp);
                self.local_pub_key_package = Some(pkp);
            } else {
                tracing::warn!(
                    "dfrost refresh-quorum adoption discarded staged rotated key material that \
                     does not match the adopted consensus verifying shares (ZEB-1030/ZEB-1027)"
                );
            }
        }

        // Any local kp not freshly installed above is checked against
        // the ADOPTED consensus — a stale holdover (this node missed
        // the live ceremony's finalization AND had nothing matching
        // staged) is dropped, un-suppressing `has_key_package` so
        // ZEB-1027 auto-repair recovers the share.
        if let Some(kp) = self.local_key_package.as_ref() {
            let matches_consensus = self
                .committee_state
                .identifier_map
                .iter()
                .any(|(addr, id)| {
                    id == kp.identifier()
                        && self.committee_state.verifying_shares.get(addr)
                            == Some(&crate::community_dfrost_crypto::verifying_share_to_bytes(
                                kp.verifying_share(),
                            ))
                });
            if !matches_consensus {
                self.local_key_package = None;
                self.local_pub_key_package = None;
                tracing::warn!(
                    "dfrost refresh-quorum adoption invalidated a stale local signing share \
                     (this node missed the live ceremony's finalization); automatic share \
                     repair is its recovery path (ZEB-1027)"
                );
            }
        }

        for ev in events {
            if !self.log.contains(&dfrost_event_id(ev)) {
                self.insert_applied(ev.clone());
            }
        }

        Ok(first.epoch)
    }

    /// ZEB-1030: first-time committee-state adoption for a node with NO
    /// active state (fresh joiner/observer). Returns the adopted epoch.
    ///
    /// Caller obligations: every event's envelope must already be
    /// signature-verified. The membership obligation is broader than
    /// "the actors who signed" — it covers EVERY committee member the
    /// adopted payload claims (`payload.members`), each checked at that
    /// event's own envelope HLC, not just the (possibly-fewer, quorum-
    /// sized) set of actors whose `dk` events happen to be in `events`.
    /// A quorum of honest actors can otherwise faithfully vouch for a
    /// payload that names an additional member who was never actually
    /// in the community.
    ///
    /// ZEB-1034 (`expected_space`): a fresh joiner has NO held state to
    /// pin the evidence against, so every payload MUST carry a community
    /// binding naming this community. Catch-up frames are sealed under
    /// the target community's epoch key, so the attacker of record is a
    /// dual-community responder replaying community X's GENUINE,
    /// validly-signed dk quorum into community Y — with overlapping
    /// membership the Y-membership gate passes, and pre-ZEB-1034 the
    /// joiner adopted X's committee/vk into Y (then wedged permanently
    /// against Y's real committee via vk-immutability). The binding is
    /// inside the signed payload, so it cannot be re-labelled without
    /// breaking every envelope signature.
    ///
    /// REJECTING unbound (legacy pre-ZEB-1034) payloads is deliberate:
    /// accept-if-absent would leave the replay open via any community's
    /// retained legacy dk events. Cost: a fresh joiner cannot
    /// catch-up-bootstrap into a community whose newest dk quorum
    /// predates this change until that community completes one
    /// post-upgrade DKG or refresh (whose dk events carry the binding)
    /// — the upgrade-before-adopt rollout posture from the ticket.
    pub fn adopt_initial_quorum(
        &mut self,
        events: &[SignedCommitteeEvent],
        expected_space: &crate::owner_state_types::SpaceId,
        rejected_vks: &std::collections::BTreeSet<[u8; 32]>,
    ) -> Result<u64, String> {
        use crate::community_dfrost_types::DkgCompletePayload;

        if self.committee_state.active {
            return Err("adopt_initial_quorum: committee is already active".into());
        }
        if self.committee_state.pending_dkg.is_some() {
            return Err("adopt_initial_quorum: a DKG ceremony is already pending".into());
        }
        if events.is_empty() {
            return Err("adopt_initial_quorum: no events supplied".into());
        }

        let mut payloads: Vec<DkgCompletePayload> = Vec::with_capacity(events.len());
        for ev in events {
            if ev.kind != DfrostEventKind::DkgComplete {
                return Err("adopt_initial_quorum: event is not a DkgComplete (dk) event".into());
            }
            let payload: DkgCompletePayload = ciborium::de::from_reader(&ev.payload[..])
                .map_err(|e| format!("adopt_initial_quorum: payload decode failed: {e}"))?;
            match payload.space_id {
                Some(sp) if sp == *expected_space => {}
                Some(_) => {
                    return Err(
                        "adopt_initial_quorum: dk payload is bound to a different community \
                         (ZEB-1034)"
                            .into(),
                    );
                }
                None => {
                    return Err(
                        "adopt_initial_quorum: dk payload carries no community binding — \
                         legacy pre-ZEB-1034 evidence is not adoptable by a fresh joiner; \
                         the community must complete one post-upgrade DKG/refresh first"
                            .into(),
                    );
                }
            }
            payloads.push(payload);
        }

        let first = &payloads[0];
        for p in &payloads[1..] {
            if p.ceremony_id != first.ceremony_id
                || p.epoch != first.epoch
                || p.joint_verifying_key != first.joint_verifying_key
                || p.members != first.members
                || p.threshold != first.threshold
                || p.max_signers != first.max_signers
                || p.verifying_shares != first.verifying_shares
            {
                return Err(
                    "adopt_initial_quorum: dk events disagree on the ceremony payload".into(),
                );
            }
        }

        // ZEB-1031 §6.1: reject any dk quorum whose claimed joint
        // verifying key is a REJECTED tv (Authorized — live replacement
        // in progress — or Consumed-and-not-superseded) — checked
        // FIRST, before any shape validation below, so a stale
        // pre-reset quorum never even reaches the shape/threshold
        // gates. Closes the stale-committee replay a colluding
        // ex-committee could otherwise replay against a fresh joiner
        // once Lapse (if any) clears the freeze.
        if rejected_vks.contains(&first.joint_verifying_key) {
            return Err(
                "adopt_initial_quorum: dk quorum's joint verifying key is a rejected target \
                 of a committee reset (ZEB-1031 provenance)"
                    .into(),
            );
        }
        // ZEB-1031 §5.3/§6.2 (controller ruling on a Task 4 plan gap):
        // while a reset is pending on THIS log — a straggler that just
        // applied its own marker via `apply_reset_marker` — the
        // successor quorum being adopted must claim EXACTLY the pinned
        // shape. Without this, a post-marker straggler would adopt ANY
        // structurally-valid quorum, defeating the covert-replacement
        // protection the pin exists for.
        if let Some(pin) = &self.committee_state.pending_reset {
            if first.members != pin.new_members || first.threshold != pin.new_threshold {
                return Err(
                    "adopt_initial_quorum: dk quorum does not match the pinned successor \
                     shape from the committee reset in progress (ZEB-1031)"
                        .into(),
                );
            }
        }

        if first.epoch < 1 {
            return Err("adopt_initial_quorum: epoch must be >= 1".into());
        }
        // Mirrors `check_ceremony_init_admissible`'s shape gate exactly
        // (sorted+deduplicated, >= 2 members, max_signers ==
        // members.len(), 2 <= threshold <= max_signers) — a fabricated
        // single-member ("committee" of one, self-signed threshold-1
        // quorum) must be rejected here exactly as the live `di`
        // admission path rejects it; a lower floor here would let a
        // single colluding actor mint a legitimate-looking joiner
        // adoption for a committee that was never actually agreed.
        let mut sorted_members = first.members.clone();
        sorted_members.sort();
        sorted_members.dedup();
        if sorted_members != first.members || first.members.len() < 2 {
            return Err(
                "adopt_initial_quorum: members must be sorted ascending, deduplicated, and \
                 number at least 2"
                    .into(),
            );
        }
        if first.max_signers as usize != first.members.len() {
            return Err("adopt_initial_quorum: max_signers does not match members.len()".into());
        }
        if first.threshold < 2 || first.threshold > first.max_signers {
            return Err("adopt_initial_quorum: threshold out of range".into());
        }

        let member_set: std::collections::BTreeSet<OwnerAddr> =
            first.members.iter().copied().collect();
        let mut actors: std::collections::BTreeSet<OwnerAddr> = std::collections::BTreeSet::new();
        for ev in events {
            if !member_set.contains(&ev.actor) {
                return Err(
                    "adopt_initial_quorum: event actor is not in the payload's member set".into(),
                );
            }
            actors.insert(ev.actor);
        }
        if actors.len() < first.threshold as usize {
            return Err(
                "adopt_initial_quorum: fewer than threshold distinct actors confirmed".into(),
            );
        }

        let mut verifying_shares: BTreeMap<OwnerAddr, [u8; 32]> = BTreeMap::new();
        for mvs in &first.verifying_shares {
            if !member_set.contains(&mvs.member) {
                return Err("adopt_initial_quorum: verifying_shares entry for a non-member".into());
            }
            if verifying_shares
                .insert(mvs.member, mvs.verifying_share)
                .is_some()
            {
                return Err("adopt_initial_quorum: duplicate verifying_shares entry".into());
            }
        }
        if verifying_shares.len() != member_set.len() {
            return Err("adopt_initial_quorum: verifying_shares missing a member".into());
        }

        // Commit — full promotion. A joiner has no local key package to
        // reconcile (it never ran the ceremony), so `local_key_package`
        // is left untouched (already `None` on a fresh log).
        let identifier_map = CommitteeState::build_identifier_map(&first.members);
        self.committee_state.active = true;
        self.committee_state.current_epoch = first.epoch;
        self.committee_state.joint_verifying_key = Some(first.joint_verifying_key);
        self.committee_state.verifying_shares = verifying_shares;
        self.committee_state.members = first.members.clone();
        self.committee_state.threshold = first.threshold;
        self.committee_state.max_signers = first.max_signers;
        self.committee_state.identifier_map = identifier_map;

        for ev in events {
            if !self.log.contains(&dfrost_event_id(ev)) {
                self.insert_applied(ev.clone());
            }
        }

        Ok(first.epoch)
    }

    /// ZEB-1030: adopt self-certifying beacons. Per-event failures skip
    /// that event (each is independent). Returns the count of index
    /// changes (fresh inserts + min-wins downgrades).
    ///
    /// Unlike `apply_vrf_beacon`, this deliberately does NOT require a
    /// matching `pending_sign` session — that is the whole point of a
    /// self-certifying beacon: the Schnorr signature under the
    /// committee's joint verifying key is itself sufficient proof, so a
    /// straggler catching up cold (no in-flight sign session of its own)
    /// can still adopt it. `pending_sign` is never touched.
    ///
    /// Tie posture (ZEB-1032): min-wins on BOTH indexing paths — see
    /// `index_beacon_min_wins`. The pre-1032 postures (first-wins here,
    /// last-wins on the live path) were each arrival-order-dependent,
    /// so a caught-up replica and an always-online replica could
    /// converge on different sortition values for the same event set.
    ///
    /// ZEB-1035: `now_wall_ms` (this node's trusted wall clock; `0` =
    /// clock unreadable ⇒ gate disabled, the voting plane's
    /// `receiver_now_ms` convention) drives an ingest-admission
    /// forward-skew gate: an event whose envelope HLC is implausibly
    /// future is REJECTED outright — neither indexed nor retained.
    /// Retaining it would make this node re-serve it forever, since it
    /// sorts above every requester's (correctly skew-capped, see
    /// `beacon_watermark_of`) watermark. This is the voting plane's
    /// ingest-admission reject, not a store purge — the house
    /// VIEW-not-store rule governs events already retained, and REJECT
    /// (never clamp) per the grow-only-register rule.
    pub fn adopt_beacons(&mut self, events: &[SignedCommitteeEvent], now_wall_ms: u64) -> usize {
        use crate::community_dfrost_types::{derive_vrf_output, VrfBeaconPayload};

        if !self.committee_state.active {
            return 0;
        }
        let Some(held_vk) = self.committee_state.joint_verifying_key else {
            return 0;
        };

        let mut newly_indexed = 0usize;
        for event in events {
            if event.kind != DfrostEventKind::VrfBeacon {
                continue;
            }
            // ZEB-1035: forward-skew ingest-admission gate — see the
            // doc comment above. Cheapest check first, before any
            // decode/crypto work on a peer-controlled stamp.
            if now_wall_ms != 0
                && crate::clock_trust::reject_future_logged(
                    event.hlc.wall_ms,
                    now_wall_ms,
                    crate::clock_trust::MAX_FORWARD_SKEW_MS,
                    "dfrost_catchup.adopt_beacons.envelope_hlc",
                )
            {
                continue;
            }
            let payload: VrfBeaconPayload = match ciborium::de::from_reader(&event.payload[..]) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if payload.signature.len() != 64 {
                continue;
            }
            let mut r_compressed = [0u8; 32];
            r_compressed.copy_from_slice(&payload.signature[..32]);
            if derive_vrf_output(&r_compressed) != payload.vrf_output {
                continue;
            }
            if crate::community_dfrost_crypto::verify_schnorr_signature(
                &held_vk,
                &payload.message_hash,
                &payload.signature,
            )
            .is_err()
            {
                continue;
            }
            // ZEB-1030 round-2: the Schnorr check above covers only the
            // PAYLOAD under the joint verifying key — the envelope `actor`
            // is just this beacon's re-broadcaster, not itself part of
            // what gets signed. Without this check, any envelope-valid
            // identity (an Ed25519 signature over `signing_bytes()`,
            // unrelated to committee membership) could re-wrap a genuine
            // beacon under its own envelope and have it retained here.
            // Beacons are committee output, so only committee members'
            // envelopes are adoptable — deliberately STRICTER than the
            // live `apply_vrf_beacon` path, which leans on its
            // `pending_sign` session gate instead of a membership check
            // on the envelope actor.
            if !self.committee_state.members.contains(&event.actor) {
                continue;
            }

            if self.index_beacon_min_wins(payload.message_hash, payload.vrf_output) {
                newly_indexed += 1;
            }
            if !self.log.contains(&dfrost_event_id(event)) {
                self.insert_applied(event.clone());
            }
        }
        newly_indexed
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
    /// re-broadcast tolerance) — but ONLY when the actor matches the
    /// pending ceremony's initiator AND the claimed shape matches the
    /// pending shape (a same-id `di` from anyone else, or with a
    /// divergent shape, is an `InvariantViolation` — otherwise any
    /// member could no-op-"progress" a stalled ceremony forever and
    /// suppress the initiator's deadline recovery, CodeAnt on #771);
    /// DIFFERENT ceremony pending → `CeremonyInFlight` (the engine's
    /// stale-replace policy calls `abort_pending_dkg()` first when it
    /// decides to admit the newcomer).
    fn apply_ceremony_init(&mut self, event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        use crate::community_dfrost_types::CeremonyInitPayload;

        let payload: CeremonyInitPayload =
            ciborium::de::from_reader(&event.payload[..]).map_err(|_| ApplyError::PayloadDecode)?;

        self.check_ceremony_init_admissible(&payload, &event.actor)?;

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
                // fresh HLC, or a duplicate delivery). Only the pending
                // ceremony's own initiator may re-mint, and only with
                // the identical shape (defence-in-depth: the engine's
                // ceremony-id binding gate already makes a divergent
                // shape unreachable for inbound events, but `apply` is
                // a public entry point and must not depend on it —
                // mirrors apply_dkg_complete's shape pinning).
                if p.initiator.is_some_and(|init| init != event.actor) {
                    return Err(ApplyError::InvariantViolation);
                }
                if p.members != payload.members
                    || p.threshold != payload.threshold
                    || p.max_signers != payload.max_signers
                    || p.proposed_epoch != payload.epoch
                {
                    return Err(ApplyError::InvariantViolation);
                }
                // No state change — in particular the accumulated
                // round1_packages / dk_confirmations survive.
                Ok(())
            }
            Some(_) => Err(ApplyError::CeremonyInFlight),
        }
    }

    /// ZEB-1022: the non-mutating half of `apply_ceremony_init`'s
    /// validation — everything that can reject a `di` REGARDLESS of the
    /// pending-slot state. Split out so the engine's stale-replace
    /// admission can verify a replacement `di` is structurally
    /// admissible BEFORE aborting the stale incumbent (CodeRabbit on
    /// #771: aborting first meant an inadmissible replacement destroyed
    /// the incumbent + local secrets and then seeded nothing).
    ///
    /// Checks: committee not `active`; members sorted bytewise
    /// ascending + deduplicated, at least 2; `max_signers ==
    /// members.len()`; `2 <= threshold <= max_signers`; the actor is a
    /// committee member; `epoch == current_epoch + 1`.
    pub fn check_ceremony_init_admissible(
        &self,
        payload: &crate::community_dfrost_types::CeremonyInitPayload,
        actor: &OwnerAddr,
    ) -> Result<(), ApplyError> {
        if self.committee_state.active {
            return Err(ApplyError::InvariantViolation);
        }
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
        if !payload.members.contains(actor) {
            return Err(ApplyError::InvariantViolation);
        }
        if payload.epoch != self.committee_state.current_epoch + 1 {
            return Err(ApplyError::InvariantViolation);
        }
        // ZEB-1031 §5.3: if a reset is pending, the di must claim exactly
        // the pinned successor member set and threshold.
        if let Some(pin) = &self.committee_state.pending_reset {
            if payload.members != pin.new_members || payload.threshold != pin.new_threshold {
                return Err(ApplyError::InvariantViolation);
            }
        }
        Ok(())
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

    /// ZEB-1028: abort the in-flight proactive refresh, clearing the
    /// pending slot, this node's zero-sharing transcript secrets (bound
    /// to the aborted attempt's randomness — see `abort_pending_dkg`),
    /// and any staged-but-unpromoted rotated key material
    /// (`pending_rotated` was produced from the aborted transcript and
    /// must never install). The active committee state — including this
    /// node's CURRENT signing share — is untouched: an aborted refresh
    /// simply leaves the committee signing at its existing epoch.
    ///
    /// Returns the aborted `ceremony_id`, or `None` if no refresh was
    /// pending. Callers: the engine's retry-budget-exhausted quiet
    /// deadline, and its stale-replace admission for a higher-attempt
    /// rn=1.
    pub fn abort_pending_refresh(&mut self) -> Option<[u8; 32]> {
        let aborted = self.committee_state.pending_refresh.take()?;
        self.local_dkg_secret = None;
        self.local_dkg_secret2 = None;
        self.pending_rotated = None;
        Some(aborted.ceremony_id)
    }

    /// ZEB-1028: abort the in-flight share repair, clearing the pending
    /// slot (declared helper set, observed rounds, and the local
    /// decrypted delta/sigma material with it — all bound to the
    /// aborted ceremony's Lagrange set). Nothing else changes: helpers
    /// keep their shares, and the participant remains shareless until a
    /// later repair completes.
    ///
    /// Returns the aborted `ceremony_id`, or `None` if no repair was
    /// pending. Caller: the engine's stale-replace admission for a
    /// competing rn=1 whose rank the deterministic apply rule would
    /// reject (without this, a dead participant's ceremony — which can
    /// never settle — starves every larger-ranked live participant
    /// forever).
    pub fn abort_pending_repair(&mut self) -> Option<[u8; 32]> {
        let aborted = self.committee_state.pending_repair.take()?;
        Some(aborted.ceremony_id)
    }

    /// Apply an `rs` (ResetMarker) event, closing out a
    /// membership-authorized committee reset (ZEB-1031 spec §5).
    ///
    /// `expected_space` is this log's own community — the caller
    /// (engine) always has it in scope, exactly like
    /// `adopt_initial_quorum`/`adopt_refresh_quorum`'s `expected_space`
    /// parameter ten screens up; unlike a stored field, a parameter
    /// can't silently re-default on a restore path. `new_members`/
    /// `new_threshold` are the successor committee's pinned shape,
    /// resolved by the ENGINE from membership state (RS-M3/M4/M5 —
    /// membership phase, digest recomputation, actor authorization —
    /// are all verified there, against membership-log state this
    /// dfrost log never sees, keeping the log membership-blind). This
    /// function verifies only RS-M1 (space), RS-M2 (held-state match),
    /// and RS-M6 (idempotent re-delivery) — the three gates checkable
    /// from held dfrost state alone.
    ///
    /// Gate order: envelope/kind → decode → RS-M1 → RS-M2/RS-M6 →
    /// effects. No partial state is written on any rejection — every
    /// check runs before the mutation block.
    pub fn apply_reset_marker(
        &mut self,
        event: &SignedCommitteeEvent,
        expected_space: &crate::owner_state_types::SpaceId,
        new_members: Vec<OwnerAddr>,
        new_threshold: u16,
    ) -> Result<ResetMarkerApplied, ApplyError> {
        use crate::community_dfrost_types::ResetMarkerPayload;

        // Same envelope gate as `apply`/`apply_with_identity` (single-
        // sourced, ZEB-753), plus the kind guard `adopt_initial_quorum`
        // runs for `dk` (`:1399-1401`-equivalent): without both, a
        // malformed-envelope or wrong-kind event can mutate committee
        // state here while `insert_applied`'s policy verify rejects it
        // ONE STEP LATER — deactivation with no event behind it to
        // explain it on catch-up.
        check_envelope(event)?;
        if event.kind != DfrostEventKind::ResetMarker {
            return Err(ApplyError::UnexpectedEnvelope);
        }

        let payload: ResetMarkerPayload =
            ciborium::de::from_reader(&event.payload[..]).map_err(|_| ApplyError::PayloadDecode)?;

        // RS-M1: `sp` must equal this log's community — unconditional
        // (mirrors `adopt_initial_quorum`'s strict style for a
        // mandatory field, vs. `adopt_refresh_quorum`'s lenient
        // tolerance for `dk`'s OPTIONAL legacy binding: `rs` is a
        // brand-new kind with no pre-ZEB-1034 history, so there is no
        // absent-binding case to tolerate).
        if payload.space_id != *expected_space {
            return Err(ApplyError::InvariantViolation);
        }

        // RS-M2: does the held state still match what this marker
        // claims to retire?
        let state_matches = self.committee_state.active
            && self.committee_state.joint_verifying_key == Some(payload.old_vk)
            && self.committee_state.current_epoch == payload.old_epoch;

        if !state_matches {
            // RS-M6: a genuine re-delivery of a marker that already
            // applied is benign (catch-up replay legitimately
            // re-delivers it) — recognized by `vk_history` already
            // carrying this exact `reset_id`. Anything else (a marker
            // for a state this log never held) is a defect, not a
            // replay.
            let already_recorded = self
                .committee_state
                .vk_history
                .iter()
                .any(|entry| entry.reset_id == payload.reset_proposal_id);
            if already_recorded {
                return Ok(ResetMarkerApplied::AlreadyMoved);
            }
            return Err(ApplyError::InvariantViolation);
        }

        // Effects (spec §5.2): deactivate, record lineage, pin the
        // successor shape.
        self.committee_state.active = false;
        self.committee_state.joint_verifying_key = None;
        self.committee_state.vk_history.push(VkLineageEntry {
            old_vk: payload.old_vk,
            old_epoch: payload.old_epoch,
            reset_id: payload.reset_proposal_id,
            digest: payload.reset_digest,
            at: event.hlc.clone(),
        });
        self.committee_state.pending_reset = Some(PendingReset {
            reset_id: payload.reset_proposal_id,
            new_members,
            new_threshold,
        });
        // Every in-flight ceremony under the old vk is dead by
        // definition — clear all four pending slots and the in-memory
        // secrets bound to them, the way `abort_pending_dkg`/
        // `abort_pending_refresh`/`abort_pending_repair` do
        // individually.
        self.committee_state.pending_dkg = None;
        self.committee_state.pending_sign.clear();
        self.committee_state.pending_refresh = None;
        self.committee_state.pending_repair = None;
        self.local_dkg_secret = None;
        self.local_dkg_secret2 = None;
        self.pending_rotated = None;
        // `current_epoch` stays at `old_epoch` (spec §5.2) — the
        // existing `epoch == current_epoch + 1` gate on `di` naturally
        // yields the successor DKG at `old_epoch + 1` (Task 4 wires the
        // `pending_reset` shape pin into that gate).

        self.insert_applied(event.clone());

        Ok(ResetMarkerApplied::Applied {
            old_epoch: payload.old_epoch,
            reset_id: payload.reset_proposal_id,
        })
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
        // ZEB-1022 (CodeRabbit merge-risk on #771): only committee
        // members may contribute rounds. Without this, any
        // signature-valid community member could inject an rn=1 package
        // — inflating `round1_packages` past `max_signers` and stalling
        // the `r1_count == n` auto-drive gate forever (a one-event DoS
        // on the ceremony). Reachable now that the `di` bootstrap makes
        // multi-node ingest live.
        if !pending.members.contains(&event.actor) {
            return Err(ApplyError::InvariantViolation);
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
        // event flowing into the accepted-event log so non-committee
        // replicas still observe the protocol progress.
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

        // ZEB-1022 (CodeRabbit merge-risk on #771): only committee
        // members' `dk` confirmations may count toward the promotion
        // quorum. Without this, signature-valid community members
        // OUTSIDE the committee could echo the winning payload and
        // push `dk_confirmations.len()` past `threshold`, activating a
        // committee no quorum of actual members confirmed.
        if !pending.members.contains(&event.actor) {
            return Err(ApplyError::InvariantViolation);
        }

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
            // ZEB-1031 §5.3: if a reset was pending (only a post-reset DKG
            // can reach this point when active=false and pending_reset=Some),
            // clear the pin now that promotion is complete.
            self.committee_state.pending_reset = None;
            // ZEB-1027: promotion ends the ceremony on this node — the
            // in-memory round secrets are dead transcript material and
            // MUST NOT leak into the next ceremony's part2/part3 inputs
            // (they also key the "round already submitted" idempotency
            // guards, which would otherwise false-trip on the first
            // post-completion refresh). Note the pre-existing straggler
            // race is unchanged by this: a node whose promote arrives
            // via peer dks before its own part3 already lost the
            // transcript when the pending slot cleared above; share
            // repair (`rp`) is now its recovery path.
            self.local_dkg_secret = None;
            self.local_dkg_secret2 = None;
            // Epoch moved: any in-flight repair targets the OLD
            // polynomial; its deltas/sigmas must never finalize. The
            // participant re-requests at the new epoch.
            self.committee_state.pending_repair = None;
            // Qodo #8 (#775 round 2): install the STAGED rotated key
            // material exactly at promotion (see `pending_rotated`'s
            // doc — the old share stays valid until the epoch actually
            // advances). Identity is checked the same way as the
            // staleness check below, because this plain-apply path has
            // no self address: the staged package's verifying share
            // must equal the promoted consensus entry for its
            // identifier. A mismatch (this promotion belongs to a
            // different ceremony than the one that staged it) discards
            // the stage; the staleness check below then routes the
            // node to repair if its ACTIVE share is also stale.
            if let Some((kp, pkp)) = self.pending_rotated.take() {
                let matches_consensus =
                    self.committee_state
                        .identifier_map
                        .iter()
                        .any(|(addr, id)| {
                            id == kp.identifier()
                                && self.committee_state.verifying_shares.get(addr)
                                    == Some(
                                        &crate::community_dfrost_crypto::verifying_share_to_bytes(
                                            kp.verifying_share(),
                                        ),
                                    )
                        });
                if matches_consensus {
                    self.local_key_package = Some(kp);
                    self.local_pub_key_package = Some(pkp);
                } else {
                    tracing::warn!(
                        "dfrost promotion discarded staged rotated key material that does not \
                         match the promoted consensus verifying shares (ZEB-1027)"
                    );
                }
            }
            // CR-2 (#775 round 1): a held signing share that does not
            // match the PROMOTED consensus verifying shares is STALE —
            // this node contributed refresh rounds but peer dks reached
            // quorum before its own part3 (or it never finalized at
            // all). Keeping it would (a) let threshold-sign produce
            // shares that can never aggregate under the new committee
            // package and (b) make `has_key_package` block the automatic
            // repair that is exactly this node's recovery path. The
            // node that DID finalize had its rotated package installed
            // from `pending_rotated` just above, so its share matches
            // and is kept.
            // Cryptographic identity (verifying share == consensus
            // entry) is the discriminator because this plain-apply path
            // has no self address to compare against.
            if let Some(kp) = self.local_key_package.as_ref() {
                let matches_consensus =
                    self.committee_state
                        .identifier_map
                        .iter()
                        .any(|(addr, id)| {
                            id == kp.identifier()
                                && self.committee_state.verifying_shares.get(addr)
                                    == Some(
                                        &crate::community_dfrost_crypto::verifying_share_to_bytes(
                                            kp.verifying_share(),
                                        ),
                                    )
                        });
                if !matches_consensus {
                    self.local_key_package = None;
                    self.local_pub_key_package = None;
                    tracing::warn!(
                        "dfrost promotion invalidated a stale local signing share (this node \
                         missed the ceremony's finalization); automatic share repair is its \
                         recovery path (ZEB-1027)"
                    );
                }
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
        // ZEB-1025: only ACTIVE-committee members may contribute signing
        // rounds. The Zenoh plane gates publishing to community members,
        // not committee members, so a signature-valid non-committee actor
        // could otherwise pollute `pending_sign[..].contributions` — and
        // because the upsert rules treat an existing entry as immutable
        // commitment state, a squatter entry under a member-colliding
        // future key would wedge that member's real contribution.
        if !self.committee_state.members.contains(&event.actor) {
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
                // ZEB-1031: the wire-level `ts` payload carries no
                // purpose signal (it's opaque commitment/share bytes) —
                // every replica creates the session at the Beacon
                // default here; the LOCAL initiating core (whichever one
                // this is) overwrites it to `ResetResponse` right after
                // its own `apply_with_identity` call, mirroring how
                // `local_nonces` is stashed post-apply below.
                purpose: SignPurpose::default(),
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
        // the verified `vrf_output` — min-wins across every indexing path
        // (ZEB-1032). Indexed before clearing pending_sign.
        self.index_beacon_min_wins(payload.message_hash, payload.vrf_output);

        // Clear the pending sign session — the ceremony is now finalised
        // on this replica.
        self.committee_state
            .pending_sign
            .remove(&payload.ceremony_id);
        Ok(())
    }

    /// Apply an `rf` event: one round of the fully-distributed
    /// zero-sharing refresh DKG (ZEB-1027 — pre-1027 rn=1 was a sealed
    /// placeholder; see `RefreshRoundPayload` for the round shapes).
    ///
    /// rn=1: initialise `pending_refresh` (once) with `proposed_epoch =
    ///   current_epoch + 1` and record the actor's PUBLIC round-1
    ///   commitment in `round1_packages` — exactly `apply_dkg_round`
    ///   rn=1's shape.
    /// rn=2: sealed per-recipient share packages; the broadcast path
    ///   validates shape only. Local decryption into
    ///   `round2_packages[actor]` happens in `apply_with_identity`.
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
        // ZEB-1025: only committee members may open or advance a refresh.
        // Refresh preserves the member set, so the ACTIVE committee's
        // members are the ceremony's members. Without this, any
        // signature-valid community member could seed a phantom
        // `pending_refresh` into the singleton slot (ZEB-1028's quiet
        // deadline would eventually clear it, but only after stalling
        // real refreshes for the full deadline window).
        if !self.committee_state.members.contains(&event.actor) {
            return Err(ApplyError::InvariantViolation);
        }

        // R2 (CodeRabbit Major): exhaustive round_num match + per-round
        // payload-shape validation. Out-of-range round_num is rejected
        // with InvariantViolation rather than silently appended.
        match payload.round_num {
            1 => {
                // ZEB-1027: rn=1 carries the public zero-sharing
                // round-1 commitment (refresh_dkg_part1 package bytes).
                let pkg = payload.package.ok_or(ApplyError::InvariantViolation)?;
                // A DKG and a refresh must never run concurrently — a
                // dk for one could cross-finalize state seeded by the
                // other's transcript expectations. `dfrost_initiate_dkg`
                // is already blocked by `active`; this guards the
                // inverse ordering (rf arriving while a stray
                // pending_dkg exists on an active committee).
                if self.committee_state.pending_dkg.is_some() {
                    return Err(ApplyError::CeremonyInFlight);
                }
                // ZEB-1028: attempt arbitration for a DIFFERENT ceremony
                // id in the slot. The id is a pure function of
                // (committee shape, next epoch, attempt), so:
                //   * higher attempt  → a deadline retry; it DISPLACES
                //     the incumbent (max-attempt-wins — a semilattice,
                //     so every replica converges on the same incumbent
                //     from the same event set regardless of arrival
                //     order). The displaced attempt's transcript
                //     secrets and staged rotation die with it — they
                //     were minted against the old attempt's randomness.
                //   * lower attempt   → a stale retry replay; dropped.
                //   * equal attempt   → a forked/forged id (two ids at
                //     one attempt cannot both derive from the shared
                //     shape); reject loudly. The engine's rn=1 ingest
                //     gate recomputes the derivation and never admits
                //     this, so it is only reachable via direct apply.
                if let Some(pr) = self.committee_state.pending_refresh.as_ref() {
                    if pr.ceremony_id != payload.ceremony_id {
                        match payload.attempt.cmp(&pr.attempt) {
                            std::cmp::Ordering::Greater => {
                                self.committee_state.pending_refresh = None;
                                self.local_dkg_secret = None;
                                self.local_dkg_secret2 = None;
                                self.pending_rotated = None;
                            }
                            std::cmp::Ordering::Less => {
                                return Err(ApplyError::CeremonyInFlight);
                            }
                            std::cmp::Ordering::Equal => {
                                return Err(ApplyError::InvariantViolation);
                            }
                        }
                    }
                }
                // Initialise the pending refresh once on the first rn=1
                // event; every member's rn=1 accumulates into it.
                if self.committee_state.pending_refresh.is_none() {
                    self.committee_state.pending_refresh = Some(PendingCeremony {
                        ceremony_id: payload.ceremony_id,
                        members: self.committee_state.members.clone(),
                        threshold: self.committee_state.threshold,
                        max_signers: self.committee_state.max_signers,
                        proposed_epoch: self.committee_state.current_epoch + 1,
                        attempt: payload.attempt,
                        ..Default::default()
                    });
                }
                let pr = self
                    .committee_state
                    .pending_refresh
                    .as_mut()
                    .expect("seeded above");
                // First-wins per actor, mirroring apply_dkg_round rn=1.
                pr.round1_packages.entry(event.actor).or_insert(pkg);
            }
            2 => {
                // Sealed per-recipient share material; the broadcast
                // path validates shape only (local decryption in
                // `apply_with_identity`).
                let pr = self
                    .committee_state
                    .pending_refresh
                    .as_ref()
                    .ok_or(ApplyError::UnknownCeremony)?;
                if pr.ceremony_id != payload.ceremony_id {
                    return Err(ApplyError::UnknownCeremony);
                }
                let cts = payload
                    .recipient_ciphertexts
                    .as_ref()
                    .ok_or(ApplyError::InvariantViolation)?;
                // Qodo #2 (#775 round 2): the recipient set must cover
                // EVERY other ceremony member exactly once — no
                // missing, duplicate, self, or non-member entries. An
                // accepted rn=2 whose omitted recipient can never store
                // this sender's package would stall the singleton
                // refresh permanently (finalization needs packages from
                // ALL other members); rejecting up front keeps the
                // event out of every replica's progress accounting.
                let mut recipients: std::collections::BTreeSet<OwnerAddr> =
                    std::collections::BTreeSet::new();
                for ct in cts {
                    if !recipients.insert(ct.recipient) {
                        return Err(ApplyError::InvariantViolation);
                    }
                }
                let expected: std::collections::BTreeSet<OwnerAddr> = pr
                    .members
                    .iter()
                    .copied()
                    .filter(|m| *m != event.actor)
                    .collect();
                if recipients != expected {
                    return Err(ApplyError::InvariantViolation);
                }
            }
            _ => return Err(ApplyError::InvariantViolation),
        }
        Ok(())
    }

    /// ZEB-1027: apply an `rp` event — one round of the RTS share
    /// repair. Broadcast-path halves only (public progress tracking);
    /// sealed delta/sigma decryption lives in `apply_with_identity`.
    ///
    /// rn=1 (request): `event.actor` IS the participant. Validates the
    ///   declared helper set (sorted, deduplicated, ⊆ members ∖
    ///   {actor}, ≥ threshold — RTS cannot run with fewer helpers than
    ///   the threshold, which also makes a t-of-n committee with
    ///   t == n structurally unrepairable), the payload mint stamp, and
    ///   epoch == current_epoch. Seeds `pending_repair`. A same-id
    ///   re-mint is an idempotent no-op. COMPETING ceremonies (any
    ///   different id — the participant's own retry included) are
    ///   arbitrated by `PendingRepair::rank`: the rank-minimum request
    ///   wins on every replica REGARDLESS of arrival order (#775
    ///   round 2, Greptile P1 / Qodo #1 — an order-dependent rule here
    ///   lets replicas lock onto different ceremonies and reject each
    ///   other's helper rounds as `UnknownCeremony`, starving both
    ///   participants). A displaced ceremony's progress is discarded
    ///   with its slot; the displaced participant's automatic request
    ///   re-arms once the winner settles.
    /// rn=2 (helper deltas): records the helper in `round2_seen`.
    /// rn=3 (helper sigma): records the helper in `round3_seen`.
    /// ZEB-1028: everything an rp rn=1 (repair request) must satisfy to
    /// SEED the slot, minus the incumbent arbitration — the shape rules
    /// (sorted/deduped helper set, no self-help, members-only, ≥
    /// threshold, mint stamp present), the epoch/actor binding, and the
    /// mutual exclusion with share-mutating ceremonies. Split out so the
    /// engine's stale-replace admission can verify a competing request
    /// is otherwise admissible BEFORE aborting a quiet incumbent for it
    /// (mirror of `check_ceremony_init_admissible` in the di flow: never
    /// destroy the slot for a request that would then fail to seed).
    pub(crate) fn check_repair_request_admissible(
        &self,
        payload: &crate::community_dfrost_types::RepairRoundPayload,
        actor: &OwnerAddr,
    ) -> Result<(), ApplyError> {
        if !self.committee_state.active {
            return Err(ApplyError::InvariantViolation);
        }
        if !self.committee_state.members.contains(actor) {
            return Err(ApplyError::InvariantViolation);
        }
        if payload.epoch != self.committee_state.current_epoch {
            return Err(ApplyError::InvariantViolation);
        }
        let helpers = payload
            .helpers
            .as_ref()
            .ok_or(ApplyError::InvariantViolation)?;
        let mut sorted = helpers.clone();
        sorted.sort();
        sorted.dedup();
        if &sorted != helpers || helpers.is_empty() {
            return Err(ApplyError::InvariantViolation);
        }
        if helpers.contains(actor) {
            return Err(ApplyError::InvariantViolation);
        }
        if helpers
            .iter()
            .any(|h| !self.committee_state.members.contains(h))
        {
            return Err(ApplyError::InvariantViolation);
        }
        if helpers.len() < self.committee_state.threshold as usize {
            return Err(ApplyError::InvariantViolation);
        }
        if payload.minted_wall_ms.is_none() || payload.minted_logical.is_none() {
            return Err(ApplyError::InvariantViolation);
        }
        // Mutual exclusion with share-mutating ceremonies: a repair
        // reconstructs a point on the CURRENT polynomial; running it
        // concurrently with a DKG/refresh transcript would mix
        // polynomials. Refresh wins (its promotion clears
        // `pending_repair`); a repair request waits.
        if self.committee_state.pending_dkg.is_some()
            || self.committee_state.pending_refresh.is_some()
        {
            return Err(ApplyError::CeremonyInFlight);
        }
        Ok(())
    }

    fn apply_repair_round(&mut self, event: &SignedCommitteeEvent) -> Result<(), ApplyError> {
        use crate::community_dfrost_types::RepairRoundPayload;

        let payload: RepairRoundPayload =
            ciborium::de::from_reader(&event.payload[..]).map_err(|_| ApplyError::PayloadDecode)?;

        if !self.committee_state.active {
            return Err(ApplyError::InvariantViolation);
        }
        if !self.committee_state.members.contains(&event.actor) {
            return Err(ApplyError::InvariantViolation);
        }
        // Epoch binding: every round must target the CURRENT epoch.
        // A refresh completing mid-repair bumps the epoch and clears
        // the slot; straggler rounds from the old epoch then land here.
        if payload.epoch != self.committee_state.current_epoch {
            return Err(ApplyError::InvariantViolation);
        }

        match payload.round_num {
            1 => {
                // Shape + exclusion validation, shared with the engine's
                // stale-replace admission (ZEB-1028) so an incumbent is
                // never aborted for a request that would then fail to
                // seed.
                self.check_repair_request_admissible(&payload, &event.actor)?;
                let helpers = payload.helpers.ok_or(ApplyError::InvariantViolation)?;
                let (Some(minted_wall_ms), Some(minted_logical)) =
                    (payload.minted_wall_ms, payload.minted_logical)
                else {
                    return Err(ApplyError::InvariantViolation);
                };
                let candidate = PendingRepair::new(
                    payload.ceremony_id,
                    event.actor,
                    payload.epoch,
                    helpers.clone(),
                    minted_wall_ms,
                    minted_logical,
                );
                match self.committee_state.pending_repair.as_ref() {
                    None => {
                        self.committee_state.pending_repair = Some(candidate);
                    }
                    Some(p) if p.ceremony_id == payload.ceremony_id => {
                        // Idempotent re-mint: same participant, same
                        // shape, or it's a forged/colliding request.
                        // (The stamp halves are inputs to
                        // `derive_repair_ceremony_id`, so a same-id
                        // event claiming different stamps is forged.)
                        if p.participant != event.actor
                            || p.helpers != helpers
                            || p.epoch != payload.epoch
                            || p.minted_wall_ms != minted_wall_ms
                            || p.minted_logical != minted_logical
                        {
                            return Err(ApplyError::InvariantViolation);
                        }
                    }
                    Some(p) => {
                        // COMMUTATIVE arbitration (#775 round 2,
                        // Greptile P1 / Qodo #1): the rank-minimum
                        // request wins, full stop — the winner over a
                        // request set is then independent of arrival
                        // order, so replicas converge on the same
                        // incumbent. Deliberately NOT gated on incumbent
                        // progress (`round2_seen`): protecting an
                        // in-flight ceremony would reintroduce exactly
                        // the order dependence being removed (a replica
                        // that saw helper progress first keeps the
                        // incumbent; one that saw the challenger first
                        // replaces it). Rank covers the participant's
                        // own retry too: a newer mint stamp outranks its
                        // older ceremony in both arrival orders.
                        if candidate.rank() < p.rank() {
                            self.committee_state.pending_repair = Some(candidate);
                        } else {
                            return Err(ApplyError::CeremonyInFlight);
                        }
                    }
                }
            }
            2 | 3 => {
                let p = self
                    .committee_state
                    .pending_repair
                    .as_mut()
                    .ok_or(ApplyError::UnknownCeremony)?;
                if p.ceremony_id != payload.ceremony_id {
                    return Err(ApplyError::UnknownCeremony);
                }
                // Only DECLARED helpers advance rounds 2–3 (the
                // participant itself contributes nothing after rn=1).
                if !p.helpers.contains(&event.actor) {
                    return Err(ApplyError::InvariantViolation);
                }
                let cts = payload
                    .recipient_ciphertexts
                    .as_ref()
                    .ok_or(ApplyError::InvariantViolation)?;
                // Qodo #3 (#775 round 2): a round may only mark public
                // progress (`roundN_seen`) if it actually carries the
                // sealed material the protocol needs — otherwise a
                // malformed event permanently satisfies the seen-set
                // while the required delta/sigma never arrives, and the
                // ceremony (or a non-participant's slot cleanup, which
                // gates on `round3_seen` alone) runs on a lie.
                if payload.round_num == 2 {
                    // Exactly one delta per DECLARED helper, the sender
                    // included (uniform sealed distribution).
                    let mut recipients: std::collections::BTreeSet<OwnerAddr> =
                        std::collections::BTreeSet::new();
                    for ct in cts {
                        if !recipients.insert(ct.recipient) {
                            return Err(ApplyError::InvariantViolation);
                        }
                    }
                    let expected: std::collections::BTreeSet<OwnerAddr> =
                        p.helpers.iter().copied().collect();
                    if recipients != expected {
                        return Err(ApplyError::InvariantViolation);
                    }
                    p.round2_seen.insert(event.actor);
                } else {
                    // Exactly one sigma, sealed to the participant.
                    if cts.len() != 1 || cts[0].recipient != p.participant {
                        return Err(ApplyError::InvariantViolation);
                    }
                    p.round3_seen.insert(event.actor);
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
        use crate::community_dfrost_types::{
            DkgRoundPayload, RefreshRoundPayload, RepairRoundPayload,
        };
        use crate::dm_signing;

        // Same envelope gate as `apply` (single-sourced, ZEB-753) so
        // the early-bail paths surface the same diagnostics.
        check_envelope(&event)?;

        // ZEB-753: exact-duplicate no-op, mirroring `apply`. The two
        // decrypt branches below return early without going through
        // `apply`, so the dedup must sit on this path too.
        if self.log.contains(&dfrost_event_id(&event)) {
            return Ok(());
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
                    // ZEB-1022: committee-membership gate, mirroring the
                    // broadcast path (`apply_dkg_round`) — a non-member's
                    // sealed rn=2 must not land in `round2_packages`
                    // (part3 would key it by a members-index lookup that
                    // cannot represent it, wedging the round).
                    if !pending.members.contains(&event.actor) {
                        return Err(ApplyError::InvariantViolation);
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
                    self.insert_applied(event);
                    return Ok(());
                }
                // For round_num != 2, fall back to the broadcast apply path.
                self.apply(event)
            }
            DfrostEventKind::ProactiveRefresh => {
                let payload: RefreshRoundPayload = ciborium::de::from_reader(&event.payload[..])
                    .map_err(|_| ApplyError::PayloadDecode)?;
                if payload.round_num == 2 {
                    // ZEB-1027: rn=2 carries the sealed refresh round-2
                    // share packages — the exact shape of DkgRound rn=2
                    // (pre-1027, rn=1 carried the sealed placeholder
                    // and decrypted here instead).
                    //
                    // R3 (CodeRabbit Major) lineage: stage the local
                    // decrypt BEFORE any state mutation so a decrypt
                    // failure rejects the event without leaving the
                    // materialized state out of sync with the log.
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
                    // — non-committee replicas legitimately have
                    // nothing to decrypt). Run the broadcast-path
                    // validation, then store the plaintext.
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
                    self.insert_applied(event);
                    return Ok(());
                }
                self.apply(event)
            }
            DfrostEventKind::RepairShare => {
                let payload: RepairRoundPayload = ciborium::de::from_reader(&event.payload[..])
                    .map_err(|_| ApplyError::PayloadDecode)?;
                match payload.round_num {
                    // rn=2: sealed deltas, one per declared helper.
                    // Decrypt the entry addressed to self (if this node
                    // is a declared helper) into `deltas[sender]`.
                    //
                    // rn=3: sealed sigma addressed to the participant.
                    // Decrypt into `sigmas[helper]` and, once every
                    // declared helper's sigma is in, finalize the
                    // repair INLINE — reconstruction is pure local
                    // arithmetic over material already in this struct,
                    // so no follow-up IPC round-trip is needed for the
                    // recovery-critical step.
                    2 | 3 => {
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

                        // Broadcast-path validation + round2/3_seen
                        // bookkeeping (also rejects non-helpers, wrong
                        // epoch, unknown ceremony — before any local
                        // mutation below).
                        self.apply_repair_round(&event)?;

                        if let Some(plaintext) = decrypted_for_self {
                            let pending = self
                                .committee_state
                                .pending_repair
                                .as_mut()
                                .ok_or(ApplyError::UnknownCeremony)?;
                            if payload.round_num == 2 {
                                pending.deltas.entry(event.actor).or_insert(plaintext);
                            } else {
                                pending.sigmas.entry(event.actor).or_insert(plaintext);
                            }
                        }
                        self.insert_applied(event);
                        if payload.round_num == 3 {
                            self.settle_repair_after_round3(self_addr);
                        }
                        Ok(())
                    }
                    // rn=1 (and malformed rounds) carry no sealed
                    // material — the broadcast path handles them.
                    _ => self.apply(event),
                }
            }
            _ => self.apply(event),
        }
    }

    /// ZEB-1027: post-rn=3 settlement, run on the identity-aware apply
    /// path (the only place `self_addr` is known).
    ///
    /// * PARTICIPANT with a full sigma set → reconstruct the share via
    ///   RTS part 3, verify the derived verifying share against the
    ///   committee's consensus `verifying_shares[self]` (part 3 itself
    ///   verifies NOTHING — without this check a single malicious
    ///   helper installs a garbage share), install
    ///   `local_key_package` + `local_pub_key_package`, clear the slot.
    ///   A failed reconstruction also clears the slot (terminal for
    ///   this ceremony) so the participant can re-request with a fresh
    ///   mint stamp; the error is logged loudly.
    /// * NON-participant once `round3_seen` covers every declared
    ///   helper → the ceremony is over from this replica's view; clear
    ///   the slot (dropping helper delta material) so the singleton
    ///   does not wedge the next repair.
    fn settle_repair_after_round3(&mut self, self_addr: &OwnerAddr) {
        let Some(p) = self.committee_state.pending_repair.as_ref() else {
            return;
        };
        if p.participant != *self_addr {
            if p.round3_seen.len() == p.helpers.len() {
                self.committee_state.pending_repair = None;
            }
            return;
        }
        if p.sigmas.len() != p.helpers.len() {
            return;
        }

        // Full sigma set — reconstruct. Everything below is pure local
        // computation; any failure is terminal for THIS ceremony.
        let outcome = (|| -> Result<(), String> {
            let self_id = *self
                .committee_state
                .identifier_map
                .get(self_addr)
                .ok_or("self not in identifier_map")?;
            let joint_vk = self
                .committee_state
                .joint_verifying_key
                .as_ref()
                .ok_or("no joint verifying key on an active committee")?;
            let mut shares_by_id = BTreeMap::new();
            for (addr, bytes) in &self.committee_state.verifying_shares {
                let id = *self
                    .committee_state
                    .identifier_map
                    .get(addr)
                    .ok_or("verifying-share holder not in identifier_map")?;
                shares_by_id.insert(id, *bytes);
            }
            let pub_pkg = crate::community_dfrost_crypto::pub_key_package_from_bytes(
                &shares_by_id,
                joint_vk,
                self.committee_state.threshold,
            )?;
            let p = self
                .committee_state
                .pending_repair
                .as_ref()
                .expect("checked above");
            let sigma_bytes: Vec<Vec<u8>> = p.sigmas.values().cloned().collect();
            let repaired = crate::community_dfrost_crypto::repair_part3_local(
                &sigma_bytes,
                self_id,
                &pub_pkg,
            )?;
            // THE check (see repair_part3_local's doc): consensus
            // verifying share must match what the reconstruction
            // derives, or some sigma was wrong/malicious.
            let derived = crate::community_dfrost_crypto::verifying_share_to_bytes(
                repaired.verifying_share(),
            );
            let consensus = self
                .committee_state
                .verifying_shares
                .get(self_addr)
                .ok_or("self has no consensus verifying share")?;
            if derived != *consensus {
                return Err(
                    "reconstructed share's verifying share does not match the committee's \
                     consensus entry — a helper contributed a wrong or epoch-skewed sigma"
                        .to_string(),
                );
            }
            self.local_key_package = Some(repaired);
            self.local_pub_key_package = Some(pub_pkg);
            Ok(())
        })();

        match outcome {
            Ok(()) => {
                tracing::info!(
                    "dfrost repair complete: signing share reconstructed and verified \
                     against the committee consensus (ZEB-1027)"
                );
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "dfrost repair finalization FAILED — ceremony aborted; the participant \
                     may re-request with a fresh mint stamp (ZEB-1027)"
                );
            }
        }
        // Terminal either way for this ceremony on this node.
        self.committee_state.pending_repair = None;
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
    /// ZEB-1034: the community every adopt-path test binds its dk
    /// evidence to (and passes as `expected_space`).
    fn zeb1034_space() -> crate::owner_state_types::SpaceId {
        crate::owner_state_types::SpaceId([0x5A; 16])
    }

    use super::*;
    use crate::owner_state_types::OwnerAddr;

    #[test]
    fn dfrost_log_starts_empty() {
        let log = DfrostLog::new();
        assert!(!log.committee_state.active);
        assert_eq!(log.committee_state.current_epoch, 0);
        assert!(log.committee_state.joint_verifying_key.is_none());
        assert!(log.events_is_empty());
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

    /// ZEB-1022 helper: build a `di` (CeremonyInit) event with a fake
    /// signature (apply does not verify — that's the engine's job).
    fn di_event(
        actor: OwnerAddr,
        members: Vec<OwnerAddr>,
        threshold: u16,
        epoch: u64,
        ceremony_id: [u8; 32],
        wall_ms: u64,
    ) -> crate::community_dfrost_types::SignedCommitteeEvent {
        use crate::community_dfrost_types::{
            CeremonyInitPayload, DfrostEventKind, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;
        let max_signers = members.len() as u16;
        let payload = CeremonyInitPayload {
            ceremony_id,
            members,
            threshold,
            max_signers,
            epoch,
            minted_wall_ms: wall_ms,
            minted_logical: 0,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::CeremonyInit,
            hlc: Hlc {
                wall_ms,
                logical: 0,
                device_id: "t".into(),
            },
            actor,
            payload: pd,
            sig: vec![0u8; 64],
        }
    }

    #[test]
    fn di_seeds_pending_dkg_zeb1022() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let ceremony_id = [0x42u8; 32];
        let mut log = DfrostLog::new();
        log.apply(di_event(alice, vec![alice, bob], 2, 1, ceremony_id, 1_000))
            .expect("di seeds");
        let p = log.committee_state.pending_dkg.as_ref().expect("pending");
        assert_eq!(p.ceremony_id, ceremony_id);
        assert_eq!(p.initiator, Some(alice));
        assert_eq!(p.members, vec![alice, bob]);
        assert_eq!(p.threshold, 2);
        assert_eq!(p.max_signers, 2);
        assert_eq!(p.proposed_epoch, 1);
        assert!(p.round1_packages.is_empty());
        assert_eq!(log.event_count(), 1);
    }

    #[test]
    fn di_same_id_is_idempotent_zeb1022() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let ceremony_id = [0x42u8; 32];
        let mut log = DfrostLog::new();
        log.apply(di_event(alice, vec![alice, bob], 2, 1, ceremony_id, 1_000))
            .expect("di seeds");
        // Accumulate a round-1 package, then re-apply the (re-minted) di.
        log.committee_state
            .pending_dkg
            .as_mut()
            .unwrap()
            .round1_packages
            .insert(alice, vec![0xde]);
        log.apply(di_event(alice, vec![alice, bob], 2, 1, ceremony_id, 2_000))
            .expect("same-id di is an idempotent no-op");
        let p = log.committee_state.pending_dkg.as_ref().unwrap();
        assert!(
            p.round1_packages.contains_key(&alice),
            "re-applied di must not reset accumulated round-1 packages"
        );
    }

    #[test]
    fn di_different_id_rejected_ceremony_in_flight_zeb1022() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let mut log = DfrostLog::new();
        log.apply(di_event(alice, vec![alice, bob], 2, 1, [0x42; 32], 1_000))
            .expect("first di seeds");
        assert_eq!(
            log.apply(di_event(bob, vec![alice, bob], 2, 1, [0x43; 32], 2_000)),
            Err(ApplyError::CeremonyInFlight),
            "the log never replaces an in-flight ceremony on its own"
        );
    }

    #[test]
    fn di_on_active_committee_rejected_zeb1022() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        assert_eq!(
            log.apply(di_event(alice, vec![alice, bob], 2, 2, [0x42; 32], 1_000)),
            Err(ApplyError::InvariantViolation),
            "fresh DKG on an active committee is refresh's job"
        );
    }

    #[test]
    fn di_shape_violations_rejected_zeb1022() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let carol = OwnerAddr([0x03; 16]);
        let cid = [0x42u8; 32];

        // Unsorted member list.
        let mut log = DfrostLog::new();
        assert_eq!(
            log.apply(di_event(alice, vec![bob, alice], 2, 1, cid, 1_000)),
            Err(ApplyError::InvariantViolation)
        );
        // Duplicate member.
        assert_eq!(
            log.apply(di_event(alice, vec![alice, alice, bob], 2, 1, cid, 1_000)),
            Err(ApplyError::InvariantViolation)
        );
        // threshold < 2.
        assert_eq!(
            log.apply(di_event(alice, vec![alice, bob], 1, 1, cid, 1_000)),
            Err(ApplyError::InvariantViolation)
        );
        // threshold > max_signers.
        assert_eq!(
            log.apply(di_event(alice, vec![alice, bob], 3, 1, cid, 1_000)),
            Err(ApplyError::InvariantViolation)
        );
        // Initiator not a committee member.
        assert_eq!(
            log.apply(di_event(carol, vec![alice, bob], 2, 1, cid, 1_000)),
            Err(ApplyError::InvariantViolation)
        );
        // Wrong proposed epoch (current is 0 ⇒ only 1 is valid).
        assert_eq!(
            log.apply(di_event(alice, vec![alice, bob], 2, 2, cid, 1_000)),
            Err(ApplyError::InvariantViolation)
        );
        // Nothing seeded by any of the rejects.
        assert!(log.committee_state.pending_dkg.is_none());
        assert!(log.events_is_empty());
    }

    #[test]
    fn di_same_id_from_non_initiator_rejected_zeb1022() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let cid = [0x42u8; 32];
        let mut log = DfrostLog::new();
        log.apply(di_event(alice, vec![alice, bob], 2, 1, cid, 1_000))
            .expect("alice's di seeds");
        // Bob replays the SAME ceremony id under his own signature —
        // rejected: only the initiator may re-mint (a no-op accept
        // would count as engine "progress" and let any member suppress
        // the initiator's deadline recovery forever).
        assert_eq!(
            log.apply(di_event(bob, vec![alice, bob], 2, 1, cid, 2_000)),
            Err(ApplyError::InvariantViolation)
        );
    }

    #[test]
    fn di_same_id_with_divergent_shape_rejected_zeb1022() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let carol = OwnerAddr([0x03; 16]);
        let cid = [0x42u8; 32];
        let mut log = DfrostLog::new();
        log.apply(di_event(alice, vec![alice, bob], 2, 1, cid, 1_000))
            .expect("di seeds");
        // Same id, different membership — must not append an event whose
        // payload contradicts the materialized pending shape.
        assert_eq!(
            log.apply(di_event(alice, vec![alice, bob, carol], 2, 1, cid, 2_000)),
            Err(ApplyError::InvariantViolation)
        );
        assert_eq!(log.event_count(), 1);
    }

    #[test]
    fn dr_from_non_committee_member_rejected_zeb1022() {
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgRoundPayload, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let mallory = OwnerAddr([0x0f; 16]);
        let cid = [0x42u8; 32];
        let mut log = DfrostLog::new();
        log.apply(di_event(alice, vec![alice, bob], 2, 1, cid, 1_000))
            .expect("di seeds");

        let payload = DkgRoundPayload {
            ceremony_id: cid,
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
                wall_ms: 2_000,
                logical: 0,
                device_id: "m".into(),
            },
            actor: mallory,
            payload: pd,
            sig: vec![0u8; 64],
        };
        // A signature-valid community member OUTSIDE the committee must
        // not be able to inject an rn=1 package (it would inflate
        // round1_packages past max_signers and wedge the r1_count == n
        // auto-drive gate forever).
        assert_eq!(log.apply(ev), Err(ApplyError::InvariantViolation));
        assert!(log
            .committee_state
            .pending_dkg
            .as_ref()
            .unwrap()
            .round1_packages
            .is_empty());
    }

    #[test]
    fn dk_from_non_committee_member_rejected_zeb1022() {
        use crate::community_dfrost_types::{
            DfrostEventKind, DkgCompletePayload, MemberVerifyingShare, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let mallory = OwnerAddr([0x0f; 16]);
        let cid = [0x42u8; 32];
        let mut log = DfrostLog::new();
        log.apply(di_event(alice, vec![alice, bob], 2, 1, cid, 1_000))
            .expect("di seeds");

        let dk_payload = DkgCompletePayload {
            ceremony_id: cid,
            joint_verifying_key: [0x55; 32],
            verifying_shares: vec![
                MemberVerifyingShare {
                    member: alice,
                    verifying_share: [0xaa; 32],
                },
                MemberVerifyingShare {
                    member: bob,
                    verifying_share: [0xbb; 32],
                },
            ],
            epoch: 1,
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
            space_id: None,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&dk_payload, &mut pd).unwrap();
        let ev = SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: Hlc {
                wall_ms: 2_000,
                logical: 0,
                device_id: "m".into(),
            },
            actor: mallory,
            payload: pd,
            sig: vec![0u8; 64],
        };
        // A non-member echoing the winning dk payload must not count
        // toward the promotion quorum.
        assert_eq!(log.apply(ev), Err(ApplyError::InvariantViolation));
        assert!(log
            .committee_state
            .pending_dkg
            .as_ref()
            .unwrap()
            .dk_confirmations
            .is_empty());
    }

    #[test]
    fn abort_pending_dkg_clears_pending_and_secrets_zeb1022() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let ceremony_id = [0x42u8; 32];
        let mut log = DfrostLog::new();
        assert_eq!(log.abort_pending_dkg(), None, "no-op on empty slot");

        log.apply(di_event(alice, vec![alice, bob], 2, 1, ceremony_id, 1_000))
            .expect("di seeds");
        let id = crate::community_dfrost_crypto::identifier_for_index(0);
        let (secret, _pkg) =
            crate::community_dfrost_crypto::dkg_part1_local(id, 2, 2).expect("part1");
        log.local_dkg_secret = Some(secret);

        assert_eq!(log.abort_pending_dkg(), Some(ceremony_id));
        assert!(log.committee_state.pending_dkg.is_none());
        assert!(
            log.local_dkg_secret.is_none(),
            "aborted ceremony's secrets must not leak into a successor"
        );
        assert!(log.local_dkg_secret2.is_none());
    }

    #[test]
    fn resign_with_fresh_hlc_preserves_payload_and_verifies_zeb1022() {
        use crate::owner_state_types::Hlc;
        use ed25519_dalek::Verifier;

        let keypair = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let actor = OwnerAddr([0x01; 16]);
        let payload = crate::community_dfrost_types::CeremonyInitPayload {
            ceremony_id: [0x42; 32],
            members: vec![actor],
            threshold: 2,
            max_signers: 2,
            epoch: 1,
            minted_wall_ms: 1_000,
            minted_logical: 0,
        };
        let original = build_signed_dfrost_event(
            &keypair,
            actor,
            crate::community_dfrost_types::DfrostEventKind::CeremonyInit,
            &payload,
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .expect("build");

        let fresh_hlc = Hlc {
            wall_ms: 5_000,
            logical: 3,
            device_id: "d".into(),
        };
        let reminted = resign_dfrost_event_with_fresh_hlc(&original, fresh_hlc.clone(), &keypair)
            .expect("resign");

        assert_eq!(reminted.payload, original.payload, "payload untouched");
        assert_eq!(reminted.hlc, fresh_hlc);
        assert_ne!(reminted.sig, original.sig, "fresh HLC ⇒ fresh signature");
        // The new signature verifies over the new signing bytes.
        let sb = reminted.signing_bytes().unwrap();
        let sig_bytes: [u8; 64] = reminted.sig.as_slice().try_into().unwrap();
        keypair
            .verifying_key()
            .verify(&sb, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
            .expect("re-minted signature verifies");
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
            space_id: None,
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
        assert_eq!(log.event_count(), 2);
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
                purpose: SignPurpose::default(),
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
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let ceremony_id = [0x77u8; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = vec![alice];
        log.committee_state.threshold = 1;
        log.committee_state.max_signers = 1;

        // ZEB-1027: rn=1 carries the PUBLIC zero-sharing round-1
        // commitment (pre-1027 it sealed a placeholder per recipient).
        let payload = RefreshRoundPayload {
            ceremony_id,
            round_num: 1,
            recipient_ciphertexts: None,
            package: Some(vec![0xde, 0xad]),
            attempt: 0,
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
        assert_eq!(
            pr.round1_packages.get(&alice),
            Some(&vec![0xde, 0xad]),
            "rn=1 must record the actor's public round-1 package (ZEB-1027)"
        );
    }

    /// ZEB-1025: a `ts` from a signature-valid actor OUTSIDE the active
    /// committee must be rejected before any session state is created.
    /// Pre-fix, mallory's contribution landed in
    /// `pending_sign[..].contributions` keyed by her addr.
    #[test]
    fn ts_from_non_committee_actor_rejected_zeb1025() {
        use crate::community_dfrost_types::{
            DfrostEventKind, SignedCommitteeEvent, ThresholdSignPayload,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let mallory = OwnerAddr([0x66; 16]);

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.members = vec![alice];

        let payload = ThresholdSignPayload {
            ceremony_id: [0xcc; 32],
            message_hash: [0xde; 32],
            commitment_bytes: vec![0x01, 0x02],
            share_bytes: vec![0x03, 0x04],
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();

        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms: 4000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: mallory,
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
        assert!(
            log.committee_state.pending_sign.is_empty(),
            "rejected non-member ts must not create a signing session"
        );
        assert!(
            log.events_is_empty(),
            "rejected event must not be appended to the log"
        );
    }

    /// ZEB-1025: an `rf` rn=1 from a non-committee actor must not open a
    /// phantom refresh ceremony. The pending_refresh slot is a singleton
    /// with no abort/deadline machinery — pre-fix, a phantom wedged real
    /// refreshes until restart.
    #[test]
    fn rf_rn1_from_non_committee_actor_rejected_zeb1025() {
        use crate::community_dfrost_types::{
            DfrostEventKind, RefreshRoundPayload, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let mallory = OwnerAddr([0x66; 16]);

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = vec![alice];
        log.committee_state.threshold = 1;
        log.committee_state.max_signers = 1;

        let payload = RefreshRoundPayload {
            ceremony_id: [0x77u8; 32],
            round_num: 1,
            recipient_ciphertexts: None,
            package: Some(vec![0xde, 0xad]),
            attempt: 0,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();

        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ProactiveRefresh,
            hlc: Hlc {
                wall_ms: 6000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: mallory,
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
        assert!(
            log.committee_state.pending_refresh.is_none(),
            "non-member rf rn=1 must not seed pending_refresh"
        );
    }

    /// ZEB-1025: an `rf` rn=2 from a non-committee actor against a real
    /// in-flight refresh is rejected (the gate covers both rounds).
    #[test]
    fn rf_rn2_from_non_committee_actor_rejected_zeb1025() {
        use crate::community_dfrost_types::{
            DfrostEventKind, RefreshRoundPayload, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let mallory = OwnerAddr([0x66; 16]);
        let ceremony_id = [0x77u8; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = vec![alice];
        log.committee_state.threshold = 1;
        log.committee_state.max_signers = 1;
        log.committee_state.pending_refresh = Some(PendingCeremony {
            ceremony_id,
            members: vec![alice],
            threshold: 1,
            max_signers: 1,
            proposed_epoch: 2,
            ..Default::default()
        });

        let payload = RefreshRoundPayload {
            ceremony_id,
            round_num: 2,
            recipient_ciphertexts: Some(vec![]),
            package: None,
            attempt: 0,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();

        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ProactiveRefresh,
            hlc: Hlc {
                wall_ms: 6500,
                logical: 0,
                device_id: "t".into(),
            },
            actor: mallory,
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::InvariantViolation));
    }

    /// ZEB-1025: the LOCAL apply path (`apply_with_identity`) for rf
    /// events routes through `apply_proactive_refresh` before any
    /// `round2_packages` insert, so the same membership gate must cover
    /// it — a non-member's rf must not seed pending_refresh via the local
    /// path either (the dr rn=2 local path needed its own mirror gate in
    /// #771; this test pins that rf does NOT regress the same way).
    /// ZEB-1027: the decrypting local rf round is now rn=2; rn=1 falls
    /// through to the broadcast apply — this test exercises the rn=1
    /// fall-through with a non-member actor.
    #[test]
    fn rf_rn1_with_identity_from_non_committee_actor_rejected_zeb1025() {
        use crate::community_dfrost_types::{
            DfrostEventKind, RefreshRoundPayload, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let mallory = OwnerAddr([0x66; 16]);

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = vec![alice, bob];
        log.committee_state.threshold = 2;
        log.committee_state.max_signers = 2;

        let payload = RefreshRoundPayload {
            ceremony_id: [0x77u8; 32],
            round_num: 1,
            recipient_ciphertexts: None,
            package: Some(vec![0xde, 0xad]),
            attempt: 0,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();

        let result = log.apply_with_identity(
            SignedCommitteeEvent {
                tag: 'd',
                version: 1,
                committee_tier: 0,
                kind: DfrostEventKind::ProactiveRefresh,
                hlc: Hlc {
                    wall_ms: 6800,
                    logical: 0,
                    device_id: "t".into(),
                },
                actor: mallory,
                payload: pd,
                sig: vec![0u8; 64],
            },
            &alice,
            &[0u8; 32],
        );
        assert_eq!(result, Err(ApplyError::InvariantViolation));
        assert!(
            log.committee_state.pending_refresh.is_none(),
            "non-member rf rn=1 must not seed pending_refresh via the local path"
        );
        assert!(
            log.events_is_empty(),
            "rejected event must not be appended to the log"
        );
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
            space_id: None,
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
            space_id: None,
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
                purpose: SignPurpose::default(),
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
            space_id: None,
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
            space_id: None,
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
            space_id: None,
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
            space_id: None,
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
            space_id: None,
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
            log.events_is_empty(),
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
                purpose: SignPurpose::default(),
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
            space_id: None,
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

    // ── ZEB-1031 Task 6: SignPurpose ──────────────────────────────────

    #[test]
    fn pending_sign_session_purpose_defaults_beacon_on_legacy_blob() {
        // Mirrors the pre-ZEB-1031 PendingSignSession shape (no
        // `purpose` field at all) to prove #[serde(default)] loads
        // old-persisted sessions as SignPurpose::Beacon — the only
        // purpose that existed before this field. `local_nonces` is
        // omitted here too since it was already #[serde(skip)] before
        // this task and carries no wire representation either way.
        #[derive(Serialize)]
        struct LegacyPendingSignSession {
            message_hash: [u8; 32],
            contributions: BTreeMap<OwnerAddr, (Vec<u8>, Vec<u8>)>,
        }
        let legacy = LegacyPendingSignSession {
            message_hash: [0xCC; 32],
            contributions: BTreeMap::new(),
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&legacy, &mut buf).expect("encode legacy shape");
        let decoded: PendingSignSession =
            ciborium::from_reader(&buf[..]).expect("decode into current shape");
        assert_eq!(
            decoded.purpose,
            SignPurpose::Beacon,
            "a legacy session blob (no purpose field) must decode as Beacon"
        );
        assert_eq!(decoded.message_hash, [0xCC; 32], "public fields preserved");
    }

    #[test]
    fn pending_sign_session_reset_response_purpose_round_trips() {
        let session = PendingSignSession {
            message_hash: [0xDD; 32],
            purpose: SignPurpose::ResetResponse {
                proposal_id: [0x77; 16],
                verdict: crate::community_membership::ResetVerdict::Veto,
            },
            ..Default::default()
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&session, &mut buf).expect("encode");
        let decoded: PendingSignSession = ciborium::from_reader(&buf[..]).expect("decode");
        assert_eq!(
            decoded.purpose, session.purpose,
            "ResetResponse purpose must round-trip exactly (proposal_id + verdict)"
        );
    }

    #[test]
    fn apply_threshold_sign_creates_session_with_beacon_purpose_by_default() {
        // The wire-level `ts` payload carries no purpose signal — every
        // replica applying a `ts` event (whether or not this replica
        // itself initiated a reset-response ceremony) creates the
        // session at the Beacon default here. Regression guard for the
        // pre-existing VRF-beacon ceremony flow: `apply_threshold_sign`
        // must keep tagging fresh sessions Beacon so
        // `dfrost_contribute_threshold_sign`'s aggregation-side match
        // still takes the vb-mint arm for ordinary beacon ceremonies.
        use crate::community_dfrost_types::ThresholdSignPayload;
        use crate::owner_state_types::Hlc;

        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let kp = key_packages.get(&ids[0]).unwrap().clone();
        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, Some(kp));

        let payload = ThresholdSignPayload {
            ceremony_id: [0x11; 32],
            message_hash: [0x22; 32],
            commitment_bytes: vec![0xAB, 0xCD],
            share_bytes: Vec::new(),
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
            actor: members[0],
            payload: pd,
            sig: vec![0u8; 64],
        })
        .expect("ts applies");

        let session = log
            .committee_state
            .pending_sign
            .get(&[0x11; 32])
            .expect("session created on first ts contribution");
        assert_eq!(session.purpose, SignPurpose::Beacon);
    }

    /// Full happy-path replay of what
    /// `dfrost_contribute_threshold_sign`'s ResetResponse completion arm
    /// does at aggregation: a REAL 2-of-3 FROST threshold signature over
    /// the endorse-domain message hash (the SAME round1/round2/aggregate
    /// mechanics the Beacon path shares), wrapped in a
    /// `DfrostResetResponse` membership event that must (a) pass RS-R3
    /// against the committee's own vk and (b) leave the dfrost log's
    /// beacon state completely untouched — no `vb`, no beacon-index
    /// entry, and (mirroring the completion arm's explicit cleanup) the
    /// ceremony's `pending_sign` session gone.
    #[test]
    fn reset_response_ceremony_aggregation_produces_verifiable_response_no_vb() {
        use crate::community_membership::{
            dfrost_reset_digest, dfrost_reset_message_hash, mint_test_owner, sign_event,
            test_enroll_member, verify_event, EventId, EventPayload, MaterializedMembership,
            MemberState, MemberStatus, MembershipEventKind, ResetPhase, ResetProposalView,
            ResetVerdict, VerifyContext, DFROST_RESET_ENDORSE_DOMAIN,
        };
        use crate::owner_state_types::{Hlc, SpaceId};

        let community_id = SpaceId([0x51; 16]);
        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let target_vk =
            crate::community_dfrost_crypto::verifying_key_to_bytes(pub_pkg.verifying_key());
        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);

        let proposal_id: EventId = [0x91; 16];
        let new_members = vec![OwnerAddr([0x71; 16]), OwnerAddr([0x72; 16])];
        let digest =
            dfrost_reset_digest(&community_id, &proposal_id, &target_vk, 1, &new_members, 2)
                .expect("digest encode");
        let message_hash = dfrost_reset_message_hash(DFROST_RESET_ENDORSE_DOMAIN, &digest, None);

        let mut sign_tag = b"sign-v1:".to_vec();
        sign_tag.extend_from_slice(&message_hash);
        let ceremony_id =
            crate::community_dfrost_types::derive_ceremony_id(&community_id, 1, &sign_tag);
        // Seed the session the way `dfrost_initiate_reset_response_core`
        // would have (purpose tagged, message_hash pinned) — the
        // completion arm's cleanup removes exactly this entry.
        log.committee_state.pending_sign.insert(
            ceremony_id,
            PendingSignSession {
                message_hash,
                contributions: BTreeMap::new(),
                local_nonces: None,
                purpose: SignPurpose::ResetResponse {
                    proposal_id,
                    verdict: ResetVerdict::Endorse,
                },
            },
        );

        // Real 2-of-3 threshold sign of the endorse-domain message —
        // the SAME frost mechanics dfrost_contribute_threshold_sign's
        // shared round1/round2/aggregate path runs regardless of
        // ceremony purpose.
        let mut rng = frost_ristretto255::rand_core::OsRng;
        let mut nonces = BTreeMap::new();
        let mut commitments = BTreeMap::new();
        for id in &ids[..2] {
            let kp = key_packages.get(id).unwrap();
            let (n, c) = frost_ristretto255::round1::commit(kp.signing_share(), &mut rng);
            nonces.insert(*id, n);
            commitments.insert(*id, c);
        }
        let signing_package = frost_ristretto255::SigningPackage::new(commitments, &message_hash);
        let mut shares = BTreeMap::new();
        for id in &ids[..2] {
            let kp = key_packages.get(id).unwrap();
            shares.insert(
                *id,
                frost_ristretto255::round2::sign(&signing_package, nonces.get(id).unwrap(), kp)
                    .expect("round2 sign"),
            );
        }
        let sig =
            frost_ristretto255::aggregate(&signing_package, &shares, &pub_pkg).expect("aggregate");
        let sig_bytes = sig.serialize().expect("sig serialize");
        let group_sig: [u8; 64] = sig_bytes.try_into().expect("schnorr sig is 64 bytes");

        // Snapshot beacon state BEFORE the (simulated) completion arm
        // runs, so the after-assertions are a genuine before/after
        // comparison, not a vacuous "started empty, still empty".
        let watermark_before = crate::community_dfrost_catchup::beacon_watermark_of(&log, 1_000);
        assert!(
            log.beacon_index.is_empty(),
            "precondition: no beacon minted yet"
        );

        // The completion arm's cleanup — no `vb` apply exists to trigger
        // apply_vrf_beacon's pending_sign removal for a ResetResponse
        // ceremony, so it clears the slot itself.
        log.committee_state.pending_sign.remove(&ceremony_id);

        // Build the response event exactly as the lib.rs match arm
        // does: MembershipEventKind::DfrostResetResponse{target_event_id,
        // verdict, group_sig, new_vk: None}, signed by a courier actor
        // (RS-R1: any Joined member, not necessarily a committee
        // member, for endorse/veto).
        let courier = mint_test_owner(0x61);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            courier.owner,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "c".into(),
                },
                left_at: None,
                enrolled_device_keys: Default::default(),
                revoked_device_keys: Default::default(),
            },
        );
        test_enroll_member(&mut prior, &courier);
        prior.reset_proposals.push(ResetProposalView {
            id: proposal_id,
            proposer: courier.owner,
            target_vk,
            target_epoch: 1,
            new_members: new_members.clone(),
            new_threshold: 2,
            veto_window_ms: 24 * 3_600_000,
            signers: std::collections::BTreeSet::from([courier.owner]),
            proposed_at_wall_ms: 0,
            deadline_ms: None,
            authorized_at_ms: Some(100),
            endorsed: true,
            phase: ResetPhase::Authorized,
            consumed_new_vk: None,
            consumption_superseded: false,
        });
        let ctx = VerifyContext {
            now_ms: None,
            expected_community_id: community_id,
            admin_addr: courier.owner,
            is_invite_only: false,
        };

        let payload = EventPayload {
            id: [0x99; 16],
            community_id,
            kind: MembershipEventKind::DfrostResetResponse {
                target_event_id: proposal_id,
                verdict: ResetVerdict::Endorse,
                group_sig,
                new_vk: None,
            },
            actor: courier.owner,
            at: Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "c".into(),
            },
        };
        let response_event =
            sign_event(&payload, &courier.device_key).expect("sign_event succeeds");

        assert_eq!(
            verify_event(&response_event, &prior, &ctx),
            Ok(()),
            "the mint-site's DfrostResetResponse construction must pass RS-R3"
        );

        // NO vb, NO beacon-index change — nothing in this test (nor the
        // real completion arm it replays) ever calls apply_vrf_beacon or
        // touches beacon_index/beacon_watermark.
        assert!(
            log.beacon_index.is_empty(),
            "a ResetResponse ceremony must never mint vb / touch the beacon index"
        );
        assert_eq!(
            crate::community_dfrost_catchup::beacon_watermark_of(&log, 1_000),
            watermark_before,
            "beacon_watermark must be unchanged by a ResetResponse ceremony's completion"
        );
        assert!(
            log.committee_state.pending_sign.is_empty(),
            "the completion arm must clear its own pending_sign session"
        );
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

    // ── ZEB-753: VerifiedLog event-set adoption ─────────────────────────

    /// An EXACT duplicate (byte-identical event ⇒ same synthesized id)
    /// is a structural no-op on the second apply: same Ok result, no
    /// second stored copy, no handler re-run side effects. The old Vec
    /// backing pushed a second copy.
    #[test]
    fn exact_duplicate_apply_is_noop_zeb753() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let cid = [0x42u8; 32];
        let ev = di_event(alice, vec![alice, bob], 2, 1, cid, 1_000);

        let mut log = DfrostLog::new();
        assert_eq!(log.apply(ev.clone()), Ok(()));
        assert_eq!(log.event_count(), 1);
        let pending_before = log.committee_state.pending_dkg.clone().expect("pending");

        assert_eq!(log.apply(ev), Ok(()), "duplicate apply stays Ok");
        assert_eq!(log.event_count(), 1, "duplicate must not store again");
        assert_eq!(
            log.committee_state
                .pending_dkg
                .as_ref()
                .map(|p| p.ceremony_id),
            Some(pending_before.ceremony_id),
            "duplicate must not disturb pending state"
        );
    }

    /// A re-mint (same logical content, fresh HLC ⇒ distinct id) is NOT
    /// deduped — the transport's healing re-broadcasts depend on
    /// re-mints reaching the handlers.
    #[test]
    fn re_minted_event_is_distinct_not_deduped_zeb753() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let cid = [0x42u8; 32];
        let original = di_event(alice, vec![alice, bob], 2, 1, cid, 1_000);
        // Same payload identity; later HLC (a re-mint). di is idempotent
        // for the same initiator+shape, so the handler accepts it.
        let mut remint = di_event(alice, vec![alice, bob], 2, 1, cid, 1_000);
        remint.hlc.wall_ms = 2_000;

        let mut log = DfrostLog::new();
        assert_eq!(log.apply(original), Ok(()));
        assert_eq!(log.apply(remint), Ok(()));
        assert_eq!(
            log.event_count(),
            2,
            "a fresh-HLC re-mint is a distinct event in the set"
        );
    }

    /// `events()` iterates in HLC order regardless of apply order — the
    /// synthesized id is HLC-major, so id-ordered iteration IS HLC
    /// order. (The old Vec yielded arrival order and left ordering as a
    /// doc obligation on the caller.)
    #[test]
    fn events_iterate_in_hlc_order_zeb753() {
        use crate::community_dfrost_types::{DfrostEventKind, DkgRoundPayload};
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let cid = [0x42u8; 32];
        // di minted at wall 2_000, then a dr(rn=1) minted EARLIER
        // (wall 1_000) applied after — valid live shape (the dr only
        // needs the pending slot to exist at apply time).
        let di = di_event(alice, vec![alice, bob], 2, 1, cid, 2_000);
        let payload = DkgRoundPayload {
            ceremony_id: cid,
            round_num: 1,
            round1_package: Some(vec![0xde]),
            recipient_ciphertexts: None,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        let dr = crate::community_dfrost_types::SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgRound,
            hlc: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![7u8; 64],
        };

        let mut log = DfrostLog::new();
        log.apply(di).expect("di applies");
        log.apply(dr).expect("dr applies");
        let walls: Vec<u64> = log.events().map(|e| e.hlc.wall_ms).collect();
        assert_eq!(walls, vec![1_000, 2_000], "iteration is HLC order");
    }

    /// `from_restored` keeps the durable subset and clears the three
    /// pending slots — interactive rounds do not survive a restart.
    #[test]
    fn from_restored_clears_pending_slots_zeb753() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let cid = [0x42u8; 32];
        let mut log = DfrostLog::new();
        log.apply(di_event(alice, vec![alice, bob], 2, 1, cid, 1_000))
            .expect("di applies");
        log.committee_state
            .pending_sign
            .insert([0x33; 32], PendingSignSession::default());
        log.committee_state.pending_refresh = Some(PendingCeremony::default());
        log.committee_state.pending_repair = Some(PendingRepair::new(
            [0x55; 32],
            bob,
            1,
            vec![alice],
            1_000,
            0,
        ));
        log.beacon_index.insert([0x11; 32], [0x22; 32]);

        let restored = DfrostLog::from_restored(
            log.export_events(),
            log.committee_state.clone(),
            log.beacon_index.clone(),
        );
        assert_eq!(restored.event_count(), 1, "events survive");
        assert_eq!(
            restored.beacon_index.get(&[0x11; 32]),
            Some(&[0x22; 32]),
            "beacon index survives"
        );
        assert!(restored.committee_state.pending_dkg.is_none());
        assert!(restored.committee_state.pending_sign.is_empty());
        assert!(restored.committee_state.pending_refresh.is_none());
        assert!(
            restored.committee_state.pending_repair.is_none(),
            "ZEB-1027: the repair slot is the fourth cleared pending slot"
        );
        assert!(restored.local_dkg_secret.is_none());
        assert!(restored.local_key_package.is_none());
    }

    // ── ZEB-1027: refresh rn=2 local decrypt ─────────────────────────────

    /// The decrypting local refresh round is rn=2 (mirroring DkgRound
    /// rn=2); a sealed-to-self package lands in
    /// `pending_refresh.round2_packages[sender]`.
    #[test]
    fn rf_rn2_with_identity_decrypts_share_package_zeb1027() {
        use crate::community_dfrost_types::{
            DfrostEventKind, RefreshRoundPayload, SignedCommitteeEvent,
        };
        use crate::community_membership::RecipientCiphertext;
        use crate::dm_signing;
        use crate::owner_state_types::Hlc;
        use x25519_dalek::{PublicKey, StaticSecret};

        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let alice_priv = [0x42u8; 32];
        let alice_x25519_pub = *PublicKey::from(&StaticSecret::from(alice_priv)).as_bytes();
        let ceremony_id = [0x77u8; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = vec![alice, bob];
        log.committee_state.threshold = 2;
        log.committee_state.max_signers = 2;
        log.committee_state.pending_refresh = Some(PendingCeremony {
            ceremony_id,
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
            proposed_epoch: 2,
            ..Default::default()
        });

        let plain = vec![0xca, 0xfe];
        let sealed = dm_signing::seal_to_owner(&alice_x25519_pub, &plain).expect("seal");
        let payload = RefreshRoundPayload {
            ceremony_id,
            round_num: 2,
            recipient_ciphertexts: Some(vec![RecipientCiphertext {
                recipient: alice,
                sealed,
            }]),
            package: None,
            attempt: 0,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        log.apply_with_identity(
            SignedCommitteeEvent {
                tag: 'd',
                version: 1,
                committee_tier: 0,
                kind: DfrostEventKind::ProactiveRefresh,
                hlc: Hlc {
                    wall_ms: 7000,
                    logical: 0,
                    device_id: "t".into(),
                },
                actor: bob,
                payload: pd,
                sig: vec![0u8; 64],
            },
            &alice,
            &alice_priv,
        )
        .expect("rf rn=2 applies");

        assert_eq!(
            log.committee_state
                .pending_refresh
                .as_ref()
                .unwrap()
                .round2_packages
                .get(&bob),
            Some(&plain)
        );
    }

    /// Qodo #2 (#775 round 2): rf rn=2 must seal to EVERY other
    /// ceremony member exactly once — an accepted event missing a
    /// recipient would stall the refresh permanently (that member's
    /// finalization can never see this sender's package).
    #[test]
    fn rf_rn2_recipient_set_completeness_zeb1027() {
        use crate::community_dfrost_types::{
            DfrostEventKind, RefreshRoundPayload, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let carol = OwnerAddr([0x03; 16]);
        let mallory = OwnerAddr([0x66; 16]);
        let ceremony_id = [0x78u8; 32];

        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = vec![alice, bob, carol];
        log.committee_state.threshold = 2;
        log.committee_state.max_signers = 3;
        log.committee_state.pending_refresh = Some(PendingCeremony {
            ceremony_id,
            members: vec![alice, bob, carol],
            threshold: 2,
            max_signers: 3,
            proposed_epoch: 2,
            ..Default::default()
        });

        let cts_for = |recipients: &[OwnerAddr]| {
            recipients
                .iter()
                .map(|r| crate::community_membership::RecipientCiphertext {
                    recipient: *r,
                    sealed: vec![0u8; 4],
                })
                .collect::<Vec<_>>()
        };
        let mut wall = 7_200u64;
        let mut rn2 = |rc: Vec<crate::community_membership::RecipientCiphertext>| {
            let payload = RefreshRoundPayload {
                ceremony_id,
                round_num: 2,
                recipient_ciphertexts: Some(rc),
                package: None,
                attempt: 0,
            };
            let mut pd = Vec::new();
            ciborium::into_writer(&payload, &mut pd).unwrap();
            wall += 1;
            SignedCommitteeEvent {
                tag: 'd',
                version: 1,
                committee_tier: 0,
                kind: DfrostEventKind::ProactiveRefresh,
                hlc: Hlc {
                    wall_ms: wall,
                    logical: 0,
                    device_id: "t".into(),
                },
                actor: bob,
                payload: pd,
                sig: vec![0u8; 64],
            }
        };

        // Empty, missing a member, duplicate, self-addressed, and
        // non-member sets are all malformed.
        for bad in [
            vec![],
            cts_for(&[alice]),
            cts_for(&[alice, alice]),
            cts_for(&[alice, bob, carol]),
            cts_for(&[alice, mallory]),
        ] {
            assert_eq!(
                log.apply(rn2(bad)),
                Err(ApplyError::InvariantViolation),
                "incomplete recipient set must be rejected"
            );
        }
        // Exactly {alice, carol} (= members ∖ {bob}) is well-formed.
        log.apply(rn2(cts_for(&[alice, carol])))
            .expect("complete recipient set applies");
    }

    /// An rf rn=1 while a (stray) DKG occupies its slot is refused —
    /// the two transcripts must never interleave.
    #[test]
    fn rf_rn1_while_pending_dkg_rejected_zeb1027() {
        use crate::community_dfrost_types::{
            DfrostEventKind, RefreshRoundPayload, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = vec![alice];
        log.committee_state.threshold = 1;
        log.committee_state.max_signers = 1;
        log.committee_state.pending_dkg = Some(PendingCeremony::default());

        let payload = RefreshRoundPayload {
            ceremony_id: [0x77u8; 32],
            round_num: 1,
            recipient_ciphertexts: None,
            package: Some(vec![0x01]),
            attempt: 0,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        let result = log.apply(SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ProactiveRefresh,
            hlc: Hlc {
                wall_ms: 7100,
                logical: 0,
                device_id: "t".into(),
            },
            actor: alice,
            payload: pd,
            sig: vec![0u8; 64],
        });
        assert_eq!(result, Err(ApplyError::CeremonyInFlight));
    }

    // ── ZEB-1027: share-repair (`rp`) unit tests ─────────────────────────

    fn rp_event(
        actor: OwnerAddr,
        ceremony_id: [u8; 32],
        round_num: u8,
        epoch: u64,
        helpers: Option<Vec<OwnerAddr>>,
        rc: Option<Vec<crate::community_membership::RecipientCiphertext>>,
        wall_ms: u64,
    ) -> SignedCommitteeEvent {
        rp_event_stamped(
            actor,
            ceremony_id,
            round_num,
            epoch,
            helpers,
            rc,
            wall_ms,
            1_000,
            0,
        )
    }

    /// `rp_event` with an explicit mint stamp — the arbitration tests
    /// need distinct stamps to exercise `PendingRepair::rank`.
    #[allow(clippy::too_many_arguments)]
    fn rp_event_stamped(
        actor: OwnerAddr,
        ceremony_id: [u8; 32],
        round_num: u8,
        epoch: u64,
        helpers: Option<Vec<OwnerAddr>>,
        rc: Option<Vec<crate::community_membership::RecipientCiphertext>>,
        wall_ms: u64,
        minted_wall_ms: u64,
        minted_logical: u32,
    ) -> SignedCommitteeEvent {
        use crate::community_dfrost_types::RepairRoundPayload;
        use crate::owner_state_types::Hlc;
        let payload = RepairRoundPayload {
            ceremony_id,
            round_num,
            epoch,
            helpers,
            minted_wall_ms: if round_num == 1 {
                Some(minted_wall_ms)
            } else {
                None
            },
            minted_logical: if round_num == 1 {
                Some(minted_logical)
            } else {
                None
            },
            recipient_ciphertexts: rc,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::RepairShare,
            hlc: Hlc {
                wall_ms,
                logical: 0,
                device_id: "t".into(),
            },
            actor,
            payload: pd,
            sig: vec![0u8; 64],
        }
    }

    /// Three-member committee scaffold for repair tests: alice, bob,
    /// carol; threshold 2; epoch 1; no key material (tests seed what
    /// they need).
    fn repair_committee_log() -> (DfrostLog, OwnerAddr, OwnerAddr, OwnerAddr) {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let carol = OwnerAddr([0x03; 16]);
        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = vec![alice, bob, carol];
        log.committee_state.threshold = 2;
        log.committee_state.max_signers = 3;
        log.committee_state.identifier_map =
            CommitteeState::build_identifier_map(&[alice, bob, carol]);
        (log, alice, bob, carol)
    }

    #[test]
    fn rp_rn1_seeds_pending_repair_zeb1027() {
        let (mut log, alice, bob, carol) = repair_committee_log();
        log.apply(rp_event(
            alice,
            [0xab; 32],
            1,
            1,
            Some(vec![bob, carol]),
            None,
            8_000,
        ))
        .expect("rn=1 applies");
        let p = log.committee_state.pending_repair.as_ref().unwrap();
        assert_eq!(p.participant, alice);
        assert_eq!(p.epoch, 1);
        assert_eq!(p.helpers, vec![bob, carol]);
        assert!(p.round2_seen.is_empty() && p.round3_seen.is_empty());
    }

    #[test]
    fn rp_rn1_rejections_zeb1027() {
        // Non-member actor.
        let (mut log, _alice, bob, carol) = repair_committee_log();
        let mallory = OwnerAddr([0x66; 16]);
        assert_eq!(
            log.apply(rp_event(
                mallory,
                [0xab; 32],
                1,
                1,
                Some(vec![bob, carol]),
                None,
                8_001,
            )),
            Err(ApplyError::InvariantViolation)
        );
        // Helpers below threshold (t=2, one declared helper).
        let (mut log, alice, bob, _carol) = repair_committee_log();
        assert_eq!(
            log.apply(rp_event(
                alice,
                [0xab; 32],
                1,
                1,
                Some(vec![bob]),
                None,
                8_002
            )),
            Err(ApplyError::InvariantViolation)
        );
        // Wrong epoch.
        let (mut log, alice, bob, carol) = repair_committee_log();
        assert_eq!(
            log.apply(rp_event(
                alice,
                [0xab; 32],
                1,
                7,
                Some(vec![bob, carol]),
                None,
                8_003,
            )),
            Err(ApplyError::InvariantViolation)
        );
        // Participant listed as its own helper.
        let (mut log, alice, _bob, carol) = repair_committee_log();
        assert_eq!(
            log.apply(rp_event(
                alice,
                [0xab; 32],
                1,
                1,
                Some(vec![alice, carol]),
                None,
                8_004,
            )),
            Err(ApplyError::InvariantViolation)
        );
        // Refresh in flight → repair waits.
        let (mut log, alice, bob, carol) = repair_committee_log();
        log.committee_state.pending_refresh = Some(PendingCeremony::default());
        assert_eq!(
            log.apply(rp_event(
                alice,
                [0xab; 32],
                1,
                1,
                Some(vec![bob, carol]),
                None,
                8_005,
            )),
            Err(ApplyError::CeremonyInFlight)
        );
        assert!(
            log.committee_state.pending_repair.is_none(),
            "no rejected rn=1 may seed the slot"
        );
    }

    #[test]
    fn rp_rn1_supersede_and_tiebreak_zeb1027() {
        // Same participant: the NEWER mint stamp wins — in both arrival
        // orders (`PendingRepair::rank` puts newer stamps first).
        let (mut log, alice, bob, carol) = repair_committee_log();
        log.apply(rp_event_stamped(
            alice,
            [0xab; 32],
            1,
            1,
            Some(vec![bob, carol]),
            None,
            8_100,
            1_000,
            0,
        ))
        .expect("first request");
        log.apply(rp_event_stamped(
            alice,
            [0xac; 32],
            1,
            1,
            Some(vec![bob, carol]),
            None,
            8_101,
            2_000,
            0,
        ))
        .expect("participant's newer retry supersedes its own ceremony");
        assert_eq!(
            log.committee_state
                .pending_repair
                .as_ref()
                .unwrap()
                .ceremony_id,
            [0xac; 32]
        );
        // Reverse order: the OLDER stamp arriving late is refused — the
        // outcome is the same ceremony either way.
        let (mut log, alice, bob, carol) = repair_committee_log();
        log.apply(rp_event_stamped(
            alice,
            [0xac; 32],
            1,
            1,
            Some(vec![bob, carol]),
            None,
            8_102,
            2_000,
            0,
        ))
        .expect("newer request first");
        assert_eq!(
            log.apply(rp_event_stamped(
                alice,
                [0xab; 32],
                1,
                1,
                Some(vec![bob, carol]),
                None,
                8_103,
                1_000,
                0,
            )),
            Err(ApplyError::CeremonyInFlight)
        );
        assert_eq!(
            log.committee_state
                .pending_repair
                .as_ref()
                .unwrap()
                .ceremony_id,
            [0xac; 32]
        );

        // Racing requests from DIFFERENT participants: the smaller
        // participant wins…
        let (mut log, alice, bob, carol) = repair_committee_log();
        log.apply(rp_event(
            bob,
            [0xab; 32],
            1,
            1,
            Some(vec![alice, carol]),
            None,
            8_104,
        ))
        .expect("bob's request");
        log.apply(rp_event(
            alice,
            [0xad; 32],
            1,
            1,
            Some(vec![bob, carol]),
            None,
            8_105,
        ))
        .expect("alice (smaller addr) wins the arbitration");
        assert_eq!(
            log.committee_state
                .pending_repair
                .as_ref()
                .unwrap()
                .participant,
            alice
        );
        // …and the larger loses against an incumbent smaller one.
        assert_eq!(
            log.apply(rp_event(
                carol,
                [0xae; 32],
                1,
                1,
                Some(vec![alice, bob]),
                None,
                8_106,
            )),
            Err(ApplyError::CeremonyInFlight)
        );
        // Helper progress does NOT protect the incumbent (#775 round 2,
        // Greptile P1 / Qodo #1): a progress-gated rule is itself
        // arrival-order-dependent — a replica that saw the helper round
        // first would keep the incumbent while one that saw the
        // challenger first replaced it, and the two then reject each
        // other's helper events. The smaller-ranked challenger wins
        // everywhere; the displaced ceremony's progress dies with its
        // slot.
        let (mut log, alice, bob, carol) = repair_committee_log();
        log.apply(rp_event(
            carol,
            [0xab; 32],
            1,
            1,
            Some(vec![alice, bob]),
            None,
            8_107,
        ))
        .expect("carol's request");
        log.committee_state
            .pending_repair
            .as_mut()
            .unwrap()
            .round2_seen
            .insert(bob);
        log.apply(rp_event(
            alice,
            [0xaf; 32],
            1,
            1,
            Some(vec![bob, carol]),
            None,
            8_108,
        ))
        .expect("smaller-ranked challenger replaces a progressed incumbent");
        let p = log.committee_state.pending_repair.as_ref().unwrap();
        assert_eq!(p.participant, alice);
        assert!(
            p.round2_seen.is_empty(),
            "displaced ceremony's progress must not leak into the winner"
        );
    }

    /// #775 round 2 (Greptile P1 / Qodo #1): the SAME event set must
    /// yield the SAME incumbent on every replica regardless of arrival
    /// order — including interleaved helper progress for the ceremony
    /// that ends up losing.
    #[test]
    fn rp_rn1_arbitration_commutes_across_arrival_orders_zeb1027() {
        let dummy_cts = |recipients: &[OwnerAddr]| {
            recipients
                .iter()
                .map(|r| crate::community_membership::RecipientCiphertext {
                    recipient: *r,
                    sealed: vec![0u8; 4],
                })
                .collect::<Vec<_>>()
        };

        let (_, alice, bob, carol) = repair_committee_log();
        // Bob's request (larger participant → loses the arbitration),
        // a helper round progressing IT, and alice's request.
        let e_bob = rp_event(bob, [0xbb; 32], 1, 1, Some(vec![alice, carol]), None, 8_200);
        let e_help = rp_event(
            carol,
            [0xbb; 32],
            2,
            1,
            None,
            Some(dummy_cts(&[alice, carol])),
            8_201,
        );
        let e_alice = rp_event(alice, [0xaa; 32], 1, 1, Some(vec![bob, carol]), None, 8_202);

        // Replica 1 sees bob's ceremony start and progress before
        // alice's request arrives.
        let (mut log1, ..) = repair_committee_log();
        log1.apply(e_bob.clone()).expect("bob seeds");
        log1.apply(e_help.clone()).expect("helper progresses bob's");
        log1.apply(e_alice.clone())
            .expect("alice replaces despite progress");

        // Replica 2 sees alice first; bob's request and the helper
        // round bounce off her incumbency.
        let (mut log2, ..) = repair_committee_log();
        log2.apply(e_alice).expect("alice seeds");
        assert_eq!(log2.apply(e_bob), Err(ApplyError::CeremonyInFlight));
        assert_eq!(log2.apply(e_help), Err(ApplyError::UnknownCeremony));

        let p1 = log1.committee_state.pending_repair.as_ref().unwrap();
        let p2 = log2.committee_state.pending_repair.as_ref().unwrap();
        assert_eq!(
            (p1.participant, p1.ceremony_id, p1.round2_seen.clone()),
            (p2.participant, p2.ceremony_id, p2.round2_seen.clone()),
            "replicas must converge on the same incumbent with the same progress"
        );
    }

    /// ZEB-1028: rf rn=1 event with an explicit attempt counter.
    fn rf1_event(
        actor: OwnerAddr,
        ceremony_id: [u8; 32],
        attempt: u32,
        wall_ms: u64,
    ) -> SignedCommitteeEvent {
        use crate::community_dfrost_types::RefreshRoundPayload;
        use crate::owner_state_types::Hlc;
        let payload = RefreshRoundPayload {
            ceremony_id,
            round_num: 1,
            recipient_ciphertexts: None,
            package: Some(vec![0xde, 0xad]),
            attempt,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ProactiveRefresh,
            hlc: Hlc {
                wall_ms,
                logical: 0,
                device_id: "t".into(),
            },
            actor,
            payload: pd,
            sig: vec![0u8; 64],
        }
    }

    /// ZEB-1028: the max-attempt refresh arbitration must converge
    /// replicas on the same incumbent from the same event set in ANY
    /// arrival order, and displacement must wipe the displaced
    /// attempt's transcript secrets + staged rotation.
    #[test]
    fn rf_rn1_attempt_supersede_commutes_zeb1028() {
        let (mut log1, alice, bob, _carol) = repair_committee_log();
        let e_a0 = rf1_event(alice, [0xA0; 32], 0, 9_000);
        let e_b1 = rf1_event(bob, [0xA1; 32], 1, 9_001);
        let e_a1 = rf1_event(alice, [0xA1; 32], 1, 9_002);

        // Replica 1: attempt 0 seeds, gathers local transcript secrets
        // + a staged rotation, then attempt 1 displaces — the stale
        // attempt's material must die with it.
        log1.apply(e_a0.clone()).expect("attempt 0 seeds");
        let id = crate::community_dfrost_crypto::identifier_for_index(0);
        let (secret, _pkg) =
            crate::community_dfrost_crypto::dkg_part1_local(id, 2, 2).expect("part1");
        log1.local_dkg_secret = Some(secret);
        log1.pending_rotated = Some(dealer_key_material_for_tests());
        log1.apply(e_b1.clone()).expect("attempt 1 displaces");
        assert!(
            log1.local_dkg_secret.is_none() && log1.pending_rotated.is_none(),
            "displaced attempt's transcript secrets and staged rotation must be wiped"
        );
        log1.apply(e_a1.clone()).expect("alice joins attempt 1");

        // Replica 2: attempt 1 arrives first; the stale attempt-0
        // replay bounces off it.
        let (mut log2, ..) = repair_committee_log();
        log2.apply(e_b1).expect("attempt 1 seeds");
        log2.apply(e_a1).expect("alice joins");
        assert_eq!(
            log2.apply(e_a0),
            Err(ApplyError::CeremonyInFlight),
            "lower-attempt replay is dropped, not an error to surface"
        );

        let p1 = log1.committee_state.pending_refresh.as_ref().unwrap();
        let p2 = log2.committee_state.pending_refresh.as_ref().unwrap();
        assert_eq!(
            (
                p1.ceremony_id,
                p1.attempt,
                p1.round1_packages.keys().copied().collect::<Vec<_>>()
            ),
            (
                p2.ceremony_id,
                p2.attempt,
                p2.round1_packages.keys().copied().collect::<Vec<_>>()
            ),
            "replicas must converge on the same attempt with the same round-1 set"
        );
        assert_eq!(p1.attempt, 1);
    }

    /// ZEB-1028: two DIFFERENT ids at the same attempt cannot both
    /// derive from the shared committee shape — a fork is rejected
    /// loudly (the engine's ingest gate never admits one; this pins the
    /// direct-apply behaviour).
    #[test]
    fn rf_rn1_equal_attempt_fork_rejected_zeb1028() {
        let (mut log, alice, bob, _carol) = repair_committee_log();
        log.apply(rf1_event(alice, [0xA0; 32], 0, 9_100))
            .expect("seeds");
        assert_eq!(
            log.apply(rf1_event(bob, [0xF0; 32], 0, 9_101)),
            Err(ApplyError::InvariantViolation),
            "same-attempt divergent id is a forked/forged proposal"
        );
    }

    /// ZEB-1028: `abort_pending_refresh` clears the slot, the
    /// zero-sharing transcript secrets, and the staged-but-unpromoted
    /// rotation; the active committee (and its current share) survives.
    #[test]
    fn abort_pending_refresh_clears_secrets_and_stage_zeb1028() {
        let (mut log, alice, ..) = repair_committee_log();
        assert_eq!(log.abort_pending_refresh(), None, "no-op on empty slot");
        log.apply(rf1_event(alice, [0xA0; 32], 0, 9_200))
            .expect("seeds");
        let id = crate::community_dfrost_crypto::identifier_for_index(0);
        let (secret, _pkg) =
            crate::community_dfrost_crypto::dkg_part1_local(id, 2, 2).expect("part1");
        log.local_dkg_secret = Some(secret);
        log.pending_rotated = Some(dealer_key_material_for_tests());

        assert_eq!(log.abort_pending_refresh(), Some([0xA0; 32]));
        assert!(log.committee_state.pending_refresh.is_none());
        assert!(log.local_dkg_secret.is_none());
        assert!(log.local_dkg_secret2.is_none());
        assert!(
            log.pending_rotated.is_none(),
            "a rotation staged from the aborted transcript must never install"
        );
        assert!(
            log.committee_state.active,
            "abort leaves the committee signing at its current epoch"
        );
    }

    /// ZEB-1028: `abort_pending_repair` clears the slot (the engine's
    /// stale-replace admission uses it before seeding a competing
    /// request over a dead incumbent).
    #[test]
    fn abort_pending_repair_clears_slot_zeb1028() {
        let (mut log, alice, bob, carol) = repair_committee_log();
        assert_eq!(log.abort_pending_repair(), None, "no-op on empty slot");
        log.apply(rp_event(
            alice,
            [0xab; 32],
            1,
            1,
            Some(vec![bob, carol]),
            None,
            9_300,
        ))
        .expect("request seeds");
        assert_eq!(log.abort_pending_repair(), Some([0xab; 32]));
        assert!(log.committee_state.pending_repair.is_none());
    }

    #[test]
    fn rp_round_tracking_and_helper_gates_zeb1027() {
        let cts_for = |recipients: &[OwnerAddr]| {
            recipients
                .iter()
                .map(|r| crate::community_membership::RecipientCiphertext {
                    recipient: *r,
                    sealed: vec![0u8; 4],
                })
                .collect::<Vec<_>>()
        };
        let (mut log, alice, bob, carol) = repair_committee_log();
        log.apply(rp_event(
            alice,
            [0xab; 32],
            1,
            1,
            Some(vec![bob, carol]),
            None,
            8_300,
        ))
        .expect("request");
        // rn=2 from a declared helper with a COMPLETE delta set (one
        // per declared helper, self included) tracks.
        log.apply(rp_event(
            bob,
            [0xab; 32],
            2,
            1,
            None,
            Some(cts_for(&[bob, carol])),
            8_301,
        ))
        .expect("helper rn=2");
        assert!(log
            .committee_state
            .pending_repair
            .as_ref()
            .unwrap()
            .round2_seen
            .contains(&bob));
        // The PARTICIPANT is not a helper — its rn=2 is malformed.
        assert_eq!(
            log.apply(rp_event(
                alice,
                [0xab; 32],
                2,
                1,
                None,
                Some(cts_for(&[bob, carol])),
                8_302,
            )),
            Err(ApplyError::InvariantViolation)
        );
        // Missing ciphertext vector is malformed.
        assert_eq!(
            log.apply(rp_event(carol, [0xab; 32], 2, 1, None, None, 8_303)),
            Err(ApplyError::InvariantViolation)
        );
        // Qodo #3 (#775 round 2): an INCOMPLETE delta set must not mark
        // round progress — empty, missing a declared helper, holding a
        // duplicate, or addressed outside the helper set.
        for bad in [
            vec![],
            cts_for(&[carol]),
            cts_for(&[carol, carol]),
            cts_for(&[alice, carol]),
        ] {
            assert_eq!(
                log.apply(rp_event(carol, [0xab; 32], 2, 1, None, Some(bad), 8_304)),
                Err(ApplyError::InvariantViolation)
            );
        }
        assert!(
            !log.committee_state
                .pending_repair
                .as_ref()
                .unwrap()
                .round2_seen
                .contains(&carol),
            "rejected rounds must not count as progress"
        );
        // rn=3 must be exactly ONE sigma sealed to the participant.
        log.committee_state
            .pending_repair
            .as_mut()
            .unwrap()
            .round2_seen
            .insert(carol);
        for bad in [
            vec![],
            cts_for(&[bob]),
            cts_for(&[alice, alice]),
            cts_for(&[alice, bob]),
        ] {
            assert_eq!(
                log.apply(rp_event(carol, [0xab; 32], 3, 1, None, Some(bad), 8_305)),
                Err(ApplyError::InvariantViolation)
            );
        }
        log.apply(rp_event(
            carol,
            [0xab; 32],
            3,
            1,
            None,
            Some(cts_for(&[alice])),
            8_306,
        ))
        .expect("well-formed rn=3");
        assert!(log
            .committee_state
            .pending_repair
            .as_ref()
            .unwrap()
            .round3_seen
            .contains(&carol));
        // Unknown ceremony.
        assert_eq!(
            log.apply(rp_event(
                carol,
                [0xff; 32],
                2,
                1,
                None,
                Some(cts_for(&[bob, carol])),
                8_307,
            )),
            Err(ApplyError::UnknownCeremony)
        );
    }

    /// Real 2-of-3 DKG via the crypto wrappers, shared by the ZEB-1027
    /// repair tests. Returns (members, ids, per-id KeyPackages, joint
    /// PublicKeyPackage).
    #[allow(clippy::type_complexity)]
    fn dkg_2of3_material() -> (
        Vec<OwnerAddr>,
        Vec<frost_ristretto255::Identifier>,
        std::collections::BTreeMap<
            frost_ristretto255::Identifier,
            frost_ristretto255::keys::KeyPackage,
        >,
        frost_ristretto255::keys::PublicKeyPackage,
    ) {
        use crate::community_dfrost_crypto as dc;
        let members: Vec<OwnerAddr> = vec![
            OwnerAddr([0x01; 16]),
            OwnerAddr([0x02; 16]),
            OwnerAddr([0x03; 16]),
        ];
        let ids: Vec<frost_ristretto255::Identifier> =
            (0..3).map(dc::identifier_for_index).collect();
        let mut r1_secrets = std::collections::BTreeMap::new();
        let mut r1_pkgs: BTreeMap<frost_ristretto255::Identifier, Vec<u8>> = BTreeMap::new();
        for id in &ids {
            let (sec, pkg) = dc::dkg_part1_local(*id, 3, 2).unwrap();
            r1_secrets.insert(*id, sec);
            r1_pkgs.insert(*id, pkg);
        }
        let mut r2_secrets = std::collections::BTreeMap::new();
        let mut r2_out: BTreeMap<
            frost_ristretto255::Identifier,
            BTreeMap<frost_ristretto255::Identifier, Vec<u8>>,
        > = BTreeMap::new();
        for id in &ids {
            let sec = r1_secrets.remove(id).unwrap();
            let recv: BTreeMap<_, _> = r1_pkgs
                .iter()
                .filter(|(o, _)| *o != id)
                .map(|(o, b)| (*o, b.clone()))
                .collect();
            let (s2, out) = dc::dkg_part2_local(sec, &recv).unwrap();
            r2_secrets.insert(*id, s2);
            r2_out.insert(*id, out);
        }
        let mut key_packages = std::collections::BTreeMap::new();
        let mut pub_pkg = None;
        for id in &ids {
            let recv_r1: BTreeMap<_, _> = r1_pkgs
                .iter()
                .filter(|(o, _)| *o != id)
                .map(|(o, b)| (*o, b.clone()))
                .collect();
            let mut recv_r2 = BTreeMap::new();
            for (sender, out) in &r2_out {
                if sender != id {
                    recv_r2.insert(*sender, out.get(id).cloned().unwrap());
                }
            }
            let (kp, pkp) =
                dc::dkg_part3_local(r2_secrets.get(id).unwrap(), &recv_r1, &recv_r2).unwrap();
            key_packages.insert(*id, kp);
            pub_pkg = Some(pkp);
        }
        (members, ids, key_packages, pub_pkg.unwrap())
    }

    /// Committee public state as a restored (post-snapshot-load) log
    /// would hold it — everything but local secret material.
    fn committee_log_from_material(
        members: &[OwnerAddr],
        ids: &[frost_ristretto255::Identifier],
        pub_pkg: &frost_ristretto255::keys::PublicKeyPackage,
        kp: Option<frost_ristretto255::keys::KeyPackage>,
    ) -> DfrostLog {
        use crate::community_dfrost_crypto as dc;
        let joint_vk = dc::verifying_key_to_bytes(pub_pkg.verifying_key());
        let mut verifying_shares: BTreeMap<OwnerAddr, [u8; 32]> = BTreeMap::new();
        for (i, addr) in members.iter().enumerate() {
            let vs = pub_pkg.verifying_shares().get(&ids[i]).unwrap();
            verifying_shares.insert(*addr, dc::verifying_share_to_bytes(vs));
        }
        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = members.to_vec();
        log.committee_state.threshold = 2;
        log.committee_state.max_signers = 3;
        log.committee_state.joint_verifying_key = Some(joint_vk);
        log.committee_state.verifying_shares = verifying_shares;
        log.committee_state.identifier_map = CommitteeState::build_identifier_map(members);
        log.local_key_package = kp;
        log
    }

    /// ZEB-1029: this member's signing-share scalar bytes, as the sealed
    /// sidecar stores them.
    fn share_scalar_bytes(kp: &frost_ristretto255::keys::KeyPackage) -> [u8; 32] {
        let v = kp.signing_share().serialize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        out
    }

    /// ZEB-1029 happy path: a stored signing-share scalar from a real
    /// DKG validates against the restored consensus (G·x check) and
    /// reinstalls BOTH key packages on a restored-shape log.
    #[test]
    fn install_restored_share_happy_path_zeb1029() {
        use crate::community_dfrost_crypto as dc;
        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let share = share_scalar_bytes(key_packages.get(&ids[0]).unwrap());

        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
        log.install_restored_share(&alice, 1, &share)
            .expect("install succeeds");
        let installed = log.local_key_package.as_ref().expect("share installed");
        assert_eq!(
            dc::verifying_share_to_bytes(installed.verifying_share()),
            *log.committee_state.verifying_shares.get(&alice).unwrap(),
            "installed share's verifying share matches consensus"
        );
        let pkp = log
            .local_pub_key_package
            .as_ref()
            .expect("pub key package rebuilt from public state");
        assert_eq!(
            dc::verifying_key_to_bytes(pkp.verifying_key()),
            log.committee_state.joint_verifying_key.unwrap(),
            "rebuilt pub package carries the joint vk"
        );
    }

    /// ZEB-1029: a share minted at an older epoch (the committee
    /// refreshed while this node was down) is refused before any
    /// crypto — repair is the recovery, not a stale share.
    #[test]
    fn install_restored_share_rejects_epoch_mismatch_zeb1029() {
        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let share = share_scalar_bytes(key_packages.get(&ids[0]).unwrap());

        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
        log.committee_state.current_epoch = 2; // committee moved on
        let err = log
            .install_restored_share(&alice, 1, &share)
            .expect_err("stale epoch must be rejected");
        assert!(
            err.contains("epoch"),
            "error names the epoch mismatch: {err}"
        );
        assert!(log.local_key_package.is_none(), "nothing installed");
        assert!(log.local_pub_key_package.is_none());
    }

    /// ZEB-1029: a share whose derived `G·x` does not match this
    /// member's consensus verifying share (foreign share, or the
    /// verifying shares rotated under an unchanged epoch counter) is
    /// refused — the settle_repair consensus check, applied at restore.
    #[test]
    fn install_restored_share_rejects_consensus_mismatch_zeb1029() {
        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        // Bob's perfectly valid share is NOT alice's share.
        let bob_share = share_scalar_bytes(key_packages.get(&ids[1]).unwrap());

        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
        let err = log
            .install_restored_share(&alice, 1, &bob_share)
            .expect_err("foreign share must be rejected");
        assert!(
            err.contains("consensus"),
            "error names the consensus mismatch: {err}"
        );
        assert!(log.local_key_package.is_none(), "nothing installed");
    }

    /// ZEB-1029: no install on an inactive committee — a share with
    /// nothing to sign for is refused (e.g. the committee snapshot was
    /// quarantined and the log spawned fresh, but the share file
    /// survived).
    #[test]
    fn install_restored_share_rejects_inactive_committee_zeb1029() {
        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let share = share_scalar_bytes(key_packages.get(&ids[0]).unwrap());

        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
        log.committee_state.active = false;
        assert!(log.install_restored_share(&alice, 1, &share).is_err());
        assert!(log.local_key_package.is_none());
    }

    /// ZEB-1029: bit-rot that survives the AEAD (only reachable through
    /// a bug, but the check is one line) — a non-canonical scalar is
    /// rejected by the G·x derivation, never fed to FROST.
    #[test]
    fn install_restored_share_rejects_noncanonical_scalar_zeb1029() {
        let (members, ids, _key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
        let err = log
            .install_restored_share(&alice, 1, &[0xff; 32])
            .expect_err("non-canonical scalar must be rejected");
        assert!(
            err.contains("canonical"),
            "error names the canonicality failure: {err}"
        );
        assert!(log.local_key_package.is_none());
    }

    /// ZEB-1027 headline flow, real crypto end-to-end at the log layer:
    /// a 2-of-3 committee where alice LOST her share (fresh log, public
    /// state only — the restored-from-snapshot shape). Bob and carol
    /// help; every event flows through `apply_with_identity` on all
    /// three logs exactly as engine ingest would deliver it. Alice's
    /// finalization runs inline on the last sigma and must reinstall
    /// the EXACT original signing share; helper slots self-clear.
    #[test]
    fn repair_full_flow_reconstructs_and_installs_share_zeb1027() {
        use crate::community_dfrost_crypto as dc;
        use crate::community_membership::RecipientCiphertext;
        use crate::dm_signing;
        use x25519_dalek::{PublicKey, StaticSecret};

        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let make_log = |kp: Option<frost_ristretto255::keys::KeyPackage>| {
            committee_log_from_material(&members, &ids, &pub_pkg, kp)
        };
        let alice = members[0];
        let bob = members[1];
        let carol = members[2];
        // Alice lost her share (restart shape); bob + carol still hold
        // theirs.
        let mut log_a = make_log(None);
        let mut log_b = make_log(Some(key_packages.get(&ids[1]).unwrap().clone()));
        let mut log_c = make_log(Some(key_packages.get(&ids[2]).unwrap().clone()));

        // Per-member X25519 identities for the sealed rounds.
        let privs: BTreeMap<OwnerAddr, [u8; 32]> = [
            (alice, [0x41u8; 32]),
            (bob, [0x42u8; 32]),
            (carol, [0x43u8; 32]),
        ]
        .into();
        let pub_of = |addr: &OwnerAddr| -> [u8; 32] {
            *PublicKey::from(&StaticSecret::from(*privs.get(addr).unwrap())).as_bytes()
        };

        let ceremony = [0xabu8; 32];
        let helpers = vec![bob, carol];
        let helper_ids = [ids[1], ids[2]];

        // rn=1: alice requests; everyone applies.
        let rn1 = rp_event(alice, ceremony, 1, 1, Some(helpers.clone()), None, 9_000);
        for (log, me) in [(&mut log_a, alice), (&mut log_b, bob), (&mut log_c, carol)] {
            log.apply_with_identity(rn1.clone(), &me, privs.get(&me).unwrap())
                .expect("rn=1 applies");
        }

        // rn=2: each helper deals deltas over the DECLARED set (self
        // included), sealed per helper.
        let mut wall = 9_001u64;
        for (helper, helper_id) in [(bob, ids[1]), (carol, ids[2])] {
            let kp = key_packages.get(&helper_id).unwrap();
            let deltas = dc::repair_part1_local(&helper_ids, kp, ids[0]).expect("part1");
            let mut rc = Vec::new();
            for (id, delta) in &deltas {
                let addr = if *id == ids[1] { bob } else { carol };
                rc.push(RecipientCiphertext {
                    recipient: addr,
                    sealed: dm_signing::seal_to_owner(&pub_of(&addr), delta).unwrap(),
                });
            }
            let ev = rp_event(helper, ceremony, 2, 1, None, Some(rc), wall);
            wall += 1;
            for (log, me) in [(&mut log_a, alice), (&mut log_b, bob), (&mut log_c, carol)] {
                log.apply_with_identity(ev.clone(), &me, privs.get(&me).unwrap())
                    .expect("rn=2 applies");
            }
        }
        assert_eq!(
            log_b
                .committee_state
                .pending_repair
                .as_ref()
                .unwrap()
                .deltas
                .len(),
            2,
            "each helper decrypted a delta from BOTH helpers (own included)"
        );

        // rn=3: each helper sums its deltas into a sigma sealed to alice.
        for helper in [bob, carol] {
            let sigma = {
                let log = if helper == bob { &log_b } else { &log_c };
                let deltas: Vec<Vec<u8>> = log
                    .committee_state
                    .pending_repair
                    .as_ref()
                    .unwrap()
                    .deltas
                    .values()
                    .cloned()
                    .collect();
                dc::repair_part2_local(&deltas).expect("part2")
            };
            let rc = vec![RecipientCiphertext {
                recipient: alice,
                sealed: dm_signing::seal_to_owner(&pub_of(&alice), &sigma).unwrap(),
            }];
            let ev = rp_event(helper, ceremony, 3, 1, None, Some(rc), wall);
            wall += 1;
            for (log, me) in [(&mut log_a, alice), (&mut log_b, bob), (&mut log_c, carol)] {
                log.apply_with_identity(ev.clone(), &me, privs.get(&me).unwrap())
                    .expect("rn=3 applies");
            }
        }

        // Alice: share reconstructed, verified, installed; slot cleared.
        let repaired = log_a
            .local_key_package
            .as_ref()
            .expect("inline finalize must install the repaired share");
        assert_eq!(
            repaired.signing_share().serialize(),
            key_packages
                .get(&ids[0])
                .unwrap()
                .signing_share()
                .serialize(),
            "reconstruction must yield alice's EXACT original signing share"
        );
        assert!(log_a.local_pub_key_package.is_some());
        assert!(log_a.committee_state.pending_repair.is_none());
        // Helpers: slot self-cleared once every declared sigma was seen.
        assert!(log_b.committee_state.pending_repair.is_none());
        assert!(log_c.committee_state.pending_repair.is_none());
    }

    /// A corrupt sigma must NOT install a share: the consensus
    /// verifying-share check fails, the ceremony aborts terminally, and
    /// the participant stays shareless (free to re-request).
    #[test]
    fn repair_corrupt_sigma_aborts_without_installing_zeb1027() {
        use crate::community_membership::RecipientCiphertext;
        use crate::dm_signing;
        use x25519_dalek::{PublicKey, StaticSecret};

        let (mut log, alice, bob, carol) = repair_committee_log();
        // Give the committee a REAL-shaped vk + verifying shares so the
        // finalize path gets to the mismatch check (garbage sigmas
        // still sum to a valid scalar → valid point).
        let sk = frost_ristretto255::SigningKey::deserialize(&[7u8; 32]).unwrap();
        let vk = frost_ristretto255::VerifyingKey::from(&sk);
        log.committee_state.joint_verifying_key =
            Some(crate::community_dfrost_crypto::verifying_key_to_bytes(&vk));
        for m in [alice, bob, carol] {
            log.committee_state.verifying_shares.insert(
                m,
                crate::community_dfrost_crypto::verifying_key_to_bytes(&vk),
            );
        }

        let alice_priv = [0x41u8; 32];
        let alice_pub = *PublicKey::from(&StaticSecret::from(alice_priv)).as_bytes();
        let ceremony = [0xabu8; 32];
        log.apply_with_identity(
            rp_event(alice, ceremony, 1, 1, Some(vec![bob, carol]), None, 9_100),
            &alice,
            &alice_priv,
        )
        .expect("request applies");

        // Two "sigmas" that are canonical scalars but garbage values.
        let mut wall = 9_101u64;
        for helper in [bob, carol] {
            let mut sigma = [0u8; 32];
            sigma[0] = if helper == bob { 3 } else { 5 };
            let rc = vec![RecipientCiphertext {
                recipient: alice,
                sealed: dm_signing::seal_to_owner(&alice_pub, &sigma).unwrap(),
            }];
            log.apply_with_identity(
                rp_event(helper, ceremony, 3, 1, None, Some(rc), wall),
                &alice,
                &alice_priv,
            )
            .expect("rn=3 event itself applies (it is evidence)");
            wall += 1;
        }

        assert!(
            log.local_key_package.is_none(),
            "a mismatched reconstruction must never be installed"
        );
        assert!(
            log.committee_state.pending_repair.is_none(),
            "the failed ceremony is terminal — cleared for a fresh re-request"
        );
    }

    /// ZEB-1027 regression (the ticket's mandated test): restore an
    /// ACTIVE committee member through the REAL snapshot-restore path
    /// (`from_restored` — public state only, share gone), run the
    /// repair protocol, then prove the restored member can produce a
    /// threshold-signing contribution that aggregates into a valid VRF
    /// beacon signature under the committee's joint verifying key.
    #[test]
    fn restored_member_regains_signing_after_repair_zeb1027() {
        use crate::community_dfrost_crypto as dc;
        use crate::community_membership::RecipientCiphertext;
        use crate::dm_signing;
        use x25519_dalek::{PublicKey, StaticSecret};

        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let carol = members[2];

        // Alice's node BEFORE restart: full state including her share.
        let pre_restart = committee_log_from_material(
            &members,
            &ids,
            &pub_pkg,
            Some(key_packages[&ids[0]].clone()),
        );
        // The restart: exactly what `load_dfrost` hands back — events +
        // committee state + beacon index; every local secret gone.
        let mut log_a = DfrostLog::from_restored(
            pre_restart.export_events(),
            pre_restart.committee_state.clone(),
            pre_restart.beacon_index.clone(),
        );
        assert!(
            log_a.local_key_package.is_none(),
            "restored member starts VERIFICATION-ONLY"
        );
        assert!(log_a.committee_state.active, "public state restored");

        let mut log_b = committee_log_from_material(
            &members,
            &ids,
            &pub_pkg,
            Some(key_packages[&ids[1]].clone()),
        );
        let mut log_c = committee_log_from_material(
            &members,
            &ids,
            &pub_pkg,
            Some(key_packages[&ids[2]].clone()),
        );

        let privs: BTreeMap<OwnerAddr, [u8; 32]> = [
            (alice, [0x41u8; 32]),
            (bob, [0x42u8; 32]),
            (carol, [0x43u8; 32]),
        ]
        .into();
        let pub_of = |addr: &OwnerAddr| -> [u8; 32] {
            *PublicKey::from(&StaticSecret::from(*privs.get(addr).unwrap())).as_bytes()
        };

        // ── Repair: request + helper rounds, every event through the
        //    identity-aware apply on all three logs. ─────────────────────
        let ceremony = [0xcdu8; 32];
        let helpers = vec![bob, carol];
        let helper_ids = [ids[1], ids[2]];
        let rn1 = rp_event(alice, ceremony, 1, 1, Some(helpers.clone()), None, 9_500);
        for (log, me) in [(&mut log_a, alice), (&mut log_b, bob), (&mut log_c, carol)] {
            log.apply_with_identity(rn1.clone(), &me, privs.get(&me).unwrap())
                .expect("rn=1 applies");
        }
        let mut wall = 9_501u64;
        for (helper, helper_id) in [(bob, ids[1]), (carol, ids[2])] {
            let deltas =
                dc::repair_part1_local(&helper_ids, &key_packages[&helper_id], ids[0]).unwrap();
            let mut rc = Vec::new();
            for (id, delta) in &deltas {
                let addr = if *id == ids[1] { bob } else { carol };
                rc.push(RecipientCiphertext {
                    recipient: addr,
                    sealed: dm_signing::seal_to_owner(&pub_of(&addr), delta).unwrap(),
                });
            }
            let ev = rp_event(helper, ceremony, 2, 1, None, Some(rc), wall);
            wall += 1;
            for (log, me) in [(&mut log_a, alice), (&mut log_b, bob), (&mut log_c, carol)] {
                log.apply_with_identity(ev.clone(), &me, privs.get(&me).unwrap())
                    .expect("rn=2 applies");
            }
        }
        for helper in [bob, carol] {
            let sigma = {
                let log = if helper == bob { &log_b } else { &log_c };
                let deltas: Vec<Vec<u8>> = log
                    .committee_state
                    .pending_repair
                    .as_ref()
                    .unwrap()
                    .deltas
                    .values()
                    .cloned()
                    .collect();
                dc::repair_part2_local(&deltas).unwrap()
            };
            let ev = rp_event(
                helper,
                ceremony,
                3,
                1,
                None,
                Some(vec![RecipientCiphertext {
                    recipient: alice,
                    sealed: dm_signing::seal_to_owner(&pub_of(&alice), &sigma).unwrap(),
                }]),
                wall,
            );
            wall += 1;
            for (log, me) in [(&mut log_a, alice), (&mut log_b, bob), (&mut log_c, carol)] {
                log.apply_with_identity(ev.clone(), &me, privs.get(&me).unwrap())
                    .expect("rn=3 applies");
            }
        }

        let repaired = log_a
            .local_key_package
            .clone()
            .expect("repair reinstalls the signing share");
        let repaired_pub = log_a
            .local_pub_key_package
            .clone()
            .expect("repair reinstalls the public package");

        // ── The restored member CONTRIBUTES: 2-of-2 signing set
        //    {alice (repaired share), bob}, verified under the
        //    committee's joint vk + derived into a VRF output. ──────────
        let seed = crate::community_dfrost_types::derive_vrf_seed(&[0x11; 32], 1);
        let mut rng = frost_ristretto255::rand_core::OsRng;
        let mut nonces = BTreeMap::new();
        let mut commitments = BTreeMap::new();
        for (id, kp) in [(ids[0], &repaired), (ids[1], &key_packages[&ids[1]])] {
            let (n, c) = frost_ristretto255::round1::commit(kp.signing_share(), &mut rng);
            nonces.insert(id, n);
            commitments.insert(id, c);
        }
        let signing_package = frost_ristretto255::SigningPackage::new(commitments, &seed);
        let mut shares = BTreeMap::new();
        for (id, kp) in [(ids[0], &repaired), (ids[1], &key_packages[&ids[1]])] {
            shares.insert(
                id,
                frost_ristretto255::round2::sign(&signing_package, nonces.get(&id).unwrap(), kp)
                    .expect("the RESTORED member's share signs"),
            );
        }
        let sig = frost_ristretto255::aggregate(&signing_package, &shares, &repaired_pub)
            .expect("aggregate with the repaired share");
        let sig_bytes = sig.serialize().expect("sig serialize");
        dc::verify_schnorr_signature(
            &log_a.committee_state.joint_verifying_key.unwrap(),
            &seed,
            &sig_bytes,
        )
        .expect("beacon signature verifies under the committee's joint vk");
        // And the VRF output derives — the full beacon shape.
        let r: [u8; 32] = sig_bytes[..32].try_into().unwrap();
        let _vrf_output = crate::community_dfrost_types::derive_vrf_output(&r);
    }

    /// Refresh promotion (`dk` quorum on `pending_refresh`) clears the
    /// round secrets AND any in-flight repair — epoch moved, both are
    /// dead transcript material (ZEB-1027).
    #[test]
    fn refresh_promotion_clears_secrets_and_repair_zeb1027() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};
        use crate::owner_state_types::Hlc;

        let (mut log, alice, bob, carol) = repair_committee_log();
        log.committee_state.joint_verifying_key = Some([0x44; 32]);
        let ceremony = [0x99u8; 32];
        log.committee_state.pending_refresh = Some(PendingCeremony {
            ceremony_id: ceremony,
            members: vec![alice, bob, carol],
            threshold: 2,
            max_signers: 3,
            proposed_epoch: 2,
            ..Default::default()
        });
        log.committee_state.pending_repair = Some(PendingRepair::new(
            [0x55; 32],
            alice,
            1,
            vec![bob, carol],
            1_000,
            0,
        ));
        let (r1_secret, _) =
            crate::community_dfrost_crypto::dkg_part1_local(identifier_1(), 3, 2).unwrap();
        log.local_dkg_secret = Some(r1_secret);
        // CR-2 (#775 round 1): a held share whose verifying share does
        // not match the PROMOTED consensus (this node missed the
        // finalization) must be invalidated at promotion.
        log.local_key_package = Some(dealer_key_package_for_tests());

        let payload = DkgCompletePayload {
            ceremony_id: ceremony,
            joint_verifying_key: [0x44; 32],
            verifying_shares: vec![
                MemberVerifyingShare {
                    member: alice,
                    verifying_share: [0xa1; 32],
                },
                MemberVerifyingShare {
                    member: bob,
                    verifying_share: [0xb1; 32],
                },
                MemberVerifyingShare {
                    member: carol,
                    verifying_share: [0xc1; 32],
                },
            ],
            epoch: 2,
            members: vec![alice, bob, carol],
            threshold: 2,
            max_signers: 3,
            space_id: None,
        };
        let mut wall = 9_200u64;
        for confirmer in [alice, bob] {
            let mut pd = Vec::new();
            ciborium::into_writer(&payload, &mut pd).unwrap();
            log.apply(SignedCommitteeEvent {
                tag: 'd',
                version: 1,
                committee_tier: 0,
                kind: DfrostEventKind::DkgComplete,
                hlc: Hlc {
                    wall_ms: wall,
                    logical: 0,
                    device_id: "t".into(),
                },
                actor: confirmer,
                payload: pd,
                sig: vec![0u8; 64],
            })
            .expect("dk applies");
            wall += 1;
        }

        assert_eq!(log.committee_state.current_epoch, 2, "promoted");
        assert!(log.committee_state.pending_refresh.is_none());
        assert!(
            log.committee_state.pending_repair.is_none(),
            "promotion voids the in-flight repair (epoch moved)"
        );
        assert!(log.local_dkg_secret.is_none() && log.local_dkg_secret2.is_none());
        assert!(
            log.local_key_package.is_none() && log.local_pub_key_package.is_none(),
            "CR-2: a share not matching the promoted consensus is stale and must be \
             invalidated (repair is the recovery path)"
        );
    }

    /// CR-2 (#775 round 1), the KEEP side: an installed share whose
    /// verifying share matches the promoted consensus must survive
    /// promotion. (Since #775 round 2 the finalizer's rotated share
    /// arrives via `pending_rotated` — see
    /// `refresh_promotion_installs_staged_rotated_share_zeb1027` — but
    /// the staleness check must still keep any installed share that
    /// matches.)
    #[test]
    fn promotion_keeps_matching_share_zeb1027() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};
        use crate::owner_state_types::Hlc;

        let (mut log, alice, bob, carol) = repair_committee_log();
        log.committee_state.joint_verifying_key = Some([0x44; 32]);
        let ceremony = [0x9au8; 32];
        log.committee_state.pending_refresh = Some(PendingCeremony {
            ceremony_id: ceremony,
            members: vec![alice, bob, carol],
            threshold: 2,
            max_signers: 3,
            proposed_epoch: 2,
            ..Default::default()
        });
        // Dealer share with identifier 1 == alice's committee identifier.
        let kp = dealer_key_package_for_tests();
        assert_eq!(kp.identifier(), &identifier_1());
        let kp_share_bytes =
            crate::community_dfrost_crypto::verifying_share_to_bytes(kp.verifying_share());
        log.local_key_package = Some(kp);

        let payload = DkgCompletePayload {
            ceremony_id: ceremony,
            joint_verifying_key: [0x44; 32],
            verifying_shares: vec![
                MemberVerifyingShare {
                    member: alice,
                    verifying_share: kp_share_bytes,
                },
                MemberVerifyingShare {
                    member: bob,
                    verifying_share: [0xb2; 32],
                },
                MemberVerifyingShare {
                    member: carol,
                    verifying_share: [0xc2; 32],
                },
            ],
            epoch: 2,
            members: vec![alice, bob, carol],
            threshold: 2,
            max_signers: 3,
            space_id: None,
        };
        let mut wall = 9_300u64;
        for confirmer in [alice, bob] {
            let mut pd = Vec::new();
            ciborium::into_writer(&payload, &mut pd).unwrap();
            log.apply(SignedCommitteeEvent {
                tag: 'd',
                version: 1,
                committee_tier: 0,
                kind: DfrostEventKind::DkgComplete,
                hlc: Hlc {
                    wall_ms: wall,
                    logical: 0,
                    device_id: "t".into(),
                },
                actor: confirmer,
                payload: pd,
                sig: vec![0u8; 64],
            })
            .expect("dk applies");
            wall += 1;
        }
        assert_eq!(log.committee_state.current_epoch, 2, "promoted");
        assert!(
            log.local_key_package.is_some(),
            "a share matching the promoted consensus must survive promotion"
        );
    }

    fn identifier_1() -> frost_ristretto255::Identifier {
        crate::community_dfrost_crypto::identifier_for_index(0)
    }

    /// Any real dealer-generated `KeyPackage` (identifier 1).
    fn dealer_key_package_for_tests() -> frost_ristretto255::keys::KeyPackage {
        dealer_key_material_for_tests().0
    }

    /// Real dealer-generated (KeyPackage for identifier 1, joint
    /// PublicKeyPackage) — for tests that stage `pending_rotated`.
    fn dealer_key_material_for_tests() -> (
        frost_ristretto255::keys::KeyPackage,
        frost_ristretto255::keys::PublicKeyPackage,
    ) {
        let (shares, pkp) = frost_ristretto255::keys::generate_with_dealer(
            3,
            2,
            frost_ristretto255::keys::IdentifierList::Default,
            frost_ristretto255::rand_core::OsRng,
        )
        .expect("dealer keygen");
        let kp =
            frost_ristretto255::keys::KeyPackage::try_from(shares.values().next().unwrap().clone())
                .expect("key package");
        (kp, pkp)
    }

    /// Qodo #8 (#775 round 2): refresh finalization STAGES the rotated
    /// key material; promotion installs it iff it matches the promoted
    /// consensus. A stage that does not match (promotion belongs to a
    /// different ceremony) is discarded — and the stale active share
    /// with it.
    #[test]
    fn refresh_promotion_installs_staged_rotated_share_zeb1027() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};
        use crate::owner_state_types::Hlc;

        let promote = |log: &mut DfrostLog,
                       members: &[OwnerAddr; 3],
                       ceremony: [u8; 32],
                       alice_share: [u8; 32],
                       wall0: u64| {
            let payload = DkgCompletePayload {
                ceremony_id: ceremony,
                joint_verifying_key: [0x44; 32],
                verifying_shares: vec![
                    MemberVerifyingShare {
                        member: members[0],
                        verifying_share: alice_share,
                    },
                    MemberVerifyingShare {
                        member: members[1],
                        verifying_share: [0xb3; 32],
                    },
                    MemberVerifyingShare {
                        member: members[2],
                        verifying_share: [0xc3; 32],
                    },
                ],
                epoch: 2,
                members: members.to_vec(),
                threshold: 2,
                max_signers: 3,
                space_id: None,
            };
            let mut wall = wall0;
            for confirmer in [members[0], members[1]] {
                let mut pd = Vec::new();
                ciborium::into_writer(&payload, &mut pd).unwrap();
                log.apply(SignedCommitteeEvent {
                    tag: 'd',
                    version: 1,
                    committee_tier: 0,
                    kind: DfrostEventKind::DkgComplete,
                    hlc: Hlc {
                        wall_ms: wall,
                        logical: 0,
                        device_id: "t".into(),
                    },
                    actor: confirmer,
                    payload: pd,
                    sig: vec![0u8; 64],
                })
                .expect("dk applies");
                wall += 1;
            }
        };

        // INSTALL side: the staged package's verifying share appears in
        // the promoted consensus → promotion installs it, replacing the
        // (now old-epoch) active share atomically.
        let (mut log, alice, bob, carol) = repair_committee_log();
        log.committee_state.joint_verifying_key = Some([0x44; 32]);
        let ceremony = [0x9bu8; 32];
        log.committee_state.pending_refresh = Some(PendingCeremony {
            ceremony_id: ceremony,
            members: vec![alice, bob, carol],
            threshold: 2,
            max_signers: 3,
            proposed_epoch: 2,
            ..Default::default()
        });
        let (rotated_kp, rotated_pkp) = dealer_key_material_for_tests();
        assert_eq!(rotated_kp.identifier(), &identifier_1());
        let rotated_share_bytes =
            crate::community_dfrost_crypto::verifying_share_to_bytes(rotated_kp.verifying_share());
        // The pre-refresh (old-epoch) share stays installed through the
        // dk-quorum window…
        log.local_key_package = Some(dealer_key_package_for_tests());
        log.pending_rotated = Some((rotated_kp, rotated_pkp));

        promote(
            &mut log,
            &[alice, bob, carol],
            ceremony,
            rotated_share_bytes,
            9_400,
        );
        assert_eq!(log.committee_state.current_epoch, 2, "promoted");
        assert!(log.pending_rotated.is_none(), "stage consumed");
        let installed = log.local_key_package.as_ref().expect("installed");
        assert_eq!(
            crate::community_dfrost_crypto::verifying_share_to_bytes(installed.verifying_share()),
            rotated_share_bytes,
            "…and promotion swaps in the STAGED rotated share"
        );
        assert!(log.local_pub_key_package.is_some());

        // DISCARD side: a stage that does not match the promoted
        // consensus is dropped, and the stale active share is
        // invalidated by the staleness check right after.
        let (mut log, alice, bob, carol) = repair_committee_log();
        log.committee_state.joint_verifying_key = Some([0x44; 32]);
        let ceremony = [0x9cu8; 32];
        log.committee_state.pending_refresh = Some(PendingCeremony {
            ceremony_id: ceremony,
            members: vec![alice, bob, carol],
            threshold: 2,
            max_signers: 3,
            proposed_epoch: 2,
            ..Default::default()
        });
        log.local_key_package = Some(dealer_key_package_for_tests());
        log.pending_rotated = Some(dealer_key_material_for_tests());

        promote(&mut log, &[alice, bob, carol], ceremony, [0xa9; 32], 9_500);
        assert_eq!(log.committee_state.current_epoch, 2, "promoted");
        assert!(log.pending_rotated.is_none(), "mismatched stage discarded");
        assert!(
            log.local_key_package.is_none() && log.local_pub_key_package.is_none(),
            "neither the mismatched stage nor the stale share may survive"
        );
    }

    // ── ZEB-1030: DfrostLog evidence-based adopt entry points ─────────

    /// ZEB-1030 helper: build a `dk` (DkgComplete) event with a fake
    /// envelope signature (the adopt paths validate the FROST-consensus
    /// shape of the payload itself; the outer Ed25519 sig is the
    /// engine's job — same convention as `di_event`).
    fn signed_dk(
        actor: OwnerAddr,
        wall: u64,
        dev: &str,
        payload: &crate::community_dfrost_types::DkgCompletePayload,
    ) -> crate::community_dfrost_types::SignedCommitteeEvent {
        use crate::owner_state_types::Hlc;
        let mut pd = Vec::new();
        ciborium::into_writer(payload, &mut pd).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: Hlc {
                wall_ms: wall,
                logical: 0,
                device_id: dev.into(),
            },
            actor,
            payload: pd,
            sig: vec![0u8; 64],
        }
    }

    #[test]
    fn adopt_refresh_quorum_happy_path_zeb1030() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};

        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let alice_kp = key_packages.get(&ids[0]).unwrap().clone();

        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, Some(alice_kp));
        // Seed local_pub_key_package too, so the is_none() assertion
        // below actually exercises the drop rather than being vacuously
        // true because it started as None.
        log.local_pub_key_package = Some(pub_pkg.clone());
        // Pre-seed a pending_sign session to assert adoption clears it.
        log.committee_state.pending_sign.insert(
            [0x77; 32],
            PendingSignSession {
                message_hash: [0x88; 32],
                contributions: BTreeMap::new(),
                local_nonces: None,
                purpose: SignPurpose::default(),
            },
        );

        let held_vk = log.committee_state.joint_verifying_key.unwrap();
        let new_shares: Vec<MemberVerifyingShare> = members
            .iter()
            .enumerate()
            .map(|(i, m)| MemberVerifyingShare {
                member: *m,
                verifying_share: [0x41 + i as u8; 32],
            })
            .collect();

        let payload = DkgCompletePayload {
            ceremony_id: [0x66; 32],
            joint_verifying_key: held_vk,
            verifying_shares: new_shares.clone(),
            epoch: 2,
            members: members.clone(),
            threshold: 2,
            max_signers: 3,
            space_id: None,
        };

        let events = vec![
            signed_dk(alice, 10_000, "a", &payload),
            signed_dk(bob, 10_001, "b", &payload),
        ];

        assert_eq!(log.adopt_refresh_quorum(&events, &zeb1034_space()), Ok(2));
        assert_eq!(log.committee_state.current_epoch, 2);
        let expected: BTreeMap<OwnerAddr, [u8; 32]> = new_shares
            .into_iter()
            .map(|mvs| (mvs.member, mvs.verifying_share))
            .collect();
        assert_eq!(log.committee_state.verifying_shares, expected);
        assert!(
            log.local_key_package.is_none(),
            "stale share dropped after refresh"
        );
        assert!(log.local_pub_key_package.is_none());
        assert!(log.committee_state.pending_sign.is_empty());
        assert_eq!(log.event_count(), 2, "both dk events retained");
        assert_eq!(log.committee_state.members, members);
        assert_eq!(log.committee_state.threshold, 2);
        assert_eq!(log.committee_state.max_signers, 3);
    }

    /// ZEB-1030: `pending_rotated` staged material is installed on
    /// adoption iff it matches the ADOPTED consensus for its identifier
    /// (mirroring `apply_dkg_complete`'s promotion gate, exercised live
    /// in `refresh_promotion_installs_staged_rotated_share_zeb1027`) —
    /// the node that finalized its own live refresh part3 and then
    /// partitioned before the quorum landed holds exactly matching
    /// material, and installing it here skips a needless repair round.
    #[test]
    fn adopt_refresh_quorum_stages_and_gates_pending_rotated_zeb1030() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};

        let (members, ids, _key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let carol = members[2];
        let held_vk =
            crate::community_dfrost_crypto::verifying_key_to_bytes(pub_pkg.verifying_key());

        // MATCH side: the staged package's verifying share appears in
        // the ADOPTED consensus for alice's identifier → adoption
        // installs it.
        {
            let (rotated_kp, rotated_pkp) = dealer_key_material_for_tests();
            let rotated_share_bytes = crate::community_dfrost_crypto::verifying_share_to_bytes(
                rotated_kp.verifying_share(),
            );

            let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
            log.pending_rotated = Some((rotated_kp, rotated_pkp));

            let payload = DkgCompletePayload {
                ceremony_id: [0x71; 32],
                joint_verifying_key: held_vk,
                verifying_shares: vec![
                    MemberVerifyingShare {
                        member: alice,
                        verifying_share: rotated_share_bytes,
                    },
                    MemberVerifyingShare {
                        member: bob,
                        verifying_share: [0x52; 32],
                    },
                    MemberVerifyingShare {
                        member: carol,
                        verifying_share: [0x53; 32],
                    },
                ],
                epoch: 2,
                members: members.clone(),
                threshold: 2,
                max_signers: 3,
                space_id: None,
            };
            let events = vec![
                signed_dk(alice, 40_000, "a", &payload),
                signed_dk(bob, 40_001, "b", &payload),
            ];
            assert_eq!(log.adopt_refresh_quorum(&events, &zeb1034_space()), Ok(2));
            let installed = log
                .local_key_package
                .as_ref()
                .expect("staged material installed");
            assert_eq!(
                crate::community_dfrost_crypto::verifying_share_to_bytes(
                    installed.verifying_share()
                ),
                rotated_share_bytes
            );
            assert!(log.local_pub_key_package.is_some());
            assert!(log.pending_rotated.is_none(), "stage consumed");
        }

        // MISMATCH side: the staged package does NOT match the adopted
        // consensus for its identifier (an unrelated dealer run) →
        // discarded — no phantom share blocking ZEB-1027 auto-repair.
        {
            let (rotated_kp, rotated_pkp) = dealer_key_material_for_tests();
            let mismatched_bytes = crate::community_dfrost_crypto::verifying_share_to_bytes(
                rotated_kp.verifying_share(),
            );
            assert_ne!(
                mismatched_bytes, [0x61u8; 32],
                "sanity: dealer share not accidentally equal to the adopted fixture value"
            );

            let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
            log.pending_rotated = Some((rotated_kp, rotated_pkp));

            let payload = DkgCompletePayload {
                ceremony_id: [0x72; 32],
                joint_verifying_key: held_vk,
                verifying_shares: vec![
                    MemberVerifyingShare {
                        member: alice,
                        verifying_share: [0x61; 32],
                    },
                    MemberVerifyingShare {
                        member: bob,
                        verifying_share: [0x62; 32],
                    },
                    MemberVerifyingShare {
                        member: carol,
                        verifying_share: [0x63; 32],
                    },
                ],
                epoch: 2,
                members: members.clone(),
                threshold: 2,
                max_signers: 3,
                space_id: None,
            };
            let events = vec![
                signed_dk(alice, 41_000, "a", &payload),
                signed_dk(bob, 41_001, "b", &payload),
            ];
            assert_eq!(log.adopt_refresh_quorum(&events, &zeb1034_space()), Ok(2));
            assert!(
                log.local_key_package.is_none(),
                "mismatched stage discarded"
            );
            assert!(log.local_pub_key_package.is_none());
            assert!(log.pending_rotated.is_none(), "stage consumed either way");
        }
    }

    #[test]
    fn adopt_refresh_quorum_reject_matrix_zeb1030() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};

        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let non_member = OwnerAddr([0x99; 16]);
        let alice_kp = key_packages.get(&ids[0]).unwrap().clone();
        let held_vk =
            crate::community_dfrost_crypto::verifying_key_to_bytes(pub_pkg.verifying_key());

        let good_shares: Vec<MemberVerifyingShare> = members
            .iter()
            .enumerate()
            .map(|(i, m)| MemberVerifyingShare {
                member: *m,
                verifying_share: [0x41 + i as u8; 32],
            })
            .collect();
        let good_payload = || DkgCompletePayload {
            ceremony_id: [0x66; 32],
            joint_verifying_key: held_vk,
            verifying_shares: good_shares.clone(),
            epoch: 2,
            members: members.clone(),
            threshold: 2,
            max_signers: 3,
            space_id: None,
        };

        let mut cases: Vec<(&str, Vec<SignedCommitteeEvent>)> = Vec::new();
        {
            let p = good_payload();
            cases.push(("sub_threshold", vec![signed_dk(alice, 20_000, "a", &p)]));
        }
        {
            // Two dk events from the SAME actor at different HLCs must
            // NOT inflate the distinct-actor tally — quorum counts
            // distinct confirming actors, not raw event count.
            let p = good_payload();
            cases.push((
                "duplicate_actor_no_quorum",
                vec![
                    signed_dk(alice, 20_000, "a", &p),
                    signed_dk(alice, 20_002, "a2", &p),
                ],
            ));
        }
        {
            let p = good_payload();
            cases.push((
                "non_member_actor",
                vec![
                    signed_dk(alice, 20_000, "a", &p),
                    signed_dk(non_member, 20_001, "x", &p),
                ],
            ));
        }
        {
            let mut p = good_payload();
            p.joint_verifying_key = [0xde; 32];
            cases.push((
                "vk_mismatch",
                vec![
                    signed_dk(alice, 20_000, "a", &p),
                    signed_dk(bob, 20_001, "b", &p),
                ],
            ));
        }
        {
            let mut p = good_payload();
            p.epoch = 1; // == held current_epoch, not >
            cases.push((
                "epoch_not_greater",
                vec![
                    signed_dk(alice, 20_000, "a", &p),
                    signed_dk(bob, 20_001, "b", &p),
                ],
            ));
        }
        {
            let mut p = good_payload();
            p.epoch = 0;
            cases.push((
                "epoch_zero",
                vec![
                    signed_dk(alice, 20_000, "a", &p),
                    signed_dk(bob, 20_001, "b", &p),
                ],
            ));
        }
        {
            let mut p = good_payload();
            p.members = vec![alice, bob]; // held members has 3
            cases.push((
                "members_differ",
                vec![
                    signed_dk(alice, 20_000, "a", &p),
                    signed_dk(bob, 20_001, "b", &p),
                ],
            ));
        }
        {
            let p1 = good_payload();
            let mut p2 = good_payload();
            p2.verifying_shares[0].verifying_share = [0x99; 32];
            cases.push((
                "disagreeing_shares",
                vec![
                    signed_dk(alice, 20_000, "a", &p1),
                    signed_dk(bob, 20_001, "b", &p2),
                ],
            ));
        }
        {
            let mut p = good_payload();
            let dup = p.verifying_shares[0].clone();
            p.verifying_shares.push(dup);
            cases.push((
                "duplicate_share_entry",
                vec![
                    signed_dk(alice, 20_000, "a", &p),
                    signed_dk(bob, 20_001, "b", &p),
                ],
            ));
        }
        {
            let mut p = good_payload();
            p.verifying_shares.pop();
            cases.push((
                "missing_member_in_shares",
                vec![
                    signed_dk(alice, 20_000, "a", &p),
                    signed_dk(bob, 20_001, "b", &p),
                ],
            ));
        }
        {
            let p = good_payload();
            let dk = signed_dk(alice, 20_000, "a", &p);
            let mut vb = dk.clone();
            vb.kind = DfrostEventKind::VrfBeacon;
            cases.push(("wrong_kind", vec![dk, vb]));
        }

        for (name, events) in &cases {
            let mut log =
                committee_log_from_material(&members, &ids, &pub_pkg, Some(alice_kp.clone()));
            let original_shares = log.committee_state.verifying_shares.clone();
            let result = log.adopt_refresh_quorum(events, &zeb1034_space());
            assert!(result.is_err(), "case {name} should reject: {result:?}");
            assert_eq!(
                log.committee_state.current_epoch, 1,
                "case {name}: epoch unchanged"
            );
            assert_eq!(
                log.committee_state.verifying_shares, original_shares,
                "case {name}: verifying_shares unchanged"
            );
            assert!(log.local_key_package.is_some(), "case {name}: kp untouched");
            assert_eq!(log.event_count(), 0, "case {name}: no partial insert");
        }

        // Inactive log — no held committee at all.
        let mut inactive = DfrostLog::new();
        let p = good_payload();
        let events = vec![
            signed_dk(alice, 20_000, "a", &p),
            signed_dk(bob, 20_001, "b", &p),
        ];
        assert!(inactive
            .adopt_refresh_quorum(&events, &zeb1034_space())
            .is_err());
        assert_eq!(inactive.event_count(), 0);
    }

    #[test]
    fn adopt_initial_quorum_happy_path_zeb1030() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};

        let (members, ids, _key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let joint_vk =
            crate::community_dfrost_crypto::verifying_key_to_bytes(pub_pkg.verifying_key());
        let verifying_shares: Vec<MemberVerifyingShare> = members
            .iter()
            .enumerate()
            .map(|(i, m)| MemberVerifyingShare {
                member: *m,
                verifying_share: crate::community_dfrost_crypto::verifying_share_to_bytes(
                    pub_pkg.verifying_shares().get(&ids[i]).unwrap(),
                ),
            })
            .collect();

        let payload = DkgCompletePayload {
            ceremony_id: [0x21; 32],
            joint_verifying_key: joint_vk,
            verifying_shares: verifying_shares.clone(),
            epoch: 1,
            members: members.clone(),
            threshold: 2,
            max_signers: 3,
            space_id: Some(zeb1034_space()),
        };

        let ev_a = signed_dk(alice, 1_000, "a", &payload);
        let ev_b = signed_dk(bob, 1_001, "b", &payload);
        let events = vec![ev_a.clone(), ev_b];

        let mut log = DfrostLog::new();
        assert_eq!(
            log.adopt_initial_quorum(
                &events,
                &zeb1034_space(),
                &std::collections::BTreeSet::new()
            ),
            Ok(1)
        );
        assert!(log.committee_state.active);
        assert_eq!(log.committee_state.joint_verifying_key, Some(joint_vk));
        let expected_shares: BTreeMap<OwnerAddr, [u8; 32]> = verifying_shares
            .into_iter()
            .map(|mvs| (mvs.member, mvs.verifying_share))
            .collect();
        assert_eq!(log.committee_state.verifying_shares, expected_shares);
        assert_eq!(log.committee_state.members, members);
        assert_eq!(log.committee_state.threshold, 2);
        assert_eq!(log.committee_state.max_signers, 3);
        assert_eq!(
            log.committee_state.identifier_map,
            CommitteeState::build_identifier_map(&members)
        );
        assert!(
            log.local_key_package.is_none(),
            "a joiner has no local signing share"
        );
        assert_eq!(log.event_count(), 2);

        // vk-immutability pin: a LIVE dk claiming a different vk still
        // fails (apply_dkg_complete's active-vk check / no pending slot).
        let mut bad_payload = payload.clone();
        bad_payload.joint_verifying_key = [0xed; 32];
        bad_payload.ceremony_id = [0x22; 32];
        let bad_ev = signed_dk(alice, 2_000, "a", &bad_payload);
        let vk_before = log.committee_state.joint_verifying_key;
        assert!(log.apply(bad_ev).is_err());
        assert_eq!(log.committee_state.joint_verifying_key, vk_before);

        // A duplicate of an adopted event is a structural apply no-op
        // (ZEB-753 dedup).
        let before_count = log.event_count();
        assert_eq!(log.apply(ev_a), Ok(()));
        assert_eq!(log.event_count(), before_count);
    }

    /// ZEB-1034: the fresh-joiner path REQUIRES a community binding
    /// matching the adopting community — the dual-community-responder
    /// replay (community X's genuine quorum served into community Y)
    /// and legacy unbound evidence are both rejected, with no state
    /// written.
    #[test]
    fn adopt_initial_quorum_community_binding_zeb1034() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};

        let (members, ids, _key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let joint_vk =
            crate::community_dfrost_crypto::verifying_key_to_bytes(pub_pkg.verifying_key());
        let verifying_shares: Vec<MemberVerifyingShare> = members
            .iter()
            .enumerate()
            .map(|(i, m)| MemberVerifyingShare {
                member: *m,
                verifying_share: crate::community_dfrost_crypto::verifying_share_to_bytes(
                    pub_pkg.verifying_shares().get(&ids[i]).unwrap(),
                ),
            })
            .collect();
        let payload = DkgCompletePayload {
            ceremony_id: [0x34; 32],
            joint_verifying_key: joint_vk,
            verifying_shares,
            epoch: 1,
            members: members.clone(),
            threshold: 2,
            max_signers: 3,
            space_id: Some(zeb1034_space()),
        };

        // Community X's GENUINE quorum (validly built, bound to X =
        // zeb1034_space) replayed at community Y → rejected, nothing
        // adopted.
        let events = vec![
            signed_dk(alice, 1_000, "a", &payload),
            signed_dk(bob, 1_001, "b", &payload),
        ];
        let community_y = crate::owner_state_types::SpaceId([0x0E; 16]);
        let mut log = DfrostLog::new();
        let err = log
            .adopt_initial_quorum(&events, &community_y, &std::collections::BTreeSet::new())
            .expect_err("cross-community dk quorum must be rejected");
        assert!(
            err.contains("bound to a different community"),
            "unexpected error: {err}"
        );
        assert!(!log.committee_state.active);
        assert_eq!(log.event_count(), 0);

        // Legacy unbound (pre-ZEB-1034, space_id absent) evidence →
        // rejected on the joiner path.
        let mut legacy_payload = payload.clone();
        legacy_payload.space_id = None;
        let legacy_events = vec![
            signed_dk(alice, 1_000, "a", &legacy_payload),
            signed_dk(bob, 1_001, "b", &legacy_payload),
        ];
        let err = log
            .adopt_initial_quorum(
                &legacy_events,
                &zeb1034_space(),
                &std::collections::BTreeSet::new(),
            )
            .expect_err("unbound legacy dk quorum must be rejected");
        assert!(
            err.contains("no community binding"),
            "unexpected error: {err}"
        );
        assert!(!log.committee_state.active);

        // Correctly-bound evidence at the right community still adopts.
        assert_eq!(
            log.adopt_initial_quorum(
                &events,
                &zeb1034_space(),
                &std::collections::BTreeSet::new()
            ),
            Ok(1)
        );
        assert!(log.committee_state.active);
    }

    /// ZEB-1034: the straggler path tolerates legacy unbound evidence
    /// (held-vk pinning already blocks cross-community replay there)
    /// but still rejects an EXPLICIT foreign binding.
    #[test]
    fn adopt_refresh_quorum_community_binding_zeb1034() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};

        let (members, ids, _key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let joint_vk =
            crate::community_dfrost_crypto::verifying_key_to_bytes(pub_pkg.verifying_key());
        let verifying_shares: Vec<MemberVerifyingShare> = members
            .iter()
            .enumerate()
            .map(|(i, m)| MemberVerifyingShare {
                member: *m,
                verifying_share: crate::community_dfrost_crypto::verifying_share_to_bytes(
                    pub_pkg.verifying_shares().get(&ids[i]).unwrap(),
                ),
            })
            .collect();

        // Activate a committee at epoch 1 via the (strict) joiner path.
        let initial = DkgCompletePayload {
            ceremony_id: [0x35; 32],
            joint_verifying_key: joint_vk,
            verifying_shares: verifying_shares.clone(),
            epoch: 1,
            members: members.clone(),
            threshold: 2,
            max_signers: 3,
            space_id: Some(zeb1034_space()),
        };
        let initial_events = vec![
            signed_dk(alice, 1_000, "a", &initial),
            signed_dk(bob, 1_001, "b", &initial),
        ];
        let mut log = DfrostLog::new();
        assert_eq!(
            log.adopt_initial_quorum(
                &initial_events,
                &zeb1034_space(),
                &std::collections::BTreeSet::new()
            ),
            Ok(1)
        );

        // Epoch-2 refresh evidence bound to a DIFFERENT community →
        // rejected even though the vk matches.
        let mut foreign = initial.clone();
        foreign.epoch = 2;
        foreign.ceremony_id = [0x36; 32];
        foreign.space_id = Some(crate::owner_state_types::SpaceId([0x0F; 16]));
        let foreign_events = vec![
            signed_dk(alice, 2_000, "a", &foreign),
            signed_dk(bob, 2_001, "b", &foreign),
        ];
        let err = log
            .adopt_refresh_quorum(&foreign_events, &zeb1034_space())
            .expect_err("foreign-bound refresh quorum must be rejected");
        assert!(
            err.contains("bound to a different community"),
            "unexpected error: {err}"
        );
        assert_eq!(log.committee_state.current_epoch, 1);

        // Legacy unbound refresh evidence is still adoptable (vk pin
        // carries the binding on this path).
        let mut legacy = foreign.clone();
        legacy.space_id = None;
        let legacy_events = vec![
            signed_dk(alice, 2_000, "a", &legacy),
            signed_dk(bob, 2_001, "b", &legacy),
        ];
        assert_eq!(
            log.adopt_refresh_quorum(&legacy_events, &zeb1034_space()),
            Ok(2)
        );
        assert_eq!(log.committee_state.current_epoch, 2);
    }

    #[test]
    fn adopt_initial_quorum_reject_matrix_zeb1030() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};

        let (members, ids, _key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let joint_vk =
            crate::community_dfrost_crypto::verifying_key_to_bytes(pub_pkg.verifying_key());
        let good_shares: Vec<MemberVerifyingShare> = members
            .iter()
            .enumerate()
            .map(|(i, m)| MemberVerifyingShare {
                member: *m,
                verifying_share: crate::community_dfrost_crypto::verifying_share_to_bytes(
                    pub_pkg.verifying_shares().get(&ids[i]).unwrap(),
                ),
            })
            .collect();
        let good_payload = || DkgCompletePayload {
            ceremony_id: [0x21; 32],
            joint_verifying_key: joint_vk,
            verifying_shares: good_shares.clone(),
            epoch: 1,
            members: members.clone(),
            threshold: 2,
            max_signers: 3,
            // ZEB-1034 (PR#780 round-1): keep the base payload BOUND to
            // the expected community — with `None` every matrix case
            // would fail on the binding gate before reaching the
            // malformed shape it exists to test.
            space_id: Some(zeb1034_space()),
        };

        let mut cases: Vec<(&str, Vec<SignedCommitteeEvent>)> = Vec::new();
        {
            let mut p = good_payload();
            p.members = vec![members[1], members[0], members[2]]; // unsorted
            cases.push((
                "unsorted_members",
                vec![
                    signed_dk(alice, 1_000, "a", &p),
                    signed_dk(bob, 1_001, "b", &p),
                ],
            ));
        }
        {
            let mut p = good_payload();
            p.threshold = 0;
            cases.push((
                "threshold_zero",
                vec![
                    signed_dk(alice, 1_000, "a", &p),
                    signed_dk(bob, 1_001, "b", &p),
                ],
            ));
        }
        {
            // Ruling: mirrors `check_ceremony_init_admissible`'s
            // threshold >= 2 floor — a single-signer "quorum" (threshold
            // 1) must be rejected exactly as the live `di` admission
            // path rejects it.
            let mut p = good_payload();
            p.threshold = 1;
            cases.push((
                "threshold_one",
                vec![
                    signed_dk(alice, 1_000, "a", &p),
                    signed_dk(bob, 1_001, "b", &p),
                ],
            ));
        }
        {
            // Ruling: mirrors `check_ceremony_init_admissible`'s
            // members.len() >= 2 floor — a fabricated single-member
            // "committee" must be rejected exactly as the live `di`
            // admission path rejects it.
            let mut p = good_payload();
            p.members = vec![alice];
            cases.push(("single_member", vec![signed_dk(alice, 1_000, "a", &p)]));
        }
        {
            let mut p = good_payload();
            p.threshold = 4; // > max_signers (3)
            cases.push((
                "threshold_gt_max_signers",
                vec![
                    signed_dk(alice, 1_000, "a", &p),
                    signed_dk(bob, 1_001, "b", &p),
                ],
            ));
        }
        {
            let mut p = good_payload();
            p.max_signers = 5; // != members.len() (3)
            cases.push((
                "max_signers_mismatch",
                vec![
                    signed_dk(alice, 1_000, "a", &p),
                    signed_dk(bob, 1_001, "b", &p),
                ],
            ));
        }
        {
            let mut p = good_payload();
            p.epoch = 0;
            cases.push((
                "epoch_zero",
                vec![
                    signed_dk(alice, 1_000, "a", &p),
                    signed_dk(bob, 1_001, "b", &p),
                ],
            ));
        }
        {
            let p1 = good_payload();
            let mut p2 = good_payload();
            p2.epoch = 2;
            cases.push((
                "disagreeing_payloads",
                vec![
                    signed_dk(alice, 1_000, "a", &p1),
                    signed_dk(bob, 1_001, "b", &p2),
                ],
            ));
        }

        for (name, events) in &cases {
            let mut log = DfrostLog::new();
            let result = log.adopt_initial_quorum(
                events,
                &zeb1034_space(),
                &std::collections::BTreeSet::new(),
            );
            assert!(result.is_err(), "case {name} should reject: {result:?}");
            // ZEB-1034 (PR#780 round-1): each case must fail on ITS OWN
            // defect — a binding-gate error here means the base fixture
            // regressed to unbound and the matrix stopped testing what
            // it exists to test.
            let err = result.unwrap_err();
            assert!(
                !err.contains("community binding") && !err.contains("different community"),
                "case {name} must fail on its own defect, not the ZEB-1034 binding gate: {err}"
            );
            assert!(!log.committee_state.active, "case {name}: stays inactive");
            assert_eq!(log.event_count(), 0, "case {name}: no partial insert");
        }

        // Active log rejects outright.
        let p = good_payload();
        let mut active_log = DfrostLog::new();
        active_log.committee_state.active = true;
        let events = vec![
            signed_dk(alice, 1_000, "a", &p),
            signed_dk(bob, 1_001, "b", &p),
        ];
        assert!(active_log
            .adopt_initial_quorum(
                &events,
                &zeb1034_space(),
                &std::collections::BTreeSet::new()
            )
            .is_err());
        assert_eq!(active_log.event_count(), 0);
    }

    /// ZEB-1031 §6.1: `adopt_initial_quorum`'s `rejected_vks` gate —
    /// a quorum whose joint verifying key is in the rejected set is
    /// rejected with the provenance error, BEFORE any shape validation
    /// (a malformed-but-rejected quorum still gets the provenance
    /// error, not a shape error); the empty-set case is prior
    /// behaviour (adopts normally).
    #[test]
    fn adopt_initial_quorum_rejected_vks_gate_zeb1031() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};

        let (members, ids, _key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let joint_vk =
            crate::community_dfrost_crypto::verifying_key_to_bytes(pub_pkg.verifying_key());
        let verifying_shares: Vec<MemberVerifyingShare> = members
            .iter()
            .enumerate()
            .map(|(i, m)| MemberVerifyingShare {
                member: *m,
                verifying_share: crate::community_dfrost_crypto::verifying_share_to_bytes(
                    pub_pkg.verifying_shares().get(&ids[i]).unwrap(),
                ),
            })
            .collect();
        let payload = DkgCompletePayload {
            ceremony_id: [0x41; 32],
            joint_verifying_key: joint_vk,
            verifying_shares,
            epoch: 1,
            members: members.clone(),
            threshold: 2,
            max_signers: 3,
            space_id: Some(zeb1034_space()),
        };
        let events = vec![
            signed_dk(alice, 1_000, "a", &payload),
            signed_dk(bob, 1_001, "b", &payload),
        ];

        // In-set: rejected with the provenance error, nothing adopted.
        let mut rejected = std::collections::BTreeSet::new();
        rejected.insert(joint_vk);
        let mut log = DfrostLog::new();
        let err = log
            .adopt_initial_quorum(&events, &zeb1034_space(), &rejected)
            .expect_err("rejected vk must not be adopted");
        assert!(
            err.contains("rejected") && err.contains("ZEB-1031"),
            "unexpected error: {err}"
        );
        assert!(!log.committee_state.active);
        assert_eq!(log.event_count(), 0, "no partial insert");

        // Empty set: prior behaviour — adopts normally.
        let mut log2 = DfrostLog::new();
        assert_eq!(
            log2.adopt_initial_quorum(
                &events,
                &zeb1034_space(),
                &std::collections::BTreeSet::new()
            ),
            Ok(1)
        );
        assert!(log2.committee_state.active);
        assert_eq!(log2.committee_state.joint_verifying_key, Some(joint_vk));
    }

    /// ZEB-1031 §5.3/§6.2 (controller ruling): `adopt_initial_quorum`
    /// enforces the `pending_reset` pin — when a reset is pending on
    /// THIS log (a straggler that just applied its own marker), a dk
    /// quorum whose shape doesn't match the pinned `new_members`/
    /// `new_threshold` is rejected, even though it is otherwise
    /// structurally valid; the pinned-shape quorum is adopted.
    #[test]
    fn adopt_initial_quorum_pending_reset_pin_zeb1031() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};

        let (members, ids, _key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let carol = members[2];
        let joint_vk =
            crate::community_dfrost_crypto::verifying_key_to_bytes(pub_pkg.verifying_key());
        let verifying_shares: Vec<MemberVerifyingShare> = members
            .iter()
            .enumerate()
            .map(|(i, m)| MemberVerifyingShare {
                member: *m,
                verifying_share: crate::community_dfrost_crypto::verifying_share_to_bytes(
                    pub_pkg.verifying_shares().get(&ids[i]).unwrap(),
                ),
            })
            .collect();

        // Pin the successor to exactly {alice, bob}, threshold 2 (a
        // strict subset of the 2-of-3 material's 3-member committee —
        // proves the pin, not merely the pre-existing shape gates,
        // rejects the mismatched quorum below).
        let mut log = DfrostLog::new();
        log.committee_state.pending_reset = Some(PendingReset {
            reset_id: [0x50; 16],
            new_members: vec![alice, bob],
            new_threshold: 2,
        });

        // Wrong shape: claims all 3 members (structurally valid on its
        // own — passes every pre-existing adopt_initial_quorum check)
        // but does not match the pin.
        let wrong_shape_payload = DkgCompletePayload {
            ceremony_id: [0x51; 32],
            joint_verifying_key: joint_vk,
            verifying_shares: verifying_shares.clone(),
            epoch: 1,
            members: members.clone(),
            threshold: 2,
            max_signers: 3,
            space_id: Some(zeb1034_space()),
        };
        let wrong_events = vec![
            signed_dk(alice, 1_000, "a", &wrong_shape_payload),
            signed_dk(bob, 1_001, "b", &wrong_shape_payload),
            signed_dk(carol, 1_002, "c", &wrong_shape_payload),
        ];
        let err = log
            .adopt_initial_quorum(
                &wrong_events,
                &zeb1034_space(),
                &std::collections::BTreeSet::new(),
            )
            .expect_err("wrong-shape quorum must be rejected while a reset is pending");
        assert!(
            err.contains("pinned successor shape") && err.contains("ZEB-1031"),
            "unexpected error: {err}"
        );
        assert!(!log.committee_state.active);
        assert_eq!(log.event_count(), 0, "no partial insert");
        assert!(
            log.committee_state.pending_reset.is_some(),
            "pin survives the rejected attempt"
        );

        // Pinned-shape quorum ({alice, bob}, threshold 2) is adopted.
        let pinned_shares: Vec<MemberVerifyingShare> = verifying_shares
            .into_iter()
            .filter(|mvs| mvs.member == alice || mvs.member == bob)
            .collect();
        let pinned_vk = [0x60; 32];
        let pinned_payload = DkgCompletePayload {
            ceremony_id: [0x52; 32],
            joint_verifying_key: pinned_vk,
            verifying_shares: pinned_shares,
            epoch: 1,
            members: vec![alice, bob],
            threshold: 2,
            max_signers: 2,
            space_id: Some(zeb1034_space()),
        };
        let pinned_events = vec![
            signed_dk(alice, 2_000, "a", &pinned_payload),
            signed_dk(bob, 2_001, "b", &pinned_payload),
        ];
        assert_eq!(
            log.adopt_initial_quorum(
                &pinned_events,
                &zeb1034_space(),
                &std::collections::BTreeSet::new()
            ),
            Ok(1)
        );
        assert!(log.committee_state.active);
        assert_eq!(log.committee_state.joint_verifying_key, Some(pinned_vk));
        assert_eq!(log.committee_state.members, vec![alice, bob]);
    }

    #[test]
    fn adopt_beacons_self_certifying_zeb1030() {
        use crate::community_dfrost_types::{derive_vrf_output, VrfBeaconPayload};
        use crate::owner_state_types::Hlc;

        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
        // Pre-seed a pending_sign session — adopt_beacons must never
        // touch it (that is the whole reason it doesn't require one).
        log.committee_state.pending_sign.insert(
            [0xaa; 32],
            PendingSignSession {
                message_hash: [0xbb; 32],
                contributions: BTreeMap::new(),
                local_nonces: None,
                purpose: SignPurpose::default(),
            },
        );

        // Inactive log: adopt_beacons must return 0 without panicking.
        let inactive_ev_payload = crate::community_dfrost_types::VrfBeaconPayload {
            ceremony_id: [0xcc; 32],
            message_hash: [0x11; 32],
            signature: vec![0u8; 64],
            vrf_output: [0x22; 32],
        };
        let mut inactive_pd = Vec::new();
        ciborium::into_writer(&inactive_ev_payload, &mut inactive_pd).unwrap();
        let inactive_ev = SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::VrfBeacon,
            hlc: crate::owner_state_types::Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "z".into(),
            },
            actor: members[0],
            payload: inactive_pd,
            sig: vec![0u8; 64],
        };
        let mut inactive_log = DfrostLog::new();
        assert_eq!(inactive_log.adopt_beacons(&[inactive_ev], 0), 0);

        let message_hash = [0x33u8; 32];
        let mut rng = frost_ristretto255::rand_core::OsRng;
        let mut nonces = BTreeMap::new();
        let mut commitments = BTreeMap::new();
        for id in &ids[..2] {
            let kp = key_packages.get(id).unwrap();
            let (n, c) = frost_ristretto255::round1::commit(kp.signing_share(), &mut rng);
            nonces.insert(*id, n);
            commitments.insert(*id, c);
        }
        let signing_package = frost_ristretto255::SigningPackage::new(commitments, &message_hash);
        let mut shares = BTreeMap::new();
        for id in &ids[..2] {
            let kp = key_packages.get(id).unwrap();
            shares.insert(
                *id,
                frost_ristretto255::round2::sign(&signing_package, nonces.get(id).unwrap(), kp)
                    .expect("round2 sign"),
            );
        }
        let sig =
            frost_ristretto255::aggregate(&signing_package, &shares, &pub_pkg).expect("aggregate");
        let sig_bytes = sig.serialize().expect("sig serialize");
        let r: [u8; 32] = sig_bytes[..32].try_into().unwrap();
        let vrf_output = derive_vrf_output(&r);

        let build_event = |wall: u64, payload: &VrfBeaconPayload| {
            let mut pd = Vec::new();
            ciborium::into_writer(payload, &mut pd).unwrap();
            SignedCommitteeEvent {
                tag: 'd',
                version: 1,
                committee_tier: 0,
                kind: DfrostEventKind::VrfBeacon,
                hlc: Hlc {
                    wall_ms: wall,
                    logical: 0,
                    device_id: "a".into(),
                },
                actor: members[0],
                payload: pd,
                sig: vec![0u8; 64],
            }
        };

        let good_payload = VrfBeaconPayload {
            ceremony_id: [0xcc; 32],
            message_hash,
            signature: sig_bytes.clone(),
            vrf_output,
        };
        let ev = build_event(5_000, &good_payload);

        assert_eq!(log.adopt_beacons(std::slice::from_ref(&ev), 0), 1);
        assert_eq!(log.beacon_index.get(&message_hash), Some(&vrf_output));
        assert_eq!(log.event_count(), 1);

        // Idempotent re-adopt.
        assert_eq!(log.adopt_beacons(std::slice::from_ref(&ev), 0), 0);
        assert_eq!(log.event_count(), 1);

        // Tampered signature (flip a byte in the `s` half) — the
        // vrf_output binding still passes (R untouched) but Schnorr
        // verify must fail.
        let mut tampered_sig = sig_bytes.clone();
        tampered_sig[40] ^= 0x01;
        let tampered_payload = VrfBeaconPayload {
            ceremony_id: [0xcc; 32],
            message_hash,
            signature: tampered_sig,
            vrf_output,
        };
        let tampered_ev = build_event(5_001, &tampered_payload);
        assert_eq!(log.adopt_beacons(&[tampered_ev], 0), 0);

        // Wrong vrf_output (R-binding check fails).
        let wrong_payload = VrfBeaconPayload {
            ceremony_id: [0xcc; 32],
            message_hash,
            signature: sig_bytes.clone(),
            vrf_output: [0x00; 32],
        };
        let wrong_ev = build_event(5_002, &wrong_payload);
        assert_eq!(log.adopt_beacons(&[wrong_ev], 0), 0);

        // Wrong-length signature (63 bytes) — rejected before any crypto.
        let mut short_sig = sig_bytes.clone();
        short_sig.pop();
        assert_eq!(short_sig.len(), 63);
        let short_payload = VrfBeaconPayload {
            ceremony_id: [0xcc; 32],
            message_hash: [0x44; 32], // distinct hash — must not collide with the indexed entry
            signature: short_sig,
            vrf_output,
        };
        let short_ev = build_event(5_004, &short_payload);
        assert_eq!(log.adopt_beacons(&[short_ev], 0), 0);

        // A second VALID signature over the SAME message_hash (fresh
        // FROST nonces → a different R, hence a different vrf_output):
        // min-wins (ZEB-1032) — the index converges on the bytewise
        // smaller output, changing iff the newcomer is smaller.
        let mut rng2 = frost_ristretto255::rand_core::OsRng;
        let mut nonces2 = BTreeMap::new();
        let mut commitments2 = BTreeMap::new();
        for id in &ids[..2] {
            let kp = key_packages.get(id).unwrap();
            let (n, c) = frost_ristretto255::round1::commit(kp.signing_share(), &mut rng2);
            nonces2.insert(*id, n);
            commitments2.insert(*id, c);
        }
        let signing_package2 = frost_ristretto255::SigningPackage::new(commitments2, &message_hash);
        let mut shares2 = BTreeMap::new();
        for id in &ids[..2] {
            let kp = key_packages.get(id).unwrap();
            shares2.insert(
                *id,
                frost_ristretto255::round2::sign(&signing_package2, nonces2.get(id).unwrap(), kp)
                    .expect("round2 sign"),
            );
        }
        let sig2 = frost_ristretto255::aggregate(&signing_package2, &shares2, &pub_pkg)
            .expect("aggregate");
        let sig2_bytes = sig2.serialize().expect("sig2 serialize");
        let r2: [u8; 32] = sig2_bytes[..32].try_into().unwrap();
        let vrf_output2 = derive_vrf_output(&r2);
        assert_ne!(
            vrf_output2, vrf_output,
            "fresh nonces must produce a distinct beacon (or this test is not exercising the tie)"
        );
        let second_payload = VrfBeaconPayload {
            ceremony_id: [0xcc; 32],
            message_hash,
            signature: sig2_bytes,
            vrf_output: vrf_output2,
        };
        let second_ev = build_event(5_005, &second_payload);
        let expect_downgrade = vrf_output2 < vrf_output;
        assert_eq!(
            log.adopt_beacons(&[second_ev], 0),
            usize::from(expect_downgrade),
            "min-wins: a second valid beacon changes the index iff its output is smaller (ZEB-1032)"
        );
        assert_eq!(
            log.beacon_index.get(&message_hash),
            Some(&std::cmp::min(vrf_output, vrf_output2)),
            "index converges on the bytewise-minimum output (ZEB-1032)"
        );

        // pending_sign was never touched by any of the above.
        assert!(log.committee_state.pending_sign.contains_key(&[0xaa; 32]));

        // Batch of [good, bad] adopts exactly the good one.
        let mut fresh_log = committee_log_from_material(&members, &ids, &pub_pkg, None);
        let bogus_payload = VrfBeaconPayload {
            ceremony_id: [0xcc; 32],
            message_hash: [0x9a; 32],
            signature: vec![0u8; 64],
            vrf_output: [0x00; 32],
        };
        let bad_ev = build_event(5_003, &bogus_payload);
        assert_eq!(fresh_log.adopt_beacons(&[ev.clone(), bad_ev], 0), 1);
        assert_eq!(fresh_log.beacon_index.get(&message_hash), Some(&vrf_output));
        assert_eq!(fresh_log.event_count(), 1);
    }

    /// ZEB-1032: the beacon index must converge on the bytewise-minimum
    /// `vrf_output` regardless of arrival order and regardless of which
    /// path (live `apply_vrf_beacon` / catch-up `adopt_beacons`)
    /// delivered each beacon — the reviewer probe on ZEB-1030 showed a
    /// caught-up replica (first-wins adopt) and an always-online replica
    /// (last-wins live apply) indexing different sortition values for
    /// the same two-beacon event set.
    #[test]
    fn beacon_index_min_wins_across_paths_and_orders_zeb1032() {
        use crate::community_dfrost_types::{derive_vrf_output, VrfBeaconPayload};
        use crate::owner_state_types::Hlc;

        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let message_hash = [0x5a; 32];

        // Two independently-valid FROST threshold signatures over the
        // SAME message (two concurrent ceremonies: fresh nonces each →
        // distinct R → distinct vrf_output, both Schnorr-valid).
        let sign_once = || {
            let mut rng = frost_ristretto255::rand_core::OsRng;
            let mut nonces = BTreeMap::new();
            let mut commitments = BTreeMap::new();
            for id in &ids[..2] {
                let kp = key_packages.get(id).unwrap();
                let (n, c) = frost_ristretto255::round1::commit(kp.signing_share(), &mut rng);
                nonces.insert(*id, n);
                commitments.insert(*id, c);
            }
            let signing_package =
                frost_ristretto255::SigningPackage::new(commitments, &message_hash);
            let mut shares = BTreeMap::new();
            for id in &ids[..2] {
                let kp = key_packages.get(id).unwrap();
                shares.insert(
                    *id,
                    frost_ristretto255::round2::sign(&signing_package, nonces.get(id).unwrap(), kp)
                        .expect("round2 sign"),
                );
            }
            let sig = frost_ristretto255::aggregate(&signing_package, &shares, &pub_pkg)
                .expect("aggregate");
            let sig_bytes = sig.serialize().expect("sig serialize");
            let r: [u8; 32] = sig_bytes[..32].try_into().unwrap();
            (sig_bytes, derive_vrf_output(&r))
        };
        let (sig_a, out_a) = sign_once();
        let (sig_b, out_b) = sign_once();
        assert_ne!(out_a, out_b, "fresh nonces must yield distinct outputs");
        let min_out = std::cmp::min(out_a, out_b);

        let vb_event = |ceremony_id: [u8; 32], wall: u64, sig: &[u8], out: [u8; 32]| {
            let payload = VrfBeaconPayload {
                ceremony_id,
                message_hash,
                signature: sig.to_vec(),
                vrf_output: out,
            };
            let mut pd = Vec::new();
            ciborium::into_writer(&payload, &mut pd).unwrap();
            SignedCommitteeEvent {
                tag: 'd',
                version: 1,
                committee_tier: 0,
                kind: DfrostEventKind::VrfBeacon,
                hlc: Hlc {
                    wall_ms: wall,
                    logical: 0,
                    device_id: "a".into(),
                },
                actor: members[0],
                payload: pd,
                sig: vec![0u8; 64],
            }
        };
        let seed_session = |log: &mut DfrostLog, cid: [u8; 32]| {
            log.committee_state.pending_sign.insert(
                cid,
                PendingSignSession {
                    message_hash,
                    contributions: BTreeMap::new(),
                    local_nonces: None,
                    purpose: SignPurpose::default(),
                },
            );
        };
        let both_orders = [
            ((sig_a.clone(), out_a), (sig_b.clone(), out_b)),
            ((sig_b.clone(), out_b), (sig_a.clone(), out_a)),
        ];

        // Adopt path: both orders converge on the min; the change count
        // reflects whether the newcomer actually changed the index.
        for (first, second) in &both_orders {
            let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
            assert_eq!(
                log.adopt_beacons(&[vb_event([0xc1; 32], 1_000, &first.0, first.1)], 0),
                1
            );
            assert_eq!(
                log.adopt_beacons(&[vb_event([0xc2; 32], 1_001, &second.0, second.1)], 0),
                usize::from(second.1 < first.1)
            );
            assert_eq!(log.beacon_index.get(&message_hash), Some(&min_out));
        }

        // Live path: both orders converge on the min. Each apply consumes
        // its own pending_sign session (distinct ceremony ids pinning the
        // SAME message_hash — exactly the concurrent-ceremony race).
        for (first, second) in &both_orders {
            let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
            seed_session(&mut log, [0xd1; 32]);
            seed_session(&mut log, [0xd2; 32]);
            log.apply(vb_event([0xd1; 32], 2_000, &first.0, first.1))
                .unwrap();
            log.apply(vb_event([0xd2; 32], 2_001, &second.0, second.1))
                .unwrap();
            assert_eq!(log.beacon_index.get(&message_hash), Some(&min_out));
        }

        // Cross-path, both directions.
        let (hi, lo) = if out_a < out_b {
            ((sig_b, out_b), (sig_a, out_a))
        } else {
            ((sig_a, out_a), (sig_b, out_b))
        };
        // (1) Live-applied larger output, then catch-up serves the
        //     smaller → adopt downgrades (self-heal for replicas that
        //     indexed the larger arrival first).
        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
        seed_session(&mut log, [0xe1; 32]);
        log.apply(vb_event([0xe1; 32], 3_000, &hi.0, hi.1)).unwrap();
        assert_eq!(log.beacon_index.get(&message_hash), Some(&hi.1));
        assert_eq!(
            log.adopt_beacons(&[vb_event([0xe2; 32], 3_001, &lo.0, lo.1)], 0),
            1,
            "adopt downgrades a larger live-indexed output (ZEB-1032 self-heal)"
        );
        assert_eq!(log.beacon_index.get(&message_hash), Some(&min_out));
        // (2) Adopted the smaller first; a later live apply of the larger
        //     must not regress the index.
        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
        assert_eq!(
            log.adopt_beacons(&[vb_event([0xf1; 32], 4_000, &lo.0, lo.1)], 0),
            1
        );
        seed_session(&mut log, [0xf2; 32]);
        log.apply(vb_event([0xf2; 32], 4_001, &hi.0, hi.1)).unwrap();
        assert_eq!(
            log.beacon_index.get(&message_hash),
            Some(&min_out),
            "live apply must not overwrite a smaller indexed output (ZEB-1032)"
        );
    }

    /// ZEB-1035: `adopt_beacons` REJECTS (skips — neither indexes nor
    /// retains) an event whose envelope HLC is beyond the forward-skew
    /// tolerance. Retaining it would mean re-serving it forever: it
    /// sorts above every requester's (skew-capped, per ZEB-1030
    /// final-review C1) watermark, so `select_catchup` would ship it
    /// every round and the fully-current short-circuit would never fire.
    #[test]
    fn adopt_beacons_rejects_forward_skewed_envelope_zeb1035() {
        use crate::community_dfrost_types::{derive_vrf_output, VrfBeaconPayload};
        use crate::owner_state_types::Hlc;

        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let message_hash = [0x6b; 32];

        // One real threshold signature, re-wrapped at different envelope
        // HLCs below (adopt_beacons verifies the payload; the envelope
        // stamp is whatever the re-broadcaster chose).
        let mut rng = frost_ristretto255::rand_core::OsRng;
        let mut nonces = BTreeMap::new();
        let mut commitments = BTreeMap::new();
        for id in &ids[..2] {
            let kp = key_packages.get(id).unwrap();
            let (n, c) = frost_ristretto255::round1::commit(kp.signing_share(), &mut rng);
            nonces.insert(*id, n);
            commitments.insert(*id, c);
        }
        let signing_package = frost_ristretto255::SigningPackage::new(commitments, &message_hash);
        let mut shares = BTreeMap::new();
        for id in &ids[..2] {
            let kp = key_packages.get(id).unwrap();
            shares.insert(
                *id,
                frost_ristretto255::round2::sign(&signing_package, nonces.get(id).unwrap(), kp)
                    .expect("round2 sign"),
            );
        }
        let sig =
            frost_ristretto255::aggregate(&signing_package, &shares, &pub_pkg).expect("aggregate");
        let sig_bytes = sig.serialize().expect("sig serialize");
        let r: [u8; 32] = sig_bytes[..32].try_into().unwrap();
        let payload = VrfBeaconPayload {
            ceremony_id: [0xcc; 32],
            message_hash,
            signature: sig_bytes,
            vrf_output: derive_vrf_output(&r),
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        let wrap_at = |wall: u64| SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::VrfBeacon,
            hlc: Hlc {
                wall_ms: wall,
                logical: 0,
                device_id: "a".into(),
            },
            actor: members[0],
            payload: pd.clone(),
            sig: vec![0u8; 64],
        };

        let now: u64 = 1_000_000_000;
        let max = crate::clock_trust::MAX_FORWARD_SKEW_MS;

        // Beyond tolerance → rejected outright: not indexed AND not
        // retained (the gate precedes decode/crypto).
        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);
        let skewed = wrap_at(now + max + 1);
        assert_eq!(log.adopt_beacons(std::slice::from_ref(&skewed), now), 0);
        assert!(log.beacon_index.is_empty(), "skewed beacon never indexed");
        assert_eq!(log.event_count(), 0, "skewed beacon never retained");

        // Exactly at the tolerance bound → still plausible: adopted.
        assert_eq!(log.adopt_beacons(&[wrap_at(now + max)], now), 1);
        assert_eq!(log.event_count(), 1);

        // now == 0 (clock unreadable) disables the gate — a bad LOCAL
        // clock must not suppress adoption.
        let mut log2 = committee_log_from_material(&members, &ids, &pub_pkg, None);
        assert_eq!(log2.adopt_beacons(&[skewed], 0), 1);
        assert_eq!(log2.event_count(), 1);
    }

    /// ZEB-1030 round-2: a genuine, committee-signed beacon PAYLOAD
    /// re-wrapped in a NON-member's envelope must be rejected — the
    /// Schnorr check alone (over the payload, under the joint vk) isn't
    /// enough, because the envelope `actor` is just the re-broadcaster,
    /// not part of what gets signed. The event-level `sig` here is a
    /// fabricated all-zero placeholder, which is fine: `adopt_beacons`
    /// operates at the log layer and never checks it (Ed25519 envelope-
    /// signature verification against a resolved identity happens at the
    /// engine's ingest layer) — what this test isolates is the new
    /// membership check on `event.actor` alone.
    #[test]
    fn adopt_beacons_rejects_non_member_envelope_actor_zeb1030() {
        use crate::community_dfrost_types::{derive_vrf_output, VrfBeaconPayload};
        use crate::owner_state_types::Hlc;

        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, None);

        let message_hash = [0x55u8; 32];
        let mut rng = frost_ristretto255::rand_core::OsRng;
        let mut nonces = BTreeMap::new();
        let mut commitments = BTreeMap::new();
        for id in &ids[..2] {
            let kp = key_packages.get(id).unwrap();
            let (n, c) = frost_ristretto255::round1::commit(kp.signing_share(), &mut rng);
            nonces.insert(*id, n);
            commitments.insert(*id, c);
        }
        let signing_package = frost_ristretto255::SigningPackage::new(commitments, &message_hash);
        let mut shares = BTreeMap::new();
        for id in &ids[..2] {
            let kp = key_packages.get(id).unwrap();
            shares.insert(
                *id,
                frost_ristretto255::round2::sign(&signing_package, nonces.get(id).unwrap(), kp)
                    .expect("round2 sign"),
            );
        }
        let sig =
            frost_ristretto255::aggregate(&signing_package, &shares, &pub_pkg).expect("aggregate");
        let sig_bytes = sig.serialize().expect("sig serialize");
        let r: [u8; 32] = sig_bytes[..32].try_into().unwrap();
        let vrf_output = derive_vrf_output(&r);

        let good_payload = VrfBeaconPayload {
            ceremony_id: [0xcc; 32],
            message_hash,
            signature: sig_bytes,
            vrf_output,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&good_payload, &mut pd).unwrap();

        let non_member_actor = OwnerAddr([0x99; 16]);
        assert!(
            !members.contains(&non_member_actor),
            "fixture actor must actually be outside the committee"
        );
        let ev = SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::VrfBeacon,
            hlc: Hlc {
                wall_ms: 5_000,
                logical: 0,
                device_id: "x".into(),
            },
            actor: non_member_actor,
            payload: pd,
            sig: vec![0u8; 64],
        };

        assert_eq!(log.adopt_beacons(&[ev], 0), 0);
        assert_eq!(log.beacon_index.get(&message_hash), None);
        assert_eq!(log.event_count(), 0);
    }

    #[test]
    fn adopt_epoch_heals_stale_beacon_lookup_zeb1030() {
        use crate::community_dfrost_types::{
            derive_vrf_seed, DkgCompletePayload, MemberVerifyingShare,
        };

        let (members, ids, key_packages, pub_pkg) = dkg_2of3_material();
        let alice = members[0];
        let bob = members[1];
        let alice_kp = key_packages.get(&ids[0]).unwrap().clone();

        let mut log = committee_log_from_material(&members, &ids, &pub_pkg, Some(alice_kp));

        let seed = [0x55u8; 32];
        let beacon_output = [0x77u8; 32];
        // The straggler's live traffic indexed a beacon under the TRUE
        // hash from the sender's epoch-2 traffic.
        let true_hash = derive_vrf_seed(&seed, 2);
        log.beacon_index.insert(true_hash, beacon_output);

        assert_eq!(
            log.find_vrf_beacon_output_by_seed(&seed, 1),
            None,
            "stale-epoch lookup misses"
        );

        let held_vk = log.committee_state.joint_verifying_key.unwrap();
        let new_shares: Vec<MemberVerifyingShare> = members
            .iter()
            .enumerate()
            .map(|(i, m)| MemberVerifyingShare {
                member: *m,
                verifying_share: [0x51 + i as u8; 32],
            })
            .collect();
        let payload = DkgCompletePayload {
            ceremony_id: [0x67; 32],
            joint_verifying_key: held_vk,
            verifying_shares: new_shares,
            epoch: 2,
            members: members.clone(),
            threshold: 2,
            max_signers: 3,
            space_id: None,
        };
        let events = vec![
            signed_dk(alice, 30_000, "a", &payload),
            signed_dk(bob, 30_001, "b", &payload),
        ];
        assert_eq!(log.adopt_refresh_quorum(&events, &zeb1034_space()), Ok(2));

        assert_eq!(
            log.find_vrf_beacon_output_by_seed(&seed, log.committee_state.current_epoch),
            Some(beacon_output),
            "the oracle now derives the message hash from the adopted epoch"
        );
    }

    // -----------------------------------------------------------------
    // ZEB-1031: `apply_reset_marker` — RS-M1/M2/M6.
    // -----------------------------------------------------------------

    const RESET_PROPOSAL_ID: crate::community_membership::EventId = [0x11; 16];
    const RESET_DIGEST: [u8; 32] = [0x22; 32];

    /// ZEB-1031 helper: build an `rs` (ResetMarker) event with a fake
    /// signature (apply does not verify — that's the engine's job, same
    /// convention as `di_event`).
    fn rs_event(
        actor: OwnerAddr,
        reset_proposal_id: crate::community_membership::EventId,
        reset_digest: [u8; 32],
        old_vk: [u8; 32],
        old_epoch: u64,
        space_id: crate::owner_state_types::SpaceId,
        wall_ms: u64,
    ) -> crate::community_dfrost_types::SignedCommitteeEvent {
        use crate::community_dfrost_types::{
            DfrostEventKind, ResetMarkerPayload, SignedCommitteeEvent,
        };
        use crate::owner_state_types::Hlc;
        let payload = ResetMarkerPayload {
            reset_proposal_id,
            reset_digest,
            old_vk,
            old_epoch,
            space_id,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ResetMarker,
            hlc: Hlc {
                wall_ms,
                logical: 0,
                device_id: "t".into(),
            },
            actor,
            payload: pd,
            sig: vec![0u8; 64],
        }
    }

    /// ZEB-1031: an active committee at `(vk, epoch)` — the fixture
    /// every reset-marker test starts from. Every test passes
    /// `&zeb1034_space()` as `apply_reset_marker`'s `expected_space`
    /// except the dedicated wrong-space one.
    fn active_committee_for_reset(
        vk: [u8; 32],
        epoch: u64,
        members: Vec<OwnerAddr>,
        threshold: u16,
    ) -> DfrostLog {
        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.joint_verifying_key = Some(vk);
        log.committee_state.current_epoch = epoch;
        log.committee_state.max_signers = members.len() as u16;
        log.committee_state.members = members;
        log.committee_state.threshold = threshold;
        log
    }

    #[test]
    fn reset_marker_happy_path_deactivates_and_pins_successor_zeb1031() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let old_vk = [0xaa; 32];
        let new_members = vec![OwnerAddr([0x03; 16]), OwnerAddr([0x04; 16])];

        let mut log = active_committee_for_reset(old_vk, 3, vec![alice, bob], 2);
        // Seed every pending slot so the effects assertions below prove
        // they all get cleared by the marker apply.
        log.committee_state.pending_dkg = Some(PendingCeremony::default());
        log.committee_state.pending_refresh = Some(PendingCeremony::default());
        log.committee_state.pending_repair = Some(PendingRepair::new(
            [0x77; 32],
            alice,
            3,
            vec![bob],
            1_000,
            0,
        ));
        log.committee_state
            .pending_sign
            .insert([0x88; 32], PendingSignSession::default());

        let ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            old_vk,
            3,
            zeb1034_space(),
            5_000,
        );

        let result = log.apply_reset_marker(&ev, &zeb1034_space(), new_members.clone(), 2);
        assert_eq!(
            result,
            Ok(ResetMarkerApplied::Applied {
                old_epoch: 3,
                reset_id: RESET_PROPOSAL_ID,
            })
        );

        assert!(!log.committee_state.active);
        assert!(log.committee_state.joint_verifying_key.is_none());
        assert_eq!(
            log.committee_state.current_epoch, 3,
            "current_epoch stays at old_epoch — the di epoch+1 gate yields the successor"
        );
        assert_eq!(log.committee_state.vk_history.len(), 1);
        let entry = &log.committee_state.vk_history[0];
        assert_eq!(entry.old_vk, old_vk);
        assert_eq!(entry.old_epoch, 3);
        assert_eq!(entry.reset_id, RESET_PROPOSAL_ID);
        assert_eq!(entry.digest, RESET_DIGEST);
        assert_eq!(entry.at, ev.hlc);

        let pin = log
            .committee_state
            .pending_reset
            .as_ref()
            .expect("pending_reset set");
        assert_eq!(pin.reset_id, RESET_PROPOSAL_ID);
        assert_eq!(pin.new_members, new_members);
        assert_eq!(pin.new_threshold, 2);

        assert!(log.committee_state.pending_dkg.is_none());
        assert!(log.committee_state.pending_refresh.is_none());
        assert!(log.committee_state.pending_repair.is_none());
        assert!(log.committee_state.pending_sign.is_empty());
        assert_eq!(log.event_count(), 1, "the marker itself joins the log");
    }

    #[test]
    fn reset_marker_rs_m1_wrong_space_rejected_zeb1031() {
        let alice = OwnerAddr([0x01; 16]);
        let old_vk = [0xaa; 32];
        let mut log = active_committee_for_reset(old_vk, 3, vec![alice], 1);
        // The marker itself carries a VALID `sp` — the mismatch is
        // between the marker's claim and the caller's own ground truth
        // (`expected_space`), which is what RS-M1 actually guards
        // against (a marker minted for a different community).
        let ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            old_vk,
            3,
            zeb1034_space(),
            5_000,
        );
        let wrong_space = crate::owner_state_types::SpaceId([0x99; 16]);

        assert_eq!(
            log.apply_reset_marker(&ev, &wrong_space, vec![alice], 1),
            Err(ApplyError::InvariantViolation)
        );
        // No partial state change.
        assert!(log.committee_state.active);
        assert_eq!(log.committee_state.joint_verifying_key, Some(old_vk));
        assert_eq!(log.committee_state.current_epoch, 3);
        assert!(log.committee_state.vk_history.is_empty());
        assert!(log.committee_state.pending_reset.is_none());
        assert_eq!(log.event_count(), 0);
    }

    #[test]
    fn reset_marker_rs_m2_not_active_rejected_zeb1031() {
        let alice = OwnerAddr([0x01; 16]);
        let old_vk = [0xaa; 32];
        let mut log = active_committee_for_reset(old_vk, 3, vec![alice], 1);
        log.committee_state.active = false;
        let ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            old_vk,
            3,
            zeb1034_space(),
            5_000,
        );

        assert_eq!(
            log.apply_reset_marker(&ev, &zeb1034_space(), vec![alice], 1),
            Err(ApplyError::InvariantViolation)
        );
        assert!(log.committee_state.vk_history.is_empty());
        assert!(log.committee_state.pending_reset.is_none());
        assert_eq!(log.event_count(), 0);
    }

    #[test]
    fn reset_marker_rs_m2_vk_mismatch_rejected_zeb1031() {
        let alice = OwnerAddr([0x01; 16]);
        let held_vk = [0xaa; 32];
        let marker_vk = [0xbb; 32];
        let mut log = active_committee_for_reset(held_vk, 3, vec![alice], 1);
        let ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            marker_vk,
            3,
            zeb1034_space(),
            5_000,
        );

        assert_eq!(
            log.apply_reset_marker(&ev, &zeb1034_space(), vec![alice], 1),
            Err(ApplyError::InvariantViolation)
        );
        assert!(log.committee_state.active);
        assert_eq!(log.committee_state.joint_verifying_key, Some(held_vk));
        assert!(log.committee_state.vk_history.is_empty());
        assert_eq!(log.event_count(), 0);
    }

    #[test]
    fn reset_marker_rs_m2_epoch_mismatch_rejected_zeb1031() {
        // Simulate a mid-flight refresh: the marker targets epoch 3, but
        // the committee has already advanced to epoch 4 by apply time —
        // membership cannot see dfrost state, so this staleness is
        // enforced here, not in the lifecycle (spec §5.1).
        let alice = OwnerAddr([0x01; 16]);
        let vk = [0xaa; 32];
        let mut log = active_committee_for_reset(vk, 4, vec![alice], 1);
        let ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            vk,
            3,
            zeb1034_space(),
            5_000,
        );

        assert_eq!(
            log.apply_reset_marker(&ev, &zeb1034_space(), vec![alice], 1),
            Err(ApplyError::InvariantViolation)
        );
        assert!(log.committee_state.active);
        assert_eq!(log.committee_state.current_epoch, 4);
        assert!(log.committee_state.vk_history.is_empty());
        assert_eq!(log.event_count(), 0);
    }

    #[test]
    fn reset_marker_rs_m6_replay_after_applied_is_already_moved_zeb1031() {
        let alice = OwnerAddr([0x01; 16]);
        let old_vk = [0xaa; 32];
        let new_members = vec![OwnerAddr([0x03; 16])];
        let mut log = active_committee_for_reset(old_vk, 3, vec![alice], 1);
        let ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            old_vk,
            3,
            zeb1034_space(),
            5_000,
        );

        let first = log.apply_reset_marker(&ev, &zeb1034_space(), new_members.clone(), 1);
        assert_eq!(
            first,
            Ok(ResetMarkerApplied::Applied {
                old_epoch: 3,
                reset_id: RESET_PROPOSAL_ID,
            })
        );

        // Catch-up replay legitimately re-delivers the SAME marker —
        // benign no-op, not an error.
        let second = log.apply_reset_marker(&ev, &zeb1034_space(), new_members, 1);
        assert_eq!(second, Ok(ResetMarkerApplied::AlreadyMoved));

        // The replay leaves state exactly as the first apply left it.
        assert_eq!(log.committee_state.vk_history.len(), 1);
        assert!(log.committee_state.pending_reset.is_some());
        assert_eq!(log.event_count(), 1, "the replay is not re-inserted");
    }

    /// Asserts committee state is byte-unchanged from a freshly-built
    /// `active_committee_for_reset(old_vk, 3, [alice], 1)` fixture — the
    /// shared assertion for I2's envelope/kind-guard regression tests
    /// (both must reject BEFORE any mutation, not one step later inside
    /// `insert_applied`'s policy verify).
    fn assert_reset_marker_state_untouched(log: &DfrostLog, old_vk: [u8; 32]) {
        assert!(log.committee_state.active);
        assert_eq!(log.committee_state.joint_verifying_key, Some(old_vk));
        assert_eq!(log.committee_state.current_epoch, 3);
        assert!(log.committee_state.vk_history.is_empty());
        assert!(log.committee_state.pending_reset.is_none());
        assert!(log.committee_state.pending_dkg.is_none());
        assert!(log.committee_state.pending_refresh.is_none());
        assert!(log.committee_state.pending_repair.is_none());
        assert!(log.committee_state.pending_sign.is_empty());
        assert_eq!(log.event_count(), 0);
    }

    #[test]
    fn reset_marker_bad_tier_rejected_before_mutation_zeb1031() {
        // I2: a non-zero `committee_tier` must be rejected by the SAME
        // envelope gate `apply`/`apply_with_identity` run — BEFORE any
        // state mutation. Without it, `committee_tier`/`tag` (both
        // attacker-chosen fields on a signed event) let a malformed
        // envelope deactivate the committee while
        // `insert_applied`'s policy verify rejects the event one step
        // later — a `tracing::warn!`, never surfaced to the caller, and
        // no event lands in the log to explain the mutation.
        let alice = OwnerAddr([0x01; 16]);
        let old_vk = [0xaa; 32];
        let mut log = active_committee_for_reset(old_vk, 3, vec![alice], 1);
        let mut ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            old_vk,
            3,
            zeb1034_space(),
            5_000,
        );
        ev.committee_tier = 7;

        assert_eq!(
            log.apply_reset_marker(&ev, &zeb1034_space(), vec![alice], 1),
            Err(ApplyError::UnexpectedEnvelope)
        );
        assert_reset_marker_state_untouched(&log, old_vk);
    }

    #[test]
    fn reset_marker_wrong_kind_rejected_before_mutation_zeb1031() {
        // I2: an event carrying an `rs`-shaped payload under a
        // DIFFERENT `kind` (e.g. `DkgComplete`) must be rejected —
        // without the kind guard it applies AND is stored under the
        // wrong kind, so the log holds a `DkgComplete` event whose
        // payload cannot decode as `DkgCompletePayload`, which fails
        // `PayloadDecode` for every peer on catch-up.
        let alice = OwnerAddr([0x01; 16]);
        let old_vk = [0xaa; 32];
        let mut log = active_committee_for_reset(old_vk, 3, vec![alice], 1);
        let mut ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            old_vk,
            3,
            zeb1034_space(),
            5_000,
        );
        ev.kind = crate::community_dfrost_types::DfrostEventKind::DkgComplete;

        assert_eq!(
            log.apply_reset_marker(&ev, &zeb1034_space(), vec![alice], 1),
            Err(ApplyError::UnexpectedEnvelope)
        );
        assert_reset_marker_state_untouched(&log, old_vk);
    }

    // ZEB-1031 Task 4: successor-DKG pin tests
    //
    // After a reset marker is applied, the DfrostLog's pending_reset is set
    // with the pinned new_members and new_threshold. A successor di must
    // claim exactly this shape, else it is rejected. On dk promotion, the
    // pending_reset is cleared.

    #[test]
    fn reset_dkg_pin_di_wrong_members_rejected_zeb1031() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let carol = OwnerAddr([0x03; 16]);
        let dave = OwnerAddr([0x04; 16]);
        let old_vk = [0xaa; 32];
        let new_members = vec![carol, dave];

        let mut log = active_committee_for_reset(old_vk, 3, vec![alice, bob], 2);

        // Apply the reset marker to set pending_reset.
        let ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            old_vk,
            3,
            zeb1034_space(),
            5_000,
        );
        log.apply_reset_marker(&ev, &zeb1034_space(), new_members.clone(), 2)
            .expect("marker applies");

        assert!(log.committee_state.pending_reset.is_some());

        // Try to apply di with wrong members (alice + carol instead of
        // carol + dave) — should be rejected as InvariantViolation.
        assert_eq!(
            log.check_ceremony_init_admissible(
                &crate::community_dfrost_types::CeremonyInitPayload {
                    ceremony_id: [0x42; 32],
                    members: vec![alice, carol],
                    threshold: 2,
                    max_signers: 2,
                    epoch: 4,
                    minted_wall_ms: 6_000,
                    minted_logical: 0,
                },
                &alice
            ),
            Err(ApplyError::InvariantViolation),
            "di with wrong members should be rejected when pending_reset is set"
        );
    }

    #[test]
    fn reset_dkg_pin_di_wrong_threshold_rejected_zeb1031() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let carol = OwnerAddr([0x03; 16]);
        let dave = OwnerAddr([0x04; 16]);
        let old_vk = [0xaa; 32];
        let new_members = vec![carol, dave];

        let mut log = active_committee_for_reset(old_vk, 3, vec![alice, bob], 2);

        // Apply the reset marker to set pending_reset.
        let ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            old_vk,
            3,
            zeb1034_space(),
            5_000,
        );
        log.apply_reset_marker(&ev, &zeb1034_space(), new_members.clone(), 2)
            .expect("marker applies");

        assert!(log.committee_state.pending_reset.is_some());

        // Try to apply di with correct members but wrong threshold
        // (1 instead of 2) — should be rejected as InvariantViolation.
        let payload = crate::community_dfrost_types::CeremonyInitPayload {
            ceremony_id: [0x42; 32],
            members: new_members.clone(),
            threshold: 1, // Wrong!
            max_signers: 2,
            epoch: 4,
            minted_wall_ms: 6_000,
            minted_logical: 0,
        };

        assert_eq!(
            log.check_ceremony_init_admissible(&payload, &carol),
            Err(ApplyError::InvariantViolation),
            "di with wrong threshold should be rejected when pending_reset is set"
        );
    }

    #[test]
    fn reset_dkg_pin_di_exact_shape_admitted_zeb1031() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let carol = OwnerAddr([0x03; 16]);
        let dave = OwnerAddr([0x04; 16]);
        let old_vk = [0xaa; 32];
        let new_members = vec![carol, dave];

        let mut log = active_committee_for_reset(old_vk, 3, vec![alice, bob], 2);

        // Apply the reset marker to set pending_reset.
        let ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            old_vk,
            3,
            zeb1034_space(),
            5_000,
        );
        log.apply_reset_marker(&ev, &zeb1034_space(), new_members.clone(), 2)
            .expect("marker applies");

        assert!(log.committee_state.pending_reset.is_some());

        // Apply di with exact pinned shape — should be admitted.
        let payload = crate::community_dfrost_types::CeremonyInitPayload {
            ceremony_id: [0x42; 32],
            members: new_members.clone(),
            threshold: 2,
            max_signers: 2,
            epoch: 4,
            minted_wall_ms: 6_000,
            minted_logical: 0,
        };

        assert_eq!(
            log.check_ceremony_init_admissible(&payload, &carol),
            Ok(()),
            "di with exact pinned shape should be admitted"
        );
    }

    #[test]
    fn reset_dkg_pin_promotion_clears_pending_reset_zeb1031() {
        use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare};
        use crate::owner_state_types::Hlc;

        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let carol = OwnerAddr([0x03; 16]);
        let dave = OwnerAddr([0x04; 16]);
        let old_vk = [0xaa; 32];
        let new_vk = [0xbb; 32];
        let new_members = vec![carol, dave];

        let mut log = active_committee_for_reset(old_vk, 3, vec![alice, bob], 2);

        // Apply the reset marker to set pending_reset.
        let rs_ev = rs_event(
            alice,
            RESET_PROPOSAL_ID,
            RESET_DIGEST,
            old_vk,
            3,
            zeb1034_space(),
            5_000,
        );
        log.apply_reset_marker(&rs_ev, &zeb1034_space(), new_members.clone(), 2)
            .expect("marker applies");

        // Verify state after marker: deactivated, pending_reset set, vk_history recorded.
        assert!(!log.committee_state.active);
        assert!(log.committee_state.joint_verifying_key.is_none());
        assert_eq!(log.committee_state.current_epoch, 3);
        assert_eq!(log.committee_state.vk_history.len(), 1);
        let pin = log
            .committee_state
            .pending_reset
            .as_ref()
            .expect("pending_reset set");
        assert_eq!(pin.new_members, new_members);
        assert_eq!(pin.new_threshold, 2);

        // Seed a di (ceremony init) with exact pinned shape.
        let ceremony_id = [0x42u8; 32];
        log.apply(di_event(
            carol,
            new_members.clone(),
            2,
            4,
            ceremony_id,
            6_000,
        ))
        .expect("di seeds pending_dkg");

        assert!(log.committee_state.pending_dkg.is_some());

        // Build and apply dk (DKG complete) events to reach quorum and promote.
        let payload = DkgCompletePayload {
            ceremony_id,
            joint_verifying_key: new_vk,
            verifying_shares: vec![
                MemberVerifyingShare {
                    member: carol,
                    verifying_share: [0xc1; 32],
                },
                MemberVerifyingShare {
                    member: dave,
                    verifying_share: [0xd1; 32],
                },
            ],
            epoch: 4,
            members: new_members.clone(),
            threshold: 2,
            max_signers: 2,
            space_id: None,
        };

        let mut wall = 7_000u64;
        // Apply dk events from carol and dave to reach quorum (threshold=2).
        for confirmer in [carol, dave] {
            let mut pd = Vec::new();
            ciborium::into_writer(&payload, &mut pd).unwrap();
            log.apply(crate::community_dfrost_types::SignedCommitteeEvent {
                tag: 'd',
                version: 1,
                committee_tier: 0,
                kind: crate::community_dfrost_types::DfrostEventKind::DkgComplete,
                hlc: Hlc {
                    wall_ms: wall,
                    logical: 0,
                    device_id: "t".into(),
                },
                actor: confirmer,
                payload: pd,
                sig: vec![0u8; 64],
            })
            .expect("dk applies");
            wall += 1;
        }

        // Verify state after promotion:
        assert!(log.committee_state.active, "committee promoted");
        assert_eq!(
            log.committee_state.joint_verifying_key,
            Some(new_vk),
            "new vk installed"
        );
        assert_eq!(log.committee_state.current_epoch, 4, "epoch advanced");
        assert_eq!(
            log.committee_state.members, new_members,
            "new members installed"
        );
        assert_eq!(log.committee_state.threshold, 2, "new threshold installed");

        // The critical assertion: pending_reset is cleared on promotion.
        assert_eq!(
            log.committee_state.pending_reset, None,
            "pending_reset cleared on dk promotion"
        );

        // Verify vk_history lineage is still present.
        assert_eq!(
            log.committee_state.vk_history.len(),
            1,
            "vk_history still carries the lineage entry"
        );
        let lineage = &log.committee_state.vk_history[0];
        assert_eq!(lineage.old_vk, old_vk, "lineage records old vk");
        assert_eq!(lineage.old_epoch, 3, "lineage records old epoch");
        assert_eq!(
            lineage.reset_id, RESET_PROPOSAL_ID,
            "lineage records reset_id"
        );
    }
}
