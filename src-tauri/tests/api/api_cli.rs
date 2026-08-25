// tests/api_cli.rs — ZEB-452: `harmony-app api` client ↔ API server.
//
// Boots api::start_server on an ephemeral port against a default
// (not-running) NodeState: the CLI contract is discovery + transport +
// status mapping, not node behavior (tests/api_server.rs owns the
// full-node flow). ZEB-428 hermetic: tempdir HOME + HARMONY_PASSPHRASE,
// and no node is ever started so no identity is written anywhere.

use crate::common;

use std::sync::{Arc, Mutex};

#[tokio::test(flavor = "multi_thread")]
async fn cli_client_drives_rpc_and_event_stream_against_live_server() {
    // ── Phase 1: hermetic env (pattern from tests/api_server.rs) ────────
    let home = tempfile::tempdir().expect("tempdir HOME");
    let home_str = home.path().to_string_lossy().into_owned();
    let _g1 = common::set_env("HOME", &home_str);
    let _g2 = common::set_env("USERPROFILE", &home_str);
    let _g3 = common::set_env("HARMONY_PASSPHRASE", "api-cli-test-pp");
    let _g4 = common::set_env("XDG_DATA_HOME", &format!("{home_str}/xdg-data"));
    let _g5 = common::set_env("APPDATA", &format!("{home_str}/appdata"));

    let data_dir = harmony_app::resolve_app_data_dir().expect("data dir under temp HOME");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    // ── Phase 2: live server, ephemeral port, default NodeState ─────────
    let state = Arc::new(Mutex::new(harmony_app::NodeState::default()));
    let events = harmony_app::api::events::ApiEventSink::new();
    let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = events.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (handle, server_task) = harmony_app::api::start_server(
        &data_dir,
        0,
        state,
        sink,
        events.clone(),
        shutdown_tx.clone(),
        shutdown_rx,
    )
    .await
    .expect("start_server");

    // ── Phase 3: discovery reads what the server wrote ──────────────────
    let d = harmony_app::api::cli::read_discovery(&data_dir).expect("discovery files");
    assert_eq!(
        d.port, handle.bound_port,
        "port file must carry the bound port"
    );

    // ── Phase 4: RPC status mapping ─────────────────────────────────────
    // Unknown command → 404 (CLI exit-1 class).
    let (status, body) = harmony_app::api::cli::rpc_call(&d, "definitely_not_a_command", None)
        .await
        .expect("transport must succeed");
    assert_eq!(status, 404, "body: {body}");
    assert!(body.contains("unknown command"), "{body}");

    // Wrong token → 401.
    let bad = harmony_app::api::cli::Discovery {
        port: d.port,
        token: "wrong-token".into(),
    };
    let (status, body) = harmony_app::api::cli::rpc_call(&bad, "get_owner_state", None)
        .await
        .expect("transport must succeed");
    assert_eq!(status, 401, "body: {body}");
    assert!(body.contains("unauthorized"), "{body}");

    // Happy 200: pre-mint owner state on a fresh profile is JSON null
    // (get_owner_state_impl is disk-backed and node-independent).
    let (status, body) = harmony_app::api::cli::rpc_call(&d, "get_owner_state", None)
        .await
        .expect("transport must succeed");
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(
        body.trim(),
        "null",
        "fresh profile must be pre-mint: {body}"
    );

    // Invalid args JSON is a LOCAL error — no HTTP round-trip.
    let err = harmony_app::api::cli::rpc_call(&d, "get_owner_state", Some("{not json"))
        .await
        .expect_err("local JSON validation must reject");
    assert!(err.contains("not valid JSON"), "{err}");

    // ── Phase 5: event stream end-to-end ────────────────────────────────
    let (frames_tx, mut frames_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let d2 = d.clone();
    let streamer = tokio::spawn(async move {
        harmony_app::api::cli::stream_events(&d2, |frame| {
            let _ = frames_tx.send(frame.to_string());
            true
        })
        .await
    });
    // Condition-based wait for the WS subscription (ZEB-453 lesson: the
    // ceiling is generous, the happy path breaks early).
    for _ in 0..100 {
        if events.receiver_count() > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        events.receiver_count() > 0,
        "WS client must subscribe within 10s"
    );

    use harmony_app::node_event_sink::NodeEventSink as _;
    events.emit("cli-test-1", serde_json::json!({"n": 1}));
    events.emit("cli-test-2", serde_json::json!({"n": 2}));

    let f1 = tokio::time::timeout(std::time::Duration::from_secs(10), frames_rx.recv())
        .await
        .expect("frame 1 within 10s")
        .expect("frame 1");
    let f2 = tokio::time::timeout(std::time::Duration::from_secs(10), frames_rx.recv())
        .await
        .expect("frame 2 within 10s")
        .expect("frame 2");
    let v1: serde_json::Value = serde_json::from_str(&f1).expect("frame 1 is JSON");
    let v2: serde_json::Value = serde_json::from_str(&f2).expect("frame 2 is JSON");
    assert_eq!(v1["event"], "cli-test-1");
    assert_eq!(v2["event"], "cli-test-2");
    assert!(
        v2["seq"].as_u64().expect("seq") > v1["seq"].as_u64().expect("seq"),
        "seq must be monotonic: {f1} then {f2}"
    );

    // ── Phase 5b: consumer-driven stop ends the stream cleanly ──────────
    // CodeAnt PR #242 round-1 finding: `harmony-app api --events | head`
    // must exit when stdout's pipe closes — `on_frame` returning false is
    // that signal, and stream_events must return Ok promptly instead of
    // consuming frames forever.
    let d3 = d.clone();
    let early_stop = tokio::spawn(async move {
        let mut seen = 0u32;
        harmony_app::api::cli::stream_events(&d3, |_frame| {
            seen += 1;
            false // consumer gone after the first frame
        })
        .await
        .map(|()| seen)
    });
    for _ in 0..100 {
        if events.receiver_count() > 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        events.receiver_count() > 1,
        "second WS client must subscribe within 10s"
    );
    events.emit("early-stop-1", serde_json::json!({}));
    events.emit("early-stop-2", serde_json::json!({}));
    let early = tokio::time::timeout(std::time::Duration::from_secs(10), early_stop)
        .await
        .expect("early-stop streamer must end within 10s of refusing a frame")
        .expect("early-stop streamer must not panic");
    assert_eq!(
        early,
        Ok(1),
        "consumer-driven stop must be a clean Ok after exactly one frame"
    );

    // ── Phase 6: graceful shutdown ───────────────────────────────────────
    // Axum's graceful shutdown drains NEW connections but does NOT
    // proactively close established WS connections — the stream task keeps
    // its socket open indefinitely past the server's drain window. Per the
    // authorized-adjustment note in ZEB-452 Task 4, we abort the streamer
    // rather than waiting for a close that never arrives.
    shutdown_tx.send(()).await.expect("request shutdown");
    // Give the server task time to finish its drain (no new requests).
    let join = tokio::time::timeout(std::time::Duration::from_secs(10), server_task)
        .await
        .expect("server task must join within 10s")
        .expect("server task must not panic");
    assert!(join.is_ok(), "graceful shutdown must be Ok: {join:?}");
    // The WS stream stays open past graceful shutdown (axum design); abort.
    streamer.abort();
}
