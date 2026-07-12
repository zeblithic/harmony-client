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
//! by `MAX_QUORUM_REQUESTS`); known ids union `initiator_sigs` /
//! `signatures` / `declined_by` grow-only with existing entries winning.
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

use crate::fleet_sync::{FleetPersist, MergeOutcome, Merger, SyncError};
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use ciborium::{from_reader, into_writer};
use harmony_owner::certs::{RevocationCert, RevocationReason};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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

/// What a pending request asks the fleet to co-sign. Revocation only in
/// S3; the S4 enrollment ceremony adds its variant, and S5 threads the
/// bundled next-epoch doc through `QuorumRequestSigs::epoch_doc_sig_hex`.
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
    },
}

/// One device's detached signatures over a request's constituent payloads.
/// One approval action yields all of them (spec §3). `epoch_doc_sig_hex`
/// is the S5 slot (bundled epoch bump) — always `None` in S3.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// Device-id hexes that declined. Grow-only; ANY decline tombstones
    /// the request (spec §3) — it stays resident but dead until expiry.
    #[serde(rename = "d", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub declined_by: BTreeSet<String>,
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

/// Fold a remote quorum doc into local. Pure union — no pruning here (see
/// module docs). Changed-detection is canonical-encode compare (docs are
/// tiny), matching the trust-merge donor.
pub fn merge_quorum_remote_into_local(
    local: &mut QuorumReqDoc,
    remote: QuorumReqDoc,
) -> MergeOutcome {
    let before = crate::owner_state_crypto::canonical_cbor_encode(&*local).ok();
    for (id, req) in remote.requests {
        match local.requests.get_mut(&id) {
            None => {
                if !within_caps(&req) {
                    tracing::warn!(request = %id, "quorum merge: over-cap request dropped");
                    continue;
                }
                if local.requests.len() >= MAX_QUORUM_REQUESTS {
                    tracing::warn!(request = %id, "quorum merge: request cap reached; dropped");
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
                // Grow-only unions; existing entries always win.
                for (k, v) in req.initiator_sigs {
                    if existing.initiator_sigs.len() >= MAX_QUORUM_SIG_ENTRIES {
                        break;
                    }
                    existing.initiator_sigs.entry(k).or_insert(v);
                }
                for (k, v) in req.signatures {
                    if existing.signatures.len() >= MAX_QUORUM_SIG_ENTRIES {
                        break;
                    }
                    existing.signatures.entry(k).or_insert(v);
                }
                for d in req.declined_by {
                    if existing.declined_by.len() >= MAX_QUORUM_SIG_ENTRIES {
                        break;
                    }
                    existing.declined_by.insert(d);
                }
            }
        }
    }
    for (armer, arm) in remote.enroll_arms {
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
/// re-merges until natural expiry. Expired enrollment arms are dropped.
/// Returns whether anything was removed.
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
        let QuorumRequestKind::Revocation { target_hex, .. } = &req.kind;
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
    doc.enroll_arms
        .retain(|_, arm| now_ms <= arm.armed_until_ms);
    doc.requests.len() != before_reqs || doc.enroll_arms.len() != before_arms
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
}

/// One assemblable completion candidate, collected under the quorum-doc
/// lock and applied after it is released (the trust mutation takes the
/// trust-doc lock; never hold both).
struct CompletionCandidate {
    request_id: String,
    cert: RevocationCert,
}

/// Validate a cosigner's entry against the CURRENT trust doc and, when it
/// verifies, assemble the K=2 cert with a freshly minted initiator part.
fn try_assemble(
    trust: &harmony_owner::state::OwnerState,
    device_signing_key: &ed25519_dalek::SigningKey,
    self_id: [u8; 16],
    req: &QuorumRequest,
) -> Option<RevocationCert> {
    let QuorumRequestKind::Revocation { reason, target_hex } = &req.kind;
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
        let Ok(vk) =
            ed25519_dalek::VerifyingKey::from_bytes(&cert.device_pubkeys.classical.ed25519_verify)
        else {
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
        match RevocationCert::assemble_quorum(
            trust.owner_id,
            target,
            req.issued_at,
            reason.clone(),
            parts,
        ) {
            Ok(cert) => return Some(cert),
            Err(e) => {
                tracing::warn!(error = %e, "quorum sweep: assemble failed; skipped");
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
/// Lock discipline: candidates are collected under the quorum lock,
/// applied under the trust lock, then removed under the quorum lock again
/// — the two locks are never held together.
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
    let self_hex = hex::encode(self_device_id);
    let trust_snapshot = trust_doc.lock().await.clone();

    // Phase A: prune + collect candidates under the quorum lock.
    let (pruned, candidates) = {
        let mut doc = quorum_doc.lock().await;
        let pruned = prune_settled_requests(&mut doc, &trust_snapshot, now_ms);
        let mut candidates = Vec::new();
        for (id, req) in doc.requests.iter() {
            if req.initiator_hex != self_hex
                || now_ms > req.expires_at_ms
                || !req.declined_by.is_empty()
            {
                continue;
            }
            if let Some(cert) =
                try_assemble(&trust_snapshot, device_signing_key, self_device_id, req)
            {
                candidates.push(CompletionCandidate {
                    request_id: id.clone(),
                    cert,
                });
            }
        }
        (pruned, candidates)
    };
    if pruned {
        quorum_engine.notify_dirty();
    }

    // Phase B: apply each assembled cert through the authoritative path.
    let mut completed = Vec::new();
    for cand in candidates {
        let cert = cand.cert;
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
                if let Err(e) = trust_engine.flush_now().await {
                    tracing::warn!(error = %e,
                        "quorum sweep: trust flush failed; dirty latch will retry");
                }
                emit("owner-devices-updated");
                if let Some(tx) = retire_nudge {
                    let _ = tx.try_send(());
                }
                completed.push(cand.request_id);
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, request = %cand.request_id,
                    "quorum sweep: assembled revocation rejected by trust state; request retained");
            }
            Err(e) => {
                tracing::warn!(error = %e, request = %cand.request_id,
                    "quorum sweep: trust mutation failed; request retained");
            }
        }
    }

    // Phase C: drop completed requests from the quorum doc.
    let applied_count = completed.len();
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
    SweepOutcome {
        doc_changed: pruned || applied_count > 0,
        revocations_applied: applied_count,
    }
}

/// The quorum engine's `on_applied` consumer: each nudge (an inbound merge
/// that changed the doc, or the one boot tick) runs a completion sweep and
/// then tells the UI the pending-request surface changed. The boot tick
/// covers signatures that accumulated while this device was offline.
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
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while nudge_rx.recv().await.is_some() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            run_quorum_sweep(
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
            )
            .await;
            emit("owner-quorum-updated");
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
            declined_by: BTreeSet::new(),
            initiator_hex: initiator.to_string(),
            kind: QuorumRequestKind::Revocation {
                reason: "lost".to_string(),
                target_hex: target.to_string(),
            },
            initiator_sigs: BTreeMap::new(),
            signatures: BTreeMap::new(),
            issued_at: NOW_SECS,
            expires_at_ms: NOW_MS + QUORUM_REVOCATION_TTL_MS,
        }
    }

    fn sigs(sig: &str) -> QuorumRequestSigs {
        QuorumRequestSigs {
            epoch_doc_sig_hex: None,
            primary_sig_hex: sig.to_string(),
        }
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
        req_c.declined_by.insert("dd".repeat(16));
        remote_c.requests.insert(id.clone(), req_c);

        assert!(merge_quorum_remote_into_local(&mut local, remote_b).changed);
        assert!(merge_quorum_remote_into_local(&mut local, remote_c).changed);
        let merged = &local.requests[&id];
        assert_eq!(merged.signatures.len(), 2);
        assert_eq!(merged.declined_by.len(), 1);
    }

    #[test]
    fn merge_never_overwrites_existing_entries() {
        let id = "ab".repeat(16);
        let mut base = test_request(&"11".repeat(16), &"22".repeat(16));
        base.signatures.insert("bb".repeat(16), sigs("original"));
        let mut local = QuorumReqDoc::default();
        local.requests.insert(id.clone(), base.clone());

        let mut remote = QuorumReqDoc::default();
        let mut req = base.clone();
        req.signatures
            .insert("bb".repeat(16), sigs("attacker-swap"));
        remote.requests.insert(id.clone(), req);

        let outcome = merge_quorum_remote_into_local(&mut local, remote);
        assert!(!outcome.changed);
        assert_eq!(
            local.requests[&id].signatures[&"bb".repeat(16)].primary_sig_hex,
            "original"
        );
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
        // Declined-but-unexpired request: retained.
        let mut declined = test_request(&"11".repeat(16), &target_hex);
        declined.declined_by.insert("22".repeat(16));
        doc.requests.insert("cc".repeat(16), declined);
        // Malformed target: dropped.
        doc.requests.insert(
            "dd".repeat(16),
            test_request(&"11".repeat(16), "zz-not-hex"),
        );
        // Expired arm dropped; live arm kept.
        doc.enroll_arms.insert(
            "11".repeat(16),
            EnrollArm {
                set_at: hlc(1, "a"),
                armed_until_ms: NOW_MS - 1,
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
                replay_tracker: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
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
                sibling_acks: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
            }));
            let quorum_doc = Arc::new(tokio::sync::Mutex::new(QuorumReqDoc::default()));
            let quorum_engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
                keys: crate::owner_state_crypto::FleetKeySet::new(Arc::clone(&kt)),
                device_id: name.to_string(),
                state: Arc::clone(&quorum_doc),
                merger: quorum_merger(),
                replay_tracker: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
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
                sibling_acks: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
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
            assert!(
                crate::owner_quorum_commands::decline_request_core(&mut doc_b, f.b_id, &id)
                    .expect("decline")
            );
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
        let assembled =
            super::try_assemble(&f.trust, &f.a_sk, f.a_id, &doc.requests[&id]).expect("assemble");
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
            replay_tracker: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
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
            sibling_acks: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
        }));

        let (q_out, mut q_drain) = mpsc::channel::<Vec<u8>>(64);
        let (_q_in_tx, q_in) = mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move { while q_drain.recv().await.is_some() {} });
        let quorum_engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
            keys: crate::owner_state_crypto::FleetKeySet::new(kt),
            device_id: "dev-a".to_string(),
            state: Arc::clone(&quorum_doc),
            merger: quorum_merger(),
            replay_tracker: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
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
            sibling_acks: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
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
        // Declined: tombstoned even with a valid signature present.
        doc.requests
            .get_mut(&id)
            .unwrap()
            .declined_by
            .insert(hex::encode(f.b_id));
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
