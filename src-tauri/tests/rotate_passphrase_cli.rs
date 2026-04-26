//! Integration test for the rotate-passphrase CLI handler. Tests run against
//! the public `harmony_app::rotate_passphrase_cli` entry point.
//!
//! Note: these tests skip gracefully when the OS keychain already holds an
//! identity, because `rotate_passphrase_cli` correctly refuses keychain
//! installs (refusal-1 takes priority over all later checks). On headless CI
//! or fresh dev machines without an identity in the keychain, all assertions
//! run.

use serial_test::serial;
use std::env;

/// Returns true if the OS keychain has an identity stored — meaning
/// rotate_passphrase_cli will fire refusal-1 before any env/file checks.
fn keychain_has_identity() -> bool {
    // We probe via the same path the CLI uses: KeychainStore::new() + load().
    // If anything fails (no keychain, no entry) we return false.
    // We cannot call internal keychain types directly from integration tests, so
    // we probe by calling rotate_passphrase_cli itself and checking the message.
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("probe.txt");
    std::fs::write(&f, b"probe").unwrap();

    // Temporarily clear env so refusal-2 doesn't fire first.
    let old_pass = env::var("HARMONY_PASSPHRASE").ok();
    let old_file = env::var("HARMONY_PASSPHRASE_FILE").ok();
    env::remove_var("HARMONY_PASSPHRASE");
    env::remove_var("HARMONY_PASSPHRASE_FILE");

    let result = harmony_app::rotate_passphrase_cli(&f);

    // Restore env.
    if let Some(v) = old_pass {
        env::set_var("HARMONY_PASSPHRASE", v);
    }
    if let Some(v) = old_file {
        env::set_var("HARMONY_PASSPHRASE_FILE", v);
    }

    matches!(result, Err(ref e) if e.contains("OS keychain"))
}

#[test]
#[serial]
fn no_old_passphrase_env_refuses() {
    if keychain_has_identity() {
        eprintln!("SKIP: OS keychain has an identity; refusal-1 fires before refusal-2 on this machine");
        return;
    }

    // Clear env so HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE are unset.
    env::remove_var("HARMONY_PASSPHRASE");
    env::remove_var("HARMONY_PASSPHRASE_FILE");

    let dir = tempfile::tempdir().unwrap();
    let new_file = dir.path().join("new.txt");
    std::fs::write(&new_file, b"new_pass").unwrap();

    let err = harmony_app::rotate_passphrase_cli(&new_file).unwrap_err();
    assert!(
        err.contains("HARMONY_PASSPHRASE") && err.contains("not set"),
        "got: {err}"
    );
}

#[test]
#[serial]
fn missing_new_passphrase_file_refuses() {
    if keychain_has_identity() {
        eprintln!("SKIP: OS keychain has an identity; refusal-1 fires before refusal-3 on this machine");
        return;
    }

    env::set_var("HARMONY_PASSPHRASE", "old");

    let dir = tempfile::tempdir().unwrap();
    let bogus = dir.path().join("does_not_exist.txt");

    let err = harmony_app::rotate_passphrase_cli(&bogus).unwrap_err();
    assert!(err.contains("could not be read"), "got: {err}");

    env::remove_var("HARMONY_PASSPHRASE");
}
