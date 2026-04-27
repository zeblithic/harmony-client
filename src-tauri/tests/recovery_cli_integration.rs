//! End-to-end recovery CLI tests.
//!
//! Each test:
//!   1. Plants a known seed in a tempdir-rooted identity store.
//!   2. Exports it via mnemonic or recovery file.
//!   3. Wipes the identity store.
//!   4. Restores from the export.
//!   5. Verifies the restored seed yields the same master `identity_hash`.

use harmony_app::{identity, recovery_cli};
use harmony_owner::lifecycle::RecoveryArtifact;
use serial_test::serial;

/// Clear any pre-existing OS keychain entry before each integration test.
/// The `*_cli` functions call the public `identity::read_seed_from_disk` /
/// `write_seed_to_disk` which construct `KeychainStore::new()` internally,
/// so stale keychain state from prior runs leaks across test boundaries.
fn clear_keychain_for_test() {
    if let Ok(kc) = identity::KeychainStore::new() {
        let _ = kc.delete();
    }
}

fn plant_seed(plaintext_path: &std::path::Path, seed: &[u8; 32]) {
    identity::write_seed_to_disk_with_keychain(
        plaintext_path,
        seed,
        /*force=*/ true,
        None,
    )
    .expect("plant");
}

fn wipe_identity_store(plaintext_path: &std::path::Path) {
    let enc_path = plaintext_path.with_file_name("identity.enc");
    let _ = std::fs::remove_file(&enc_path);
}

#[test]
#[serial]
fn mnemonic_round_trip_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");

    clear_keychain_for_test();
    std::env::set_var("HARMONY_PASSPHRASE", "mnemonic-rt");

    let original_seed = [0xA1u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    // Export mnemonic. The unit CLI writes mnemonic to stdout; we replicate
    // that using the library API directly to capture it for restore.
    let seed = identity::read_seed_from_disk(&plaintext_path).unwrap();
    let mnemonic = RecoveryArtifact::from_seed(*seed).to_mnemonic();
    std::fs::write(&mnemonic_path, mnemonic.as_str()).unwrap();

    // Wipe and restore.
    wipe_identity_store(&plaintext_path);
    recovery_cli::restore_mnemonic_cli(&plaintext_path, &mnemonic_path, false)
        .expect("restore");

    let reloaded = identity::read_seed_from_disk(&plaintext_path).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);

    clear_keychain_for_test();
    std::env::remove_var("HARMONY_PASSPHRASE");
}

#[test]
#[serial]
fn recovery_file_round_trip_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let recovery_path = dir.path().join("recovery.bin");

    clear_keychain_for_test();
    std::env::set_var("HARMONY_PASSPHRASE", "recovery-rt");
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "rt-pass");

    let original_seed = [0xB2u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    recovery_cli::export_recovery_file_cli(&plaintext_path, &recovery_path, Some("rt-test"))
        .expect("export");

    wipe_identity_store(&plaintext_path);

    recovery_cli::restore_recovery_file_cli(&plaintext_path, &recovery_path, false)
        .expect("restore");

    let reloaded = identity::read_seed_from_disk(&plaintext_path).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);

    clear_keychain_for_test();
    std::env::remove_var("HARMONY_PASSPHRASE");
    std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
}

#[test]
#[serial]
fn cross_encoding_equivalence_via_cli() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");
    let recovery_path = dir.path().join("recovery.bin");

    clear_keychain_for_test();
    std::env::set_var("HARMONY_PASSPHRASE", "cross-rt");
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "rt-cross");

    let original_seed = [0xC3u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    // Export both ways.
    let seed = identity::read_seed_from_disk(&plaintext_path).unwrap();
    let mnemonic = RecoveryArtifact::from_seed(*seed).to_mnemonic();
    std::fs::write(&mnemonic_path, mnemonic.as_str()).unwrap();
    recovery_cli::export_recovery_file_cli(&plaintext_path, &recovery_path, None)
        .expect("export-recovery");

    // Wipe + restore from mnemonic.
    wipe_identity_store(&plaintext_path);
    recovery_cli::restore_mnemonic_cli(&plaintext_path, &mnemonic_path, false)
        .expect("restore-mnemonic");
    let id_via_m = RecoveryArtifact::from_seed(*identity::read_seed_from_disk(&plaintext_path).unwrap())
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(id_via_m, original_id, "mnemonic restore preserves identity_hash");

    // Wipe + restore from recovery file.
    // Also clear the keychain: restore_mnemonic_cli wrote to it via the public
    // write_seed_to_disk, and we want to prove restore-from-file works with a
    // clean slate (not merely prove force-overwrite works).
    wipe_identity_store(&plaintext_path);
    clear_keychain_for_test();
    recovery_cli::restore_recovery_file_cli(&plaintext_path, &recovery_path, false)
        .expect("restore-recovery");
    let id_via_f = RecoveryArtifact::from_seed(*identity::read_seed_from_disk(&plaintext_path).unwrap())
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(id_via_f, original_id, "recovery-file restore preserves identity_hash");

    clear_keychain_for_test();
    std::env::remove_var("HARMONY_PASSPHRASE");
    std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
}
