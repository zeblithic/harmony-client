//! tests/api/watch.rs — ZEB-480: in-process channel-watch backfill + resume.
//!
//! Boots the serve core exactly like `api_server.rs` (temp HOME, keychain-
//! hermetic per ZEB-428, in-process HTTP server on an ephemeral port), mints an
//! owner, creates a community + channel, posts a message, then drives the watch
//! `backfill` against the live node and asserts:
//!
//!   1. the posted message is emitted exactly once as `source:"backfill"`, and
//!   2. a second backfill emits nothing (the HLC cursor advanced + id-dedupe).
//!
//! Together these pin the resume contract deterministically in CI (no subprocess,
//! no real-time clock). The live-stream path is covered by the `handle_frame`
//! unit tests and the `--features e2e` subprocess smoke.

use crate::common;

use std::sync::{Arc, Mutex};

/// Bearer-authed `POST /v1/rpc/{cmd}` helper (mirrors api_server.rs).
async fn rpc(
    http: &reqwest::Client,
    base: &str,
    bearer: &str,
    cmd: &str,
    body: serde_json::Value,
) -> reqwest::Response {
    http.post(format!("{base}/v1/rpc/{cmd}"))
        .header("authorization", bearer)
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST /v1/rpc/{cmd} transport error: {e}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_backfill_emits_then_dedupes() {
    // ── Boot: temp HOME + passphrase (ZEB-428 hermetic), iroh warm-up ───
    let home = tempfile::tempdir().expect("tempdir for HOME override");
    let home_str = home
        .path()
        .to_str()
        .expect("tempdir path is valid utf8")
        .to_string();
    let _g1 = common::set_env("HOME", &home_str);
    let _g2 = common::set_env("USERPROFILE", &home_str);
    let _g3 = common::set_env("HARMONY_PASSPHRASE", "watch-test-pp");
    let _g4 = common::set_env("XDG_DATA_HOME", &format!("{home_str}/xdg-data"));
    let _g5 = common::set_env("APPDATA", &format!("{home_str}/appdata"));

    let data_dir = harmony_app::resolve_app_data_dir().expect("resolve app data dir");
    assert!(
        data_dir.starts_with(home.path()),
        "HOME override must scope the app data dir to the tempdir; got {}",
        data_dir.display()
    );

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    // ── Boot the serve core in-process (mirrors serve_cli / api_server.rs) ─
    let state = Arc::new(Mutex::new(harmony_app::NodeState::default()));
    let events = harmony_app::api::events::ApiEventSink::new();
    let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = events.clone();
    harmony_app::start_node_inner(None, sink.clone(), None, &state, Some(Arc::clone(&state)))
        .await
        .expect("headless node boots without Tauri");

    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (handle, _server_task) = harmony_app::api::start_server(
        &data_dir,
        0,
        state.clone(),
        sink.clone(),
        events.clone(),
        shutdown_tx.clone(),
        shutdown_rx,
    )
    .await
    .expect("api server binds an ephemeral port");

    let token = std::fs::read_to_string(handle.api_dir.join("token"))
        .expect("token discovery file")
        .trim()
        .to_string();
    let base = format!("http://127.0.0.1:{}", handle.bound_port);
    let bearer = format!("Bearer {token}");
    let http = reqwest::Client::new();

    // ── Mint owner (restarts the node), create community + channel, post ──
    let r = rpc(
        &http,
        &base,
        &bearer,
        "mint_owner_identity",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(r.status(), 200, "mint_owner_identity must succeed");

    let r = rpc(
        &http,
        &base,
        &bearer,
        "create_community",
        serde_json::json!({"name": "watch-e2e", "isInviteOnly": false}),
    )
    .await;
    assert_eq!(r.status(), 200, "create_community must succeed");
    let community_id: String = r.json().await.expect("community id string");

    let r = rpc(
        &http,
        &base,
        &bearer,
        "create_channel",
        serde_json::json!({
            "communityId": community_id,
            "name": "team-chat",
            "writePower": 0,
            "kind": "text"
        }),
    )
    .await;
    assert_eq!(r.status(), 200, "create_channel must succeed");
    let channel_id: String = r.json().await.expect("channel id string");

    let r = rpc(
        &http,
        &base,
        &bearer,
        "post_channel_message",
        serde_json::json!({
            "communityId": community_id,
            "channelId": channel_id,
            "body": b"hello watch".to_vec(),
            "replyTo": null
        }),
    )
    .await;
    let post_status = r.status();
    let post_body = r.text().await.unwrap_or_default();
    assert_eq!(
        post_status, 200,
        "post_channel_message must succeed; got {post_status}: {post_body}"
    );

    // ── Drive the watch backfill against the live node ──────────────────
    let d = harmony_app::api::cli::Discovery {
        port: handle.bound_port,
        token,
    };
    let cfg = harmony_app::WatchConfig {
        community_id: community_id.clone(),
        channels: vec![channel_id.clone()],
        since: None,
        cursor_file: None,
        raw: false,
        no_retry: true,
    };
    let mut cursors = harmony_app::api::watch::CursorSet::load(&cfg).expect("cursor set");

    let mut got: Vec<String> = Vec::new();
    harmony_app::api::watch::backfill(&d, &cfg, &mut cursors, &mut |line: &str| {
        got.push(line.to_string());
        true
    })
    .await
    .expect("backfill must succeed against the live node");

    assert_eq!(got.len(), 1, "exactly one backfilled message; got {got:?}");
    let line: serde_json::Value = serde_json::from_str(&got[0]).expect("watch line is JSON");
    assert_eq!(line["source"], "backfill");
    assert_eq!(line["channelId"], channel_id);
    assert_eq!(line["body"], "hello watch");
    assert!(line["seq"].is_null(), "backfill rows carry null seq");

    // ── Second backfill: cursor advanced + dedupe → nothing re-emitted ──
    got.clear();
    harmony_app::api::watch::backfill(&d, &cfg, &mut cursors, &mut |line: &str| {
        got.push(line.to_string());
        true
    })
    .await
    .expect("second backfill succeeds");
    assert!(
        got.is_empty(),
        "cursor + dedupe must suppress re-emission; got {got:?}"
    );
}
