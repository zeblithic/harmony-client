//! ZEB-338: self-lifecycle mint IPC integration tests.
//!
//! These exercise `mint_owner_identity_inner` end-to-end against a tempdir
//! identity directory (HOME override). The cbor file is the load-bearing
//! assertion.
//!
//! ## ZEB-428: this file is keychain-hermetic by construction
//!
//! An earlier revision let the mint persist through a real
//! `KeychainStore::new()` — the HOME override scopes the cbor path but NOT
//! the OS keychain (fixed service/account names), and a full-suite run
//! silently overwrote a developer's real owner identity. Now hermetic at
//! two layers: the `_for_test` shim hard-codes `keychain: None` (the mint
//! persists via the HARMONY_PASSPHRASE encrypted-file fallback inside the
//! tempdir), and `KeychainStore::new()` itself refuses in test-fixtures
//! builds (see tests/keychain_isolation.rs).
//!
//! ## Why the inner fn + injected restart, not the real `#[tauri::command]`
//!
//! `mint_owner_identity` restarts the node via `crate::start_node_inner`,
//! which requires a concrete `AppHandle<tauri::Wry>` (it calls
//! `app.path().app_data_dir()`). A headless test binary can only construct
//! `tauri::test::mock_app()` → `App<MockRuntime>`, whose `AppHandle` is
//! `AppHandle<MockRuntime>` — NOT `AppHandle<Wry>` — so the real restart
//! cannot be driven here. Instead we drive the testable inner fn
//! `mint_owner_identity_inner(state, restart)` and inject the `restart`
//! closure: production passes the real `start_node_inner`; here we pass a
//! recording closure (asserts restart-after-mint with cbor on disk) or a
//! deliberately-failing closure (locks the no-rollback invariant from
//! `feedback_metadata_before_irreversible_write` + spec §7.1).
//!
//! All five tests mutate `$HOME`/`$USERPROFILE` (and the ZEB-796 test also
//! `HARMONY_DATA_DIR`) — all process-global — so they run `#[serial]` (the
//! workspace already depends on `serial_test`).
//!
//! ZEB-442: uses `mint_owner_identity_inner_for_test` (gated behind
//! `feature = "test-fixtures"`), so the module self-gates — matching the
//! sibling harness modules — so the consolidated `mint_tests` binary still
//! builds feature-off (the module compiles out, like it did as a standalone
//! binary) instead of failing on the unresolved fixtures-only import.
#![cfg(feature = "test-fixtures")]

use harmony_app::connectivity_settings::ConnectivitySettings;
use harmony_app::friend_graph::PeerIntroPolicy;
use harmony_app::owner_commands::mint_owner_identity_inner_for_test;
use harmony_app::NodeState;
use serial_test::serial;
use std::cell::Cell;
use std::sync::Mutex;
use tempfile::TempDir;

/// RAII guard: sets an env var on construction, restores the previous value
/// (or removes it) on drop — even on panic. Prevents a panicking test from
/// leaking HOME/USERPROFILE into the next `#[serial]` test.
struct EnvVarGuard {
    name: &'static str,
    prev: Option<String>,
}

impl EnvVarGuard {
    fn set(name: &'static str, value: &std::path::Path) -> Self {
        let prev = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self { name, prev }
    }

    fn set_str(name: &'static str, value: &str) -> Self {
        let prev = std::env::var(name).ok();
        std::env::set_var(name, value);
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

/// Set HOME + USERPROFILE to a fresh tempdir so the identity dir resolves
/// there (`identity.rs::resolve_path` reads $HOME / $USERPROFILE), and set
/// HARMONY_PASSPHRASE so the encrypted-file fallback can persist the mint:
/// the shim passes `keychain: None` (ZEB-428), so `save_owner_state_atomic`
/// always writes the HARMONY_PASSPHRASE-backed encrypted file inside this
/// tempdir — never the OS keychain. Returns the tempdir (keep alive for
/// the test) and all three guards.
fn home_override() -> (TempDir, EnvVarGuard, EnvVarGuard, EnvVarGuard) {
    let home = TempDir::new().unwrap();
    let g1 = EnvVarGuard::set("HOME", home.path());
    let g2 = EnvVarGuard::set("USERPROFILE", home.path());
    let g3 = EnvVarGuard::set_str("HARMONY_PASSPHRASE", "mint-lifecycle-test-pp");
    (home, g1, g2, g3)
}

/// Path to the owner_state.cbor under a HOME tempdir (`~/.harmony/owner_state.cbor`).
fn cbor_path(home: &TempDir) -> std::path::PathBuf {
    home.path().join(".harmony").join("owner_state.cbor")
}

/// A `restart` closure that always succeeds (the happy path). Mirrors a
/// successful `start_node_inner`.
fn ok_restart() -> impl FnOnce() -> std::future::Ready<Result<(), String>> {
    || std::future::ready(Ok(()))
}

/// ZEB-796: `connectivity-settings.json` resolves via `resolve_app_data_dir()`
/// (HARMONY_DATA_DIR base → `<base>/net.zeblith.harmony`, profile None — no test
/// here sets HARMONY_PROFILE). Pinning HARMONY_DATA_DIR to a fresh tempdir makes
/// the mint's privacy-posture reset land at a deterministic, cross-platform path
/// we can seed and assert. Returns the tempdir (keep alive), its guard, and the
/// resolved settings-file path. Independent of `home_override` (identity dir uses
/// HOME; settings dir uses HARMONY_DATA_DIR).
fn data_dir_override() -> (TempDir, EnvVarGuard, std::path::PathBuf) {
    let data = TempDir::new().unwrap();
    let guard = EnvVarGuard::set("HARMONY_DATA_DIR", data.path());
    let settings_path = data
        .path()
        .join("net.zeblith.harmony")
        .join("connectivity-settings.json");
    (data, guard, settings_path)
}

#[test]
#[serial]
fn mint_resets_inherited_privacy_posture_preserving_relays() {
    // ZEB-796: minting a fresh identity into an app-data dir that already holds a
    // prior identity's connectivity-settings.json must NOT inherit that posture
    // (the boot-failure-reset / profile-reuse footgun), but MUST keep the
    // machine's relay infrastructure.
    let (_home, _g1, _g2, _g3) = home_override();
    let (_data, _gd, settings_path) = data_dir_override();

    // Seed an "inherited" settings file: every privacy/trust toggle flipped away
    // from its product default, plus a custom relay pool the machine configured.
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    // Every privacy/trust toggle seeded AWAY from its product default. Post
    // ZEB-881 the discoverable default is ON, so the "flipped away" value here is
    // OFF (a prior identity's opt-OUT) — mint must NOT inherit it.
    let inherited = ConnectivitySettings {
        identity_discoverable: false,
        friend_auto_accept_known: false,
        presence_invisible: true,
        peer_intro_policy: PeerIntroPolicy::Closed,
        relays: vec!["https://relay.pkarr.org".to_string()],
        iroh_relays: vec!["https://use1-1.relay.n0.iroh.link".to_string()],
    };
    inherited
        .save(&settings_path)
        .expect("seed inherited settings");

    let state = Mutex::new(NodeState::default());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(mint_owner_identity_inner_for_test(&state, ok_restart()));
    assert!(result.is_ok(), "mint must succeed; got: {result:?}");

    // The mint reset the identity-scoped posture to product defaults...
    let after = ConnectivitySettings::load_or_default(&settings_path);
    assert!(
        after.identity_discoverable,
        "ZEB-881: mint must reset discoverable to the product default ON (not inherit the prior identity's opt-out)"
    );
    assert!(
        after.friend_auto_accept_known,
        "auto-accept-known back to product default ON"
    );
    assert!(!after.presence_invisible, "presence back to visible");
    assert_eq!(after.peer_intro_policy, PeerIntroPolicy::FriendsOfFriends);
    // ...while preserving the machine's relay infrastructure verbatim.
    assert_eq!(after.relays, vec!["https://relay.pkarr.org".to_string()]);
    assert_eq!(
        after.iroh_relays,
        vec!["https://use1-1.relay.n0.iroh.link".to_string()]
    );
}

#[test]
#[serial]
fn mint_owner_identity_writes_cbor() {
    let (home, _g1, _g2, _g3) = home_override();
    let state = Mutex::new(NodeState::default());

    let cbor = cbor_path(&home);
    assert!(!cbor.exists(), "precondition: no owner_state.cbor yet");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(mint_owner_identity_inner_for_test(&state, ok_restart()));

    assert!(result.is_ok(), "mint must succeed; got: {result:?}");
    assert!(
        cbor.exists(),
        "mint must write owner_state.cbor at {}",
        cbor.display()
    );
    // The view should reflect a freshly minted owner with this device enrolled.
    let view = result.unwrap();
    assert_eq!(view.state.devices.len(), 1, "fresh mint enrolls one device");
    assert!(
        view.state.can_back_up,
        "fresh mint retains the master seed (backup possible)"
    );
}

#[test]
#[serial]
fn mint_owner_identity_idempotent_failure_when_already_exists() {
    let (home, _g1, _g2, _g3) = home_override();
    let state = Mutex::new(NodeState::default());
    let rt = tokio::runtime::Runtime::new().unwrap();

    // First mint succeeds and writes the cbor.
    let first = rt.block_on(mint_owner_identity_inner_for_test(&state, ok_restart()));
    assert!(first.is_ok(), "first mint must succeed; got: {first:?}");
    let cbor = cbor_path(&home);
    let before = std::fs::read(&cbor).expect("cbor written after first mint");

    // Second mint must fail with the idempotent "already exists" error...
    let second = rt.block_on(mint_owner_identity_inner_for_test(&state, ok_restart()));
    assert!(second.is_err(), "second mint must fail (already exists)");
    let err = second.unwrap_err();
    assert!(
        err.contains("already exists"),
        "error must mention already-exists; got: {err}"
    );

    // ...and the on-disk identity must be byte-identical (no overwrite, no
    // partial rewrite of the first owner's state).
    let after = std::fs::read(&cbor).expect("cbor still present after failed re-mint");
    assert_eq!(
        before, after,
        "idempotent-failure re-mint must not touch the existing owner_state.cbor"
    );
}

#[test]
#[serial]
fn mint_owner_identity_restarts_node_with_owner_loaded() {
    // We cannot run the real node restart headless (needs AppHandle<Wry>).
    // Instead assert the behavioral contract the real restart depends on:
    // the restart step is invoked exactly once, AFTER the mint, with
    // owner_state.cbor already on disk — i.e. `start_node_inner` would find
    // the owner identity to load. (The real start_node_inner populating
    // crdt_state/dm_outbox/community_registry from that cbor is covered by
    // the Task 3 start_node_inner tests + start_node wiring.)
    let (home, _g1, _g2, _g3) = home_override();
    let state = Mutex::new(NodeState::default());
    let cbor = cbor_path(&home);

    let restart_calls = Cell::new(0u32);
    let cbor_present_at_restart = Cell::new(false);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(mint_owner_identity_inner_for_test(&state, || {
        restart_calls.set(restart_calls.get() + 1);
        cbor_present_at_restart.set(cbor.exists());
        std::future::ready(Ok(()))
    }));

    assert!(result.is_ok(), "mint must succeed; got: {result:?}");
    assert_eq!(
        restart_calls.get(),
        1,
        "restart must be invoked exactly once after a successful mint"
    );
    assert!(
        cbor_present_at_restart.get(),
        "owner_state.cbor must be on disk BEFORE restart so the node loads the owner identity"
    );
    // Default NodeState was never actually started (no Wry app), so it stays
    // stopped — the assertion that matters is the restart-after-persist
    // ordering above.
    assert!(
        !state.lock().unwrap().is_running(),
        "no real node thread is spawned in the headless test"
    );
}

#[test]
#[serial]
fn mint_owner_identity_node_restart_failure_preserves_minted_state() {
    // MOST IMPORTANT TEST: locks the no-rollback invariant
    // (feedback_metadata_before_irreversible_write + spec §7.1). We inject a
    // restart closure that fails AFTER the mint has written the cbor; the
    // mint must NOT be rolled back, and the surfaced error must carry the
    // "Node restart failed after mint:" prefix.
    let (home, _g1, _g2, _g3) = home_override();
    let state = Mutex::new(NodeState::default());
    let cbor = cbor_path(&home);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(mint_owner_identity_inner_for_test(&state, || {
        std::future::ready(Err("simulated start_node_inner failure".to_string()))
    }));

    assert!(result.is_err(), "restart failure must surface as an error");
    let err = result.unwrap_err();
    assert!(
        err.starts_with("Node restart failed after mint:"),
        "error must carry the restart-failure prefix; got: {err}"
    );
    // NO ROLLBACK: the minted identity must remain on disk despite the
    // failed restart. Rolling it back would lose the user's identity.
    assert!(
        cbor.exists(),
        "minted owner_state.cbor must survive a failed restart (no rollback)"
    );
}
