# ZEB-720: Headless Tier-2 voting + spawned two-node e2e scenario — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a `--features e2e` two-node scenario that finalizes a Tier-2 Conviction `SetPower` poll and asserts the target member's materialized power changes on both replicas — which requires first making Tier-2 voting operable in a headless `serve` node.

**Architecture:** Five components in dependency order — (1) decouple the voting-engine handle extraction from the GUI `AppHandle`; (2) make the finalization cadence (contestability window + tick interval) env-overridable with today's constants as defaults; (3) expose three Tier-2 verbs over the `/v1/rpc` surface (create/signal/get); (4) add e2e-harness driver verbs + a per-node env-injection field; (5) the spawned two-node scenario. The ZEB-719 auto-exec dispatch and the injectable `run_voting_tick(now_ms)` already exist, so the tick core is untouched.

**Tech Stack:** Rust (Tauri app lib `harmony-app`), `serde`/`serde_json`, `tokio`; the standalone `e2e-harness` crate (reqwest HTTP client against the headless `serve` node, `tokio-tungstenite` for WS, `cargo-nextest`).

## Global Constraints

- **CI gates (run from `src-tauri/`):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. The `e2e-harness` crate is NOT in the workspace CI gate.
- **Tauri IPC / RPC arg naming:** Rust params `snake_case`; JSON arg keys `camelCase` via `#[serde(rename_all = "camelCase")]` on arg structs.
- **`app.emit(...)` → `crate::node_event_sink::emit_ser(sink.as_ref(), ...)`** in every extracted `_impl`. GUI event delivery must not regress.
- **Env overrides default to today's constants** (`CONTESTABILITY_WINDOW_MS = 86_400_000`, `DEFAULT_TICK_INTERVAL = 60_000 ms`). A build with no envs set behaves exactly as `main`.
- **Do NOT change `run_voting_tick`'s core logic, the `CONTESTABILITY_WINDOW_MS` constant value, or add any wall-clock sleep to the tick.** Finalization stays a pure function of injected/config time.
- **Never remove `#[cfg(any(test, feature = "test-fixtures"))]` gates.** Not touched here, but the full test-run gate requires `--features test-fixtures`.
- **`e2e-harness` runs manually:** build the binary first (`cd src-tauri && cargo build --bin harmony-app`), then `cd e2e-harness && cargo nextest run --features e2e --test-threads 1`.

## File Structure

| File | Responsibility | Tasks |
|---|---|---|
| `src-tauri/src/lib.rs` | `VotingEngineNodeHandles` decouple; `_impl` seams for 3 voting verbs; env-read wiring for tick spawn + context | 1, 2, 3 |
| `src-tauri/src/community_voting_tick.rs` | `VotingTickContext.contestability_window_ms` field + read it at the two finalize checks | 2 |
| `src-tauri/src/api/rpc.rs` | arg structs + `rpc!` registrations for 3 verbs + drift-guard allowlist update | 3 |
| `e2e-harness/src/node.rs` | `NodeConfig.extra_env` field + `.env(...)` emission in `spawn` | 4 |
| `e2e-harness/src/driver.rs` | 4 driver verbs (create/signal/get proposal + `member_power`) | 4 |
| `e2e-harness/tests/e2e_two_node.rs` | `s13_tier2_conviction_setpower` scenario | 5 |

---

## Task 1: Decouple voting-engine handle extraction from the GUI `AppHandle`

**Files:**
- Modify: `src-tauri/src/lib.rs` — `VotingEngineNodeHandles` struct (`:50018`), `extract` (`:50033`, `:50058`); `ensure_voting_engine_for` signature (`:49796`) + engine construction (`:49877`); 11 `extract` call sites (`:48061, :48279, :48371, :48443, :48516, :48614, :48676, :50383, :50471, :50599, :50671`); ZEB-445 comment (`:11434-11439`).

**Interfaces:**
- Consumes: nothing new.
- Produces: `VotingEngineNodeHandles::extract(state: &std::sync::Mutex<NodeState>) -> Result<Self, String>` (was `&tauri::State<'_, Mutex<NodeState>>`); `VotingEngineNodeHandles.app_handle_wry: Option<tauri::AppHandle<tauri::Wry>>` (was non-optional). Both enable headless callers (Task 3's `_impl` seams call `extract(state)` with a plain `&Mutex`).

**Why no dedicated unit test:** `extract` returning `Ok` requires a *fully-wired* `NodeState` (8 other `Option` fields must be `Some` — `hlc_tracker`, `dm_device_id`, `dm_self_owner`, `community_registry`, `dm_outbox`, `crdt_state`, `voting_log_adapter_request_tx`, `dm_identity_pub_64`), which only a full `start_node` bringup produces. Building that in a unit test is precisely what the existing `apply_auto_exec_set_power` tests document as out of scope. This task is a **type-level refactor**: its correctness gate is (a) it compiles under `--all-targets`, (b) the GUI path is unchanged (all existing `community_voting_*` + voting-IPC tests still pass), (c) clippy clean. The headless *behavior* it unlocks is proven end-to-end by Task 5.

- [ ] **Step 1: Make the struct field optional.**

In `src-tauri/src/lib.rs:50018`, change:
```rust
    app_handle_wry: tauri::AppHandle<tauri::Wry>,
```
to:
```rust
    // ZEB-720: Optional so headless `serve` (no GUI AppHandle) can extract
    // voting handles. `None` ⇒ engine runs headless; Tier-3 + delegate-on-
    // behalf emits (already `if let Some(app)`-gated) simply no-op.
    app_handle_wry: Option<tauri::AppHandle<tauri::Wry>>,
```

- [ ] **Step 2: Change `extract`'s signature and drop the hard requirement.**

In `src-tauri/src/lib.rs:50033`, change the signature:
```rust
    fn extract(state_lock: &tauri::State<'_, Mutex<NodeState>>) -> Result<Self, String> {
```
to:
```rust
    fn extract(state_lock: &std::sync::Mutex<NodeState>) -> Result<Self, String> {
```
And at `:50058`, change:
```rust
            app_handle_wry: g.app_handle_wry.clone().ok_or("app_handle_wry missing")?,
```
to:
```rust
            // ZEB-720: no longer required — headless nodes have no AppHandle.
            app_handle_wry: g.app_handle_wry.clone(),
```

- [ ] **Step 3: Thread the `Option` through `ensure_voting_engine_for`.**

In `src-tauri/src/lib.rs:49796`, change the param:
```rust
    app_handle: tauri::AppHandle<tauri::Wry>,
```
to:
```rust
    // ZEB-720: `None` in headless `serve`; the engine's `app_handle` field
    // is already `Option`, so Tier-3/delegate emits no-op without a GUI.
    app_handle: Option<tauri::AppHandle<tauri::Wry>>,
```
And at the engine construction site `:49877`, change:
```rust
            app_handle: Some(app_handle.clone()),
```
to:
```rust
            app_handle: app_handle.clone(),
```
(`ensure_engine` at `:50098` already passes `self.app_handle_wry.clone()`, which is now `Option` — matches the new param with no further change.)

- [ ] **Step 4: Update the 11 `extract` call sites to pass `&Mutex` instead of `&State`.**

At each of `src-tauri/src/lib.rs:48061, :48279, :48371, :48443, :48516, :48614, :48676, :50383, :50471, :50599, :50671`, change:
```rust
    let handles = VotingEngineNodeHandles::extract(&state_lock)?;
```
to:
```rust
    let handles = VotingEngineNodeHandles::extract(state_lock.inner())?;
```
(`tauri::State::inner()` returns `&Mutex<NodeState>`. Every one of these 11 sites is a `#[tauri::command]` whose `state_lock: tauri::State<'_, Mutex<NodeState>>`, so `.inner()` is always in scope. Do NOT change any site that Task 3 later converts to an `_impl` — those become `extract(state)` in Task 3; leave them as `extract(state_lock.inner())` here so the tree stays compiling between tasks.)

- [ ] **Step 5: Update the ZEB-445 comment that declared voting Tauri-bound.**

In `src-tauri/src/lib.rs` around `:11434-11439`, the comment currently reads `ZEB-445: None in serve mode (voting IPCs stay Tauri-bound).` Change it to note the reversal, e.g.:
```rust
    // ZEB-445/ZEB-720: `None` in serve mode. Voting handle extraction no
    // longer requires this (ZEB-720 made it Optional); the Tier-2 create/
    // signal/get verbs are now headless-capable over the RPC surface.
    guard.app_handle_wry = wry_handle.clone();
```
(Keep the assignment itself unchanged — only the comment.)

- [ ] **Step 6: Compile-check the decouple in isolation.**

Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -30`
Expected: clean (exit 0). If any `ensure_voting_engine_for` caller other than `ensure_engine` exists and passes a non-`Option` `AppHandle`, the compiler names it — wrap that argument in `Some(...)`. (Recon found `ensure_engine` is the only caller; this step catches any missed site by compiler enumeration.)

- [ ] **Step 7: Run the voting regression suite (GUI path unchanged).**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voting) or test(community_voting) or binary(community_voting_tests)'`
Expected: all pass (0 failed). These exercise the engine + Tier-2 command paths that still pass an (always-`Some`) `AppHandle`; they prove the decouple didn't regress GUI behavior.

- [ ] **Step 8: fmt + clippy + commit.**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add -A && git commit -m "ZEB-720: decouple voting-engine handle extraction from GUI AppHandle

VotingEngineNodeHandles.app_handle_wry -> Option; extract() takes &Mutex<NodeState>.
Headless serve can now extract voting handles instead of failing 'app_handle_wry
missing'. GUI path unchanged (always Some). Reverses the ZEB-445 Tauri-bound note.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```
Expected: clippy exit 0; commit succeeds.

---

## Task 2: Finalization-cadence env overrides

**Files:**
- Modify: `src-tauri/src/community_voting_tick.rs` — `VotingTickContext` struct (`:99-107`); the two finalize checks (`:271`, `:306`); add a unit test.
- Modify: `src-tauri/src/lib.rs` — `VotingTickContext` construction (`:12766-12773`) and `spawn_voting_tick` interval (`:12774-12777`).

**Interfaces:**
- Consumes: nothing from prior tasks.
- Produces: `VotingTickContext.contestability_window_ms: i128`. `run_voting_tick` reads it instead of the bare `CONTESTABILITY_WINDOW_MS` constant. Env `HARMONY_VOTING_CONTESTABILITY_WINDOW_MS` (ms, default `86_400_000`) and `HARMONY_VOTING_TICK_INTERVAL_MS` (ms, default `60_000`) selected at node bringup. Task 5 sets both to short values on the spawned nodes.

- [ ] **Step 1: Write the failing test for short-window finalize.**

Add to the `#[cfg(test)] mod` tests in `src-tauri/src/community_voting_tick.rs` (next to `community_voting_tick_tier2_contestability_finalize_after_24h` at `:838`). This is that test's fixture verbatim (`make_tier2_config` / `make_tier2_poll` / `make_ctx_with_logs` are the existing test helpers), with the field overridden to a 2 s window (fields are `pub`, so override after construction) and finalized just past the short window instead of at +25h:

```rust
    #[tokio::test]
    async fn tier2_finalizes_when_ctx_window_is_short() {
        let cid = SpaceId([0x33; 16]);
        let pid = PollId([0x44; 32]);
        let cfg = make_tier2_config(AutoExecAction::None);
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 0, 86_400_000);
        t2.per_voter.insert(OwnerAddr([0xbb; 16]), vs);
        let reached_at = 1_000i128;
        t2.threshold_reached_at_ms = Some(reached_at);

        let mut log = VotingLog::new();
        log.polls
            .insert(pid, make_tier2_poll(cid, pid, Lifecycle::ThresholdReached, t2));
        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));

        let (mut ctx, _events, _) = make_ctx_with_logs(logs, reached_at);
        ctx.contestability_window_ms = 2_000; // ZEB-720 short window

        // 1s < 2s window: not finalized.
        let s1 = run_voting_tick(&ctx, reached_at + 1_000).await.unwrap();
        assert_eq!(s1.tier2_proposals_finalized, 0, "1s < 2s window: not yet");
        // 3s > 2s window: finalized exactly once.
        let s2 = run_voting_tick(&ctx, reached_at + 3_000).await.unwrap();
        assert_eq!(s2.tier2_proposals_finalized, 1, "3s > 2s window: finalized");
    }
```
> **Implementer:** this mirrors `community_voting_tick_tier2_contestability_finalize_after_24h` (`:838-879`) exactly except for the `ctx.contestability_window_ms = 2_000` override and the two `now_ms` args. If `make_ctx_with_logs` returns a non-`mut`-friendly tuple, bind `mut ctx`.

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(tier2_finalizes_when_ctx_window_is_short)'`
Expected: FAIL to compile — `VotingTickContext` has no field `contestability_window_ms` (and `make_ctx_with_logs` doesn't set it yet).

- [ ] **Step 2: Add the field to `VotingTickContext`.**

In `src-tauri/src/community_voting_tick.rs`, inside `pub struct VotingTickContext { ... }` (ends `:107`), add:
```rust
    /// ZEB-720: contestability window before a ThresholdReached Tier-2 poll
    /// finalizes. Defaults to `CONTESTABILITY_WINDOW_MS` (24h) at every
    /// production bringup; overridable via `HARMONY_VOTING_CONTESTABILITY_WINDOW_MS`
    /// for deterministic e2e finalization. Read as pure config — the tick
    /// never reads the constant directly.
    pub contestability_window_ms: i128,
```
Then update the test helper `make_ctx_with_logs` (in the same `#[cfg(test)] mod`) to set the field to the default so every existing tick test is unchanged:
```rust
        contestability_window_ms: CONTESTABILITY_WINDOW_MS,
```
(add this line inside the `VotingTickContext { ... }` that `make_ctx_with_logs` builds.)

- [ ] **Step 3: Read the field at the two finalize checks.**

In `src-tauri/src/community_voting_tick.rs`, at `:271`:
```rust
                    if (now_ms - uncontested_since) >= CONTESTABILITY_WINDOW_MS {
```
→
```rust
                    if (now_ms - uncontested_since) >= ctx.contestability_window_ms {
```
And at `:306` (inside the `window_still_clear` closure):
```rust
                                        (now_ms - uncontested_since) >= CONTESTABILITY_WINDOW_MS
```
→
```rust
                                        (now_ms - uncontested_since) >= ctx.contestability_window_ms
```
(Leave `pub const CONTESTABILITY_WINDOW_MS` defined — it is now the default source, referenced in lib.rs Step 5.)

- [ ] **Step 4: Run the test — it passes.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(tier2_finalizes_when_ctx_window_is_short)'`
Expected: PASS. Also run the sibling 24h/12h tests to confirm no regression: `-E 'test(contestability)'` → PASS (they go through `make_ctx_with_logs`, now defaulting to the 24h window).
> **Implementer:** the sibling tick tests build their context via `make_ctx_with_logs` (fixed in Step 2), so they need no per-test edit. Only a *direct* `VotingTickContext { ... }` literal would — the compiler enumerates any such site; use `CONTESTABILITY_WINDOW_MS`. (The one production literal, in lib.rs, is handled in Step 5.)

- [ ] **Step 5: Wire the env reads at node bringup (lib.rs).**

In `src-tauri/src/lib.rs`, the `VotingTickContext { ... }` literal at `:12766-12773` — add the field:
```rust
                        // ZEB-720: default 24h; short override only when the
                        // operator sets the env (never in production).
                        contestability_window_ms: std::env::var(
                            "HARMONY_VOTING_CONTESTABILITY_WINDOW_MS",
                        )
                        .ok()
                        .and_then(|s| s.parse::<i128>().ok())
                        .unwrap_or(crate::community_voting_tick::CONTESTABILITY_WINDOW_MS),
```
And replace the `spawn_voting_tick` interval arg at `:12774-12777`:
```rust
                    let handle = crate::community_voting_tick::spawn_voting_tick(
                        tick_ctx,
                        crate::community_voting_tick::DEFAULT_TICK_INTERVAL,
                    );
```
→
```rust
                    // ZEB-720: default 60s; short override only for e2e.
                    let tick_interval = std::env::var("HARMONY_VOTING_TICK_INTERVAL_MS")
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(std::time::Duration::from_millis)
                        .unwrap_or(crate::community_voting_tick::DEFAULT_TICK_INTERVAL);
                    let handle = crate::community_voting_tick::spawn_voting_tick(
                        tick_ctx,
                        tick_interval,
                    );
```

- [ ] **Step 6: Add an env-fallback unit test.**

Add to `src-tauri/src/community_voting_tick.rs` tests (no env dependency — test the parse-or-default expression directly by factoring it, OR assert the constant default). Minimal, deterministic version that needs no process-env mutation:
```rust
    #[test]
    fn contestability_window_env_parse_falls_back_to_default() {
        // Mirrors the lib.rs bringup expression: garbage/missing → 24h default.
        let parse = |v: Option<&str>| -> i128 {
            v.and_then(|s| s.parse::<i128>().ok())
                .unwrap_or(CONTESTABILITY_WINDOW_MS)
        };
        assert_eq!(parse(None), CONTESTABILITY_WINDOW_MS);
        assert_eq!(parse(Some("not-a-number")), CONTESTABILITY_WINDOW_MS);
        assert_eq!(parse(Some("2000")), 2000);
    }
```
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(contestability_window_env_parse_falls_back_to_default)'` → PASS.

- [ ] **Step 7: fmt + clippy + commit.**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add -A && git commit -m "ZEB-720: make Tier-2 finalization cadence env-overridable

VotingTickContext.contestability_window_ms (default 24h const) + tick interval
read HARMONY_VOTING_CONTESTABILITY_WINDOW_MS / HARMONY_VOTING_TICK_INTERVAL_MS at
bringup. Defaults reproduce main; short overrides let e2e finalize deterministically
with no wall-clock sleep and no change to run_voting_tick's core.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

## Task 3: Tier-2 RPC surface (create / signal / get)

**Files:**
- Modify: `src-tauri/src/lib.rs` — extract `_impl` seams for `voting_create_tier2_proposal` (`:50305-50446`), `voting_signal_tier2` (`:50453-50572`), `voting_get_tier2_proposal` (`:50867-50910`).
- Modify: `src-tauri/src/api/rpc.rs` — arg structs (near `:78`), `rpc!` registrations in `build_registry()` (near the community block ~`:584`), allowlist test (`:2133+`).

**Interfaces:**
- Consumes: `VotingEngineNodeHandles::extract(&Mutex)` (Task 1).
- Produces: RPC commands `voting_create_tier2_proposal`, `voting_signal_tier2`, `voting_get_tier2_proposal` on `/v1/rpc`. `create` accepts a hex `setPowerTarget` + numeric `setPowerNewPower` (NOT a raw `AutoExecAction` — avoids pushing the `OwnerAddr` bstr encoding across the JSON boundary). Task 4's driver verbs call these.

- [ ] **Step 1: Extract `voting_get_tier2_proposal_impl` (read; no sink).**

In `src-tauri/src/lib.rs`, split `voting_get_tier2_proposal` (`:50867-50910`) into a thin wrapper + `_impl`. The current body reads `state_lock` directly (no `extract`, no `app`), so the `_impl` just takes `&Mutex`:
```rust
#[tauri::command]
async fn voting_get_tier2_proposal(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    proposal_id: String,
) -> Result<Tier2ProposalExport, String> {
    voting_get_tier2_proposal_impl(state_lock.inner(), proposal_id).await
}

pub(crate) async fn voting_get_tier2_proposal_impl(
    state_lock: &std::sync::Mutex<NodeState>,
    proposal_id: String,
) -> Result<Tier2ProposalExport, String> {
    // ... the existing body verbatim from :50872-50909 ...
}
```
> **Implementer:** move lines `:50872-50909` verbatim into the `_impl`; the only change is the function header. `Tier2ProposalExport` already `#[derive(Serialize)]` (it's a Tauri return type), so the RPC layer serializes it.

- [ ] **Step 2: Extract `voting_signal_tier2_impl` (mutation; sink).**

`voting_signal_tier2<R>` (`:50453-50572`) uses `app` only at the final emit (`:50568`) and calls `extract(&state_lock)` (`:50471`). Split it:
```rust
#[tauri::command]
async fn voting_signal_tier2<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    proposal_id: String,
    support: bool,
) -> Result<(), String> {
    let sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> = std::sync::Arc::new(app);
    voting_signal_tier2_impl(state_lock.inner(), sink, proposal_id, support).await
}

pub(crate) async fn voting_signal_tier2_impl(
    state: &std::sync::Mutex<NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    proposal_id: String,
    support: bool,
) -> Result<(), String> {
    // ... existing body :50460-50571 verbatim, EXCEPT:
    //   - the extract call becomes: VotingEngineNodeHandles::extract(state)?
    //   - the final emit block is replaced (see Step 4 below)
}
```

- [ ] **Step 3: Extract `voting_create_tier2_proposal_impl` (mutation; sink).**

`voting_create_tier2_proposal<R>` (`:50305-50446`) uses `app` only at the final emit (`:50442`) and calls `extract(&state_lock)` (`:50383`). Split it, keeping the full param list:
```rust
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn voting_create_tier2_proposal<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    channel_id: String,
    proposal_text: String,
    half_life_seconds: Option<u32>,
    threshold_min: Option<i64>,
    threshold_max: Option<i64>,
    beta: Option<u8>,
    delegation_allowed: Option<bool>,
    auto_exec: Option<crate::community_voting_conviction::AutoExecAction>,
    min_power: Option<u32>,
) -> Result<String, String> {
    let sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> = std::sync::Arc::new(app);
    voting_create_tier2_proposal_impl(
        state_lock.inner(), sink, community_id, channel_id, proposal_text,
        half_life_seconds, threshold_min, threshold_max, beta, delegation_allowed,
        auto_exec, min_power,
    ).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn voting_create_tier2_proposal_impl(
    state: &std::sync::Mutex<NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    community_id: String,
    channel_id: String,
    proposal_text: String,
    half_life_seconds: Option<u32>,
    threshold_min: Option<i64>,
    threshold_max: Option<i64>,
    beta: Option<u8>,
    delegation_allowed: Option<bool>,
    auto_exec: Option<crate::community_voting_conviction::AutoExecAction>,
    min_power: Option<u32>,
) -> Result<String, String> {
    // ... existing body :50321-50445 verbatim, EXCEPT:
    //   - the extract call becomes: VotingEngineNodeHandles::extract(state)?
    //   - the final emit block is replaced (see Step 4 below)
}
```
Note the `_impl` keeps `auto_exec: Option<AutoExecAction>` — the Tauri wrapper passes its own deserialized param; the RPC handler (Step 6) BUILDS this value in Rust from hex args.

- [ ] **Step 4: Swap the three `app.emit` sites for `emit_ser`.**

In `voting_create_tier2_proposal_impl`, replace (was `:50442-50444`):
```rust
    if let Err(e) = app.emit("voting-tier2-proposal-created", &payload) {
        tracing::warn!(error = %e, "voting-tier2-proposal-created emit failed");
    }
```
with:
```rust
    crate::node_event_sink::emit_ser(sink.as_ref(), "voting-tier2-proposal-created", &payload);
```
In `voting_signal_tier2_impl`, replace (was `:50568-50570`) the `app.emit("voting-tier2-signal-cast", &payload)` block with:
```rust
    crate::node_event_sink::emit_ser(sink.as_ref(), "voting-tier2-signal-cast", &payload);
```
(`get` has no emit.)

- [ ] **Step 5: Compile-check the seams.**

Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -20`
Expected: clean. (If `payload` or an intermediate binding was only used by the removed `if let Err` and now warns unused, confirm it is still constructed — it is passed to `emit_ser`.)

- [ ] **Step 6: Add arg structs + `rpc!` registrations.**

In `src-tauri/src/api/rpc.rs`, in the "Arg structs" region (after `:78`), add:
```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VotingCreateTier2Args {
    community_id: String,
    channel_id: String,
    proposal_text: String,
    #[serde(default)]
    half_life_seconds: Option<u32>,
    #[serde(default)]
    threshold_min: Option<i64>,
    #[serde(default)]
    threshold_max: Option<i64>,
    #[serde(default)]
    beta: Option<u8>,
    #[serde(default)]
    delegation_allowed: Option<bool>,
    #[serde(default)]
    min_power: Option<u32>,
    /// Hex-encoded 16-byte OwnerAddr of the SetPower target. When present
    /// (with `set_power_new_power`), the handler builds an
    /// `AutoExecAction::SetPower`; otherwise auto_exec is None. Hex avoids
    /// pushing the OwnerAddr bstr encoding across the JSON boundary.
    #[serde(default)]
    set_power_target: Option<String>,
    #[serde(default)]
    set_power_new_power: Option<u32>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VotingSignalTier2Args {
    proposal_id: String,
    support: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VotingGetTier2Args {
    proposal_id: String,
}
```
In `build_registry()`, near the community block (after the `list_community_members` registration ~`:590`), add:
```rust
    // tier-2 conviction voting (ZEB-720)
    rpc!(
        m,
        "voting_create_tier2_proposal",
        VotingCreateTier2Args,
        |state, sink, a| async move {
            let auto_exec = match (a.set_power_target, a.set_power_new_power) {
                (Some(hex_target), Some(np)) => {
                    let bytes: [u8; 16] = hex::decode(&hex_target)
                        .map_err(|e| format!("invalid setPowerTarget hex: {e}"))?
                        .as_slice()
                        .try_into()
                        .map_err(|_| "setPowerTarget must be 16 bytes (32 hex chars)".to_string())?;
                    Some(crate::community_voting_conviction::AutoExecAction::SetPower {
                        target_pubkey: crate::owner_state_types::OwnerAddr(bytes),
                        new_power: np,
                    })
                }
                _ => None,
            };
            crate::voting_create_tier2_proposal_impl(
                state, sink, a.community_id, a.channel_id, a.proposal_text,
                a.half_life_seconds, a.threshold_min, a.threshold_max, a.beta,
                a.delegation_allowed, auto_exec, a.min_power,
            ).await
        }
    );
    rpc!(
        m,
        "voting_signal_tier2",
        VotingSignalTier2Args,
        |state, sink, a| async move {
            crate::voting_signal_tier2_impl(state, sink, a.proposal_id, a.support).await
        }
    );
    rpc!(
        m,
        "voting_get_tier2_proposal",
        VotingGetTier2Args,
        |state, _sink, a| async move {
            crate::voting_get_tier2_proposal_impl(state, a.proposal_id).await
        }
    );
```
> **Implementer:** confirm `hex` and `crate::owner_state_types::OwnerAddr` are reachable from `rpc.rs` (both are crate-internal; add `use` if needed). Verify `OwnerAddr`'s tuple field is `pub` (it is: `pub struct OwnerAddr(pub [u8; 16])`).

- [ ] **Step 7: Update the drift-guard allowlist test (write the failing assertion first).**

In `src-tauri/src/api/rpc.rs`, in `registry_has_exactly_the_curated_v1_surface`'s `expected` vector (`:2133+`), add after the admin-recovery block:
```rust
            // tier-2 conviction voting (ZEB-720)
            "voting_create_tier2_proposal",
            "voting_signal_tier2",
            "voting_get_tier2_proposal",
```
Run BEFORE adding (to see red) then AFTER: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(registry_has_exactly_the_curated_v1_surface)'`
Expected: with the registrations from Step 6 present but the allowlist NOT yet updated, the test FAILS (registry has 3 names not in `expected`). After adding the three names, PASS.
> **Implementer:** the test sorts `names`; ensure `expected` is compared consistently (match the existing test's comparison — it either sorts `expected` too or compares as a set; follow whatever the current code does).

- [ ] **Step 8: fmt + clippy + full scoped test + commit.**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cargo nextest run --locked --features test-fixtures -E 'test(registry_has_exactly_the_curated_v1_surface) or test(voting) or binary(community_voting_tests)' 2>&1 | tail -8
git add -A && git commit -m "ZEB-720: expose Tier-2 create/signal/get over /v1/rpc

_impl seams (app.emit -> sink.emit_ser) + rpc! registrations + drift-guard
allowlist. create takes hex setPowerTarget + numeric power and builds
AutoExecAction::SetPower in Rust, avoiding OwnerAddr bstr over JSON.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```
Expected: clippy exit 0; tests pass.

---

## Task 4: e2e-harness driver verbs + per-node env injection

**Files:**
- Modify: `e2e-harness/src/node.rs` — `NodeConfig` struct (`:15-24`) + `NodeConfig::new` (`:27-35`) + `spawn`'s `Command` chain (`:79-96`).
- Modify: `e2e-harness/src/driver.rs` — add 4 verbs (after the admin-recovery block, ~`:1181`).

**Interfaces:**
- Consumes: the three RPC commands (Task 3).
- Produces: `NodeConfig.extra_env: Vec<(String, String)>`; driver `create_tier2_setpower_proposal`, `signal_tier2`, `get_tier2_proposal`, `member_power`. Task 5 uses all of them.

**No dedicated unit test:** driver verbs require a live spawned node; they are exercised by Task 5. Gate is compilation.

- [ ] **Step 1: Add `extra_env` to `NodeConfig`.**

In `e2e-harness/src/node.rs`, add a field to the `NodeConfig` struct (after `log_dir`, `:23`):
```rust
    /// ZEB-720: extra env vars injected into the spawned node (e.g. short
    /// voting cadence). Layered on top of the hardcoded `.env(...)` set in
    /// `spawn`; the child already inherits the parent env (no env_clear).
    pub extra_env: Vec<(String, String)>,
```
And in `NodeConfig::new` (`:28-34`), add the default:
```rust
            extra_env: Vec::new(),
```

- [ ] **Step 2: Emit the extra env in `spawn`.**

In `e2e-harness/src/node.rs`, the `Command` is built as a single chained expression ending `:96` (`.env("HARMONY_API_PORT", "0")`) then `.stdin(...)`. Because `.envs(...)` accepts an iterator of pairs, insert it right after the `HARMONY_API_PORT` line and before `.stdin`:
```rust
            .env("HARMONY_API_PORT", "0")
            .envs(config.extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
```
> **Implementer:** `Command::envs` exists on `tokio::process::Command`. If the chain's ownership makes `.envs` on a borrowed iterator awkward, collect first: `for (k, v) in &config.extra_env { cmd.env(k, v); }` after building `cmd` — but the inline `.envs(...)` is preferred and matches the existing chained style.

- [ ] **Step 3: Add the driver verbs.**

In `e2e-harness/src/driver.rs`, after the admin-recovery block (~`:1181`), add:
```rust
// ── Tier-2 Conviction voting (ZEB-720) ───────────────────────────────

/// Create a Tier-2 Conviction proposal whose finalization auto-execs
/// `SetPower{target, new_power}`. `threshold_min` is the raw ms-scale
/// conviction floor (small ⇒ threshold crosses quickly). Returns the hex
/// proposal id.
pub async fn create_tier2_setpower_proposal(
    node: &NodeHandle,
    community_id: &str,
    channel_id: &str,
    proposal_text: &str,
    target_addr: &str,
    new_power: u32,
    threshold_min: i64,
    min_power: u32,
) -> anyhow::Result<String> {
    let v = node
        .rpc(
            "voting_create_tier2_proposal",
            json!({
                "communityId": community_id,
                "channelId": channel_id,
                "proposalText": proposal_text,
                "thresholdMin": threshold_min,
                "minPower": min_power,
                "setPowerTarget": target_addr,
                "setPowerNewPower": new_power,
            }),
        )
        .await?;
    as_str(&v)
}

/// Cast (support=true) or withdraw (support=false) a Tier-2 conviction signal.
pub async fn signal_tier2(
    node: &NodeHandle,
    proposal_id: &str,
    support: bool,
) -> anyhow::Result<()> {
    node.rpc(
        "voting_signal_tier2",
        json!({ "proposalId": proposal_id, "support": support }),
    )
    .await
    .map(|_| ())
}

/// Full Tier-2 proposal export (lifecycle, tally). Raw DTO for assertions.
pub async fn get_tier2_proposal(node: &NodeHandle, proposal_id: &str) -> anyhow::Result<Value> {
    node.rpc(
        "voting_get_tier2_proposal",
        json!({ "proposalId": proposal_id }),
    )
    .await
}

/// A member's materialized power from the roster, or `None` if absent.
/// Non-array roster ⇒ loud error (mirrors `list_community_members`).
pub async fn member_power(
    node: &NodeHandle,
    community_id: &str,
    member_owner: &str,
) -> anyhow::Result<Option<u64>> {
    let members = list_community_members(node, community_id).await?;
    Ok(members.iter().find_map(|m| {
        (m.get("addr").and_then(Value::as_str) == Some(member_owner))
            .then(|| m.get("power").and_then(Value::as_u64))
            .flatten()
    }))
}
```
> **Implementer:** confirm `as_str`, `list_community_members`, `json!`, `Value`, `NodeHandle` are already in scope in `driver.rs` (they are — `as_str` at `:29`, `list_community_members` at `:120`). Verify the `get_tier2_proposal` DTO's lifecycle field name by reading `Tier2ProposalExport`'s serde (camelCase) if Task 5 asserts on lifecycle; `member_power` only needs `addr`/`power` which `MemberInfoDto` provides.

- [ ] **Step 4: Compile the harness.**

Run: `cd e2e-harness && cargo build 2>&1 | tail -10`
Expected: clean (exit 0). (No `--features e2e` needed to compile `src/`.)

- [ ] **Step 5: Commit.**

```bash
git add -A && git commit -m "ZEB-720: e2e-harness Tier-2 voting driver verbs + NodeConfig.extra_env

create_tier2_setpower_proposal / signal_tier2 / get_tier2_proposal / member_power
over the new RPC surface; NodeConfig.extra_env injects per-node env (short voting
cadence) via spawn's Command chain.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

## Task 5: The spawned two-node scenario `s13_tier2_conviction_setpower`

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs` — add `s13_tier2_conviction_setpower` (behind the file-level `#![cfg(feature = "e2e")]`).

**Interfaces:**
- Consumes: Task 4 driver verbs + `NodeConfig.extra_env`; the `two_minted_nodes`/`owner_id`/`create_community`/`generate_invite`/`poll_join_iroh`/`roster_has_joined`/`create_channel`/`poll_until` helpers already in the harness.

- [ ] **Step 1: Write the scenario.**

Add to `e2e-harness/tests/e2e_two_node.rs`, cloning the S11 skeleton (`:2473-2620`) for spawn/join/roster-converge. Because both nodes need the short voting cadence, build their `NodeConfig`s with `extra_env` set (do NOT use `two_minted_nodes`, which sets no env — inline the spawn+mint, mirroring `two_minted_nodes` at `:91-122`):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s13_tier2_conviction_setpower() {
    use e2e_harness::driver::*;
    use e2e_harness::{NodeConfig, NodeHandle, RunDir};
    use serde_json::Value;
    use std::path::PathBuf;
    use std::time::Duration;

    // Short, deterministic finalization cadence: window (1500ms) >> tick
    // interval (250ms) so the poll is observed ThresholdReached before it
    // finalizes; poll_until absorbs tick jitter. No wall-clock sleep, no 24h.
    let voting_env = vec![
        ("HARMONY_VOTING_CONTESTABILITY_WINDOW_MS".to_string(), "1500".to_string()),
        ("HARMONY_VOTING_TICK_INTERVAL_MS".to_string(), "250".to_string()),
    ];

    let run = RunDir::new("s13-tier2-setpower").expect("run dir");
    let alice_home = fresh_home("s13-tier2-a");
    let bob_home = fresh_home("s13-tier2-b");
    let mk = |home: &tempfile::TempDir, profile: &str| {
        let mut cfg = NodeConfig::new(PathBuf::from(home.path()), profile);
        cfg.log_dir = Some(run.log_dir());
        cfg.extra_env = voting_env.clone();
        cfg
    };
    let alice = NodeHandle::spawn(mk(&alice_home, "alice")).await.expect("spawn alice");
    let bob = NodeHandle::spawn(mk(&bob_home, "bob")).await.expect("spawn bob");
    alice.rpc("mint_owner_identity", json!({})).await.expect("alice mint");
    bob.rpc("mint_owner_identity", json!({})).await.expect("bob mint");

    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Community + channel + join + roster convergence (s11 skeleton).
    let community = create_community(&alice, "s13-community", true).await.expect("create community");
    let channel = create_channel(&alice, &community, "general").await.expect("create channel");
    let invite = generate_invite(&alice, &community).await.expect("generate invite");
    poll_join_iroh(&bob, &invite, Duration::from_secs(240)).await.expect("bob joins");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner).await?.then_some(()))
    }).await.expect("alice sees bob joined");
    poll_until(Duration::from_secs(120), || async {
        Ok(roster_has_joined(&bob, &community, &alice_owner).await?.then_some(()))
    }).await.expect("bob sees alice joined");

    // Record bob's initial power (baseline; must differ from the target 60).
    let bob_power_before = member_power(&alice, &community, &bob_owner)
        .await.expect("read bob power").expect("bob in roster");
    assert_ne!(bob_power_before, 60, "test target 60 must differ from baseline");

    // Alice (admin, power 100) creates a Tier-2 SetPower{bob -> 60} proposal.
    // 60 < POWER_THRESHOLDS.max ⇒ NOT admin-affecting ⇒ direct-mint at
    // admin_quorum==1 (no AdminProposal route). thresholdMin=1 (raw ms) so a
    // support signal crosses the dynamic threshold's floor immediately;
    // minPower=0 so bob is eligible to signal regardless of his power.
    let proposal = create_tier2_setpower_proposal(
        &alice, &community, &channel, "raise bob to 60", &bob_owner, 60, 1, 0,
    ).await.expect("create tier-2 proposal");

    // Both replicas observe the proposal, then both signal support.
    poll_until(Duration::from_secs(120), || async {
        Ok(get_tier2_proposal(&bob, &proposal).await.ok().map(|_| ()))
    }).await.expect("bob observes the proposal");
    signal_tier2(&alice, &proposal, true).await.expect("alice signals support");
    signal_tier2(&bob, &proposal, true).await.expect("bob signals support");

    // Alice's tick finalizes after the 1.5s window and direct-mints SetPower;
    // bob converges via membership sync. Assert on BOTH replicas.
    for (name, node) in [("alice", &alice), ("bob", &bob)] {
        poll_until(Duration::from_secs(120), || async {
            let p = member_power(node, &community, &bob_owner).await?;
            Ok((p == Some(60)).then_some(()))
        }).await.unwrap_or_else(|e| panic!("{name} sees bob power == 60: {e}"));
    }

    run.mark_success();
    drop((alice, bob, alice_home, bob_home));
}
```
> **Implementer:** verify the exact names/signatures of `fresh_home`, `create_channel`, `generate_invite`, `poll_join_iroh`, `owner_id`, `create_community`, `RunDir`, `NodeConfig`, `NodeHandle` against the current file (all used by existing scenarios — copy their call shapes verbatim from s11/s8). If `create_channel`'s signature differs (e.g. returns a struct), adapt the `channel` binding. Do NOT change any driver/production code in this task — if a mismatch appears, it belongs to Task 3/4.

- [ ] **Step 2: Build the binary the harness drives.**

Run: `cd src-tauri && cargo build --bin harmony-app 2>&1 | tail -5`
Expected: clean. (The freshness guard in the harness hard-fails if this is stale.)

- [ ] **Step 3: Run the scenario.**

Run: `cd e2e-harness && cargo nextest run --features e2e --test-threads 1 -E 'test(s13_tier2_conviction_setpower)' 2>&1 | tail -30`
Expected: PASS. On failure, re-run with `HARMONY_E2E_KEEP=1` and inspect `e2e-harness/target/e2e-runs/s13-tier2-setpower-*/alice.stderr.log` for the finalize/dispatch trace (`tier2_proposals_finalized`, `apply_auto_exec_set_power`). Likely tuning knobs: raise the window if finalize races ahead of the `ThresholdReached` observation; lower `thresholdMin` if the threshold never crosses; confirm alice is admin (power 100) so the direct-mint path fires.

- [ ] **Step 4: Commit.**

```bash
git add -A && git commit -m "ZEB-720: s13 two-node scenario — Tier-2 SetPower finalize + power change

Spawned alice+bob nodes with short voting cadence finalize a Tier-2 Conviction
SetPower poll; both replicas converge to the target's new materialized power.
Closes ZEB-720.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

## Final verification (before PR)

- [ ] Full CI-parity Rust gate from `src-tauri/`:
```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
All three exit 0 / all tests pass.
- [ ] e2e scenario green (Task 5 Step 3), binary freshly built.
- [ ] `git log --oneline` shows the 5 task commits atop `main@2528630f`; branch pushed; PR opened with `Closes ZEB-720.`

## Notes for the executor

- **Task order is a hard dependency chain for 1→3→4→5** (Task 3's `_impl`s call the Task-1 `extract(&Mutex)`; Task 4 calls Task 3's RPCs; Task 5 uses Task 4). **Task 2 is independent** of 1/3 but must land before Task 5 (the scenario needs the env overrides). Recommended sequence: 1, 2, 3, 4, 5.
- **The tick core is off-limits** — no edits to `run_voting_tick`'s pass logic, the constant's value, or `spawn_voting_tick`'s loop beyond reading `ctx.contestability_window_ms` and the interval arg.
- **Every `VotingTickContext { ... }` literal in the codebase** must add `contestability_window_ms` after Task 2 Step 2 — the compiler enumerates them; use `CONTESTABILITY_WINDOW_MS` for all pre-existing fixtures so their behavior is unchanged.
- **Do not add delegate/undelegate verbs or the quorum>1 AdminProposal path** — explicit non-goals (spec §"Scope boundaries").
