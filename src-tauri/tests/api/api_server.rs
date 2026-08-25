//! tests/api_server.rs — ZEB-445: end-to-end proof the node boots Tauri-free
//! and is drivable over the localhost HTTP+WS control surface.
//!
//! Temp-HOME, keychain-hermetic (ZEB-428: `KeychainStore::new()` refuses in
//! test-fixtures builds; HARMONY_PASSPHRASE routes identity persistence to
//! the encrypted-file store inside the tempdir).
//!
//! ONE test fn: env vars are process-global, and a single fn avoids ordering
//! hazards — nextest gives this binary its own process.
//!
//! ## Owner mint is driven explicitly (designed, not discovered)
//!
//! First boot is pre-mint: `start_node_inner` boots without an owner when
//! `owner_state.cbor` is absent, and nothing in the backend auto-mints. In
//! the GUI the mint fires from the WelcomeModal's "Create identity" click
//! (`owner-gate.ts` → WelcomeModal → `mint_owner_identity` IPC); headless,
//! the same `mint_owner_identity_impl` seam is the `mint_owner_identity`
//! RPC command, and this test drives it over an authed
//! `POST /v1/rpc/mint_owner_identity`. Keychain-hermetic per ZEB-428: the
//! production seam calls `KeychainStore::new().ok()`, which refuses in
//! test-fixtures builds, so the mint persists through the encrypted-file
//! fallback (`HARMONY_PASSPHRASE`) inside the tempdir HOME.

use crate::common;

use futures_util::StreamExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bearer-authed `POST /v1/rpc/{cmd}` helper. Panics with the command name
/// on transport errors so a refused connection is attributable.
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
async fn serve_core_drives_full_flow_over_http_and_ws() {
    // ── Phase 1: tempdir HOME + USERPROFILE + HARMONY_PASSPHRASE ────────
    // mint_owner_lifecycle.rs's home_override() pattern: HOME/USERPROFILE
    // scope the identity dir (~/.harmony) and the app data dir to the
    // tempdir; HARMONY_PASSPHRASE gives the encrypted-file identity
    // fallback a passphrase (the keychain is refused in test builds,
    // ZEB-428).
    let home = tempfile::tempdir().expect("tempdir for HOME override");
    let home_str = home
        .path()
        .to_str()
        .expect("tempdir path is valid utf8")
        .to_string();
    let _g1 = common::set_env("HOME", &home_str);
    let _g2 = common::set_env("USERPROFILE", &home_str);
    let _g3 = common::set_env("HARMONY_PASSPHRASE", "api-server-test-pp");
    // dirs::data_dir() prefers XDG_DATA_HOME (Linux) / APPDATA (Windows)
    // over HOME-derived paths — pin both into the tempdir so a CI/dev
    // environment that sets them can't leak the profile outside it.
    let _g4 = common::set_env("XDG_DATA_HOME", &format!("{home_str}/xdg-data"));
    let _g5 = common::set_env("APPDATA", &format!("{home_str}/appdata"));

    // Read-only safety check BEFORE any node boot writes anything: with
    // HOME tempdir'd, dirs::data_dir() follows the platform convention
    // (macOS: $HOME/Library/Application Support) — either way it must land
    // inside the tempdir, never the developer's real profile.
    let data_dir = harmony_app::resolve_app_data_dir().expect("resolve app data dir");
    assert!(
        data_dir.starts_with(home.path()),
        "HOME override must scope the app data dir to the tempdir; got {} (HOME {})",
        data_dir.display(),
        home.path().display()
    );

    // ── Phase 2: iroh global-init warm-up ───────────────────────────────
    // ZEB-347: the first iroh bind per process pays a one-time global init
    // (~10s CI / ~30s macOS); warm up before any asserted timeout.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    // ── Phase 3: boot the serve core in-process ─────────────────────────
    // Same construction as serve_cli: default NodeState, API event sink as
    // the only NodeEventSink, no Tauri handle.
    let state = Arc::new(Mutex::new(harmony_app::NodeState::default()));
    let events = harmony_app::api::events::ApiEventSink::new();
    let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = events.clone();
    // ZEB-719: pass the owned NodeState handle exactly as `serve_cli` does, so this
    // headless harness faithfully mirrors the real serve wiring (Tier-2 auto-exec
    // dispatches rather than stubs). Inert here — no Tier-2 poll is finalized.
    let boot =
        harmony_app::start_node_inner(None, sink.clone(), None, &state, Some(Arc::clone(&state)))
            .await
            .expect("headless node boots without Tauri");
    assert!(
        !boot.has_owner_identity,
        "fresh profile must boot pre-mint (no owner_state.cbor yet)"
    );

    // ── Phase 4: bind the API server on an ephemeral port ───────────────
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (handle, server_task) = harmony_app::api::start_server(
        &data_dir,
        0, // port 0 = OS-assigned ephemeral
        state.clone(),
        sink.clone(),
        events.clone(),
        shutdown_tx.clone(),
        shutdown_rx,
    )
    .await
    .expect("api server binds an ephemeral port");

    // ── Phase 5: discovery files (token + port) ─────────────────────────
    let token =
        std::fs::read_to_string(handle.api_dir.join("token")).expect("token discovery file exists");
    let port: u16 = std::fs::read_to_string(handle.api_dir.join("port"))
        .expect("port discovery file exists")
        .trim()
        .parse()
        .expect("port file parses as u16");
    assert_eq!(
        port, handle.bound_port,
        "port discovery file must match the actually-bound port"
    );
    let base = format!("http://127.0.0.1:{port}");
    let bearer = format!("Bearer {}", token.trim());
    let http = reqwest::Client::new();

    // ── Phase 6a: auth is enforced — status without a token is 401 ──────
    let r = http
        .get(format!("{base}/v1/status"))
        .send()
        .await
        .expect("GET /v1/status (unauthed) transport");
    assert_eq!(r.status(), 401, "status without auth must be 401");

    // ── Phase 6b: authed status — node is running ───────────────────────
    let r = http
        .get(format!("{base}/v1/status"))
        .header("authorization", &bearer)
        .send()
        .await
        .expect("GET /v1/status (authed) transport");
    assert_eq!(r.status(), 200, "authed status must be 200");
    let status: serde_json::Value = r.json().await.expect("status body is JSON");
    assert_eq!(
        status["running"],
        serde_json::json!(true),
        "status must report running == true after boot; got: {status}"
    );

    // ── Phase 6c: connect the WS firehose BEFORE acting ─────────────────
    // Auth rides the upgrade request's headers; connecting before the mint
    // and the community/channel writes means their frames are captured.
    let ws_req = {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://127.0.0.1:{port}/v1/events")
            .into_client_request()
            .expect("ws url builds a client request");
        req.headers_mut().insert(
            "authorization",
            bearer.parse().expect("bearer is a valid header value"),
        );
        req
    };
    let (mut ws, _resp) = tokio_tungstenite::connect_async(ws_req)
        .await
        .expect("ws /v1/events connects with auth header");

    // ── Phase 6d: unknown command → 404; bad args → 400 ─────────────────
    let r = rpc(
        &http,
        &base,
        &bearer,
        "no_such_command",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(r.status(), 404, "unknown RPC command must be 404");
    let r = rpc(
        &http,
        &base,
        &bearer,
        "list_channels",
        serde_json::json!({ "communityId": 42 }),
    )
    .await;
    assert_eq!(
        r.status(),
        400,
        "wrong-typed args must be 400 (communityId must be a string)"
    );

    // ── Phase 6e-pre: fresh profile has NO owner (ground truth) ─────────
    // get_owner_state on a fresh profile is 200 + JSON null — nothing in
    // the backend auto-mints (see module doc). Pin that behavior, then
    // mint over the API: `mint_owner_identity` is in the curated RPC
    // surface (ZEB-445 DoD — headless identity bootstrap). The handler
    // restarts the node via a real start_node_inner closure (same flow as
    // the GUI's IPC wrapper).
    let r = rpc(
        &http,
        &base,
        &bearer,
        "get_owner_state",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(r.status(), 200, "pre-mint get_owner_state must be 200");
    let pre_mint: serde_json::Value = r.json().await.expect("pre-mint owner body is JSON");
    assert!(
        pre_mint.is_null(),
        "fresh profile must report no owner identity; got: {pre_mint}"
    );

    let r = rpc(
        &http,
        &base,
        &bearer,
        "mint_owner_identity",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "POST /v1/rpc/mint_owner_identity must succeed on a fresh profile"
    );
    let minted: serde_json::Value = r.json().await.expect("mint body is JSON");
    // MintIpcResult is { state: OwnerStateView, recovery_token } — camelCase
    // on the wire, so the owner id lives at state.ownerId.
    let minted_owner_id = minted["state"]["ownerId"]
        .as_str()
        .unwrap_or_else(|| panic!("mint result must carry state.ownerId; got: {minted}"))
        .to_string();
    assert!(
        !minted_owner_id.is_empty(),
        "mint must return a non-empty owner id; got: {minted}"
    );
    assert!(
        minted["recoveryToken"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "mint result must carry a non-empty recoveryToken; got: {minted}"
    );

    // ── Phase 6e: get_owner_state over HTTP — owner present ─────────────
    // Bounded poll (30s / 500ms): the mint restart completed above, so the
    // first iteration normally succeeds; the poll absorbs slow-CI skew.
    let owner_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let owner: serde_json::Value = loop {
        let r = rpc(
            &http,
            &base,
            &bearer,
            "get_owner_state",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(r.status(), 200, "get_owner_state must be 200 after mint");
        let v: serde_json::Value = r.json().await.expect("owner body is JSON");
        if v["ownerId"].is_string() {
            break v;
        }
        assert!(
            tokio::time::Instant::now() < owner_deadline,
            "owner not visible over the API within 30s of mint; last JSON: {v}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert_eq!(
        owner["ownerId"].as_str(),
        Some(minted_owner_id.as_str()),
        "API-visible ownerId must match the mint result; got: {owner}"
    );

    // ── Phase 6f: create_community → community id (JSON string) ─────────
    let r = rpc(
        &http,
        &base,
        &bearer,
        "create_community",
        serde_json::json!({"name": "api-e2e", "isInviteOnly": false}),
    )
    .await;
    assert_eq!(r.status(), 200, "create_community must succeed");
    let community_id: String = r
        .json()
        .await
        .expect("create_community returns a JSON string");
    assert_eq!(
        community_id.len(),
        32,
        "community id is 16 bytes as 32 hex chars; got: {community_id}"
    );

    // ── Phase 6f-relay (ZEB-582): invite-only auto-enables relay opt-in ──
    // The mint restart above re-ran the owner-loaded boot block, which installs
    // the relay-optin fleet-sync engine — so `get_community_relay_status` is a
    // real bool here (not a 500 "not running"). The OPEN community just created
    // must NOT auto-enable relay opt-in (open-join delivers channel config
    // inline; no relay dependency).
    let r = rpc(
        &http,
        &base,
        &bearer,
        "get_community_relay_status",
        serde_json::json!({ "communityIdHex": community_id }),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "get_community_relay_status must be 200 for an existing community"
    );
    assert!(
        !r.json::<bool>().await.expect("relay status body is JSON"),
        "ZEB-582: an OPEN community must NOT auto-enable relay opt-in"
    );

    // A freshly-created INVITE-ONLY community MUST auto-enable relay opt-in at
    // creation, so a remote/CGNAT joiner gets a relay-backed state-root pull
    // path instead of silently never receiving channels. The set is applied
    // synchronously inside create_community before it returns, so the status
    // read needs no poll.
    let r = rpc(
        &http,
        &base,
        &bearer,
        "create_community",
        serde_json::json!({ "name": "api-e2e-invite", "isInviteOnly": true }),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "create_community (invite-only) must succeed"
    );
    let invite_community_id: String = r
        .json()
        .await
        .expect("create_community (invite-only) returns a JSON string");
    let r = rpc(
        &http,
        &base,
        &bearer,
        "get_community_relay_status",
        serde_json::json!({ "communityIdHex": invite_community_id }),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "get_community_relay_status must be 200 after an invite-only create"
    );
    assert!(
        r.json::<bool>().await.expect("relay status body is JSON"),
        "ZEB-582: an invite-only community must auto-enable relay opt-in at creation"
    );

    // ── Phase 6f-mod: the nine ZEB-512/520/527 verbs are REACHABLE ──────
    // Tasks 1–4 exposed these on the headless RPC surface; each previously
    // returned 404 `unknown command`. The non-negotiable proof for all nine
    // is reachability (status != 404) through the real axum server; where
    // node state permits, we make a stronger positive assertion. The owner
    // minted above is this community's admin (full power), so the read verbs
    // are authorized and a freshly-created community yields empty audit feeds.

    // ZEB-520: identity pub hex is Some (128 hex chars) after mint.
    let r = rpc(
        &http,
        &base,
        &bearer,
        "connectivity_get_my_identity_pub_hex",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "connectivity_get_my_identity_pub_hex must be 200 after mint"
    );
    let pub_hex: Option<String> = r.json().await.expect("identity pub hex body is JSON");
    assert_eq!(
        pub_hex.expect("identity pub present after mint").len(),
        128,
        "identity pub is 64 bytes as 128 hex chars"
    );

    // ZEB-512 / ZEB-881: discoverable now defaults TRUE for a fresh identity.
    let r = rpc(
        &http,
        &base,
        &bearer,
        "connectivity_get_identity_discoverable",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "connectivity_get_identity_discoverable must be 200"
    );
    assert!(
        r.json::<bool>().await.expect("discoverable body is JSON"),
        "ZEB-881: discoverable now defaults true for a fresh identity"
    );
    // ZEB-512: setter is reachable. It needs a pkarr identity publisher in
    // NodeState; this headless boot may not install one (no relay config),
    // so the floor is reachability (status != 404). When the harness DOES
    // carry a publisher and the set succeeds (200), the getter must then
    // report true.
    let r = rpc(
        &http,
        &base,
        &bearer,
        "connectivity_set_identity_discoverable",
        serde_json::json!({ "enabled": true }),
    )
    .await;
    let set_status = r.status();
    // Reaches the handler: 200 (set applied, publisher present) or 500 (publisher
    // unavailable in this headless boot). Asserting the specific codes — not merely
    // `!= 404` — catches 4xx regressions (auth/arg-shape) that would otherwise pass.
    assert!(
        matches!(set_status.as_u16(), 200 | 500),
        "connectivity_set_identity_discoverable must be 200 or 500; got {set_status}"
    );
    if set_status == 200 {
        let r = rpc(
            &http,
            &base,
            &bearer,
            "connectivity_get_identity_discoverable",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(r.status(), 200, "getter after a successful set must be 200");
        assert!(
            r.json::<bool>().await.expect("discoverable body is JSON"),
            "discoverable must reflect the set (true) after a 200 set"
        );
    }

    // ZEB-527 read verbs: a freshly-created community has no pending joins,
    // counter-signs, or moderation events → 200 with an empty list.
    let r = rpc(
        &http,
        &base,
        &bearer,
        "list_pending_joins",
        serde_json::json!({ "communityId": community_id }),
    )
    .await;
    assert_eq!(r.status(), 200, "list_pending_joins must be 200 for admin");
    assert_eq!(
        r.json::<serde_json::Value>()
            .await
            .expect("pending joins body is JSON"),
        serde_json::json!([]),
        "a freshly-created community has no pending joins"
    );
    let r = rpc(
        &http,
        &base,
        &bearer,
        "list_recent_counter_signs",
        serde_json::json!({ "communityId": community_id, "limit": 20 }),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "list_recent_counter_signs must be 200 for admin"
    );
    assert_eq!(
        r.json::<serde_json::Value>()
            .await
            .expect("counter signs body is JSON"),
        serde_json::json!([]),
        "a freshly-created community has no counter-signs"
    );
    let r = rpc(
        &http,
        &base,
        &bearer,
        "list_recent_moderation_events",
        serde_json::json!({ "communityId": community_id, "limit": 20 }),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "list_recent_moderation_events must be 200 for admin"
    );
    assert_eq!(
        r.json::<serde_json::Value>()
            .await
            .expect("moderation events body is JSON"),
        serde_json::json!([]),
        "a freshly-created community has no moderation events"
    );

    // ZEB-527 action verbs: reach the handler. A bogus-but-valid 32-hex
    // target/proposal parses, then the impl returns a Command error (the
    // target is not a member / the proposal does not exist) → HTTP 500,
    // never 404. The floor is reachability (status != 404).
    let r = rpc(
        &http,
        &base,
        &bearer,
        "kick_from_community",
        serde_json::json!({
            "communityId": community_id,
            "targetAddr": "ffffffffffffffffffffffffffffffff"
        }),
    )
    .await;
    assert_eq!(
        r.status(),
        500,
        "kick_from_community with a bogus-but-valid target must fail through the handler (500), not 404/4xx"
    );
    let r = rpc(
        &http,
        &base,
        &bearer,
        "unban_from_community",
        serde_json::json!({
            "communityId": community_id,
            "targetAddr": "ffffffffffffffffffffffffffffffff"
        }),
    )
    .await;
    assert_eq!(
        r.status(),
        500,
        "unban_from_community with a bogus-but-valid target must fail through the handler (500), not 404/4xx"
    );
    let r = rpc(
        &http,
        &base,
        &bearer,
        "countersign_admin_proposal",
        serde_json::json!({
            "communityId": community_id,
            "proposalEventId": "ffffffffffffffffffffffffffffffff"
        }),
    )
    .await;
    assert_eq!(
        r.status(),
        500,
        "countersign_admin_proposal with a bogus-but-valid proposal id must fail through the handler (500), not 404/4xx"
    );

    // ── Phase 6g: create_channel → channel id ───────────────────────────
    // Args mirror CreateChannelArgs exactly: communityId, name, writePower,
    // kind (optional, "text"). ZEB-583: use a non-"general" name — the
    // community already auto-seeds #general, and create_channel now rejects a
    // duplicate live name; this phase exercises the create→post RPC plumbing,
    // not dedup.
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
    let channel_id: String = r
        .json()
        .await
        .expect("create_channel returns a JSON string");
    assert_eq!(
        channel_id.len(),
        32,
        "channel id is 16 bytes as 32 hex chars; got: {channel_id}"
    );

    // ── Phase 6h: post_channel_message ──────────────────────────────────
    let body_bytes: Vec<u8> = b"hello headless".to_vec();
    let r = rpc(
        &http,
        &base,
        &bearer,
        "post_channel_message",
        serde_json::json!({
            "communityId": community_id,
            "channelId": channel_id,
            "body": body_bytes,
            "replyTo": null
        }),
    )
    .await;
    // ZEB-467: capture the backend error body so a future flake pins WHY
    // post_channel_message failed (the create→post channel-log engine race
    // historically surfaced here as a bare 500). The body is otherwise
    // unused — the next phase issues a fresh request.
    let post_status = r.status();
    let post_body = r.text().await.unwrap_or_default();
    assert_eq!(
        post_status, 200,
        "post_channel_message must succeed; got {post_status} with body: {post_body}"
    );

    // ── Phase 6i: list_channel_messages — exactly one message ───────────
    let r = rpc(
        &http,
        &base,
        &bearer,
        "list_channel_messages",
        serde_json::json!({
            "communityId": community_id,
            "channelId": channel_id,
            "since": null,
            "limit": 10
        }),
    )
    .await;
    assert_eq!(r.status(), 200, "list_channel_messages must succeed");
    let msgs: serde_json::Value = r.json().await.expect("message list is JSON");
    let arr = msgs
        .as_array()
        .unwrap_or_else(|| panic!("message list must be a JSON array; got: {msgs}"));
    assert_eq!(arr.len(), 1, "exactly the one posted message; got: {msgs}");
    assert_eq!(
        arr[0]["body"],
        serde_json::json!(b"hello headless".to_vec()),
        "posted body must read back byte-identical; got: {msgs}"
    );

    // ── Phase 6j: the WS saw real frames with monotonic numeric seq ─────
    // zenoh-status fires during the post-mint node restart; nav-updated
    // fires on create_community — both happened after the WS connected.
    let ws_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut frames: Vec<serde_json::Value> = Vec::new();
    let mut saw_real_event = false;
    while !saw_real_event {
        match tokio::time::timeout_at(ws_deadline, ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                let f: serde_json::Value = serde_json::from_str(&t)
                    .unwrap_or_else(|e| panic!("ws frame is not JSON ({e}): {t}"));
                if f["event"] == "zenoh-status" || f["event"] == "nav-updated" {
                    saw_real_event = true;
                }
                frames.push(f);
            }
            // Ping/pong/binary frames: not event frames, keep draining.
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => panic!("ws stream error after frames {frames:?}: {e}"),
            Ok(None) => panic!("ws closed before a real event frame; got: {frames:?}"),
            Err(_) => break, // 15s deadline
        }
    }
    assert!(
        saw_real_event,
        "expected ≥1 frame with event in {{zenoh-status, nav-updated}} within 15s; got: {frames:?}"
    );
    // Every non-lagged frame carries a numeric seq, strictly monotonic in
    // arrival order (a _lagged marker frame has seq null by contract and
    // would be a legitimate, if unexpected, occurrence — excluded).
    let seqs: Vec<u64> = frames
        .iter()
        .filter(|f| f["event"] != "_lagged")
        .map(|f| {
            f["seq"]
                .as_u64()
                .unwrap_or_else(|| panic!("frame without numeric seq: {f}"))
        })
        .collect();
    assert!(
        !seqs.is_empty(),
        "no seq-bearing frames captured: {frames:?}"
    );
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq must be strictly monotonic; got: {seqs:?}"
    );
    // Observability (nextest captures this; visible with --no-capture and
    // on CI failure re-runs): which events the firehose actually carried.
    eprintln!(
        "ws captured {} frames; events: {:?}; seqs: {seqs:?}",
        frames.len(),
        frames
            .iter()
            .map(|f| f["event"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>()
    );

    // Close the WS before shutdown so the upgraded connection doesn't hold
    // the server's graceful drain open.
    ws.close(None).await.ok();
    drop(ws);

    // ── Phase 6k: POST /v1/shutdown → 200 + shuttingDown ────────────────
    let r = http
        .post(format!("{base}/v1/shutdown"))
        .header("authorization", &bearer)
        .send()
        .await
        .expect("POST /v1/shutdown transport");
    assert_eq!(r.status(), 200, "shutdown must respond 200");
    let body: serde_json::Value = r.json().await.expect("shutdown body is JSON");
    assert_eq!(
        body["shuttingDown"],
        serde_json::json!(true),
        "shutdown must confirm; got: {body}"
    );

    // ── Phase 7: the server actually stopped ────────────────────────────
    // The graceful-shutdown send wakes axum::serve; the spawned server task
    // must join, after which a follow-up status request errs (connection
    // refused/reset) within a bounded retry window.
    tokio::time::timeout(Duration::from_secs(10), server_task)
        .await
        .expect("server task must join within 10s of /v1/shutdown")
        .expect("server task must not panic")
        .expect("graceful shutdown must end the server cleanly (Ok)");
    let stop_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut refused = false;
    while tokio::time::Instant::now() < stop_deadline {
        match http
            .get(format!("{base}/v1/status"))
            .header("authorization", &bearer)
            .send()
            .await
        {
            Err(_) => {
                refused = true;
                break;
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
    assert!(
        refused,
        "GET /v1/status must err (connection refused/reset) after shutdown"
    );

    // Node teardown is deliberately NOT asserted here: `stop_inner` is
    // pub(crate), and the serve_cli binary path — not this test — owns
    // stop_inner + discovery-file cleanup after the server winds down
    // (covered by the api/mod.rs unit tests + serve_cli). The test process
    // exits and takes the node's threads with it.
}
