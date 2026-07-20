# ZEB-719: Headless serve path — wire Tier-2 auto-exec dispatch

**Status:** design approved 2026-07-20 (Jake). Follow-up to ZEB-300 (Tier-2 auto-exec, PR #505), parent ZEB-291.

## Problem

`start_node_inner` builds the voting-tick `auto_exec_set_power` closure. That closure is
`'static` (spawned by `spawn_voting_tick`), so it cannot capture the borrowed
`state: &Mutex<NodeState>` the function receives. ZEB-300 worked around this in the **GUI**
path by capturing the `'static` Tauri `AppHandle` and fetching Tauri's managed
`Mutex<NodeState>` via `app.state::<Mutex<NodeState>>()` at dispatch time.

In the **headless `serve`/test** path (`wry_handle == None`, ZEB-445), there is no
`AppHandle`, so the closure falls to the `SkippedNotAdmin` stub — the finalized Tier-2
SetPower is never dispatched. Effect: the agent-testing e2e stack (Ildwyn/AVALON headless
serve) cannot exercise Tier-2 auto-exec end-to-end; it is validated only in the GUI.

## Key facts (established by code investigation)

- `apply_auto_exec_set_power(node_state: &Mutex<NodeState>, cid, target, new_power)` already
  takes a **borrow** and only locks it (never Arc-clones/stores). So all the closure needs is
  a `'static` **owned** handle to the *same* Mutex the node runs on.
- The primary serve boot (`lib.rs:25002`) already owns
  `let state = Arc::new(Mutex::new(NodeState::default()))` and passes `&state`
  (deref-coerced) into `start_node_inner`.
- The GUI's `NodeState` lives inside **Tauri's** managed-state container; `app.state::<T>()`
  yields a `&T`, never an `Arc` we own. Making `start_node_inner` take
  `Arc<Mutex<NodeState>>` would force Tauri to manage `Arc<Mutex<NodeState>>` and rewrite
  every IPC command's `state.inner()` — a large blast radius for zero GUI benefit. **Rejected.**
- The headless RPC surface (`api/rpc.rs`) holds `ApiCtx.state: Arc<dyn NodeStateAccess>`,
  which for `serve` is the same `Arc<Mutex<NodeState>>` coerced to the trait object
  (`impl NodeStateAccess for Mutex<NodeState>`). The RPC `start_node` handler can therefore
  recover the owned Arc from `__access` if the trait exposes it.
- `community_admin_quorum_integration.rs` already drives a real engine finalize →
  materialized power change (the end-to-end auto-exec effect is covered). The *narrow* gap
  ZEB-719 closes is the headless **closure selection** (stub vs. real dispatch).

## Design (additive, single repo, single PR)

### 1. `owned_state` seam on `start_node_inner`

Add a 5th parameter `owned_state: Option<Arc<Mutex<NodeState>>>`. GUI callers pass `None`
(unchanged behavior); headless callers pass `Some(Arc::clone(&state))` pointing at the same
`NodeState` the node runs on.

### 2. Extract `build_auto_exec_fn` (testability + clarity)

Move the inline closure construction into a free function:

```rust
fn build_auto_exec_fn(
    wry_handle: Option<tauri::AppHandle<tauri::Wry>>,
    owned_state: Option<Arc<Mutex<NodeState>>>,
) -> crate::community_voting_tick::AutoExecSetPowerFn
```

Dispatch precedence inside the returned closure:
- `wry_handle` `Some` → GUI: `app.state::<Mutex<NodeState>>()` → `apply_auto_exec_set_power`
  (byte-identical to today).
- else `owned_state` `Some` → headless: capture the Arc, `apply_auto_exec_set_power(&arc, …)`.
- else → `Ok(SkippedNotAdmin)` (defensive; no handle available).

Extraction also makes the closure-selection unit-testable without a full node bringup.

### 3. `NodeStateAccess::node_state_arc` for the RPC-restart path

The headless `stop_node`→`start_node` RPC path must also wire auto-exec (not just first boot).
Add to the trait (`api/mod.rs`):

```rust
fn node_state_arc(self: Arc<Self>) -> Option<Arc<Mutex<NodeState>>> { None } // default
```

- `impl NodeStateAccess for Mutex<NodeState>` overrides → `Some(self)` (same allocation the
  serve node runs on; `Arc<Self>` receiver is object-safe).
- GUI host (`gui_host.rs`) keeps the default `None` (GUI uses `wry_handle`).

Both headless-reboot RPC handlers — `start_node` **and** `mint_owner_identity` — become
**bespoke** (not the generic `rpc!` macro) so they can read both
`let state = __access.node_state();` and `let owned = __access.clone().node_state_arc();`.

### 3b. Mint-restart path (as-built — discovered during implementation)

`mint_owner_identity` restarts the node (loads the freshly-minted owner) via
`mint_owner_identity_impl` → `start_node_inner`. **Every agent-testing node mints on first run**
(`NodeHandle::spawn` then `rpc("mint_owner_identity")`), so if that restart passed
`owned_state = None`, the post-mint tick would re-stub auto-exec — defeating the fix for the
exact flow it targets. So `mint_owner_identity_impl` also gains an
`owned_state: Option<Arc<Mutex<NodeState>>>` param, threaded into its inner `start_node_inner`
call; the headless mint RPC handler passes `__access.clone().node_state_arc()`, the GUI mint
command passes `None` (uses `wry_handle`).

### 4. Caller matrix (as-built)

| Site | Path | `owned_state` |
|---|---|---|
| `lib.rs` GUI `start_node` command wrapper | GUI first boot | `None` (wry `Some`) |
| `owner_commands.rs` `mint_owner_identity_impl` → `start_node_inner` | mint restart | threaded param (below) |
| `lib.rs` primary serve boot | **headless serve** | `Some(Arc::clone(&state))` |
| `lib.rs` in-lib `ok_restart` test | test | `None` |
| `api/rpc.rs` `start_node` (bespoke) | **headless RPC reboot** | `__access.clone().node_state_arc()` |
| `api/rpc.rs` `mint_owner_identity` (bespoke) | **headless mint** | `__access.clone().node_state_arc()` |
| `owner_commands.rs` GUI `mint_owner_identity` command | GUI mint | `None` (wry `Some`) |
| `tests/profile/profile_isolation.rs`, `tests/api/api_server.rs` | headless test harnesses | `Some(Arc::clone(&state))` (mirror serve; inert — no Tier-2 finalized) |

## Testing

The ZEB-719 change is the **closure selection** (headless now dispatches instead of stubbing).
The surrounding links are already covered and compose:

- finalize → auto-exec dispatch: `community_voting_tick.rs` tick unit tests (injected closure).
- dispatch → SetPower mint → materialized power: `apply_auto_exec_set_power` signing-path unit
  test + `community_admin_quorum_integration` `materialize` tests.

So the new coverage targets the selection itself, deterministically, in CI:

- **Unit (CI) — the regression guard.** `build_auto_exec_fn(None, None)` → closure yields
  `Ok(SkippedNotAdmin)`. `build_auto_exec_fn(None, Some(Arc::new(Mutex::new(NodeState::default()))))`
  → closure yields `Err(…missing / not running…)`, proving it **dispatched** to
  `apply_auto_exec_set_power` (which fails closed on a bare NodeState) rather than returning the
  stub. This `Ok(SkippedNotAdmin)` vs `Err` split is the exact ZEB-719 discriminator, and it
  composes with the existing dispatch→power coverage above for the full path.
- **Unit (CI).** `Arc::new(Mutex::new(NodeState::default())).node_state_arc()` returns
  `Some` (same pointer, via `Arc::ptr_eq`); the trait default (GUI host) returns `None`.

### Scope note — a live in-process power-change e2e is NOT cheaply reachable

Reaching a **fully-wired admin `NodeState`** (`hlc_tracker` + `dm_outbox` +
`CommunitySyncRegistry` + materialized state) requires a full `start_node` bringup — the
existing `apply_auto_exec_set_power` unit tests document this explicitly and deliberately punt
on it. The `community_admin_quorum_integration` suite runs at the `materialize` layer (hand-built
event lists), not through a live NodeState. So a deterministic in-process "headless closure →
real power change" assertion is not available without replicating the full node bringup.

The ticket's literal "two agent nodes finalize a Tier-2 Conviction poll → power change"
spawned-binary scenario is therefore **deferred to a follow-up**. The e2e-harness `driver` today
has no voting/conviction verbs, driving a real-time Tier-2 finalization clock through a spawned
binary is heavy + flaky, and `--features e2e` never runs in CI. The follow-up would add the
driver verb surface (create-Tier-2-poll, cast-conviction-signal, read-power) + a short
contestability window. The production fix + the closure-selection guard is the durable value
here; the spawned scenario adds transport realism only.

## Non-goals

- No change to GUI dispatch (byte-identical `app.state()` path).
- No change to `apply_auto_exec_set_power` or the tick logic.
- No migration of Tauri managed state to `Arc<Mutex<NodeState>>`.
