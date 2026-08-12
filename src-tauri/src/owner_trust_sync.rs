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
/// Records the local doc already knows are pre-filtered (skipped silently)
/// so that a `add_*` rejection genuinely means an invalid/suspect record —
/// those are dropped with a **warn** log (trust-boundary signal, per spec
/// §2), without the benign-duplicate spam a bare re-merge would produce.
///
/// Changed-detection is canonical-encode compare — trust docs are tiny, and
/// this stays correct regardless of which individual `add_*` calls were
/// idempotent no-ops vs. real inserts.
pub fn merge_trust_remote_into_local(local: &mut OwnerState, remote: OwnerState) -> MergeOutcome {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    merge_trust_remote_into_local_at(local, remote, now)
}

/// Clock-injectable inner of [`merge_trust_remote_into_local`] (ZEB-854 test
/// seam). `now` is epoch **seconds**; `0` marks an unreadable/pre-epoch local
/// clock (fail-open at the liveness forward-skew bound below).
fn merge_trust_remote_into_local_at(
    local: &mut OwnerState,
    remote: OwnerState,
    now: u64,
) -> MergeOutcome {
    let before = harmony_owner::cbor::to_canonical(&*local).ok();
    let OwnerState {
        owner_id: _,
        enrollments,
        vouching,
        revocations,
        liveness,
    } = remote;
    for (id, cert) in enrollments {
        // Enrollment certs are immutable per device in v1 — a known id is a
        // benign re-send, not a candidate update.
        if local.enrollments.contains_key(&id) {
            continue;
        }
        if let Err(e) = local.add_enrollment(cert, now, DEFAULT_ACTIVE_WINDOW_SECS) {
            tracing::warn!(error = %e, "trust merge: enrollment dropped");
        }
    }
    for cert in revocations.iter() {
        if local.is_revoked(cert.target) {
            continue;
        }
        if let Err(e) = local.add_revocation(cert.clone(), now, DEFAULT_ACTIVE_WINDOW_SECS) {
            tracing::warn!(error = %e, "trust merge: revocation dropped");
        }
    }
    for cert in vouching.iter() {
        // LWW per (signer, target): skip anything not strictly newer than
        // what we already hold from that signer about that target.
        let known_newer = local.vouching.iter().any(|v| {
            v.signer == cert.signer && v.target == cert.target && v.issued_at >= cert.issued_at
        });
        if known_newer {
            continue;
        }
        if let Err(e) = local.add_vouching(cert.clone()) {
            tracing::warn!(error = %e, "trust merge: vouching dropped");
        }
    }
    for (id, cert) in liveness {
        // ZEB-854: harmony-owner's freshness reads (trust.rs / state.rs) are
        // one-sided lower bounds, so a sibling cert stamped in our future reads
        // as "active"/"fresh" forever. Reject a beyond-tolerance future stamp at
        // this ingest funnel — the sibling-side mirror of the ZEB-721 self-cert
        // ClockRegressed guard, extending the ZEB-847 reject-at-ingest pattern to
        // this (trust) merge. Fail-open when our own clock is unreadable (now == 0).
        if crate::clock_trust::secs_exceeds_forward_skew(cert.timestamp, now) {
            // Attribute the reject so an operator can tell WHICH sibling is
            // misbehaving and by how much (this branch only fires when now != 0).
            tracing::warn!(
                device = %hex::encode(id),
                cert_ts = cert.timestamp,
                now,
                skew_secs = cert.timestamp.saturating_sub(now),
                "trust merge: sibling liveness cert rejected (future-stamped beyond skew tolerance)"
            );
            continue;
        }
        let known_newer = local
            .liveness
            .get(&id)
            .is_some_and(|l| l.timestamp >= cert.timestamp);
        if known_newer {
            continue;
        }
        if let Err(e) = local.add_liveness(cert) {
            tracing::warn!(error = %e, "trust merge: liveness dropped");
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
        // `save_owner_state_cbor_only` requires OWNER_STATE_WRITE_LOCK —
        // the engine persists from spawn_blocking, so an unlocked write
        // here could race pairing/mint writers and clobber a newer
        // enrollment (Qodo PR #451). Sync lock in a sync context; held
        // across both file writes so doc + replay stay a consistent pair.
        let _guard = crate::owner_commands::OWNER_STATE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Load-merge-save: the engine snapshot can be STALER than disk —
        // pairing writes a fresh enrollment straight to owner_state.cbor,
        // and the resident doc only learns it at the get_pairing_state
        // resync. Saving the raw snapshot here would clobber that
        // enrollment before the resync ever reads it (Greptile PR #451
        // P1). Folding disk into the snapshot first makes the write
        // monotonic: nothing either side knows is ever lost.
        let merged = match load_owner_state_cbor(&self.identity_dir) {
            Ok(mut disk) => {
                let _ = merge_trust_remote_into_local(&mut disk, state.clone());
                disk
            }
            Err(e) => {
                // Missing/corrupt disk state: the snapshot is the best
                // truth we have; save it rather than failing the persist.
                tracing::warn!(error = %e,
                    "trust persist: could not load disk state before save; using engine snapshot");
                state.clone()
            }
        };
        save_owner_state_cbor_only(&self.identity_dir, &merged).map_err(SyncError::Persist)?;
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
            // Serialize the load-mutate-save against every other
            // owner_state.cbor writer (mint / pairing / get_owner_state) —
            // same lock they hold. No `.await` occurs inside the guard
            // scope, so holding a std guard here is safe in async context.
            let _guard = crate::owner_commands::OWNER_STATE_WRITE_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut state = load_owner_state_cbor(&identity_dir)?;
            let r = f(&mut state);
            save_owner_state_cbor_only(&identity_dir, &state)?;
            Ok(r)
        }
    }
}

/// Fold the on-disk trust doc into the resident doc (idempotent — the
/// merge is monotonic, so re-folding already-known records is a no-op).
/// Used after flows that write `owner_state.cbor` directly while the node
/// runs (pairing install writes through `save_owner_state_atomic`, not the
/// resident doc). Returns whether anything new was learned; nudges the
/// engine only then.
pub async fn resync_trust_from_disk(
    doc: &Arc<tokio::sync::Mutex<OwnerState>>,
    engine: &Arc<crate::fleet_sync::FleetSyncEngine<OwnerState>>,
    identity_dir: &Path,
) -> Result<bool, String> {
    let disk = load_owner_state_cbor(identity_dir)?;
    let changed = {
        let mut guard = doc.lock().await;
        merge_trust_remote_into_local(&mut guard, disk).changed
    };
    if changed {
        engine.notify_dirty();
    }
    Ok(changed)
}

/// One-shot halt used by the revoked-self detector: lib.rs packs the
/// shutdowns of every fleet engine a revoked device must stop driving
/// (owner-state, fleet-net, trust) into this closure.
pub type HaltFn =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

/// The device-set fingerprint gating `owner-devices-updated`: the pair of
/// enrolled ids and revoked ids. Liveness/vouching-only merges leave it
/// unchanged, so the event fires only when the panel's device rows
/// actually change (spec §2 event contract; CodeRabbit PR #451).
fn device_set_fingerprint(
    doc: &OwnerState,
) -> (
    std::collections::BTreeSet<[u8; 16]>,
    std::collections::BTreeSet<[u8; 16]>,
) {
    (
        doc.enrollments.keys().copied().collect(),
        doc.revocations.iter().map(|c| c.target).collect(),
    )
}

/// The trust engine's `on_applied` consumer. Each nudge (an inbound merge
/// that changed local state) emits `owner-devices-updated` when the device
/// set (enrollments/revocations) changed; if the merge revealed THIS
/// device revoked, latch the flag, emit `device-revoked-self` (exactly
/// once), and run the halt closure. Halting from here is safe: `on_applied`
/// only try_sends the nudge and returns, so the engine task is free to
/// process its own shutdown message while we await it.
///
/// The halt is HYGIENE, not the security boundary: a queued publish may
/// still slip out between the merge that delivered the revocation and this
/// task running. That is acceptable — enforcement is receiver-side (every
/// sibling/community that has merged the revocation rejects the key), and
/// S2's revoke IPC orders sign → flush → halt deterministically for the
/// self-revoke path.
pub fn spawn_trust_applied_task(
    mut nudge_rx: tokio::sync::mpsc::Receiver<()>,
    doc: Arc<tokio::sync::Mutex<OwnerState>>,
    self_device_id: [u8; 16],
    revoked_flag: Arc<std::sync::atomic::AtomicBool>,
    emit: Arc<dyn Fn(&str) + Send + Sync>,
    halt: HaltFn,
) -> tokio::task::JoinHandle<()> {
    let mut halt = Some(halt);
    tokio::spawn(async move {
        let mut last_fp = device_set_fingerprint(&*doc.lock().await);
        while nudge_rx.recv().await.is_some() {
            let (fp, revoked) = {
                let g = doc.lock().await;
                (device_set_fingerprint(&g), g.is_revoked(self_device_id))
            };
            if fp != last_fp {
                emit("owner-devices-updated");
                last_fp = fp;
            }
            if revoked && !revoked_flag.swap(true, std::sync::atomic::Ordering::AcqRel) {
                tracing::warn!(
                    "trust merge revealed this device revoked — halting fleet participation"
                );
                emit("device-revoked-self");
                if let Some(h) = halt.take() {
                    h().await;
                }
            }
        }
    })
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
        let res =
            enroll_via_master(state, artifact, &sk, pkb, now, DEFAULT_ACTIVE_WINDOW_SECS).unwrap();
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
        remote_revoked
            .add_revocation(rev, now + 20, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();

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
    fn merge_rejects_future_stamped_sibling_liveness() {
        // ZEB-854: a sibling cert stamped far in our future must NOT enter
        // local.liveness — else it reads "active"/"fresh" forever in
        // harmony-owner's one-sided freshness checks.
        let now = 1_700_000_000u64;
        let (mut local, artifact, _sk1) = test_mint(now);
        let (sk2, cert2) = test_enroll_second_device(&artifact, &local, now + 10);
        let d2 = cert2.device_id;
        local
            .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let owner_id = local.owner_id;

        let mut remote = local.clone();
        remote
            .add_liveness(test_liveness(&sk2, owner_id, now + 3600)) // +1h ≫ 5-min tol
            .unwrap();

        merge_trust_remote_into_local_at(&mut local, remote, now);
        assert!(!local.liveness.contains_key(&d2));
    }

    #[test]
    fn merge_accepts_in_window_sibling_liveness() {
        let now = 1_700_000_000u64;
        let (mut local, artifact, _sk1) = test_mint(now);
        let (sk2, cert2) = test_enroll_second_device(&artifact, &local, now + 10);
        let d2 = cert2.device_id;
        local
            .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let owner_id = local.owner_id;

        let mut remote = local.clone();
        remote
            .add_liveness(test_liveness(&sk2, owner_id, now + 60)) // within 5-min tol
            .unwrap();

        merge_trust_remote_into_local_at(&mut local, remote, now);
        assert!(local.liveness.contains_key(&d2));
    }

    #[test]
    fn merge_fails_open_on_unreadable_local_clock() {
        // now == 0 (pre-epoch/unreadable) ⇒ apply-all: even a future cert is
        // accepted, so a bad LOCAL clock never drops honest sibling state.
        let now = 1_700_000_000u64;
        let (mut local, artifact, _sk1) = test_mint(now);
        let (sk2, cert2) = test_enroll_second_device(&artifact, &local, now + 10);
        let d2 = cert2.device_id;
        local
            .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let owner_id = local.owner_id;

        let mut remote = local.clone();
        remote
            .add_liveness(test_liveness(&sk2, owner_id, now + 3600))
            .unwrap();

        merge_trust_remote_into_local_at(&mut local, remote, 0);
        assert!(local.liveness.contains_key(&d2));
    }

    #[test]
    fn merge_reject_does_not_clobber_stored_honest_liveness() {
        // A stored honest cert must survive an incoming future cert for the same
        // signer (reject fires before the LWW branch).
        let now = 1_700_000_000u64;
        let (mut local, artifact, _sk1) = test_mint(now);
        let (sk2, cert2) = test_enroll_second_device(&artifact, &local, now + 10);
        let d2 = cert2.device_id;
        local
            .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let owner_id = local.owner_id;
        local
            .add_liveness(test_liveness(&sk2, owner_id, now + 30)) // stored honest cert
            .unwrap();

        let mut remote = local.clone();
        remote
            .add_liveness(test_liveness(&sk2, owner_id, now + 3600)) // future overwrite attempt
            .unwrap();

        merge_trust_remote_into_local_at(&mut local, remote, now);
        assert_eq!(local.liveness.get(&d2).unwrap().timestamp, now + 30);
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
    fn trust_persist_preserves_newer_disk_enrollment() {
        // Greptile PR #451 P1: pairing writes a fresh enrollment straight
        // to disk; a pending engine persist carrying an OLDER snapshot
        // must not clobber it. The persist load-merge-saves, so the disk
        // file ends up as the union.
        let dir = tempfile::tempdir().unwrap();
        let now = 1_700_000_000u64;
        let (old_snapshot, artifact, _sk) = test_mint(now);
        // Disk gains a second enrollment the snapshot doesn't know.
        let (_sk2, cert2) = test_enroll_second_device(&artifact, &old_snapshot, now + 10);
        let mut disk_state = old_snapshot.clone();
        disk_state
            .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        save_owner_state_cbor_only(dir.path(), &disk_state).unwrap();

        let persist = TrustPersist {
            identity_dir: dir.path().to_path_buf(),
            replay_path: dir.path().join(OWNER_TRUST_REPLAY_FILENAME),
        };
        FleetPersist::persist(&persist, &old_snapshot, &BTreeMap::new()).unwrap();

        let reloaded = load_owner_state_cbor(dir.path()).unwrap();
        assert_eq!(
            reloaded.enrollments.len(),
            2,
            "persist must not lose the disk-fresh enrollment"
        );
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

    /// Two trust engines over crossed in-memory channels sharing one CAS —
    /// the owner_state_sync `TwoDevices` harness recipe, retargeted at the
    /// generic engine + trust doc.
    struct TrustPair {
        a_engine: Arc<crate::fleet_sync::FleetSyncEngine<OwnerState>>,
        b_engine: Arc<crate::fleet_sync::FleetSyncEngine<OwnerState>>,
        a_doc: Arc<tokio::sync::Mutex<OwnerState>>,
        b_doc: Arc<tokio::sync::Mutex<OwnerState>>,
        _dir: tempfile::TempDir,
    }

    fn spawn_trust_pair(seeded: OwnerState) -> TrustPair {
        use crate::content_store::{ContentStore, InMemoryStub};
        use crate::fleet_sync::{FleetSyncConfig, FleetSyncEngine};
        use crate::owner_state_crypto::KeyTree;
        use tokio::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let kt = Arc::new(KeyTree::derive(&[0x66u8; 32]).expect("kt"));
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let a_doc = Arc::new(tokio::sync::Mutex::new(seeded.clone()));
        let b_doc = Arc::new(tokio::sync::Mutex::new(seeded));

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

        let mk = |name: &str,
                  doc: &Arc<tokio::sync::Mutex<OwnerState>>,
                  pub_tx: mpsc::Sender<Vec<u8>>,
                  sub_rx: mpsc::Receiver<Vec<u8>>| {
            Arc::new(FleetSyncEngine::new(FleetSyncConfig {
                keys: Some(crate::owner_state_crypto::FleetKeySet::new(Arc::clone(&kt))),
                device_id: name.to_string(),
                state: Arc::clone(doc),
                merger: trust_merger(),
                replay_tracker: Arc::new(tokio::sync::Mutex::new(
                    harmony_crdt_sync::ReplayTracker::new(name.to_string()),
                )),
                content_store: Arc::clone(&store),
                publisher_tx: pub_tx,
                subscriber_rx: sub_rx,
                persist: Arc::new(TrustPersist {
                    identity_dir: dir.path().join(name),
                    replay_path: dir.path().join(format!("{name}-replay.cbor")),
                }),
                lookup_key_tag: OWNER_TRUST_LOOKUP_TAG,
                debounce_ms: 50,
                publish_seen: true,
                on_applied: None,
                sibling_acks: Arc::new(tokio::sync::Mutex::new(
                    harmony_crdt_sync::MonotoneMap::new(),
                )),
                adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
            }))
        };
        // Per-engine persist dirs must exist (save_owner_state_cbor_only
        // writes into identity_dir directly).
        std::fs::create_dir_all(dir.path().join("dev-a")).unwrap();
        std::fs::create_dir_all(dir.path().join("dev-b")).unwrap();
        let a_engine = mk("dev-a", &a_doc, a_pub_tx, a_sub_rx);
        let b_engine = mk("dev-b", &b_doc, b_pub_tx, b_sub_rx);
        TrustPair {
            a_engine,
            b_engine,
            a_doc,
            b_doc,
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn two_engines_converge_on_revocation() {
        // Device A revokes device B; B's doc must converge to
        // is_revoked(B) through the replication path.
        let now = 1_700_000_000u64;
        let (state_a, artifact, _sk1) = test_mint(now);
        let (_sk_b, cert_b) = test_enroll_second_device(&artifact, &state_a, now + 10);
        let d_b = cert_b.device_id;
        let mut seeded = state_a;
        seeded
            .add_enrollment(cert_b, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let pair = spawn_trust_pair(seeded);

        let rev = test_master_revocation(&artifact, d_b, now + 20);
        mutate_trust_state(
            TrustStateAccess::Resident {
                doc: Arc::clone(&pair.a_doc),
                engine: Arc::clone(&pair.a_engine),
            },
            move |s| {
                s.add_revocation(rev, now + 20, DEFAULT_ACTIVE_WINDOW_SECS)
                    .unwrap()
            },
        )
        .await
        .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if pair.b_doc.lock().await.is_revoked(d_b) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "B never converged on the revocation"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = pair.a_engine.shutdown().await;
        let _ = pair.b_engine.shutdown().await;
    }

    fn test_halt(halted: Arc<std::sync::atomic::AtomicBool>) -> HaltFn {
        Box::new(move || {
            Box::pin(async move {
                halted.store(true, std::sync::atomic::Ordering::Release);
            })
        })
    }

    fn collecting_emit(
        events: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Arc<dyn Fn(&str) + Send + Sync> {
        Arc::new(move |name: &str| events.lock().unwrap().push(name.to_string()))
    }

    #[tokio::test]
    async fn applied_task_gates_devices_updated_on_device_set_change() {
        let now = 1_700_000_000u64;
        let (state, artifact, sk1) = test_mint(now);
        let self_id = *state.enrollments.keys().next().unwrap();
        let owner_id = state.owner_id;
        let doc = Arc::new(tokio::sync::Mutex::new(state));
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let halted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _h = spawn_trust_applied_task(
            rx,
            Arc::clone(&doc),
            self_id,
            Arc::clone(&flag),
            collecting_emit(Arc::clone(&events)),
            test_halt(Arc::clone(&halted)),
        );
        // Give the task a beat to record its baseline fingerprint before
        // the doc mutates.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Nudge with NO device-set change → no event.
        tx.send(()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(events.lock().unwrap().is_empty());

        // Enrollment added → nudge → exactly one owner-devices-updated.
        let (_sk2, cert2) = {
            let g = doc.lock().await;
            test_enroll_second_device(&artifact, &g, now + 10)
        };
        doc.lock()
            .await
            .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        tx.send(()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(events.lock().unwrap().as_slice(), ["owner-devices-updated"]);

        // Liveness-only refresh → nudge → NO further event (event contract:
        // fires only when the device set changes).
        doc.lock()
            .await
            .add_liveness(test_liveness(&sk1, owner_id, now + 20))
            .unwrap();
        tx.send(()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(events.lock().unwrap().as_slice(), ["owner-devices-updated"]);
        assert!(!flag.load(std::sync::atomic::Ordering::Acquire));
        assert!(!halted.load(std::sync::atomic::Ordering::Acquire));
    }

    #[tokio::test]
    async fn applied_task_detects_self_revocation_halts_and_fires_once() {
        let now = 1_700_000_000u64;
        let (mut state, artifact, _sk1) = test_mint(now);
        let self_id = *state.enrollments.keys().next().unwrap();
        // Second device so the fixture doesn't revoke the only enrollment.
        let (_skb, cert_b) = test_enroll_second_device(&artifact, &state, now + 5);
        state
            .add_enrollment(cert_b, now + 5, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let doc = Arc::new(tokio::sync::Mutex::new(state));
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let halted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _h = spawn_trust_applied_task(
            rx,
            Arc::clone(&doc),
            self_id,
            Arc::clone(&flag),
            collecting_emit(Arc::clone(&events)),
            test_halt(Arc::clone(&halted)),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The revocation arrives AFTER the baseline (as a real merge would).
        let rev = test_master_revocation(&artifact, self_id, now + 10);
        doc.lock()
            .await
            .add_revocation(rev, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        // Two nudges: the second must NOT re-fire device-revoked-self.
        tx.send(()).await.unwrap();
        tx.send(()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let evs = events.lock().unwrap().clone();
        assert!(flag.load(std::sync::atomic::Ordering::Acquire));
        assert!(halted.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            evs.iter().filter(|e| *e == "device-revoked-self").count(),
            1,
            "device-revoked-self must fire exactly once, got {evs:?}"
        );
        assert_eq!(
            evs.iter().filter(|e| *e == "owner-devices-updated").count(),
            1,
            "revocation changes the device set exactly once, got {evs:?}"
        );
    }

    #[tokio::test]
    async fn resync_from_disk_folds_new_enrollment_and_nudges() {
        let now = 1_700_000_000u64;
        let (state, artifact, _sk1) = test_mint(now);
        let pair = spawn_trust_pair(state.clone());

        // Simulate a pairing install: disk gains an enrollment the
        // resident doc doesn't know.
        let dir = tempfile::tempdir().unwrap();
        let (_sk2, cert2) = test_enroll_second_device(&artifact, &state, now + 10);
        let mut disk_state = state;
        disk_state
            .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        save_owner_state_cbor_only(dir.path(), &disk_state).unwrap();

        let changed = resync_trust_from_disk(&pair.a_doc, &pair.a_engine, dir.path())
            .await
            .unwrap();
        assert!(changed);
        assert_eq!(pair.a_doc.lock().await.enrollments.len(), 2);
        // Second resync learns nothing.
        let changed2 = resync_trust_from_disk(&pair.a_doc, &pair.a_engine, dir.path())
            .await
            .unwrap();
        assert!(!changed2);
        let _ = pair.a_engine.shutdown().await;
        let _ = pair.b_engine.shutdown().await;
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
