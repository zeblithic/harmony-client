# Headless `api` surface completion — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose nine GUI-only `#[tauri::command]`s on the curated headless `api` v1 allowlist (`src-tauri/src/api/rpc.rs`) by extracting `&Mutex<NodeState>`-based `_impl` seams, so an agent driving a windowless `harmony-app serve` node can read its identity pub key, toggle pkarr discoverability, and observe/moderate community pending-joins.

**Architecture:** Each verb = (1) extract a `pub(crate) async fn <name>_impl(state: &std::sync::Mutex<NodeState>, …) -> Result<Dto, String>` seam from the existing command (body verbatim, `state_lock` → `state`), (2) reduce the `#[tauri::command]` to a thin wrapper calling the seam, (3) add a `rpc!(…)` registration + arg struct in `rpc.rs`, (4) add the verb name to the curated-surface audit test, (5) add unit tests. Eight of nine are pure refactors; `connectivity_set_identity_discoverable` keeps its `app.emit` change-event in the Tauri wrapper so the GUI is byte-identical. No frontend changes, no wire/DTO/protocol change.

**Tech Stack:** Rust (Tauri 2, axum, tokio, serde), cargo-nextest.

**Spec:** `docs/specs/2026-06-23-headless-api-surface-completion-design.md`

**Conventions (all tasks):**
- Cargo commands run from `src-tauri/`. Always `--locked --features test-fixtures`.
- The `_impl` seam param type is exactly `state: &std::sync::Mutex<NodeState>` (copy from existing seams like `list_owner_communities_impl` at `lib.rs:18090`).
- `tauri::State::inner()` yields `&std::sync::Mutex<NodeState>` for the wrapper call.
- The `rpc!` macro form for no-event verbs: `rpc!(m, "name", ArgsType, |state, _sink, a| async move { crate::name_impl(state, …).await });`
- Arg structs go in the `// ── Arg structs ──` region of `rpc.rs` (after `EmptyArgs`, ~line 86+); `#[derive(serde::Deserialize)] #[serde(rename_all = "camelCase")]`, NO `deny_unknown_fields`.
- The curated-surface audit test is `registry_has_exactly_the_curated_v1_surface` in `rpc.rs` (the `expected` vec, ~line 980). Add each new name in a sensibly-labelled section; it's exact-set equality, so the test stays green only if registration + expected-list entry land together.
- Per-task verification (fast inner loop): `cargo nextest run --locked --features test-fixtures -E 'test(rpc)' -p harmony-app` for the rpc unit tests, plus `cargo clippy --locked -p harmony-app --lib --features test-fixtures -- -D warnings` and `cargo fmt --all`. The full `--all-targets` sweep is Task 5.
- **Commit before running long gates**; 10-min wall-clock kill switch on any single cargo command; report `DONE_WITH_CONCERNS` rather than stalling silently.

---

### Task 1: ZEB-520 — `connectivity_get_my_identity_pub_hex` headless verb

**Files:**
- Modify: `src-tauri/src/lib.rs` (command at `connectivity_get_my_identity_pub_hex`, ~line 47511)
- Modify: `src-tauri/src/api/rpc.rs` (registration + audit list + unit test)

- [ ] **Step 1: Write the failing unit test** in `rpc.rs` test module (near the other dispatch tests):

```rust
#[tokio::test]
async fn connectivity_get_my_identity_pub_hex_returns_null_pre_owner() {
    let reg = build_registry();
    let out = reg
        .dispatch(
            "connectivity_get_my_identity_pub_hex",
            test_state(),
            test_sink(),
            serde_json::Value::Null,
        )
        .await
        .expect("verb registered + dispatches");
    assert_eq!(out, serde_json::Value::Null); // Option<String>::None → JSON null
}
```

- [ ] **Step 2: Run it — verify it fails** with `RpcError::UnknownCommand` (the verb isn't registered yet):

Run: `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(connectivity_get_my_identity_pub_hex_returns_null_pre_owner)'`
Expected: FAIL (panics on `.expect`, unknown command).

- [ ] **Step 3: Extract the seam** in `lib.rs`. Read the existing command at ~line 47511. Replace it with a seam + thin wrapper:

```rust
/// Seam for `connectivity_get_my_identity_pub_hex` — shared by the Tauri IPC
/// and the headless `api` surface (ZEB-520).
pub(crate) async fn connectivity_get_my_identity_pub_hex_impl(
    state: &std::sync::Mutex<NodeState>,
) -> Result<Option<String>, String> {
    let g = state
        .lock()
        .map_err(|e| format!("NodeState poisoned: {e}"))?;
    Ok(g.dm_identity_pub_64.map(hex::encode))
}

#[tauri::command]
async fn connectivity_get_my_identity_pub_hex(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Option<String>, String> {
    connectivity_get_my_identity_pub_hex_impl(state.inner()).await
}
```

(Keep the existing doc-comment on the command. `Mutex` in the wrapper resolves to the file's existing `Mutex` alias — leave it as the surrounding commands write it.)

- [ ] **Step 4: Register the verb** in `rpc.rs` `build_registry()`, in the connectivity group (near `connectivity_get_my_reachability_record`):

```rust
rpc!(m, "connectivity_get_my_identity_pub_hex", EmptyArgs, |state, _sink, _a| {
    async move { crate::connectivity_get_my_identity_pub_hex_impl(state).await }
});
```

- [ ] **Step 5: Add to the curated-surface audit list** in `registry_has_exactly_the_curated_v1_surface`, in the `// connectivity` section:

```rust
            "connectivity_get_my_identity_pub_hex",
```

- [ ] **Step 6: Run tests — verify pass**

Run: `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(rpc)'`
Expected: PASS (new test + `registry_has_exactly_the_curated_v1_surface` both green).

- [ ] **Step 7: clippy + fmt, then commit**

```bash
cargo clippy --locked -p harmony-app --lib --features test-fixtures -- -D warnings
cargo fmt --all
git add -A && git commit -m "feat(api): expose connectivity_get_my_identity_pub_hex on headless surface"
```

---

### Task 2: ZEB-512 — `connectivity_set_identity_discoverable` + `connectivity_get_identity_discoverable`

**Files:**
- Modify: `src-tauri/src/lib.rs` (commands at ~line 43962 setter, ~line 44016 getter)
- Modify: `src-tauri/src/api/rpc.rs`

- [ ] **Step 1: Write failing unit tests** in `rpc.rs`:

```rust
#[tokio::test]
async fn connectivity_get_identity_discoverable_defaults_false() {
    let reg = build_registry();
    let out = reg
        .dispatch(
            "connectivity_get_identity_discoverable",
            test_state(),
            test_sink(),
            serde_json::Value::Null,
        )
        .await
        .expect("verb registered");
    assert_eq!(out, serde_json::Value::Bool(false)); // no settings path → default off
}

#[tokio::test]
async fn connectivity_set_identity_discoverable_errs_pre_owner() {
    let reg = build_registry();
    let err = reg
        .dispatch(
            "connectivity_set_identity_discoverable",
            test_state(),
            test_sink(),
            serde_json::json!({ "enabled": true }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::Command(_)), "expected Command, got {err:?}");
}

#[tokio::test]
async fn connectivity_set_identity_discoverable_rejects_missing_enabled() {
    let reg = build_registry();
    let err = reg
        .dispatch(
            "connectivity_set_identity_discoverable",
            test_state(),
            test_sink(),
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::BadArgs(_)), "expected BadArgs, got {err:?}");
}
```

- [ ] **Step 2: Run — verify all three fail** (unknown command).

Run: `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(connectivity_set_identity_discoverable) + test(connectivity_get_identity_discoverable)'`
Expected: FAIL.

- [ ] **Step 3: Extract the getter seam** in `lib.rs` (~line 44016). Replace command with:

```rust
pub(crate) async fn connectivity_get_identity_discoverable_impl(
    state: &std::sync::Mutex<NodeState>,
) -> Result<bool, String> {
    let path = {
        state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?
            .pkarr_settings_path
            .clone()
    };
    let Some(path) = path else {
        return Ok(false);
    };
    Ok(pkarr_settings::PkarrSettings::load_or_default(&path).identity_discoverable)
}

#[tauri::command]
async fn connectivity_get_identity_discoverable(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    connectivity_get_identity_discoverable_impl(state.inner()).await
}
```

- [ ] **Step 4: Extract the setter seam** in `lib.rs` (~line 43962). Move the core work (persist toggle + publisher enable/disable) into the seam; keep `app.emit` in the wrapper:

```rust
pub(crate) async fn connectivity_set_identity_discoverable_impl(
    state: &std::sync::Mutex<NodeState>,
    enabled: bool,
) -> Result<(), String> {
    let (id_pub, settings_path) = {
        let guard = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            guard.pkarr_identity_publisher.clone(),
            guard.pkarr_settings_path.clone(),
        )
    };
    let Some(id_pub) = id_pub else {
        return Err(OWNER_NOT_LOADED_MSG.into());
    };
    let Some(path) = settings_path else {
        return Err("pkarr_settings_path missing".into());
    };
    let mut settings = pkarr_settings::PkarrSettings::load_or_default(&path);
    settings.identity_discoverable = enabled;
    settings
        .save(&path)
        .map_err(|e| format!("save connectivity-settings: {e}"))?;
    if enabled {
        id_pub.enable().await;
    } else {
        id_pub.disable().await;
    }
    Ok(())
}

#[tauri::command]
async fn connectivity_set_identity_discoverable(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
    enabled: bool,
) -> Result<(), String> {
    connectivity_set_identity_discoverable_impl(state.inner(), enabled).await?;
    if let Err(e) = app.emit(
        "connectivity-identity-discoverable-changed",
        serde_json::json!({ "enabled": enabled }),
    ) {
        tracing::warn!(error = %e, "connectivity_set_identity_discoverable: emit failed");
    }
    Ok(())
}
```

(Preserve the existing doc-comments. If `OWNER_NOT_LOADED_MSG` / `Manager`/`Emitter` import for `app.emit` were already in scope for the original command, they remain in scope.)

- [ ] **Step 5: Add arg struct** in `rpc.rs` arg-structs region:

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetIdentityDiscoverableArgs {
    enabled: bool,
}
```

- [ ] **Step 6: Register both verbs** in `build_registry()` (connectivity group):

```rust
rpc!(m, "connectivity_set_identity_discoverable", SetIdentityDiscoverableArgs, |state, _sink, a| {
    async move { crate::connectivity_set_identity_discoverable_impl(state, a.enabled).await }
});
rpc!(m, "connectivity_get_identity_discoverable", EmptyArgs, |state, _sink, _a| {
    async move { crate::connectivity_get_identity_discoverable_impl(state).await }
});
```

- [ ] **Step 7: Add both names** to the audit list (`// connectivity` section):

```rust
            "connectivity_set_identity_discoverable",
            "connectivity_get_identity_discoverable",
```

- [ ] **Step 8: Run tests, clippy, fmt, commit**

```bash
cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(rpc)'
cargo clippy --locked -p harmony-app --lib --features test-fixtures -- -D warnings
cargo fmt --all
git add -A && git commit -m "feat(api): expose identity_discoverable set/get on headless surface"
```
Expected: all `rpc` tests PASS.

---

### Task 3: ZEB-527 (read verbs) — `list_pending_joins`, `list_recent_counter_signs`, `list_recent_moderation_events`

**Files:**
- Modify: `src-tauri/src/lib.rs` (commands at ~line 32500, ~line 32657, ~line 32362)
- Modify: `src-tauri/src/api/rpc.rs`

- [ ] **Step 1: Write failing unit tests** in `rpc.rs` (use a valid 32-hex dummy community id; pre-owner state returns a Command error; missing arg → BadArgs):

```rust
const DUMMY_COMMUNITY_HEX: &str = "00000000000000000000000000000000"; // 16 bytes

#[tokio::test]
async fn list_pending_joins_errs_pre_owner() {
    let reg = build_registry();
    let err = reg
        .dispatch(
            "list_pending_joins",
            test_state(),
            test_sink(),
            serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
}

#[tokio::test]
async fn list_pending_joins_rejects_missing_community_id() {
    let reg = build_registry();
    let err = reg
        .dispatch("list_pending_joins", test_state(), test_sink(), serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::BadArgs(_)), "got {err:?}");
}

#[tokio::test]
async fn list_recent_counter_signs_errs_pre_owner() {
    let reg = build_registry();
    let err = reg
        .dispatch(
            "list_recent_counter_signs",
            test_state(),
            test_sink(),
            serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX, "limit": 20 }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
}

#[tokio::test]
async fn list_recent_moderation_events_errs_pre_owner() {
    let reg = build_registry();
    let err = reg
        .dispatch(
            "list_recent_moderation_events",
            test_state(),
            test_sink(),
            serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX, "limit": 20 }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
}
```

- [ ] **Step 2: Run — verify they fail** (unknown command).

Run: `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(list_pending_joins) + test(list_recent_counter_signs) + test(list_recent_moderation_events)'`
Expected: FAIL.

- [ ] **Step 3: Extract the three seams** in `lib.rs`. For each command (`list_pending_joins` ~32500, `list_recent_counter_signs` ~32657, `list_recent_moderation_events` ~32362): rename the existing `async fn NAME(state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>, …) -> Result<Dto, String>` to `pub(crate) async fn NAME_impl(state: &std::sync::Mutex<NodeState>, …) -> Result<Dto, String>`, replacing every `state_lock` reference in the body with `state` (body otherwise verbatim). Then add a thin wrapper preserving the original signature:

```rust
#[tauri::command]
async fn list_pending_joins(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<PendingJoinDto>, String> {
    list_pending_joins_impl(state_lock.inner(), community_id).await
}
```
```rust
#[tauri::command]
async fn list_recent_counter_signs(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    limit: u32,
) -> Result<Vec<CounterSignDto>, String> {
    list_recent_counter_signs_impl(state_lock.inner(), community_id, limit).await
}
```
```rust
#[tauri::command]
async fn list_recent_moderation_events(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    limit: u32,
) -> Result<Vec<ModerationEventDto>, String> {
    list_recent_moderation_events_impl(state_lock.inner(), community_id, limit).await
}
```

- [ ] **Step 4: Add the shared arg struct** in `rpc.rs` (reuse existing `CommunityIdArgs` for `list_pending_joins`; add for the `limit` pair):

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommunityLimitArgs {
    community_id: String,
    limit: u32,
}
```

- [ ] **Step 5: Register the three verbs** in `build_registry()` (add a `// community moderation (ZEB-527)` group near the community verbs):

```rust
rpc!(m, "list_pending_joins", CommunityIdArgs, |state, _sink, a| {
    async move { crate::list_pending_joins_impl(state, a.community_id).await }
});
rpc!(m, "list_recent_counter_signs", CommunityLimitArgs, |state, _sink, a| {
    async move { crate::list_recent_counter_signs_impl(state, a.community_id, a.limit).await }
});
rpc!(m, "list_recent_moderation_events", CommunityLimitArgs, |state, _sink, a| {
    async move { crate::list_recent_moderation_events_impl(state, a.community_id, a.limit).await }
});
```

- [ ] **Step 6: Add the three names** to the audit list (new `// community moderation (ZEB-527)` section):

```rust
            "list_pending_joins",
            "list_recent_counter_signs",
            "list_recent_moderation_events",
```

- [ ] **Step 7: Run tests, clippy, fmt, commit**

```bash
cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(rpc)'
cargo clippy --locked -p harmony-app --lib --features test-fixtures -- -D warnings
cargo fmt --all
git add -A && git commit -m "feat(api): expose community pending-join/moderation read verbs on headless surface"
```

---

### Task 4: ZEB-527 (action verbs) — `countersign_admin_proposal`, `kick_from_community`, `unban_from_community`

**Files:**
- Modify: `src-tauri/src/lib.rs` (commands at ~line 33221, ~line 31721, ~line 32249)
- Modify: `src-tauri/src/api/rpc.rs`

- [ ] **Step 1: Write failing unit tests** in `rpc.rs`:

```rust
#[tokio::test]
async fn countersign_admin_proposal_errs_pre_owner() {
    let reg = build_registry();
    let err = reg
        .dispatch(
            "countersign_admin_proposal",
            test_state(),
            test_sink(),
            serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX, "proposalEventId": DUMMY_COMMUNITY_HEX }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
}

#[tokio::test]
async fn kick_from_community_errs_pre_owner() {
    let reg = build_registry();
    let err = reg
        .dispatch(
            "kick_from_community",
            test_state(),
            test_sink(),
            serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX, "targetAddr": DUMMY_COMMUNITY_HEX }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
}

#[tokio::test]
async fn kick_from_community_rejects_missing_target() {
    let reg = build_registry();
    let err = reg
        .dispatch(
            "kick_from_community",
            test_state(),
            test_sink(),
            serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::BadArgs(_)), "got {err:?}");
}

#[tokio::test]
async fn unban_from_community_errs_pre_owner() {
    let reg = build_registry();
    let err = reg
        .dispatch(
            "unban_from_community",
            test_state(),
            test_sink(),
            serde_json::json!({ "communityId": DUMMY_COMMUNITY_HEX, "targetAddr": DUMMY_COMMUNITY_HEX }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::Command(_)), "got {err:?}");
}
```

- [ ] **Step 2: Run — verify they fail** (unknown command).

Run: `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(countersign_admin_proposal) + test(kick_from_community) + test(unban_from_community)'`
Expected: FAIL.

- [ ] **Step 3: Extract the three seams** in `lib.rs` (same body-verbatim, `state_lock` → `state` rename as Task 3). Wrappers:

```rust
#[tauri::command]
async fn kick_from_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    reason: Option<String>,
) -> Result<AdminActionResult, String> {
    kick_from_community_impl(state_lock.inner(), community_id, target_addr, reason).await
}
```
```rust
#[tauri::command]
async fn unban_from_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    reason: Option<String>,
) -> Result<(), String> {
    unban_from_community_impl(state_lock.inner(), community_id, target_addr, reason).await
}
```
```rust
#[tauri::command]
async fn countersign_admin_proposal(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    proposal_event_id: String,
) -> Result<CountersignResult, String> {
    countersign_admin_proposal_impl(state_lock.inner(), community_id, proposal_event_id).await
}
```

- [ ] **Step 4: Add arg structs** in `rpc.rs`:

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModerationTargetArgs {
    community_id: String,
    target_addr: String,
    reason: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountersignArgs {
    community_id: String,
    proposal_event_id: String,
}
```

- [ ] **Step 5: Register the three verbs** in `build_registry()` (in the `// community moderation (ZEB-527)` group):

```rust
rpc!(m, "countersign_admin_proposal", CountersignArgs, |state, _sink, a| {
    async move { crate::countersign_admin_proposal_impl(state, a.community_id, a.proposal_event_id).await }
});
rpc!(m, "kick_from_community", ModerationTargetArgs, |state, _sink, a| {
    async move { crate::kick_from_community_impl(state, a.community_id, a.target_addr, a.reason).await }
});
rpc!(m, "unban_from_community", ModerationTargetArgs, |state, _sink, a| {
    async move { crate::unban_from_community_impl(state, a.community_id, a.target_addr, a.reason).await }
});
```

- [ ] **Step 6: Add the three names** to the audit list (`// community moderation (ZEB-527)` section):

```rust
            "countersign_admin_proposal",
            "kick_from_community",
            "unban_from_community",
```

- [ ] **Step 7: Run tests, clippy, fmt, commit**

```bash
cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(rpc)'
cargo clippy --locked -p harmony-app --lib --features test-fixtures -- -D warnings
cargo fmt --all
git add -A && git commit -m "feat(api): expose community moderation action verbs on headless surface"
```

---

### Task 5: e2e round-trips + full gate sweep

**Files:**
- Modify: `src-tauri/tests/api_server.rs` (extend the existing booted-server e2e)

- [ ] **Step 1: Add e2e assertions** in `api_server.rs`. Read the existing test that boots the server, mints an owner, and creates a community (the `create_community` → community-id flow). Extend it (or add a sibling test reusing the same boot helper) to assert the nine new verbs are **reachable** (HTTP 200, not 404) and behave correctly where state allows. Use the existing `rpc(&http, &base, &bearer, "verb", json)` helper pattern. Concretely, after mint + community-create where the caller is the community admin:

```rust
// ZEB-520: identity pub hex is Some after mint (128 hex chars).
let r = rpc(&http, &base, &bearer, "connectivity_get_my_identity_pub_hex", serde_json::json!({})).await;
assert_eq!(r.status(), 200);
let pub_hex: Option<String> = r.json().await.expect("json");
let pub_hex = pub_hex.expect("identity pub present after mint");
assert_eq!(pub_hex.len(), 128);

// ZEB-512: discoverable defaults false, then reflects a set.
let r = rpc(&http, &base, &bearer, "connectivity_get_identity_discoverable", serde_json::json!({})).await;
assert_eq!(r.status(), 200);
assert_eq!(r.json::<bool>().await.expect("json"), false);
let r = rpc(&http, &base, &bearer, "connectivity_set_identity_discoverable", serde_json::json!({ "enabled": true })).await;
assert_eq!(r.status(), 200);
let r = rpc(&http, &base, &bearer, "connectivity_get_identity_discoverable", serde_json::json!({})).await;
assert_eq!(r.json::<bool>().await.expect("json"), true);

// ZEB-527: a freshly-created community has no pending joins / counter-signs / mod events.
let r = rpc(&http, &base, &bearer, "list_pending_joins", serde_json::json!({ "communityId": community_id })).await;
assert_eq!(r.status(), 200);
assert_eq!(r.json::<serde_json::Value>().await.expect("json"), serde_json::json!([]));
let r = rpc(&http, &base, &bearer, "list_recent_counter_signs", serde_json::json!({ "communityId": community_id, "limit": 20 })).await;
assert_eq!(r.status(), 200);
let r = rpc(&http, &base, &bearer, "list_recent_moderation_events", serde_json::json!({ "communityId": community_id, "limit": 20 })).await;
assert_eq!(r.status(), 200);
// Action verbs reach the handler (not 404). A bogus target yields a Command error (HTTP 500), never 404 unknown-command.
let r = rpc(&http, &base, &bearer, "kick_from_community", serde_json::json!({ "communityId": community_id, "targetAddr": "ffffffffffffffffffffffffffffffff" })).await;
assert_ne!(r.status(), 404);
```

Notes for the implementer:
- If `connectivity_set_identity_discoverable` returns non-200 because the started node has no `pkarr_identity_publisher` in this harness, downgrade that pair of assertions to `assert_ne!(status, 404)` (reachability) rather than asserting the toggle round-trips — the unit tests already pin the seam behavior. Verify which is true against the actual harness boot before finalizing; prefer the strongest assertion the harness supports.
- Match the existing test's variable names for the http client / base url / bearer / community_id; adapt as needed.

- [ ] **Step 2: Run the e2e test**

Run: `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(api_server)'`
Expected: PASS.

- [ ] **Step 3: Full gate sweep** (commit first). Background-run with a wall-clock safety net:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean, clippy 0, nextest all pass (3708+ tests; the 9 new unit tests + extended e2e).

- [ ] **Step 4: Frontend gates** (from repo root — expected untouched, but verify no drift):

```bash
npx tsc --noEmit
npx vitest run
```
Expected: tsc 0, vitest all pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "test(api): e2e round-trips for the nine new headless verbs"
```

---

## Self-Review (run after drafting)

- **Spec coverage:** ZEB-520 (1 verb) = Task 1; ZEB-512 (2 verbs) = Task 2; ZEB-527 (6 verbs) = Tasks 3–4. All nine verbs + audit-list + tests covered. ✓
- **Type consistency:** seam names are `<command>_impl`; arg structs `SetIdentityDiscoverableArgs` / `CommunityLimitArgs` / `ModerationTargetArgs` / `CountersignArgs`; reuse `EmptyArgs` + `CommunityIdArgs`. DTOs (`PendingJoinDto`, `CounterSignDto`, `ModerationEventDto`, `AdminActionResult`, `CountersignResult`) are returned unchanged from existing commands — already `Serialize`. ✓
- **Audit test stays green per-task:** each task adds registration + expected-list entry together. ✓
- **No GUI behavior change:** every Tauri command keeps its signature; only its body becomes a wrapper. The one event-emitting command keeps `app.emit`. ✓
