//! ZEB-428 regression: test builds must never open the developer's real
//! OS keychain.
//!
//! This file compiles exactly like every other integration test — with the
//! `test-fixtures` feature and WITHOUT `cfg(test)` on the library — which is
//! the precise build class that destroyed a developer identity: a
//! `--workspace --all-targets --features test-fixtures` sweep ran
//! `tests/mint_owner_lifecycle.rs`, whose HOME-to-tempdir redirect scoped the
//! cbor write but not the OS keychain, and the mint silently overwrote the
//! real owner identity in Windows Credential Manager (owner `cb7026bb…`,
//! unrecoverable).
//!
//! The assertion here is the system-level guarantee: in any test-fixtures
//! build, `KeychainStore::new()` refuses, so no test — present or future,
//! injected or not — can reach the real credential store without setting
//! `HARMONY_ALLOW_REAL_KEYCHAIN=1` on purpose.

use harmony_app::identity::KeychainStore;
use serial_test::serial;

#[test]
#[serial]
fn integration_test_builds_cannot_open_the_real_keychain() {
    std::env::remove_var("HARMONY_ALLOW_REAL_KEYCHAIN");
    std::env::remove_var("HARMONY_DISABLE_KEYCHAIN");
    let err = match KeychainStore::new() {
        Ok(_) => panic!(
            "KeychainStore::new() must refuse in test-fixtures builds — \
             this is the gate that prevents the ZEB-428 identity clobber"
        ),
        Err(e) => e,
    };
    assert!(
        err.contains("ZEB-428"),
        "gate error should cite the incident ticket: {err}"
    );
}

#[test]
#[serial]
fn disable_env_refuses_even_with_allow_override() {
    std::env::set_var("HARMONY_DISABLE_KEYCHAIN", "1");
    std::env::set_var("HARMONY_ALLOW_REAL_KEYCHAIN", "1");
    let result = KeychainStore::new();
    std::env::remove_var("HARMONY_DISABLE_KEYCHAIN");
    std::env::remove_var("HARMONY_ALLOW_REAL_KEYCHAIN");
    // Assert the DISABLE branch specifically — a bare is_err() would also
    // pass on the test-build refusal or a backend failure and wouldn't pin
    // the precedence guarantee (disable beats allow).
    let err = match result {
        Ok(_) => panic!("HARMONY_DISABLE_KEYCHAIN must win over the allow override"),
        Err(e) => e,
    };
    assert!(
        err.contains("HARMONY_DISABLE_KEYCHAIN"),
        "expected the disable-branch error, got: {err}"
    );
}
