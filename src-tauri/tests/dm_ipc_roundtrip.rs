//! ZEB-247: end-to-end JS↔Rust binding roundtrip tests for the 4 DM
//! IPCs (`send_dm`, `read_dm_thread`, `delete_outbox_entry`,
//! `add_space`).
//!
//! ## Why this test exists
//!
//! PR #81 round 4 caught a silent IPC param-naming bug
//! (`space_id_hex` Rust param vs `spaceId` JS key — neither matched
//! after Tauri 2's default `spaceId → space_id` conversion). The bug
//! shipped through 4 review rounds because:
//!
//! * `vitest` mocks the TauriAdapter — the JS-side mock returns
//!   whatever the test sets up, regardless of what params the IPC
//!   would actually have demanded.
//! * `dm_send_integration.rs` / `dm_thread_integration.rs` exercise
//!   the inner functions (`DmOutbox::send_dm`, `read_dm_thread_inner`)
//!   directly, bypassing `tauri::invoke` entirely.
//! * No existing test path runs the actual Tauri command macro's
//!   argument-binding code.
//!
//! This file fills that gap. Each test invokes one IPC via
//! `tauri::test::get_ipc_response` with a JSON body that uses the
//! same camelCase key shape the production frontend sends. Tauri's
//! IPC layer deserializes the JSON and binds the keys to the
//! `#[tauri::command]` fn's parameters. If the binding succeeds, the
//! command body runs against the empty `NodeState::default()` we
//! injected and returns a well-known `"node not running or no owner
//! identity"` (or similar) `Err` — that Err proves the params bound.
//! If a Rust param is renamed without updating the frontend (or vice
//! versa), Tauri's deserializer returns a different shape of Err
//! ("missing field …") and the test fails loudly.
//!
//! ## Out of scope
//!
//! * **Ok-path coverage.** Running each IPC to success requires a
//!   live `NodeState` (dm_outbox + crdt_state + runtime). Existing
//!   integration tests (`dm_send_integration.rs`, etc.) cover that.
//! * **Generalization to all ~50 IPCs.** This file establishes the
//!   pattern for the 4 DM IPCs per ZEB-247 ticket scope. Sweeping
//!   the rest is a separate follow-up if desired.
//! * **Frontend-side coverage.** vitest covers the JS shapes; this
//!   file covers the Tauri JSON deserializer boundary.

use std::sync::Mutex;

use harmony_app::{add_dm_ipc_handlers, NodeState};
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{get_ipc_response, mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::WebviewWindowBuilder;

/// Build a mock Tauri app with the 4 DM IPCs registered and an empty
/// `NodeState`. Production code uses `tauri::Builder::default()` +
/// the full `invoke_handler` list in `lib.rs::run()` — this fixture
/// mirrors the structure with only the 4 commands ZEB-247 covers.
fn build_test_app() -> tauri::App<tauri::test::MockRuntime> {
    add_dm_ipc_handlers(mock_builder())
        .manage(Mutex::new(NodeState::default()))
        .build(mock_context(noop_assets()))
        .expect("failed to build mock app")
}

/// Build a Tauri 2 `InvokeRequest` with the canonical boilerplate
/// (callback/error fn IDs, URL, invoke_key) and the given JSON body.
/// The JSON body is what `window.__TAURI__.invoke(cmd, args)` would
/// produce on the frontend side.
fn make_invoke_request(cmd: &str, body: serde_json::Value) -> InvokeRequest {
    InvokeRequest {
        cmd: cmd.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "http://tauri.localhost".parse().expect("url must parse"),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

/// Decode the Err body of `get_ipc_response` to a string. Tauri 2's
/// signature returns `Result<InvokeResponseBody, serde_json::Value>`
/// — the Err is a raw `serde_json::Value`, NOT an
/// `InvokeResponseBody`. Our IPCs all return `Result<T, String>`, so
/// the Err Value is always a JSON string; pull it out as a String for
/// substring matching.
fn err_value_as_string(v: &serde_json::Value) -> String {
    v.as_str()
        .map(String::from)
        .unwrap_or_else(|| v.to_string())
}

/// Assert that the response is an `Err` whose body contains one of
/// the "command body ran but bailed because NodeState is empty"
/// markers. This proves the Tauri IPC layer successfully bound the
/// JSON keys to the Rust function's parameters.
///
/// The marker substrings ("node not running", "no owner identity",
/// etc.) are the early-bail strings used by all 4 DM IPCs when
/// `NodeState::default()` lacks the runtime handles.
fn assert_err_contains_empty_state_marker(
    response: Result<tauri::ipc::InvokeResponseBody, serde_json::Value>,
    cmd: &str,
) {
    match response {
        Ok(body) => {
            panic!("{cmd}: expected command-body Err against empty NodeState, but got Ok: {body:?}")
        }
        Err(err_value) => {
            let err_str = err_value_as_string(&err_value);
            assert!(
                err_str.contains("node not running")
                    || err_str.contains("no owner identity")
                    || err_str.contains("crdt_state missing")
                    || err_str.contains("dm_outbox"),
                "{cmd}: expected command-body Err proving params bound (one of: \
                 'node not running', 'no owner identity', 'crdt_state missing', \
                 'dm_outbox'). got: {err_str}"
            );
        }
    }
}

#[test]
fn send_dm_binds_camelcase_params() {
    // Production frontend sends spaceId/content/mimeType. Tauri 2's
    // IPC layer converts camelCase → snake_case at the deserialize
    // boundary, binding to send_dm's space_id/content/mime_type
    // params. Against an empty NodeState, send_dm bails with
    // "node not running or no owner identity" — proving the JSON
    // keys reached the function body.
    let app = build_test_app();
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("webview build");

    let response = get_ipc_response(
        &webview,
        make_invoke_request(
            "send_dm",
            serde_json::json!({
                "spaceId": "00".repeat(16),
                "content": [104, 105],
                "mimeType": "text/plain",
            }),
        ),
    );
    assert_err_contains_empty_state_marker(response, "send_dm");
}

#[test]
fn read_dm_thread_binds_camelcase_params() {
    let app = build_test_app();
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("webview build");

    let response = get_ipc_response(
        &webview,
        make_invoke_request(
            "read_dm_thread",
            serde_json::json!({
                "spaceId": "00".repeat(16),
                "limit": 20,
                "beforeHlc": null,
            }),
        ),
    );
    assert_err_contains_empty_state_marker(response, "read_dm_thread");
}

#[test]
fn delete_outbox_entry_binds_camelcase_params() {
    let app = build_test_app();
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("webview build");

    let response = get_ipc_response(
        &webview,
        make_invoke_request(
            "delete_outbox_entry",
            serde_json::json!({
                "messageId": "00".repeat(16),
            }),
        ),
    );
    assert_err_contains_empty_state_marker(response, "delete_outbox_entry");
}

#[test]
fn add_space_binds_camelcase_params() {
    // add_space takes kind/name/members — all single-word camelCase
    // (matching snake_case identically). Members is Option<Vec<String>>.
    // Tauri's JSON deserializer handles the Option + Vec correctly.
    let app = build_test_app();
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("webview build");

    let response = get_ipc_response(
        &webview,
        make_invoke_request(
            "add_space",
            serde_json::json!({
                "kind": "dm",
                "name": "Test",
                "members": ["00".repeat(16)],
            }),
        ),
    );
    assert_err_contains_empty_state_marker(response, "add_space");
}

#[test]
fn send_dm_rejects_snake_case_keys_that_dont_match_param_names() {
    // The PR #81 bug was the inverse: Rust param `space_id_hex` and
    // JS key `spaceId` (which converts to `space_id`, not
    // `space_id_hex`). This test locks in the contract by using a
    // JS-side key (`space_id_hex`) that does NOT correspond to any
    // current Rust param after Tauri's conversion. The expected
    // failure mode is a deserialization Err (NOT the empty-NodeState
    // marker), proving Tauri's binding is STRICT — silent failures
    // like the PR #81 regression would surface here.
    let app = build_test_app();
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("webview build");

    let response = get_ipc_response(
        &webview,
        make_invoke_request(
            "send_dm",
            serde_json::json!({
                "space_id_hex": "00".repeat(16),  // the PR #81 misnomer
                "content": [104, 105],
                "mimeType": "text/plain",
            }),
        ),
    );

    let err_value = response
        .expect_err("wrong key name must NOT bind silently — should produce a deserialization Err");
    let err_str = err_value_as_string(&err_value);
    // The error must NOT contain the empty-NodeState marker (which
    // would imply silent binding to defaults), AND should mention
    // missing fields or similar deserialization-failure terminology.
    assert!(
        !err_str.contains("node not running") && !err_str.contains("no owner identity"),
        "send_dm should NOT silently bind unknown keys to defaults — got: {err_str}"
    );
    assert!(
        err_str.contains("invalid args")
            || err_str.contains("missing field")
            || err_str.contains("spaceId")
            || err_str.contains("space_id"),
        "expected deserialization Err mentioning missing/invalid field; got: {err_str}"
    );
}
