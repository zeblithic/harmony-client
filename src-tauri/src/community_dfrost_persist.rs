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
//! never enter the snapshot type at all — the identity-switch teardown
//! contract (`NodeState.dfrost_logs` doc) requires local DKG secret
//! material to die with the process.
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
//! the three pending slots — interactive ceremony rounds do not survive
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
}
