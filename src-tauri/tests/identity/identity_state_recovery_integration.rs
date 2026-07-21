//! ZEB-213 cross-machine restore integration tests.

#![cfg(feature = "test-fixtures")]

use crate::common;

use common::set_env;
use harmony_app::backup_state;
use harmony_app::identity;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_persist;
use harmony_app::owner_state_types::{Hlc, Space, SpaceId, SpaceKind};
use harmony_app::recovery_cli;
use serial_test::serial;
use tempfile::TempDir;

fn plant_state(harmony_dir: &std::path::Path) -> OwnerState {
    let mut state = OwnerState::default();
    let sp = Space {
        id: SpaceId([0x07; 16]),
        kind: SpaceKind::Folder,
        parent: None,
        community_id: None,
        name: "Demo".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "d".into(),
        },
        updated_at: Hlc {
            wall_ms: 2,
            logical: 0,
            device_id: "d".into(),
        },
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        pending_join_at: None,
    };
    state.spaces.insert(sp.id, sp);
    owner_state_persist::save_crdt(&recovery_cli::owner_state_path(harmony_dir), &state).unwrap();
    state
}

fn setup_machine() -> TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
#[serial]
fn mnemonic_round_trip_still_works_unchanged() {
    // Regression: ZEB-176 mnemonic flow byte-identical after ZEB-213.
    let dir = setup_machine();
    let identity_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("m.txt");
    let _at_rest = set_env("HARMONY_PASSPHRASE", "rt");
    let original = harmony_owner::lifecycle::RecoveryArtifact::from_seed([0xEF; 32]);
    std::fs::write(&mnemonic_path, original.to_mnemonic().as_str()).unwrap();
    let original_id = original.master_pubkey_bundle().identity_hash();
    recovery_cli::restore_mnemonic_with_keychain(&identity_path, &mnemonic_path, false, None)
        .unwrap();
    let reloaded = identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
    let restored = harmony_owner::lifecycle::RecoveryArtifact::from_seed(*reloaded);
    assert_eq!(restored.master_pubkey_bundle().identity_hash(), original_id);
}

#[test]
#[serial]
fn recovery_file_round_trip_with_state() {
    let dir = setup_machine();
    let identity_path = dir.path().join("identity.key");
    let out = dir.path().join("recovery.bin");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "rt-at-rest");
    let _recov = set_env("HARMONY_RECOVERY_PASSPHRASE", "rt-recovery");

    identity::write_seed_to_disk_with_keychain(&identity_path, &[0x12; 32], true, None).unwrap();
    let original_state = plant_state(dir.path());
    let original_bytes = owner_state_persist::canonicalize(&original_state).unwrap();

    recovery_cli::export_recovery_file_pair_with_keychain(
        &identity_path,
        dir.path(),
        &out,
        None,
        None,
        true,
        true,
        None,
    )
    .unwrap();

    // Wipe + restore.
    let _ = std::fs::remove_file(&identity_path);
    let _ = std::fs::remove_file(recovery_cli::owner_state_path(dir.path()));
    recovery_cli::restore_recovery_file_pair_with_keychain(
        &identity_path,
        dir.path(),
        &out,
        /*passphrase=*/ None,
        /*force=*/ true,
        /*ignore_state=*/ false,
        /*keychain=*/ None,
    )
    .unwrap();
    let restored_bytes = std::fs::read(recovery_cli::owner_state_path(dir.path())).unwrap();
    assert_eq!(
        restored_bytes, original_bytes,
        "owner-state must round-trip byte-equal"
    );
}

#[test]
#[serial]
fn legacy_hrmr_only_restores_with_empty_state() {
    let dir = setup_machine();
    let identity_path = dir.path().join("identity.key");
    let out = dir.path().join("legacy.bin");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "rt-legacy");
    let _recov = set_env("HARMONY_RECOVERY_PASSPHRASE", "rt-legacy-rec");

    // Plant a pre-ZEB-213 HRMR by calling export with include_state=false
    // (no sidecar emitted).
    identity::write_seed_to_disk_with_keychain(&identity_path, &[0x34; 32], true, None).unwrap();
    // No plant_state — owner_state_crdt.cbor absent.
    recovery_cli::export_recovery_file_pair_with_keychain(
        &identity_path,
        dir.path(),
        &out,
        None,
        None,
        /*include_state=*/ false,
        /*force=*/ true,
        None,
    )
    .unwrap();
    let sidecar = recovery_cli::sidecar_path(&out);
    assert!(!sidecar.exists(), "no sidecar in legacy mode");

    // Wipe + restore — should succeed with empty owner-state.
    let _ = std::fs::remove_file(&identity_path);
    let result = recovery_cli::restore_recovery_file_pair_with_keychain(
        &identity_path,
        dir.path(),
        &out,
        /*passphrase=*/ None,
        /*force=*/ true,
        /*ignore_state=*/ false,
        /*keychain=*/ None,
    )
    .unwrap();
    assert!(!result.sidecar_present);
    assert_eq!(result.spaces_restored, 0);
}

#[test]
#[serial]
fn cross_machine_state_restore() {
    let machine_a = setup_machine();
    let machine_b = setup_machine();

    let identity_path_a = machine_a.path().join("identity.key");
    let out = machine_a.path().join("recovery.bin");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "rt-cm-rest");
    let _recov = set_env("HARMONY_RECOVERY_PASSPHRASE", "rt-cm-rec");

    identity::write_seed_to_disk_with_keychain(&identity_path_a, &[0x55; 32], true, None).unwrap();
    let original_state = plant_state(machine_a.path());
    let original_bytes = owner_state_persist::canonicalize(&original_state).unwrap();
    recovery_cli::export_recovery_file_pair_with_keychain(
        &identity_path_a,
        machine_a.path(),
        &out,
        None,
        None,
        true,
        true,
        None,
    )
    .unwrap();

    // Move artifacts to machine B's working dir.
    let out_b = machine_b.path().join("recovery.bin");
    std::fs::copy(&out, &out_b).unwrap();
    std::fs::copy(
        recovery_cli::sidecar_path(&out),
        recovery_cli::sidecar_path(&out_b),
    )
    .unwrap();

    let identity_path_b = machine_b.path().join("identity.key");
    recovery_cli::restore_recovery_file_pair_with_keychain(
        &identity_path_b,
        machine_b.path(),
        &out_b,
        /*passphrase=*/ None,
        /*force=*/ true,
        /*ignore_state=*/ false,
        /*keychain=*/ None,
    )
    .unwrap();

    let restored = std::fs::read(recovery_cli::owner_state_path(machine_b.path())).unwrap();
    assert_eq!(restored, original_bytes);
}

#[test]
#[serial]
fn last_backup_record_drives_staleness() {
    let dir = setup_machine();
    let identity_path = dir.path().join("identity.key");
    let out = dir.path().join("recovery.bin");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "rt-stale");
    let _recov = set_env("HARMONY_RECOVERY_PASSPHRASE", "rt-stale-rec");

    identity::write_seed_to_disk_with_keychain(&identity_path, &[0x77; 32], true, None).unwrap();
    plant_state(dir.path());
    recovery_cli::export_recovery_file_pair_with_keychain(
        &identity_path,
        dir.path(),
        &out,
        None,
        None,
        true,
        true,
        None,
    )
    .unwrap();

    let last = backup_state::load_last_backup(&recovery_cli::last_backup_path(dir.path()))
        .unwrap()
        .expect("file present");
    let state =
        owner_state_persist::load_crdt(&recovery_cli::owner_state_path(dir.path())).unwrap();

    // 1 minute later — no mutation, not stale.
    let r = backup_state::should_warn_about_stale_backup(
        last.at.wall_ms + 60_000,
        Some(&last),
        &state,
        None,
    );
    assert!(!r.is_stale);

    // Simulate a mutation by re-saving state with a much-later HLC.
    let mut mutated = state.clone();
    if let Some(s) = mutated.spaces.values_mut().next() {
        s.updated_at = Hlc {
            wall_ms: last.at.wall_ms + 30 * 86_400_000,
            logical: 0,
            device_id: "x".into(),
        };
    }
    let r = backup_state::should_warn_about_stale_backup(
        last.at.wall_ms + 30 * 86_400_000,
        Some(&last),
        &mutated,
        None,
    );
    assert!(r.is_stale, "30d-late mutation + 30d wall clock → stale");
    assert_eq!(r.days_since, 30);
}
