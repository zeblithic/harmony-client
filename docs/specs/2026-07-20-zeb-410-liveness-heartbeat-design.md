# ZEB-410 — periodic multi-device liveness heartbeat (design)

**Status:** approved 2026-07-20 (Koya + Jake). Branch `zeb-410-multi-device-liveness-heartbeat`.

## Problem

`evaluate_trust` (`harmony-owner/src/trust.rs:23`) refuses an owner's trust state
(`Refused(StaleTrustState)`) when no **active** device has a `LivenessCert` (or
vouch) newer than `DEFAULT_FRESHNESS_WINDOW_SECS` = **30 days**
(`harmony-owner/src/trust.rs:5`). ZEB-342 made the local device re-publish its own
`LivenessCert` via `refresh_self_liveness`, but that only fires **on Devices-panel
load** (a `get_owner_state` IPC call). A device that never opens its panel within
the 30-day window — most importantly a **headless `serve` node**, which has no
panel at all — lets its self-liveness cert age out, and siblings' `evaluate_trust`
for it degrades toward `Refused(StaleTrustState)`.

The gap is purely a **trigger cadence** gap. Everything else already exists:

- **The conditional refresh** — `refresh_self_liveness(state, device_sk, now)`
  (`src-tauri/src/owner_state.rs:873`) is already idempotent / refresh-if-stale: it
  re-signs only when the local device has no cert or its cert is older than
  `DEFAULT_FRESHNESS_WINDOW_SECS / 2` (~15 days), and returns `true` **iff** it
  wrote a new cert. It does **not** persist or replicate — that is the caller's job.
- **The propagation path** — ZEB-668 S1 (PR #451). A local trust mutation followed
  by `FleetSyncEngine::notify_dirty()` on the `owner-trust-v1` engine
  (`src-tauri/src/owner_trust_sync.rs`) debounces (250 ms) and publishes over Zenoh
  to sibling devices, which merge it into their resident trust doc. Liveness-only
  merges do **not** change the device-set fingerprint, so this replicates silently
  (no `owner-devices-updated` churn — pinned by the test at
  `owner_trust_sync.rs:696`).

So the only missing piece is a periodic task that performs the existing
resident-branch recipe (`get_owner_state` at `owner_commands.rs:729`/`734`) on a
timer instead of only on IPC:

```rust
let refreshed = refresh_self_liveness(&mut g, &device_sk, now_unix());
if refreshed { engine.notify_dirty(); }
```

## Goal

On a running node (GUI **or** headless `serve`), periodically re-check the local
device's self-liveness and, when it has aged past the existing ~15-day threshold,
re-sign it and let the existing trust-sync engine replicate it to siblings — so a
device stays `Full` in siblings' `evaluate_trust` without anyone opening its
Devices panel.

## Non-goals (YAGNI)

- No new event/cert type. Reuse `LivenessCert` + `refresh_self_liveness` verbatim.
- No UI change. Per-device last-seen already shipped (ZEB-668 S4, PR #454).
- No new config surface, and **no change** to the ~15-day re-sign threshold
  (`owner_state.rs:877`) or the 30-day freshness window (`trust.rs:5`).
- No change to the on-panel-load refresh — it stays as the "show me fresh right
  now" path; the heartbeat is the background path. They compose.
- No vouching heartbeat — `evaluate_trust` freshness is satisfied by liveness
  **or** vouch; this ticket covers self-liveness only.

## Design

Mirror the established `run_voting_tick` / `spawn_voting_tick` split
(`src-tauri/src/community_voting_tick.rs`): an async, unit-testable
single-iteration function (it awaits the resident doc's `tokio::sync::Mutex`)
plus a thin interval loop that spawns it.

### New module `src-tauri/src/liveness_heartbeat.rs`

```rust
/// One heartbeat iteration: lock the resident trust doc and run the existing
/// conditional self-liveness refresh. Returns whether it re-signed (so the
/// caller can `notify_dirty()` + log). Never signs unconditionally —
/// `refresh_self_liveness` is the ~15-day gate, so this is a cheap no-op on
/// almost every tick. Mirrors the panel path's `refresh -> if refreshed
/// notify_dirty` split (owner_commands.rs:729/734): the notify lives in the
/// caller, keeping this unit engine-free and trivially testable.
pub async fn run_liveness_heartbeat_once(
    doc: &Arc<tokio::sync::Mutex<OwnerState>>,
    device_sk: &ed25519_dalek::SigningKey,
    now_secs: u64,
) -> bool {
    let mut g = doc.lock().await;
    crate::owner_state::refresh_self_liveness(&mut g, device_sk, now_secs)
}

pub const LIVENESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 h

/// Interval loop. First `tick()` fires immediately → a refresh-check at node
/// start (the important case for intermittently-online and headless nodes),
/// then hourly. The notify_dirty (→ debounced sibling sync + persist) fires only
/// on the rare tick that actually re-signed.
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

Notes:
- `notify_dirty(&self)` is a sync `pub fn` (`fleet_sync.rs:460`); its dirty flag is
  a private field, which is another reason `run_..._once` stays engine-free (an
  external module cannot observe the flag in a test).
- `now_unix_secs()` is a small module-local `SystemTime`→secs helper (mirrors the
  private `now_unix` at `owner_commands.rs:61`).
- The lock is `tokio::sync::Mutex` (the resident `owner_trust_doc` type,
  `lib.rs:1413`), so `run_..._once` is `async`.

### Wiring into `start_node_inner`

Spawn the heartbeat **inside the owner-trust guard-store block** (`lib.rs:~11725`,
right after `guard.owner_trust_sync = owner_trust_sync_engine_opt.clone();`). This is
the point where all three inputs are simultaneously in scope **and** the `NodeState`
`guard` is held — so the spawn and the handle-stash happen in one place.

(As-built note: an earlier draft placed the spawn at the late voting-tick block
(`~12843`) reading the `_opt` locals directly. The compiler rejected it — those
`_opt` locals go out of scope before that block, which is precisely why the
voting-tick block reads `voting_logs` from `guard` rather than a local. The
guard-store block is the correct home.)

Inputs:
- `owner_trust_doc_opt` and `owner_trust_sync_engine_opt` already exist (decl
  `lib.rs:~4194`, set `lib.rs:~6120`) and are in scope at the guard-store block
  (they are consumed there — `guard.owner_trust_doc = owner_trust_doc_opt.clone()`).
- A **new** sibling local `heartbeat_device_sk_opt: Option<Arc<ed25519_dalek::SigningKey>>`
  declared next to `owner_trust_doc_opt` (`~4199`) and set at `~6122`, where
  `loaded.device_signing_key` is in scope:
  `heartbeat_device_sk_opt = Some(Arc::new(loaded.device_signing_key.clone()));`.
  The key lives **only** in this local and is moved into the task closure — it is
  **never stored on `NodeState`**, keeping secret-key access scoped to the heartbeat
  task. (`loaded.device_signing_key` is dropped later in the function, which is why
  the key is captured into the local at `~6122` rather than read at the spawn block.)

The spawn block: only if all three `_opt`s are `Some` (i.e. an owner identity +
resident trust engine exist), call `spawn_liveness_heartbeat(...)` and stash the
`JoinHandle` into the `guard.liveness_heartbeat_handle` slot (`replace()`, aborting
any prior handle). Because the **spawn and the stash live in the same guard block**
that stores the doc/engine, they are **skipped together** on a lock-poison rollback
that bypasses the block — so a rollback never leaves a spawned-but-untracked task
(no leak), without any start-failure cleanup-tuple threading (unlike the
early-spawned `community_relay_*` handles). On the normal path the handle is in the
`NodeState`-resident slot, so `stop_inner` finds and aborts it.

`JoinHandle` lifecycle (mirror `voting_tick_handle`):
- new `NodeState` field `pub liveness_heartbeat_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>`
  (mirror field `lib.rs:~1184`, inits `lib.rs:~1915` + `lib.rs:~70332`);
- aborted on node stop in `stop_inner` (mirror `lib.rs:~2467`).

Because the heartbeat captures only these three already-`Arc`/`Clone` handles (not
the whole `NodeState`), it **avoids the two-seam GUI-`wry_handle`-vs-headless-`owned_Arc`
closure** that the voting-tick auto-exec fn requires (`build_auto_exec_fn`,
`lib.rs:3384`). One code path covers both GUI and headless `serve` with no
divergence; headless nodes — which never open a Devices panel — are the primary
beneficiary.

### Cadence rationale

- `tokio::time::interval` fires its first `tick()` immediately, so **every node
  re-checks at boot** — the decisive case for intermittently-online and headless
  nodes — then hourly.
- The actual re-sign + `notify_dirty` + Zenoh replication fires only when the cert
  crosses the existing ~15-day threshold, i.e. roughly **once per fortnight per
  device**. Almost every hourly tick is a cheap local no-op (one lock + one
  timestamp compare, no network).
- `1 h` (`LIVENESS_HEARTBEAT_INTERVAL`) matches the existing `IDLE_REFRESH_INTERVAL`
  idiom (`reachability_publisher.rs:62`). Worst-case staleness after crossing 15 d
  is `15 d + 1 h` — negligible against the 30-day window.
- **No thundering herd / no jitter needed:** each device's 15-day clock is relative
  to its own cert `timestamp`, which is naturally staggered across a user's
  devices, so even if all of a user's devices tick on the same wall-clock second
  they do not all re-sign at once.

### Persistence

The heartbeat runs only on a **running** node, so it uses the resident-branch
semantics: `refresh_self_liveness` + `engine.notify_dirty()`. The `owner-trust-v1`
engine is configured with `persist: TrustPersist` (`owner_trust_sync.rs:197`,
disk source of truth `owner_state.cbor` via `save_owner_state_cbor_only`), so the
same mechanism the panel-load path already relies on both persists the refreshed
cert and publishes it. The plan will confirm the debounced dirty path persists
locally (not only publishes); if it does not, add an explicit `persist_now()`
after `notify_dirty()`. Even in the worst case (solo device, publish-to-nobody),
losing the refresh across a restart is merely a re-sign on next boot — no
correctness impact.

## Error handling

- A sign/add failure inside `refresh_self_liveness` logs a `warn` and returns
  `false` (`owner_state.rs:888`/`895`) → the heartbeat treats it as a no-op tick
  and retries next interval. No panic, no task exit.
- The spawned task loop never returns; it is aborted on node stop via the handle.
- If the resident trust doc/engine are absent, the heartbeat is not spawned at all.
- **Clock anomalies (ZEB-721 detection).** `now_unix_secs()` returns `Option`: a
  pre-epoch/broken clock yields `None`, and the loop **skips** that tick with a
  `warn` rather than passing `0` (which, on a missing cert, would sign a
  timestamp-`0` cert that is instantly stale to peers). Separately, if our own cert
  is stamped **in the future** relative to `now` (the host clock regressed since we
  signed), `run_liveness_heartbeat_once` emits a `warn` — `refresh_self_liveness`
  correctly no-ops (re-signing with a lower timestamp would lose the liveness CRDT
  merge), so renewal is suppressed until the clock recovers. The heartbeat can only
  **detect + surface** this; the systemic remediation (monotonic floor / signing-time
  clock sanity in the shared refresh path) is tracked as **ZEB-721** — it is a
  pre-existing property of `refresh_self_liveness` shared with the panel-load path,
  not introduced here.

## Testing

- **Unit (`liveness_heartbeat.rs` `#[cfg(test)]`)** — `run_liveness_heartbeat_once`:
  - fresh cert (timestamp = now) → returns `false`, cert timestamp unchanged.
  - missing cert → returns `true`, cert now present at `now`.
  - stale cert (timestamp = now − 20 days) → returns `true`, cert timestamp
    advanced to `now`.
  Because `run_liveness_heartbeat_once` is engine-free, each test needs only an
  `Arc<tokio::sync::Mutex<OwnerState>>` (built from
  `harmony_owner::lifecycle::mint_owner(now)` → `MintResult { state,
  device_signing_key, .. }`) and asserts on the returned bool + the doc's
  `liveness` map — no `FleetSyncEngine` construction required. The
  `refresh_self_liveness` gate itself is already covered by
  `owner_state.rs:1739`/`1765`/`1779`; these tests pin the heartbeat's async
  lock wrapper and its return contract.
- The `if refreshed { engine.notify_dirty() }` glue in `spawn_liveness_heartbeat`
  is the same trivial pattern as the panel path (`owner_commands.rs:734`); the
  panel path already exercises it in production, so it is left as thin, un-unit-
  tested spawn glue (mirroring `spawn_voting_tick`, which is likewise not
  unit-tested apart from its `run_voting_tick` core).
- Full CI-parity gates: `cargo fmt --all -- --check`,
  `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`,
  `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.

## Files

- **Create:** `src-tauri/src/liveness_heartbeat.rs` — `run_liveness_heartbeat_once`,
  `spawn_liveness_heartbeat`, `LIVENESS_HEARTBEAT_INTERVAL`, `now_unix_secs` helper,
  unit tests.
- **Modify:** `src-tauri/src/lib.rs`:
  - `pub mod liveness_heartbeat;` declaration (near the other `pub mod` lines, ~208).
  - `NodeState.liveness_heartbeat_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>`
    field (mirror `voting_tick_handle` `~1184`) + inits in the two `NodeState` literals
    (mirror `~1915`, `~70332`) + abort in `stop_inner` (mirror `~2467`).
  - New sibling local `heartbeat_device_sk_opt: Option<Arc<ed25519_dalek::SigningKey>>`
    (decl next to `owner_trust_doc_opt` `~4199`), set at `~6122`.
  - Spawn + slot-store block inside the owner-trust guard-store block (`~11726`,
    right after `guard.owner_trust_sync = owner_trust_sync_engine_opt.clone();`).

## Risks / open detail

- **`_opt` local scope** was the one care-point, and the **compiler settled it**: the
  `_opt` locals are in scope at the owner-trust guard-store block (they are consumed
  there) but **not** at the later voting-tick block — an earlier draft that spawned at
  the voting-tick block failed to compile (`not found in this scope`), which is why the
  spawn lives in the guard-store block.
- **No start-failure cleanup needed:** the spawn and the handle-stash are in the same
  guard block that stores the trust doc/engine, so a lock-poison rollback that bypasses
  that block skips both together (no spawned-but-untracked task → no leak). On the
  normal path the handle is in the `NodeState`-resident slot, so `stop_inner` finds it —
  unlike the early-spawned `community_relay_*` handles, no cleanup-tuple threading is
  required. (A superseding start's spawn `replace()`s and aborts the prior handle.)
- **DRY (deferred):** the panel path's resident branch (`owner_commands.rs:729`/
  `734`) is left untouched — its direct `&mut` guard differs from the heartbeat's
  `tokio::sync::Mutex` lock, so sharing a helper would complicate more than it saves.
  The shared logic (`refresh_self_liveness`) is already one function.
- **`harmony-app relink` cost:** this touches `lib.rs`, which relinks the ~97
  integration binaries; use `--lib` / `scripts/test-select` for iterative gates and
  reserve the full `--workspace --all-targets` sweep for the final gate.
