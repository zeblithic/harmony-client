//! Integration test for the rotate-passphrase CLI handler. Tests run against
//! the public `harmony_app::rotate_passphrase_cli` entry point.
//!
//! These tests are `#[ignore]`d by default because they probe the real OS
//! keychain via `KeychainStore::new()`, and on macOS that may trigger an
//! interactive Keychain Access prompt that hangs `cargo test` indefinitely
//! when the keychain holds a `harmony` entry created by a prior GUI run.
//!
//! To run them, opt in explicitly:
//!
//!     cargo test --test rotate_passphrase_cli -- --ignored
//!
//! On a fresh dev machine or headless CI runner (no `harmony` keychain
//! entry), they pass. On a machine with a populated keychain they assert
//! refusal-1 fires before the env/file checks rather than hanging.

use serial_test::serial;
use std::env;

#[test]
#[ignore = "probes real OS keychain — may hang on macOS dev machines; opt-in via --ignored"]
#[serial]
fn no_old_passphrase_env_refuses() {
    // Clear env so HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE are unset.
    env::remove_var("HARMONY_PASSPHRASE");
    env::remove_var("HARMONY_PASSPHRASE_FILE");

    let dir = tempfile::tempdir().unwrap();
    let new_file = dir.path().join("new.txt");
    std::fs::write(&new_file, b"new_pass").unwrap();

    let err = harmony_app::rotate_passphrase_cli(&new_file).unwrap_err();
    // On a clean machine: refusal-2 fires (no env var set).
    // On a machine with a keychain identity: refusal-1 fires (also acceptable).
    assert!(
        (err.contains("HARMONY_PASSPHRASE") && err.contains("not set"))
            || err.contains("OS keychain"),
        "got: {err}"
    );
}

#[test]
#[ignore = "probes real OS keychain — may hang on macOS dev machines; opt-in via --ignored"]
#[serial]
fn missing_new_passphrase_file_refuses() {
    env::set_var("HARMONY_PASSPHRASE", "old");

    let dir = tempfile::tempdir().unwrap();
    let bogus = dir.path().join("does_not_exist.txt");

    let err = harmony_app::rotate_passphrase_cli(&bogus).unwrap_err();
    // On a clean machine: refusal-3 fires (file missing).
    // On a machine with a keychain identity: refusal-1 fires (also acceptable).
    assert!(
        err.contains("could not be read") || err.contains("OS keychain"),
        "got: {err}"
    );

    env::remove_var("HARMONY_PASSPHRASE");
}
