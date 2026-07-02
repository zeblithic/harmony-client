# ZEB-321 Phase 3 Small-Slice Bundle (S0+S1+S2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One bundled PR delivering ZEB-617 (pin iroh relays off n0 canary), ZEB-613 (headless presence auto-subscribe), ZEB-618 (root-driver parity: persisted floor + presence-kick for mail-root and community-root), plus the ZEB-321 Phase 3 umbrella decision-record spec doc.

**Architecture:** Three independent small slices sharing one branch (`zeb-617-613-618-phase3-small-slices`). ZEB-617 is a relay-map override at the endpoint builder. ZEB-613 adds a backend subscribe-all helper called from `serve_cli` bootstrap and the join-success tail. ZEB-618 extends `run_root_fetch_driver` with the same two optional inputs `run_backfill_driver` already has (`full_resync_rx` presence kick + `ResyncPersist` restart-aware floor), then wires both call sites.

**Tech Stack:** Rust (tokio, iroh 0.98.2, zenoh 1.9.0), cargo-nextest, paused-time tokio tests.

## Global Constraints

- Rust gates per task: `cargo fmt --all` + `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` + targeted `cargo nextest run --locked -p harmony-app --features test-fixtures -E '...'`. Full sweep only in the final task (relink cost ~50min under --all-targets; per-task scoping is deliberate).
- All cargo commands run from `src-tauri/`.
- `--locked` and `--features test-fixtures` are load-bearing on every cargo invocation.
- Timing tests use paused-time tokio (`#[tokio::test(start_paused = true)]`) — never wall-clock sleeps near budget thresholds.
- Commit after each task; commit messages end with the Claude Code trailer.
- Do NOT touch: `zenoh_conns` registry code (ZEB-616, just merged), DM tunnel, dial driver.

---

### Task 1: Commit the Phase 3 umbrella decision-record spec doc

**Files:**
- Create: `docs/specs/2026-07-02-zeb-321-phase3-decision-record.md`

**Interfaces:**
- Consumes: the working draft at `/private/tmp/claude-501/-Users-zeblith-work/2500b964-6db1-47fa-a7cc-9550e0f242e2/scratchpad/zeb321-phase3-decision-record-draft.md` (session scratchpad; if unavailable, the same content lives as the 2026-07-02 decision-record comment on Linear ZEB-321).
- Produces: the durable spec doc other slices' PRs will reference.

- [ ] **Step 1: Read the scratchpad draft** at the absolute path above (or fetch the ZEB-321 Linear comment).

- [ ] **Step 2: Write `docs/specs/2026-07-02-zeb-321-phase3-decision-record.md`** — full draft content, with the header replaced by:

```markdown
# ZEB-321 Phase 3 — Liveness / Rebinding / Reconnection: Decision Record

**Status:** all areas decided + blessed 2026-07-01
**Ticket:** [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 3
**Slices:** S0=ZEB-617 · S1=ZEB-613 · S2=ZEB-618 · S3=ZEB-619 · S4=ZEB-620 · S5=ZEB-621 · S6=ZEB-622 · S7=ZEB-623 · S8=ZEB-624 · S9=ZEB-522
```

Drop the scratchpad-only lines ("WORKING DRAFT", "Final home", "Open research in flight"); keep everything else (settled scope, ground truth, research findings, approved decisions log, decision areas, slice decomposition).

- [ ] **Step 3: Commit**

```bash
git add docs/specs/2026-07-02-zeb-321-phase3-decision-record.md
git commit -m "docs(zeb-321): Phase 3 umbrella decision record (liveness/rebinding/reconnection)"
```

---

### Task 2: ZEB-617 — pin iroh relays off canary

**Files:**
- Modify: `src-tauri/src/iroh_endpoint.rs:121-143` (`new_with_secret`)
- Test: same file, new `#[cfg(test)]` cases in the existing test module

**Interfaces:**
- Consumes: iroh 0.98.2 `RelayMode::custom(impl IntoIterator<Item = RelayUrl>)` (endpoint.rs:1849), builder `.relay_mode(RelayMode)` (endpoint.rs:533). Preset-override precedent: `presets::N0DisableRelay` = `N0.apply(builder).relay_mode(RelayMode::Disabled)`.
- Produces: `pub(crate) fn stable_relay_mode() -> iroh::RelayMode` (used only within this file, but named so ZEB-624's config surface can reuse it).

- [ ] **Step 1: Write the failing tests** (in `iroh_endpoint.rs`'s existing test module):

```rust
/// ZEB-617: the pinned relay map must be the n0 STABLE cluster —
/// exactly 4 relays, none canary. Guards against a silent revert to
/// `presets::N0`'s canary default on an iroh bump (until ZEB-619
/// supersedes this pin with 1.0's stable defaults).
#[test]
fn stable_relay_mode_pins_four_non_canary_relays() {
    let mode = stable_relay_mode();
    let map = mode.relay_map();
    let urls: Vec<String> = map.urls().map(|u| u.to_string()).collect();
    assert_eq!(urls.len(), 4, "expected 4 stable relays, got {urls:?}");
    for u in &urls {
        assert!(
            !u.contains("canary"),
            "canary relay leaked into stable pin: {u}"
        );
        assert!(
            u.contains(".relay.n0.iroh.link"),
            "unexpected relay host: {u}"
        );
    }
}
```

(If `RelayMap` in 0.98.2 exposes no `urls()` iterator, use `format!("{map:?}")` and assert the Debug string contains all four hostnames and not `canary` — check `docs.rs/iroh/0.98.2` `RelayMap` for the accessor; `relay_map()` on `RelayMode` is confirmed present.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(stable_relay_mode)'`
Expected: FAIL — `stable_relay_mode` not found (compile error).

- [ ] **Step 3: Implement** in `iroh_endpoint.rs`:

```rust
/// ZEB-617: n0's STABLE production relay cluster. iroh 0.98.2's
/// `presets::N0` hard-codes the CANARY cluster (`*.iroh-canary.*`,
/// no prod SLA — the fleet landed there by default, see ZEB-615);
/// these are the hostnames iroh 1.0's stable defaults use. The
/// ZEB-619 upgrade slice supersedes this pin.
const STABLE_RELAY_URLS: [&str; 4] = [
    "https://use1-1.relay.n0.iroh.link.",
    "https://usw1-1.relay.n0.iroh.link.",
    "https://euc1-1.relay.n0.iroh.link.",
    "https://aps1-1.relay.n0.iroh.link.",
];

/// Relay mode pinning the stable cluster. Overrides the preset's
/// canary map the same way `presets::N0DisableRelay` overrides it
/// with `Disabled` — a `.relay_mode(..)` call AFTER the preset wins.
pub(crate) fn stable_relay_mode() -> iroh::RelayMode {
    iroh::RelayMode::custom(STABLE_RELAY_URLS.iter().map(|u| {
        u.parse()
            .expect("STABLE_RELAY_URLS are compile-time constants and must parse")
    }))
}
```

And in `new_with_secret`, change the builder chain (line 126):

```rust
let inner = Endpoint::builder(presets::N0)
    // ZEB-617: pin off the canary relay cluster the N0 preset defaults to.
    .relay_mode(stable_relay_mode())
    .secret_key(secret_key)
```

Adjust the doc comment on `new_with_secret` (line 123-124): replace "uses the default (n0 production) relay configuration" with "pins the n0 STABLE relay cluster (ZEB-617 — the 0.98 preset default is canary)".

- [ ] **Step 4: Run tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(stable_relay_mode)'`
Expected: PASS.

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(iroh_endpoint)'`
Expected: PASS (hermetic endpoint tests use `RelayMode::Disabled` paths, unaffected).

- [ ] **Step 5: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
git add src/iroh_endpoint.rs
git commit -m "feat(zeb-617): pin iroh relays to n0 stable cluster (off canary default)"
```

**Live-verification note for the PR:** 0.98.2-client ↔ stable-relay wire compatibility is UNVERIFIED upstream. Before marking ready: launch the app once and confirm `network_health_snapshot.myNetwork.homeRelayUrl` shows a `*.relay.n0.iroh.link` host (relay handshake succeeded). If stable relays reject 0.98 clients, revert this commit and fold the pin into ZEB-619 — say so on the ticket.

---

### Task 3: ZEB-613 — headless presence auto-subscribe

**Files:**
- Modify: `src-tauri/src/lib.rs` — new helper near `subscribe_community_presence_impl` (~line 31300); hook in `serve_cli` after `start_node_inner` success (line 18680-18683); hook at the join-success tail (~line 28117, see Step 4).
- Test: `src-tauri/src/lib.rs` new `#[cfg(test)]` module `zeb613_auto_subscribe_tests`.

**Interfaces:**
- Consumes: `list_owner_communities_impl(&Mutex<NodeState>) -> Result<Vec<CommunityNavDto>, String>` (lib.rs:18989; `CommunityNavDto { space_id: String /*32-hex*/, name, is_invite_only, pending: bool }`); `subscribe_community_presence_impl(&Mutex<NodeState>, String) -> Result<(), String>` (lib.rs:31303); `NodeState.community_presence_request_tx: Option<mpsc::Sender<CommunityPresenceRequest>>`, `NodeState.crdt_state: Option<Arc<tokio::sync::Mutex<OwnerState>>>`.
- Produces: `pub(crate) async fn auto_subscribe_presence_all_communities(state: &std::sync::Mutex<NodeState>) -> usize`.

- [ ] **Step 1: Write the failing test** (new module next to `zeb393_communities_for_nav_tests`, reusing its `Space`/`OwnerState` fixture builders — copy its `hlc()`/space-construction helpers as needed):

```rust
#[cfg(test)]
mod zeb613_auto_subscribe_tests {
    use super::*;

    /// ZEB-613: the helper subscribes exactly the live (joined,
    /// non-pending, non-left) communities and reports the count.
    #[tokio::test]
    async fn auto_subscribe_covers_live_communities_only() {
        // OwnerState with: one live community, one pending, one left,
        // one non-community space — build via the same constructors
        // zeb393_communities_for_nav_tests uses.
        let owner_state = /* fixture: 4 spaces as above */;

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let state = std::sync::Mutex::new(NodeState::default());
        {
            let mut g = state.lock().unwrap();
            g.crdt_state = Some(std::sync::Arc::new(tokio::sync::Mutex::new(owner_state)));
            g.community_presence_request_tx = Some(tx);
        }

        let n = auto_subscribe_presence_all_communities(&state).await;
        assert_eq!(n, 1);

        let req = rx.try_recv().expect("one Subscribe expected");
        match req {
            crate::event_loop::CommunityPresenceRequest::Subscribe { community_id } => {
                assert_eq!(community_id, LIVE_COMMUNITY_ID_BYTES);
            }
            other => panic!("expected Subscribe, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no extra subscriptions");
    }

    /// No owner loaded (no crdt_state) → 0, no panic.
    #[tokio::test]
    async fn auto_subscribe_no_owner_is_graceful() {
        let state = std::sync::Mutex::new(NodeState::default());
        assert_eq!(auto_subscribe_presence_all_communities(&state).await, 0);
    }
}
```

(Adapt fixture construction to the exact `Space` builder shapes in `zeb393_communities_for_nav_tests` (lib.rs:19003) — same fields, same `SpaceKind::Community`, `left_at`, `pending_join_at` markers. If `CommunityPresenceRequest` doesn't derive `Debug`, match without the panic-formatting.)

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(zeb613)'`
Expected: FAIL — helper not defined.

- [ ] **Step 3: Implement the helper** (in lib.rs, directly above `subscribe_community_presence_impl`):

```rust
/// ZEB-613: subscribe community presence for every live (non-pending,
/// non-left) joined community. Headless parity with the GUI's
/// subscribe-all (`App.svelte` boot path): without this a `serve` node
/// publishes no beacon and builds no roster, so the #384 presence-kick
/// backfill re-arm — and Phase 3's presence-triggered re-dial — are
/// inert for it. Errors are logged, never fatal: presence is a
/// self-healing enhancement, not a boot dependency. Returns the number
/// of communities subscribed.
pub(crate) async fn auto_subscribe_presence_all_communities(
    state: &std::sync::Mutex<NodeState>,
) -> usize {
    let dtos = match list_owner_communities_impl(state).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "ZEB-613: presence auto-subscribe: cannot enumerate communities");
            return 0;
        }
    };
    let mut subscribed = 0usize;
    for dto in dtos.into_iter().filter(|d| !d.pending) {
        match subscribe_community_presence_impl(state, dto.space_id.clone()).await {
            Ok(()) => subscribed += 1,
            Err(e) => {
                tracing::warn!(community = %dto.space_id, error = %e, "ZEB-613: presence auto-subscribe failed");
            }
        }
    }
    tracing::info!(subscribed, "ZEB-613: presence auto-subscribed for joined communities");
    subscribed
}
```

- [ ] **Step 4: Wire the two hooks.**

(a) `serve_cli` (lib.rs, right after the `start_node_inner` success block ending line 18683):

```rust
// ZEB-613: headless presence parity — engage beacon + roster for
// every joined community so presence-kick recovery works on serve
// nodes without a manual `api subscribe_community_presence`.
let _ = auto_subscribe_presence_all_communities(state.as_ref()).await;
```

(b) Join-success tail: the block ending `force_reachability_republish(state); Ok(dto)` (~lib.rs:28117). First run `grep -n "fn redeem_invite_inner\|fn redeem_invite_impl\|fn join_open_community_impl" src/lib.rs` and determine which function contains that tail:
- If it is `redeem_invite_inner` (shared by both the invite-redeem and open-join paths): add ONE hook there, immediately after `force_reachability_republish(state);`:

```rust
// ZEB-613: engage presence for the freshly-joined community now.
// The GUI also subscribes from the frontend; the event loop's
// dup-guard makes the double Subscribe a no-op.
if let Err(e) = subscribe_community_presence_impl(state, dto.community_id.clone()).await {
    tracing::warn!(error = %e, "ZEB-613: post-join presence subscribe failed");
}
```

- If it is `redeem_invite_impl` only: add the same hook there AND at `join_open_community_impl`'s success return (lib.rs:28209ff — insert before its `Ok(..)` with that path's community-id hex variable).

(Field name check: the tail logs `community_id = %dto.community_id` at lib.rs:28105, so `dto.community_id` is the 32-hex id in scope.)

- [ ] **Step 5: Run tests + gates**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(zeb613)'`
Expected: PASS (both tests).

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs
git commit -m "feat(zeb-613): auto-subscribe community presence on serve boot + join"
```

---

### Task 4: ZEB-618a — root-driver parity in `channel_backfill.rs` (presence kick + persisted floor)

**Files:**
- Modify: `src-tauri/src/channel_backfill.rs:969-1059` (`run_root_fetch_driver`) + its doc comment (:955-968)
- Test: same file's `#[cfg(test)] mod tests`
- Modify (mechanical): every existing `run_root_fetch_driver(` call — production sites get real values in Tasks 5-6; **test call sites in this file get `None, None` placeholders in this task**; the two production call sites (event_loop.rs:2726, community_state_sync.rs:4840) get `None, None` in THIS task so the workspace compiles, upgraded in Tasks 5-6.

**Interfaces:**
- Consumes: `ResyncPersist { first_deadline_ms: u64, on_full_reconcile: Arc<dyn Fn(u64) + Send + Sync> }` (:358), `first_resync_deadline` (:394), `epoch_bump` (:414), `resync_tick` (:426), `cooldown_wait` (:435), `MIN_RESYNC_WAKE_MS` (:340). Pattern source: `run_backfill_driver`'s Idle/WaitUntil arms (:596-768).
- Produces: new signature —

```rust
pub async fn run_root_fetch_driver<Rq, RqFut>(
    mut latch: RootFetchLatch,
    request_root: Rq,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    mut epoch_rx: Option<tokio::sync::watch::Receiver<u64>>,
    // ZEB-618: presence-driven reachability kick (ZEB-599 D1 parity
    // with run_backfill_driver). A kick re-arms the latch — for a
    // root fetch, reset() IS the full re-fetch. Cooldown-gated like
    // the epoch arm; sender-drop degrades to None.
    mut full_resync_rx: Option<tokio::sync::watch::Receiver<u64>>,
    resync_interval_ms: Option<u64>,
    now_ms: impl Fn() -> u64,
    // ZEB-618: Some(..) makes the floor restart-aware (ZEB-584 parity):
    // first fire at the persisted absolute deadline, each fire persists.
    resync_persist: Option<ResyncPersist>,
) where ...
```

- [ ] **Step 1: Write the failing tests** (paused-time, mirroring the existing `backfill_persist_floor_*` tests around :2528 — copy their latch/request stub helpers for `RootFetchLatch`):

```rust
/// ZEB-618: a presence kick while Idle re-arms the root latch
/// (cooldown-gated), producing a fresh Request.
#[tokio::test(start_paused = true)]
async fn root_driver_presence_kick_rearms() {
    let (kick_tx, kick_rx) = tokio::sync::watch::channel(0u64);
    let (_shut_tx, shut_rx) = tokio::sync::watch::channel(false);
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let req = requests.clone();
    let request_root = move || {
        let req = req.clone();
        async move {
            req.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            RootFetch::Answered
        }
    };
    let driver = tokio::spawn(run_root_fetch_driver(
        RootFetchLatch::new(),
        request_root,
        shut_rx,
        None,                    // no epoch watch
        Some(kick_rx),           // presence kick wired
        None,                    // floor disabled
        test_now_ms(),           // same injected clock the existing tests use
        None,                    // no persist
    ));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await; // initial Request done, Idle
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    kick_tx.send_modify(|e| *e = e.wrapping_add(1));
    tokio::time::sleep(std::time::Duration::from_millis(
        EPOCH_REARM_COOLDOWN_MS + 1_000,
    )).await;
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2,
        "presence kick must re-arm the root fetch after cooldown");
    driver.abort();
}

/// ZEB-618: with a persist sink, the FIRST floor fire lands at the
/// absolute persisted deadline (not interval-from-spawn), and the fire
/// invokes on_full_reconcile.
#[tokio::test(start_paused = true)]
async fn root_driver_persisted_floor_first_fire_at_deadline() { /* mirror
    backfill_persist_floor_first_fire_at_deadline_then_interval's clock
    setup exactly, adapted to RootFetchLatch + RootFetch::Answered stub:
    persist deadline = now + 5_000 with interval 60_000; assert the 2nd
    Request happens ~5s in (not 60s), and the recorded on_full_reconcile
    stamp equals the fire time. */ }

/// ZEB-618: presence-kick sender drop degrades to None (driver keeps
/// running on the floor), mirroring epoch-sender-drop.
#[tokio::test(start_paused = true)]
async fn root_driver_presence_sender_drop_degrades() { /* drop kick_tx,
    advance past a floor interval, assert a floor-driven Request still
    happens and the driver hasn't returned. */ }
```

(Use the exact stub/clock helpers already present in this test module for `run_root_fetch_driver` — there are existing tests for it; extend their patterns rather than inventing new ones. `test_now_ms()` here stands for whatever injected-clock closure those tests use — copy it.)

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(root_driver_presence) or test(root_driver_persisted)'`
Expected: FAIL — signature mismatch (compile error).

- [ ] **Step 3: Implement**, mirroring `run_backfill_driver` LINE-FOR-LINE where marked:

1. Add the two params as in the Produces block.
2. Top of fn: `let mut resync_deadline: Option<u64> = resync_persist.as_ref().map(|p| p.first_deadline_ms);` plus the started debug log (target `"harmony_channel"`, fields `floor_deadline_ms`, `restart_aware`, message `"root fetch driver started"`).
3. `Idle` arm: extend the early-return guard to `epoch_rx.is_none() && full_resync_rx.is_none() && !matches!(...)`; compute `resync_arg` exactly as :607-617 (absolute deadline when persist wired, legacy interval otherwise); add the presence-kick select arm (copy :639-665, with `latch.reset()` instead of `latch.reset(None)` and log message `"root re-arm: presence kick"`); in the floor arm (`resync_tick(resync_arg)`), after `latch.reset()`, add the persist block (copy :693-705: `on_full_reconcile(fired_at)` + advance `resync_deadline`).
4. `WaitUntil` arm: add the mid-backoff presence-kick select arm (copy :742-759, `latch.reset()`, log `"root re-arm: presence kick mid-backoff"`).
5. Update the fn doc comment: add `full_resync_rx` and `resync_persist` bullets mirroring `run_backfill_driver`'s (:556-568).
6. Fix ALL existing call sites to compile: test call sites in this file and the two production sites (`event_loop.rs:2726-2742`, `community_state_sync.rs:4840-4856`) get `None,` (full_resync_rx, 5th arg) and `None,` (resync_persist, last arg) for now.

- [ ] **Step 4: Run tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(root_driver) or test(root_fetch)'`
Expected: PASS (new tests + all pre-existing root-fetch driver tests).

- [ ] **Step 5: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
git add src/channel_backfill.rs src/event_loop.rs src/community_state_sync.rs
git commit -m "feat(zeb-618): run_root_fetch_driver gains presence-kick + restart-aware floor (parity with run_backfill_driver)"
```

---

### Task 5: ZEB-618b — wire the mail-root driver

**Files:**
- Modify: `src-tauri/src/event_loop.rs` — the event-loop fn's parameter list (add one param next to `presence_resync_tx` at :891) and the mail-root spawn site (:2726-2742)
- Modify: `src-tauri/src/lib.rs` — the event-loop invocation (arg pass-through near :9099) in `start_node_inner`, building the persist pair where `app_data_dir` is in scope
- Test: persistence round-trip is covered by Task 4's driver tests + `ChannelBackfillState` tests; this task's test is the compile-checked wiring plus one integration-leaning unit test if cheap (see Step 3).

**Interfaces:**
- Consumes: `ChannelBackfillState::{load, save}` (community_channel_log.rs:1642/1658 — sidecar `<dir>/backfill_state.cbor`, single `last_full_reconcile_ms: u64`); `first_resync_deadline` + `periodic_resync_interval_ms()` (channel_backfill.rs); `presence_resync_tx: watch::Sender<u64>` already an event-loop param (:891) — `presence_resync_tx.subscribe()` mints the receiver.
- Produces: new event-loop param `mail_resync: Option<(u64, crate::channel_backfill::ResyncPersist)>` — `(interval_ms, persist)` computed by `start_node_inner`.

- [ ] **Step 1: Build the pair in `start_node_inner`** (lib.rs, where `app_data_dir` is in scope before the event-loop spawn; the mail dir precedent is `app_data_dir.join("mail")` at lib.rs:3330):

```rust
// ZEB-618: restart-aware anti-entropy floor for the mail-root fetch
// (ZEB-584 parity). Sidecar: <data>/mail/backfill_state.cbor.
let mail_resync = {
    let mail_dir = app_data_dir.join("mail");
    let interval_ms = crate::channel_backfill::periodic_resync_interval_ms();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = crate::community_channel_log::ChannelBackfillState::load(&mail_dir)
        .map(|s| s.last_full_reconcile_ms);
    let first_deadline_ms =
        crate::channel_backfill::first_resync_deadline(last, interval_ms, now_ms);
    let persist_dir = mail_dir.clone();
    Some((
        interval_ms,
        crate::channel_backfill::ResyncPersist {
            first_deadline_ms,
            on_full_reconcile: std::sync::Arc::new(move |fired_at_ms| {
                let dir = persist_dir.clone();
                // Tiny sidecar write off the driver task (same shape as
                // the channel-log engine's ZEB-599 callback).
                tokio::task::spawn_blocking(move || {
                    if let Err(e) =
                        crate::community_channel_log::ChannelBackfillState::save(&dir, fired_at_ms)
                    {
                        tracing::debug!(error = %e, "mail-root resync persist failed (hint only)");
                    }
                });
            }),
        },
    ))
};
```

(Check `ChannelBackfillState::save`'s exact signature at community_channel_log.rs:1658 — if it takes `(root: &Path, last_full_reconcile_ms: u64)` this matches; adjust field/arg names to what's there. Also confirm `load` returns the struct with field `last_full_reconcile_ms` (struct at :1618). If the mail dir may not exist at boot, `std::fs::create_dir_all(&mail_dir)` before `load` — mirror however lib.rs:3330's consumer handles it.)

- [ ] **Step 2: Thread the param.** Add `mail_resync: Option<(u64, crate::channel_backfill::ResyncPersist)>` to the event-loop fn params (adjacent to `presence_resync_tx`, event_loop.rs:891) and pass it at the lib.rs call site (near :9099). At the mail-root spawn (:2726-2742) replace the two `None` placeholders and the interval arg:

```rust
let (mail_interval_ms, mail_persist) = match &mail_resync {
    Some((ms, p)) => (Some(*ms), Some(p.clone())),
    None => (Some(crate::channel_backfill::periodic_resync_interval_ms()), None),
};
tokio::spawn(crate::channel_backfill::run_root_fetch_driver(
    crate::channel_backfill::RootFetchLatch::new(),
    request_root,
    mail_shutdown_rx,
    epoch_rx_mail,
    // ZEB-618: presence kick — same watch the channel-log drivers get.
    Some(presence_resync_tx.subscribe()),
    mail_interval_ms,
    || { /* unchanged wall-clock closure */ },
    mail_persist,
));
```

(The `presence_resync_tx.subscribe()` must happen BEFORE the sender is moved into the presence task at :3030 — the mail spawn at :2726 precedes it, confirmed. `ResyncPersist` derives `Clone` (:357).)

- [ ] **Step 3: Compile-level verification + targeted tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(mail)'`
Expected: PASS (existing mail tests unaffected). The driver-level behavior is already pinned by Task 4's paused-time tests; wiring is compile-checked. If an existing event-loop harness test constructs the event loop directly, update its arg list with `None`.

- [ ] **Step 4: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
git add src/event_loop.rs src/lib.rs
git commit -m "feat(zeb-618): mail-root fetch driver gets presence kick + persisted floor"
```

---

### Task 6: ZEB-618c — wire the community-root driver

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` — `CommunityRegistryConfig` (+1 field, :3960-4025), `CommunitySyncRegistry` (clone-through), the root-driver spawn inside `spawn_engine_inner_now` (:4840-4856)
- Modify: `src-tauri/src/lib.rs` — the three `CommunityRegistryConfig { .. }` construction sites (lib.rs:5103, :24051, :26092) + community_state_sync.rs:5395 (in-crate test/helper site)
- Test: `src-tauri/src/community_state_sync.rs` config-derivation unit test

**Interfaces:**
- Consumes: `cfg.identity_dir: PathBuf` — per-community dirs are `identity_dir/communities/{id_hex}/` (doc at :3971-3974); `presence_resync_rx` origin: `lib.rs:3612` (`watch::channel(0u64)`; watch receivers are `Clone`); `ChannelBackfillState` sidecar (same as Task 5).
- Produces: `CommunityRegistryConfig.presence_resync_rx: Option<tokio::sync::watch::Receiver<u64>>` — registry-level, zero churn at the five `spawn_engine*` call sites.

- [ ] **Step 1: Write the failing test** (community_state_sync.rs tests):

```rust
/// ZEB-618: the per-community resync sidecar path derives from
/// identity_dir exactly like PersistPaths does.
#[test]
fn community_root_resync_dir_matches_engine_layout() {
    let dir = community_root_resync_dir(std::path::Path::new("/tmp/idroot"), &space_id_fixture());
    assert_eq!(
        dir,
        std::path::PathBuf::from(format!(
            "/tmp/idroot/communities/{}",
            hex::encode(space_id_fixture().0)
        ))
    );
}
```

(Use whatever `SpaceId` fixture helper the existing tests in this file use.)

- [ ] **Step 2: Run to verify failure** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_root_resync_dir)'` → FAIL (fn missing).

- [ ] **Step 3: Implement.**

(a) Field on `CommunityRegistryConfig`:

```rust
/// ZEB-618: presence-driven reachability kick for each engine's
/// root-fetch driver (ZEB-599 D1 parity with the channel-log
/// drivers). Cloned per spawned driver. `None` for tests/callers
/// without presence.
pub presence_resync_rx: Option<tokio::sync::watch::Receiver<u64>>,
```

(b) Path helper (free fn in community_state_sync.rs):

```rust
/// ZEB-618: sidecar dir for a community's root-fetch resync stamp —
/// the community's own engine dir (same layout PersistPaths derives).
fn community_root_resync_dir(identity_dir: &std::path::Path, id: &SpaceId) -> std::path::PathBuf {
    identity_dir.join("communities").join(hex::encode(id.0))
}
```

(c) In `spawn_engine_inner_now`, at the root-driver spawn (:4840): build interval/deadline/persist from the sidecar (same code shape as Task 5 Step 1, with `dir = community_root_resync_dir(&self.cfg.identity_dir, &community_id)`), pass `self.cfg.presence_resync_rx.clone()` as the kick arg, the computed interval, and `Some(persist)`. Load the stamp with `ChannelBackfillState::load_async(&dir).await` (async context available) before the spawn.

(d) Construction sites: add `presence_resync_rx: Some(presence_resync_rx.clone())` at lib.rs:5103 (start_node_inner — the channel from lib.rs:3612 is in scope; verify variable name there) and `presence_resync_rx: None` at lib.rs:24051, lib.rs:26092, community_state_sync.rs:5395 UNLESS the presence receiver is reachable in those scopes — check each; wire it wherever available, `None` otherwise with a `// ZEB-618: no presence source in this path` comment.

- [ ] **Step 4: Run tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_root_resync_dir) or test(spawn_engine)'`
Expected: PASS.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
git add src/community_state_sync.rs src/lib.rs
git commit -m "feat(zeb-618): community-root fetch drivers get presence kick + persisted floor"
```

---

### Task 7: Full gates sweep + PR

- [ ] **Step 1: Full clippy** — `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` → clean. (This is the ~50min relink; run once, here.)
- [ ] **Step 2: Full tests** — `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` → all green.
- [ ] **Step 3: fmt check** — `cargo fmt --all -- --check` → clean.
- [ ] **Step 4: Frontend gates** (nothing touched, but CI runs them) — from repo root: `npx tsc --noEmit && npx vitest run` → green.
- [ ] **Step 5: ZEB-617 live check** — launch once, confirm `homeRelayUrl` is `*.relay.n0.iroh.link` (see Task 2 note; if it fails, revert Task 2's commit and note on ZEB-617).
- [ ] **Step 6: Push + open PR** titled `ZEB-617/613/618: Phase 3 small slices — stable relays, headless presence, root-driver parity`, body listing all three tickets (remember: Linear will auto-close every ZEB-NNN in the body on merge — reference ONLY ZEB-617/613/618 by ID; write other tickets as plain text like "Phase 3 umbrella" to avoid cascade-closing them). One push, then converge on CI + bots.

## Self-review notes (done at authoring)

- Spec coverage: ZEB-617 (Task 2), ZEB-613 both hooks (Task 3), ZEB-618 driver+both call sites (Tasks 4-6), decision-record doc (Task 1). ✓
- The `presence_resync_tx.subscribe()`-before-move ordering (Task 5) verified against event_loop.rs:2726 < :3030. ✓
- Known look-up-at-implementation points are explicit greps with decision rules (Task 3 Step 4b; Task 6 Step 3d), not placeholders.
- PR-body cascade guard included (Task 7 Step 6).
