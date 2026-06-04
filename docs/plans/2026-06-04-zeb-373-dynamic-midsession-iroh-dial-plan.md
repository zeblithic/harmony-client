# ZEB-373 — Dynamic mid-session iroh→Zenoh dial — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Dial iroh peers discovered *strictly mid-session* into the live Zenoh session, so cross-WAN CRDT sync reaches peers that appear after `zenoh::open` and never dial us.

**Architecture:** Swap the terminal `zenoh::open(config)` for the `internal`-feature `RuntimeBuilder` + `session::init` path so we retain a `Runtime` handle. Add a notify seam to `ReachabilityResolver::update()` that fires a `DialHint` the first time a `(owner, node_id)` is learned. A `DynamicDialDriver` task consumes hints, dedups by node-id, and dials through a `PeerDialer` (production = `runtime.connect_peer(fresh_zid, &[iroh_locator])`) with bounded backoff. Dial activity is recorded in a `DialTelemetry` surfaced on `NetworkHealthSnapshot`.

**Tech Stack:** Rust, Tauri, zenoh 1.9.0 (`internal` feature), iroh, tokio, the vendored `zenoh-link` fork (ZEB-368).

**Spec:** `docs/specs/2026-06-04-zeb-373-dynamic-midsession-iroh-dial-design.md`

**Scope boundary:** dial-once per node-id per session; failed dials retried-then-re-armable; **no** re-dial on transport drop (that is ZEB-321 Phase 3). Telemetry is in scope.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src-tauri/Cargo.toml` | deps | add `features = ["internal"]` to `zenoh` |
| `src-tauri/src/event_loop.rs` | session creation + driver spawn | swap `zenoh::open`→RuntimeBuilder; create DialHint channel; install resolver sender; spawn driver |
| `src-tauri/src/reachability_resolver.rs` | reachability map + notify seam | `DialHint`, `dial_hint_tx` field + setter, emit-on-newly-active |
| `src-tauri/src/iroh_dial_driver.rs` | **new** — dial driver + dialer trait | `PeerDialer`, `DynamicDialDriver`/`run_dial_driver`, `RuntimePeerDialer` |
| `src-tauri/src/network_health.rs` | health snapshot | `DialTelemetry`, `DynamicDialHit`, `DialHealthSummary`, `DialSnapshot` trait, snapshot field |
| `src-tauri/src/lib.rs` | boot wiring + IPC | `NodeState.dial_telemetry`; populate/clear; thread into `event_loop::run`; `ProdDialSnapshot` into service |
| `src-tauri/tests/zeb_373_dynamic_dial_integration.rs` | **new** — acceptance test | one real Runtime A dials a bare manager B mid-session |

## Verified zenoh-1.9.0 API anchors (internal feature)

These were read from `~/.cargo/registry/src/.../zenoh-1.9.0`. They are **internal** APIs — if the compiler disagrees on an exact path/signature, adjust imports but keep the shape:

- `zenoh::internal::runtime::{Runtime, RuntimeBuilder}` (lib.rs:1066, gated by `feature="internal"`).
- `RuntimeBuilder::new(config: Config) -> Self` (net/runtime/mod.rs:561); `pub async fn build(self) -> ZResult<Runtime>` (mod.rs:602).
- `Runtime::start(&mut self)` (net/runtime/orchestrator.rs:125) — needs `let mut runtime`.
- `zenoh::session::init(runtime: impl Into<GenericRuntime>)` exposed under `#[zenoh_macros::internal]` (lib.rs:390). Call `zenoh::session::init(runtime.clone().into()).await?`.
- `Runtime::connect_peer(&self, zid: &ZenohIdProto, locators: &[Locator]) -> bool` (orchestrator.rs:1052) — the **un-filtered** dial path; pass the iroh locator here, never in `connect/endpoints`.
- `ZenohIdProto` and `Locator` come from `zenoh_protocol::core` (already a direct dep). Use `ZenohIdProto::rand()` for the fresh placeholder per dial; `Locator::from_str("iroh/<hex>")` (or `.try_from`/`FromStr`) for the locator. Verify the exact constructor against the crate at build time.

## Per-task gate commands

Run from `src-tauri/`. Per-task (fast, lib-only — avoids relinking ~97 integration binaries):

```bash
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```

Integration-test tasks (T6) additionally run that one test target with `--test zeb_373_dynamic_dial_integration`. The **final sweep** (T7) runs the full `--all-targets` gate.

---

## Task 1: zenoh `internal` feature + RuntimeBuilder session swap (RISK GATE)

This is the highest-risk change: it reroutes the session-creation path ZEB-368 shipped. Get it to parity (no behavior change, all existing tests green) **before** building anything else.

**Files:**
- Modify: `src-tauri/Cargo.toml:34`
- Modify: `src-tauri/src/event_loop.rs:651` (the `zenoh::open` call site)

- [ ] **Step 1: Enable the `internal` feature**

In `src-tauri/Cargo.toml`, change line 34 from:
```toml
zenoh = "1"
```
to:
```toml
zenoh = { version = "1", features = ["internal"] }
```

- [ ] **Step 2: Confirm the feature compiles and the internal paths resolve**

Run: `cargo build --locked -p harmony-app`
Expected: builds clean. If `zenoh::internal::runtime` is not visible, the feature name is wrong — re-check `zenoh-1.9.0/Cargo.toml` (`internal = [ ... ]` at line 79).

- [ ] **Step 3: Replace `zenoh::open` with the RuntimeBuilder path**

In `event_loop.rs`, the current site (~line 651) is:
```rust
let session = match cancellable!(zenoh::open(config), "zenoh::open") {
    Ok(s) => s,
    Err(e) => {
        let e = format!("zenoh open failed: {e}");
        let _ = ready_tx.send(Err(e.clone()));
        let _ = app.emit("zenoh-status", &crate::ZenohStatus {
            status: "error".to_string(), endpoint: None, error: Some(e),
        });
        return;
    }
};
```

Replace with a helper that builds the runtime, starts it, and inits the session — keeping the exact same `cancellable!` cancellation and the same error emit. Define this free fn at the bottom of `event_loop.rs`:

```rust
/// ZEB-373: build a started zenoh Runtime + Session from `config`, returning the
/// Runtime handle (for dynamic `connect_peer` dialing) alongside the Session.
/// Replaces `zenoh::open(config)` — the `internal` feature exposes
/// `RuntimeBuilder` + `session::init`, which `zenoh::open` uses under the hood.
async fn open_session_with_runtime(
    config: zenoh::Config,
) -> zenoh::Result<(zenoh::internal::runtime::Runtime, zenoh::Session)> {
    let mut runtime = zenoh::internal::runtime::RuntimeBuilder::new(config)
        .build()
        .await?;
    runtime.start().await?;
    let session = zenoh::session::init(runtime.clone().into()).await?;
    Ok((runtime, session))
}
```

And the call site becomes:
```rust
let (zenoh_runtime, session) =
    match cancellable!(open_session_with_runtime(config), "zenoh::open") {
        Ok(pair) => pair,
        Err(e) => {
            let e = format!("zenoh open failed: {e}");
            let _ = ready_tx.send(Err(e.clone()));
            let _ = app.emit("zenoh-status", &crate::ZenohStatus {
                status: "error".to_string(), endpoint: None, error: Some(e),
            });
            return;
        }
    };
```

Leave `zenoh_runtime` unused for now (prefix `_zenoh_runtime` to silence the warning, OR add `let _ = &zenoh_runtime;`). It is consumed in Task 5.

- [ ] **Step 4: Build + clippy + parity test**

Run: `cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

Run the iroh/zenoh integration tests that exercise the session path (these are the parity proof — the swap must not regress ZEB-368):
```bash
cargo nextest run --locked --features test-fixtures \
  --test community_reachability_two_engine_integration \
  -E 'test(zenoh_iroh) + test(reachability) + test(zeb_321)'
```
Expected: PASS (the known iroh/zenoh loopback flakes may need a single rerun — they are CPU-contention flakes, not regressions; confirm by re-running the failing test in isolation).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/event_loop.rs
git commit -m "feat(zeb-373): swap zenoh::open for internal RuntimeBuilder path (retain Runtime)"
```

---

## Task 2: `DialHint` type + resolver notify seam

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (struct ~42-48, `new` ~81, `update` ~90-100)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `reachability_resolver.rs`:

```rust
#[test]
fn dial_hint_fires_once_on_first_learn() {
    let r = ReachabilityResolver::new();
    let (tx, rx) = std::sync::mpsc::channel();
    r.set_dial_hint_sender_blocking_for_test(tx); // see note below
    let owner = [0xAA; 16];
    let mut payload = sample_payload();
    payload.iroh_node_id = [0x11; 32];
    let hlc = sample_hlc(1);
    r.update(owner, payload.clone(), hlc.clone());
    let hint = rx.try_recv().expect("hint on first learn");
    assert_eq!(hint.node_id, [0x11; 32]);
    assert_eq!(hint.owner, owner);
    // A newer hlc for the SAME (owner,node_id) replaces but is not newly-active.
    let hlc2 = sample_hlc(2);
    r.update(owner, payload, hlc2);
    assert!(rx.try_recv().is_err(), "no hint on hlc-replace of known peer");
}

#[test]
fn dial_hint_silent_when_sender_unset() {
    let r = ReachabilityResolver::new();
    // no sender installed
    r.update([0xAA; 16], sample_payload(), sample_hlc(1)); // must not panic
}
```

> The production setter takes a `tokio::sync::mpsc::UnboundedSender<DialHint>`. For the unit test, provide a tiny test-only setter variant that accepts a `std::sync::mpsc::Sender` OR (preferred) make the production setter generic-free by testing the async sender with `tokio::sync::mpsc::unbounded_channel()` and `try_recv()`. Use the tokio channel to match production:

Rewrite the test channel lines as:
```rust
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
r.set_dial_hint_sender(tx);
// ... and replace rx.try_recv() with rx.try_recv() (tokio UnboundedReceiver has try_recv)
```
Make both tests `#[test]` (no runtime needed — `unbounded_channel` + `try_recv` are sync). Drop the `_blocking_for_test` note; use `set_dial_hint_sender`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(dial_hint)'`
Expected: FAIL — `DialHint`, `set_dial_hint_sender` do not exist.

- [ ] **Step 3: Add `DialHint`, the sender field + setter, and the emit**

Add the type near the top of `reachability_resolver.rs` (after imports):
```rust
/// ZEB-373: emitted the first time the resolver learns a `(owner, node_id)`, so the
/// dynamic dial driver can dial a peer discovered strictly mid-session. Dedup of a
/// node-id seen under multiple owners is the driver's responsibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialHint {
    pub node_id: [u8; 32],
    pub owner: OwnerAddr,
}
```

Add a field to the struct (it currently has `inner` + `fallback_source`):
```rust
pub struct ReachabilityResolver {
    inner: Arc<RwLock<BTreeMap<ResolverKey, ResolverEntry>>>,
    fallback_source: Arc<RwLock<Option<Arc<dyn ReachabilityFallback>>>>,
    // ZEB-373: optional notify seam to the dynamic dial driver. None until boot
    // installs it; behind Option so every existing caller/test is unaffected.
    dial_hint_tx: Arc<RwLock<Option<tokio::sync::mpsc::UnboundedSender<DialHint>>>>,
}
```
Update `#[derive(Default)]`/`Default` impl and any manual `new`/field construction to initialize `dial_hint_tx: Arc::new(RwLock::new(None))`. (If the struct uses `#[derive(Default)]`, the new `Arc<RwLock<Option<_>>>` derives Default automatically.)

Add the setter in `impl ReachabilityResolver`:
```rust
/// Install the dial-hint sender. Called once at boot (event_loop) after the
/// dynamic dial driver's channel is created.
pub fn set_dial_hint_sender(&self, tx: tokio::sync::mpsc::UnboundedSender<DialHint>) {
    *self.dial_hint_tx.write().expect("dial hint tx lock") = Some(tx);
}
```

Modify `update()` to emit only when the key is newly inserted:
```rust
pub fn update(&self, actor: OwnerAddr, payload: ReachabilityAnnouncePayload, hlc: Hlc) {
    let key: ResolverKey = (actor, payload.iroh_node_id);
    let node_id = payload.iroh_node_id;
    let mut map = self.inner.write().expect("resolver write lock");
    let was_present = map.contains_key(&key);
    let next = ResolverEntry { payload, hlc };
    match map.get(&key) {
        Some(prev) if !should_replace(prev, &next) => { /* keep prev */ }
        _ => {
            map.insert(key, next);
        }
    }
    drop(map);
    // ZEB-373: notify the dial driver the FIRST time we learn this (owner,node_id).
    if !was_present {
        if let Some(tx) = self.dial_hint_tx.read().expect("dial hint tx lock").as_ref() {
            let _ = tx.send(DialHint { node_id, owner: actor });
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(dial_hint)'`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/reachability_resolver.rs
git commit -m "feat(zeb-373): DialHint + resolver notify seam (emit on first-learn)"
```

---

## Task 3: `DialTelemetry` + Network Health surfacing

**Files:**
- Modify: `src-tauri/src/network_health.rs`

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `network_health.rs`:
```rust
#[test]
fn dial_telemetry_counts_and_rings() {
    let t = DialTelemetry::new();
    t.record_attempt();
    t.record_succeeded([0x11; 32], [0xAA; 16]);
    t.record_attempt();
    t.record_failed([0x22; 32], [0xBB; 16]);
    t.record_skipped_duplicate();
    let s = t.summary();
    assert_eq!(s.attempts, 2);
    assert_eq!(s.succeeded, 1);
    assert_eq!(s.failed, 1);
    assert_eq!(s.skipped_duplicate, 1);
    assert_eq!(s.recent.len(), 2); // success + failure recorded in the ring
    assert!(s.recent.iter().any(|h| h.outcome == "succeeded"));
    assert!(s.recent.iter().any(|h| h.outcome == "failed"));
}

#[test]
fn empty_snapshot_has_zeroed_dial_status() {
    let snap = NetworkHealthSnapshot::empty();
    assert_eq!(snap.dial_status.attempts, 0);
    assert!(snap.dial_status.recent.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(dial_telemetry) + test(zeroed_dial)'`
Expected: FAIL — `DialTelemetry`, `dial_status` do not exist.

- [ ] **Step 3: Add the telemetry types + snapshot field**

Add to `network_health.rs`:
```rust
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// One recorded dial outcome for the Network Health panel (ZEB-373).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicDialHit {
    pub node_id_short: String,
    pub owner_short: String,
    pub outcome: String, // "succeeded" | "failed"
    pub captured_at_ms: u64,
}

/// Snapshot of dynamic-dial activity surfaced on `NetworkHealthSnapshot`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DialHealthSummary {
    pub attempts: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub skipped_duplicate: u64,
    pub recent: Vec<DynamicDialHit>,
}

const DIAL_RING_CAP: usize = 32;

/// Process-lifetime counters + a bounded ring of recent dial outcomes. Shared
/// (`Arc`) between the dial driver (writer) and `network_health_snapshot` (reader).
#[derive(Debug, Default)]
pub struct DialTelemetry {
    attempts: AtomicU64,
    succeeded: AtomicU64,
    failed: AtomicU64,
    skipped_duplicate: AtomicU64,
    recent: Mutex<VecDeque<DynamicDialHit>>,
}

impl DialTelemetry {
    pub fn new() -> Self { Self::default() }
    pub fn record_attempt(&self) { self.attempts.fetch_add(1, Ordering::Relaxed); }
    pub fn record_skipped_duplicate(&self) { self.skipped_duplicate.fetch_add(1, Ordering::Relaxed); }
    pub fn record_succeeded(&self, node_id: [u8; 32], owner: [u8; 16]) {
        self.succeeded.fetch_add(1, Ordering::Relaxed);
        self.push(node_id, owner, "succeeded");
    }
    pub fn record_failed(&self, node_id: [u8; 32], owner: [u8; 16]) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        self.push(node_id, owner, "failed");
    }
    fn push(&self, node_id: [u8; 32], owner: [u8; 16], outcome: &str) {
        let hit = DynamicDialHit {
            node_id_short: hex::encode(&node_id[..4]),
            owner_short: hex::encode(&owner[..4]),
            outcome: outcome.to_string(),
            captured_at_ms: now_ms(),
        };
        let mut ring = self.recent.lock().expect("dial ring lock");
        if ring.len() == DIAL_RING_CAP { ring.pop_front(); }
        ring.push_back(hit);
    }
    pub fn summary(&self) -> DialHealthSummary {
        DialHealthSummary {
            attempts: self.attempts.load(Ordering::Relaxed),
            succeeded: self.succeeded.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            skipped_duplicate: self.skipped_duplicate.load(Ordering::Relaxed),
            recent: self.recent.lock().expect("dial ring lock").iter().cloned().collect(),
        }
    }
}
```

Add `pub dial_status: DialHealthSummary` to `NetworkHealthSnapshot` (after `pkarr_status`). In `NetworkHealthSnapshot::empty()`, add `dial_status: DialHealthSummary::default(),`. Bump `schema_version` to `2` in both `empty()` and the main `snapshot()` builder (the panel is additive-tolerant, but bumping is correct).

> `now_ms()` is the existing helper used by `empty()`. Confirm it is in scope in this module (it is — `empty()` already calls it).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(dial_telemetry) + test(zeroed_dial)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/network_health.rs
git commit -m "feat(zeb-373): DialTelemetry + DialHealthSummary on NetworkHealthSnapshot"
```

---

## Task 4: `PeerDialer` trait + `DynamicDialDriver` (mock-tested)

The driver is unit-tested with a mock `PeerDialer` — no real zenoh/iroh, no flakes. Backoff delay is injectable so tests run instantly.

**Files:**
- Create: `src-tauri/src/iroh_dial_driver.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod iroh_dial_driver;` near the other `mod` decls)

- [ ] **Step 1: Create the module with the trait, driver, and failing tests**

Create `src-tauri/src/iroh_dial_driver.rs`:
```rust
//! ZEB-373: dynamic mid-session iroh dial driver. Consumes `DialHint`s from the
//! resolver notify seam, dedups by node-id, and dials each newly-learned peer once
//! through a `PeerDialer` with bounded backoff. Re-dial on transport drop is out of
//! scope (ZEB-321 Phase 3).
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::network_health::DialTelemetry;
use crate::reachability_resolver::DialHint;

/// Abstraction over "dial this iroh peer". Production wraps a zenoh `Runtime`
/// (`connect_peer`); tests use a mock. `locator` is `iroh/<hex>`.
#[async_trait::async_trait]
pub trait PeerDialer: Send + Sync {
    async fn dial(&self, node_id: [u8; 32], locator: String) -> bool;
}

fn iroh_locator(node_id: &[u8; 32]) -> String {
    format!("iroh/{}", hex::encode(node_id))
}

/// Run the dial driver until the hint channel closes (node stop drops the sender).
/// `backoff_base` is the first retry delay (doubles each retry); tests pass ZERO.
pub async fn run_dial_driver(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<DialHint>,
    dialer: Arc<dyn PeerDialer>,
    telemetry: Arc<DialTelemetry>,
    self_node_id: [u8; 32],
    backoff_base: Duration,
) {
    let dialed: Arc<Mutex<HashSet<[u8; 32]>>> = Arc::new(Mutex::new(HashSet::new()));
    while let Some(hint) = rx.recv().await {
        if hint.node_id == self_node_id {
            tracing::debug!("ZEB-373: skip dial to self");
            continue;
        }
        {
            let mut d = dialed.lock().expect("dialed set lock");
            if !d.insert(hint.node_id) {
                telemetry.record_skipped_duplicate();
                continue;
            }
        }
        let dialer = Arc::clone(&dialer);
        let telemetry = Arc::clone(&telemetry);
        let dialed = Arc::clone(&dialed);
        tokio::spawn(async move {
            telemetry.record_attempt();
            let loc = iroh_locator(&hint.node_id);
            let mut ok = dialer.dial(hint.node_id, loc.clone()).await;
            let mut delay = backoff_base;
            let mut attempts = 1u32;
            while !ok && attempts < 3 {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                delay = delay.saturating_mul(2);
                ok = dialer.dial(hint.node_id, loc.clone()).await;
                attempts += 1;
            }
            if ok {
                telemetry.record_succeeded(hint.node_id, hint.owner);
                tracing::info!("ZEB-373: dialed iroh peer {}", hex::encode(&hint.node_id[..4]));
            } else {
                telemetry.record_failed(hint.node_id, hint.owner);
                // Re-arm: a later announce for this peer can trigger a fresh dial.
                dialed.lock().expect("dialed set lock").remove(&hint.node_id);
                tracing::warn!("ZEB-373: dial failed (3 attempts) for {}", hex::encode(&hint.node_id[..4]));
            }
        });
    }
    tracing::debug!("ZEB-373: dial driver stopping (hint channel closed)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockDialer {
        calls: AtomicU32,
        fail_first_n: u32, // fail this many calls, then succeed
    }
    #[async_trait::async_trait]
    impl PeerDialer for MockDialer {
        async fn dial(&self, _node_id: [u8; 32], _locator: String) -> bool {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            n >= self.fail_first_n
        }
    }

    fn hint(node_id: u8) -> DialHint {
        DialHint { node_id: [node_id; 32], owner: [0xAA; 16] }
    }

    #[tokio::test]
    async fn dials_new_peer_once_and_skips_self_and_duplicates() {
        let dialer = Arc::new(MockDialer { calls: AtomicU32::new(0), fail_first_n: 0 });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let self_id = [0xEE; 32];
        let driver = tokio::spawn(run_dial_driver(
            rx, dialer.clone(), telemetry.clone(), self_id, Duration::ZERO,
        ));
        tx.send(hint(0x11)).unwrap();          // new → dial
        tx.send(hint(0x11)).unwrap();          // dup → skip
        tx.send(DialHint { node_id: self_id, owner: [0xAA; 16] }).unwrap(); // self → skip
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.attempts, 1, "one real dial attempt");
        assert_eq!(s.succeeded, 1);
        assert_eq!(s.skipped_duplicate, 1);
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_then_succeeds_within_three_attempts() {
        let dialer = Arc::new(MockDialer { calls: AtomicU32::new(0), fail_first_n: 2 });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = tokio::spawn(run_dial_driver(
            rx, dialer.clone(), telemetry.clone(), [0xEE; 32], Duration::ZERO,
        ));
        tx.send(hint(0x22)).unwrap();
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.succeeded, 1);
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 3, "3 attempts: fail,fail,succeed");
    }

    #[tokio::test]
    async fn exhausted_failure_rearms_for_redial() {
        let dialer = Arc::new(MockDialer { calls: AtomicU32::new(0), fail_first_n: 100 });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = tokio::spawn(run_dial_driver(
            rx, dialer.clone(), telemetry.clone(), [0xEE; 32], Duration::ZERO,
        ));
        tx.send(hint(0x33)).unwrap(); // 3 attempts → fail → re-arm
        // a small yield so the first dial task finishes before the 2nd hint
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(hint(0x33)).unwrap(); // re-armed → 3 more attempts → fail
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.failed, 2, "both rounds recorded a terminal failure");
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 6, "3 + 3 attempts");
    }
}
```

Add the module declaration in `lib.rs` alongside the other `pub mod`/`mod` lines (e.g. near `pub mod iroh_zenoh_registration;`):
```rust
pub mod iroh_dial_driver;
```

- [ ] **Step 2: Run tests to verify they fail then pass**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(dials_new_peer) + test(retries_then) + test(exhausted_failure)'`
Expected: PASS after the module is added (TDD: they fail to compile before the module exists; this step writes both test and impl together, so confirm GREEN).

> If `async_trait` is not yet a dependency, add `async-trait = "0.1"` under `[dependencies]` in `src-tauri/Cargo.toml` (it is already transitively present via zenoh; prefer adding the direct dep). Verify with `cargo build`.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/iroh_dial_driver.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(zeb-373): PeerDialer trait + DynamicDialDriver (dedup + backoff, mock-tested)"
```

---

## Task 5: Production wiring — RuntimePeerDialer, driver spawn, NodeState, teardown

Wire the pieces into the live boot path. No new test logic (the driver/telemetry/seam are already tested); this is integration. Verify by build + lib tests + a manual reasoning check that the snapshot reads the same telemetry the driver writes.

**Files:**
- Modify: `src-tauri/src/iroh_dial_driver.rs` (add `RuntimePeerDialer`)
- Modify: `src-tauri/src/event_loop.rs` (channel + sender install + driver spawn; thread `dial_telemetry` param)
- Modify: `src-tauri/src/lib.rs` (`NodeState.dial_telemetry`; populate/clear; pass to `run`; `ProdDialSnapshot`)
- Modify: `src-tauri/src/network_health.rs` (`DialSnapshot` trait + service field + `ProdDialSnapshot`)

- [ ] **Step 1: Add `RuntimePeerDialer`**

In `iroh_dial_driver.rs`:
```rust
use zenoh::internal::runtime::Runtime;
use zenoh_protocol::core::{Locator, ZenohIdProto};

/// Production `PeerDialer`: dials through the live zenoh `Runtime` via the
/// un-filtered `connect_peer` path. The placeholder zid is FRESH per dial (zenoh
/// uses it only for pre-dial dedup; the real peer zid is negotiated on the wire).
pub struct RuntimePeerDialer {
    runtime: Runtime,
}
impl RuntimePeerDialer {
    pub fn new(runtime: Runtime) -> Self { Self { runtime } }
}
#[async_trait::async_trait]
impl PeerDialer for RuntimePeerDialer {
    async fn dial(&self, _node_id: [u8; 32], locator: String) -> bool {
        let loc = match locator.parse::<Locator>() {
            Ok(l) => l,
            Err(e) => { tracing::warn!("ZEB-373: bad iroh locator {locator}: {e}"); return false; }
        };
        let placeholder = ZenohIdProto::rand();
        self.runtime.connect_peer(&placeholder, &[loc]).await
    }
}
```
> Verify `Locator: FromStr` and `ZenohIdProto::rand()` exist in `zenoh_protocol::core` at build time; if `rand()` is named differently, use the crate's random-id constructor. `connect_peer` returns `bool`.

- [ ] **Step 2: Add `DialSnapshot` trait + service field + `ProdDialSnapshot` (network_health.rs)**

```rust
/// Source of dynamic-dial telemetry for the snapshot. Mirrors the existing
/// `PkarrSnapshot`/`IrohSnapshot` source-trait pattern.
pub trait DialSnapshot: Send + Sync {
    fn dial_summary(&self) -> DialHealthSummary;
}

/// Production source: reads the shared `DialTelemetry`.
pub struct ProdDialSnapshot {
    pub telemetry: std::sync::Arc<DialTelemetry>,
}
impl DialSnapshot for ProdDialSnapshot {
    fn dial_summary(&self) -> DialHealthSummary { self.telemetry.summary() }
}
```
Add a field `dial: Arc<dyn DialSnapshot>` to `NetworkHealthService`, set it in the service constructor, and in `NetworkHealthService::snapshot()` set `dial_status: self.dial.dial_summary()` on the returned snapshot. For any test constructor of the service, pass a trivial double:
```rust
#[cfg(test)]
pub struct EmptyDialSnapshot;
#[cfg(test)]
impl DialSnapshot for EmptyDialSnapshot {
    fn dial_summary(&self) -> DialHealthSummary { DialHealthSummary::default() }
}
```
> Find every `NetworkHealthService::new(...)` / struct-literal construction (production in `lib.rs` `start_node_inner`, plus any in `network_health.rs` tests) and thread the new `dial` source through. Production passes `Arc::new(ProdDialSnapshot { telemetry: dial_telemetry.clone() })`.

- [ ] **Step 3: Add `dial_telemetry` to `NodeState` + populate + clear (lib.rs)**

- Add field near `reachability_resolver` (~line 739):
  ```rust
  pub dial_telemetry: Option<std::sync::Arc<crate::network_health::DialTelemetry>>,
  ```
- In `impl Default for NodeState` (~line 1015): `dial_telemetry: None,`
- In `start_node_inner` where the iroh/resolver handles are populated, create the telemetry and store it BEFORE building the `NetworkHealthService`, so the service's `ProdDialSnapshot` shares it:
  ```rust
  let dial_telemetry = std::sync::Arc::new(crate::network_health::DialTelemetry::new());
  state.dial_telemetry = Some(dial_telemetry.clone());
  ```
  (Use whatever the local owner-state variable is named — mirror how `reachability_resolver`/`network_health` are assigned nearby.)
- In `clear_iroh_handles` (~line 854, the ZEB-368 ctx-clear block): add `self.dial_telemetry = None;`

- [ ] **Step 4: Thread `dial_telemetry` into `event_loop::run` and spawn the driver**

- Add a parameter to `event_loop::run` (mirror the `iroh_handles: Option<IrohRuntimeHandles>` parameter):
  ```rust
  dial_telemetry: Option<std::sync::Arc<crate::network_health::DialTelemetry>>,
  ```
  Update the single call site in `lib.rs` (`start_node_inner` spawns `event_loop::run(...)`) to pass `dial_telemetry.clone()` (the `Some(_)` you stored in Step 3).
- After the session is created (Task 1's `(zenoh_runtime, session)`), and after the iroh ctx is in place, spawn the driver when both iroh + telemetry are present:
  ```rust
  // ZEB-373: dynamic mid-session dial. Create the hint channel, install the sender
  // on the resolver, and spawn the driver to dial newly-learned peers via the live
  // zenoh Runtime. Inbound + static-seed (ZEB-368) are unchanged.
  if let (Some(ref ih), Some(ref telemetry)) = (&iroh_handles, &dial_telemetry) {
      let (hint_tx, hint_rx) = tokio::sync::mpsc::unbounded_channel();
      ih.link_manager.resolver().set_dial_hint_sender(hint_tx);
      let self_nid = *ih.endpoint.node_id().as_bytes();
      let dialer = std::sync::Arc::new(
          crate::iroh_dial_driver::RuntimePeerDialer::new(zenoh_runtime.clone()),
      );
      tokio::spawn(crate::iroh_dial_driver::run_dial_driver(
          hint_rx,
          dialer,
          std::sync::Arc::clone(telemetry),
          self_nid,
          std::time::Duration::from_secs(1),
      ));
  }
  ```
  > Confirm `ih.link_manager.resolver()` returns the same `ReachabilityResolver` the production `update()` calls mutate (it does — it is the shared Arc-backed store, per ZEB-368 boot-ordering). The driver task ends when `hint_tx` is dropped; it is owned by the resolver's `dial_hint_tx`, which `clear_iroh_handles` clears on stop. Drop `zenoh_runtime`'s earlier `_`-prefix from Task 1 now that it is used.

- [ ] **Step 5: Build, clippy, lib tests**

Run:
```bash
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: clean + all lib tests pass (including Task 2-4 tests and existing network_health tests with the new `dial` source threaded through).

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(zeb-373): wire RuntimePeerDialer + driver + DialTelemetry into boot/teardown/health"
```

---

## Task 6: Acceptance integration test — mid-session dial via a real Runtime

Proves the new path end to end at the link layer: one real Runtime A (built via Task 1's path, with the iroh factory + ctx) learns peer B **mid-session** via a resolver update and dials it through `connect_peer`, and B's accept loop receives the inbound iroh link. (Two full Runtimes in one process is infeasible — the iroh session ctx is a process-global singleton — so B is a bare `IrohZenohLinkManager` + accept loop, exactly as the existing `community_reachability_two_engine_integration` test asserts at the link layer.)

**Files:**
- Create: `src-tauri/tests/zeb_373_dynamic_dial_integration.rs`

- [ ] **Step 1: Write the test (mirror `community_reachability_two_engine_integration.rs`)**

Create `src-tauri/tests/zeb_373_dynamic_dial_integration.rs`. Mirror the existing test's imports, `build_hermetic_endpoint`, payload construction, and manager/accept-loop setup (read that file for the exact helpers). Structure:

```rust
// ZEB-373: dynamic mid-session dial. A (a real zenoh Runtime built via the
// internal RuntimeBuilder path, with the iroh factory + per-session ctx) starts
// with an EMPTY resolver, then learns B mid-session and dials it via the dial
// driver. Assert B's accept loop receives the inbound iroh link.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mid_session_dial_connects_to_a_peer_learned_after_open() {
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;
    tokio::time::timeout(std::time::Duration::from_secs(60), inner())
        .await
        .expect("mid_session_dial must complete within 60s");
}

async fn inner() {
    // 1. Endpoints.
    let ep_a = build_hermetic_endpoint().await;
    let ep_b = build_hermetic_endpoint().await;

    // 2. B: bare manager + accept loop; capture inbound links on rx_b.
    let resolver_b = ReachabilityResolver::new();
    let (tx_b, rx_b) = flume::unbounded::<LinkUnicast>();
    let mgr_b = Arc::new(IrohZenohLinkManager::new(Arc::clone(&ep_b), resolver_b, tx_b));
    let _accept_b = mgr_b.spawn_accept_loop();

    // 3. A: real Runtime via the internal path, with factory + ctx wired (ZEB-368).
    //    Build A's config: listen on iroh/<A>, connect/endpoints empty (no static seed),
    //    so the ONLY way A reaches B is the dynamic dial.
    let resolver_a = ReachabilityResolver::new();
    let (tx_a, _rx_a) = flume::unbounded::<LinkUnicast>();
    let mgr_a = Arc::new(IrohZenohLinkManager::new(Arc::clone(&ep_a), resolver_a.clone(), tx_a));
    let _accept_a = mgr_a.spawn_accept_loop();

    crate_iroh_factory_register_once(); // harmony_app::iroh_zenoh_registration::ensure_iroh_factory_registered()
    harmony_app::iroh_zenoh_registration::set_iroh_session_ctx(/* IrohSessionCtx { manager: mgr_a, new_link_rx: <A's factory rx> } */);

    // Build A's runtime + session via the same helper production uses (expose it as
    // pub(crate)/test-visible, or replicate: RuntimeBuilder::new(cfg).build()+start();
    // session::init(rt.clone().into())). Keep rt_a for connect_peer.
    let (rt_a, _session_a) = harmony_app::event_loop::open_session_with_runtime(cfg_a).await
        .expect("A session opens via internal runtime path");

    // 4. Driver on A: install hint sender, spawn run_dial_driver with the real dialer.
    let telemetry = Arc::new(harmony_app::network_health::DialTelemetry::new());
    let (hint_tx, hint_rx) = tokio::sync::mpsc::unbounded_channel();
    resolver_a.set_dial_hint_sender(hint_tx);
    let dialer = Arc::new(harmony_app::iroh_dial_driver::RuntimePeerDialer::new(rt_a.clone()));
    tokio::spawn(harmony_app::iroh_dial_driver::run_dial_driver(
        hint_rx, dialer, telemetry.clone(), *ep_a.node_id().as_bytes(), std::time::Duration::from_millis(200),
    ));

    // 5. MID-SESSION: A learns B (this is the event that never happened at open).
    let hlc = Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: "fix".into() };
    let payload_b = ReachabilityAnnouncePayload {
        iroh_node_id: *ep_b.node_id().as_bytes(),
        home_relay_url: ep_b.home_relay().map(|r| r.to_string()).unwrap_or_default(),
        direct_addresses: ep_b.bound_sockets(),
        announced_at_ms: hlc.wall_ms,
        identity_signature: [0; 64],
    };
    resolver_a.update([0xBB; 16], payload_b, hlc); // → DialHint → driver → connect_peer

    // 6. Assert B accepted an inbound iroh link within the window.
    let link = tokio::time::timeout(std::time::Duration::from_secs(20), rx_b.recv_async())
        .await
        .expect("B should receive an inbound link from A's dynamic dial")
        .expect("link channel open");
    let _ = link;
    assert!(telemetry.summary().attempts >= 1, "A attempted a dynamic dial");
}
```

> This test depends on exact harmony helpers (`build_hermetic_endpoint`, the factory `new_link_rx` plumbing, `cfg_a` construction with `listen=iroh/<A>` + empty `connect`). **Read `tests/community_reachability_two_engine_integration.rs` fully and reuse its helpers verbatim.** If exposing `open_session_with_runtime` as test-visible is awkward, make it `pub` in `event_loop.rs` (it is harmless) or replicate its 3 lines inline. The `connect_peer` may return `false` (B runs no zenoh TransportManager, so no zenoh handshake completes) — that is expected; **assert on B's accept loop receiving the link** (the iroh transport forming), not on `connect_peer`'s bool, exactly like the existing two-engine test.

- [ ] **Step 2: Run the test**

Run: `cargo nextest run --locked --features test-fixtures --test zeb_373_dynamic_dial_integration`
Expected: PASS. If it flakes on the iroh first-bind contention class, rerun once in isolation to confirm it is a timing flake, not a logic failure. If the real-Runtime-in-test setup proves infeasible (factory/ctx singleton conflicts, or `connect_peer` never routes), fall back to: keep the driver/seam/telemetry unit tests (Tasks 2-4) as the coverage, and mark this integration test `#[ignore]` with a comment pointing at ZEB-330 for the real two-machine proof. Record the decision in the commit message.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/zeb_373_dynamic_dial_integration.rs src-tauri/src/event_loop.rs
git commit -m "test(zeb-373): mid-session dial acceptance test (one real Runtime A dials bare peer B)"
```

---

## Task 7: Final gate sweep + push + PR

**Files:** none (verification + delivery)

- [ ] **Step 1: Full `--all-targets` sweep**

Run from `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --all-targets --features test-fixtures
```
And from repo root:
```bash
npx tsc --noEmit
npx vitest run
```
Expected: all green. Known iroh/zenoh loopback flakes (the ZEB-347 class) may need a single rerun; confirm any failure is a timing flake by re-running in isolation. No frontend changes are expected in this PR (backend-only), so `tsc`/`vitest` should be unaffected — run them anyway to honor the CI gates.

- [ ] **Step 2: MSRV check**

Run: `cargo check --locked --all-targets --features test-fixtures` (with the declared MSRV toolchain if available).
Expected: clean.

- [ ] **Step 3: Push + open PR**

```bash
git push -u origin zeb-373-dynamic-midsession-iroh-dial
gh pr create --repo zeblithic/harmony-client \
  --title "ZEB-373: dynamic mid-session iroh→Zenoh dial (internal RuntimeBuilder + dial driver + telemetry)" \
  --body "<see Step 4>"
```

- [ ] **Step 4: PR body**

Reference: spec `docs/specs/2026-06-04-zeb-373-dynamic-midsession-iroh-dial-design.md`; plan `docs/plans/2026-06-04-zeb-373-dynamic-midsession-iroh-dial-plan.md`; parent ZEB-321 Phase 2; defer-from ZEB-368. Summarize: enabled zenoh `internal`; swapped `zenoh::open` → RuntimeBuilder/session::init (retain Runtime); resolver notify seam (emit on first-learn); `DynamicDialDriver` (dial-once dedup + bounded backoff, mock-tested); `RuntimePeerDialer` via `connect_peer` (fresh placeholder zid, iroh locator out of `connect/endpoints`); dial telemetry on `NetworkHealthSnapshot` (the ZEB-330 evidence surface). Note: re-dial-on-drop is out of scope (ZEB-321 Phase 3); no wire-format change. Test plan: cargo fmt/clippy/nextest (`--all-targets`), tsc/vitest, MSRV; the mid-session-dial acceptance test belongs to the known iroh-loopback flake class.

---

## Self-Review (against the spec)

- **§3 approach A (internal feature + RuntimeBuilder + connect_peer):** Task 1 (feature + swap), Task 5 (`RuntimePeerDialer`). ✅
- **§4.1 session swap, config untouched, parity gate:** Task 1 (keeps full config build; parity tests). ✅
- **§4.2 resolver notify seam, emit on newly-active, Option-guarded:** Task 2. ✅
- **§4.3 driver, dedup, bounded backoff, re-arm on failure:** Task 4. ✅
- **§4.4 telemetry into NetworkHealthSnapshot, Arc shared writer/reader:** Task 3 (+ Task 5 `DialSnapshot`/`ProdDialSnapshot`, `NodeState.dial_telemetry`). ✅
- **§6 error handling (same failure emit), lifecycle (drop sender on stop):** Task 1 (error emit), Task 5 (clear in `clear_iroh_handles`). ✅
- **§8 testing: unit (dedup/backoff/resolver-emit), acceptance two-engine:** Tasks 2/4 (unit), Task 6 (acceptance). ✅
- **§9 scope: dial-once, no re-dial-on-drop:** enforced by the dialed-set; no transport-event hook added. ✅
- **§10 files:** all listed files have tasks. ✅

Type consistency: `DialHint { node_id: [u8;32], owner: OwnerAddr=[u8;16] }` used identically in Tasks 2/4/6; `DialTelemetry` methods (`record_attempt/succeeded/failed/skipped_duplicate/summary`) consistent across Tasks 3/4/5; `PeerDialer::dial(node_id, locator) -> bool` consistent Tasks 4/5/6; `run_dial_driver(rx, dialer, telemetry, self_node_id, backoff_base)` consistent Tasks 4/5/6.
