//! ZEB-677 S3: the `owner-quorum-req-v1` fleet dataset — pending quorum
//! co-sign requests (revocation now; enrollment arms in S4) replicated
//! between the owner's devices as the next `FleetSyncEngine` dataset.
//! Donor pattern: `owner_trust_sync.rs` (merge/persist/applied-task shape)
//! and `fleet_key_epoch.rs` (own-doc-file persistence recipe). Spec:
//! `docs/specs/2026-07-12-zeb-677-quorum-wiring-design.md` §3/§4.
//!
//! ## Ceremony data flow
//!
//! The S1 crate binds the SIGNER SET into the quorum signing payload
//! (`RevocationCert::quorum_signing_payload_bytes`), and any eligible
//! sibling may co-sign — so every detached signature is over the payload
//! for the sorted pair `[initiator, cosigner]`. The initiator pre-signs
//! one payload per eligible cosigner at request creation
//! (`QuorumRequest::initiator_sigs`), which doubles as request
//! authentication: a co-signer verifies the initiator's part against the
//! initiator's enrolled key before adding its own. The initiator's
//! assembly-time part is minted fresh when a valid co-signature arrives
//! (`run_quorum_sweep`, Task 3).
//!
//! ## Merge discipline
//!
//! Requests are keyed by a 16-byte random id: unknown ids insert (bounded
//! by `MAX_QUORUM_REQUESTS`, with an expiry-horizon sanity check); known
//! ids union `initiator_sigs` / `signatures` / `declined_by`. The union is
//! COMMUTATIVE: conflicting values for the same key resolve to the
//! lexicographically smaller value, and caps are enforced by
//! union-then-truncate in sorted key order — so every replica converges on
//! the same set regardless of arrival order. (A hostile in-fleet writer
//! can still plant garbage values — it holds the fleet keys — but garbage
//! never verifies; enforcement lives in the signature checks, and the
//! deterministic merge just guarantees replicas agree on WHAT they hold.)
//! Identity fields (`kind`, initiator, timestamps) must match on re-merge
//! or the remote copy is dropped with a warn — the id is random, so a
//! mismatch is a tamper signal, not a race. `enroll_arms` cells are LWW on
//! their `set_at` Hlc. Pruning NEVER happens inside the merge (which stays
//! a pure union) — `prune_settled_requests` runs from the applied-task
//! sweep and removes requests that are expired or whose revocation target
//! is already revoked in the trust doc (the convergent completion signal;
//! no explicit tombstone needed). Declined requests stay resident (dead,
//! hidden from the co-sign UI) until expiry so a union re-merge from a
//! device that never saw the decline cannot resurrect them as actionable.
//!
//! ## Declines are signed
//!
//! A decline permanently kills a request, and the whole point of the
//! ceremony is removing a possibly-compromised device — so a decline must
//! not be forgeable by the device being removed. `declined_by` maps
//! decliner id → detached signature over `decline_signing_payload`, and a
//! decline only COUNTS (`verified_decliners`) when the signature verifies
//! against the decliner's enrolled key AND the decliner is an eligible
//! voter: enrolled, master-issued, not revoked, and neither the request's
//! target nor its initiator. Unverifiable entries are inert junk.

use crate::fleet_sync::{FleetPersist, MergeOutcome, Merger, SyncError};
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use ciborium::{from_reader, into_writer};
use harmony_owner::certs::{RevocationCert, RevocationReason};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Dataset name — forms the Zenoh topic
/// `harmony/owner/{addr_hex}/ds/owner-quorum-req-v1` via
/// `spawn_dataset_sync_zenoh_adapter`, and doubles as the CAS lookup tag.
pub const OWNER_QUORUM_DATASET: &str = "owner-quorum-req-v1";
pub const OWNER_QUORUM_LOOKUP_TAG: &[u8] = b"owner-quorum-req-v1";

/// Request docs are tiny (≤ `MAX_QUORUM_REQUESTS` requests, each with a
/// handful of 64-byte signatures); 256 KiB is generous headroom while
/// still bounding a hostile publish.
pub const OWNER_QUORUM_DATASET_MAX_BYTES: usize = 256 * 1024;

pub const OWNER_QUORUM_DOC_FILENAME: &str = "owner_quorum_req.cbor";
pub const OWNER_QUORUM_REPLAY_FILENAME: &str = "owner_quorum_replay.cbor";

const OWNER_QUORUM_SCHEMA_V1: u8 = 1;

/// Revocation co-sign requests expire after 24 h (spec §3).
pub const QUORUM_REVOCATION_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// Hard cap on resident pending requests — far above any real ceremony
/// rate; bounds a hostile or runaway publish.
pub const MAX_QUORUM_REQUESTS: usize = 32;

/// Per-request cap on each signature/decline map — matches the realistic
/// fleet-size ceiling and bounds hostile growth.
pub const MAX_QUORUM_SIG_ENTRIES: usize = 16;

/// Clock-skew allowance when validating remote expiry stamps. A remote
/// request may not claim an expiry beyond `now + TTL + skew` — otherwise
/// 32 `u64::MAX`-expiry requests would exhaust `MAX_QUORUM_REQUESTS`
/// forever (they'd never prune).
pub const QUORUM_CLOCK_SKEW_MS: u64 = 60 * 60 * 1000;

/// The S4 pre-armed enrollment co-sign window: 15 minutes, single-use.
pub const ARM_WINDOW_MS: u64 = 15 * 60 * 1000;

/// The same horizon rule bounds hostile arm cells (window + skew).
pub const QUORUM_ARM_HORIZON_MS: u64 = ARM_WINDOW_MS + QUORUM_CLOCK_SKEW_MS;

/// What a pending request asks the fleet to co-sign. Revocation only in
/// S3; the S4 enrollment ceremony adds its variant; S5 bundles the next-epoch
/// carrier doc (full crypto cutoff) into `Revocation` + adds `EpochBump`, and
/// the co-signer's second detached signature rides `epoch_doc_sig_hex`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum QuorumRequestKind {
    #[serde(rename = "r")]
    Revocation {
        /// Wire label of the revocation reason ("decommissioned" | "lost"
        /// | "compromised" — `owner_commands::parse_revoke_reason`).
        #[serde(rename = "e")]
        reason: String,
        /// Hex of the target's 16-byte device id.
        #[serde(rename = "t")]
        target_hex: String,
        /// ZEB-677 S5 — canonical-CBOR hex of the UNSIGNED next-epoch carrier
        /// doc bundled with this revocation (full crypto cutoff, §7). The
        /// co-signer signs its `signing_bytes` into `epoch_doc_sig_hex`; A
        /// assembles the quorum-signed carrier on completion. `None` when the
        /// initiator's node isn't carrying fleet keys — revoke-only, and the
        /// `fleetEpochStale` banner offers a manual rotate.
        #[serde(rename = "d", default, skip_serializing_if = "Option::is_none")]
        epoch_doc_cbor_hex: Option<String>,
        /// ZEB-677 S5 — the INITIATOR's detached signature over the bundled
        /// epoch doc's `signing_bytes` (present iff `epoch_doc_cbor_hex` is).
        /// Binds the epoch doc to the initiator's authorization: a co-signer
        /// verifies this before signing, so a replicated-doc write cannot
        /// substitute a different epoch doc for the co-signer to bless (Qodo
        /// PR #461).
        #[serde(rename = "s", default, skip_serializing_if = "Option::is_none")]
        epoch_doc_initiator_sig_hex: Option<String>,
    },
    /// ZEB-677 S5 — a standalone quorum fleet-epoch rotation (no revocation),
    /// for the `fleetEpochStale` retry surface on a master-less fleet. The
    /// co-signer signs the carrier `signing_bytes` into `primary_sig_hex`
    /// (there is no revocation payload for this kind).
    #[serde(rename = "m")]
    EpochBump {
        /// Canonical-CBOR hex of the UNSIGNED next-epoch carrier doc.
        #[serde(rename = "d")]
        epoch_doc_cbor_hex: String,
    },
    /// S4 enrollment ceremony: the initiator asks its armed sibling to
    /// co-sign a quorum enrollment cert for a newly-paired device. The
    /// joiner's device id + pubkey bundle are fixed in the payload the
    /// signers cover (`enrollment_quorum_payload`).
    #[serde(rename = "n")]
    Enrollment {
        /// Hex of the joiner's 16-byte device id.
        #[serde(rename = "j")]
        joiner_device_id_hex: String,
        /// CBOR-hex of the joiner's `PubKeyBundle` (the enrolled key set).
        #[serde(rename = "b")]
        joiner_pubkeys_cbor_hex: String,
    },
}

/// One device's detached signatures over a request's constituent payloads.
/// One approval action yields all of them (spec §3). `epoch_doc_sig_hex`
/// is the S5 slot (bundled epoch bump) — always `None` in S3.
/// `Ord` backs the merge's deterministic conflict resolution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct QuorumRequestSigs {
    #[serde(rename = "e", default, skip_serializing_if = "Option::is_none")]
    pub epoch_doc_sig_hex: Option<String>,
    /// Hex of the 64-byte detached signature over the pair payload for
    /// `sorted([initiator, this signer])`.
    #[serde(rename = "p")]
    pub primary_sig_hex: String,
}

/// A pending co-sign request. Fields other than the three CRDT maps are
/// identity: fixed at creation, never merged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuorumRequest {
    /// Creation stamp — LWW metadata / display ordering only.
    #[serde(rename = "c")]
    pub created_at: Hlc,
    /// decliner device-id hex → hex of its detached signature over
    /// `decline_signing_payload` (see module docs). Grow-only; ANY
    /// VERIFIED decline from an eligible voter tombstones the request
    /// (spec §3) — it stays resident but dead until expiry. Unverified
    /// entries never count.
    #[serde(rename = "d", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub declined_by: BTreeMap<String, String>,
    /// Hex of the initiating device's 16-byte device id.
    #[serde(rename = "i")]
    pub initiator_hex: String,
    #[serde(rename = "k")]
    pub kind: QuorumRequestKind,
    /// candidate cosigner device-id hex → hex of the initiator's detached
    /// signature over the pair payload `sorted([initiator, cosigner])`.
    /// Written in full at creation; authenticates the request to each
    /// candidate (a co-signer refuses to sign an unauthenticated entry).
    #[serde(rename = "p", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub initiator_sigs: BTreeMap<String, String>,
    /// cosigner device-id hex → that device's signatures. Grow-only union;
    /// the initiator assembles from the first entry that verifies.
    #[serde(rename = "s", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub signatures: BTreeMap<String, QuorumRequestSigs>,
    /// Cert timestamp (unix SECONDS) — part of every signed payload.
    #[serde(rename = "u")]
    pub issued_at: u64,
    /// Wall-clock ms after which the request is dead (24 h TTL).
    #[serde(rename = "x")]
    pub expires_at_ms: u64,
}

/// Pre-armed enrollment co-sign window (S4 ceremony; struct lands now for
/// schema stability — no S3 writer).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrollArm {
    /// LWW stamp for cell replacement.
    #[serde(rename = "a")]
    pub set_at: Hlc,
    /// Wall-clock ms the 15-minute single-use window closes.
    #[serde(rename = "u")]
    pub armed_until_ms: u64,
}

/// The replicated quorum-request doc.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QuorumReqDoc {
    /// Pre-armed enrollment windows, keyed by armer device-id hex.
    #[serde(rename = "e", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub enroll_arms: BTreeMap<String, EnrollArm>,
    /// Pending co-sign requests, keyed by request id (16-byte random, hex).
    #[serde(rename = "r", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requests: BTreeMap<String, QuorumRequest>,
}

// Manual CanonicalPayload registration (the `impl_canonical!` macro in
// owner_state_types.rs covers that module's types; this is the same pair
// of empty impls it expands to).
impl CanonicalPayloadSealed for QuorumReqDoc {}
impl CanonicalPayload for QuorumReqDoc {}

/// True when the request's per-device maps are within the hostile-growth
/// caps (checked before inserting a remote-authored request).
fn within_caps(req: &QuorumRequest) -> bool {
    req.initiator_sigs.len() <= MAX_QUORUM_SIG_ENTRIES
        && req.signatures.len() <= MAX_QUORUM_SIG_ENTRIES
        && req.declined_by.len() <= MAX_QUORUM_SIG_ENTRIES
}

/// Identity fields must be byte-identical for two copies of the same
/// (random) request id — anything else is a tamper signal, not a race.
fn same_identity(a: &QuorumRequest, b: &QuorumRequest) -> bool {
    a.kind == b.kind
        && a.initiator_hex == b.initiator_hex
        && a.issued_at == b.issued_at
        && a.expires_at_ms == b.expires_at_ms
}

/// Commutative bounded map union: conflicting values for the same key
/// resolve to the smaller value; the result is truncated to `cap` entries
/// in sorted key order. Pure function of the two input maps — replicas
/// converge regardless of merge order.
fn union_bounded<V: Ord>(
    existing: &mut BTreeMap<String, V>,
    incoming: BTreeMap<String, V>,
    cap: usize,
) {
    for (k, v) in incoming {
        match existing.entry(k) {
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(v);
            }
            std::collections::btree_map::Entry::Occupied(mut e) => {
                if v < *e.get() {
                    e.insert(v);
                }
            }
        }
    }
    while existing.len() > cap {
        let last = existing
            .keys()
            .next_back()
            .expect("non-empty over cap")
            .clone();
        existing.remove(&last);
    }
}

/// Fold a remote quorum doc into local. Pure union — no pruning here (see
/// module docs). Changed-detection is canonical-encode compare (docs are
/// tiny), matching the trust-merge donor.
pub fn merge_quorum_remote_into_local(
    local: &mut QuorumReqDoc,
    remote: QuorumReqDoc,
) -> MergeOutcome {
    let before = crate::owner_state_crypto::canonical_cbor_encode(&*local).ok();
    // Real wall clock, like the trust merge: an expired remote request is
    // never (re-)inserted (a stale peer republishing one can't ping-pong
    // it back after the local sweep pruned it), and a remote request may
    // not claim an expiry beyond the TTL horizon (32 u64::MAX-expiry
    // requests would otherwise exhaust the cap forever).
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let expiry_horizon = now_ms.saturating_add(QUORUM_REVOCATION_TTL_MS + QUORUM_CLOCK_SKEW_MS);
    for (id, req) in remote.requests {
        match local.requests.get_mut(&id) {
            None => {
                if now_ms > req.expires_at_ms {
                    continue;
                }
                if req.expires_at_ms > expiry_horizon {
                    tracing::warn!(request = %id, "quorum merge: over-horizon expiry; dropped");
                    continue;
                }
                if !within_caps(&req) {
                    tracing::warn!(request = %id, "quorum merge: over-cap request dropped");
                    continue;
                }
                local.requests.insert(id, req);
            }
            Some(existing) => {
                if !same_identity(existing, &req) {
                    tracing::warn!(
                        request = %id,
                        "quorum merge: identity-field mismatch on known request id; remote dropped"
                    );
                    continue;
                }
                union_bounded(
                    &mut existing.initiator_sigs,
                    req.initiator_sigs,
                    MAX_QUORUM_SIG_ENTRIES,
                );
                union_bounded(
                    &mut existing.signatures,
                    req.signatures,
                    MAX_QUORUM_SIG_ENTRIES,
                );
                union_bounded(
                    &mut existing.declined_by,
                    req.declined_by,
                    MAX_QUORUM_SIG_ENTRIES,
                );
            }
        }
    }
    // Deterministic request-count cap: union-then-truncate in sorted id
    // order, so replicas keep the SAME 32 requests whatever the arrival
    // order was. (An in-fleet flooder can evict — it holds the fleet keys
    // and could equally flood any dataset; the bound is resource hygiene,
    // not a security boundary.)
    while local.requests.len() > MAX_QUORUM_REQUESTS {
        let last = local
            .requests
            .keys()
            .next_back()
            .expect("non-empty over cap")
            .clone();
        tracing::warn!(request = %last, "quorum merge: request cap reached; evicted");
        local.requests.remove(&last);
    }
    for (armer, arm) in remote.enroll_arms {
        // Horizon rule for arm cells too (15-min window + skew).
        if arm.armed_until_ms > now_ms.saturating_add(QUORUM_ARM_HORIZON_MS) {
            tracing::warn!(armer = %armer, "quorum merge: over-horizon arm cell; dropped");
            continue;
        }
        match local.enroll_arms.get(&armer) {
            Some(cur) if !arm.set_at.is_strictly_newer_than(&cur.set_at) => {}
            _ => {
                local.enroll_arms.insert(armer, arm);
            }
        }
    }
    let after = crate::owner_state_crypto::canonical_cbor_encode(&*local).ok();
    MergeOutcome {
        changed: before != after,
    }
}

/// The quorum doc's `Merger` for `FleetSyncEngine` construction.
pub fn quorum_merger() -> Merger<QuorumReqDoc> {
    Arc::new(merge_quorum_remote_into_local)
}

/// Remove settled requests: expired, or (Revocation) target already
/// revoked in the trust doc — the convergent completion signal every
/// device reaches without an explicit tombstone. Malformed target hex
/// (cannot ever complete) is settled too. Declined-but-unexpired requests
/// are RETAINED (UI-dead) so the decline tombstone survives union
/// re-merges until natural expiry. Expired enrollment arms are retained a
/// full merge horizon past expiry (the single-use tombstone must outlive
/// any older live arm — see the `retain` below). Returns whether anything
/// was removed.
pub fn prune_settled_requests(
    doc: &mut QuorumReqDoc,
    trust: &harmony_owner::state::OwnerState,
    now_ms: u64,
) -> bool {
    let before_reqs = doc.requests.len();
    let before_arms = doc.enroll_arms.len();
    doc.requests.retain(|id, req| {
        if now_ms > req.expires_at_ms {
            return false;
        }
        // Enrollment requests have no convergent completion signal in the
        // trust doc (the cert lands via the A-side pairing flow, not the
        // sweep), so keep them until TTL expiry (handled above).
        let QuorumRequestKind::Revocation { target_hex, .. } = &req.kind else {
            return true;
        };
        match parse_device_id_hex(target_hex) {
            Ok(target) => {
                if trust.is_revoked(target) {
                    tracing::debug!(request = %id, "quorum prune: target already revoked");
                    return false;
                }
                true
            }
            Err(_) => {
                tracing::warn!(request = %id, "quorum prune: malformed target hex; dropped");
                false
            }
        }
    });
    // Expired arm cells are RETAINED for a full merge horizon past their
    // expiry — NOT dropped at `armed_until_ms`. A disarm/consume writes a
    // newer-Hlc but already-expired tombstone (see `stamp_arm_cell`); if we
    // pruned it the instant it expired, an older live arm re-merging from a
    // lagging replica would resurrect the single-use window (no tombstone
    // left to win LWW). By `armed_until_ms + QUORUM_ARM_HORIZON_MS` any such
    // older arm has itself expired, so resurrecting it is harmless.
    doc.enroll_arms
        .retain(|_, arm| now_ms <= arm.armed_until_ms.saturating_add(QUORUM_ARM_HORIZON_MS));
    doc.requests.len() != before_reqs || doc.enroll_arms.len() != before_arms
}

/// Stamp THIS device's arm cell with a strictly-newer Hlc so the merge's
/// LWW always prefers it over the prior arm/disarm/consume (the cell is
/// keyed by armer device-id, so every write to it supersedes the same key).
/// `armed_until_ms <= now_ms` ⇒ disarmed. We NEVER delete the cell: a delete
/// can be resurrected when an older `set_at` re-merges from another replica,
/// so disarm/consume write an already-expired cell that wins LWW and is
/// reaped later by `prune_settled_requests`. The `(wall_ms, logical)` bump
/// guarantees strict newness even for two writes in the same millisecond.
pub(crate) fn stamp_arm_cell(
    doc: &mut QuorumReqDoc,
    self_id: [u8; 16],
    armed_until_ms: u64,
    now_ms: u64,
) {
    let self_hex = hex::encode(self_id);
    let (wall_ms, logical) = match doc.enroll_arms.get(&self_hex).map(|a| &a.set_at) {
        Some(prev) if prev.wall_ms >= now_ms => {
            // Advance strictly past the prior stamp without overflowing the
            // u32 logical counter: roll the wall clock forward one tick when
            // it is saturated (astronomically rare, but strict monotonicity
            // is load-bearing for the arm cell's LWW).
            if prev.logical == u32::MAX {
                (prev.wall_ms.saturating_add(1), 0)
            } else {
                (prev.wall_ms, prev.logical + 1)
            }
        }
        _ => (now_ms, 0),
    };
    doc.enroll_arms.insert(
        self_hex.clone(),
        EnrollArm {
            set_at: Hlc {
                wall_ms,
                logical,
                device_id: self_hex,
            },
            armed_until_ms,
        },
    );
}

/// Decode a 16-byte device-id hex string.
pub(crate) fn parse_device_id_hex(hex_str: &str) -> Result<[u8; 16], String> {
    let bytes = hex::decode(hex_str).map_err(|e| format!("bad device id hex: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| "bad device id hex: expected 16 bytes".to_string())
}

/// The canonical detached-signature payload for the quorum pair
/// `sorted([a, b])` — both the initiator's pre-signed parts and the
/// co-signer's approval sign exactly this (see module docs).
pub fn revocation_pair_payload(
    owner_id: [u8; 16],
    target: [u8; 16],
    issued_at: u64,
    reason: &RevocationReason,
    a: [u8; 16],
    b: [u8; 16],
) -> Result<Vec<u8>, String> {
    let mut pair = [a, b];
    pair.sort();
    RevocationCert::quorum_signing_payload_bytes(owner_id, target, issued_at, reason, &pair)
        .map_err(|e| format!("quorum pair payload: {e}"))
}

/// Domain-separated payload a decliner signs to veto a request (module
/// docs: declines must not be forgeable). The prefix cannot collide with
/// the crate's canonical-CBOR cert payloads (those start with a CBOR map
/// header byte), and the owner id + request id bind the veto to exactly
/// one ceremony.
/// True when the request's INITIATOR has signed an abandon marker into
/// `declined_by[initiator]` — the convergent "I gave up on this ceremony"
/// signal written by `LiveQuorumEnrollPort` on co-sign timeout. The
/// signature is verified against the initiator's enrolled key over
/// `decline_signing_payload` (same tag/label as a revocation decline), so a
/// forged marker cannot silently block a live enrollment. Grow-only union
/// carries it to every replica, so an abandoned enrollment request is never
/// co-signed even after a stale re-merge.
fn initiator_abandoned(
    trust: &harmony_owner::state::OwnerState,
    request_id_hex: &str,
    req: &QuorumRequest,
) -> bool {
    let Some(sig_hex) = req.declined_by.get(&req.initiator_hex) else {
        return false;
    };
    let (Ok(initiator), Ok(sig)) = (
        parse_device_id_hex(&req.initiator_hex),
        hex::decode(sig_hex),
    ) else {
        return false;
    };
    let Some(cert) = trust.enrollments.get(&initiator) else {
        return false;
    };
    let Ok(vk) =
        ed25519_dalek::VerifyingKey::from_bytes(&cert.device_pubkeys.classical.ed25519_verify)
    else {
        return false;
    };
    let payload = decline_signing_payload(trust.owner_id, request_id_hex);
    harmony_owner::signing::verify_with_tag(
        &vk,
        harmony_owner::signing::tags::REVOCATION,
        &payload,
        &sig,
        "Revocation-Quorum-Decline",
    )
    .is_ok()
}

pub fn decline_signing_payload(owner_id: [u8; 16], request_id_hex: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32 + 16 + request_id_hex.len());
    buf.extend_from_slice(b"harmony-zeb677-quorum-decline-v1");
    buf.extend_from_slice(&owner_id);
    buf.extend_from_slice(request_id_hex.as_bytes());
    buf
}

/// The decliner ids whose veto COUNTS for this request: entry signature
/// verifies over `decline_signing_payload` against the decliner's enrolled
/// key, and the decliner is an eligible voter — enrolled, Master-issued,
/// not revoked, and neither the target nor the initiator (the device being
/// removed must never veto its own removal; the initiator cancels by
/// letting the request expire, not by declining).
pub fn verified_decliners(
    trust: &harmony_owner::state::OwnerState,
    request_id_hex: &str,
    req: &QuorumRequest,
) -> std::collections::BTreeSet<String> {
    // Enrollment requests carry no decline flow (an armed sibling auto-
    // co-signs; there is no veto surface), so no declines ever count.
    let QuorumRequestKind::Revocation { target_hex, .. } = &req.kind else {
        return std::collections::BTreeSet::new();
    };
    let payload = decline_signing_payload(trust.owner_id, request_id_hex);
    req.declined_by
        .iter()
        .filter(|(id_hex, sig_hex)| {
            if **id_hex == req.initiator_hex || *id_hex == target_hex {
                return false;
            }
            let Ok(id) = parse_device_id_hex(id_hex) else {
                return false;
            };
            if trust.is_revoked(id) {
                return false;
            }
            let Some(cert) = trust.enrollments.get(&id) else {
                return false;
            };
            if !crate::owner_quorum_commands::is_master_issued(cert) {
                return false;
            }
            let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(
                &cert.device_pubkeys.classical.ed25519_verify,
            ) else {
                return false;
            };
            let Ok(sig) = hex::decode(sig_hex) else {
                return false;
            };
            harmony_owner::signing::verify_with_tag(
                &vk,
                harmony_owner::signing::tags::REVOCATION,
                &payload,
                &sig,
                "Revocation-Quorum-Decline",
            )
            .is_ok()
        })
        .map(|(id_hex, _)| id_hex.clone())
        .collect()
}

/// Save the doc atomically (schema byte + canonical CBOR).
pub fn save_quorum_doc(path: &Path, doc: &QuorumReqDoc) -> Result<(), String> {
    let mut bytes = vec![OWNER_QUORUM_SCHEMA_V1];
    into_writer(doc, &mut bytes)
        .map_err(|e| format!("encode quorum doc {}: {e}", path.display()))?;
    crate::owner_state_persist::save_atomically(path, &bytes).map_err(|e| e.to_string())
}

/// Load the doc, recovering to empty on ANY failure. A lost doc is benign:
/// pending requests re-arrive through replication (or the user retries),
/// and the trust doc — the only authority — is untouched. Corrupt files
/// are quarantined (renamed aside) for manual inspection.
pub fn load_quorum_doc_or_recover(path: &Path) -> QuorumReqDoc {
    load_schema_v1_or_recover(path, "quorum doc")
}

/// Replay-tracker file body (schema byte precedes this on disk).
#[derive(Default, Serialize, Deserialize)]
struct QuorumReplayFileV1(BTreeMap<String, Hlc>);

/// Save the replay tracker atomically (schema byte + canonical CBOR).
pub fn save_quorum_replay(path: &Path, tracker: &BTreeMap<String, Hlc>) -> Result<(), String> {
    let mut bytes = vec![OWNER_QUORUM_SCHEMA_V1];
    into_writer(&QuorumReplayFileV1(tracker.clone()), &mut bytes)
        .map_err(|e| format!("encode quorum replay {}: {e}", path.display()))?;
    crate::owner_state_persist::save_atomically(path, &bytes).map_err(|e| e.to_string())
}

/// Load the replay tracker, recovering to empty on ANY failure (re-merging
/// an already-known publish is idempotent through the union merge).
pub fn load_quorum_replay_or_recover(path: &Path) -> BTreeMap<String, Hlc> {
    load_schema_v1_or_recover::<QuorumReplayFileV1>(path, "quorum replay").0
}

/// Shared schema-byte + quarantine load recipe (donor:
/// `owner_trust_sync::load_trust_replay_or_recover`).
fn load_schema_v1_or_recover<T: serde::de::DeserializeOwned + Default>(
    path: &Path,
    what: &str,
) -> T {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "{what} read failed; starting empty");
            return T::default();
        }
    };
    let decoded = match bytes.split_first() {
        Some((&OWNER_QUORUM_SCHEMA_V1, rest)) => from_reader::<T, _>(rest),
        _ => Err(ciborium::de::Error::Semantic(
            None,
            format!("bad {what} schema byte"),
        )),
    };
    match decoded {
        Ok(t) => t,
        Err(e) => {
            quarantine(path, what, &e.to_string());
            T::default()
        }
    }
}

// `load_schema_v1_or_recover::<QuorumReplayFileV1>` needs a Default.
/// Rename a corrupt file aside with a timestamped suffix (never clobbers a
/// prior quarantine or the live file; preserves bytes for recovery).
fn quarantine(path: &Path, what: &str, err: &str) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut corrupt = path.as_os_str().to_os_string();
    corrupt.push(format!(".corrupt-{ms}"));
    tracing::error!(path = %path.display(), error = %err,
        "{what} load failed; quarantining corrupt file and starting fresh (bytes preserved)");
    if let Err(re) = std::fs::rename(path, &corrupt) {
        tracing::warn!(path = %path.display(), error = %re,
            "failed to quarantine corrupt {what} file");
    }
}

/// Durability sink for the quorum engine: doc + replay tracker each to
/// their own file (no shared-file writer to serialize against, unlike the
/// trust doc's owner_state.cbor).
pub struct QuorumPersist {
    pub doc_path: PathBuf,
    pub replay_path: PathBuf,
}

impl FleetPersist<QuorumReqDoc> for QuorumPersist {
    fn persist(
        &self,
        state: &QuorumReqDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        save_quorum_doc(&self.doc_path, state).map_err(SyncError::Persist)?;
        save_quorum_replay(&self.replay_path, tracker).map_err(SyncError::Persist)?;
        Ok(())
    }
}

/// What one completion sweep did (test observability).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    pub doc_changed: bool,
    pub revocations_applied: usize,
    /// B-side enrollment co-signs applied this sweep (0 or 1 — single-use arm).
    pub enrollment_cosigns: usize,
    /// ZEB-677 S5 — quorum fleet-epoch bumps installed this sweep (bundled with
    /// a revocation or standalone `EpochBump`).
    pub epoch_bumps_installed: usize,
}

/// ZEB-677 S5 — the artifacts a completed quorum request yields. A revocation
/// yields a `RevocationCert` and (when bundled) a quorum-signed carrier doc;
/// a standalone `EpochBump` yields only the carrier doc (`cert: None`).
struct QuorumAssembly {
    cert: Option<RevocationCert>,
    epoch_doc: Option<crate::fleet_key_epoch::FleetKeyEpochDoc>,
}

/// One assemblable completion candidate, collected under the quorum-doc
/// lock and applied after it is released (the trust mutation takes the
/// trust-doc lock; never hold both).
struct CompletionCandidate {
    request_id: String,
    assembly: QuorumAssembly,
}

/// ZEB-677 S5 — assemble the quorum-signed next-epoch carrier doc from the
/// request-carried UNSIGNED doc + a co-signer's detached epoch-doc part.
/// Verifies both signers are Master-issued (depth-1) and the co-signer's part
/// is valid before minting A's own part and stamping the K=2 signature.
/// Returns `None` (revoke/bump still proceeds; banner offers a retry) on any
/// missing/invalid input.
fn assemble_quorum_epoch_doc(
    trust: &harmony_owner::state::OwnerState,
    device_signing_key: &ed25519_dalek::SigningKey,
    self_id: [u8; 16],
    unsigned_hex: &str,
    cosigner: [u8; 16],
    cosigner_epoch_sig_hex: &str,
) -> Option<crate::fleet_key_epoch::FleetKeyEpochDoc> {
    let bytes = hex::decode(unsigned_hex).ok()?;
    let mut doc: crate::fleet_key_epoch::FleetKeyEpochDoc =
        crate::owner_state_crypto::canonical_cbor_decode(&bytes).ok()?;
    let self_cert = trust.enrollments.get(&self_id)?;
    if !crate::owner_quorum_commands::is_master_issued(self_cert) {
        return None;
    }
    let cosigner_cert = trust.enrollments.get(&cosigner)?;
    if !crate::owner_quorum_commands::is_master_issued(cosigner_cert) {
        return None;
    }
    let cosigner_epoch_sig = hex::decode(cosigner_epoch_sig_hex).ok()?;
    let cosigner_vk = ed25519_dalek::VerifyingKey::from_bytes(
        &cosigner_cert.device_pubkeys.classical.ed25519_verify,
    )
    .ok()?;
    if !doc.verify_quorum_part(&cosigner_vk, &cosigner_epoch_sig) {
        tracing::warn!(cosigner = %hex::encode(cosigner),
            "quorum sweep: co-signer epoch-doc part failed verification; bump skipped");
        return None;
    }
    let own_epoch_sig = doc.quorum_part_over(device_signing_key).ok()?;
    let mut parts: Vec<([u8; 16], Vec<u8>)> =
        vec![(self_id, own_epoch_sig), (cosigner, cosigner_epoch_sig)];
    parts.sort_by_key(|(id, _)| *id);
    let signers: Vec<[u8; 16]> = parts.iter().map(|(id, _)| *id).collect();
    let signatures: Vec<Vec<u8>> = parts.into_iter().map(|(_, s)| s).collect();
    let signer_certs = vec![self_cert.clone(), cosigner_cert.clone()];
    doc.assemble_quorum(signers, signatures, signer_certs);
    Some(doc)
}

/// Validate a cosigner's entry against the CURRENT trust doc and, when it
/// verifies, assemble the K=2 revocation cert (+ bundled quorum epoch doc) or
/// a standalone quorum epoch bump.
fn try_assemble(
    trust: &harmony_owner::state::OwnerState,
    device_signing_key: &ed25519_dalek::SigningKey,
    self_id: [u8; 16],
    req: &QuorumRequest,
) -> Option<QuorumAssembly> {
    match &req.kind {
        // Enrollment certs are assembled A-side by the pairing ceremony
        // (owner_quorum_enroll), never by this sweep.
        QuorumRequestKind::Enrollment { .. } => None,
        QuorumRequestKind::EpochBump { epoch_doc_cbor_hex } => {
            // Standalone rotation: the co-signer's `primary_sig_hex` IS its
            // epoch-doc part (there is no revocation payload for this kind).
            for (cosigner_hex, sigs) in &req.signatures {
                let Ok(cosigner) = parse_device_id_hex(cosigner_hex) else {
                    continue;
                };
                if cosigner == self_id || trust.is_revoked(cosigner) {
                    continue;
                }
                if let Some(doc) = assemble_quorum_epoch_doc(
                    trust,
                    device_signing_key,
                    self_id,
                    epoch_doc_cbor_hex,
                    cosigner,
                    &sigs.primary_sig_hex,
                ) {
                    return Some(QuorumAssembly {
                        cert: None,
                        epoch_doc: Some(doc),
                    });
                }
            }
            None
        }
        QuorumRequestKind::Revocation {
            reason,
            target_hex,
            epoch_doc_cbor_hex,
            ..
        } => {
            let target = parse_device_id_hex(target_hex).ok()?;
            let reason = crate::owner_commands::parse_revoke_reason(reason).ok()?;
            for (cosigner_hex, sigs) in &req.signatures {
                let Ok(cosigner) = parse_device_id_hex(cosigner_hex) else {
                    continue;
                };
                if cosigner == self_id || cosigner == target || trust.is_revoked(cosigner) {
                    continue;
                }
                let Some(cert) = trust.enrollments.get(&cosigner) else {
                    continue;
                };
                if !crate::owner_quorum_commands::is_master_issued(cert) {
                    continue;
                }
                let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(
                    &cert.device_pubkeys.classical.ed25519_verify,
                ) else {
                    continue;
                };
                let Ok(payload) = revocation_pair_payload(
                    trust.owner_id,
                    target,
                    req.issued_at,
                    &reason,
                    self_id,
                    cosigner,
                ) else {
                    continue;
                };
                let Ok(cosig) = hex::decode(&sigs.primary_sig_hex) else {
                    continue;
                };
                if harmony_owner::signing::verify_with_tag(
                    &vk,
                    harmony_owner::signing::tags::REVOCATION,
                    &payload,
                    &cosig,
                    "Revocation-Quorum-Part",
                )
                .is_err()
                {
                    tracing::warn!(cosigner = %cosigner_hex, "quorum sweep: cosigner signature failed verification; skipped");
                    continue;
                }
                let own = RevocationCert::sign_quorum_part(device_signing_key, &payload);
                let mut parts = vec![(self_id, own), (cosigner, cosig)];
                parts.sort_by_key(|(id, _)| *id);
                let cert = match RevocationCert::assemble_quorum(
                    trust.owner_id,
                    target,
                    req.issued_at,
                    reason.clone(),
                    parts,
                ) {
                    Ok(cert) => cert,
                    Err(e) => {
                        tracing::warn!(error = %e, "quorum sweep: assemble failed; skipped");
                        continue;
                    }
                };
                // ZEB-677 S5 — same co-signer's second part assembles the
                // bundled crypto cutoff (if present). Revoke stands even if the
                // bump can't assemble.
                let epoch_doc = match (epoch_doc_cbor_hex, &sigs.epoch_doc_sig_hex) {
                    (Some(unsigned_hex), Some(epoch_sig_hex)) => assemble_quorum_epoch_doc(
                        trust,
                        device_signing_key,
                        self_id,
                        unsigned_hex,
                        cosigner,
                        epoch_sig_hex,
                    ),
                    _ => None,
                };
                return Some(QuorumAssembly {
                    cert: Some(cert),
                    epoch_doc,
                });
            }
            None
        }
    }
}

/// Canonical bytes both quorum signers cover for an enrollment cert. For
/// K=2 the `signers` slice is the sorted `[initiator, cosigner]` pair.
/// Quorum enrollment certs mint `expires_at: None` (the fleet's active
/// window governs liveness, not cert expiry) — sign and verify go through
/// the crate's single payload builder so they cannot drift.
pub fn enrollment_quorum_payload(
    owner_id: [u8; 16],
    joiner_device_id: [u8; 16],
    joiner_pubkeys: &harmony_owner::pubkey_bundle::PubKeyBundle,
    issued_at: u64,
    signers: &[[u8; 16]],
) -> Result<Vec<u8>, String> {
    harmony_owner::certs::EnrollmentCert::quorum_signing_payload_bytes(
        owner_id,
        joiner_device_id,
        joiner_pubkeys,
        issued_at,
        None,
        signers,
    )
    .map_err(|e| format!("enrollment quorum payload: {e}"))
}

/// Decode the joiner's `PubKeyBundle` from an Enrollment request (ciborium,
/// matching how the pairing SM CBOR-encodes certs/state).
pub(crate) fn decode_joiner_pubkeys(
    hex_str: &str,
) -> Result<harmony_owner::pubkey_bundle::PubKeyBundle, String> {
    let bytes = hex::decode(hex_str).map_err(|e| format!("joiner pubkeys hex: {e}"))?;
    ciborium::from_reader(bytes.as_slice()).map_err(|e| format!("joiner pubkeys cbor: {e}"))
}

/// A staged B-side enrollment co-signature: apply the Vouch under the trust
/// lock, then union the signature + consume the arm under the quorum lock.
struct EnrollmentCosign {
    request_id: String,
    self_sig_hex: String,
    vouch_cert: harmony_owner::certs::VouchingCert,
}

/// B-side (spec §5.2): if THIS device holds a live arm and an authenticated
/// `Enrollment` request from a sibling is pending (and not yet co-signed by
/// us), produce our quorum part + a `Vouch` for the joiner. The arm IS the
/// consent — no manual step. Single-use: only the FIRST eligible request is
/// taken; the caller consumes the arm on apply so a second ceremony in the
/// same window cannot ride it. Returns `None` when nothing is co-signable.
fn collect_enrollment_cosign(
    doc: &QuorumReqDoc,
    trust: &harmony_owner::state::OwnerState,
    device_signing_key: &ed25519_dalek::SigningKey,
    self_id: [u8; 16],
    now_secs: u64,
    now_ms: u64,
) -> Option<EnrollmentCosign> {
    let self_hex = hex::encode(self_id);
    // A live arm is the consent gate.
    if doc
        .enroll_arms
        .get(&self_hex)
        .is_none_or(|a| a.armed_until_ms <= now_ms)
    {
        return None;
    }
    // Depth-1: our own part is only valid if we hold a Master-issued cert.
    let self_cert = trust.enrollments.get(&self_id)?;
    if !crate::owner_quorum_commands::is_master_issued(self_cert) || trust.is_revoked(self_id) {
        return None;
    }
    for (id, req) in doc.requests.iter() {
        if now_ms > req.expires_at_ms
            || req.initiator_hex == self_hex
            || req.signatures.contains_key(&self_hex)
        {
            continue;
        }
        // The initiator can convergently ABANDON its own request (a grow-only
        // `declined_by` entry survives a union re-merge, unlike a delete) — do
        // not co-sign it, so a lagging replica re-merging an abandoned request
        // can't make us burn our single-use arm on a ceremony the inviter has
        // already given up on (Greptile).
        if initiator_abandoned(trust, id, req) {
            continue;
        }
        let QuorumRequestKind::Enrollment {
            joiner_device_id_hex,
            joiner_pubkeys_cbor_hex,
        } = &req.kind
        else {
            continue;
        };
        let (Ok(joiner_id), Ok(joiner_pk), Ok(initiator)) = (
            parse_device_id_hex(joiner_device_id_hex),
            decode_joiner_pubkeys(joiner_pubkeys_cbor_hex),
            parse_device_id_hex(&req.initiator_hex),
        ) else {
            continue;
        };
        // Both signers cover the SAME payload over the sorted signer set.
        let mut signers = [initiator, self_id];
        signers.sort();
        let Ok(payload) = enrollment_quorum_payload(
            trust.owner_id,
            joiner_id,
            &joiner_pk,
            req.issued_at,
            &signers,
        ) else {
            continue;
        };
        // Authenticate the request: the initiator's part must verify against
        // its enrolled Master key. An unauthenticated request is never
        // co-signed (a peer cannot forge a ceremony from A).
        let Some(init_sig_hex) = req.initiator_sigs.get(&self_hex) else {
            continue;
        };
        let Ok(init_sig) = hex::decode(init_sig_hex) else {
            continue;
        };
        let Some(init_cert) = trust.enrollments.get(&initiator) else {
            continue;
        };
        if !crate::owner_quorum_commands::is_master_issued(init_cert) || trust.is_revoked(initiator)
        {
            continue;
        }
        let Ok(init_vk) = ed25519_dalek::VerifyingKey::from_bytes(
            &init_cert.device_pubkeys.classical.ed25519_verify,
        ) else {
            continue;
        };
        if harmony_owner::signing::verify_with_tag(
            &init_vk,
            harmony_owner::signing::tags::ENROLLMENT,
            &payload,
            &init_sig,
            "Enrollment-Quorum-Member",
        )
        .is_err()
        {
            tracing::warn!(request = %id, "enrollment co-sign: initiator part failed verification; skipped");
            continue;
        }
        let self_sig =
            harmony_owner::certs::EnrollmentCert::sign_quorum_part(device_signing_key, &payload);
        let vouch_cert = harmony_owner::certs::VouchingCert::sign(
            device_signing_key,
            trust.owner_id,
            joiner_id,
            harmony_owner::certs::Stance::Vouch,
            now_secs,
        )
        .ok()?;
        return Some(EnrollmentCosign {
            request_id: id.clone(),
            self_sig_hex: hex::encode(self_sig),
            vouch_cert,
        });
    }
    None
}

/// A-side (spec §5.2/§5.3): pick an armed, active, Master-certed sibling
/// and build an `Enrollment` request for `joiner`, with THIS device's
/// authenticating quorum part attached (keyed by the chosen cosigner). Pure
/// — the caller supplies the arms snapshot + `now_ms` and writes/publishes
/// the returned request. `request_id` is caller-supplied so the planner is
/// deterministic under test.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_enrollment_request(
    trust: &harmony_owner::state::OwnerState,
    enroll_arms: &BTreeMap<String, EnrollArm>,
    device_signing_key: &ed25519_dalek::SigningKey,
    self_id: [u8; 16],
    joiner_device_id: [u8; 16],
    joiner_pubkeys: &harmony_owner::pubkey_bundle::PubKeyBundle,
    issued_at: u64,
    now_ms: u64,
    request_id: [u8; 16],
) -> Result<(String, QuorumRequest), String> {
    let self_hex = hex::encode(self_id);
    // Depth-1: the initiator's own part is only valid if it is Master-certed.
    let self_cert = trust
        .enrollments
        .get(&self_id)
        .ok_or_else(|| "notEnrolled: this device has no enrollment".to_string())?;
    if !crate::owner_quorum_commands::is_master_issued(self_cert) {
        return Err("notEligible: this device is not master-certed".to_string());
    }
    // Pick an armed sibling that can actually co-sign: live arm, active,
    // Master-certed, not self, not revoked.
    let active: std::collections::BTreeSet<[u8; 16]> = trust
        .active_devices(issued_at, harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS)
        .into_iter()
        .collect();
    let sibling = enroll_arms
        .iter()
        .filter(|(armer, arm)| arm.armed_until_ms > now_ms && **armer != self_hex)
        .filter_map(|(armer, _)| parse_device_id_hex(armer).ok())
        .find(|id| {
            *id != self_id
                && active.contains(id)
                && !trust.is_revoked(*id)
                && trust
                    .enrollments
                    .get(id)
                    .is_some_and(crate::owner_quorum_commands::is_master_issued)
        })
        .ok_or_else(|| {
            "noArmedSibling: no other device has an active enrollment window — ask a sibling \
             to Approve adding a device"
                .to_string()
        })?;
    let mut signers = [self_id, sibling];
    signers.sort();
    let payload = enrollment_quorum_payload(
        trust.owner_id,
        joiner_device_id,
        joiner_pubkeys,
        issued_at,
        &signers,
    )?;
    let a_part =
        harmony_owner::certs::EnrollmentCert::sign_quorum_part(device_signing_key, &payload);
    let mut joiner_pk_cbor = Vec::new();
    ciborium::into_writer(joiner_pubkeys, &mut joiner_pk_cbor)
        .map_err(|e| format!("encode joiner pubkeys: {e}"))?;
    let mut initiator_sigs = BTreeMap::new();
    initiator_sigs.insert(hex::encode(sibling), hex::encode(a_part));
    let req = QuorumRequest {
        created_at: Hlc {
            wall_ms: now_ms,
            logical: 0,
            device_id: self_hex.clone(),
        },
        declined_by: BTreeMap::new(),
        initiator_hex: self_hex,
        kind: QuorumRequestKind::Enrollment {
            joiner_device_id_hex: hex::encode(joiner_device_id),
            joiner_pubkeys_cbor_hex: hex::encode(joiner_pk_cbor),
        },
        initiator_sigs,
        signatures: BTreeMap::new(),
        issued_at,
        // Bound to the arm window: a co-signer only acts inside it anyway.
        expires_at_ms: now_ms.saturating_add(ARM_WINDOW_MS),
    };
    Ok((hex::encode(request_id), req))
}

/// A-side completion: assemble the quorum `EnrollmentCert` for a request
/// THIS device initiated, from the first cosigner signature that verifies
/// against the current trust doc. Returns `None` until a valid co-signature
/// has merged in. The initiator's own part is its `initiator_sigs` entry for
/// that cosigner. Mirror of `try_assemble` for the enrollment ceremony.
pub(crate) fn try_assemble_enrollment(
    doc: &QuorumReqDoc,
    trust: &harmony_owner::state::OwnerState,
    self_id: [u8; 16],
    request_id: &str,
) -> Option<harmony_owner::certs::EnrollmentCert> {
    let self_hex = hex::encode(self_id);
    let req = doc.requests.get(request_id)?;
    if req.initiator_hex != self_hex {
        return None;
    }
    let QuorumRequestKind::Enrollment {
        joiner_device_id_hex,
        joiner_pubkeys_cbor_hex,
    } = &req.kind
    else {
        return None;
    };
    let joiner_id = parse_device_id_hex(joiner_device_id_hex).ok()?;
    let joiner_pk = decode_joiner_pubkeys(joiner_pubkeys_cbor_hex).ok()?;
    for (cosigner_hex, sigs) in &req.signatures {
        let Ok(cosigner) = parse_device_id_hex(cosigner_hex) else {
            continue;
        };
        if cosigner == self_id || trust.is_revoked(cosigner) {
            continue;
        }
        let Some(cosigner_cert) = trust.enrollments.get(&cosigner) else {
            continue;
        };
        if !crate::owner_quorum_commands::is_master_issued(cosigner_cert) {
            continue;
        }
        let mut signers = [self_id, cosigner];
        signers.sort();
        let Ok(payload) = enrollment_quorum_payload(
            trust.owner_id,
            joiner_id,
            &joiner_pk,
            req.issued_at,
            &signers,
        ) else {
            continue;
        };
        let Ok(cosig) = hex::decode(&sigs.primary_sig_hex) else {
            continue;
        };
        let Ok(cosigner_vk) = ed25519_dalek::VerifyingKey::from_bytes(
            &cosigner_cert.device_pubkeys.classical.ed25519_verify,
        ) else {
            continue;
        };
        if harmony_owner::signing::verify_with_tag(
            &cosigner_vk,
            harmony_owner::signing::tags::ENROLLMENT,
            &payload,
            &cosig,
            "Enrollment-Quorum-Member",
        )
        .is_err()
        {
            tracing::warn!(request = %request_id, "enrollment assemble: cosigner sig failed; skipped");
            continue;
        }
        // A's own part is its authenticating entry for this cosigner.
        let Some(a_sig_hex) = req.initiator_sigs.get(cosigner_hex) else {
            continue;
        };
        let Ok(a_sig) = hex::decode(a_sig_hex) else {
            continue;
        };
        let mut parts = vec![(self_id, a_sig), (cosigner, cosig)];
        parts.sort_by_key(|(id, _)| *id);
        match harmony_owner::certs::EnrollmentCert::assemble_quorum(
            trust.owner_id,
            joiner_id,
            joiner_pk.clone(),
            req.issued_at,
            None,
            parts,
        ) {
            Ok(cert) => return Some(cert),
            Err(e) => {
                tracing::warn!(error = %e, "enrollment assemble failed; skipped");
                continue;
            }
        }
    }
    None
}

/// One completion pass over the quorum doc (spec §3: completion is
/// initiator-driven). Prunes settled requests, then — for requests THIS
/// device initiated — assembles the K=2 cert from the first cosigner
/// signature that verifies and applies it through the trust doc's
/// validating `add_revocation` (the authority; its quorum arm re-checks
/// the full signer policy incl. the active-window). A crate-level
/// rejection leaves the request resident for retry — expiry bounds it.
///
/// It also runs the B-side enrollment co-sign (spec §5.2): when THIS device
/// holds a live arm, an authenticated `Enrollment` request is co-signed and
/// the joiner vouched, then the arm is consumed (single-use).
///
/// Lock discipline: candidates are collected under the quorum lock,
/// applied under the trust lock, then removed under the quorum lock again
/// — the two locks are never held together.
#[allow(clippy::too_many_arguments)]
/// ZEB-677 S5 — the resident fleet-keys carrier handles the sweep needs to
/// install a quorum-signed epoch bump (bundled with a revocation or standalone).
/// All fields are cheap-clone handles, so the applied task can pull a snapshot
/// from a fillable slot each sweep (the carrier is built later in boot than the
/// task spawns).
#[derive(Clone)]
pub struct QuorumSweepCarrier {
    pub carrier_doc: Arc<tokio::sync::Mutex<crate::fleet_key_epoch::FleetKeyEpochDoc>>,
    pub carrier_engine:
        Arc<crate::fleet_sync::FleetSyncEngine<crate::fleet_key_epoch::FleetKeyEpochDoc>>,
    pub fleet_keys: crate::owner_state_crypto::FleetKeySet,
}

/// ZEB-677 S5 — install an assembled quorum-signed carrier doc under the
/// monotonic no-rollback rule (mirrors `revoke_device_inner`'s master-path
/// bump): adopt only if strictly newer, then install THIS device's own KeyTree
/// from its sealed blob and flush best-effort. Returns whether the doc was
/// adopted (the ceremony can then prune its request).
async fn install_quorum_epoch_doc(
    carrier: &QuorumSweepCarrier,
    device_signing_key: &ed25519_dalek::SigningKey,
    self_device_id: [u8; 16],
    doc: crate::fleet_key_epoch::FleetKeyEpochDoc,
) -> bool {
    {
        let mut cur = carrier.carrier_doc.lock().await;
        if doc.epoch <= cur.epoch {
            tracing::warn!(
                new = doc.epoch,
                current = cur.epoch,
                "quorum epoch install: assembled doc not newer than resident; skipped"
            );
            return false;
        }
        *cur = doc.clone();
    }
    // Install this device's own KeyTree from its sealed blob (like any
    // survivor). A missing/failed blob is non-fatal — the doc still published
    // for the other survivors; this device catches up on the next adoption.
    let self_hex = hex::encode(self_device_id);
    match crate::fleet_key_epoch::unseal_own_material(&doc, &self_hex, device_signing_key) {
        Ok(material) => match crate::owner_state_crypto::KeyTree::from_fleet_material(&material) {
            Ok(kt) => carrier.fleet_keys.install(Arc::new(kt)),
            Err(e) => tracing::warn!(error = %e,
                    "quorum epoch install: from_fleet_material failed"),
        },
        Err(e) => tracing::warn!(error = %e,
            "quorum epoch install: unseal own material failed (doc still published for survivors)"),
    }
    carrier.carrier_engine.notify_dirty();
    if let Err(e) = carrier.carrier_engine.flush_now().await {
        tracing::warn!(error = %e,
            "quorum epoch install: carrier flush failed; dirty latch will retry");
    }
    true
}

/// Thin wrapper: run a sweep with NO fleet-keys carrier (revoke-only; the
/// bundled/standalone epoch bump is skipped). Production and the S5 integration
/// test call [`run_quorum_sweep_with_carrier`].
#[allow(clippy::too_many_arguments)]
pub async fn run_quorum_sweep(
    quorum_doc: &Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    quorum_engine: &Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
    trust_doc: &Arc<tokio::sync::Mutex<harmony_owner::state::OwnerState>>,
    trust_engine: &Arc<crate::fleet_sync::FleetSyncEngine<harmony_owner::state::OwnerState>>,
    device_signing_key: &ed25519_dalek::SigningKey,
    self_device_id: [u8; 16],
    emit: &Arc<dyn Fn(&str) + Send + Sync>,
    retire_nudge: Option<&tokio::sync::mpsc::Sender<()>>,
    now_secs: u64,
    now_ms: u64,
) -> SweepOutcome {
    run_quorum_sweep_with_carrier(
        quorum_doc,
        quorum_engine,
        trust_doc,
        trust_engine,
        device_signing_key,
        self_device_id,
        emit,
        retire_nudge,
        now_secs,
        now_ms,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_quorum_sweep_with_carrier(
    quorum_doc: &Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    quorum_engine: &Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
    trust_doc: &Arc<tokio::sync::Mutex<harmony_owner::state::OwnerState>>,
    trust_engine: &Arc<crate::fleet_sync::FleetSyncEngine<harmony_owner::state::OwnerState>>,
    device_signing_key: &ed25519_dalek::SigningKey,
    self_device_id: [u8; 16],
    emit: &Arc<dyn Fn(&str) + Send + Sync>,
    retire_nudge: Option<&tokio::sync::mpsc::Sender<()>>,
    now_secs: u64,
    now_ms: u64,
    carrier: Option<&QuorumSweepCarrier>,
) -> SweepOutcome {
    let self_hex = hex::encode(self_device_id);
    let trust_snapshot = trust_doc.lock().await.clone();

    // Phase A: prune + collect candidates under the quorum lock.
    let (pruned, candidates, enroll_cosign) = {
        let mut doc = quorum_doc.lock().await;
        let pruned = prune_settled_requests(&mut doc, &trust_snapshot, now_ms);
        let mut candidates = Vec::new();
        for (id, req) in doc.requests.iter() {
            if req.initiator_hex != self_hex || now_ms > req.expires_at_ms {
                continue;
            }
            // ANY verified decline tombstones the request (unverified
            // entries are forgeable junk and never block — the target
            // must not be able to veto its own removal).
            if !verified_decliners(&trust_snapshot, id, req).is_empty() {
                continue;
            }
            if let Some(assembly) =
                try_assemble(&trust_snapshot, device_signing_key, self_device_id, req)
            {
                candidates.push(CompletionCandidate {
                    request_id: id.clone(),
                    assembly,
                });
            }
        }
        // B-side: at most one enrollment co-sign per sweep (single-use arm).
        let enroll_cosign = collect_enrollment_cosign(
            &doc,
            &trust_snapshot,
            device_signing_key,
            self_device_id,
            now_secs,
            now_ms,
        );
        (pruned, candidates, enroll_cosign)
    };
    if pruned {
        quorum_engine.notify_dirty();
    }

    // Phase B: apply each completed request through the authoritative path.
    // `completed` (pruned in Phase C) holds BOTH revocations and epoch bumps;
    // `revocations_applied` counts only the former for the outcome.
    let mut completed = Vec::new();
    let mut revocations_applied = 0usize;
    let mut epoch_bumps_installed = 0usize;
    for cand in candidates {
        let CompletionCandidate {
            request_id,
            assembly,
        } = cand;
        let QuorumAssembly { cert, epoch_doc } = assembly;
        match cert {
            // Revocation (possibly + bundled crypto cutoff).
            Some(cert) => {
                let target = cert.target;
                let applied = crate::owner_trust_sync::mutate_trust_state(
                    crate::owner_trust_sync::TrustStateAccess::Resident {
                        doc: Arc::clone(trust_doc),
                        engine: Arc::clone(trust_engine),
                    },
                    move |s| {
                        if s.is_revoked(target) {
                            return Ok(());
                        }
                        s.add_revocation(
                            cert,
                            now_secs,
                            harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
                        )
                    },
                )
                .await;
                match applied {
                    Ok(Ok(())) => {
                        emit("owner-devices-updated");
                        if let Some(tx) = retire_nudge {
                            let _ = tx.try_send(());
                        }
                        // The request is the ceremony's only retry source —
                        // retire it ONLY once the revocation is durably
                        // flushed. On flush failure the request stays resident;
                        // the dirty latch retries the publish+persist, and the
                        // next sweep prunes via the revoked-target predicate
                        // once the trust doc carries the revocation durably.
                        match trust_engine.flush_now().await {
                            Ok(()) => {
                                revocations_applied += 1;
                                // NO-ROLLBACK: install the bundled crypto cutoff
                                // AFTER the revoke is durable.
                                match (epoch_doc, carrier) {
                                    // Bundle + carrier ready: install (monotonic,
                                    // best-effort) and prune — a not-newer result
                                    // means the fleet already rotated, so we're done.
                                    (Some(doc), Some(c)) => {
                                        if install_quorum_epoch_doc(
                                            c,
                                            device_signing_key,
                                            self_device_id,
                                            doc,
                                        )
                                        .await
                                        {
                                            epoch_bumps_installed += 1;
                                        }
                                        completed.push(request_id);
                                    }
                                    // Bundle but NO carrier yet (boot race, Code-
                                    // Rabbit PR #461): the revoke is durable; RETAIN
                                    // the request so a later sweep installs the bump
                                    // once the carrier slot is filled. fleetEpochStale
                                    // is the interim surface — the bump is NOT dropped.
                                    (Some(_doc), None) => {
                                        tracing::info!(request = %request_id,
                                            "quorum sweep: revoke durable, bundled bump deferred until the carrier is ready; request retained");
                                    }
                                    // Revoke-only: prune.
                                    (None, _) => completed.push(request_id),
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, request = %request_id,
                                    "quorum sweep: trust flush failed; request retained until the \
                                     revocation is durable (dirty latch retries)");
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, request = %request_id,
                            "quorum sweep: assembled revocation rejected by trust state; request retained");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, request = %request_id,
                            "quorum sweep: trust mutation failed; request retained");
                    }
                }
            }
            // Standalone quorum epoch bump (no revocation). Complete only on a
            // successful install; a failed install retains the request to retry.
            None => match (epoch_doc, carrier) {
                (Some(doc), Some(c)) => {
                    if install_quorum_epoch_doc(c, device_signing_key, self_device_id, doc).await {
                        completed.push(request_id);
                        epoch_bumps_installed += 1;
                        emit("owner-devices-updated");
                    }
                }
                _ => {
                    // No carrier to install into (never happens in production —
                    // the node always carries fleet keys when it can co-sign).
                    tracing::warn!(request = %request_id,
                        "quorum sweep: epoch-bump request assembled without a carrier; retained");
                }
            },
        }
    }

    // Phase C: drop completed requests (revocations AND epoch bumps) from the
    // quorum doc.
    let pruned_count = completed.len();
    if !completed.is_empty() {
        {
            let mut doc = quorum_doc.lock().await;
            for id in &completed {
                doc.requests.remove(id);
            }
        }
        quorum_engine.notify_dirty();
        if let Err(e) = quorum_engine.flush_now().await {
            tracing::warn!(error = %e,
                "quorum sweep: quorum flush failed; dirty latch will retry");
        }
    }

    // Phase B2: apply the B-side enrollment co-sign. Flush-gated like the
    // revocation path — we only union our signature (which lets the
    // initiator assemble the enrollment cert) AFTER our Vouch is durable, so
    // the joiner is never enrolled without a ratifying vouch on record. On
    // any failure the request stays un-co-signed and the arm stays live; the
    // next sweep retries.
    let mut enroll_cosigns = 0usize;
    if let Some(ec) = enroll_cosign {
        let vouch = ec.vouch_cert;
        let applied = crate::owner_trust_sync::mutate_trust_state(
            crate::owner_trust_sync::TrustStateAccess::Resident {
                doc: Arc::clone(trust_doc),
                engine: Arc::clone(trust_engine),
            },
            move |s| s.add_vouching(vouch),
        )
        .await;
        match applied {
            Ok(Ok(())) => match trust_engine.flush_now().await {
                Ok(()) => {
                    {
                        let mut doc = quorum_doc.lock().await;
                        if let Some(req) = doc.requests.get_mut(&ec.request_id) {
                            req.signatures
                                .entry(self_hex.clone())
                                .or_insert(QuorumRequestSigs {
                                    primary_sig_hex: ec.self_sig_hex,
                                    epoch_doc_sig_hex: None,
                                });
                        }
                        // Consume the single-use arm (fresh-Hlc expired cell).
                        stamp_arm_cell(&mut doc, self_device_id, now_ms.saturating_sub(1), now_ms);
                    }
                    quorum_engine.notify_dirty();
                    if let Err(e) = quorum_engine.flush_now().await {
                        tracing::warn!(error = %e,
                            "enrollment co-sign: quorum flush failed; dirty latch will retry");
                    }
                    emit("owner-devices-updated");
                    enroll_cosigns = 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, request = %ec.request_id,
                        "enrollment co-sign: trust flush failed; not co-signed (retry next sweep)");
                }
            },
            Ok(Err(e)) => {
                tracing::warn!(error = %e, request = %ec.request_id,
                    "enrollment co-sign: add_vouching rejected; not co-signed");
            }
            Err(e) => {
                tracing::warn!(error = %e, request = %ec.request_id,
                    "enrollment co-sign: trust mutation failed; not co-signed");
            }
        }
    }

    SweepOutcome {
        doc_changed: pruned || pruned_count > 0 || enroll_cosigns > 0,
        revocations_applied,
        enrollment_cosigns: enroll_cosigns,
        epoch_bumps_installed,
    }
}

/// Cadence of the fallback expiry sweep — without it, expired requests
/// and arms would linger (consuming the request cap) whenever no inbound
/// merge arrives to nudge the task.
const QUORUM_SWEEP_INTERVAL_SECS: u64 = 60;

/// The quorum engine's `on_applied` consumer: each nudge (an inbound merge
/// that changed the doc, or the one boot tick) runs a completion sweep and
/// then tells the UI the pending-request surface changed. A 60-second
/// interval backstops TTL expiry when no merges arrive; interval ticks
/// only emit when the sweep actually changed the doc (no idle refresh
/// spam). The boot tick covers signatures that accumulated while this
/// device was offline. Exits when every nudge sender is dropped (engine
/// shutdown).
#[allow(clippy::too_many_arguments)]
pub fn spawn_quorum_applied_task(
    mut nudge_rx: tokio::sync::mpsc::Receiver<()>,
    quorum_doc: Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    quorum_engine: Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
    trust_doc: Arc<tokio::sync::Mutex<harmony_owner::state::OwnerState>>,
    trust_engine: Arc<crate::fleet_sync::FleetSyncEngine<harmony_owner::state::OwnerState>>,
    device_signing_key: ed25519_dalek::SigningKey,
    self_device_id: [u8; 16],
    emit: Arc<dyn Fn(&str) + Send + Sync>,
    retire_nudge: Option<tokio::sync::mpsc::Sender<()>>,
    // ZEB-677 S5 — the fleet-keys carrier, filled AFTER this task spawns (the
    // carrier is built later in boot). Empty until then → revoke-only sweeps.
    carrier_slot: Arc<tokio::sync::Mutex<Option<QuorumSweepCarrier>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick =
            tokio::time::interval(std::time::Duration::from_secs(QUORUM_SWEEP_INTERVAL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let from_nudge = tokio::select! {
                n = nudge_rx.recv() => match n {
                    Some(()) => true,
                    None => break,
                },
                _ = tick.tick() => false,
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let carrier = carrier_slot.lock().await.clone();
            let outcome = run_quorum_sweep_with_carrier(
                &quorum_doc,
                &quorum_engine,
                &trust_doc,
                &trust_engine,
                &device_signing_key,
                self_device_id,
                &emit,
                retire_nudge.as_ref(),
                now.as_secs(),
                now.as_millis() as u64,
                carrier.as_ref(),
            )
            .await;
            if from_nudge || outcome.doc_changed {
                emit("owner-quorum-updated");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_owner::lifecycle::{enroll_via_master, mint_owner, MintResult, RecoveryArtifact};
    use harmony_owner::pubkey_bundle::PubKeyBundle;
    use harmony_owner::state::OwnerState;
    use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;

    const NOW_SECS: u64 = 1_700_000_000;
    const NOW_MS: u64 = NOW_SECS * 1000;
    /// Default fixture expiry — far future (~2099) because the MERGE skips
    /// expired inserts against the REAL wall clock, while prune tests pass
    /// their own explicit `now_ms`. Tests that exercise expiry override it.
    const FAR_FUTURE_MS: u64 = 4_100_000_000_000;

    fn hlc(wall_ms: u64, device: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: device.to_string(),
        }
    }

    fn test_request(initiator: &str, target: &str) -> QuorumRequest {
        QuorumRequest {
            created_at: hlc(NOW_MS, initiator),
            declined_by: BTreeMap::new(),
            initiator_hex: initiator.to_string(),
            kind: QuorumRequestKind::Revocation {
                reason: "lost".to_string(),
                target_hex: target.to_string(),
                epoch_doc_cbor_hex: None,
                epoch_doc_initiator_sig_hex: None,
            },
            initiator_sigs: BTreeMap::new(),
            signatures: BTreeMap::new(),
            issued_at: NOW_SECS,
            expires_at_ms: FAR_FUTURE_MS,
        }
    }

    fn sigs(sig: &str) -> QuorumRequestSigs {
        QuorumRequestSigs {
            epoch_doc_sig_hex: None,
            primary_sig_hex: sig.to_string(),
        }
    }

    #[test]
    fn stamp_arm_cell_is_monotonic_and_disarm_expires() {
        let mut doc = QuorumReqDoc::default();
        let me = [0x11u8; 16];
        let me_hex = hex::encode(me);
        // Arm: a future single-use window.
        stamp_arm_cell(&mut doc, me, NOW_MS + ARM_WINDOW_MS, NOW_MS);
        let armed = doc.enroll_arms.get(&me_hex).expect("armed").clone();
        assert!(armed.armed_until_ms > NOW_MS);
        // Disarm at the SAME wall-ms: must still win LWW via logical bump.
        stamp_arm_cell(&mut doc, me, NOW_MS - 1, NOW_MS);
        let disarmed = doc.enroll_arms.get(&me_hex).expect("cell present").clone();
        assert!(
            disarmed.armed_until_ms <= NOW_MS,
            "disarm writes an already-expired cell"
        );
        assert!(
            disarmed.set_at.is_strictly_newer_than(&armed.set_at),
            "disarm must win LWW over the armed cell"
        );
        // Never deleted — a stale re-merge cannot resurrect the window.
        assert!(doc.enroll_arms.contains_key(&me_hex));
    }

    #[test]
    fn stamp_arm_cell_advances_past_saturated_logical() {
        let mut doc = QuorumReqDoc::default();
        let me = [0x11u8; 16];
        let me_hex = hex::encode(me);
        // Prime a cell already at the logical ceiling.
        doc.enroll_arms.insert(
            me_hex.clone(),
            EnrollArm {
                set_at: Hlc {
                    wall_ms: NOW_MS,
                    logical: u32::MAX,
                    device_id: me_hex.clone(),
                },
                armed_until_ms: NOW_MS + ARM_WINDOW_MS,
            },
        );
        // A same-millisecond restamp must still be strictly newer (no u32
        // overflow panic / wrap) by rolling the wall clock forward a tick.
        let prev = doc.enroll_arms.get(&me_hex).unwrap().set_at.clone();
        stamp_arm_cell(&mut doc, me, NOW_MS - 1, NOW_MS);
        let next = doc.enroll_arms.get(&me_hex).unwrap().set_at.clone();
        assert!(next.is_strictly_newer_than(&prev));
        assert_eq!((next.wall_ms, next.logical), (NOW_MS + 1, 0));
    }

    #[test]
    fn consumed_arm_tombstone_survives_older_arm_remerge() {
        let me = [0x11u8; 16];
        let me_hex = hex::encode(me);
        let arm_ms = NOW_MS;
        // Replica X: armed.
        let mut x = QuorumReqDoc::default();
        stamp_arm_cell(&mut x, me, arm_ms + ARM_WINDOW_MS, arm_ms);
        let old_arm = x.clone();

        // Replica Y: same arm, then consumed (fresh-Hlc expired tombstone).
        let mut y = old_arm.clone();
        stamp_arm_cell(&mut y, me, arm_ms - 1, arm_ms); // consume

        // Y's sweep runs shortly after: the tombstone is RETAINED (within the
        // merge horizon), NOT pruned — otherwise the next step resurrects it.
        let trust = OwnerState::default();
        prune_settled_requests(&mut y, &trust, arm_ms + 1);
        assert!(
            y.enroll_arms
                .get(&me_hex)
                .is_some_and(|a| a.armed_until_ms <= arm_ms),
            "consumed tombstone must survive the immediate post-consume sweep"
        );

        // The older, still-live arm from X re-merges into Y. LWW must keep Y's
        // newer tombstone — the single-use window does NOT come back.
        merge_quorum_remote_into_local(&mut y, old_arm);
        assert!(
            y.enroll_arms
                .get(&me_hex)
                .is_some_and(|a| a.armed_until_ms <= arm_ms),
            "an older live arm must not resurrect a consumed single-use window"
        );
    }

    #[test]
    fn enrollment_request_kind_round_trips_in_doc() {
        let mut doc = QuorumReqDoc::default();
        doc.requests.insert(
            "ab".repeat(8),
            QuorumRequest {
                created_at: hlc(NOW_MS, "aa"),
                declined_by: BTreeMap::new(),
                initiator_hex: "aa".repeat(8),
                kind: QuorumRequestKind::Enrollment {
                    joiner_device_id_hex: "cc".repeat(8),
                    joiner_pubkeys_cbor_hex: "dd".repeat(4),
                },
                initiator_sigs: BTreeMap::new(),
                signatures: BTreeMap::new(),
                issued_at: NOW_SECS,
                expires_at_ms: FAR_FUTURE_MS,
            },
        );
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&doc).expect("encode");
        let back: QuorumReqDoc =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(doc, back);
        assert!(matches!(
            back.requests.values().next().map(|r| &r.kind),
            Some(QuorumRequestKind::Enrollment { .. })
        ));
    }

    fn test_mint(now: u64) -> (OwnerState, RecoveryArtifact) {
        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(now).unwrap();
        (state, recovery_artifact)
    }

    fn enroll_device(state: &mut OwnerState, artifact: &RecoveryArtifact, now: u64) -> [u8; 16] {
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let pkb = PubKeyBundle::classical_only(sk.verifying_key().to_bytes());
        let res =
            enroll_via_master(state, artifact, &sk, pkb, now, DEFAULT_ACTIVE_WINDOW_SECS).unwrap();
        let id = res.enrollment_cert.device_id;
        state
            .add_enrollment(res.enrollment_cert, now, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        id
    }

    #[test]
    fn doc_round_trips_and_omits_empty_maps() {
        let mut doc = QuorumReqDoc::default();
        let encoded_empty = crate::owner_state_crypto::canonical_cbor_encode(&doc).unwrap();
        // An empty doc is an empty CBOR map (both fields skipped).
        assert_eq!(encoded_empty, vec![0xa0]);
        doc.requests.insert(
            "aa".repeat(16),
            test_request(&"11".repeat(16), &"22".repeat(16)),
        );
        doc.enroll_arms.insert(
            "11".repeat(16),
            EnrollArm {
                set_at: hlc(NOW_MS, "a"),
                armed_until_ms: NOW_MS + 900_000,
            },
        );
        let encoded = crate::owner_state_crypto::canonical_cbor_encode(&doc).unwrap();
        let decoded: QuorumReqDoc =
            crate::owner_state_crypto::canonical_cbor_decode(&encoded).unwrap();
        assert_eq!(decoded, doc);
    }

    #[test]
    fn merge_unions_signatures_from_disjoint_remotes() {
        let id = "ab".repeat(16);
        let base = test_request(&"11".repeat(16), &"22".repeat(16));
        let mut local = QuorumReqDoc::default();
        local.requests.insert(id.clone(), base.clone());

        let mut remote_b = QuorumReqDoc::default();
        let mut req_b = base.clone();
        req_b.signatures.insert("bb".repeat(16), sigs("b-sig"));
        remote_b.requests.insert(id.clone(), req_b);

        let mut remote_c = QuorumReqDoc::default();
        let mut req_c = base.clone();
        req_c.signatures.insert("cc".repeat(16), sigs("c-sig"));
        // Value is opaque here — this test exercises the raw CRDT union, not
        // signature verification (that lives in `verified_decliners`).
        req_c
            .declined_by
            .insert("dd".repeat(16), "unverified-sig".to_string());
        remote_c.requests.insert(id.clone(), req_c);

        assert!(merge_quorum_remote_into_local(&mut local, remote_b).changed);
        assert!(merge_quorum_remote_into_local(&mut local, remote_c).changed);
        let merged = &local.requests[&id];
        assert_eq!(merged.signatures.len(), 2);
        assert_eq!(merged.declined_by.len(), 1);
    }

    #[test]
    fn merge_is_commutative_on_conflicting_values() {
        // Two replicas can only hold DIFFERENT values for the same signature
        // key via tampering: an honest ed25519 co-signature over the fixed
        // pair payload is byte-identical everywhere (RFC 8032 deterministic
        // nonces), so honest replicas never conflict on a key. When a
        // conflict does arise, the union must converge to the SAME value
        // regardless of merge order (smaller value wins). The tampered value
        // is harmless — it fails `verify_with_tag` at assembly and can never
        // forge a revocation.
        let id = "ab".repeat(16);
        let base = test_request(&"11".repeat(16), &"22".repeat(16));
        let key = "bb".repeat(16);
        let doc_with = |sig: &str| {
            let mut d = QuorumReqDoc::default();
            let mut r = base.clone();
            r.signatures.insert(key.clone(), sigs(sig));
            d.requests.insert(id.clone(), r);
            d
        };

        // merge(original, swap) and merge(swap, original) must agree.
        let mut ab = doc_with("original");
        merge_quorum_remote_into_local(&mut ab, doc_with("attacker-swap"));
        let mut ba = doc_with("attacker-swap");
        merge_quorum_remote_into_local(&mut ba, doc_with("original"));

        assert_eq!(
            ab.requests[&id].signatures, ba.requests[&id].signatures,
            "union must be commutative regardless of arrival order"
        );
        // Deterministic tiebreak: smaller value wins ("attacker-swap" < "original").
        assert_eq!(
            ab.requests[&id].signatures[&key].primary_sig_hex,
            "attacker-swap"
        );
        // Identical values (the honest case) never register as a change.
        let mut same = doc_with("original");
        assert!(!merge_quorum_remote_into_local(&mut same, doc_with("original")).changed);
    }

    #[test]
    fn merge_drops_identity_mutated_duplicate() {
        let id = "ab".repeat(16);
        let base = test_request(&"11".repeat(16), &"22".repeat(16));
        let mut local = QuorumReqDoc::default();
        local.requests.insert(id.clone(), base.clone());

        let mut remote = QuorumReqDoc::default();
        let mut req = base.clone();
        req.kind = QuorumRequestKind::Revocation {
            reason: "compromised".to_string(),
            target_hex: "33".repeat(16),
            epoch_doc_cbor_hex: None,
            epoch_doc_initiator_sig_hex: None,
        };
        req.signatures.insert("bb".repeat(16), sigs("sig"));
        remote.requests.insert(id.clone(), req);

        let outcome = merge_quorum_remote_into_local(&mut local, remote);
        assert!(!outcome.changed);
        assert!(local.requests[&id].signatures.is_empty());
    }

    #[test]
    fn merge_caps_inserts_and_map_growth() {
        // Over-cap request never inserts.
        let mut local = QuorumReqDoc::default();
        let mut fat = test_request(&"11".repeat(16), &"22".repeat(16));
        for i in 0..=MAX_QUORUM_SIG_ENTRIES {
            fat.signatures.insert(format!("{i:032x}"), sigs("s"));
        }
        let mut remote = QuorumReqDoc::default();
        remote.requests.insert("aa".repeat(16), fat);
        assert!(!merge_quorum_remote_into_local(&mut local, remote).changed);
        assert!(local.requests.is_empty());

        // Request-count cap holds.
        let mut full = QuorumReqDoc::default();
        for i in 0..MAX_QUORUM_REQUESTS {
            full.requests.insert(
                format!("{i:032x}"),
                test_request(&"11".repeat(16), &"22".repeat(16)),
            );
        }
        let mut one_more = QuorumReqDoc::default();
        one_more.requests.insert(
            "ff".repeat(16),
            test_request(&"11".repeat(16), &"22".repeat(16)),
        );
        assert!(!merge_quorum_remote_into_local(&mut full, one_more).changed);
        assert_eq!(full.requests.len(), MAX_QUORUM_REQUESTS);
    }

    #[test]
    fn merge_never_inserts_expired_requests() {
        // The merge checks expiry against the REAL wall clock, so a stale
        // peer republishing an expired request can't ping-pong it back
        // after the local sweep pruned it.
        let mut local = QuorumReqDoc::default();
        let mut remote = QuorumReqDoc::default();
        let mut expired = test_request(&"11".repeat(16), &"22".repeat(16));
        expired.expires_at_ms = NOW_MS; // 2023 — long past by the real clock
        remote.requests.insert("aa".repeat(16), expired);
        assert!(!merge_quorum_remote_into_local(&mut local, remote).changed);
        assert!(local.requests.is_empty());
    }

    #[test]
    fn arm_cells_are_lww_on_set_at() {
        let armer = "11".repeat(16);
        let mut local = QuorumReqDoc::default();
        local.enroll_arms.insert(
            armer.clone(),
            EnrollArm {
                set_at: hlc(100, "a"),
                armed_until_ms: 1_000,
            },
        );
        // Newer remote wins.
        let mut remote = QuorumReqDoc::default();
        remote.enroll_arms.insert(
            armer.clone(),
            EnrollArm {
                set_at: hlc(200, "a"),
                armed_until_ms: 2_000,
            },
        );
        assert!(merge_quorum_remote_into_local(&mut local, remote).changed);
        assert_eq!(local.enroll_arms[&armer].armed_until_ms, 2_000);
        // Older remote loses.
        let mut stale = QuorumReqDoc::default();
        stale.enroll_arms.insert(
            armer.clone(),
            EnrollArm {
                set_at: hlc(50, "a"),
                armed_until_ms: 9_000,
            },
        );
        assert!(!merge_quorum_remote_into_local(&mut local, stale).changed);
        assert_eq!(local.enroll_arms[&armer].armed_until_ms, 2_000);
    }

    #[test]
    fn prune_removes_expired_and_revoked_target_keeps_declined() {
        let (mut trust, artifact) = test_mint(NOW_SECS);
        let target = enroll_device(&mut trust, &artifact, NOW_SECS + 1);
        let victim = enroll_device(&mut trust, &artifact, NOW_SECS + 2);
        let target_hex = hex::encode(target);
        let victim_hex = hex::encode(victim);

        let mut doc = QuorumReqDoc::default();
        // Expired request.
        let mut expired = test_request(&"11".repeat(16), &target_hex);
        expired.expires_at_ms = NOW_MS - 1;
        doc.requests.insert("aa".repeat(16), expired);
        // Live request against a soon-revoked target.
        doc.requests
            .insert("bb".repeat(16), test_request(&"11".repeat(16), &victim_hex));
        // Declined-but-unexpired request: retained. (Prune ignores
        // `declined_by` entirely — retention is by expiry/revoked-target — so
        // the signature value is irrelevant here.)
        let mut declined = test_request(&"11".repeat(16), &target_hex);
        declined
            .declined_by
            .insert("22".repeat(16), "unverified-sig".to_string());
        doc.requests.insert("cc".repeat(16), declined);
        // Malformed target: dropped.
        doc.requests.insert(
            "dd".repeat(16),
            test_request(&"11".repeat(16), "zz-not-hex"),
        );
        // Arm expired PAST the merge horizon is dropped; live arm kept. (A
        // recently-expired tombstone within the horizon is retained — see
        // `consumed_arm_tombstone_survives_older_arm_remerge`.)
        doc.enroll_arms.insert(
            "11".repeat(16),
            EnrollArm {
                set_at: hlc(1, "a"),
                armed_until_ms: NOW_MS - QUORUM_ARM_HORIZON_MS - 1,
            },
        );
        doc.enroll_arms.insert(
            "22".repeat(16),
            EnrollArm {
                set_at: hlc(1, "b"),
                armed_until_ms: NOW_MS + 1,
            },
        );

        let rev = RevocationCert::sign_master(
            &artifact.master_signing_key(),
            artifact.master_pubkey_bundle(),
            victim,
            NOW_SECS + 3,
            harmony_owner::certs::RevocationReason::Lost,
        )
        .unwrap();
        trust
            .add_revocation(rev, NOW_SECS + 3, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();

        assert!(prune_settled_requests(&mut doc, &trust, NOW_MS));
        assert_eq!(
            doc.requests.keys().cloned().collect::<Vec<_>>(),
            vec!["cc".repeat(16)],
            "only the declined-but-unexpired request survives"
        );
        assert_eq!(
            doc.enroll_arms.keys().cloned().collect::<Vec<_>>(),
            vec!["22".repeat(16)]
        );
        // Idempotent second sweep.
        assert!(!prune_settled_requests(&mut doc, &trust, NOW_MS));
    }

    #[test]
    fn pair_payload_is_order_invariant_and_reason_sensitive() {
        let owner = [1u8; 16];
        let target = [2u8; 16];
        let a = [3u8; 16];
        let b = [4u8; 16];
        let p1 = revocation_pair_payload(
            owner,
            target,
            NOW_SECS,
            &harmony_owner::certs::RevocationReason::Lost,
            a,
            b,
        )
        .unwrap();
        let p2 = revocation_pair_payload(
            owner,
            target,
            NOW_SECS,
            &harmony_owner::certs::RevocationReason::Lost,
            b,
            a,
        )
        .unwrap();
        assert_eq!(p1, p2, "pair payload must not depend on argument order");
        let p3 = revocation_pair_payload(
            owner,
            target,
            NOW_SECS,
            &harmony_owner::certs::RevocationReason::Compromised,
            a,
            b,
        )
        .unwrap();
        assert_ne!(p1, p3);
    }

    // ── Task 6: two-engine ceremony integration tests ───────────────────

    /// One device's half of the replicated pair: both datasets resident.
    struct DevRig {
        quorum_doc: Arc<tokio::sync::Mutex<QuorumReqDoc>>,
        quorum_engine: Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
        trust_doc: Arc<tokio::sync::Mutex<OwnerState>>,
        trust_engine: Arc<crate::fleet_sync::FleetSyncEngine<OwnerState>>,
    }

    struct QuorumPair {
        a: DevRig,
        b: DevRig,
        _dir: tempfile::TempDir,
    }

    /// Two devices, BOTH datasets (trust + quorum) crossed over in-memory
    /// channels sharing one CAS + KeyTree — the trust-sync `TrustPair`
    /// harness extended for the ceremony (spec §10: donor trust-sync tests).
    fn spawn_quorum_pair(seeded_trust: OwnerState) -> QuorumPair {
        use crate::content_store::{ContentStore, InMemoryStub};
        use crate::fleet_sync::{FleetSyncConfig, FleetSyncEngine};
        use crate::owner_state_crypto::KeyTree;
        use tokio::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let kt = Arc::new(KeyTree::derive(&[0x77u8; 32]).expect("kt"));
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;

        // Crossed channel pair builder: returns (a_pub_tx, a_sub_rx,
        // b_pub_tx, b_sub_rx) where a's publishes feed b's subscriber and
        // vice versa.
        let crossed = || {
            let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
            let (a_to_b_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);
            tokio::spawn(async move {
                while let Some(bytes) = a_pub_rx.recv().await {
                    let _ = a_to_b_tx.send(bytes).await;
                }
            });
            let (b_pub_tx, mut b_pub_rx) = mpsc::channel::<Vec<u8>>(64);
            let (b_to_a_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(64);
            tokio::spawn(async move {
                while let Some(bytes) = b_pub_rx.recv().await {
                    let _ = b_to_a_tx.send(bytes).await;
                }
            });
            (a_pub_tx, a_sub_rx, b_pub_tx, b_sub_rx)
        };
        let (tq_a_pub, tq_a_sub, tq_b_pub, tq_b_sub) = crossed();
        let (qq_a_pub, qq_a_sub, qq_b_pub, qq_b_sub) = crossed();

        let mk_dev = |name: &str,
                      trust_seed: OwnerState,
                      t_pub: mpsc::Sender<Vec<u8>>,
                      t_sub: mpsc::Receiver<Vec<u8>>,
                      q_pub: mpsc::Sender<Vec<u8>>,
                      q_sub: mpsc::Receiver<Vec<u8>>| {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
            let trust_doc = Arc::new(tokio::sync::Mutex::new(trust_seed));
            let trust_engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
                keys: crate::owner_state_crypto::FleetKeySet::new(Arc::clone(&kt)),
                device_id: name.to_string(),
                state: Arc::clone(&trust_doc),
                merger: crate::owner_trust_sync::trust_merger(),
                replay_tracker: Arc::new(tokio::sync::Mutex::new(
                    harmony_crdt_sync::ReplayTracker::new(name.to_string()),
                )),
                content_store: Arc::clone(&store),
                publisher_tx: t_pub,
                subscriber_rx: t_sub,
                persist: Arc::new(crate::owner_trust_sync::TrustPersist {
                    identity_dir: dir.path().join(name),
                    replay_path: dir.path().join(format!("{name}-trust-replay.cbor")),
                }),
                lookup_key_tag: crate::owner_trust_sync::OWNER_TRUST_LOOKUP_TAG,
                debounce_ms: 50,
                publish_seen: true,
                on_applied: None,
                sibling_acks: Arc::new(tokio::sync::Mutex::new(
                    harmony_crdt_sync::MonotoneMap::new(),
                )),
                adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
            }));
            let quorum_doc = Arc::new(tokio::sync::Mutex::new(QuorumReqDoc::default()));
            let quorum_engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
                keys: crate::owner_state_crypto::FleetKeySet::new(Arc::clone(&kt)),
                device_id: name.to_string(),
                state: Arc::clone(&quorum_doc),
                merger: quorum_merger(),
                replay_tracker: Arc::new(tokio::sync::Mutex::new(
                    harmony_crdt_sync::ReplayTracker::new(name.to_string()),
                )),
                content_store: Arc::clone(&store),
                publisher_tx: q_pub,
                subscriber_rx: q_sub,
                persist: Arc::new(QuorumPersist {
                    doc_path: dir.path().join(format!("{name}-quorum.cbor")),
                    replay_path: dir.path().join(format!("{name}-quorum-replay.cbor")),
                }),
                lookup_key_tag: OWNER_QUORUM_LOOKUP_TAG,
                debounce_ms: 50,
                publish_seen: true,
                on_applied: None,
                sibling_acks: Arc::new(tokio::sync::Mutex::new(
                    harmony_crdt_sync::MonotoneMap::new(),
                )),
                adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
            }));
            DevRig {
                quorum_doc,
                quorum_engine,
                trust_doc,
                trust_engine,
            }
        };

        let a = mk_dev(
            "dev-a",
            seeded_trust.clone(),
            tq_a_pub,
            tq_a_sub,
            qq_a_pub,
            qq_a_sub,
        );
        let b = mk_dev(
            "dev-b",
            seeded_trust,
            tq_b_pub,
            tq_b_sub,
            qq_b_pub,
            qq_b_sub,
        );
        QuorumPair { a, b, _dir: dir }
    }

    async fn shutdown_pair(pair: QuorumPair) {
        let _ = pair.a.quorum_engine.shutdown().await;
        let _ = pair.a.trust_engine.shutdown().await;
        let _ = pair.b.quorum_engine.shutdown().await;
        let _ = pair.b.trust_engine.shutdown().await;
    }

    /// Poll until `pred` returns true (5 s deadline, donor loop shape).
    async fn wait_for<F, Fut>(what: &str, mut pred: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if pred().await {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for: {what}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn two_engines_full_ceremony_revocation_lands_fleet_wide() {
        let base = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 60;
        let base_ms = base * 1000;
        let f = sweep_fleet_at(base);
        let pair = spawn_quorum_pair(f.trust.clone());

        // A initiates (the IPC body minus NodeState: plan + insert + nudge).
        let (id, req) = crate::owner_quorum_commands::plan_quorum_revocation_request(
            &f.trust,
            &f.a_sk,
            false,
            &f.c_vk_hex,
            "lost",
            base + 10,
            base_ms + 10_000,
            [0xaa; 16],
            None,
        )
        .expect("plan");
        pair.a
            .quorum_doc
            .lock()
            .await
            .requests
            .insert(id.clone(), req);
        pair.a.quorum_engine.notify_dirty();

        // The request replicates to B.
        let id_for_b = id.clone();
        let b_doc = Arc::clone(&pair.b.quorum_doc);
        wait_for("request to reach B", move || {
            let doc = Arc::clone(&b_doc);
            let id = id_for_b.clone();
            async move { doc.lock().await.requests.contains_key(&id) }
        })
        .await;

        // B approves (cosign core against B's resident docs).
        {
            let trust_b = pair.b.trust_doc.lock().await.clone();
            let mut doc_b = pair.b.quorum_doc.lock().await;
            let signed = crate::owner_quorum_commands::cosign_request_core(
                &mut doc_b,
                &trust_b,
                &f.b_sk,
                f.b_id,
                &id,
                base_ms + 20_000,
            )
            .expect("cosign");
            assert!(signed);
        }
        pair.b.quorum_engine.notify_dirty();

        // B's signature replicates back to A.
        let id_for_a = id.clone();
        let a_doc = Arc::clone(&pair.a.quorum_doc);
        let b_hex = hex::encode(f.b_id);
        wait_for("co-signature to reach A", move || {
            let doc = Arc::clone(&a_doc);
            let id = id_for_a.clone();
            let b_hex = b_hex.clone();
            async move {
                doc.lock()
                    .await
                    .requests
                    .get(&id)
                    .is_some_and(|r| r.signatures.contains_key(&b_hex))
            }
        })
        .await;

        // A's completion sweep (what the applied task runs on the nudge).
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let outcome = run_quorum_sweep(
            &pair.a.quorum_doc,
            &pair.a.quorum_engine,
            &pair.a.trust_doc,
            &pair.a.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::clone(&events)),
            None,
            base + 30,
            base_ms + 30_000,
        )
        .await;
        assert_eq!(outcome.revocations_applied, 1);
        assert!(pair.a.trust_doc.lock().await.is_revoked(f.c_id));
        assert!(pair.a.quorum_doc.lock().await.requests.is_empty());

        // The revocation lands fleet-wide through TRUST replication.
        let b_trust = Arc::clone(&pair.b.trust_doc);
        let c_id = f.c_id;
        wait_for("revocation to reach B's trust doc", move || {
            let doc = Arc::clone(&b_trust);
            async move { doc.lock().await.is_revoked(c_id) }
        })
        .await;

        // B's own sweep prunes its copy via the revoked-target predicate.
        let outcome_b = run_quorum_sweep(
            &pair.b.quorum_doc,
            &pair.b.quorum_engine,
            &pair.b.trust_doc,
            &pair.b.trust_engine,
            &f.b_sk,
            f.b_id,
            &collecting_emit(Arc::new(std::sync::Mutex::new(Vec::new()))),
            None,
            base + 40,
            base_ms + 40_000,
        )
        .await;
        assert_eq!(outcome_b.revocations_applied, 0);
        assert!(pair.b.quorum_doc.lock().await.requests.is_empty());

        shutdown_pair(pair).await;
    }

    /// Full enrollment ceremony across two real engines: B arms, A opens the
    /// request through the LIVE `LiveQuorumEnrollPort`, the request replicates,
    /// B's sweep auto-co-signs + vouches + consumes its arm, the co-signature
    /// replicates back, and A's port assembles a valid K=2 cert. Exercises the
    /// production port + sweep wiring (not just the pure helpers).
    #[tokio::test]
    async fn two_engines_full_ceremony_enrollment_lands() {
        use crate::owner_quorum_enroll::QuorumEnrollPort;
        let base = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 60;
        let base_ms = base * 1000;
        let f = sweep_fleet_at(base);
        let pair = spawn_quorum_pair(f.trust.clone());

        let joiner_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let joiner_pk = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let joiner_id = joiner_pk.identity_hash();

        // B arms; the arm replicates to A.
        {
            let mut doc_b = pair.b.quorum_doc.lock().await;
            stamp_arm_cell(
                &mut doc_b,
                f.b_id,
                base_ms + 10_000 + ARM_WINDOW_MS,
                base_ms + 10_000,
            );
        }
        pair.b.quorum_engine.notify_dirty();
        let a_doc = Arc::clone(&pair.a.quorum_doc);
        let b_hex = hex::encode(f.b_id);
        wait_for("arm to reach A", move || {
            let doc = Arc::clone(&a_doc);
            let b_hex = b_hex.clone();
            async move { doc.lock().await.enroll_arms.contains_key(&b_hex) }
        })
        .await;

        // A opens the enrollment request via the LIVE port.
        let port = crate::owner_quorum_enroll::LiveQuorumEnrollPort::new(
            Arc::clone(&pair.a.quorum_doc),
            Arc::clone(&pair.a.quorum_engine),
            Arc::clone(&pair.a.trust_doc),
            f.a_sk.clone(),
            f.a_id,
        );
        let rid = port
            .open_enrollment_request(joiner_id, joiner_pk.clone(), base + 10)
            .await
            .expect("open");

        // The request replicates to B.
        let b_doc = Arc::clone(&pair.b.quorum_doc);
        let rid_b = rid.clone();
        wait_for("request to reach B", move || {
            let doc = Arc::clone(&b_doc);
            let rid = rid_b.clone();
            async move { doc.lock().await.requests.contains_key(&rid) }
        })
        .await;

        // B's sweep auto-co-signs (armed), vouches the joiner, consumes the arm.
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let b_outcome = run_quorum_sweep(
            &pair.b.quorum_doc,
            &pair.b.quorum_engine,
            &pair.b.trust_doc,
            &pair.b.trust_engine,
            &f.b_sk,
            f.b_id,
            &collecting_emit(Arc::clone(&events)),
            None,
            base + 11,
            base_ms + 11_000,
        )
        .await;
        assert_eq!(b_outcome.enrollment_cosigns, 1, "B co-signed");

        // A's port polls until the co-signature replicates, then assembles.
        let cert = port
            .await_cosign_and_assemble(rid.clone(), std::time::Duration::from_secs(5))
            .await
            .expect("assemble");
        let a_cert = f.trust.enrollments.get(&f.a_id).unwrap().clone();
        let b_cert = f.trust.enrollments.get(&f.b_id).unwrap().clone();
        cert.verify_quorum_with_signers(&[a_cert, b_cert], base + 12)
            .expect("valid quorum enrollment cert");
        assert_eq!(cert.device_id, joiner_id);

        // A applies the enrollment; B's vouch (from its sweep) replicates in,
        // so the joiner reaches Full (N=1 sibling vouch).
        {
            let mut trust_a = pair.a.trust_doc.lock().await;
            trust_a
                .add_enrollment(cert, base + 12, DEFAULT_ACTIVE_WINDOW_SECS)
                .expect("enroll joiner");
        }
        pair.a.trust_engine.notify_dirty();
        let a_trust = Arc::clone(&pair.a.trust_doc);
        wait_for("B's vouch to reach A", move || {
            let t = Arc::clone(&a_trust);
            async move {
                t.lock()
                    .await
                    .vouching
                    .vouches_for(joiner_id)
                    .any(|v| v.signer == f.b_id)
            }
        })
        .await;

        shutdown_pair(pair).await;
    }

    #[tokio::test]
    async fn two_engines_decline_tombstones_and_expiry_prunes() {
        let base = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 60;
        let base_ms = base * 1000;
        let f = sweep_fleet_at(base);
        let pair = spawn_quorum_pair(f.trust.clone());

        let (id, req) = crate::owner_quorum_commands::plan_quorum_revocation_request(
            &f.trust,
            &f.a_sk,
            false,
            &f.c_vk_hex,
            "compromised",
            base + 10,
            base_ms + 10_000,
            [0xbb; 16],
            None,
        )
        .expect("plan");
        let expires_at_ms = req.expires_at_ms;
        pair.a
            .quorum_doc
            .lock()
            .await
            .requests
            .insert(id.clone(), req);
        pair.a.quorum_engine.notify_dirty();

        let id_for_b = id.clone();
        let b_doc = Arc::clone(&pair.b.quorum_doc);
        wait_for("request to reach B", move || {
            let doc = Arc::clone(&b_doc);
            let id = id_for_b.clone();
            async move { doc.lock().await.requests.contains_key(&id) }
        })
        .await;

        // B declines.
        {
            let mut doc_b = pair.b.quorum_doc.lock().await;
            assert!(crate::owner_quorum_commands::decline_request_core(
                &mut doc_b, &f.trust, &f.b_sk, f.b_id, &id,
            )
            .expect("decline"));
        }
        pair.b.quorum_engine.notify_dirty();

        // The tombstone reaches A.
        let id_for_a = id.clone();
        let a_doc = Arc::clone(&pair.a.quorum_doc);
        wait_for("decline to reach A", move || {
            let doc = Arc::clone(&a_doc);
            let id = id_for_a.clone();
            async move {
                doc.lock()
                    .await
                    .requests
                    .get(&id)
                    .is_some_and(|r| !r.declined_by.is_empty())
            }
        })
        .await;

        // A's sweep never assembles a declined request; it stays resident
        // (UI-dead) until expiry.
        let outcome = run_quorum_sweep(
            &pair.a.quorum_doc,
            &pair.a.quorum_engine,
            &pair.a.trust_doc,
            &pair.a.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::new(std::sync::Mutex::new(Vec::new()))),
            None,
            base + 30,
            base_ms + 30_000,
        )
        .await;
        assert_eq!(outcome.revocations_applied, 0);
        assert!(!pair.a.trust_doc.lock().await.is_revoked(f.c_id));
        assert!(pair.a.quorum_doc.lock().await.requests.contains_key(&id));

        // Past expiry the sweep prunes it.
        let outcome2 = run_quorum_sweep(
            &pair.a.quorum_doc,
            &pair.a.quorum_engine,
            &pair.a.trust_doc,
            &pair.a.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::new(std::sync::Mutex::new(Vec::new()))),
            None,
            base + 40,
            expires_at_ms + 1,
        )
        .await;
        assert!(outcome2.doc_changed);
        assert!(pair.a.quorum_doc.lock().await.requests.is_empty());

        shutdown_pair(pair).await;
    }

    #[tokio::test]
    async fn duplicate_request_after_completion_prunes_without_reapply() {
        // Initiator-crash retry shape: a prior ceremony already revoked
        // the target when a stale duplicate request resurfaces (e.g. the
        // initiator died between apply and prune, or the user re-requested
        // against a sibling's not-yet-converged view). The sweep prunes it
        // via the revoked-target predicate without a second add_revocation.
        let f = sweep_fleet();
        let (doc, id) = planned_and_cosigned(&f);
        // Complete the ceremony out-of-band: assemble the pending request
        // and apply it to the trust state the sweep will see.
        let mut trust_revoked = f.trust.clone();
        let assembled = super::try_assemble(&f.trust, &f.a_sk, f.a_id, &doc.requests[&id])
            .expect("assemble")
            .cert
            .expect("revocation cert");
        trust_revoked
            .add_revocation(assembled, NOW_SECS + 30, DEFAULT_ACTIVE_WINDOW_SECS)
            .expect("apply");
        assert!(trust_revoked.is_revoked(f.c_id));

        // The doc still carries the (now stale) pending request.
        let rig = sweep_rig(trust_revoked, doc);
        let outcome = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::new(std::sync::Mutex::new(Vec::new()))),
            None,
            NOW_SECS + 60,
            NOW_MS + 60_000,
        )
        .await;
        assert_eq!(
            outcome.revocations_applied, 0,
            "already-revoked target must not re-apply"
        );
        assert!(outcome.doc_changed, "stale duplicate pruned");
        assert!(rig.quorum_doc.lock().await.requests.is_empty());
        let _ = rig.quorum_engine.shutdown().await;
        let _ = rig.trust_engine.shutdown().await;
    }

    // ── Task 3: completion-sweep tests ──────────────────────────────────

    struct SweepFleet {
        trust: OwnerState,
        a_sk: ed25519_dalek::SigningKey,
        a_id: [u8; 16],
        b_sk: ed25519_dalek::SigningKey,
        b_id: [u8; 16],
        c_id: [u8; 16],
        c_vk_hex: String,
    }

    /// Three master-enrolled devices with fresh liveness: A (initiator),
    /// B (cosigner), C (target). Fixed epoch (fine for single-rig sweeps,
    /// which pass explicit `now` everywhere).
    fn sweep_fleet() -> SweepFleet {
        sweep_fleet_at(NOW_SECS)
    }

    /// Same fleet anchored at an arbitrary `base` — the two-engine tests
    /// use real-clock-relative time because the REPLICATION merge path
    /// (`merge_trust_remote_into_local`) validates with the real wall
    /// clock: a fixed-2023 fixture's liveness fails the 90-day
    /// active-window check there and the quorum revocation is dropped.
    fn sweep_fleet_at(base: u64) -> SweepFleet {
        let MintResult {
            mut state,
            recovery_artifact,
            device_signing_key: a_sk,
        } = mint_owner(base).expect("mint");
        let a_id = crate::owner_state::device_id_from_signing_key(&a_sk);
        let owner_id = state.owner_id;
        let mut enroll = |now: u64| {
            let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
            let res = enroll_via_master(
                &state,
                &recovery_artifact,
                &sk,
                PubKeyBundle::classical_only(sk.verifying_key().to_bytes()),
                now,
                DEFAULT_ACTIVE_WINDOW_SECS,
            )
            .expect("enroll");
            let id = res.enrollment_cert.device_id;
            state
                .add_enrollment(res.enrollment_cert, now, DEFAULT_ACTIVE_WINDOW_SECS)
                .expect("add enrollment");
            (sk, id)
        };
        let (b_sk, b_id) = enroll(base + 1);
        let (c_sk, c_id) = enroll(base + 2);
        let c_vk_hex = hex::encode(c_sk.verifying_key().to_bytes());
        for sk in [&a_sk, &b_sk, &c_sk] {
            state
                .add_liveness(
                    harmony_owner::certs::LivenessCert::sign(sk, owner_id, base + 3).unwrap(),
                )
                .expect("liveness");
        }
        SweepFleet {
            trust: state,
            a_sk,
            a_id,
            b_sk,
            b_id,
            c_id,
            c_vk_hex,
        }
    }

    type TrustEngine = crate::fleet_sync::FleetSyncEngine<OwnerState>;
    type QuorumEngine = crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>;

    struct SweepRig {
        quorum_doc: Arc<tokio::sync::Mutex<QuorumReqDoc>>,
        quorum_engine: Arc<QuorumEngine>,
        trust_doc: Arc<tokio::sync::Mutex<OwnerState>>,
        trust_engine: Arc<TrustEngine>,
        _dir: tempfile::TempDir,
    }

    /// Single-device engine rig with drained publish channels — enough for
    /// sweep tests (replication itself is the two-engine tests below).
    fn sweep_rig(trust: OwnerState, quorum: QuorumReqDoc) -> SweepRig {
        use crate::content_store::{ContentStore, InMemoryStub};
        use crate::fleet_sync::{FleetSyncConfig, FleetSyncEngine};
        use crate::owner_state_crypto::KeyTree;
        use tokio::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("trust")).unwrap();
        let kt = Arc::new(KeyTree::derive(&[0x55u8; 32]).expect("kt"));
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let trust_doc = Arc::new(tokio::sync::Mutex::new(trust));
        let quorum_doc = Arc::new(tokio::sync::Mutex::new(quorum));

        let (t_out, mut t_drain) = mpsc::channel::<Vec<u8>>(64);
        let (_t_in_tx, t_in) = mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move { while t_drain.recv().await.is_some() {} });
        let trust_engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
            keys: crate::owner_state_crypto::FleetKeySet::new(Arc::clone(&kt)),
            device_id: "dev-a".to_string(),
            state: Arc::clone(&trust_doc),
            merger: crate::owner_trust_sync::trust_merger(),
            replay_tracker: Arc::new(tokio::sync::Mutex::new(
                harmony_crdt_sync::ReplayTracker::new("dev-a".to_string()),
            )),
            content_store: Arc::clone(&store),
            publisher_tx: t_out,
            subscriber_rx: t_in,
            persist: Arc::new(crate::owner_trust_sync::TrustPersist {
                identity_dir: dir.path().join("trust"),
                replay_path: dir.path().join("trust-replay.cbor"),
            }),
            lookup_key_tag: crate::owner_trust_sync::OWNER_TRUST_LOOKUP_TAG,
            debounce_ms: 25,
            publish_seen: true,
            on_applied: None,
            sibling_acks: Arc::new(tokio::sync::Mutex::new(
                harmony_crdt_sync::MonotoneMap::new(),
            )),
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
        }));

        let (q_out, mut q_drain) = mpsc::channel::<Vec<u8>>(64);
        let (_q_in_tx, q_in) = mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move { while q_drain.recv().await.is_some() {} });
        let quorum_engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
            keys: crate::owner_state_crypto::FleetKeySet::new(kt),
            device_id: "dev-a".to_string(),
            state: Arc::clone(&quorum_doc),
            merger: quorum_merger(),
            replay_tracker: Arc::new(tokio::sync::Mutex::new(
                harmony_crdt_sync::ReplayTracker::new("dev-a".to_string()),
            )),
            content_store: store,
            publisher_tx: q_out,
            subscriber_rx: q_in,
            persist: Arc::new(QuorumPersist {
                doc_path: dir.path().join(OWNER_QUORUM_DOC_FILENAME),
                replay_path: dir.path().join(OWNER_QUORUM_REPLAY_FILENAME),
            }),
            lookup_key_tag: OWNER_QUORUM_LOOKUP_TAG,
            debounce_ms: 25,
            publish_seen: true,
            on_applied: None,
            sibling_acks: Arc::new(tokio::sync::Mutex::new(
                harmony_crdt_sync::MonotoneMap::new(),
            )),
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
        }));

        SweepRig {
            quorum_doc,
            quorum_engine,
            trust_doc,
            trust_engine,
            _dir: dir,
        }
    }

    fn collecting_emit(
        events: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Arc<dyn Fn(&str) + Send + Sync> {
        Arc::new(move |name: &str| events.lock().unwrap().push(name.to_string()))
    }

    /// Plan A's request + B's co-signature into a doc (the state the
    /// initiator's sweep sees after B's signature merges back).
    fn planned_and_cosigned(f: &SweepFleet) -> (QuorumReqDoc, String) {
        let (id, req) = crate::owner_quorum_commands::plan_quorum_revocation_request(
            &f.trust,
            &f.a_sk,
            false,
            &f.c_vk_hex,
            "lost",
            NOW_SECS + 10,
            NOW_MS + 10_000,
            [0xcd; 16],
            None,
        )
        .expect("plan");
        let mut doc = QuorumReqDoc::default();
        doc.requests.insert(id.clone(), req);
        crate::owner_quorum_commands::cosign_request_core(
            &mut doc,
            &f.trust,
            &f.b_sk,
            f.b_id,
            &id,
            NOW_MS + 20_000,
        )
        .expect("cosign");
        (doc, id)
    }

    /// Like `planned_and_cosigned`, but the request BUNDLES a next-epoch carrier
    /// doc (`current_fleet_epoch = Some`) so B's co-sign yields both detached
    /// signatures (ZEB-677 S5).
    fn planned_and_cosigned_bundle(f: &SweepFleet, fleet_epoch: u32) -> (QuorumReqDoc, String) {
        let (id, req) = crate::owner_quorum_commands::plan_quorum_revocation_request(
            &f.trust,
            &f.a_sk,
            false,
            &f.c_vk_hex,
            "lost",
            NOW_SECS + 10,
            NOW_MS + 10_000,
            [0xce; 16],
            Some(fleet_epoch),
        )
        .expect("plan");
        let mut doc = QuorumReqDoc::default();
        doc.requests.insert(id.clone(), req);
        crate::owner_quorum_commands::cosign_request_core(
            &mut doc,
            &f.trust,
            &f.b_sk,
            f.b_id,
            &id,
            NOW_MS + 20_000,
        )
        .expect("cosign");
        (doc, id)
    }

    /// A resident fleet-keys carrier (doc + engine + key set) for exercising the
    /// sweep's quorum epoch install end to end.
    struct CarrierRig {
        carrier: super::QuorumSweepCarrier,
        _dir: tempfile::TempDir,
    }

    fn carrier_rig() -> CarrierRig {
        use crate::content_store::{ContentStore, InMemoryStub};
        use crate::fleet_sync::{FleetSyncConfig, FleetSyncEngine, MergeOutcome};
        use crate::owner_state_crypto::KeyTree;
        use tokio::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let carrier_doc = Arc::new(tokio::sync::Mutex::new(
            crate::fleet_key_epoch::FleetKeyEpochDoc::default(),
        ));
        let kt0 = Arc::new(KeyTree::derive(&[0x77u8; 32]).expect("kt"));
        let fleet_keys = crate::owner_state_crypto::FleetKeySet::new(Arc::clone(&kt0));
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let (c_out, mut c_drain) = mpsc::channel::<Vec<u8>>(64);
        let (_c_in_tx, c_in) = mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move { while c_drain.recv().await.is_some() {} });
        let carrier_engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
            keys: crate::owner_state_crypto::FleetKeySet::new(kt0),
            device_id: "dev-a".to_string(),
            state: Arc::clone(&carrier_doc),
            merger: Arc::new(|_l: &mut crate::fleet_key_epoch::FleetKeyEpochDoc, _r| {
                MergeOutcome { changed: false }
            }),
            replay_tracker: Arc::new(tokio::sync::Mutex::new(
                harmony_crdt_sync::ReplayTracker::new("dev-a".to_string()),
            )),
            content_store: store,
            publisher_tx: c_out,
            subscriber_rx: c_in,
            persist: Arc::new(crate::fleet_key_epoch::FleetKeyEpochPersist {
                doc_path: dir.path().join("fleet_keys.cbor"),
                replay_path: dir.path().join("fleet_keys_replay.cbor"),
            }),
            lookup_key_tag: crate::fleet_key_epoch::FLEET_KEYS_LOOKUP_TAG,
            debounce_ms: 25,
            publish_seen: false,
            on_applied: None,
            sibling_acks: Arc::new(tokio::sync::Mutex::new(
                harmony_crdt_sync::MonotoneMap::new(),
            )),
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
        }));
        CarrierRig {
            carrier: super::QuorumSweepCarrier {
                carrier_doc,
                carrier_engine,
                fleet_keys,
            },
            _dir: dir,
        }
    }

    #[test]
    fn bundled_revocation_assembles_quorum_signed_epoch_doc() {
        let f = sweep_fleet();
        let (doc, id) = planned_and_cosigned_bundle(&f, 4);
        // B's co-signature carries BOTH detached signatures from one approval.
        let b_hex = hex::encode(f.b_id);
        let sigs = &doc.requests[&id].signatures[&b_hex];
        assert!(
            sigs.epoch_doc_sig_hex.is_some(),
            "co-signer produced the epoch-doc part"
        );

        let assembly =
            super::try_assemble(&f.trust, &f.a_sk, f.a_id, &doc.requests[&id]).expect("assemble");
        assert!(assembly.cert.is_some(), "revocation cert assembled");
        let epoch_doc = assembly.epoch_doc.expect("bundled epoch doc assembled");
        let owner_id = f.trust.owner_id;
        assert!(
            epoch_doc.verify_quorum(&owner_id),
            "quorum-signed carrier verifies against its embedded signer bundle"
        );
        assert_eq!(epoch_doc.epoch, 5, "epoch bumped to N+1");
        // The revoked target is excluded from the sealed set; both signers are in.
        assert!(
            !epoch_doc.sealed.contains_key(&hex::encode(f.c_id)),
            "revoked target must not receive new key material"
        );
        assert!(epoch_doc.sealed.contains_key(&hex::encode(f.a_id)));
        assert!(epoch_doc.sealed.contains_key(&b_hex));
        // The initiator can open its own material at the new epoch.
        let material =
            crate::fleet_key_epoch::unseal_own_material(&epoch_doc, &hex::encode(f.a_id), &f.a_sk)
                .expect("unseal own");
        assert_eq!(material.epoch, 5);
    }

    #[tokio::test]
    async fn sweep_with_carrier_installs_bundled_epoch_bump() {
        let f = sweep_fleet();
        let (doc, _id) = planned_and_cosigned_bundle(&f, 4);
        let rig = sweep_rig(f.trust.clone(), doc);
        let cr = carrier_rig();
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let outcome = run_quorum_sweep_with_carrier(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::clone(&events)),
            None,
            NOW_SECS + 30,
            NOW_MS + 30_000,
            Some(&cr.carrier),
        )
        .await;

        assert_eq!(outcome.revocations_applied, 1, "revoke landed");
        assert_eq!(outcome.epoch_bumps_installed, 1, "bundled bump installed");
        assert!(rig.trust_doc.lock().await.is_revoked(f.c_id));
        assert!(
            rig.quorum_doc.lock().await.requests.is_empty(),
            "request pruned"
        );
        // The carrier advanced to N+1 and the initiator publishes on the new epoch.
        assert_eq!(cr.carrier.carrier_doc.lock().await.epoch, 5);
        assert_eq!(cr.carrier.fleet_keys.newest().epoch, 5);
    }

    #[test]
    fn cosign_rejects_substituted_bundled_epoch_doc() {
        // Qodo PR #461 (security): the initiator binds the bundled epoch doc,
        // so a co-signer refuses to sign a doc swapped in after the request was
        // written (which could, e.g., still seal new material to the target).
        let f = sweep_fleet();
        let (id, mut req) = crate::owner_quorum_commands::plan_quorum_revocation_request(
            &f.trust,
            &f.a_sk,
            false,
            &f.c_vk_hex,
            "lost",
            NOW_SECS + 10,
            NOW_MS + 10_000,
            [0xce; 16],
            Some(4),
        )
        .expect("plan");
        // Substitute a DIFFERENT epoch doc (target NOT excluded) — the
        // initiator's binding signature no longer matches these bytes.
        let (evil, _kt) =
            crate::owner_commands::plan_fleet_epoch_bump_quorum(&f.trust, 4, NOW_MS + 10_000, None)
                .expect("evil doc");
        let evil_hex =
            hex::encode(crate::owner_state_crypto::canonical_cbor_encode(&evil).expect("encode"));
        if let QuorumRequestKind::Revocation {
            epoch_doc_cbor_hex, ..
        } = &mut req.kind
        {
            *epoch_doc_cbor_hex = Some(evil_hex);
        }
        let mut doc = QuorumReqDoc::default();
        doc.requests.insert(id.clone(), req);
        let err = crate::owner_quorum_commands::cosign_request_core(
            &mut doc,
            &f.trust,
            &f.b_sk,
            f.b_id,
            &id,
            NOW_MS + 20_000,
        )
        .expect_err("substituted epoch doc must be rejected");
        assert!(err.contains("badEpochDoc"), "unexpected error: {err}");
        // No signature was added.
        assert!(!doc.requests[&id]
            .signatures
            .contains_key(&hex::encode(f.b_id)));
    }

    #[tokio::test]
    async fn bundled_bump_without_carrier_retains_request_for_retry() {
        // CodeRabbit PR #461: a bundled revocation swept before the carrier slot
        // is filled (boot race) must NOT drop the bump — the revoke lands but the
        // request is RETAINED so a later sweep (with the carrier) installs it.
        let f = sweep_fleet();
        let (doc, _id) = planned_and_cosigned_bundle(&f, 4);
        let rig = sweep_rig(f.trust.clone(), doc);
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        // No carrier passed → the plain wrapper (revoke-only install path).
        let outcome = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::clone(&events)),
            None,
            NOW_SECS + 30,
            NOW_MS + 30_000,
        )
        .await;
        assert_eq!(outcome.revocations_applied, 1, "revoke still lands");
        assert_eq!(outcome.epoch_bumps_installed, 0, "no carrier → no install");
        assert!(rig.trust_doc.lock().await.is_revoked(f.c_id));
        assert!(
            !rig.quorum_doc.lock().await.requests.is_empty(),
            "request retained so the bundled bump can be installed on a later sweep"
        );
    }

    #[tokio::test]
    async fn manual_epoch_bump_ceremony_installs_without_revocation() {
        let f = sweep_fleet();
        // A opens a standalone rotation at current epoch 4.
        let (id, req) = crate::owner_quorum_commands::plan_quorum_epoch_bump_request(
            &f.trust,
            &f.a_sk,
            false,
            4,
            NOW_SECS + 10,
            NOW_MS + 10_000,
            [0xef; 16],
        )
        .expect("plan bump");
        assert!(matches!(req.kind, QuorumRequestKind::EpochBump { .. }));
        let mut doc = QuorumReqDoc::default();
        doc.requests.insert(id.clone(), req);

        // B co-signs manually (the epoch-doc part rides `primary_sig_hex`).
        let signed = crate::owner_quorum_commands::cosign_request_core(
            &mut doc,
            &f.trust,
            &f.b_sk,
            f.b_id,
            &id,
            NOW_MS + 20_000,
        )
        .expect("cosign bump");
        assert!(signed);
        let b_hex = hex::encode(f.b_id);
        assert!(!doc.requests[&id].signatures[&b_hex]
            .primary_sig_hex
            .is_empty());

        // A assembles: no revocation cert, a valid quorum-signed carrier at N+1.
        let assembly =
            super::try_assemble(&f.trust, &f.a_sk, f.a_id, &doc.requests[&id]).expect("assemble");
        assert!(assembly.cert.is_none(), "epoch bump has no revocation cert");
        let epoch_doc = assembly.epoch_doc.expect("epoch doc");
        assert_eq!(epoch_doc.epoch, 5);
        assert!(epoch_doc.verify_quorum(&f.trust.owner_id));

        // The sweep installs it (no revocation applied).
        let rig = sweep_rig(f.trust.clone(), doc);
        let cr = carrier_rig();
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let outcome = run_quorum_sweep_with_carrier(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::clone(&events)),
            None,
            NOW_SECS + 30,
            NOW_MS + 30_000,
            Some(&cr.carrier),
        )
        .await;
        assert_eq!(outcome.revocations_applied, 0, "no revocation");
        assert_eq!(outcome.epoch_bumps_installed, 1);
        assert_eq!(cr.carrier.carrier_doc.lock().await.epoch, 5);
        assert_eq!(cr.carrier.fleet_keys.newest().epoch, 5);
        assert!(rig.quorum_doc.lock().await.requests.is_empty());
    }

    #[tokio::test]
    async fn sweep_assembles_applies_prunes_and_is_idempotent() {
        let f = sweep_fleet();
        let (doc, _id) = planned_and_cosigned(&f);
        let rig = sweep_rig(f.trust.clone(), doc);
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (retire_tx, mut retire_rx) = tokio::sync::mpsc::channel::<()>(1);

        let outcome = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::clone(&events)),
            Some(&retire_tx),
            NOW_SECS + 30,
            NOW_MS + 30_000,
        )
        .await;
        assert_eq!(outcome.revocations_applied, 1);
        assert!(outcome.doc_changed);
        assert!(rig.trust_doc.lock().await.is_revoked(f.c_id));
        assert!(rig.quorum_doc.lock().await.requests.is_empty());
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["owner-devices-updated"],
            "sweep emits the device-set change; the task loop adds owner-quorum-updated"
        );
        assert!(retire_rx.try_recv().is_ok(), "retire sweeper nudged");

        // Second sweep: nothing left to do.
        let outcome2 = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::new(std::sync::Mutex::new(Vec::new()))),
            None,
            NOW_SECS + 31,
            NOW_MS + 31_000,
        )
        .await;
        assert_eq!(outcome2, SweepOutcome::default());
        let _ = rig.quorum_engine.shutdown().await;
        let _ = rig.trust_engine.shutdown().await;
    }

    #[tokio::test]
    async fn sweep_skips_tampered_cosigner_sig_and_retains_request() {
        let f = sweep_fleet();
        let (mut doc, id) = planned_and_cosigned(&f);
        {
            let req = doc.requests.get_mut(&id).unwrap();
            let entry = req.signatures.get_mut(&hex::encode(f.b_id)).unwrap();
            entry.primary_sig_hex = "00".repeat(64);
        }
        let rig = sweep_rig(f.trust.clone(), doc);
        let outcome = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::new(std::sync::Mutex::new(Vec::new()))),
            None,
            NOW_SECS + 30,
            NOW_MS + 30_000,
        )
        .await;
        assert_eq!(outcome, SweepOutcome::default());
        assert!(!rig.trust_doc.lock().await.is_revoked(f.c_id));
        assert!(rig.quorum_doc.lock().await.requests.contains_key(&id));
        let _ = rig.quorum_engine.shutdown().await;
        let _ = rig.trust_engine.shutdown().await;
    }

    #[tokio::test]
    async fn sweep_never_assembles_foreign_declined_or_expired_requests() {
        let f = sweep_fleet();
        let (mut doc, id) = planned_and_cosigned(&f);
        // Declined: tombstoned even with a valid signature present. B's
        // decline is a real, verifiable veto (the sweep skips only VERIFIED
        // declines).
        crate::owner_quorum_commands::decline_request_core(
            &mut doc, &f.trust, &f.b_sk, f.b_id, &id,
        )
        .expect("b declines");
        // Expired copy under another id: pruned without assembly.
        let mut expired = doc.requests[&id].clone();
        expired.declined_by.clear();
        expired.expires_at_ms = NOW_MS;
        doc.requests.insert("ee".repeat(16), expired);
        let rig = sweep_rig(f.trust.clone(), doc);

        // Run as B (not the initiator): B must never assemble A's request.
        let outcome_b = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.b_sk,
            f.b_id,
            &collecting_emit(Arc::new(std::sync::Mutex::new(Vec::new()))),
            None,
            NOW_SECS + 40,
            NOW_MS + 40_000,
        )
        .await;
        assert_eq!(outcome_b.revocations_applied, 0);
        assert!(outcome_b.doc_changed, "expired copy pruned");
        // Run as A: the declined request must still never assemble.
        let outcome_a = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::new(std::sync::Mutex::new(Vec::new()))),
            None,
            NOW_SECS + 41,
            NOW_MS + 41_000,
        )
        .await;
        assert_eq!(outcome_a.revocations_applied, 0);
        assert!(!rig.trust_doc.lock().await.is_revoked(f.c_id));
        assert!(rig.quorum_doc.lock().await.requests.contains_key(&id));
        let _ = rig.quorum_engine.shutdown().await;
        let _ = rig.trust_engine.shutdown().await;
    }

    #[tokio::test]
    async fn sweep_retains_request_when_trust_state_rejects_the_cert() {
        // The cosigner is master-certed with a valid signature, but the
        // sweep's trust doc has NO liveness for it — `add_revocation`'s
        // quorum arm (active-window policy) rejects the assembled cert and
        // the request stays resident for retry.
        let f = sweep_fleet();
        let (doc, id) = planned_and_cosigned(&f);
        let mut inactive_trust = f.trust.clone();
        inactive_trust.liveness.remove(&f.b_id);
        let rig = sweep_rig(inactive_trust, doc);
        let outcome = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.a_sk,
            f.a_id,
            &collecting_emit(Arc::new(std::sync::Mutex::new(Vec::new()))),
            None,
            NOW_SECS + 30,
            NOW_MS + 30_000,
        )
        .await;
        assert_eq!(outcome.revocations_applied, 0);
        assert!(!rig.trust_doc.lock().await.is_revoked(f.c_id));
        assert!(rig.quorum_doc.lock().await.requests.contains_key(&id));
        let _ = rig.quorum_engine.shutdown().await;
        let _ = rig.trust_engine.shutdown().await;
    }

    /// Build an A-initiated Enrollment request for a fresh joiner, addressed
    /// to cosigner B (A's authenticating part attached). Returns the doc, the
    /// request id, A's part, the joiner id + bundle.
    fn enrollment_request_for(
        f: &SweepFleet,
        now_secs: u64,
        now_ms: u64,
        arm_b: bool,
    ) -> (QuorumReqDoc, String, Vec<u8>, [u8; 16], PubKeyBundle) {
        let joiner_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let joiner_pk = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let joiner_id = joiner_pk.identity_hash();
        let mut signers = [f.a_id, f.b_id];
        signers.sort();
        let payload =
            enrollment_quorum_payload(f.trust.owner_id, joiner_id, &joiner_pk, now_secs, &signers)
                .expect("payload");
        let a_part = harmony_owner::certs::EnrollmentCert::sign_quorum_part(&f.a_sk, &payload);
        let mut joiner_pk_cbor = Vec::new();
        ciborium::into_writer(&joiner_pk, &mut joiner_pk_cbor).unwrap();
        let mut initiator_sigs = BTreeMap::new();
        initiator_sigs.insert(hex::encode(f.b_id), hex::encode(&a_part));
        let rid = "ee".repeat(8);
        let mut doc = QuorumReqDoc::default();
        doc.requests.insert(
            rid.clone(),
            QuorumRequest {
                created_at: hlc(now_ms, "aa"),
                declined_by: BTreeMap::new(),
                initiator_hex: hex::encode(f.a_id),
                kind: QuorumRequestKind::Enrollment {
                    joiner_device_id_hex: hex::encode(joiner_id),
                    joiner_pubkeys_cbor_hex: hex::encode(&joiner_pk_cbor),
                },
                initiator_sigs,
                signatures: BTreeMap::new(),
                issued_at: now_secs,
                expires_at_ms: now_ms + 100_000,
            },
        );
        if arm_b {
            stamp_arm_cell(&mut doc, f.b_id, now_ms + ARM_WINDOW_MS, now_ms);
        }
        (doc, rid, a_part, joiner_id, joiner_pk)
    }

    /// B-side auto-co-sign (spec §5.2): armed B reacts to A's authenticated
    /// Enrollment request — signs, vouches the joiner, consumes the arm —
    /// and the co-signature assembles into a valid quorum enrollment cert.
    #[tokio::test]
    async fn armed_b_auto_cosigns_enrollment_vouches_and_consumes_arm() {
        let f = sweep_fleet();
        let now_secs = NOW_SECS + 10;
        let now_ms = NOW_MS + 10_000;
        let (doc, rid, a_part, joiner_id, joiner_pk) =
            enrollment_request_for(&f, now_secs, now_ms, true);

        let rig = sweep_rig(f.trust.clone(), doc);
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let outcome = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.b_sk,
            f.b_id,
            &collecting_emit(Arc::clone(&events)),
            None,
            now_secs + 1,
            now_ms + 1,
        )
        .await;

        assert_eq!(outcome.enrollment_cosigns, 1, "B co-signed the enrollment");

        // B's co-signature is present and assembles into a valid K=2 cert.
        let b_part = {
            let doc = rig.quorum_doc.lock().await;
            let req = doc.requests.get(&rid).expect("request");
            hex::decode(
                &req.signatures
                    .get(&hex::encode(f.b_id))
                    .expect("B signed")
                    .primary_sig_hex,
            )
            .unwrap()
        };
        let mut parts = vec![(f.a_id, a_part), (f.b_id, b_part)];
        parts.sort_by_key(|(id, _)| *id);
        let cert = harmony_owner::certs::EnrollmentCert::assemble_quorum(
            f.trust.owner_id,
            joiner_id,
            joiner_pk,
            now_secs,
            None,
            parts,
        )
        .expect("assemble");
        let a_cert = f.trust.enrollments.get(&f.a_id).unwrap().clone();
        let b_cert = f.trust.enrollments.get(&f.b_id).unwrap().clone();
        cert.verify_quorum_with_signers(&[a_cert, b_cert], now_secs + 2)
            .expect("valid quorum enrollment cert");

        // B vouched the joiner (lifts Provisional→Full under N=1).
        {
            let trust = rig.trust_doc.lock().await;
            assert!(
                trust
                    .vouching
                    .vouches_for(joiner_id)
                    .any(|v| v.signer == f.b_id
                        && matches!(v.stance, harmony_owner::certs::Stance::Vouch)),
                "B minted a Vouch for the joiner"
            );
        }
        // B's single-use arm is consumed.
        {
            let doc = rig.quorum_doc.lock().await;
            let arm = doc
                .enroll_arms
                .get(&hex::encode(f.b_id))
                .expect("arm cell present");
            assert!(arm.armed_until_ms <= now_ms + 1, "arm consumed");
        }

        let _ = rig.quorum_engine.shutdown().await;
        let _ = rig.trust_engine.shutdown().await;
    }

    /// Without a live arm, B never co-signs an enrollment request.
    #[tokio::test]
    async fn unarmed_b_ignores_enrollment_request() {
        let f = sweep_fleet();
        let now_secs = NOW_SECS + 10;
        let now_ms = NOW_MS + 10_000;
        let (doc, rid, _a_part, _jid, _jpk) = enrollment_request_for(&f, now_secs, now_ms, false);

        let rig = sweep_rig(f.trust.clone(), doc);
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let outcome = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.b_sk,
            f.b_id,
            &collecting_emit(events),
            None,
            now_secs + 1,
            now_ms + 1,
        )
        .await;
        assert_eq!(outcome.enrollment_cosigns, 0, "no arm ⇒ no co-sign");
        {
            let doc = rig.quorum_doc.lock().await;
            assert!(!doc
                .requests
                .get(&rid)
                .unwrap()
                .signatures
                .contains_key(&hex::encode(f.b_id)));
        }
        let _ = rig.quorum_engine.shutdown().await;
        let _ = rig.trust_engine.shutdown().await;
    }

    /// Convergent abandon (Greptile): an armed B must NOT co-sign an
    /// enrollment request whose initiator has signed an abandon marker into
    /// `declined_by` — even though B holds a live arm and the request is
    /// otherwise valid — so a stale re-merge can't burn B's single-use arm.
    #[tokio::test]
    async fn armed_b_skips_abandoned_enrollment_request() {
        let f = sweep_fleet();
        let now_secs = NOW_SECS + 10;
        let now_ms = NOW_MS + 10_000;
        let (mut doc, rid, _a_part, _jid, _jpk) =
            enrollment_request_for(&f, now_secs, now_ms, true);
        // A abandons its own request (signed `declined_by[A]`).
        let payload = decline_signing_payload(f.trust.owner_id, &rid);
        let a_sig = harmony_owner::signing::sign_with_tag(
            &f.a_sk,
            harmony_owner::signing::tags::REVOCATION,
            &payload,
        );
        doc.requests
            .get_mut(&rid)
            .unwrap()
            .declined_by
            .insert(hex::encode(f.a_id), hex::encode(a_sig));

        let rig = sweep_rig(f.trust.clone(), doc);
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let outcome = run_quorum_sweep(
            &rig.quorum_doc,
            &rig.quorum_engine,
            &rig.trust_doc,
            &rig.trust_engine,
            &f.b_sk,
            f.b_id,
            &collecting_emit(events),
            None,
            now_secs + 1,
            now_ms + 1,
        )
        .await;
        assert_eq!(
            outcome.enrollment_cosigns, 0,
            "an abandoned request must not be co-signed"
        );
        {
            let doc = rig.quorum_doc.lock().await;
            assert!(!doc
                .requests
                .get(&rid)
                .unwrap()
                .signatures
                .contains_key(&hex::encode(f.b_id)));
        }
        let _ = rig.quorum_engine.shutdown().await;
        let _ = rig.trust_engine.shutdown().await;
    }

    /// A-side planner picks the armed sibling and its request assembles into
    /// a valid cert once that sibling co-signs — the full A-plan → B-cosign →
    /// A-assemble path, pure (no engine).
    #[test]
    fn plan_enrollment_then_assemble_after_cosign() {
        let f = sweep_fleet();
        let now_secs = NOW_SECS + 10;
        let now_ms = NOW_MS + 10_000;
        let joiner_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let joiner_pk = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let joiner_id = joiner_pk.identity_hash();

        // B is the only armed sibling.
        let mut arms = BTreeMap::new();
        arms.insert(
            hex::encode(f.b_id),
            EnrollArm {
                set_at: hlc(now_ms, "bb"),
                armed_until_ms: now_ms + ARM_WINDOW_MS,
            },
        );

        // A plans the request.
        let (id, req) = plan_enrollment_request(
            &f.trust, &arms, &f.a_sk, f.a_id, joiner_id, &joiner_pk, now_secs, now_ms, [0xab; 16],
        )
        .expect("plan");
        // The request targets B (the armed sibling) and carries A's part.
        assert_eq!(req.initiator_hex, hex::encode(f.a_id));
        assert!(req.initiator_sigs.contains_key(&hex::encode(f.b_id)));
        let mut doc = QuorumReqDoc::default();
        doc.requests.insert(id.clone(), req);

        // Before B co-signs, assembly yields nothing.
        assert!(try_assemble_enrollment(&doc, &f.trust, f.a_id, &id).is_none());

        // B co-signs (same sorted-[A,B] payload).
        let mut signers = [f.a_id, f.b_id];
        signers.sort();
        let payload =
            enrollment_quorum_payload(f.trust.owner_id, joiner_id, &joiner_pk, now_secs, &signers)
                .unwrap();
        let b_part = harmony_owner::certs::EnrollmentCert::sign_quorum_part(&f.b_sk, &payload);
        doc.requests
            .get_mut(&id)
            .unwrap()
            .signatures
            .insert(hex::encode(f.b_id), sigs(&hex::encode(b_part)));

        // A assembles a valid quorum enrollment cert.
        let cert = try_assemble_enrollment(&doc, &f.trust, f.a_id, &id).expect("assemble");
        let a_cert = f.trust.enrollments.get(&f.a_id).unwrap().clone();
        let b_cert = f.trust.enrollments.get(&f.b_id).unwrap().clone();
        cert.verify_quorum_with_signers(&[a_cert, b_cert], now_secs + 1)
            .expect("valid cert");
        assert_eq!(cert.device_id, joiner_id);
    }

    #[test]
    fn plan_enrollment_request_errors_without_armed_sibling() {
        let f = sweep_fleet();
        let now_secs = NOW_SECS + 10;
        let now_ms = NOW_MS + 10_000;
        let joiner_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let joiner_pk = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let err = plan_enrollment_request(
            &f.trust,
            &BTreeMap::new(), // no arms
            &f.a_sk,
            f.a_id,
            joiner_pk.identity_hash(),
            &joiner_pk,
            now_secs,
            now_ms,
            [0xab; 16],
        )
        .unwrap_err();
        assert!(err.starts_with("noArmedSibling:"), "got: {err}");
    }

    #[test]
    fn persist_round_trips_doc_and_replay_and_quarantines_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let doc_path = dir.path().join(OWNER_QUORUM_DOC_FILENAME);
        let replay_path = dir.path().join(OWNER_QUORUM_REPLAY_FILENAME);
        let mut doc = QuorumReqDoc::default();
        doc.requests.insert(
            "aa".repeat(16),
            test_request(&"11".repeat(16), &"22".repeat(16)),
        );
        let mut tracker = BTreeMap::new();
        tracker.insert("device-a".to_string(), hlc(NOW_MS, "device-a"));

        let persist = QuorumPersist {
            doc_path: doc_path.clone(),
            replay_path: replay_path.clone(),
        };
        FleetPersist::persist(&persist, &doc, &tracker).unwrap();
        assert_eq!(load_quorum_doc_or_recover(&doc_path), doc);
        assert_eq!(
            load_quorum_replay_or_recover(&replay_path).get("device-a"),
            tracker.get("device-a")
        );

        // Missing → default; corrupt → quarantined + default.
        assert_eq!(
            load_quorum_doc_or_recover(&dir.path().join("nope.cbor")),
            QuorumReqDoc::default()
        );
        std::fs::write(&doc_path, b"definitely not cbor").unwrap();
        assert_eq!(
            load_quorum_doc_or_recover(&doc_path),
            QuorumReqDoc::default()
        );
        assert!(!doc_path.exists(), "corrupt file quarantined aside");
    }
}
