# ZEB-720: Headless Tier-2 Conviction voting + spawned two-node e2e scenario

**Ticket:** ZEB-720 (child of ZEB-451, the agent-driven dev/testing ecosystem epic)
**Author:** Koya
**Date:** 2026-07-22
**Status:** Design approved — ready for implementation plan
**Branch:** `zeb-720-e2e-tier2-voting-scenario` (off `main@2528630f`)

## Goal

Ship a `--features e2e` two-node scenario that **finalizes a Tier-2 Conviction `SetPower` poll and asserts the target member's *materialized* power changes on both replicas**, over real transport, driven entirely through the headless `serve` HTTP/RPC surface.

This closes ZEB-719 **acceptance criterion #2**, which was deliberately deferred (see `docs/specs/2026-07-20-zeb-719-headless-tier2-auto-exec-design.md`, "Scope note"). ZEB-719 shipped the production auto-exec *dispatch* (the tick now dispatches Tier-2 SetPower through `apply_auto_exec_set_power` in headless mode via the `owned_state` seam + `build_auto_exec_fn` selector). ZEB-720 adds the transport-realism proof — and, as a prerequisite the ticket text understated, the headless **operability** of the Tier-2 voting surface itself.

## Context: why this is bigger than "add a test"

The e2e-harness drives a spawned node **only** over `POST http://127.0.0.1:<port>/v1/rpc/{command}` (bearer-auth). To create a proposal and cast a signal from a spawned node, those operations must be reachable over that RPC surface. They are not today, for two independent reasons discovered during recon and **verified against source**:

1. **Tier-2 voting mutations are hard-bound to the GUI `AppHandle`.** Every Tier-2 mutation IPC (`voting_create_tier2_proposal`, `voting_signal_tier2`, `voting_delegate_tier2`, `voting_undelegate_tier2`, `voting_contest_tier2_finalization`) routes through `VotingEngineNodeHandles::extract` (`src-tauri/src/lib.rs:50029-50062`), whose struct field `app_handle_wry: tauri::AppHandle<tauri::Wry>` is **non-optional** (`:50018`) and whose extractor does `app_handle_wry: g.app_handle_wry.clone().ok_or("app_handle_wry missing")?` (`:50058`). In `serve` mode `NodeState.app_handle_wry` is `None` (a deliberate ZEB-445 scoping decision — "voting IPCs stay Tauri-bound", `lib.rs:11434-11439`). So every Tier-2 mutation **fails closed** in a headless node, regardless of any RPC-registration work.

2. **None of the Tier-2 verbs are registered in the curated `/v1/rpc` surface.** `build_registry()` in `src-tauri/src/api/rpc.rs` has zero `voting_*` entries (confirmed by the allowlist drift-guard test `registry_has_exactly_the_curated_v1_surface`, `rpc.rs:2125-2262`).

The engine layer *itself* already tolerates headless operation: `VotingLogEngine<R>.app_handle` is `Option<AppHandle<R>>`, its Tier-3 lifecycle emits and the one Tier-2 "delegate-on-behalf" emit are all `if let Some(app)`-gated, and dozens of the engine's own unit fixtures construct it with `app_handle: None`. The GUI-coupling is confined to the *handle-extraction* layer, not the engine. So the decouple is contained.

This is the same established **"Headless api: add X RPC surface for the e2e harness"** pattern already executed for vines (ZEB-552, ZEB-562), profile cards (ZEB-464), moderation verbs (ZEB-527), and identity reads (ZEB-520) — all Done. Voting is simply the verb family that hasn't been done yet. Per the packaging decision (see "Decisions"), the enablement + surface + driver + scenario land as **one PR** under ZEB-720.

### Finalization is already deterministically drivable

Tier-2 finalization is tick-driven. `run_voting_tick(ctx, now_ms: i128)` (`src-tauri/src/community_voting_tick.rs:114`) takes an **explicit, injectable `now_ms`** and performs **zero internal `SystemTime::now()` reads**; the 24h contestability window is pure arithmetic — `now_ms - uncontested_since >= CONTESTABILITY_WINDOW_MS` at `:271` and `:306`. The **only** wall-clock reader is `spawn_voting_tick` (`:522-550`, `SystemTime::now()` at `:535`), which wraps the tick in a `tokio::time::interval(interval)` loop and feeds it the real clock. So finalization cadence is entirely a function of (a) the tick interval and (b) the window constant — both currently hardcoded, both trivially made env-overridable **without touching the tick core**.

## Decisions (settled with Jake, 2026-07-22)

1. **Packaging: one PR.** ZEB-720 absorbs the decouple + RPC surface + driver verbs + scenario. Cohesive — the scenario is the acceptance test for the new headless-voting surface — and mirrors the ZEB-464 precedent (which bundled decouple + RPC + driver). No separate surface ticket.
2. **Finalize-on-demand mechanism: env-overridable window (no force-finalize RPC).** `spawn_voting_tick`/`start_node_inner` read optional env overrides for the contestability window and tick interval, both defaulting to today's production values. The spawned test sets a short window + short interval so the *real* production tick loop finalizes naturally in a few seconds. Rejected the alternative `voting_run_tick_at{now_ms}` RPC because it would introduce a "force-finalize any poll at any time" capability requiring careful gating; the env override cannot force a *specific* poll — it only shortens the window uniformly on a node whose operator sets the env, which no production deployment does.

## Architecture — five components, in dependency order

### Component 1 — Decouple voting-engine handle extraction from the GUI `AppHandle` (production)

**Files:** `src-tauri/src/lib.rs` (`VotingEngineNodeHandles` struct + `extract` + `ensure_engine`/`ensure_voting_engine_for`, ~`:49769-50062`).

- `VotingEngineNodeHandles.app_handle_wry`: `tauri::AppHandle<tauri::Wry>` → `Option<tauri::AppHandle<tauri::Wry>>`.
- `extract`: change signature from `fn extract(state_lock: &tauri::State<'_, Mutex<NodeState>>)` to `fn extract(state_lock: &std::sync::Mutex<NodeState>)`. A `tauri::State<'_, Mutex<NodeState>>` derefs to `&Mutex<NodeState>`, so every existing GUI caller passes `state_lock.inner()` (or `&*state_lock`) with no behavior change. Replace `.ok_or("app_handle_wry missing")?` with a plain `.clone()` (now `Option`).
- Thread the now-`Option<AppHandle<Wry>>` through `ensure_engine`/`ensure_voting_engine_for` down to the `VotingLogEngine` constructor, which already accepts `Option<AppHandle<R>>`. Every consumer of `app_handle_wry` inside these methods must already be `if let Some(app)`-shaped or become so; the recon confirmed the only Tier-2-relevant emit that reaches the engine (`voting-delegate-signaled-on-your-behalf`) is already `Option`-gated.

**Net behavioral change:** GUI path byte-identical (an always-`Some` `AppHandle`). Headless path: Tier-2 voting handle extraction succeeds instead of erroring `"app_handle_wry missing"`. This is the change that reverses ZEB-445's "voting stays Tauri-bound" note; the note's comment sites (`lib.rs:11434-11439`, and the `VotingEngineNodeHandles` doc) get updated to reflect headless support.

**Invariant preserved:** headless nodes still emit *no* Tauri events (there is no window); events that a GUI would surface simply no-op in `serve`, exactly as they do for every other already-headless command. Any Tier-2 event a *test* needs to observe is observed through state reads (`voting_get_tier2_proposal`, `list_community_members`), not the Tauri emit path.

### Component 2 — Finalization-cadence env overrides (production; test-only in practice)

**Files:** `src-tauri/src/community_voting_tick.rs` (window constant + `VotingTickContext` + `spawn_voting_tick`); `src-tauri/src/lib.rs` (`start_node_inner` tick spawn, ~`:12747-12777`).

- **Contestability window:** add a `contestability_window_ms: i128` field to `VotingTickContext`, defaulting to `CONTESTABILITY_WINDOW_MS` (unchanged `pub const` = `86_400_000`). The two finalize checks (`community_voting_tick.rs:271`, `:306`) read `ctx.contestability_window_ms` instead of the bare constant. The field is populated where the context is built (`start_node_inner`) from `std::env::var("HARMONY_VOTING_CONTESTABILITY_WINDOW_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(CONTESTABILITY_WINDOW_MS)`.
  - *Why a context field, not a raw env read inside the tick:* keeps `run_voting_tick` a pure function of its inputs (no hidden env reads), preserves testability, and matches how the tick already receives all its config. Env is read exactly once, at node bringup.
- **Tick interval:** `start_node_inner` computes the `spawn_voting_tick` interval from `std::env::var("HARMONY_VOTING_TICK_INTERVAL_MS").ok().and_then(|s| s.parse().ok()).map(Duration::from_millis).unwrap_or(DEFAULT_TICK_INTERVAL)`. (Planning will confirm whether an override seam already exists; if so, reuse it.)
- Both envs are documented (in code comments and this spec) as **test/diagnostic-only**; production defaults are identical to today. A unit test pins the default fallback (missing/unparseable env → production constant).

### Component 3 — Tier-2 RPC surface (production)

**Files:** `src-tauri/src/lib.rs` (extract `_impl` seams for the mutation/read commands); `src-tauri/src/api/rpc.rs` (arg structs + `rpc!` registrations + allowlist-test update).

Expose exactly the verbs the scenario needs:

| RPC command | Kind | Notes |
|---|---|---|
| `voting_create_tier2_proposal` | mutation | full arg set; `auto_exec` carries `SetPower{target, new_power}` |
| `voting_signal_tier2` | mutation | `{proposal_id, support}` |
| `voting_get_tier2_proposal` | read | observe `Open→ThresholdReached→Finalized` lifecycle |

Reading a member's power reuses the **already-registered** `list_community_members` (returns `MemberInfoDto.power`, `lib.rs:26591-26678`) — no new read verb.

Each command follows the canonical dual-wire pattern (as `create_community`/`create_community_impl`, `lib.rs:32021-32038`):
1. Extract `pub(crate) async fn <name>_impl(state: &Mutex<NodeState>, sink: Arc<dyn NodeEventSink>, ...) -> Result<_, String>` holding all logic; swap `app.emit(...)` → `sink.emit(...)`/`emit_ser(...)`.
2. Thin `#[tauri::command]` wrapper wraps `AppHandle` into `Arc<dyn NodeEventSink>` and calls the `_impl` (GUI path unchanged).
3. `rpc!(m, "<name>", <Name>Args, |state, sink, a| async move { crate::<name>_impl(state, sink, ...).await })` in `build_registry()`.
4. Add the three names to the `expected` vector in `registry_has_exactly_the_curated_v1_surface` (`rpc.rs:2125-2262`) — the drift guard fails by design otherwise.

Arg structs use `#[serde(rename_all = "camelCase")]` (matching the JS/driver camelCase convention).

### Component 4 — e2e-harness driver verbs (test)

**File:** `e2e-harness/src/driver.rs`.

Thin wrappers over the new RPCs, shaped like the existing admin-recovery block (`driver.rs:1009-1181`):
- `create_tier2_setpower_proposal(node, community_id, channel_id, proposal_text, target_addr, new_power, threshold_min) -> poll_id`
- `signal_tier2(node, proposal_id, support)`
- `get_tier2_proposal(node, proposal_id) -> Value` (raw DTO for lifecycle assertions)
- `member_power(node, community_id, addr) -> Option<u8>` — pulls `.power` out of the existing `list_community_members` response.

### Component 5 — The scenario (test)

**File:** `e2e-harness/tests/e2e_two_node.rs` — new `s13_tier2_conviction_setpower` (behind the file-level `#![cfg(feature = "e2e")]`).

Cloned from the S11/S12 admin-recovery skeleton (`:2473-2845`):
1. Spawn alice + bob nodes **with the finalization-cadence envs set** (e.g. `HARMONY_VOTING_CONTESTABILITY_WINDOW_MS=2000`, `HARMONY_VOTING_TICK_INTERVAL_MS=250`) via `NodeConfig`'s env-injection.
2. alice creates a community (`admin_quorum == 1`, the default — keeps finalize on the single-tick **direct-mint** path, not the ZEB-300 AdminProposal route); bob redeems an invite; `poll_until` both rosters converge (both see 2 members).
3. Record bob's initial materialized power.
4. alice creates a Tier-2 `SetPower{target: bob, new_power: 60}` proposal with a low `threshold_min` so a couple of support signals cross the threshold quickly. (60 is a non-admin level, unambiguously off the admin-affecting/AdminProposal branch.)
5. alice (and optionally bob) signal support.
6. `poll_until` (generous timeout, e.g. 60-120 s) on **both replicas'** `member_power(bob) == 60`. alice's node finalizes + direct-mints the `SetPower` once its short window elapses; bob's node converges via membership-log sync.
7. Optionally assert the proposal DTO reaches `Finalized` on alice via `get_tier2_proposal` as a lifecycle checkpoint.

Determinism: the window (≈2 s) is comfortably larger than the tick interval (≈250 ms) so the poll is observed `ThresholdReached` before it finalizes; `poll_until` absorbs all tick jitter. No `sleep`, no real 24h wait, no shortened *production-default* constant.

## Scope boundaries (YAGNI)

**In scope:** Components 1-5 above; the quorum=1 direct-mint finalize path.

**Explicit non-goals / stretch (deferrable to a follow-up):**
- `voting_delegate_tier2` / `voting_undelegate_tier2` RPC verbs and a delegation sub-scenario. The decouple (Component 1) makes them trivial to add later, but the acceptance criterion doesn't need them.
- The quorum>1 **AdminProposal-routed** SetPower path (ZEB-300). Independently testable; adds multi-admin-tick convergence complexity not needed for "target's power changes."
- Tier-1 Approval / Tier-3 Sortition RPC surface.

If Component 1-5 land cleanly with budget to spare, a delegation sub-case is the first stretch to add.

## Global constraints

- **CI gates (run from `src-tauri/`):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. The `e2e-harness` crate is **not** in the workspace CI gate — its `--features e2e` suite is run manually (build `harmony-app` first).
- **Tauri IPC naming:** Rust params `snake_case`; JSON/driver args `camelCase` (`#[serde(rename_all = "camelCase")]`).
- **`app.emit` → `sink.emit` in every extracted `_impl`;** production must never regress GUI event delivery.
- **No new deterministic-nonce exposure;** this work does not touch crypto helpers.
- **Env overrides default to today's constants** — a build with no envs set behaves exactly as `main` does.
- **Do not add anti-backdating / wall-clock-sleep logic** to the tick (per standing lessons); finalization stays a pure function of injected/config time.

## File-by-file change map

| File | Change |
|---|---|
| `src-tauri/src/lib.rs` | `VotingEngineNodeHandles.app_handle_wry` → `Option`; `extract` takes `&Mutex<NodeState>`, drops `.ok_or`; thread `Option<AppHandle>` through `ensure_engine`/`ensure_voting_engine_for`; extract `_impl` seams for `voting_create_tier2_proposal`/`voting_signal_tier2`/`voting_get_tier2_proposal` (swap `app.emit`→`sink.emit`); tick-spawn reads `HARMONY_VOTING_TICK_INTERVAL_MS`; build `VotingTickContext.contestability_window_ms` from env; update ZEB-445 comment sites |
| `src-tauri/src/community_voting_tick.rs` | add `contestability_window_ms` field to `VotingTickContext`; read it at the two finalize checks (`:271`, `:306`) instead of the bare const (const stays as the default) |
| `src-tauri/src/api/rpc.rs` | arg structs + `rpc!` registrations for the 3 verbs; add names to `registry_has_exactly_the_curated_v1_surface` allowlist |
| `e2e-harness/src/driver.rs` | 4 driver wrappers (`create_tier2_setpower_proposal`, `signal_tier2`, `get_tier2_proposal`, `member_power`) |
| `e2e-harness/tests/e2e_two_node.rs` | `s13_tier2_conviction_setpower` scenario |
| `docs/specs/2026-07-22-zeb-720-headless-tier2-e2e-design.md` | this doc |

## Testing strategy

- **In-crate unit tests (CI-gated):** headless `extract` succeeds with `app_handle_wry: None` (no longer errors); each new `_impl` round-trips against a suitably-wired fixture at whatever layer the existing voting command tests use; `contestability_window_ms` env fallback returns the production default on missing/garbage input; tick finalize honors a short `ctx.contestability_window_ms`.
- **Registry drift guard (CI-gated):** the allowlist test enforces the three new commands are present and spelled correctly.
- **e2e scenario (manual, `--features e2e`):** `s13` is the end-to-end proof. Not in CI, per harness convention; run locally on Koya with `cargo build --bin harmony-app` first, then `cd e2e-harness && cargo nextest run --features e2e --test-threads 1 -E 'test(s13_tier2_conviction_setpower)'`.

## Risks & mitigations

- **Decouple ripples wider than expected** (some `ensure_engine` consumer needs the `AppHandle` unconditionally). *Mitigation:* recon confirmed engine tolerates `None`; planning will `cargo check` the decouple in isolation before wiring RPCs, enumerating call sites via the compiler.
- **Threshold never crosses / crosses trivially with zero votes** (conviction is charge×time). *Mitigation:* pick a small positive `threshold_min` and cast real support signals; the scenario asserts the *observed* `ThresholdReached` before finalize, so a mis-tuned threshold fails loudly rather than flakily.
- **e2e flakiness from cadence race** (finalize before `ThresholdReached` observed). *Mitigation:* window ≫ tick interval; `poll_until` on both the lifecycle checkpoint and the power change.
- **Building `harmony-app` with the envs unset in other e2e scenarios** must be unaffected. *Mitigation:* envs are per-`NodeConfig`; only `s13` sets them; defaults reproduce `main`.

## References

- ZEB-719 — production auto-exec dispatch + `build_auto_exec_fn`/`owned_state` seam (PR #507, `dad0e64e`); spec `docs/specs/2026-07-20-zeb-719-headless-tier2-auto-exec-design.md`
- ZEB-291 — Tier-2 Conviction voting backend
- ZEB-300 — AdminProposal routing at quorum > 1 (the stretch path)
- ZEB-445 — headless control surface (the "voting stays Tauri-bound" decision this reverses)
- ZEB-464 / ZEB-552 / ZEB-527 — precedent "headless api: add X RPC surface" PRs
- ZEB-447 — two-agent scripted E2E scenario-suite pattern
- Precedent scenario shape: `e2e-harness/tests/e2e_two_node.rs` S11/S12 (admin-recovery)

## Converge refinements (2026-07-22, post-review)

Three robustness refinements from the PR #526 review, on top of the design above:

- **Driver `create_tier2_setpower_proposal` gained a `threshold_max` param** (raw ms band, alongside `threshold_min`). The e2e sets a tiny band (`min=1, max=10`) so a single admin signal crosses the participation-shaped dynamic threshold in <1s — Tier-2 conviction is a real-time integral, so without a low band a finalize would take days. The RPC already accepted `thresholdMax`.
- **Partial `SetPower` args are rejected.** The `voting_create_tier2_proposal` RPC handler now errors when exactly one of `setPowerTarget` / `setPowerNewPower` is supplied (only both-present or both-absent are valid), instead of silently minting a proposal whose finalize changes no power.
- **The two cadence env overrides are validated strictly positive** via a shared, unit-tested `parse_positive_ms(Option<String>, default)` helper (community_voting_tick.rs). Absent / unparseable / non-positive input falls back to the production constant. The positive filter is load-bearing: a `0` `HARMONY_VOTING_TICK_INTERVAL_MS` would panic `tokio::time::interval` ("period must be non-zero").
