//! ZEB-430 binary-level CLI contract tests.
//!
//! These spawn the REAL `harmony-app` binary (via `CARGO_BIN_EXE_*`). The
//! child is a production build — `KeychainStore::new()` works for real in
//! it — so every child runs with `HARMONY_DISABLE_KEYCHAIN=1` (the ZEB-428
//! operator kill-switch) plus `HOME`/`USERPROFILE` redirected to a tempdir.
//! A test here can never touch the developer's real keychain or `~/.harmony`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serial_test::serial;

/// At-rest passphrase shared between in-process planting (lib calls) and
/// the spawned child, so both resolve the same encrypted-file store.
const CHILD_PASSPHRASE: &str = "cli-contract-test-passphrase";

/// RAII guard: removes the env var on drop, even when an assertion panics
/// mid-test. nextest runs each test in its own process, but plain
/// `cargo test` shares one — without the guard a failing test would leak
/// HARMONY_PASSPHRASE into later tests. Mirrors the EnvVarGuard idiom in
/// the owner_state/owner_commands unit tests.
struct EnvVarGuard(&'static str);

impl EnvVarGuard {
    fn set(name: &'static str, value: &str) -> Self {
        std::env::set_var(name, value);
        Self(name)
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

struct CliOutput {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

fn run_cli(home: &Path, args: &[&str], rust_log: &str) -> CliOutput {
    let mut child = Command::new(env!("CARGO_BIN_EXE_harmony-app"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("HARMONY_DISABLE_KEYCHAIN", "1")
        .env("HARMONY_PASSPHRASE", CHILD_PASSPHRASE)
        .env("RUST_LOG", rust_log)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn harmony-app");

    // Drain both pipes on threads so a chatty child can't deadlock on a
    // full pipe buffer while we poll for exit.
    let mut out_pipe = child.stdout.take().expect("stdout piped");
    let mut err_pipe = child.stderr.take().expect("stderr piped");
    let out_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut out_pipe, &mut b).ok();
        b
    });
    let err_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut err_pipe, &mut b).ok();
        b
    });

    // Kill-guard: a CLI subcommand must exit on its own. On argv parse
    // failure, main.rs deliberately falls through to LAUNCHING THE GUI —
    // without this guard a wiring regression would hang the suite (or
    // flash a window on a desktop machine) instead of failing.
    let deadline = Instant::now() + Duration::from_secs(120);
    let code = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status.code(),
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let stderr = String::from_utf8_lossy(&err_t.join().unwrap()).into_owned();
                let _ = out_t.join();
                panic!(
                    "harmony-app {args:?} did not exit within 120s — the subcommand \
                     likely failed argv parsing and fell through to GUI launch; \
                     stderr:\n{stderr}"
                );
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    CliOutput {
        stdout: String::from_utf8_lossy(&out_t.join().unwrap()).into_owned(),
        stderr: String::from_utf8_lossy(&err_t.join().unwrap()).into_owned(),
        code,
    }
}

/// ZEB-430 finding 3: with tracing initialized, log lines must never land
/// on stdout — `harmony-app export mnemonic > backup.txt` must capture the
/// 24 words and nothing else.
///
/// A fresh install guarantees tracing output exists during the run: the
/// export mints an identity as a documented side effect, and the mint logs
/// at info level ("identity stored in encrypted file"). Before the fix
/// that line landed on STDOUT ahead of the words.
#[test]
#[serial]
fn export_mnemonic_stdout_is_pure_under_rust_log() {
    let home = tempfile::tempdir().unwrap();

    let out = run_cli(home.path(), &["export", "mnemonic"], "info");
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    let line = out.stdout.trim_end_matches('\n');
    assert!(
        !line.contains('\n'),
        "stdout must hold ONLY the 24 words; got:\n{}",
        out.stdout
    );
    assert_eq!(
        line.split(' ').count(),
        24,
        "stdout must be exactly 24 words; got:\n{}",
        out.stdout
    );
    // Must parse as a real mnemonic — catches a log token sharing the line.
    harmony_owner::lifecycle::RecoveryArtifact::from_mnemonic(line)
        .expect("stdout must parse as a BIP39 mnemonic");
    // The warning + fingerprint (and any tracing output) belong on stderr.
    assert!(
        out.stderr.contains("identity-hash:"),
        "stderr must carry the fingerprint; got: {}",
        out.stderr
    );
}

/// ZEB-430 finding 2: `export owner-mnemonic` must exist as a real
/// subcommand and print the OWNER master seed — end-to-end through the
/// shipped binary, including the clap wiring main.rs unit tests can't see.
#[test]
#[serial]
fn export_owner_mnemonic_end_to_end_through_real_binary() {
    use harmony_owner::lifecycle::{mint_owner, MintResult};

    let home = tempfile::tempdir().unwrap();
    let identity_dir = home.path().join(".harmony");
    std::fs::create_dir_all(&identity_dir).unwrap();

    // Plant a minted owner identity where the child will look. The lib-side
    // save runs in THIS process (test-fixtures build: real keychain refuses;
    // keychain is injected as None anyway) against the encrypted-file store,
    // so the passphrase must match what run_cli hands the child.
    let _passphrase_guard = EnvVarGuard::set("HARMONY_PASSPHRASE", CHILD_PASSPHRASE);
    let MintResult {
        state,
        recovery_artifact,
        device_signing_key,
    } = mint_owner(1_700_000_000).unwrap();
    let master_seed = *recovery_artifact.as_bytes();
    harmony_app::owner_state::save_owner_state_atomic(
        &identity_dir,
        &state,
        &device_signing_key,
        Some(&master_seed),
        None,
    )
    .unwrap();

    let out = run_cli(home.path(), &["export", "owner-mnemonic"], "info");
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    // Exact stdout: the planted owner master seed's 24 words, nothing else.
    let expected = harmony_owner::lifecycle::RecoveryArtifact::from_seed(master_seed).to_mnemonic();
    assert_eq!(
        out.stdout,
        format!("{}\n", expected.as_str()),
        "stdout must be exactly the owner mnemonic; stderr: {}",
        out.stderr
    );
    // Fingerprint must be the id the UI shows for this owner.
    assert!(
        out.stderr
            .contains(&format!("owner-id: {}", hex::encode(state.owner_id))),
        "stderr owner-id must match OwnerState.owner_id; got: {}",
        out.stderr
    );
}

/// ZEB-439: `restore owner-mnemonic` must re-adopt the owner identity from a
/// 24-word file end-to-end through the shipped binary — onto an EMPTY install
/// (no `~/.harmony` yet) — and then `export owner-mnemonic` must round-trip the
/// same words with the same owner-id. This is the DoD round-trip through the
/// real binary (clap wiring the main.rs unit tests can't see).
#[test]
#[serial]
fn restore_owner_mnemonic_end_to_end_through_real_binary() {
    use harmony_owner::lifecycle::{mint_owner, MintResult, RecoveryArtifact};

    let home = tempfile::tempdir().unwrap();

    // A paper backup minted elsewhere: a known-valid owner master seed, its 24
    // words, and its owner-id. We do NOT persist any state here — the child
    // restores onto a blank install from the words alone.
    let _passphrase_guard = EnvVarGuard::set("HARMONY_PASSPHRASE", CHILD_PASSPHRASE);
    let MintResult {
        state,
        recovery_artifact,
        ..
    } = mint_owner(1_700_000_000).unwrap();
    let master_seed = *recovery_artifact.as_bytes();
    let owner_id = state.owner_id;
    let words = RecoveryArtifact::from_seed(master_seed).to_mnemonic();
    let mnemonic_path = home.path().join("owner-backup.txt");
    std::fs::write(&mnemonic_path, words.as_str()).unwrap();

    // Empty install: ~/.harmony does not exist yet (restore must create it).
    let identity_dir = home.path().join(".harmony");
    assert!(
        !identity_dir.exists(),
        "test setup: install must start empty"
    );

    let mnemonic_arg = mnemonic_path.to_str().unwrap();
    let out = run_cli(
        home.path(),
        &["restore", "owner-mnemonic", "--mnemonic-file", mnemonic_arg],
        "info",
    );
    assert_eq!(
        out.code,
        Some(0),
        "restore must succeed; stderr: {}",
        out.stderr
    );
    assert!(
        out.stdout.is_empty(),
        "restore writes nothing to stdout; got: {:?}",
        out.stdout
    );
    assert!(
        out.stderr
            .contains(&format!("restored owner-id: {}", hex::encode(owner_id))),
        "stderr must confirm the restored owner-id; got: {}",
        out.stderr
    );

    // DoD round-trip: export from the restored install yields the SAME words
    // and the SAME owner-id.
    let exported = run_cli(home.path(), &["export", "owner-mnemonic"], "info");
    assert_eq!(exported.code, Some(0), "stderr: {}", exported.stderr);
    assert_eq!(
        exported.stdout,
        format!("{}\n", words.as_str()),
        "exported words must round-trip the restored owner seed; stderr: {}",
        exported.stderr
    );
    assert!(
        exported
            .stderr
            .contains(&format!("owner-id: {}", hex::encode(owner_id))),
        "exported owner-id must match the restored identity; got: {}",
        exported.stderr
    );
}

/// ZEB-439: `restore owner-mnemonic` must refuse to clobber an existing owner
/// without `--force` — end-to-end through the binary (non-zero exit, the
/// identity left intact).
#[test]
#[serial]
fn restore_owner_mnemonic_refuses_existing_owner_without_force_through_binary() {
    use harmony_owner::lifecycle::{mint_owner, MintResult, RecoveryArtifact};

    let home = tempfile::tempdir().unwrap();
    let identity_dir = home.path().join(".harmony");
    std::fs::create_dir_all(&identity_dir).unwrap();

    // Plant an existing owner where the child will look.
    let _passphrase_guard = EnvVarGuard::set("HARMONY_PASSPHRASE", CHILD_PASSPHRASE);
    let MintResult {
        state,
        recovery_artifact,
        device_signing_key,
    } = mint_owner(1_700_000_050).unwrap();
    let installed_owner_id = state.owner_id;
    let installed_seed = *recovery_artifact.as_bytes();
    harmony_app::owner_state::save_owner_state_atomic(
        &identity_dir,
        &state,
        &device_signing_key,
        Some(&installed_seed),
        None,
    )
    .unwrap();

    // Back the restore file with a DIFFERENT owner's mnemonic so a silent
    // partial overwrite would be observable (the installed owner-id would
    // change). (CodeRabbit round 1.)
    let MintResult {
        recovery_artifact: other_artifact,
        ..
    } = mint_owner(1_700_000_100).unwrap();
    let other_seed = *other_artifact.as_bytes();
    assert_ne!(
        installed_seed, other_seed,
        "test setup: backup must be a different owner"
    );
    let words = RecoveryArtifact::from_seed(other_seed).to_mnemonic();
    let mnemonic_path = home.path().join("owner-backup.txt");
    std::fs::write(&mnemonic_path, words.as_str()).unwrap();

    let out = run_cli(
        home.path(),
        &[
            "restore",
            "owner-mnemonic",
            "--mnemonic-file",
            mnemonic_path.to_str().unwrap(),
        ],
        "info",
    );
    assert_eq!(
        out.code,
        Some(1),
        "restore must refuse an existing owner without --force; stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("--force"),
        "stderr must point at --force; got: {}",
        out.stderr
    );

    // The installed owner must be untouched — export still returns the ORIGINAL
    // owner-id, proving no partial overwrite leaked through the refusal.
    let exported = run_cli(home.path(), &["export", "owner-mnemonic"], "info");
    assert_eq!(exported.code, Some(0), "stderr: {}", exported.stderr);
    assert!(
        exported
            .stderr
            .contains(&format!("owner-id: {}", hex::encode(installed_owner_id))),
        "installed owner must remain intact after a refused restore; got: {}",
        exported.stderr
    );
}

/// Pins the discoverability half of the rename/document fix: `export --help`
/// must list both seeds so an operator backing up "their identity" can see
/// there are two different artifacts before picking one.
#[test]
#[serial]
fn export_help_lists_both_mnemonic_subcommands() {
    let home = tempfile::tempdir().unwrap();
    let out = run_cli(home.path(), &["export", "--help"], "info");
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    // Exact-token matching: "mnemonic" is a substring of "owner-mnemonic",
    // so a naive contains() would still pass if the plain subcommand
    // disappeared from the help output (CodeAnt round-1 catch). Match the
    // first whitespace-delimited token of each help line instead.
    let lists_subcommand = |name: &str| {
        out.stdout
            .lines()
            .any(|l| l.split_whitespace().next() == Some(name))
    };
    assert!(
        lists_subcommand("owner-mnemonic"),
        "export --help must list owner-mnemonic; got:\n{}",
        out.stdout
    );
    assert!(
        lists_subcommand("mnemonic"),
        "export --help must list the plain mnemonic subcommand; got:\n{}",
        out.stdout
    );
}
