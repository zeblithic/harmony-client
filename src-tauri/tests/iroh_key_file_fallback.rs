//! ZEB-449: the iroh transport key gets an encrypted-file fallback when no OS
//! keychain backend is available (RPi5 headless, or a run with
//! `HARMONY_DISABLE_KEYCHAIN`). Before this the iroh key was keychain-only, so a
//! keychain-less node booted with transport silently disabled (ZEB-450).
//!
//! Integration-level because it drives the REAL `load_or_create_secret_key`
//! env-resolution wiring — `resolve_path` + `EncryptedFileStore::from_env` +
//! the `vault_app_key_or_create_with_fallback` orchestrator — not just the
//! orchestrator in isolation (that is unit-tested in `identity::vault_tests`).
//!
//! These mutate process-global env (HOME/USERPROFILE/HARMONY_*), so they run
//! `#[serial]` and restore every var on drop via `EnvVarGuard`.

use harmony_app::iroh_endpoint::load_or_create_secret_key;
use serial_test::serial;
use tempfile::TempDir;

/// Sets (or removes) an env var and restores its prior value on drop — even on
/// panic — so a `#[serial]` test never leaks state into the next.
struct EnvVarGuard {
    name: &'static str,
    prev: Option<String>,
}

impl EnvVarGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let prev = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self { name, prev }
    }

    fn unset(name: &'static str) -> Self {
        let prev = std::env::var(name).ok();
        std::env::remove_var(name);
        Self { name, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.name, v),
            None => std::env::remove_var(self.name),
        }
    }
}

fn harmony_dir(home: &TempDir) -> std::path::PathBuf {
    home.path().join(".harmony")
}

#[test]
#[serial]
fn iroh_key_persists_to_encrypted_file_without_keychain_and_is_stable_across_boots() {
    let home = TempDir::new().unwrap();
    // The real first boot creates `~/.harmony` when minting the identity seed
    // before the iroh endpoint starts; mirror that precondition here.
    std::fs::create_dir_all(harmony_dir(&home)).unwrap();
    let _g_home = EnvVarGuard::set("HOME", home.path().to_str().unwrap());
    let _g_up = EnvVarGuard::set("USERPROFILE", home.path().to_str().unwrap());
    let _g_kc = EnvVarGuard::set("HARMONY_DISABLE_KEYCHAIN", "1");
    let _g_pp = EnvVarGuard::set("HARMONY_PASSPHRASE", "zeb449-iroh-fallback-pp");

    let iroh_enc = harmony_dir(&home).join("iroh.enc");
    assert!(
        !iroh_enc.exists(),
        "precondition: no iroh.enc before first boot"
    );

    // First boot: no keychain, empty file => generate + persist.
    let (key1, fresh1) =
        load_or_create_secret_key().expect("first boot mints the iroh key to the encrypted file");
    assert!(fresh1, "first boot freshly creates the key");
    assert!(
        iroh_enc.exists(),
        "the iroh key persisted to ~/.harmony/iroh.enc on a keychain-less node"
    );

    // Second boot: same file => reload the SAME key, not fresh. The stable
    // EndpointId is the whole point — peers know this node by it.
    let (key2, fresh2) =
        load_or_create_secret_key().expect("second boot loads the iroh key from the file");
    assert!(
        !fresh2,
        "a key reloaded from the file is not freshly created"
    );
    assert_eq!(
        key1.public(),
        key2.public(),
        "the iroh EndpointId is stable across restarts on a keychain-less node"
    );
}

#[test]
#[serial]
fn no_keychain_and_no_passphrase_is_a_loud_error_not_silent_transport_off() {
    let home = TempDir::new().unwrap();
    std::fs::create_dir_all(harmony_dir(&home)).unwrap();
    let _g_home = EnvVarGuard::set("HOME", home.path().to_str().unwrap());
    let _g_up = EnvVarGuard::set("USERPROFILE", home.path().to_str().unwrap());
    let _g_kc = EnvVarGuard::set("HARMONY_DISABLE_KEYCHAIN", "1");
    // No passphrase anywhere => no encrypted-file backend either.
    let _g_pp = EnvVarGuard::unset("HARMONY_PASSPHRASE");
    let _g_pf = EnvVarGuard::unset("HARMONY_PASSPHRASE_FILE");

    let err = load_or_create_secret_key()
        .expect_err("no keychain and no passphrase must be a hard, explicit error");
    let msg = format!("{err}");
    assert!(
        msg.contains("HARMONY_PASSPHRASE") || msg.to_lowercase().contains("keychain"),
        "the error must name the remediation (keychain or passphrase), got: {msg}"
    );
}
