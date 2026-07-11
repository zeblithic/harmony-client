//! Trust-state replication (ZEB-668 S1). Replicates the harmony-owner
//! trust CRDT (enrollments / vouching / revocations / liveness) between
//! the owner's devices as the next `FleetSyncEngine` dataset. Donor
//! pattern: `owner_state_sync.rs` (wrapper shape) + the fleet-net boot
//! block in `lib.rs` (engine construction) + `fleet_net_persist.rs`
//! (replay-tracker persistence recipe). Spec:
//! `docs/specs/2026-07-11-zeb-668-device-management-design.md` §2.
//!
//! The trust doc's disk source of truth stays `owner_state.cbor` (written
//! through the existing `save_owner_state_cbor_only` — disk only, no
//! keychain, so ZEB-428's `*_inner` seam rules are untouched). Only the
//! replay tracker gets a new file. Merge is NEVER trust-degrading: every
//! remote record passes through the crate's own validating `add_*`
//! mutators, and records that fail validation are dropped with a log.

use crate::fleet_sync::{FleetPersist, MergeOutcome, Merger, SyncError};
use crate::owner_state::{load_owner_state_cbor, save_owner_state_cbor_only};
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use ciborium::{from_reader, into_writer};
use harmony_owner::state::OwnerState;
use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Dataset name — forms the Zenoh topic
/// `harmony/owner/{addr_hex}/ds/owner-trust-v1` via
/// `spawn_dataset_sync_zenoh_adapter`, and doubles as the CAS lookup tag.
pub const OWNER_TRUST_DATASET: &str = "owner-trust-v1";
pub const OWNER_TRUST_LOOKUP_TAG: &[u8] = b"owner-trust-v1";

/// Trust docs are tiny (≤ `MAX_DEVICES_PER_OWNER` certs per map); 256 KiB
/// is generous headroom while still bounding a hostile publish.
pub const OWNER_TRUST_DATASET_MAX_BYTES: usize = 256 * 1024;

pub const OWNER_TRUST_REPLAY_FILENAME: &str = "owner_trust_replay.cbor";

const OWNER_TRUST_REPLAY_SCHEMA_V1: u8 = 1;

// ZEB-220 sealed CanonicalPayload registration for the FOREIGN type
// `harmony_owner::state::OwnerState` — the same two empty impls the
// `impl_canonical!` macro expands to (see fleet_sync.rs, which does this
// manually for `FleetRootPublish`). Coherent because both traits are
// crate-local.
impl CanonicalPayloadSealed for OwnerState {}
impl CanonicalPayload for OwnerState {}

/// Fold a remote trust snapshot into local via the crate's validating
/// mutators. Fold order is load-bearing: enrollments → revocations →
/// vouching → liveness (vouching/liveness validation requires the signer's
/// enrollment to exist; remove-wins revocations must land before
/// vouching/liveness so a revoked signer's records are rejected).
///
/// Changed-detection is canonical-encode compare — trust docs are tiny, and
/// this stays correct regardless of which individual `add_*` calls were
/// idempotent no-ops vs. real inserts.
pub fn merge_trust_remote_into_local(local: &mut OwnerState, remote: OwnerState) -> MergeOutcome {
    let before = harmony_owner::cbor::to_canonical(&*local).ok();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let OwnerState {
        owner_id: _,
        enrollments,
        vouching,
        revocations,
        liveness,
    } = remote;
    for (_id, cert) in enrollments {
        if let Err(e) = local.add_enrollment(cert, now, DEFAULT_ACTIVE_WINDOW_SECS) {
            tracing::debug!(error = %e, "trust merge: enrollment dropped");
        }
    }
    for cert in revocations.iter() {
        if let Err(e) = local.add_revocation(cert.clone()) {
            tracing::debug!(error = %e, "trust merge: revocation dropped");
        }
    }
    for cert in vouching.iter() {
        if let Err(e) = local.add_vouching(cert.clone()) {
            tracing::debug!(error = %e, "trust merge: vouching dropped");
        }
    }
    for (_id, cert) in liveness {
        if let Err(e) = local.add_liveness(cert) {
            tracing::debug!(error = %e, "trust merge: liveness dropped");
        }
    }
    let after = harmony_owner::cbor::to_canonical(&*local).ok();
    MergeOutcome {
        changed: before != after,
    }
}

/// The trust doc's `Merger` for `FleetSyncEngine` construction.
pub fn trust_merger() -> Merger<OwnerState> {
    Arc::new(merge_trust_remote_into_local)
}

/// Replay-tracker file body (schema byte precedes this on disk).
#[derive(Serialize, Deserialize)]
struct TrustReplayFileV1(BTreeMap<String, Hlc>);

/// Save the replay tracker atomically (schema byte + canonical CBOR).
pub fn save_trust_replay(path: &Path, tracker: &BTreeMap<String, Hlc>) -> Result<(), String> {
    let mut bytes = vec![OWNER_TRUST_REPLAY_SCHEMA_V1];
    into_writer(&TrustReplayFileV1(tracker.clone()), &mut bytes)
        .map_err(|e| format!("encode trust replay {}: {e}", path.display()))?;
    crate::owner_state_persist::save_atomically(path, &bytes).map_err(|e| e.to_string())
}

/// Load the replay tracker, recovering to empty on ANY failure. A lost
/// tracker is benign for trust state: the doc itself (`owner_state.cbor`)
/// is the source of truth, and re-accepting an already-merged publish is
/// idempotent through the validating merge. Corrupt files are quarantined
/// (renamed aside) so the bytes survive for manual inspection — the
/// `fleet_net_persist::quarantine` contract.
pub fn load_trust_replay_or_recover(path: &Path) -> BTreeMap<String, Hlc> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "trust replay read failed; starting with empty tracker");
            return BTreeMap::new();
        }
    };
    let decoded = match bytes.split_first() {
        Some((&OWNER_TRUST_REPLAY_SCHEMA_V1, rest)) => {
            from_reader::<TrustReplayFileV1, _>(rest).map(|f| f.0)
        }
        _ => Err(ciborium::de::Error::Semantic(
            None,
            "bad trust-replay schema byte".to_string(),
        )),
    };
    match decoded {
        Ok(t) => t,
        Err(e) => {
            quarantine(path, &e.to_string());
            BTreeMap::new()
        }
    }
}

/// Rename a corrupt file aside with a timestamped suffix (never clobbers a
/// prior quarantine or the live file; preserves bytes for manual recovery).
fn quarantine(path: &Path, err: &str) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut corrupt = path.as_os_str().to_os_string();
    corrupt.push(format!(".corrupt-{ms}"));
    tracing::error!(path = %path.display(), error = %err,
        "trust replay load failed; quarantining corrupt file and starting fresh (bytes preserved)");
    if let Err(re) = std::fs::rename(path, &corrupt) {
        tracing::warn!(path = %path.display(), error = %re,
            "failed to quarantine corrupt trust replay file");
    }
}

/// Durability sink for the trust engine. The doc goes through the existing
/// `owner_state.cbor` writer (disk-only — no keychain); the replay tracker
/// to its own file. The engine calls `persist` inside `spawn_blocking`.
pub struct TrustPersist {
    pub identity_dir: PathBuf,
    pub replay_path: PathBuf,
}

impl FleetPersist<OwnerState> for TrustPersist {
    fn persist(
        &self,
        state: &OwnerState,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        save_owner_state_cbor_only(&self.identity_dir, state).map_err(SyncError::Persist)?;
        save_trust_replay(&self.replay_path, tracker).map_err(SyncError::Persist)?;
        Ok(())
    }
}

/// How a caller reaches the trust doc, depending on app mode.
pub enum TrustStateAccess {
    /// Node running: the resident doc + engine from AppState.
    Resident {
        doc: Arc<tokio::sync::Mutex<OwnerState>>,
        engine: Arc<crate::fleet_sync::FleetSyncEngine<OwnerState>>,
    },
    /// Node stopped / CLI: classic load-mutate-save on `owner_state.cbor`.
    FileOnly { identity_dir: PathBuf },
}

/// Apply a mutation to the trust doc in whichever mode the app is in.
/// Resident: mutate the shared doc and let the engine's debounced
/// publish+persist carry it. FileOnly: load, mutate, save.
pub async fn mutate_trust_state<R>(
    access: TrustStateAccess,
    f: impl FnOnce(&mut OwnerState) -> R,
) -> Result<R, String> {
    match access {
        TrustStateAccess::Resident { doc, engine } => {
            let r = {
                let mut guard = doc.lock().await;
                f(&mut guard)
            };
            engine.notify_dirty();
            Ok(r)
        }
        TrustStateAccess::FileOnly { identity_dir } => {
            let mut state = load_owner_state_cbor(&identity_dir)?;
            let r = f(&mut state);
            save_owner_state_cbor_only(&identity_dir, &state)?;
            Ok(r)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use harmony_owner::certs::{LivenessCert, RevocationCert, RevocationReason};
    use harmony_owner::lifecycle::{enroll_via_master, mint_owner, MintResult, RecoveryArtifact};
    use harmony_owner::pubkey_bundle::PubKeyBundle;

    fn test_mint(now: u64) -> (OwnerState, RecoveryArtifact, SigningKey) {
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(now).unwrap();
        (state, recovery_artifact, device_signing_key)
    }

    fn test_enroll_second_device(
        artifact: &RecoveryArtifact,
        state: &OwnerState,
        now: u64,
    ) -> (SigningKey, harmony_owner::certs::EnrollmentCert) {
        let sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let pkb = PubKeyBundle::classical_only(sk.verifying_key().to_bytes());
        let res = enroll_via_master(state, artifact, &sk, pkb, now, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        (sk, res.enrollment_cert)
    }

    fn test_master_revocation(
        artifact: &RecoveryArtifact,
        target: [u8; 16],
        now: u64,
    ) -> RevocationCert {
        RevocationCert::sign_master(
            &artifact.master_signing_key(),
            artifact.master_pubkey_bundle(),
            target,
            now,
            RevocationReason::Decommissioned,
        )
        .unwrap()
    }

    fn test_liveness(signer_sk: &SigningKey, owner_id: [u8; 16], now: u64) -> LivenessCert {
        LivenessCert::sign(signer_sk, owner_id, now).unwrap()
    }

    #[test]
    fn merge_folds_new_enrollment_from_remote() {
        let now = 1_700_000_000u64;
        let (mut local, artifact, _sk1) = test_mint(now);
        let mut remote = local.clone();
        let (_sk2, cert2) = test_enroll_second_device(&artifact, &remote, now + 10);
        remote
            .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        assert_eq!(local.enrollments.len(), 1);
        let outcome = merge_trust_remote_into_local(&mut local, remote);
        assert!(outcome.changed);
        assert_eq!(local.enrollments.len(), 2);
    }

    #[test]
    fn merge_is_idempotent_and_reports_unchanged() {
        let now = 1_700_000_000u64;
        let (mut local, _artifact, _sk) = test_mint(now);
        let remote = local.clone();
        let outcome = merge_trust_remote_into_local(&mut local, remote);
        assert!(!outcome.changed);
    }

    #[test]
    fn merge_revocation_wins_over_concurrent_liveness() {
        // Remote branch A revokes device 2; remote branch B (unaware)
        // carries a liveness cert signed by device 2. After both merges the
        // revocation must hold and the liveness record must be dropped
        // (fold order: revocations before liveness).
        let now = 1_700_000_000u64;
        let (mut local, artifact, _sk1) = test_mint(now);
        let (sk2, cert2) = test_enroll_second_device(&artifact, &local, now + 10);
        let d2 = cert2.device_id;
        local
            .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let owner_id = local.owner_id;

        let mut remote_revoked = local.clone();
        let rev = test_master_revocation(&artifact, d2, now + 20);
        remote_revoked.add_revocation(rev).unwrap();

        let mut remote_liveness = local.clone();
        remote_liveness
            .add_liveness(test_liveness(&sk2, owner_id, now + 25))
            .unwrap();

        merge_trust_remote_into_local(&mut local, remote_revoked);
        merge_trust_remote_into_local(&mut local, remote_liveness);

        assert!(local.is_revoked(d2));
        assert!(!local.liveness.contains_key(&d2));
    }

    #[test]
    fn merge_drops_record_for_foreign_owner_without_degrading() {
        let now = 1_700_000_000u64;
        let (mut local, _a1, _s1) = test_mint(now);
        let (foreign, _a2, _s2) = test_mint(now + 5);
        let before = harmony_owner::cbor::to_canonical(&local).unwrap();
        let outcome = merge_trust_remote_into_local(&mut local, foreign);
        assert!(!outcome.changed);
        assert_eq!(harmony_owner::cbor::to_canonical(&local).unwrap(), before);
    }

    #[test]
    fn trust_persist_round_trips_doc_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_700_000_000u64;
        let (state, _a, _s) = test_mint(now);
        let replay_path = dir.path().join(OWNER_TRUST_REPLAY_FILENAME);
        let persist = TrustPersist {
            identity_dir: dir.path().to_path_buf(),
            replay_path: replay_path.clone(),
        };
        let mut tracker = BTreeMap::new();
        tracker.insert(
            "device-a".to_string(),
            Hlc {
                wall_ms: now,
                logical: 0,
                device_id: "device-a".to_string(),
            },
        );
        FleetPersist::persist(&persist, &state, &tracker).unwrap();
        let reloaded = load_owner_state_cbor(dir.path()).unwrap();
        assert_eq!(reloaded.enrollments.len(), state.enrollments.len());
        assert_eq!(reloaded.owner_id, state.owner_id);
        let replay = load_trust_replay_or_recover(&replay_path);
        assert_eq!(replay.get("device-a"), tracker.get("device-a"));
    }

    #[test]
    fn replay_recover_returns_empty_on_missing_or_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.cbor");
        assert!(load_trust_replay_or_recover(&missing).is_empty());
        let corrupt = dir.path().join("bad.cbor");
        std::fs::write(&corrupt, b"not cbor at all").unwrap();
        assert!(load_trust_replay_or_recover(&corrupt).is_empty());
        // Quarantined aside, original gone.
        assert!(!corrupt.exists());
    }

    #[tokio::test]
    async fn mutate_file_only_loads_applies_saves() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_700_000_000u64;
        let (state, _a, _s) = test_mint(now);
        save_owner_state_cbor_only(dir.path(), &state).unwrap();
        let n = mutate_trust_state(
            TrustStateAccess::FileOnly {
                identity_dir: dir.path().to_path_buf(),
            },
            |s| s.enrollments.len(),
        )
        .await
        .unwrap();
        assert_eq!(n, 1);
    }
}
