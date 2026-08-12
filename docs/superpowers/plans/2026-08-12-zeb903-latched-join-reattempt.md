# ZEB-903 Latched-Join Re-attempt Driver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When an iroh invite redeem latches a pending Space (ZEB-899), automatically re-run the one-round-trip handshake on the next transport up-edge so convergence takes seconds, not minutes.

**Architecture:** A per-community driver task (new module `src/latched_join_reattempt.rs`) subscribes to the existing `transport_epoch_tx` watch and re-invokes `connectivity_redeem_invite_iroh_inner` with the same owned handles the IPC impl already snapshots. Lifecycle is hosted on `CommunitySyncRegistry` (a shutdown-watch map mirroring `root_fetch_shutdowns`); the spawn site is `connectivity_redeem_invite_iroh_impl`, gated on the latched outcome.

**Tech Stack:** Rust / tokio (`watch`, paused-clock `tokio::time`), existing pkarr_net two-party integration harness.

**Spec:** `docs/superpowers/specs/2026-08-12-zeb903-latched-join-reattempt-design.md`

## Global Constraints

- All cargo commands run from `src-tauri/`, with `--locked --features test-fixtures` for tests.
- Gates: `cargo fmt --all -- --check`; `cargo clippy --all-targets --no-deps -- -D warnings`; `cargo nextest run --locked --features test-fixtures` (targeted per task, full sweep before PR).
- Pipe exit codes lie in zsh — check `${pipestatus[1]}` when piping cargo output.
- No worktrees; work on branch `zeblith/zeb-903-zeb-902-part-2-follow-up-cross-wan-pending-join-host-roster`.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D`.
- No wall-clock reads in the driver — `tokio::time::Instant` only (paused-clock testable).

---

### Task 1: Registry lifecycle surface (`latched_reattempt_shutdowns`)

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (registry struct ~5680, constructor ~5954, `stop_engine` ~6782, `shutdown_all` ~6807; tests at file bottom)

**Interfaces:**
- Produces: `CommunitySyncRegistry::register_latched_reattempt(&self, community_id: SpaceId) -> (u64, watch::Receiver<bool>)` (latest-wins: flips + replaces an existing entry) and `unregister_latched_reattempt(&self, community_id: &SpaceId, registration_gen: u64)` (removes only if the generation matches — a replaced driver must not remove its successor's entry). `stop_engine` / `shutdown_all` flip + remove unconditionally.

- [ ] **Step 1: Write the failing unit test** (module test block in `community_state_sync.rs`, near the existing registry tests)

```rust
#[tokio::test]
async fn latched_reattempt_registration_is_latest_wins_and_gen_guarded() {
    let registry = test_registry(); // whatever constructor the neighboring registry tests use
    let cid = SpaceId([7u8; 32]);

    let (gen1, rx1) = registry.register_latched_reattempt(cid).await;
    assert!(!*rx1.borrow(), "fresh registration must start un-flipped");

    // Latest-wins: second registration flips the first receiver.
    let (gen2, rx2) = registry.register_latched_reattempt(cid).await;
    assert!(*rx1.borrow(), "replaced registration must be flipped");
    assert!(!*rx2.borrow());
    assert_ne!(gen1, gen2);

    // Stale-gen unregister must NOT remove the newer entry.
    registry.unregister_latched_reattempt(&cid, gen1).await;
    let (_, rx3) = registry.register_latched_reattempt(cid).await;
    assert!(
        *rx2.borrow(),
        "gen2 entry must still have been present (its sender flipped by the third registration)"
    );
    drop(rx3);
}

#[tokio::test]
async fn shutdown_all_flips_latched_reattempt_watches() {
    let registry = test_registry();
    let cid = SpaceId([8u8; 32]);
    let (_, rx) = registry.register_latched_reattempt(cid).await;
    let _ = registry.shutdown_all().await;
    assert!(*rx.borrow(), "shutdown_all must flip the re-attempt shutdown watch");
}
```

(Adapt `test_registry()` to the construction idiom the neighboring registry unit tests use — do not invent a new fixture.)

- [ ] **Step 2: Run to verify failure** — `cargo nextest run --locked --features test-fixtures 'test(latched_reattempt_registration)'` → FAIL (method not found).

- [ ] **Step 3: Implement**

Add the field beside `root_fetch_shutdowns` (same lock-discipline doc comment style):

```rust
/// ZEB-903: per-community shutdown senders for the latched-join
/// re-attempt drivers, plus a monotonically increasing registration
/// generation. Latest-wins on re-registration (a fresh invite URL for
/// the same community replaces a parked driver); `stop_engine` /
/// `shutdown_all` flip + remove so the driver collapses with its
/// community. Lock-discipline: identical to `root_fetch_shutdowns` —
/// never held with the `engines` lock; `watch::Sender::send` is sync.
latched_reattempt_shutdowns:
    tokio::sync::Mutex<std::collections::HashMap<SpaceId, (u64, tokio::sync::watch::Sender<bool>)>>,
latched_reattempt_next_gen: std::sync::atomic::AtomicU64,
```

Constructor: `latched_reattempt_shutdowns: tokio::sync::Mutex::new(std::collections::HashMap::new()), latched_reattempt_next_gen: std::sync::atomic::AtomicU64::new(0),`

Methods (near the root-fetch shutdown helpers):

```rust
/// ZEB-903: register (or latest-wins replace) the re-attempt driver slot
/// for `community_id`. Returns the registration generation + shutdown
/// receiver the driver must hold.
pub async fn register_latched_reattempt(
    &self,
    community_id: SpaceId,
) -> (u64, tokio::sync::watch::Receiver<bool>) {
    let registration_gen = self
        .latched_reattempt_next_gen
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = tokio::sync::watch::channel(false);
    let mut g = self.latched_reattempt_shutdowns.lock().await;
    if let Some((_, old)) = g.insert(community_id, (registration_gen, tx)) {
        let _ = old.send(true);
    }
    (registration_gen, rx)
}

/// ZEB-903: driver self-removal on exit. Generation-guarded so a driver
/// replaced by a latest-wins re-registration cannot remove its
/// successor's entry.
pub async fn unregister_latched_reattempt(&self, community_id: &SpaceId, registration_gen: u64) {
    let mut g = self.latched_reattempt_shutdowns.lock().await;
    if g.get(community_id).is_some_and(|(gen, _)| *gen == registration_gen) {
        g.remove(community_id);
    }
}
```

In `stop_engine` (beside the `root_fetch_shutdowns` removal) and `shutdown_all` (beside its root-fetch loop), flip + remove:

```rust
if let Some((_, tx)) = self.latched_reattempt_shutdowns.lock().await.remove(community_id) {
    let _ = tx.send(true);
}
```

(`shutdown_all`: drain the whole map, flipping each — sequential with, never nested inside, the other locks, mirroring the existing root-fetch loop.)

- [ ] **Step 4: Run tests** → PASS. Also `cargo clippy --all-targets --no-deps -- -D warnings` and `cargo fmt --all`.
- [ ] **Step 5: Commit** — `feat(zeb903): registry lifecycle slots for latched-join re-attempt drivers`

---

### Task 2: `latched_join_reattempt` module — cooldown primitive

**Files:**
- Create: `src-tauri/src/latched_join_reattempt.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod latched_join_reattempt;` beside the neighboring module declarations)

**Interfaces:**
- Produces: `pub const REATTEMPT_COOLDOWN_MS: u64 = 30_000;` and `async fn cooldown_wait(last_attempt: Option<tokio::time::Instant>, shutdown_rx: &mut watch::Receiver<bool>) -> bool` (true = proceed; deferred-not-dropped; false = shutdown fired during the wait). Module-private; consumed by Task 3's driver loop.

- [ ] **Step 1: Write the failing unit tests** (in-module `#[cfg(test)]`)

```rust
#[tokio::test(start_paused = true)]
async fn cooldown_defers_to_boundary_not_drops() {
    let (_tx, mut rx) = tokio::sync::watch::channel(false);
    // No prior attempt: immediate.
    assert!(cooldown_wait(None, &mut rx).await);
    // Prior attempt just now: the wait must complete only once the paused
    // clock reaches the boundary (auto-advance under start_paused).
    let start = tokio::time::Instant::now();
    assert!(cooldown_wait(Some(start), &mut rx).await);
    assert!(
        tokio::time::Instant::now() >= start + std::time::Duration::from_millis(REATTEMPT_COOLDOWN_MS),
        "cooldown must defer to the boundary, not return early"
    );
}

#[tokio::test(start_paused = true)]
async fn cooldown_aborts_on_shutdown_flip() {
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let wait = tokio::spawn(async move {
        cooldown_wait(Some(tokio::time::Instant::now()), &mut rx).await
    });
    tx.send(true).expect("send shutdown");
    assert!(!wait.await.expect("join"), "shutdown during cooldown must return false");
}
```

- [ ] **Step 2: Run to verify failure** — module doesn't exist → compile FAIL.

- [ ] **Step 3: Implement**

```rust
//! ZEB-903: reachability-driven re-attempt driver for latched-pending
//! iroh joins (spec: docs/superpowers/specs/2026-08-12-zeb903-latched-join-reattempt-design.md).

/// Minimum spacing between re-attempts per community. An up-edge inside
/// the window is deferred to the boundary (not dropped), mirroring
/// `channel_backfill::cooldown_wait`.
pub const REATTEMPT_COOLDOWN_MS: u64 = 30_000;

/// True = proceed with the attempt (immediately, or after deferring to
/// the cooldown boundary). False = the shutdown watch flipped (or its
/// sender dropped) during the wait — the caller must exit.
async fn cooldown_wait(
    last_attempt: Option<tokio::time::Instant>,
    shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    let Some(last) = last_attempt else {
        return true;
    };
    let target = last + std::time::Duration::from_millis(REATTEMPT_COOLDOWN_MS);
    if tokio::time::Instant::now() >= target {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep_until(target) => true,
        changed = shutdown_rx.changed() => {
            !(changed.is_err() || *shutdown_rx.borrow())
        }
    }
}
```

- [ ] **Step 4: Run tests** → PASS; clippy + fmt.
- [ ] **Step 5: Commit** — `feat(zeb903): latched_join_reattempt module + cooldown primitive`

---

### Task 3: Driver loop + spawn helper

**Files:**
- Modify: `src-tauri/src/latched_join_reattempt.rs`

**Interfaces:**
- Consumes: Task 1's `register_latched_reattempt` / `unregister_latched_reattempt`; Task 2's `cooldown_wait`; `crate::connectivity_redeem_invite_iroh_inner` (existing, unchanged).
- Produces: `pub struct ReattemptContext { ... }` (owned clones — field list below) and `pub async fn spawn_reattempt_driver(ctx: ReattemptContext) -> Option<tokio::task::JoinHandle<()>>` (None when the URL fails to decode or no epoch watch is present; the handle is for tests — production drops it).

- [ ] **Step 1: Implementation** (integration tests land in Task 5 and are the red/green cycle for this task — write this code, then Task 5's T1 first proves it end-to-end; the unit-testable pieces were Task 2)

```rust
/// Owned-clone bundle of everything `connectivity_redeem_invite_iroh_inner`
/// needs, captured by the IPC impl BEFORE its own inner call (which
/// consumes its arguments). `sink` is optional so tests can omit event
/// emission; production passes the real `NodeEventSink`.
pub struct ReattemptContext {
    pub invite_url: String,
    pub pkarr_resolver: Option<std::sync::Arc<harmony_pkarr::PkarrResolver>>,
    pub reachability_resolver: Option<crate::reachability_resolver::ReachabilityResolver>,
    pub iroh_endpoint: Option<std::sync::Arc<crate::iroh_endpoint::IrohEndpoint>>,
    pub crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    pub hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<harmony_crdt_sync::ReplayTracker<String, crate::owner_state_types::Hlc>>,
    >,
    pub adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
    pub device_id: String,
    pub self_owner: crate::owner_state_types::OwnerAddr,
    pub community_signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    pub enrollment_cert: harmony_owner::certs::EnrollmentCert,
    pub community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    pub community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    pub transport_epoch_rx: Option<tokio::sync::watch::Receiver<u64>>,
    pub dm_outbox: std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    pub channel_log_registry:
        std::sync::Arc<crate::community_channel_log_engine::ChannelLogRegistry>,
    pub sync_engine: Option<std::sync::Arc<crate::owner_state_sync::SyncEngine>>,
    pub identity_dir: Option<std::path::PathBuf>,
    pub sink: Option<std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>>,
    pub dial_config: crate::HandshakeDialConfig,
}

/// Register + spawn the per-community driver. Returns the task handle
/// for tests; production ignores it. `None` = nothing spawned (URL
/// undecodable — cannot happen right after a successful redeem — or no
/// transport-epoch watch to subscribe to).
pub async fn spawn_reattempt_driver(ctx: ReattemptContext) -> Option<tokio::task::JoinHandle<()>> {
    let payload = crate::community_invite::decode_invite_url(&ctx.invite_url).ok()?;
    let community_id = payload.community_id;
    let epoch_rx = ctx.transport_epoch_rx.clone()?;
    let (registration_gen, shutdown_rx) = ctx
        .community_registry
        .register_latched_reattempt(community_id)
        .await;
    tracing::info!(
        community_id = %hex::encode(community_id.0),
        "ZEB-903: latched-join re-attempt driver armed"
    );
    Some(tokio::spawn(run_reattempt_driver(
        ctx,
        community_id,
        registration_gen,
        epoch_rx,
        shutdown_rx,
    )))
}

/// True while the Space still exists AND still carries `pending_join_at`
/// — the demand that justifies the driver. Space gone (left community)
/// or pending cleared (gossip / manual retry converged) ⇒ collapse.
async fn space_still_pending(
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    community_id: &crate::owner_state_types::SpaceId,
) -> bool {
    let g = crdt_state.lock().await;
    g.spaces
        .get(community_id)
        .is_some_and(|s| s.pending_join_at.is_some())
}

async fn run_reattempt_driver(
    ctx: ReattemptContext,
    community_id: crate::owner_state_types::SpaceId,
    registration_gen: u64,
    mut epoch_rx: tokio::sync::watch::Receiver<u64>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    // The driver reacts to FUTURE up-edges only: the latch was committed
    // seconds after a failed handshake — re-dialing immediately would
    // just re-fail.
    epoch_rx.borrow_and_update();
    let mut last_attempt: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
            changed = epoch_rx.changed() => {
                if changed.is_err() {
                    // Event loop gone — no further reachability signals.
                    break;
                }
                epoch_rx.borrow_and_update();
            }
        }
        if !cooldown_wait(last_attempt, &mut shutdown_rx).await {
            break;
        }
        if !space_still_pending(&ctx.crdt_state, &community_id).await {
            break;
        }
        last_attempt = Some(tokio::time::Instant::now());
        match attempt_once(&ctx, &shutdown_rx).await {
            Ok(outcome) if outcome.status == "joined" && !outcome.pending => {
                tracing::info!(
                    community_id = %hex::encode(community_id.0),
                    "ZEB-903: re-attempt converged the latched pending join"
                );
                break;
            }
            Ok(outcome) => {
                tracing::debug!(
                    community_id = %hex::encode(community_id.0),
                    status = %outcome.status,
                    pending = outcome.pending,
                    "ZEB-903: re-attempt did not converge; waiting for the next up-edge"
                );
            }
            Err(e) => {
                tracing::debug!(
                    community_id = %hex::encode(community_id.0),
                    error = %e,
                    "ZEB-903: re-attempt errored; waiting for the next up-edge"
                );
            }
        }
    }
    ctx.community_registry
        .unregister_latched_reattempt(&community_id, registration_gen)
        .await;
}

/// One full handshake attempt. No-op progress sink (never ghost-drive
/// the redeem dialog); real nav sink when a `NodeEventSink` is present;
/// fence = this driver's own shutdown watch (a teardown racing an
/// in-flight attempt suppresses the commit exactly like a generation
/// trip — see spec §2.2 for why no NodeState generation fence).
async fn attempt_once(
    ctx: &ReattemptContext,
    shutdown_rx: &tokio::sync::watch::Receiver<bool>,
) -> Result<crate::RedemptionOutcome, crate::community_invite::RedeemInviteError> {
    let fence_rx = shutdown_rx.clone();
    let fence_check = move || -> Result<(), crate::community_invite::RedeemInviteError> {
        if *fence_rx.borrow() {
            return Err(crate::community_invite::RedeemInviteError::new(
                crate::community_invite::RedeemInviteErrorCode::GenerationChanged,
                "latched-join re-attempt driver shut down mid-attempt; commit suppressed"
                    .to_string(),
            ));
        }
        Ok(())
    };
    let sink = ctx.sink.clone();
    let nav_emit_sink = move |payload: crate::NavUpdatedPayload| {
        if let Some(s) = sink.as_ref() {
            crate::node_event_sink::emit_ser(s.as_ref(), "nav-updated", &payload);
        }
    };
    crate::connectivity_redeem_invite_iroh_inner(
        ctx.invite_url.clone(),
        ctx.pkarr_resolver.clone(),
        ctx.reachability_resolver.clone(),
        ctx.iroh_endpoint.clone(),
        std::sync::Arc::clone(&ctx.crdt_state),
        std::sync::Arc::clone(&ctx.hlc_tracker),
        ctx.adopt_floor.clone(),
        ctx.device_id.clone(),
        ctx.self_owner,
        std::sync::Arc::clone(&ctx.community_signing_key),
        ctx.enrollment_cert.clone(),
        std::sync::Arc::clone(&ctx.community_registry),
        ctx.community_adapter_tx.clone(),
        ctx.transport_epoch_rx.clone(),
        std::sync::Arc::clone(&ctx.dm_outbox),
        std::sync::Arc::clone(&ctx.channel_log_registry),
        ctx.sync_engine.clone(),
        ctx.identity_dir.clone(),
        |_progress| {},
        nav_emit_sink,
        ctx.dial_config.clone(),
        fence_check,
    )
    .await
}
```

Adaptation notes (resolve at implementation time against the real code, not by guessing): exact types for `SpaceId` import path, `HandshakeDialConfig` (derive `Clone` if it doesn't already), `RedemptionOutcome.status` field access (it may be a method or enum — match whatever the ZEB-899 tests assert on), and the `self_owner` copy-vs-clone. The inner's parameter ORDER is authoritative from its definition at `lib.rs:63622`.

- [ ] **Step 2: Compile + clippy + fmt** (`cargo clippy --all-targets --no-deps -- -D warnings`) — no behavior tests yet (Task 5 is the red/green for this code).
- [ ] **Step 3: Commit** — `feat(zeb903): re-attempt driver loop + spawn helper`

---

### Task 4: Spawn wiring in the IPC impl

**Files:**
- Modify: `src-tauri/src/lib.rs` (`connectivity_redeem_invite_iroh_impl`, ~62412–62600: clone the bundle before the inner call; gate + spawn after it; update the fn's flow doc)

**Interfaces:**
- Consumes: Task 3's `ReattemptContext` / `spawn_reattempt_driver`.

- [ ] **Step 1: Implement**

Immediately before the `let outcome = connectivity_redeem_invite_iroh_inner(` call, build the bundle from the already-snapshotted handles (all are still in scope; the inner call consumes the originals, so clone here):

```rust
// ZEB-903: owned-clone bundle for the latched-join re-attempt driver.
// Cloned BEFORE the inner call (which consumes its arguments); only
// used when the outcome latches (joined + pending).
let reattempt_ctx = crate::latched_join_reattempt::ReattemptContext {
    invite_url: invite_url.clone(),
    pkarr_resolver: pkarr_resolver.clone(),
    reachability_resolver: reachability_resolver.clone(),
    iroh_endpoint: iroh_endpoint.clone(),
    crdt_state: std::sync::Arc::clone(&crdt_state),
    hlc_tracker: std::sync::Arc::clone(&hlc_tracker),
    adopt_floor: adopt_floor.clone(),
    device_id: device_id.clone(),
    self_owner,
    community_signing_key: std::sync::Arc::clone(&community_signing_key),
    enrollment_cert: enrollment_cert.clone(),
    community_registry: std::sync::Arc::clone(&community_registry),
    community_adapter_tx: community_adapter_tx.clone(),
    transport_epoch_rx: transport_epoch_rx.clone(),
    dm_outbox: std::sync::Arc::clone(&dm_outbox),
    channel_log_registry: std::sync::Arc::clone(&channel_log_registry),
    sync_engine: sync_engine.clone(),
    identity_dir: crate::owner_commands::resolve_identity_dir().ok(),
    sink: Some(std::sync::Arc::clone(&sink)),
    dial_config: HandshakeDialConfig::from_env(),
};
```

After `let outcome = ... .await?;`:

```rust
// ZEB-903: a latched outcome (joined + pending) arms the per-community
// re-attempt driver — the next transport up-edge re-runs the fast
// handshake instead of waiting minutes for gossip convergence. No-op
// when the node has no transport-epoch watch (the driver has nothing
// to subscribe to; the passive paths still converge).
if outcome.status == "joined" && outcome.pending {
    let _ = crate::latched_join_reattempt::spawn_reattempt_driver(reattempt_ctx).await;
}
```

(Adapt the `outcome.status` comparison to the real `RedemptionOutcome` shape, matching how the ZEB-899 tests read it.)

- [ ] **Step 2: Compile + clippy + fmt.** Run the existing redeem integration tests to prove no regression: `cargo nextest run --locked --features test-fixtures 'test(pkarr_iroh_redeem)'` → all existing tests PASS (they call the inner directly, so no driver spawns).
- [ ] **Step 3: Commit** — `feat(zeb903): arm the re-attempt driver on latched iroh redeem outcomes`

---

### Task 5: Integration tests (T1–T3)

**Files:**
- Modify: `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs`

**Interfaces:**
- Consumes: the existing two-party harness (`setup_two_party_iroh_handshake[_with_config]`, `zeb889_build_targeted_invite`, the latch-seed idiom from `zeb889_retry_reuses_mint_and_redeems_zombie_invite`), Tasks 1–4.

- [ ] **Step 1: Write T1 (happy path) — this is the red test for Tasks 3–4**

Structure (concrete assertions; plumb the harness fields exactly as `zeb889_retry_reuses_mint_and_redeems_zombie_invite` does — same mint acquisition, same latch seed with `redeem_timeout: Some(Duration::from_secs(1))`):

```rust
#[tokio::test(flavor = "multi_thread")]
async fn zeb903_reattempt_driver_converges_latched_join_on_epoch_bump() {
    // 1. Live two-party harness (normal acceptor config).
    // 2. Build the targeted invite + mint (same helpers as the zeb889 retry test).
    // 3. Seed a latched-pending Space via redeem_invite_inner_with_overrides
    //    (same call shape as the zeb889 retry test's latch seed; assert
    //    latch_dto.pending and the Space row's pending_join_at is Some).
    // 4. Build a ReattemptContext from Bob's harness handles:
    //    - invite_url, pkarr_resolver: Some(s.pkarr_resolver), reachability_resolver: None,
    //      iroh_endpoint: Some(bob endpoint), crdt_state/hlc_tracker/adopt_floor/device
    //      fields exactly as the ZEB-899 latch-degrade test passes them to the
    //      connectivity inner, sink: None,
    //      transport_epoch_rx: Some(rx) from a fresh watch::channel(0u64),
    //      dial_config: the same HandshakeDialConfig the latch-degrade test builds.
    let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
    // 5. let handle = spawn_reattempt_driver(ctx).await.expect("driver must arm");
    // 6. epoch_tx.send_modify(|e| *e += 1);
    // 7. Poll (poll_until idiom, ≤30s) until Bob's Space pending_join_at is None.
    // 8. Assert: invite burned on Alice's side (same assertion as the retry test),
    //    mint cache evicted, and the driver task ends: handle await with timeout.
    //    (Driver exit implies unregistration; T2/T3 pin the registry entry directly.)
}
```

- [ ] **Step 2: Run T1 → verify it fails before Task 3/4 code exists, passes after.** (If implementing strictly in plan order, write T1 first, watch it fail to compile, then land Tasks 3–4.)

- [ ] **Step 3: Write T2 (demand collapsed) + T3 (shutdown)**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn zeb903_reattempt_driver_exits_without_attempt_when_pending_cleared() {
    // Complete a normal full redeem (existing happy-path idiom) so the Space
    // is joined with pending_join_at == None. Build a ReattemptContext with
    // iroh_endpoint: None (an attempt would observably error — control for
    // the no-attempt claim). Spawn, bump the epoch watch, await the handle
    // (≤5s): the driver must exit via the demand-collapsed branch. Assert
    // the Space is still joined/not-pending and the registry slot is gone
    // (register again and assert the fresh receiver is un-flipped).
}

#[tokio::test(flavor = "multi_thread")]
async fn zeb903_reattempt_driver_collapses_on_registry_shutdown() {
    // Seed a latch (as T1), spawn the driver, then registry.shutdown_all()
    // (best-effort `let _ =` — this harness has no live adapter transport).
    // Await the handle (≤5s): driver must exit. Space must STILL be pending
    // (no attempt fired), invite still unburned.
}
```

- [ ] **Step 4: Full targeted run** — `cargo nextest run --locked --features test-fixtures 'test(zeb903)'` + the whole `pkarr_iroh_redeem` file green, no LEAK flags (use the best-effort `let _ = registry.shutdown_all().await;` teardown in every engine-spawning test).
- [ ] **Step 5: Commit** — `test(zeb903): re-attempt driver integration pins (converge / collapse / shutdown)`

---

### Task 6: Gates + PR

- [ ] `cargo fmt --all -- --check`, `cargo clippy --all-targets --no-deps -- -D warnings` (check `${pipestatus[1]}`), full `cargo nextest run --locked --features test-fixtures` sweep, `git status` clean.
- [ ] Push branch; open PR (body: premise recap, behavior change, tests; footer per convention); fire `@coderabbitai review` ONCE; converge per protocol.

## Self-review notes

- Spec §2.2 (latest-wins + gen guard) ↔ Task 1; §2.1 loop ↔ Task 3 (cooldown deferred-not-dropped, future-up-edges-only, demand-collapse checks); §2.3 spawn gate ↔ Task 4; §4 tests ↔ Tasks 2/5 (U1 in Task 2, U2 in Task 1, T1–T3 in Task 5). A2/persistence/LAN declines need no tasks.
- Type-consistency: `register_latched_reattempt` returns `(u64, Receiver)` everywhere; `spawn_reattempt_driver(ctx) -> Option<JoinHandle<()>>` used in Tasks 4 (drop) and 5 (await).
