//! End-to-end recovery CLI tests.
//!
//! Each test:
//!   1. Plants a known seed in a tempdir-rooted identity store.
//!   2. Exports it via mnemonic or recovery file.
//!   3. Wipes the identity store.
//!   4. Restores from the export.
//!   5. Verifies the restored seed yields the same master `identity_hash`.
//!
//! All calls use the `_with_keychain(None)` injected variants so the tests
//! never read or write the developer's real OS keychain entry.

use crate::common;

use common::set_env;
use harmony_app::{identity, identity_commands, recovery_cli};
use harmony_owner::lifecycle::RecoveryArtifact;
use serial_test::serial;

fn plant_seed(identity_path: &std::path::Path, seed: &[u8; 32]) {
    identity::write_seed_to_disk_with_keychain(identity_path, seed, /*force=*/ true, None)
        .expect("plant");
}

fn wipe_identity_store(identity_path: &std::path::Path) {
    let enc_path = identity_path.with_file_name("identity.enc");
    let _ = std::fs::remove_file(&enc_path);
}

#[test]
#[serial]
fn mnemonic_round_trip_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let identity_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "mnemonic-rt");

    let original_seed = [0xA1u8; 32];
    plant_seed(&identity_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    // Export mnemonic. The unit CLI writes mnemonic to stdout; we replicate
    // that using the library API directly to capture it for restore.
    let seed = identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
    let mnemonic = RecoveryArtifact::from_seed(*seed).to_mnemonic();
    std::fs::write(&mnemonic_path, mnemonic.as_str()).unwrap();

    // Wipe and restore.
    wipe_identity_store(&identity_path);
    recovery_cli::restore_mnemonic_with_keychain(&identity_path, &mnemonic_path, false, None)
        .expect("restore");

    let reloaded = identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);
}

#[test]
#[serial]
fn recovery_file_round_trip_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let identity_path = dir.path().join("identity.key");
    let recovery_path = dir.path().join("recovery.bin");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "recovery-rt");
    let _recov = set_env("HARMONY_RECOVERY_PASSPHRASE", "rt-pass");

    let original_seed = [0xB2u8; 32];
    plant_seed(&identity_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    recovery_cli::export_recovery_file_with_keychain(
        &identity_path,
        &recovery_path,
        Some("rt-test"),
        /*passphrase=*/ None,
        /*keychain=*/ None,
    )
    .expect("export");

    wipe_identity_store(&identity_path);

    recovery_cli::restore_recovery_file_with_keychain(&identity_path, &recovery_path, false, None)
        .expect("restore");

    let reloaded = identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);
}

#[test]
#[serial]
fn export_mnemonic_words_returns_24_words() {
    let dir = tempfile::tempdir().unwrap();
    let identity_path = dir.path().join("identity.key");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "words-test");

    let original_seed = [0xD4u8; 32];
    plant_seed(&identity_path, &original_seed);

    let (words, id_hash) = recovery_cli::export_mnemonic_words_with_keychain(&identity_path, None)
        .expect("export words");
    assert_eq!(words.len(), 24, "BIP39-24 produces exactly 24 words");
    for w in &words {
        assert!(
            !w.is_empty() && w.chars().all(|c: char| c.is_ascii_lowercase()),
            "each word is non-empty lowercase ASCII"
        );
    }
    // The returned hash must match what re-parsing the phrase produces.
    let phrase = words.join(" ");
    let reparsed = RecoveryArtifact::from_mnemonic(&phrase).expect("re-parse");
    assert_eq!(
        id_hash,
        reparsed.master_pubkey_bundle().identity_hash(),
        "returned hash must match artifact derived from words"
    );
}

#[test]
#[serial]
fn restore_mnemonic_from_words_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let identity_path = dir.path().join("identity.key");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "words-rt");

    let original_seed = [0xD5u8; 32];
    plant_seed(&identity_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    let (words, _id_hash) =
        recovery_cli::export_mnemonic_words_with_keychain(&identity_path, None).expect("export");
    wipe_identity_store(&identity_path);

    recovery_cli::restore_mnemonic_from_words_with_keychain(
        &identity_path,
        &words,
        /*force=*/ false,
        None,
    )
    .expect("restore from words");

    let reloaded = identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);
}

#[test]
#[serial]
fn cross_encoding_equivalence_via_cli() {
    let dir = tempfile::tempdir().unwrap();
    let identity_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");
    let recovery_path = dir.path().join("recovery.bin");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "cross-rt");
    let _recov = set_env("HARMONY_RECOVERY_PASSPHRASE", "rt-cross");

    let original_seed = [0xC3u8; 32];
    plant_seed(&identity_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    // Export both ways.
    let seed = identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
    let mnemonic = RecoveryArtifact::from_seed(*seed).to_mnemonic();
    std::fs::write(&mnemonic_path, mnemonic.as_str()).unwrap();
    recovery_cli::export_recovery_file_with_keychain(
        &identity_path,
        &recovery_path,
        /*comment=*/ None,
        /*passphrase=*/ None,
        /*keychain=*/ None,
    )
    .expect("export-recovery");

    // Wipe + restore from mnemonic.
    wipe_identity_store(&identity_path);
    recovery_cli::restore_mnemonic_with_keychain(&identity_path, &mnemonic_path, false, None)
        .expect("restore-mnemonic");
    let id_via_m = RecoveryArtifact::from_seed(
        *identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap(),
    )
    .master_pubkey_bundle()
    .identity_hash();
    assert_eq!(
        id_via_m, original_id,
        "mnemonic restore preserves identity_hash"
    );

    // Wipe + restore from recovery file. With the keychain wired out via
    // None, no separate keychain-clear step is needed — wiping the .enc is
    // a complete reset.
    wipe_identity_store(&identity_path);
    recovery_cli::restore_recovery_file_with_keychain(&identity_path, &recovery_path, false, None)
        .expect("restore-recovery");
    let id_via_f = RecoveryArtifact::from_seed(
        *identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap(),
    )
    .master_pubkey_bundle()
    .identity_hash();
    assert_eq!(
        id_via_f, original_id,
        "recovery-file restore preserves identity_hash"
    );
}

/// GUI helper exports mnemonic words → CLI restores from a word file.
/// Proves the GUI export path produces words that the CLI restore path accepts
/// and that identity hashes agree end-to-end.
#[test]
#[serial]
fn gui_export_mnemonic_restored_via_cli_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let identity_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "gui-export-cli-restore");

    let original_seed = [0xE1u8; 32];
    plant_seed(&identity_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    // GUI path: call the helper directly (no Tauri runtime needed).
    let words = identity_commands::export_mnemonic_words_helper(&identity_path, None)
        .expect("gui export words");
    assert_eq!(words.len(), 24);

    // Write words to a file so the CLI restore_mnemonic_with_keychain can read it.
    std::fs::write(&mnemonic_path, words.join(" ")).unwrap();

    wipe_identity_store(&identity_path);

    // CLI path: restore from the mnemonic file.
    recovery_cli::restore_mnemonic_with_keychain(&identity_path, &mnemonic_path, false, None)
        .expect("cli restore");

    let reloaded = identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(
        reloaded_id, original_id,
        "identity hash must survive gui-export → cli-restore round-trip"
    );
}

/// CLI exports mnemonic words → GUI helper restores from the word array.
/// Proves the CLI export produces words that the GUI restore path accepts and
/// that identity hashes agree end-to-end.
#[test]
#[serial]
fn cli_export_mnemonic_restored_via_gui_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let identity_path = dir.path().join("identity.key");

    let _at_rest = set_env("HARMONY_PASSPHRASE", "cli-export-gui-restore");

    let original_seed = [0xE2u8; 32];
    plant_seed(&identity_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    // CLI path: export words via the with_keychain variant (no real keychain).
    let (words, _id_hash) = recovery_cli::export_mnemonic_words_with_keychain(&identity_path, None)
        .expect("cli export words");

    wipe_identity_store(&identity_path);

    // GUI path: restore via the helper (force=true, no Tauri runtime needed).
    let restored_hash =
        identity_commands::restore_mnemonic_from_words_helper(&identity_path, &words, None)
            .expect("gui restore");

    // The helper returns the hash directly; also verify via disk re-read.
    let reloaded = identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(
        reloaded_id, original_id,
        "identity hash must survive cli-export → gui-restore round-trip"
    );
    assert_eq!(
        restored_hash,
        hex::encode(original_id),
        "helper return value must match on-disk identity hash"
    );
}
