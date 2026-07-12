//! ZEB-677 S3: the `owner-quorum-req-v1` fleet dataset — pending quorum
//! co-sign requests (revocation now; enrollment arms in S4) replicated
//! between the owner's devices as the next `FleetSyncEngine` dataset.
//! Donor pattern: `owner_trust_sync.rs` (merge/persist/applied-task shape)
//! + `fleet_key_epoch.rs` (own-doc-file persistence recipe). Spec:
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
#[derive(Serialize, Deserialize)]
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
impl Default for QuorumReplayFileV1 {
    fn default() -> Self {
        QuorumReplayFileV1(BTreeMap::new())
    }
}

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
