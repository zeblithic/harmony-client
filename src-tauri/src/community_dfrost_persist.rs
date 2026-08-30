//! ZEB-753: D-FROST committee-log disk persistence.
//!
//! Fourth member of the per-community persistence family
//! (`crdt.cbor` / `replay.cbor` / `voting.cbor` → `dfrost.cbor`), same
//! posture throughout: CBOR inner image (plain ciborium, the
//! `voting.cbor` idiom — nothing hashes or signs this file, so the
//! certified canonical encoder is not involved), sealed at rest under
//! the ZEB-982 device cipher (`device_dataset_file` v3 envelope, AAD =
//! the identity-dir-relative path), atomic writes, missing-file →
//! default, corrupt → quarantine + default, `community_id` mismatch →
//! hard error. The error type and seal-label derivation are shared with
//! `community_state_persist` so the whole family stays one contract.
//!
//! ONE deliberate divergence from the older siblings: `dfrost.cbor` is
//! BORN-SEALED (this module shipped after ZEB-982), so there is no
//! legacy-plaintext migration — a plaintext image is rejected and
//! quarantined instead of parsed (see `load_dfrost`). An
//! unauthenticated blob must never become trusted committee state.
//!
//! WHAT IS PERSISTED — and, more importantly, what is not. The snapshot
//! carries the accepted-event set (the `VerifiedLog` contents), the
//! materialized `CommitteeState` (whose secret fields are already
//! `#[serde(skip)]` at the type level: `PendingCeremony.round2_packages`,
//! `PendingSignSession.local_nonces`), and the completed-beacon index.
//! The `DfrostLog`-level secrets (`local_dkg_secret{,2}`,
//! `local_key_package`, `local_pub_key_package`, `local_signing_nonces`)
//! never enter the snapshot type at all. ONE of them — the signing-share
//! scalar inside `local_key_package` — is persisted SEPARATELY in the
//! sealed `dfrost_share.cbor` sidecar (ZEB-1029; see the sidecar section
//! below for the threat-model rationale); everything else dies with the
//! process, as the identity-switch teardown contract
//! (`NodeState.dfrost_logs` doc) requires.
//!
//! WHY events + state are persisted TOGETHER rather than events-only
//! with a boot replay: replaying the log through the apply handlers
//! would reject history the live engine admitted under engine-only
//! context (the stale-replace policy consults wall-clock ceremony quiet
//! time before admitting a replacement `di`; a replay sees only
//! `CeremonyInFlight`). The snapshot is written atomically under one
//! log lock, so the two halves cannot diverge on disk.
//!
//! The restore path (`DfrostLog::from_restored`) additionally clears
//! the four pending slots — interactive ceremony rounds do not survive
//! a restart by design, and their secret halves were never persisted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::community_dfrost_log::{CommitteeState, DfrostLog};
use crate::community_dfrost_types::SignedCommitteeEvent;
use crate::community_state_persist::{seal_label, PersistError};
use crate::device_dataset_file::{read_image, write_image, DeviceCipher, Image, ImageError};
use crate::owner_state_types::SpaceId;

pub(crate) const DFROST_FILENAME: &str = "dfrost.cbor";

/// Where + how the engine seals its snapshots. Carries `identity_dir`
/// (not a final file path) so path derivation stays owned by
/// [`dfrost_path_for`]. Cheap to clone (`DeviceCipher` is `Arc`-backed).
#[derive(Clone)]
pub struct DfrostPersistTarget {
    pub identity_dir: PathBuf,
    pub cipher: DeviceCipher,
}

/// Current snapshot schema version. Bump on breaking layout changes;
/// an unknown version loads as corruption (quarantine + default) so an
/// older build never half-parses a newer layout.
const DFROST_SNAPSHOT_VERSION: u8 = 1;

/// `identity_dir/communities/{id_hex}/dfrost.cbor` — matches the
/// `crdt.cbor` / `voting.cbor` layout.
pub fn dfrost_path_for(identity_dir: &Path, community_id: &SpaceId) -> PathBuf {
    let id_hex = hex::encode(community_id.0);
    identity_dir
        .join("communities")
        .join(id_hex)
        .join(DFROST_FILENAME)
}

/// The on-disk shape. `community_id` is defence-in-depth routing: a
/// misrouted SEALED file already fails the AAD tag (the label binds the
/// community id), so with plaintext images rejected outright this field
/// can only disagree with the label if label derivation itself drifted.
#[derive(Serialize, Deserialize)]
struct DfrostSnapshot {
    version: u8,
    community_id: SpaceId,
    events: Vec<SignedCommitteeEvent>,
    committee_state: CommitteeState,
    /// `BTreeMap` (not the in-memory `HashMap`) so the encode is
    /// byte-stable across saves of unchanged state.
    beacon_index: BTreeMap<[u8; 32], [u8; 32]>,
}

/// An owned, `Send + 'static` snapshot of a `DfrostLog`'s durable
/// subset, built while holding the log lock and then handed to
/// [`write_snapshot`] on a blocking thread — the codebase's
/// snapshot-under-lock / write-off-worker persistence split.
pub struct DfrostLogSnapshot(DfrostSnapshot);

/// Build a [`DfrostLogSnapshot`] from `log`. Clones, no I/O — safe to
/// call under the async log lock.
pub fn snapshot_for_persist(log: &DfrostLog, community_id: &SpaceId) -> DfrostLogSnapshot {
    DfrostLogSnapshot(DfrostSnapshot {
        version: DFROST_SNAPSHOT_VERSION,
        community_id: *community_id,
        events: log.export_events(),
        committee_state: log.committee_state.clone(),
        beacon_index: log.beacon_index.iter().map(|(k, v)| (*k, *v)).collect(),
    })
}

/// Write a snapshot to `path`, sealed + atomic. Blocking I/O — call on
/// the blocking pool.
pub fn write_snapshot(
    cipher: &DeviceCipher,
    path: &Path,
    snapshot: &DfrostLogSnapshot,
) -> Result<(), PersistError> {
    // Plain ciborium (the `voting.cbor` idiom, not `canonical_cbor_encode`):
    // the snapshot is a sealed private file nothing ever hashes or signs,
    // so canonical field order is not load-bearing and the type stays off
    // the certified `CanonicalPayload` registry.
    let mut bytes = Vec::new();
    ciborium::into_writer(&snapshot.0, &mut bytes)
        .map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_image(
        cipher,
        path,
        &seal_label(&snapshot.0.community_id, DFROST_FILENAME),
        &bytes,
    )
    .map_err(PersistError::Io)
}

/// Open the sealed (or legacy plaintext) image, mapping envelope errors
/// onto the family contract: `Io` → hard, `Crypto` → quarantine +
/// `None`, missing → `None`.
fn open_family_image(
    cipher: &DeviceCipher,
    path: &Path,
    label: &str,
) -> Result<Option<Image>, PersistError> {
    match read_image(cipher, path, label) {
        Ok(opt) => Ok(opt),
        Err(ImageError::Io(e)) => Err(PersistError::Io(e)),
        Err(ImageError::Crypto(msg)) => {
            quarantine_corrupted(path, &msg);
            Ok(None)
        }
    }
}

/// Load the persisted committee log from `path`.
///
/// - Missing file → a fresh `DfrostLog` (first-boot / never-persisted
///   is the common case).
/// - Legacy PLAINTEXT image → quarantine + fresh default (CodeRabbit
///   on #774). `dfrost.cbor` is born-sealed — this module and ZEB-982
///   sealing coexisted from the file's first release, so no honest
///   plaintext snapshot can exist. Unlike its older siblings, there is
///   nothing to migrate, and accepting bare CBOR here would let an
///   unauthenticated on-disk blob impersonate committee state (joint
///   vk, verifying shares, beacon outputs) that `from_restored` then
///   trusts without re-verification.
/// - Corrupt content (CBOR decode, AEAD tag, unknown snapshot version)
///   → quarantine + fresh default. Self-heal is the right posture: a
///   lost committee snapshot costs a re-DKG at worst, while refusing to
///   spawn would wedge the community's Tier-3 permanently.
/// - `community_id` mismatch → hard `PersistError::CommunityIdMismatch`,
///   file left in place. With plaintext rejected this is pure
///   defence-in-depth: it can only fire if a sealed image's AAD label
///   and body disagree, i.e. an internal label-derivation bug.
/// - I/O error → hard: the bytes may be fine; the caller should run
///   without persistence armed rather than risk clobbering them.
pub fn load_dfrost(
    cipher: &DeviceCipher,
    path: &Path,
    expected_id: &SpaceId,
) -> Result<DfrostLog, PersistError> {
    let label = seal_label(expected_id, DFROST_FILENAME);
    let image = match open_family_image(cipher, path, &label)? {
        Some(image) => image,
        None => return Ok(DfrostLog::new()),
    };
    if image.was_legacy {
        quarantine_corrupted(
            path,
            "plaintext dfrost.cbor rejected: the file is born-sealed and an \
             unauthenticated snapshot must never be trusted",
        );
        return Ok(DfrostLog::new());
    }
    match ciborium::from_reader::<DfrostSnapshot, _>(image.bytes.as_slice()) {
        Ok(snapshot) => {
            if snapshot.community_id != *expected_id {
                return Err(PersistError::CommunityIdMismatch {
                    found: snapshot.community_id,
                    expected: *expected_id,
                });
            }
            if snapshot.version != DFROST_SNAPSHOT_VERSION {
                quarantine_corrupted(
                    path,
                    &format!(
                        "unknown dfrost snapshot version {} (expected {DFROST_SNAPSHOT_VERSION})",
                        snapshot.version
                    ),
                );
                return Ok(DfrostLog::new());
            }
            Ok(DfrostLog::from_restored(
                snapshot.events,
                snapshot.committee_state,
                snapshot.beacon_index.into_iter().collect(),
            ))
        }
        Err(decode_err) => {
            quarantine_corrupted(path, &decode_err.to_string());
            Ok(DfrostLog::new())
        }
    }
}

// ── ZEB-1029: sealed signing-share sidecar (`dfrost_share.cbor`) ──────────────
//
// Fifth member of the family, and the ONE deliberate exception to the
// "local secret material dies with the process" posture: Jake's 2026-08-29
// product call (ZEB-1029, revisiting the fork ZEB-1027 deferred) persists
// the local FROST signing share so a full-committee restart — the case no
// protocol-side recovery can reach (repair needs ≥ t live share-holders,
// refresh needs every member's old share) — becomes a non-event.
//
// Scope is exactly ONE 32-byte scalar. DKG transcript secrets, nonces,
// repair deltas/sigmas, staged rotations, and pending ceremony slots stay
// never-persisted. The sidecar is sealed under the same ZEB-982 device
// cipher as `dfrost.cbor` (the same master-seed-derived key that already
// guards `identity.key` — which transitively yields a share via repair
// anyway), and the restore install is self-authenticating: the caller
// recomputes `G·x` and requires the committee's consensus verifying-share
// entry to match (`DfrostLog::install_restored_share`), so a stale,
// foreign, or corrupt share fails closed into the RTS repair path.
//
// Write/delete discipline: written ONLY when a share is installed
// (`share_snapshot_for_persist` returns `None` otherwise, and the writer
// leaves the file alone — never delete-on-absence, so a session that
// failed to LOAD the share can never clobber a recoverable file), and
// deleted ONLY when restore-time validation rejects it (the share is
// then provably useless: superseded or corrupt).

pub(crate) const DFROST_SHARE_FILENAME: &str = "dfrost_share.cbor";

/// Current share-sidecar schema version. Unknown version loads as
/// corruption (quarantine + shareless), same rule as the main snapshot.
const DFROST_SHARE_VERSION: u8 = 1;

/// `identity_dir/communities/{id_hex}/dfrost_share.cbor` — alongside
/// `dfrost.cbor`.
pub fn dfrost_share_path_for(identity_dir: &Path, community_id: &SpaceId) -> PathBuf {
    let id_hex = hex::encode(community_id.0);
    identity_dir
        .join("communities")
        .join(id_hex)
        .join(DFROST_SHARE_FILENAME)
}

/// On-disk shape of the share sidecar. `epoch` binds the share to the
/// committee generation that minted it — `install_restored_share`
/// refuses an epoch that doesn't match the restored committee state
/// before it even reaches the cryptographic consensus check.
#[derive(Serialize, Deserialize)]
struct DfrostShareSnapshot {
    version: u8,
    community_id: SpaceId,
    epoch: u64,
    /// The FROST signing-share scalar (canonical Ristretto encoding) —
    /// the ONLY secret in the file. Zeroized on drop.
    signing_share: [u8; 32],
}

impl Drop for DfrostShareSnapshot {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.signing_share.zeroize();
    }
}

/// An owned, `Send + 'static` capture of the local signing share, built
/// under the log lock and written off-worker — same split as
/// [`DfrostLogSnapshot`].
pub struct DfrostShareImage(DfrostShareSnapshot);

/// Capture the local signing share for persistence, or `None` when there
/// is nothing durable to write (no share installed, or committee not
/// active). Clones 32 bytes, no I/O — safe under the async log lock.
pub fn share_snapshot_for_persist(
    log: &DfrostLog,
    community_id: &SpaceId,
) -> Option<DfrostShareImage> {
    if !log.committee_state.active {
        return None;
    }
    let kp = log.local_key_package.as_ref()?;
    let share_vec = kp.signing_share().serialize();
    let mut signing_share = [0u8; 32];
    if share_vec.len() != 32 {
        // Unreachable for Ristretto255; refuse to write a malformed file.
        tracing::error!(
            len = share_vec.len(),
            "dfrost share persist: signing share serialized to a non-32-byte scalar; skipping"
        );
        return None;
    }
    signing_share.copy_from_slice(&share_vec);
    Some(DfrostShareImage(DfrostShareSnapshot {
        version: DFROST_SHARE_VERSION,
        community_id: *community_id,
        epoch: log.committee_state.current_epoch,
        signing_share,
    }))
}

/// Write the share sidecar, sealed + atomic. Blocking I/O — call on the
/// blocking pool. The plaintext encode buffer is zeroized on drop.
pub fn write_share_snapshot(
    cipher: &DeviceCipher,
    path: &Path,
    snapshot: &DfrostShareImage,
) -> Result<(), PersistError> {
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    ciborium::into_writer(&snapshot.0, &mut *bytes)
        .map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_image(
        cipher,
        path,
        &seal_label(&snapshot.0.community_id, DFROST_SHARE_FILENAME),
        &bytes,
    )
    .map_err(PersistError::Io)
}

/// What [`load_share`] hands back: the epoch the share was minted at and
/// the signing-share scalar (zeroized on drop). The caller feeds both to
/// `DfrostLog::install_restored_share`, which owns validation.
pub type RestoredShare = (u64, zeroize::Zeroizing<[u8; 32]>);

/// Load the persisted signing share, returning `(epoch, scalar bytes)`.
///
/// Same family contract as [`load_dfrost`], with "shareless" playing the
/// role of "fresh default" (the node falls back to RTS repair, so every
/// soft failure self-heals): missing → `None`; plaintext (born-sealed,
/// like its sibling) → quarantine + `None`; corrupt / unknown version →
/// quarantine + `None`; `community_id` label/body mismatch → hard error;
/// I/O error → hard error (the caller must not later clobber the file).
pub fn load_share(
    cipher: &DeviceCipher,
    path: &Path,
    expected_id: &SpaceId,
) -> Result<Option<RestoredShare>, PersistError> {
    let label = seal_label(expected_id, DFROST_SHARE_FILENAME);
    let image = match open_family_image(cipher, path, &label)? {
        Some(image) => image,
        None => return Ok(None),
    };
    if image.was_legacy {
        quarantine_corrupted(
            path,
            "plaintext dfrost_share.cbor rejected: the file is born-sealed and an \
             unauthenticated signing share must never be installed",
        );
        return Ok(None);
    }
    match ciborium::from_reader::<DfrostShareSnapshot, _>(image.bytes.as_slice()) {
        Ok(snapshot) => {
            if snapshot.community_id != *expected_id {
                return Err(PersistError::CommunityIdMismatch {
                    found: snapshot.community_id,
                    expected: *expected_id,
                });
            }
            if snapshot.version != DFROST_SHARE_VERSION {
                quarantine_corrupted(
                    path,
                    &format!(
                        "unknown dfrost share version {} (expected {DFROST_SHARE_VERSION})",
                        snapshot.version
                    ),
                );
                return Ok(None);
            }
            Ok(Some((
                snapshot.epoch,
                zeroize::Zeroizing::new(snapshot.signing_share),
            )))
        }
        Err(decode_err) => {
            quarantine_corrupted(path, &decode_err.to_string());
            Ok(None)
        }
    }
}

/// Best-effort removal of a share file that restore-time validation
/// rejected (stale, foreign, or the committee is gone). Idempotent;
/// failures are logged and swallowed — a leftover stale file is re-judged
/// (and re-rejected) on the next restore, never installed.
pub fn remove_share_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            ?path,
            err = %e,
            "dfrost share persist: failed to remove a rejected share file"
        ),
    }
}

/// Move a corrupted file aside under `<path>.corrupt.<unix_ms>` —
/// same dialect as `community_state_persist::quarantine_corrupted`.
fn quarantine_corrupted(path: &Path, decode_err: &str) {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut quarantine = path.as_os_str().to_owned();
    quarantine.push(format!(".corrupt.{suffix}"));
    let quarantine_path = std::path::PathBuf::from(quarantine);
    match std::fs::rename(path, &quarantine_path) {
        Ok(()) => tracing::warn!(
            ?path,
            quarantine = ?quarantine_path,
            error = %decode_err,
            "dfrost persist: corrupted file quarantined; recovering with a fresh committee log"
        ),
        Err(rename_err) => tracing::error!(
            ?path,
            decode_error = %decode_err,
            rename_error = %rename_err,
            "dfrost persist: failed to quarantine corrupted file; recovering with a fresh log anyway"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_dfrost_types::{
        CeremonyInitPayload, DfrostEventKind, SignedCommitteeEvent,
    };
    use crate::device_dataset_file::test_cipher;
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn cid(byte: u8) -> SpaceId {
        SpaceId([byte; 16])
    }

    /// A `di` event that `DfrostLog::apply` accepts on a fresh log.
    fn di_event(actor: OwnerAddr, members: Vec<OwnerAddr>, wall_ms: u64) -> SignedCommitteeEvent {
        let payload = CeremonyInitPayload {
            ceremony_id: [0x42; 32],
            max_signers: members.len() as u16,
            members,
            threshold: 2,
            epoch: 1,
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
            sig: vec![9u8; 64],
        }
    }

    /// A log with one accepted event, an active committee, a completed
    /// beacon, and a pending ceremony (to prove restore clears it).
    fn sample_log() -> DfrostLog {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let mut log = DfrostLog::new();
        log.apply(di_event(alice, vec![alice, bob], 1_000))
            .expect("di applies");
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.joint_verifying_key = Some([0xC4; 32]);
        log.committee_state.members = vec![alice, bob];
        log.committee_state.threshold = 2;
        log.committee_state.max_signers = 2;
        log.committee_state
            .verifying_shares
            .insert(alice, [0xA1; 32]);
        log.committee_state.verifying_shares.insert(bob, [0xB1; 32]);
        log.beacon_index.insert([0x11; 32], [0x22; 32]);
        log
    }

    #[test]
    fn snapshot_roundtrip_restores_durable_subset_and_clears_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dfrost.cbor");
        let cipher = test_cipher();
        let log = sample_log();
        assert!(
            log.committee_state.pending_dkg.is_some(),
            "fixture must carry a pending ceremony"
        );

        let snapshot = snapshot_for_persist(&log, &cid(7));
        write_snapshot(&cipher, &path, &snapshot).unwrap();
        // Sealed on disk: v3 sentinel, not bare CBOR.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(
            raw.first(),
            Some(&crate::device_dataset_file::SEALED_DEVICE_SCHEMA_V3)
        );

        let restored = load_dfrost(&cipher, &path, &cid(7)).unwrap();
        assert_eq!(restored.event_count(), 1, "accepted events restored");
        assert!(restored.committee_state.active);
        assert_eq!(restored.committee_state.current_epoch, 1);
        assert_eq!(
            restored.committee_state.joint_verifying_key,
            log.committee_state.joint_verifying_key
        );
        assert_eq!(
            restored.committee_state.verifying_shares,
            log.committee_state.verifying_shares
        );
        assert_eq!(
            restored.committee_state.identifier_map.len(),
            2,
            "identifier_map rebuilt by the CommitteeStateRaw shim"
        );
        assert_eq!(restored.beacon_index.get(&[0x11; 32]), Some(&[0x22; 32]));
        assert!(
            restored.committee_state.pending_dkg.is_none(),
            "pending ceremony cleared on restore"
        );
        assert!(restored.local_key_package.is_none());
    }

    #[test]
    fn missing_file_loads_fresh_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.cbor");
        let restored = load_dfrost(&test_cipher(), &path, &cid(7)).unwrap();
        assert!(restored.events_is_empty());
        assert!(!restored.committee_state.active);
    }

    #[test]
    fn corrupt_file_quarantines_and_loads_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dfrost.cbor");
        std::fs::write(&path, b"\xff not cbor \xff").unwrap();
        let restored = load_dfrost(&test_cipher(), &path, &cid(7)).unwrap();
        assert!(restored.events_is_empty());
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".corrupt.")),
            "corrupt snapshot quarantined aside"
        );
    }

    /// CodeRabbit on #774: `dfrost.cbor` is born-sealed, so a PLAINTEXT
    /// snapshot — even one whose `community_id` MATCHES — is rejected
    /// and quarantined, never parsed into trusted committee state. An
    /// unauthenticated blob claiming an active committee (joint vk,
    /// verifying shares, beacon outputs) must not survive the load.
    #[test]
    fn matching_id_plaintext_snapshot_rejected_and_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dfrost.cbor");
        let snapshot = snapshot_for_persist(&sample_log(), &cid(7));
        let mut bytes = Vec::new();
        ciborium::into_writer(&snapshot.0, &mut bytes).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        let restored = load_dfrost(&test_cipher(), &path, &cid(7)).unwrap();
        assert!(
            restored.events_is_empty() && !restored.committee_state.active,
            "plaintext snapshot must load as a fresh log, never as committee state"
        );
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".corrupt.")),
            "plaintext snapshot quarantined aside"
        );
    }

    /// Defence-in-depth pin: a SEALED image whose AAD label and body
    /// `community_id` disagree (only reachable through a label-derivation
    /// bug — `write_snapshot` derives the label from the body) surfaces
    /// as the hard `CommunityIdMismatch`, file left in place.
    #[test]
    fn sealed_label_body_id_mismatch_stays_hard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dfrost.cbor");
        let cipher = test_cipher();
        // Body claims community 9; seal it under community 7's label.
        let snapshot = snapshot_for_persist(&sample_log(), &cid(9));
        let mut bytes = Vec::new();
        ciborium::into_writer(&snapshot.0, &mut bytes).unwrap();
        write_image(
            &cipher,
            &path,
            &seal_label(&cid(7), DFROST_FILENAME),
            &bytes,
        )
        .unwrap();

        let err = load_dfrost(&cipher, &path, &cid(7)).unwrap_err();
        assert!(matches!(err, PersistError::CommunityIdMismatch { .. }));
        assert!(path.exists(), "mismatched file left in place");
    }

    /// An unknown snapshot version is corruption, not a parse success:
    /// quarantine + fresh default, so an older build never half-applies
    /// a newer layout.
    #[test]
    fn unknown_snapshot_version_quarantines_and_loads_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dfrost.cbor");
        let cipher = test_cipher();
        let mut snapshot = snapshot_for_persist(&sample_log(), &cid(7));
        snapshot.0.version = 99;
        write_snapshot(&cipher, &path, &snapshot).unwrap();

        let restored = load_dfrost(&cipher, &path, &cid(7)).unwrap();
        assert!(restored.events_is_empty(), "unknown version loads fresh");
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".corrupt.")),
            "unknown-version snapshot quarantined aside"
        );
    }

    // ── ZEB-1029: share sidecar ──────────────────────────────────────────────

    /// Real dealer-generated KeyPackage for the share-sidecar tests.
    fn dealer_kp() -> frost_ristretto255::keys::KeyPackage {
        let (shares, _pkp) = frost_ristretto255::keys::generate_with_dealer(
            3,
            2,
            frost_ristretto255::keys::IdentifierList::Default,
            frost_ristretto255::rand_core::OsRng,
        )
        .expect("dealer keygen");
        frost_ristretto255::keys::KeyPackage::try_from(shares.values().next().unwrap().clone())
            .expect("key package")
    }

    /// A log holding an installed signing share on an active committee —
    /// the shape `share_snapshot_for_persist` captures.
    fn log_with_share() -> (DfrostLog, [u8; 32]) {
        let mut log = sample_log();
        let kp = dealer_kp();
        let mut scalar = [0u8; 32];
        scalar.copy_from_slice(&kp.signing_share().serialize());
        log.local_key_package = Some(kp);
        (log, scalar)
    }

    #[test]
    fn share_sidecar_roundtrip_seals_and_restores_scalar_zeb1029() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DFROST_SHARE_FILENAME);
        let cipher = test_cipher();
        let (log, scalar) = log_with_share();

        let image = share_snapshot_for_persist(&log, &cid(7)).expect("share captured");
        write_share_snapshot(&cipher, &path, &image).unwrap();
        // Sealed on disk: v3 sentinel, never bare CBOR — the scalar must
        // not exist in plaintext anywhere on the substrate.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(
            raw.first(),
            Some(&crate::device_dataset_file::SEALED_DEVICE_SCHEMA_V3)
        );
        assert!(
            !raw.windows(32).any(|w| w == scalar),
            "signing-share scalar must not appear in the sealed image"
        );

        let (epoch, restored) = load_share(&cipher, &path, &cid(7))
            .unwrap()
            .expect("share loads");
        assert_eq!(epoch, log.committee_state.current_epoch);
        assert_eq!(*restored, scalar, "scalar round-trips exactly");
    }

    /// Nothing durable ⇒ `None`: no installed share, or an inactive
    /// committee (a share with nothing to sign for is not persisted).
    #[test]
    fn share_snapshot_none_without_installed_share_zeb1029() {
        assert!(
            share_snapshot_for_persist(&sample_log(), &cid(7)).is_none(),
            "no share installed"
        );
        let (mut log, _) = log_with_share();
        log.committee_state.active = false;
        assert!(
            share_snapshot_for_persist(&log, &cid(7)).is_none(),
            "inactive committee"
        );
    }

    /// Born-sealed, like its sibling: a plaintext share file — even a
    /// well-formed one — is quarantined, never parsed into a secret the
    /// node would then USE for threshold signatures.
    #[test]
    fn plaintext_share_rejected_and_quarantined_zeb1029() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DFROST_SHARE_FILENAME);
        let (log, _) = log_with_share();
        let image = share_snapshot_for_persist(&log, &cid(7)).unwrap();
        let mut bytes = Vec::new();
        ciborium::into_writer(&image.0, &mut bytes).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        assert!(
            load_share(&test_cipher(), &path, &cid(7))
                .unwrap()
                .is_none(),
            "plaintext share must load as shareless"
        );
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".corrupt.")),
            "plaintext share quarantined aside"
        );
    }

    /// Unknown share-schema version ⇒ quarantine + shareless, so an
    /// older build never half-parses a newer layout into key material.
    #[test]
    fn unknown_share_version_quarantines_zeb1029() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DFROST_SHARE_FILENAME);
        let cipher = test_cipher();
        let (log, _) = log_with_share();
        let mut image = share_snapshot_for_persist(&log, &cid(7)).unwrap();
        image.0.version = 99;
        write_share_snapshot(&cipher, &path, &image).unwrap();

        assert!(load_share(&cipher, &path, &cid(7)).unwrap().is_none());
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".corrupt.")),
            "unknown-version share quarantined aside"
        );
    }

    /// Defence-in-depth pin, mirroring the main snapshot: label/body
    /// `community_id` disagreement is the hard `CommunityIdMismatch`.
    #[test]
    fn share_label_body_id_mismatch_stays_hard_zeb1029() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DFROST_SHARE_FILENAME);
        let cipher = test_cipher();
        let (log, _) = log_with_share();
        let image = share_snapshot_for_persist(&log, &cid(9)).unwrap();
        let mut bytes = Vec::new();
        ciborium::into_writer(&image.0, &mut bytes).unwrap();
        write_image(
            &cipher,
            &path,
            &seal_label(&cid(7), DFROST_SHARE_FILENAME),
            &bytes,
        )
        .unwrap();

        let err = load_share(&cipher, &path, &cid(7)).unwrap_err();
        assert!(matches!(err, PersistError::CommunityIdMismatch { .. }));
        assert!(path.exists(), "mismatched share file left in place");
    }

    /// `remove_share_file` is idempotent — the restore path calls it on
    /// rejection without caring whether the file still exists.
    #[test]
    fn remove_share_file_idempotent_zeb1029() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DFROST_SHARE_FILENAME);
        remove_share_file(&path); // nothing there — no panic
        std::fs::write(&path, b"x").unwrap();
        remove_share_file(&path);
        assert!(!path.exists());
        remove_share_file(&path); // again — still fine
    }
}
