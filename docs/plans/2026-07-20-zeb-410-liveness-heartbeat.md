# ZEB-410 — periodic multi-device liveness heartbeat — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a periodic (node-start + hourly) task that re-signs the local device's `LivenessCert` when it ages past the existing ~15-day threshold, so a device — especially a headless `serve` node that never opens the Devices panel — stays `Full` in siblings' `evaluate_trust`.

**Architecture:** A new `liveness_heartbeat.rs` module with a `run_voting_tick`-style split: `run_liveness_heartbeat_once` (async lock + the existing conditional `refresh_self_liveness`, returns whether it re-signed) and `spawn_liveness_heartbeat` (interval loop that calls it and, on a re-sign, nudges the `owner-trust-v1` `FleetSyncEngine` to replicate + persist). Wired into `start_node_inner` at the voting-tick spawn block, mirroring `voting_tick_handle`'s lifecycle. The device signing key is captured into the task closure only — never stored on `NodeState`.

**Tech Stack:** Rust, tokio (`interval`, `sync::Mutex`, `spawn`), `harmony_owner` (`refresh_self_liveness`, `LivenessCert`, `DEFAULT_FRESHNESS_WINDOW_SECS`), the existing `FleetSyncEngine<OwnerState>` trust-sync engine.

**Design doc:** `docs/specs/2026-07-20-zeb-410-liveness-heartbeat-design.md`.

## Global Constraints

- Cargo commands run from `src-tauri/`.
- Gates (CI-parity, run for the final sweep):
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- Iterative gates: `--lib` / `-p harmony-app` scoped runs or `scripts/test-select --context task`; the full `--workspace --all-targets` sweep is only the final gate (touching `lib.rs` relinks ~97 integration binaries).
- `--all-targets` and `--locked` are load-bearing (CLAUDE.md).
- The `test-fixtures` feature is required for any `--all-targets` / integration compile.
- IPC/camelCase, keychain-hermetic-test rules: N/A here (no new IPC, no identity-persistence test paths — `mint_owner` is the pure `harmony_owner` mint, no keychain/passphrase needed).
- Do **not** change the ~15-day re-sign threshold (`owner_state.rs:877`) or the 30-day freshness window (`harmony-owner/src/trust.rs:5`).

## File Structure

- **Create `src-tauri/src/liveness_heartbeat.rs`** — the whole heartbeat surface: `run_liveness_heartbeat_once` (testable unit), `spawn_liveness_heartbeat` (interval loop), `LIVENESS_HEARTBEAT_INTERVAL` const, a module-local `now_unix_secs()` helper, and inline `#[cfg(test)]` unit tests. Single responsibility: the liveness heartbeat.
- **Modify `src-tauri/src/lib.rs`** — module declaration, one new `NodeState` field + its inits + stop abort, one new `_opt` local, and the spawn block. All in `start_node_inner` / `NodeState`, mirroring the existing `voting_tick_handle` and `owner_trust_doc_opt` wiring.

---

### Task 1: `liveness_heartbeat.rs` module + `run_liveness_heartbeat_once` (TDD)

**Files:**
- Create: `src-tauri/src/liveness_heartbeat.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod liveness_heartbeat;`)
- Test: inline `#[cfg(test)] mod tests` in `src-tauri/src/liveness_heartbeat.rs`

**Interfaces:**
- Consumes: `crate::owner_state::refresh_self_liveness(state: &mut harmony_owner::state::OwnerState, device_sk: &ed25519_dalek::SigningKey, now: u64) -> bool`; `crate::fleet_sync::FleetSyncEngine<S>::notify_dirty(&self)`; `harmony_owner::lifecycle::{mint_owner, MintResult}` (tests); `harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS` (tests).
- Produces:
  - `pub async fn run_liveness_heartbeat_once(doc: &Arc<tokio::sync::Mutex<OwnerState>>, device_sk: &ed25519_dalek::SigningKey, now_secs: u64) -> bool`
  - `pub fn spawn_liveness_heartbeat(doc: Arc<tokio::sync::Mutex<OwnerState>>, engine: Arc<FleetSyncEngine<OwnerState>>, device_sk: Arc<ed25519_dalek::SigningKey>, interval: Duration) -> tokio::task::JoinHandle<()>`
  - `pub const LIVENESS_HEARTBEAT_INTERVAL: Duration`

- [ ] **Step 1: Write the failing unit tests**

Create `src-tauri/src/liveness_heartbeat.rs` with only the test module first (the `use super::*;` will fail to resolve until Step 3, which is the intended red state):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use harmony_owner::lifecycle::{mint_owner, MintResult};
    use std::sync::Arc;

    // run_liveness_heartbeat_once faithfully reflects refresh_self_liveness's
    // conditional (~15-day) gate through the async lock: a just-written cert is
    // fresh (no re-sign), and a cert aged past freshness/2 re-signs then is fresh
    // again. The fresh/missing/stale *decision* is already covered by
    // owner_state.rs:1739/1765/1779; these pin the async wrapper + return contract.

    #[tokio::test]
    async fn heartbeat_once_noop_when_fresh() {
        let now = 1_700_000_333;
        let MintResult {
            state,
            device_signing_key,
            ..
        } = mint_owner(now).unwrap();
        let doc = Arc::new(tokio::sync::Mutex::new(state));
        // Guarantee a fresh cert exists at `now` (idempotent regardless of mint).
        let _ = run_liveness_heartbeat_once(&doc, &device_signing_key, now).await;
        // A just-written cert is fresh → no re-sign.
        assert!(
            !run_liveness_heartbeat_once(&doc, &device_signing_key, now).await,
            "a fresh cert must not be re-signed"
        );
    }

    #[tokio::test]
    async fn heartbeat_once_resigns_when_stale() {
        let t0 = 1_700_000_000;
        let MintResult {
            state,
            device_signing_key,
            ..
        } = mint_owner(t0).unwrap();
        let doc = Arc::new(tokio::sync::Mutex::new(state));
        let _ = run_liveness_heartbeat_once(&doc, &device_signing_key, t0).await;
        assert!(
            !run_liveness_heartbeat_once(&doc, &device_signing_key, t0).await,
            "cert is fresh at t0"
        );
        // Advance past the refresh threshold (freshness / 2).
        let later = t0 + harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS / 2 + 1;
        assert!(
            run_liveness_heartbeat_once(&doc, &device_signing_key, later).await,
            "a stale cert must be re-signed"
        );
        assert!(
            !run_liveness_heartbeat_once(&doc, &device_signing_key, later).await,
            "the re-signed cert is fresh again at `later`"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail (red)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(heartbeat_once)'`
Expected: FAIL to compile — `cannot find function run_liveness_heartbeat_once` (the production fns don't exist yet).

- [ ] **Step 3: Write the minimal implementation**

Prepend the production code above the test module in `src-tauri/src/liveness_heartbeat.rs`:

```rust
//! ZEB-410 — periodic multi-device liveness heartbeat.
//!
//! Re-signs the local device's `LivenessCert` on a timer (node-start + hourly)
//! so a device — especially a headless `serve` node that never opens the Devices
//! panel — stays `Full` in siblings' `evaluate_trust`. Reuses the conditional
//! `refresh_self_liveness` (~15-day re-sign gate) and the ZEB-668 S1 trust-sync
//! propagation path (`FleetSyncEngine::notify_dirty` → debounced sibling sync +
//! persist). The `run_once`/`spawn` split mirrors `community_voting_tick`.

use std::sync::Arc;
use std::time::Duration;

use harmony_owner::state::OwnerState;

use crate::fleet_sync::FleetSyncEngine;
use crate::owner_state::refresh_self_liveness;

/// Heartbeat check cadence. The first `interval.tick()` fires immediately, so
/// this is also the node-start refresh. Almost every tick is a cheap no-op —
/// `refresh_self_liveness` only re-signs when the cert has aged past ~15 days —
/// so an actual re-sign + replicate fires only ~once per fortnight per device.
/// 1 h matches the existing `reachability_publisher::IDLE_REFRESH_INTERVAL`.
pub const LIVENESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60 * 60);

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One heartbeat iteration: lock the resident trust doc and run the existing
/// conditional self-liveness refresh. Returns whether it re-signed (so the
/// caller can `notify_dirty()` + log). Engine-free by design — mirrors the panel
/// path's `refresh -> if refreshed notify_dirty` split (`owner_commands.rs:729`/
/// `734`), keeping this unit trivially testable with just a doc + key.
pub async fn run_liveness_heartbeat_once(
    doc: &Arc<tokio::sync::Mutex<OwnerState>>,
    device_sk: &ed25519_dalek::SigningKey,
    now_secs: u64,
) -> bool {
    let mut g = doc.lock().await;
    refresh_self_liveness(&mut g, device_sk, now_secs)
}

/// Spawn the interval loop. On the rare tick that actually re-signs, nudge the
/// `owner-trust-v1` engine so the fresh cert replicates to siblings and persists
/// (the same path the on-panel-load refresh uses). The task runs until aborted
/// via its `JoinHandle` on node stop.
pub fn spawn_liveness_heartbeat(
    doc: Arc<tokio::sync::Mutex<OwnerState>>,
    engine: Arc<FleetSyncEngine<OwnerState>>,
    device_sk: Arc<ed25519_dalek::SigningKey>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if run_liveness_heartbeat_once(&doc, &device_sk, now_unix_secs()).await {
                engine.notify_dirty();
                tracing::info!(
                    target: "harmony_liveness",
                    "self-liveness heartbeat re-signed + queued for sibling sync"
                );
            }
        }
    })
}
```

- [ ] **Step 4: Add the module declaration to `lib.rs`**

In `src-tauri/src/lib.rs`, next to the other `pub mod` lines (near `pub mod community_voting_tick;` ~L158), add:

```rust
pub mod liveness_heartbeat;
```

- [ ] **Step 5: Run the tests to verify they pass (green)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(heartbeat_once)'`
Expected: PASS — `heartbeat_once_noop_when_fresh` and `heartbeat_once_resigns_when_stale` both pass.

- [ ] **Step 6: Lint the new module**

Run: `cd src-tauri && cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings`
Expected: clean (exit 0). Note: `--lib` clippy misses inline-test doc lints; the `--all-targets` sweep in Task 2 Step 8 is the backstop.
Run: `cd src-tauri && cargo fmt --all -- --check`
Expected: clean (exit 0).

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/liveness_heartbeat.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-410): liveness heartbeat module (run_once + spawn)

<trailers>"
```

(The `lib.rs` change in this commit is only the `pub mod liveness_heartbeat;` line.)

---

### Task 2: Wire the heartbeat into `start_node_inner` + `NodeState`

> **As-built correction (2026-07-20):** Steps 4-6 below originally targeted the late
> voting-tick spawn block (`~12843`), reading the `_opt` locals there. The compiler
> rejected it — those locals are out of scope at that block (which is why voting-tick
> reads `voting_logs` from `guard`). The spawn was moved **into the owner-trust
> guard-store block** (`~11726`, right after
> `guard.owner_trust_sync = owner_trust_sync_engine_opt.clone();`), where the three
> `_opt` locals are in scope and `guard` is held. Spawn + slot-store live in that one
> block, so a lock-poison rollback skips both together (no leak) — no cleanup-tuple
> threading, no signing key on `NodeState`. The field/init/abort mirrors of
> `voting_tick_handle` (Steps 1-3) are unchanged.

**Files:**
- Modify: `src-tauri/src/lib.rs` (`NodeState` field + inits + `stop_inner` abort; new `_opt` local; spawn block)

**Interfaces:**
- Consumes: `crate::liveness_heartbeat::{spawn_liveness_heartbeat, LIVENESS_HEARTBEAT_INTERVAL}` (Task 1); existing locals `owner_trust_doc_opt`, `owner_trust_sync_engine_opt` (`lib.rs:4181`/`6120-6121`); `loaded.device_signing_key` (in scope at `~6121`); the voting-tick spawn block (`~12843-12885`) as the mirror.
- Produces: `NodeState.liveness_heartbeat_handle` + a running heartbeat task per node start.

All edits mirror the existing `voting_tick_handle` (an `Arc<Mutex<Option<JoinHandle>>>` slot spawned late under the generation gate) and `owner_trust_doc_opt` (a function-body `_opt` local). Verify each mirror site with grep before editing, since exact line numbers drift.

- [ ] **Step 1: Add the `NodeState` field**

In `src-tauri/src/lib.rs`, next to `pub voting_tick_handle: ...` (grep `voting_tick_handle:` → field decl ~L1183), add:

```rust
    /// ZEB-410: JoinHandle for the periodic self-liveness heartbeat task
    /// (spawned in start_node_inner when a resident trust engine exists;
    /// aborted in stop_inner). Slot type mirrors `voting_tick_handle`.
    pub liveness_heartbeat_handle: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
```

- [ ] **Step 2: Initialize the field in both `NodeState` literals**

Grep `voting_tick_handle: std::sync::Arc::new(std::sync::Mutex::new(None))` → two sites (~L1914 and ~L70331). After each, add:

```rust
            liveness_heartbeat_handle: std::sync::Arc::new(std::sync::Mutex::new(None)),
```

- [ ] **Step 3: Abort the handle in `stop_inner`**

Grep for the `voting_tick_handle` abort in `stop_inner` (~L2466 — `if let Ok(mut slot) = guard.voting_tick_handle.lock()`). Immediately after that block, add the mirror — recovering a poisoned slot (`into_inner`) so shutdown can still take + abort the stored handle rather than leaking the task:

```rust
            {
                let mut slot = guard
                    .liveness_heartbeat_handle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(handle) = slot.take() {
                    handle.abort();
                }
            }
```

- [ ] **Step 4: Declare the signing-key `_opt` local**

Grep `let mut owner_trust_doc_opt` (~L4181). Immediately after the `owner_trust_doc_opt` / `owner_trust_sync_engine_opt` declarations, add:

```rust
        // ZEB-410: device signing key for the liveness heartbeat, captured here
        // so it reaches the spawn block without living on NodeState. Set at the
        // owner-trust engine block below (where `loaded` is in scope).
        let mut heartbeat_device_sk_opt: Option<std::sync::Arc<ed25519_dalek::SigningKey>> = None;
```

- [ ] **Step 5: Populate the `_opt` at the owner-trust engine block**

Grep `owner_trust_sync_engine_opt = Some(std::sync::Arc::clone(&owner_trust_sync));` (~L6121). Immediately after it (where `loaded.device_signing_key`, `owner_trust_doc`, and `owner_trust_sync` are all in scope), add:

```rust
                    heartbeat_device_sk_opt =
                        Some(std::sync::Arc::new(loaded.device_signing_key.clone()));
```

- [ ] **Step 6: Add the spawn + slot-store inside the owner-trust guard-store block**

> **As-built:** the spawn lives **inside** the owner-trust guard-store block (right
> after `guard.owner_trust_sync = owner_trust_sync_engine_opt.clone();`), NOT at the
> later voting-tick block — the `_opt` locals are out of scope there. Because the
> spawn and the slot-store happen while `guard` is already held, they are skipped
> together on a lock-poison rollback (no untracked task) and need no separate
> generation re-lock. Grep for `guard.owner_trust_sync = owner_trust_sync_engine_opt.clone();`
> and insert immediately after it:

```rust
                        // ZEB-410: spawn the periodic self-liveness heartbeat here,
                        // where the three inputs (trust doc + engine, device signing
                        // key) are in scope and `guard` is held. Reuses
                        // refresh_self_liveness (~15d gate) + the owner-trust-v1 sync
                        // path (notify_dirty). Spawn + slot-store live in this one
                        // guard block, so a lock-poison rollback skips both together
                        // (no leak) and stop_inner aborts via the NodeState slot. The
                        // device signing key lives only in the task closure — never on
                        // NodeState. Only spawns when a resident trust engine + owner
                        // identity exist (all three _opt are Some).
                        if let (Some(hb_doc), Some(hb_engine), Some(hb_device_sk)) = (
                            owner_trust_doc_opt.clone(),
                            owner_trust_sync_engine_opt.clone(),
                            heartbeat_device_sk_opt.clone(),
                        ) {
                            let handle = crate::liveness_heartbeat::spawn_liveness_heartbeat(
                                hb_doc,
                                hb_engine,
                                hb_device_sk,
                                crate::liveness_heartbeat::LIVENESS_HEARTBEAT_INTERVAL,
                            );
                            // Recover a poisoned slot (into_inner) so a failed lock
                            // can't leave the just-spawned task untracked (leaked).
                            let mut slot = guard
                                .liveness_heartbeat_handle
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if let Some(old) = slot.replace(handle) {
                                old.abort();
                            }
                        }
```

The matching `stop_inner` abort (Step 3) likewise recovers a poisoned slot with
`.lock().unwrap_or_else(|e| e.into_inner())` before `take()`.

- [ ] **Step 7: Compile-check the wiring**

Run: `cd src-tauri && cargo check --locked --lib --features test-fixtures`
Expected: clean (exit 0). The `_opt` locals are in scope at the guard-store block
(they are consumed there by the existing `guard.owner_trust_* = ..._opt.clone()`
lines), so the spawn compiles. (An earlier draft placed the spawn at the voting-tick
block and failed with `not found in this scope` — the guard-store block is the
correct home.)

- [ ] **Step 8: Full CI-parity gate sweep**

Run each and confirm exit 0 (read the actual Summary line — piped exit codes lie):

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: fmt clean; clippy clean (this `--all-targets` run is the backstop for the inline-test doc lints `--lib` missed); nextest all green including the two new `heartbeat_once_*` tests. This is the ~97-binary relink — budget ~10-13 min warm.

- [ ] **Step 9: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-410): spawn liveness heartbeat in start_node_inner

<trailers>"
```

---

## Self-Review

**Spec coverage:**
- Periodic re-publish (node-start + hourly): Task 1 (`spawn_liveness_heartbeat` interval, immediate first tick) + Task 2 (wiring). ✓
- Reuse existing conditional refresh + propagation: Task 1 (`run_once` → `refresh_self_liveness`; `notify_dirty` in the spawn loop). ✓
- GUI + headless one code path, no two-seam: Task 2 spawn block reads `_opt` locals, no `NodeState`-whole closure. ✓
- Key off `NodeState`: Task 2 Steps 4-6 (key in `_opt` local → task closure). ✓
- Handle lifecycle (spawn/stop): Task 2 Steps 1-3, 6 (mirror `voting_tick_handle`). ✓
- No new event/UI/config, thresholds unchanged: enforced by scope; only additive code. ✓
- Tests: Task 1 Steps 1-5 (fresh no-op, stale re-sign through the async wrapper). ✓

**Placeholder scan:** none — every step has concrete code or an exact command.

**Type consistency:** `OwnerState` = `harmony_owner::state::OwnerState` throughout (matches `NodeState.owner_trust_doc` / `owner_trust_sync`); `FleetSyncEngine` = `crate::fleet_sync::FleetSyncEngine`; `run_liveness_heartbeat_once` / `spawn_liveness_heartbeat` / `LIVENESS_HEARTBEAT_INTERVAL` names identical in Task 1 (definition) and Task 2 (call site); handle slot type `Arc<Mutex<Option<JoinHandle<()>>>>` identical to `voting_tick_handle`.
