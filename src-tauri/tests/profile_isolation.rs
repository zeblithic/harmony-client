//! tests/profile_isolation.rs — ZEB-446: named-profile storage isolation +
//! degradable Reticulum bind, proven on real headless node boots.
//!
//! Temp-HOME, keychain-hermetic (ZEB-428: `KeychainStore::new()` refuses in
//! test-fixtures builds AND on named profiles; HARMONY_PASSPHRASE routes
//! identity persistence to the encrypted-file store inside the tempdir).
//!
//! Two test fns; nextest gives each its own process, so the profile
//! OnceLock and env guards cannot leak between them.

mod common;

use std::sync::{Arc, Mutex};

/// A named profile scopes BOTH storage roots, and the node boots with
/// Reticulum disabled (HARMONY_RETICULUM_PORT=0) — the coordination-
/// instance configuration from the ZEB-446 recipe.
#[tokio::test(flavor = "multi_thread")]
async fn named_profile_scopes_both_roots_and_boots() {
    let home = tempfile::tempdir().expect("tempdir for HOME override");
    let home_str = home
        .path()
        .to_str()
        .expect("tempdir path is valid utf8")
        .to_string();
    let _g1 = common::set_env("HOME", &home_str);
    let _g2 = common::set_env("USERPROFILE", &home_str);
    let _g3 = common::set_env("HARMONY_PASSPHRASE", "profile-isolation-pp");
    let _g4 = common::set_env("XDG_DATA_HOME", &format!("{home_str}/xdg-data"));
    let _g5 = common::set_env("APPDATA", &format!("{home_str}/appdata"));
    let _g6 = common::set_env("HARMONY_RETICULUM_PORT", "0");

    harmony_app::profile::set_active_profile(Some("coordtest")).expect("activate profile");
    assert_eq!(harmony_app::profile::active_profile(), Some("coordtest"));

    // Read-only path checks BEFORE any boot writes anything.
    let data_dir = harmony_app::resolve_app_data_dir().expect("resolve app data dir");
    assert!(
        data_dir.starts_with(home.path()),
        "HOME override must scope the app data dir to the tempdir; got {}",
        data_dir.display()
    );
    assert!(
        data_dir.ends_with("net.zeblith.harmony/profiles/coordtest"),
        "named profile must nest under profiles/<name>; got {}",
        data_dir.display()
    );
    let identity_path = harmony_app::identity::resolve_path(None).expect("identity path");
    assert!(
        identity_path.ends_with(".harmony/profiles/coordtest/identity.key"),
        "named profile must scope the identity tree; got {}",
        identity_path.display()
    );

    // ZEB-347: the first iroh bind per process pays a one-time global init
    // (~10s CI / ~30s macOS); warm up before the boot below.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    // Boot the serve core exactly as serve_cli does.
    let state = Arc::new(Mutex::new(harmony_app::NodeState::default()));
    let events = harmony_app::api::events::ApiEventSink::new();
    let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = Arc::new(events.clone());
    harmony_app::start_node_inner(None, sink.clone(), None, &state)
        .await
        .expect("node must boot on a named profile with Reticulum disabled");

    // The boot persisted ONLY under the profile trees.
    assert!(
        home.path()
            .join(".harmony")
            .join("profiles")
            .join("coordtest")
            .is_dir(),
        "profile identity tree must exist after boot"
    );
    assert!(
        !home.path().join(".harmony").join("identity.key").exists(),
        "default-profile identity must NOT be touched by a named-profile boot"
    );
}

/// An occupied Reticulum port degrades (warn + boot continues) instead of
/// killing transport init — the ZEB-420/ZEB-165 class, and the exact
/// collision a second local instance produces.
#[tokio::test(flavor = "multi_thread")]
async fn occupied_reticulum_port_degrades_instead_of_failing_boot() {
    let home = tempfile::tempdir().expect("tempdir for HOME override");
    let home_str = home
        .path()
        .to_str()
        .expect("tempdir path is valid utf8")
        .to_string();
    let _g1 = common::set_env("HOME", &home_str);
    let _g2 = common::set_env("USERPROFILE", &home_str);
    let _g3 = common::set_env("HARMONY_PASSPHRASE", "profile-isolation-pp");
    let _g4 = common::set_env("XDG_DATA_HOME", &format!("{home_str}/xdg-data"));
    let _g5 = common::set_env("APPDATA", &format!("{home_str}/appdata"));

    // Deterministic collision without touching the real 4242: pre-bind an
    // ephemeral UDP port and point the node at it.
    let blocker = std::net::UdpSocket::bind("0.0.0.0:0").expect("blocker socket");
    let port = blocker.local_addr().expect("local addr").port();
    let _g6 = common::set_env("HARMONY_RETICULUM_PORT", &port.to_string());

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    let state = Arc::new(Mutex::new(harmony_app::NodeState::default()));
    let events = harmony_app::api::events::ApiEventSink::new();
    let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = Arc::new(events.clone());
    harmony_app::start_node_inner(None, sink.clone(), None, &state)
        .await
        .expect("occupied Reticulum port must degrade, not fail the boot");
    drop(blocker);
}
