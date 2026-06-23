# Headless `api` surface completion — identity + discoverable + community moderation verbs

**Status:** approved-pending-review
**Date:** 2026-06-23
**Tickets:** identity-pub-hex getter, identity-discoverable set/get, community pending-join/moderation family (three sibling gaps under the cross-WAN epic)
**Branch:** `headless-api-surface-completion` (off `main` @ `d535f012`)

## Goal

Close three sibling gaps where a GUI-only `#[tauri::command]` was never added to the curated headless `api` v1 allowlist (`src-tauri/src/api/rpc.rs`), so an agent driving a windowless `harmony-app serve` node via `harmony-app api <verb> '<json>'` gets HTTP 404 `unknown command`. All three were surfaced during live cross-WAN bring-up runs as test-infra blockers: a headless node could not (a) emit its own identity pub key for `add_friend_by_key`, (b) make itself pkarr-discoverable, or (c) observe/moderate community pending-joins.

This is the **surface-verb half** only. The underlying convergence bug (3rd-member community join never converges cross-process) and the `network_health` pkarr stub are explicitly out of scope (separate tickets).

## Background — the wiring pattern

The headless RPC surface is a curated allowlist built by a macro in `api/rpc.rs::build_registry()`:

```rust
rpc!(m, "verb_name", ArgsType, |state, sink, a| async move {
    crate::verb_name_impl(state, a.field, …).await
});
```

The macro hands the closure `state: &Mutex<NodeState>` (via `NodeStateAccess::node_state()`) and a `sink: Arc<dyn NodeEventSink>`. Each verb therefore needs an `_impl` seam that takes `&Mutex<NodeState>` (plus a `sink` only if it emits events) and returns `Result<Dto, String>` — the *same* DTO and error strings the Tauri IPC returns, so GUI and headless share one mental model. The Tauri `#[tauri::command]` becomes a thin wrapper around the seam.

**Key finding:** every target command already takes `state_lock: tauri::State<'_, Mutex<NodeState>>` and nothing else Tauri-specific — *except* `connectivity_set_identity_discoverable`, whose only `AppHandle` use is an `app.emit` of a change event. So the extraction is behavior-preserving for the GUI, and **no seam needs the event sink** (the one `emit` stays in the Tauri wrapper).

## The nine verbs

| Verb | Args | Returns | Source `#[tauri::command]` |
|---|---|---|---|
| `connectivity_get_my_identity_pub_hex` | `{}` | `Option<String>` (128-hex) | `lib.rs` (state-only) |
| `connectivity_set_identity_discoverable` | `{ enabled: bool }` | `()` | `lib.rs` (app+state) |
| `connectivity_get_identity_discoverable` | `{}` | `bool` | `lib.rs` (state-only) |
| `list_pending_joins` | `{ communityId }` | `Vec<PendingJoinDto>` | `lib.rs` (state-only) |
| `list_recent_counter_signs` | `{ communityId, limit }` | `Vec<CounterSignDto>` | `lib.rs` (state-only) |
| `countersign_admin_proposal` | `{ communityId, proposalEventId }` | `CountersignResult` | `lib.rs` (state-only) |
| `kick_from_community` | `{ communityId, targetAddr, reason? }` | `AdminActionResult` | `lib.rs` (state-only) |
| `unban_from_community` | `{ communityId, targetAddr, reason? }` | `()` | `lib.rs` (state-only) |
| `list_recent_moderation_events` | `{ communityId, limit }` | `Vec<ModerationEventDto>` | `lib.rs` (state-only) |

### Scope note — moderation breadth (ZEB-527)

The ticket literally names `list_pending_joins`, `list_recent_counter_signs`, `kick_from_community`, and "any approve/counter-sign trigger." This design adds the **complete symmetric set**: also `unban_from_community` (kick without unban is a footgun in test harnesses) and `list_recent_moderation_events` (pure read-only audit observability — the natural companion to the actions). All six are already fully exposed to the GUI and operate over the same `NodeState`; there is **no new trust boundary** — the headless `api` is a localhost, bearer-token surface operated by the node owner, who *is* the community admin. Per-verb marginal cost is one seam extraction + one `rpc!` line + tests.

## Design

### Seam extraction (8 of 9 — pure refactor)

For each state-only command, rename the body into `pub(crate) async fn <name>_impl(state: &Mutex<NodeState>, …args) -> Result<…, String>` (body unchanged except `state_lock` → `state`), and reduce the `#[tauri::command]` to:

```rust
#[tauri::command]
async fn list_pending_joins(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<PendingJoinDto>, String> {
    list_pending_joins_impl(state_lock.inner(), community_id).await
}
```

(`tauri::State::inner()` yields `&Mutex<NodeState>`, matching the macro's `node_state()`.)

### `connectivity_set_identity_discoverable` (event-emitting wrapper)

Extract only the core work (persist toggle to `connectivity-settings.json` + `id_pub.enable()/disable()`):

```rust
pub(crate) async fn connectivity_set_identity_discoverable_impl(
    state: &Mutex<NodeState>,
    enabled: bool,
) -> Result<(), String> { /* persist + publisher toggle — no emit */ }

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

Headless callers do not receive the push event (no frontend); they read state back via `connectivity_get_identity_discoverable`. GUI behavior is byte-identical.

### Arg structs (`api/rpc.rs`)

Reuse `EmptyArgs` (both getters) and the existing `CommunityIdArgs` (`list_pending_joins`). Add (mirroring the camelCase/snake_case convention, no `deny_unknown_fields`):

- `SetIdentityDiscoverableArgs { enabled: bool }`
- `CommunityLimitArgs { community_id: String, limit: u32 }` — shared by `list_recent_counter_signs` + `list_recent_moderation_events`
- `ModerationTargetArgs { community_id: String, target_addr: String, reason: Option<String> }` — shared by `kick_from_community` + `unban_from_community`
- `CountersignArgs { community_id: String, proposal_event_id: String }`

(If an equivalent struct already exists in `rpc.rs`, reuse it rather than duplicating.)

## Testing

- **rpc.rs unit tests** — one per verb against `build_registry()` on a default (pre-owner) `NodeState`, asserting the expected `Ok`/`Err(Command(OWNER_NOT_LOADED_MSG))` shape (mirrors existing verb unit tests). Plus `BadArgs` coverage for the verbs with required args (e.g. missing `communityId`).
- **Surface-composition audit** — extend the curated-surface test list in `rpc.rs` (currently the 49-name set) to include the 9 new names; bump the asserted count to 58. This is the load-bearing guard that the allowlist matches intent.
- **api_server.rs e2e** — a round-trip asserting each new verb returns HTTP 200 (not 404) through the real axum server + bearer auth: identity-pub-hex after mint is `Some`, get-discoverable defaults `false` then reflects a set, and the community verbs return their expected error/empty shape pre-membership.
- **Gates** — `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`. (No frontend code changes expected; the GUI Tauri commands keep identical signatures, so `channel-message-service.ts` and the Svelte panels are untouched.)

## Out of scope

- The 3rd-member community-join cross-process convergence bug (separate ticket; this is only the missing observability/moderation *levers*).
- `network_health_snapshot.pkarrStatus.identityPublished` stub (separate ticket).
- The secondary `connectivity_get_my_reachability_record` `announcedAtMs:0` local-view observation noted on the identity-pub-hex ticket (defer; logging-only).
- No new wire/protocol change; no DTO changes; no new event types.
