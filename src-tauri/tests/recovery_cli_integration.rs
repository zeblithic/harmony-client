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

use harmony_app::{identity, recovery_cli};
use harmony_owner::lifecycle::RecoveryArtifact;
use serial_test::serial;

fn plant_seed(plaintext_path: &std::path::Path, seed: &[u8; 32]) {
    identity::write_seed_to_disk_with_keychain(plaintext_path, seed, /*force=*/ true, None)
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

    std::env::set_var("HARMONY_PASSPHRASE", "mnemonic-rt");

    let original_seed = [0xA1u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    // Export mnemonic. The unit CLI writes mnemonic to stdout; we replicate
    // that using the library API directly to capture it for restore.
    let seed = identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
    let mnemonic = RecoveryArtifact::from_seed(*seed).to_mnemonic();
    std::fs::write(&mnemonic_path, mnemonic.as_str()).unwrap();

    // Wipe and restore.
    wipe_identity_store(&plaintext_path);
    recovery_cli::restore_mnemonic_with_keychain(&plaintext_path, &mnemonic_path, false, None)
        .expect("restore");

    let reloaded = identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);

    std::env::remove_var("HARMONY_PASSPHRASE");
}

#[test]
#[serial]
fn recovery_file_round_trip_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let recovery_path = dir.path().join("recovery.bin");

    std::env::set_var("HARMONY_PASSPHRASE", "recovery-rt");
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "rt-pass");

    let original_seed = [0xB2u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    recovery_cli::export_recovery_file_with_keychain(
        &plaintext_path,
        &recovery_path,
        Some("rt-test"),
        None,
    )
    .expect("export");

    wipe_identity_store(&plaintext_path);

    recovery_cli::restore_recovery_file_with_keychain(&plaintext_path, &recovery_path, false, None)
        .expect("restore");

    let reloaded = identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);

    std::env::remove_var("HARMONY_PASSPHRASE");
    std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
}

#[test]
#[serial]
fn export_mnemonic_words_returns_24_words() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");

    std::env::set_var("HARMONY_PASSPHRASE", "words-test");

    let original_seed = [0xD4u8; 32];
    plant_seed(&plaintext_path, &original_seed);

    let (words, id_hash) = recovery_cli::export_mnemonic_words_with_keychain(&plaintext_path, None)
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

    std::env::remove_var("HARMONY_PASSPHRASE");
}

#[test]
#[serial]
fn restore_mnemonic_from_words_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");

    std::env::set_var("HARMONY_PASSPHRASE", "words-rt");

    let original_seed = [0xD5u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    let (words, _id_hash) =
        recovery_cli::export_mnemonic_words_with_keychain(&plaintext_path, None).expect("export");
    wipe_identity_store(&plaintext_path);

    recovery_cli::restore_mnemonic_from_words_with_keychain(
        &plaintext_path,
        &words,
        /*force=*/ false,
        None,
    )
    .expect("restore from words");

    let reloaded = identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);

    std::env::remove_var("HARMONY_PASSPHRASE");
}

#[test]
#[serial]
fn cross_encoding_equivalence_via_cli() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");
    let recovery_path = dir.path().join("recovery.bin");

    std::env::set_var("HARMONY_PASSPHRASE", "cross-rt");
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "rt-cross");

    let original_seed = [0xC3u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    // Export both ways.
    let seed = identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
    let mnemonic = RecoveryArtifact::from_seed(*seed).to_mnemonic();
    std::fs::write(&mnemonic_path, mnemonic.as_str()).unwrap();
    recovery_cli::export_recovery_file_with_keychain(&plaintext_path, &recovery_path, None, None)
        .expect("export-recovery");

    // Wipe + restore from mnemonic.
    wipe_identity_store(&plaintext_path);
    recovery_cli::restore_mnemonic_with_keychain(&plaintext_path, &mnemonic_path, false, None)
        .expect("restore-mnemonic");
    let id_via_m = RecoveryArtifact::from_seed(
        *identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap(),
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
    wipe_identity_store(&plaintext_path);
    recovery_cli::restore_recovery_file_with_keychain(&plaintext_path, &recovery_path, false, None)
        .expect("restore-recovery");
    let id_via_f = RecoveryArtifact::from_seed(
        *identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap(),
    )
    .master_pubkey_bundle()
    .identity_hash();
    assert_eq!(
        id_via_f, original_id,
        "recovery-file restore preserves identity_hash"
    );

    std::env::remove_var("HARMONY_PASSPHRASE");
    std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
}
