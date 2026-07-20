# ZEB-719: Headless Tier-2 auto-exec wiring — Implementation Plan

> **For agentic workers:** Steps use checkbox (`- [ ]`) syntax. TDD where the behavior change is
> observable; the extraction+wiring is compile-coupled so it lands as one reviewable unit.

**Goal:** The headless `serve` path dispatches finalized Tier-2 SetPower auto-exec through the
real membership helper instead of the `SkippedNotAdmin` stub — via an owned `NodeState` handle
the `'static` voting-tick closure can capture.

**Architecture:** Additive `Option<Arc<Mutex<NodeState>>>` seam on `start_node_inner`; extract the
closure builder for testability; expose an owned Arc on `NodeStateAccess` so the headless RPC
reboot path also wires it. GUI path unchanged (`app.state()` via `AppHandle`).

**Tech Stack:** Rust, Tauri, tokio; `cargo nextest` + clippy + fmt gates.

## Global Constraints

- Cargo commands run from `src-tauri/`.
- Iterative gates: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  and scoped `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(<name>)'`.
- Final pre-PR sweep: full `--workspace --all-targets` clippy + nextest + `cargo fmt --all -- --check`.
- `start_node_inner` is in `harmony-app`'s lib; a lib change relinks ~97 integ binaries (~50 min
  full). Use `--lib` / scoped selection for iterative cycles; full sweep only once at the end.
- Commit before running the long gate.

---

### Task 1: `node_state_arc` on `NodeStateAccess`

**Files:**
- Modify: `src-tauri/src/api/mod.rs` (trait `NodeStateAccess` ~L29-38)

**Interfaces:**
- Produces: `NodeStateAccess::node_state_arc(self: Arc<Self>) -> Option<Arc<Mutex<NodeState>>>`
  (default `None`; `Mutex<NodeState>` impl → `Some(self)`).

- [ ] **Step 1: Add the default trait method + `Mutex<NodeState>` override**

```rust
pub trait NodeStateAccess: Send + Sync + 'static {
    fn node_state(&self) -> &Mutex<NodeState>;

    /// ZEB-719: an owned handle to the `NodeState` for the `'static` voting-tick
    /// auto-exec closure on the headless path. The GUI host borrows Tauri's managed
    /// state through its `AppHandle` and dispatches via that seam, so it keeps the
    /// default `None`. `Arc<Self>` receiver stays object-safe for `Arc<dyn ..>`.
    fn node_state_arc(self: Arc<Self>) -> Option<Arc<Mutex<NodeState>>> {
        None
    }
}

impl NodeStateAccess for Mutex<NodeState> {
    fn node_state(&self) -> &Mutex<NodeState> {
        self
    }
    fn node_state_arc(self: Arc<Self>) -> Option<Arc<Mutex<NodeState>>> {
        Some(self)
    }
}
```

- [ ] **Step 2: Add unit tests for the new method** (append to `api/mod.rs`'s `#[cfg(test)]`, or add one)

```rust
#[cfg(test)]
mod node_state_arc_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn mutex_impl_returns_same_arc() {
        let arc: Arc<Mutex<NodeState>> = Arc::new(Mutex::new(NodeState::default()));
        let dynned: Arc<dyn NodeStateAccess> = arc.clone();
        let recovered = dynned.node_state_arc().expect("Mutex impl yields Some");
        assert!(Arc::ptr_eq(&arc, &recovered), "same NodeState allocation");
    }

    #[test]
    fn default_impl_returns_none() {
        struct NoArc;
        impl NodeStateAccess for NoArc {
            fn node_state(&self) -> &Mutex<NodeState> {
                unreachable!("not exercised")
            }
        }
        let d: Arc<dyn NodeStateAccess> = Arc::new(NoArc);
        assert!(d.node_state_arc().is_none(), "default is None (GUI-host class)");
    }
}
```

- [ ] **Step 3: Gate + commit**

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(node_state_arc)'`
Expected: 2 passed.
```bash
git add src-tauri/src/api/mod.rs
git commit -m "feat(api): NodeStateAccess::node_state_arc — owned NodeState handle for headless auto-exec (ZEB-719)"
```

---

### Task 2: `build_auto_exec_fn` extraction + `owned_state` seam + callers

**Files:**
- Modify: `src-tauri/src/lib.rs` — new free fn `build_auto_exec_fn`; `start_node_inner` signature
  (~L3386-3391) + closure build site (~L12755-12831); callers at L3375, L25009, L73602.
- Modify: `src-tauri/src/owner_commands.rs` — caller at L1377.
- Modify: `src-tauri/src/api/rpc.rs` — bespoke `start_node` handler (~L498-503).
- Modify: `src-tauri/tests/profile/profile_isolation.rs:61`, `src-tauri/tests/api/api_server.rs:93`.
- Test: `src-tauri/src/lib.rs` `#[cfg(test)]` — `build_auto_exec_fn` selection test.

**Interfaces:**
- Consumes: `NodeStateAccess::node_state_arc` (Task 1);
  `crate::community_membership::apply_auto_exec_set_power(&Mutex<NodeState>, SpaceId, OwnerAddr, u32)`;
  `crate::community_voting_tick::AutoExecSetPowerFn`.
- Produces:
  - `fn build_auto_exec_fn(wry_handle: Option<tauri::AppHandle<tauri::Wry>>, owned_state: Option<Arc<Mutex<NodeState>>>) -> AutoExecSetPowerFn`
  - `start_node_inner(endpoint, sink, wry_handle, state: &Mutex<NodeState>, owned_state: Option<Arc<Mutex<NodeState>>>)`

- [ ] **Step 1: Write the failing selection test** (add to `lib.rs` `#[cfg(test)]`)

```rust
#[tokio::test]
async fn build_auto_exec_fn_headless_dispatches_not_stub() {
    use crate::owner_state_types::{OwnerAddr, SpaceId};
    let cid = SpaceId([0x11; 16]);
    let target = OwnerAddr([0x22; 16]);

    // No handle (neither GUI nor owned) → defensive stub.
    let stub = build_auto_exec_fn(None, None);
    let out = stub(cid, target, 50).await.expect("stub path returns Ok");
    assert!(
        matches!(out, crate::community_membership::AutoExecOutcome::SkippedNotAdmin),
        "no-handle path must be the SkippedNotAdmin stub"
    );

    // Headless owned handle → dispatches to apply_auto_exec_set_power, which fails
    // CLOSED on a bare NodeState (missing hlc_tracker / dm_outbox / registry). The
    // Err — not Ok(SkippedNotAdmin) — is the exact ZEB-719 discriminator: the
    // closure reached the real helper instead of the stub.
    let arc = std::sync::Arc::new(std::sync::Mutex::new(crate::NodeState::default()));
    let headless = build_auto_exec_fn(None, Some(arc));
    let err = headless(cid, target, 50)
        .await
        .expect_err("headless dispatch must Err on a bare NodeState");
    assert!(
        err.contains("missing") || err.contains("not running"),
        "must be a missing-handles dispatch error (proves real dispatch), got: {err}"
    );
}
```

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(build_auto_exec_fn_headless)'`
Expected: FAIL to compile (`build_auto_exec_fn` not defined) — the red state.

- [ ] **Step 2: Add `build_auto_exec_fn`** (free fn in `lib.rs`, near `start_node_inner`)

```rust
/// ZEB-719: build the `'static` voting-tick auto-exec closure. Dispatch precedence:
/// GUI (`wry_handle`) → fetch Tauri's managed `Mutex<NodeState>` at call time
/// (byte-identical to ZEB-300); else headless (`owned_state`) → dispatch against the
/// owned Arc the serve node runs on; else → `SkippedNotAdmin` (no handle available).
fn build_auto_exec_fn(
    wry_handle: Option<tauri::AppHandle<tauri::Wry>>,
    owned_state: Option<std::sync::Arc<Mutex<NodeState>>>,
) -> crate::community_voting_tick::AutoExecSetPowerFn {
    std::sync::Arc::new(
        move |cid: crate::owner_state_types::SpaceId,
              target: crate::owner_state_types::OwnerAddr,
              new_power: u32| {
            let wry = wry_handle.clone();
            let owned = owned_state.clone();
            Box::pin(async move {
                if let Some(app) = wry {
                    use tauri::Manager as _;
                    let node_state = app.state::<std::sync::Mutex<crate::NodeState>>();
                    return crate::community_membership::apply_auto_exec_set_power(
                        node_state.inner(),
                        cid,
                        target,
                        new_power,
                    )
                    .await;
                }
                if let Some(arc) = owned {
                    return crate::community_membership::apply_auto_exec_set_power(
                        &arc, cid, target, new_power,
                    )
                    .await;
                }
                Ok(crate::community_membership::AutoExecOutcome::SkippedNotAdmin)
            })
        },
    )
}
```

- [ ] **Step 3: Replace the inline closure build site** (`lib.rs` ~L12767-12806)

Delete the inline `let wry_for_auto_exec = wry_handle.clone(); let auto_exec_fn = Arc::new(move |…| { match wry { … } })` block and replace the `auto_exec_fn` binding with:

```rust
                    // ZEB-719: GUI captures the AppHandle (app.state() at call time);
                    // headless captures the owned NodeState Arc. Both dispatch through
                    // apply_auto_exec_set_power; neither is the SkippedNotAdmin stub
                    // unless no handle is available.
                    let auto_exec_fn = build_auto_exec_fn(wry_handle.clone(), owned_state.clone());
```

(Leave the surrounding `emit_fn`, `tick_ctx`, `spawn_voting_tick` wiring unchanged.)

- [ ] **Step 4: Add the `owned_state` param to `start_node_inner`** (`lib.rs` ~L3386)

```rust
pub async fn start_node_inner(
    endpoint: Option<String>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    wry_handle: Option<tauri::AppHandle<tauri::Wry>>,
    state: &Mutex<NodeState>,
    // ZEB-719: owned handle to the SAME NodeState for the 'static voting-tick
    // auto-exec closure on the headless path. `None` in GUI (uses wry_handle);
    // `Some(Arc::clone(&state))` in serve/RPC boots.
    owned_state: Option<std::sync::Arc<Mutex<NodeState>>>,
) -> Result<StartNodeResponse, String> {
```

- [ ] **Step 5: Update all callers** (add the 5th arg per the matrix)

- `lib.rs:3375` (GUI command wrapper): `start_node_inner(endpoint, sink, Some(app), state.inner(), None).await`
- `owner_commands.rs:1377` (GUI restart): append `, None` to the call.
- `lib.rs:25009` (primary serve boot):
  `start_node_inner(None, sink.clone(), None, &state, Some(std::sync::Arc::clone(&state))).await`
- `lib.rs:73602` (in-lib `ok_restart` test): append `, None`.
- `tests/profile/profile_isolation.rs:61`: append `, None`.
- `tests/api/api_server.rs:93`: append `, None`.

- [ ] **Step 6: Make the `start_node` RPC handler bespoke** (`api/rpc.rs`, replace the `rpc!(m, "start_node", …)` invocation)

```rust
    // Node lifecycle. ZEB-719: `start_node` is hand-written (not the generic `rpc!`
    // macro) so it can pass the owned `Arc<Mutex<NodeState>>` for headless Tier-2
    // auto-exec — the macro only exposes the borrowed `node_state()`.
    m.insert(
        "start_node",
        Box::new(
            move |__access: Arc<dyn super::NodeStateAccess>,
                  sink: Arc<dyn NodeEventSink>,
                  raw: serde_json::Value| {
                Box::pin(async move {
                    let owned = __access.clone().node_state_arc();
                    let state = __access.node_state();
                    let raw = if raw.is_null() {
                        serde_json::json!({})
                    } else {
                        raw
                    };
                    let a: StartNodeArgs = serde_json::from_value(raw)
                        .map_err(|e| RpcError::BadArgs(e.to_string()))?;
                    let out = crate::start_node_inner(a.endpoint, sink, None, state, owned)
                        .await
                        .map_err(RpcError::Command)?;
                    serde_json::to_value(out)
                        .map_err(|e| RpcError::Command(format!("serialize: {e}")))
                }) as RpcFuture
            },
        ) as RpcHandler,
    );
```

- [ ] **Step 7: Run the selection test (now green) + compile check**

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(build_auto_exec_fn_headless)'`
Expected: PASS.
Run: `cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/owner_commands.rs src-tauri/src/api/rpc.rs \
        src-tauri/tests/profile/profile_isolation.rs src-tauri/tests/api/api_server.rs
git commit -m "feat(voting): dispatch Tier-2 auto-exec on the headless serve+RPC path (ZEB-719)"
```

---

### Task 3: Docs + final full gate

**Files:**
- Modify: `docs/plans/2026-07-20-zeb-719-headless-tier2-auto-exec.md` (as-built notes if anything drifted).

- [x] **Step 1: Reconcile plan/spec with as-built.**

  **As-built deviation (correct, scope-completing):** the **mint-restart path** was added.
  `mint_owner_identity` restarts the node via `mint_owner_identity_impl` → `start_node_inner`,
  and every agent-testing node mints on first run — so `mint_owner_identity_impl` gained an
  `owned_state` param, and the headless `mint_owner_identity` RPC handler is now bespoke too
  (passes `__access.clone().node_state_arc()`). Without this, the post-mint tick would re-stub
  auto-exec on exactly the headless flow the ticket targets. See spec §3b + updated caller matrix.

  Iterative gate results: clippy `--lib` clean; 3 new unit tests pass
  (`build_auto_exec_fn_tests::headless_dispatches_not_stub`,
  `node_state_arc_mutex_impl_returns_same_arc`, `node_state_arc_default_impl_returns_none`).

- [ ] **Step 2: Full CI-parity gate** (commit first; long run — relinks integ binaries)

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean, clippy clean, all tests pass.

- [ ] **Step 3: Commit any doc reconciliation + open PR.**

## Self-Review (post-write)

- **Spec coverage:** owned_state seam (Task 2), build_auto_exec_fn (Task 2), node_state_arc +
  RPC path (Tasks 1-2), 7 callers (Task 2 Step 5), selection test + node_state_arc test (both).
  Spawned two-node scenario explicitly deferred (spec). ✓
- **Type consistency:** `build_auto_exec_fn` returns `AutoExecSetPowerFn`; closure sig
  `(SpaceId, OwnerAddr, u32) -> Future<Result<AutoExecOutcome, String>>` matches the type alias. ✓
- **No placeholders:** every code step is complete. ✓
