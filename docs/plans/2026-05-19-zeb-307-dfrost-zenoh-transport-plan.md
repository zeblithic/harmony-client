# ZEB-307 D-FROST Zenoh Transport Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `DfrostLogEngine` + `DfrostLogRegistry` so the 5 D-FROST IPCs from [ZEB-305](https://linear.app/zeblith/issue/ZEB-305) broadcast on `harmony/community/{community_id}/dfrost` and apply peer-received events, making D-FROST committees genuinely multi-node-capable.

**Architecture:** Mirror the `community_voting_log_engine.rs` pattern (per-community signed-event log, dedup tracker, mpsc-based publisher/subscriber channels for adapter bridging). Diverge in one place: emit the 3 D-FROST Tauri events (`dfrost-dkg-progress`, `dfrost-beacon-ready`, `dfrost-refresh-progress`) from `process_inbound` so peers see ceremony progress identical to local-driven progress — requires parameterizing the engine on `R: tauri::Runtime` and threading `AppHandle<R>`. Out of scope: actual Zenoh adapter wiring in `event_loop.rs` (voting punted this too — separate ticket).

**Tech Stack:** Tokio mpsc channels, `ciborium` CBOR codec, FROST-Ristretto255 (already in-tree), `ed25519-dalek` for inbound signature verification, Tauri 2 event emission.

---

## Pre-flight: pattern source map

Already mapped:
- `src-tauri/src/community_voting_log_engine.rs` (~635 lines) — primary pattern source.
  - `VotingReplayTracker` (lines 56–93): `HashMap<(OwnerAddr, String), (u64, u32)>` keyed on `(actor, device_id)` tracking max HLC.
  - `VotingLogEngineParams` (lines 102–110); `VotingLogEngine` (lines 118–127); `start()` (line 136); `publish_event()` (line 187); `process_inbound()` (lines 230–291).
  - Topic: `harmony/community/{community_id}/voting`. Wire format: CBOR-encoded `SignedVotingEvent` direct.
  - `VotingLogRegistry` (lines 299–335): `Mutex<HashMap<SpaceId, Arc<VotingLogEngine>>>`.
- `src-tauri/src/community_dfrost_log.rs`: `DfrostLog::apply` (line 283), `DfrostLog::apply_with_identity` (line 806).
- NodeState `dfrost_logs` field: `lib.rs` lines 544–551 — `Arc<Mutex<HashMap<SpaceId, Arc<Mutex<DfrostLog>>>>>`.
- IPC apply call-sites (broadcast must be inserted AFTER the log lock releases, per R8 + R10 lock-ordering discipline):
  - `dfrost_initiate_dkg` — apply line 22185, lock-release after line 22192.
  - `dfrost_contribute_dkg_round` — apply lines 22450 (rn=1), 22872 (rn=3); lock-release after lines 22452, 22874.
  - `dfrost_request_vrf_beacon` — apply line 23140, lock-release after line 23152.
  - `dfrost_contribute_threshold_sign` — apply lines 23490 (share), 23647 (vb aggregate); lock-release after lines 23510, 23667.
  - `dfrost_propose_refresh` — apply line 24055, lock-release after line 24070.
- `SignedCommitteeEvent` envelope: `tag: 'd' (D-FROST)`, `committee_tier: 0`, `kind`, `hlc`, `actor: OwnerAddr`, `payload: Vec<u8>` (CBOR), `sig: Vec<u8>` (Ed25519 over `signing_bytes()`).

---

## File Structure

**Create:**
- `src-tauri/src/community_dfrost_log_engine.rs` — engine + registry + replay tracker + unit tests.
- `src-tauri/tests/community_dfrost_transport_integration.rs` — multi-engine integration tests driving convergence purely through publisher/subscriber channels.

**Modify:**
- `src-tauri/src/lib.rs` — add `mod community_dfrost_log_engine;`; add 5 broadcast call-sites in the 5 dfrost IPCs after the log lock releases; add `dfrost_log_registry: Option<Arc<DfrostLogRegistry<R>>>` to `NodeState` (or equivalent based on Tauri-runtime generic shape — engine type needs investigation in Task 2 — see note).
- `src-tauri/src/community_dfrost_log.rs` — no changes expected (apply paths already validate); only touch if signature-verify helper needs to be exposed for the engine.

**Test:**
- `src-tauri/src/community_dfrost_log_engine.rs#[cfg(test)]` — in-file unit tests for tracker + engine self-loopback dedup.
- `src-tauri/tests/community_dfrost_transport_integration.rs` — peer-driven DKG completion via in-process mpsc channel pair.

---

### Task 0: Pre-flight verification (no commit)

**Files:** none (read-only).

- [ ] **Step 1: Verify branch state**

```bash
git status
git rev-parse HEAD
git log --oneline -5
```

Expected: clean working tree on `zeb-307-dfrost-zenoh-transport`, HEAD at the post-merge `983b26e` commit.

- [ ] **Step 2: Confirm five gates green on main lineage**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(/dfrost/)'
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run
```

Expected: all green. If any fail, STOP and surface to controller — main lineage is broken.

- [ ] **Step 3: Identify the Ed25519 signature-verify helper**

```bash
grep -n "ed25519_dalek::VerifyingKey\|verify_strict\|verify_signature\|SignedCommitteeEvent.*verify\|verify_event" src/community_dfrost_log.rs src/community_voting_core.rs src/community_membership.rs 2>&1 | head -20
```

Confirm whether `SignedCommitteeEvent` has an existing helper for signature verification, OR whether the engine will need to call `ed25519_dalek::VerifyingKey::from_bytes(&actor_pub).verify_strict(&event.signing_bytes(), &Signature::from_bytes(&event.sig))` inline. Report finding to controller before Task 4 dispatches.

No commit.

---

### Task 1: `DfrostReplayTracker` + unit tests

**Files:**
- Create: `src-tauri/src/community_dfrost_log_engine.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod community_dfrost_log_engine;`)

- [ ] **Step 1: Write failing test for tracker insert + duplicate detection**

```rust
// In src-tauri/src/community_dfrost_log_engine.rs#[cfg(test)] mod tests:
use crate::community_dfrost_types::{SignedCommitteeEvent, DfrostEventKind, ThresholdSignPayload};
use crate::owner_state_types::{Hlc, OwnerAddr};

fn test_event(actor: OwnerAddr, wall_ms: u64, logical: u32) -> SignedCommitteeEvent {
    let payload = ThresholdSignPayload {
        ceremony_id: [0u8; 32],
        message_hash: [0u8; 32],
        commitment_bytes: vec![],
        share_bytes: vec![],
    };
    let mut payload_bytes = Vec::new();
    ciborium::ser::into_writer(&payload, &mut payload_bytes).unwrap();
    SignedCommitteeEvent {
        tag: 'd',
        version: 1,
        committee_tier: 0,
        kind: DfrostEventKind::ThresholdSign,
        hlc: Hlc { wall_ms, logical, device_id: "dev-a".into() },
        actor,
        payload: payload_bytes,
        sig: vec![0u8; 64],
    }
}

#[test]
fn replay_tracker_dedups_repeat_event() {
    let mut t = DfrostReplayTracker::new();
    let addr = OwnerAddr([1u8; 16]);
    let e = test_event(addr, 100, 0);
    assert!(!t.contains(&e), "fresh event not contained");
    t.record(&e);
    assert!(t.contains(&e), "recorded event is contained");
}

#[test]
fn replay_tracker_dedups_per_actor_device() {
    let mut t = DfrostReplayTracker::new();
    let addr_a = OwnerAddr([1u8; 16]);
    let addr_b = OwnerAddr([2u8; 16]);
    t.record(&test_event(addr_a, 100, 0));
    assert!(!t.contains(&test_event(addr_b, 100, 0)), "different actor not deduped");
}

#[test]
fn replay_tracker_advances_on_higher_hlc() {
    let mut t = DfrostReplayTracker::new();
    let addr = OwnerAddr([1u8; 16]);
    t.record(&test_event(addr, 100, 0));
    let later = test_event(addr, 100, 1);
    assert!(!t.contains(&later), "advancing logical not deduped");
    t.record(&later);
    assert!(t.contains(&later), "advanced event recorded");
    // Older event still considered seen (replay window covers up-to-max).
    assert!(t.contains(&test_event(addr, 100, 0)));
}
```

- [ ] **Step 2: Verify tests fail with "module not found"**

```bash
cd src-tauri && cargo test --locked --features test-fixtures --lib community_dfrost_log_engine 2>&1 | tail -10
```

Expected: compile error — `community_dfrost_log_engine` module not declared.

- [ ] **Step 3: Implement the tracker**

```rust
// src-tauri/src/community_dfrost_log_engine.rs
//! D-FROST per-community signed-event log engine. Mirrors the
//! `community_voting_log_engine.rs` pattern: one topic per community
//! at `harmony/community/{community_id}/dfrost`; mpsc-based publisher
//! and subscriber channels bridged to Zenoh by the event-loop adapter
//! (deferred — out of scope for ZEB-307; this ticket ships the engine,
//! a follow-up ships the adapter).

use crate::community_dfrost_types::SignedCommitteeEvent;
use crate::owner_state_types::OwnerAddr;
use std::collections::HashMap;

/// Replay-defense tracker keyed on `(actor, device_id)`. Records the
/// max-observed `(wall_ms, logical)` HLC per signer; any inbound event
/// whose HLC is at-or-below the recorded max is considered a replay /
/// loopback and silently dropped.
#[derive(Default)]
pub struct DfrostReplayTracker {
    seen_max: HashMap<(OwnerAddr, String), (u64, u32)>,
}

impl DfrostReplayTracker {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn contains(&self, event: &SignedCommitteeEvent) -> bool {
        match self.seen_max.get(&(event.actor, event.hlc.device_id.clone())) {
            Some((w, l)) => (event.hlc.wall_ms, event.hlc.logical) <= (*w, *l),
            None => false,
        }
    }
    pub fn record(&mut self, event: &SignedCommitteeEvent) {
        let key = (event.actor, event.hlc.device_id.clone());
        let new_hlc = (event.hlc.wall_ms, event.hlc.logical);
        self.seen_max
            .entry(key)
            .and_modify(|cur| {
                if new_hlc > *cur {
                    *cur = new_hlc;
                }
            })
            .or_insert(new_hlc);
    }
}

#[cfg(test)]
mod tests {
    // Tests from Step 1 paste here.
}
```

Add to `src-tauri/src/lib.rs` near the other `mod community_dfrost_*` declarations:

```rust
mod community_dfrost_log_engine;
```

- [ ] **Step 4: Run tests + verify gates green**

```bash
cd src-tauri && cargo test --locked --features test-fixtures --lib community_dfrost_log_engine
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: 3/3 tracker tests pass; fmt + clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_dfrost_log_engine.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-307): DfrostReplayTracker + module scaffold"
```

---

### Task 2: `DfrostLogEngineParams` + `DfrostLogEngine::start` skeleton

**Files:**
- Modify: `src-tauri/src/community_dfrost_log_engine.rs`

This task creates the engine struct and starts the receive loop, but the loop body just logs+drops packets — no apply yet. Lets us verify the Tauri-runtime generic compiles before any non-trivial logic lands.

- [ ] **Step 1: Write failing test for engine startup**

```rust
#[tokio::test]
async fn engine_start_returns_handle_and_drops_cleanly() {
    let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let log = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::community_dfrost_log::DfrostLog::new(),
    ));
    let community_id = crate::owner_state_types::SpaceId([0u8; 16]);
    let app = tauri::test::mock_app();
    let app_handle = app.handle().clone();

    let engine = DfrostLogEngine::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: log.clone(),
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        app_handle,
        self_addr: crate::owner_state_types::OwnerAddr([0u8; 16]),
        self_x25519_priv: [0u8; 32],
    })
    .await;

    assert_eq!(engine.community_id(), community_id);
    // Drop sub_tx to signal end-of-stream; loop should exit cleanly.
    drop(sub_tx);
    drop(engine);
}
```

- [ ] **Step 2: Run test, expect compile failure (types not defined)**

```bash
cd src-tauri && cargo test --locked --features test-fixtures --lib community_dfrost_log_engine::tests::engine_start 2>&1 | tail -20
```

- [ ] **Step 3: Implement skeleton**

```rust
// Add to src-tauri/src/community_dfrost_log_engine.rs:

use crate::community_dfrost_log::DfrostLog;
use crate::owner_state_types::SpaceId;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Parameters bundle for `DfrostLogEngine::start`.
pub struct DfrostLogEngineParams<R: tauri::Runtime> {
    pub community_id: SpaceId,
    pub dfrost_log: Arc<Mutex<DfrostLog>>,
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
    pub app_handle: tauri::AppHandle<R>,
    pub self_addr: OwnerAddr,
    pub self_x25519_priv: [u8; 32],
}

/// Per-community D-FROST engine. Owns:
/// - The Arc<Mutex<DfrostLog>> for this community.
/// - A publisher channel into the Zenoh adapter (outbound).
/// - A subscriber receive loop (inbound).
/// - The Tauri AppHandle used to emit progress events from
///   inbound applies (peer-driven ceremony progress).
pub struct DfrostLogEngine<R: tauri::Runtime> {
    community_id: SpaceId,
    dfrost_log: Arc<Mutex<DfrostLog>>,
    tracker: Arc<Mutex<DfrostReplayTracker>>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    _receive_handle: tokio::task::JoinHandle<()>,
    _phantom: std::marker::PhantomData<R>,
}

impl<R: tauri::Runtime> DfrostLogEngine<R> {
    pub fn community_id(&self) -> SpaceId {
        self.community_id
    }

    pub async fn start(params: DfrostLogEngineParams<R>) -> Arc<Self> {
        let tracker = Arc::new(Mutex::new(DfrostReplayTracker::new()));
        let community_id = params.community_id;
        let log_for_loop = params.dfrost_log.clone();
        let tracker_for_loop = tracker.clone();
        let app_for_loop = params.app_handle;
        let self_addr_for_loop = params.self_addr;
        let self_x_priv_for_loop = params.self_x25519_priv;
        let mut rx = params.subscriber_rx;

        let receive_handle = tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                // Task 3+ will populate. For now, drop to test the
                // start/shutdown shape.
                let _ = (
                    &community_id,
                    &log_for_loop,
                    &tracker_for_loop,
                    &app_for_loop,
                    &self_addr_for_loop,
                    &self_x_priv_for_loop,
                    packet,
                );
            }
        });

        Arc::new(Self {
            community_id,
            dfrost_log: params.dfrost_log,
            tracker,
            publisher_tx: params.publisher_tx,
            _receive_handle: receive_handle,
            _phantom: std::marker::PhantomData,
        })
    }
}
```

- [ ] **Step 4: Run tests + verify gates**

```bash
cd src-tauri && cargo test --locked --features test-fixtures --lib community_dfrost_log_engine
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: engine_start_returns_handle_and_drops_cleanly passes; tracker tests still pass; fmt + clippy clean.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(zeb-307): DfrostLogEngine skeleton + receive-loop spawn"
```

---

### Task 3: `process_inbound` — signature verify + apply (the load-bearing inbound path)

**Files:**
- Modify: `src-tauri/src/community_dfrost_log_engine.rs`

This is the security-critical task. Inbound bytes must NOT reach `apply` until:
1. They decode cleanly as `SignedCommitteeEvent` (else: drop).
2. The Ed25519 signature verifies against the actor's identity-pubkey (else: drop — invalid signature).
3. The replay tracker says "not seen" (else: drop — replay or self-loopback).

Only after those gates does `apply_with_identity` run.

**Note on signature-verify pubkey lookup:** Task 0 Step 3 surfaced which helper to use. If `community_dfrost_log` doesn't already verify the envelope sig, the engine must — call this out in the implementer prompt. The actor's Ed25519 pubkey lookup needs the community membership snapshot. **Defer the lookup-from-membership-CRDT for now**: this task does the SIG verify against `event.actor`'s Ed25519 pubkey directly only if we have a self-contained way to derive it. If we need cross-CRDT lookup, the engine receives an `IdentityResolver` in `DfrostLogEngineParams` (mirror the pattern from `dfrost_propose_refresh` which receives `community_registry.identity_resolver()`). The implementer subagent should check whether such a resolver is already on NodeState and is cheaply cloneable.

- [ ] **Step 1: Write failing test for inbound apply via subscriber channel**

```rust
#[tokio::test]
async fn engine_inbound_dkg_round1_applies_via_subscriber_channel() {
    // Build two engines (Alice + Bob); Alice initiates a dkg dr rn=1 event,
    // pushes the CBOR bytes onto Bob's subscriber_rx, asserts Bob's
    // DfrostLog has pending_dkg seeded with matching ceremony_id.
    //
    // (Concrete code reuses the dkg_ipc_round_trip_two_engine_2of2 fixture
    // helpers — read those for the alice_sk / bob_x_priv / ALICE / BOB
    // constants and the build_signed_dfrost_event pattern.)
    //
    // Failing assertion target: bob.log.committee_state.pending_dkg.is_some()
}
```

Note: this test is genuinely cross-cutting. The implementer subagent should structure it minimally (just enough to surface the engine inbound path), and if test setup balloons, escalate.

- [ ] **Step 2: Verify test fails with "pending_dkg is None" (or compile error)**

- [ ] **Step 3: Implement `process_inbound`**

```rust
impl<R: tauri::Runtime> DfrostLogEngine<R> {
    /// Decode + signature-verify + dedup + apply an inbound CBOR-encoded
    /// `SignedCommitteeEvent`. Errors are logged and dropped (never
    /// propagate up to the receive loop or kill the engine).
    async fn process_inbound(
        community_id: SpaceId,
        dfrost_log: &Arc<Mutex<DfrostLog>>,
        tracker: &Arc<Mutex<DfrostReplayTracker>>,
        app_handle: &tauri::AppHandle<R>,
        self_addr: &OwnerAddr,
        self_x25519_priv: &[u8; 32],
        packet: &[u8],
    ) {
        // 1. Decode.
        let event: SignedCommitteeEvent = match ciborium::de::from_reader(packet) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(community_id = %hex::encode(community_id.0), error = %e, "drop: ciborium decode");
                return;
            }
        };

        // 2. Signature verify. (See note in plan re: pubkey lookup —
        //    implementer determines based on Task 0 Step 3 finding.)
        if let Err(e) = verify_signed_committee_event(&event) {
            tracing::warn!(community_id = %hex::encode(community_id.0), error = %e, "drop: sig verify");
            return;
        }

        // 3. Dedup.
        {
            let t = tracker.lock().await;
            if t.contains(&event) {
                return; // silent drop — common case for self-loopback
            }
        }

        // 4. Apply. Use apply_with_identity so DKG rn=2 / refresh rn=1
        //    decrypt-to-self paths are exercised.
        let apply_result = {
            let mut log = dfrost_log.lock().await;
            log.apply_with_identity(event.clone(), self_addr, self_x25519_priv)
        };

        if let Err(e) = apply_result {
            tracing::warn!(community_id = %hex::encode(community_id.0), error = ?e, "drop: apply");
            return;
        }

        // 5. Record in tracker.
        {
            let mut t = tracker.lock().await;
            t.record(&event);
        }

        // 6. Emit Tauri event (Task 4 fills this in).
        let _ = app_handle;
    }
}

fn verify_signed_committee_event(_event: &SignedCommitteeEvent) -> Result<(), String> {
    // Determined by Task 0 Step 3 finding.
    // OPTION A: existing helper exists — call it.
    // OPTION B: no helper — inline ed25519 verify against event.actor's pubkey
    //           (which requires an IdentityResolver, which means
    //           DfrostLogEngineParams gets an additional field).
    // The implementer MUST implement one of these and remove the
    // unreachable!() before this task commits.
    Err("TODO: ZEB-307 Task 3 sig verify".into())
}
```

Update the receive loop body in `start` to call `process_inbound`:

```rust
let receive_handle = tokio::spawn(async move {
    while let Some(packet) = rx.recv().await {
        Self::process_inbound(
            community_id,
            &log_for_loop,
            &tracker_for_loop,
            &app_for_loop,
            &self_addr_for_loop,
            &self_x_priv_for_loop,
            &packet,
        )
        .await;
    }
});
```

- [ ] **Step 4: Run tests + verify gates**

```bash
cd src-tauri && cargo test --locked --features test-fixtures --lib community_dfrost_log_engine
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(zeb-307): inbound decode + sig-verify + dedup + apply"
```

---

### Task 4: Emit Tauri events from inbound apply

**Files:**
- Modify: `src-tauri/src/community_dfrost_log_engine.rs`

After a successful inbound apply, the engine emits one of the 3 D-FROST Tauri events to mirror what the IPC layer emits on local-driven apply. Frontend code listens via `listen("dfrost-dkg-progress", ...)` and sees both local + peer-driven progress identically.

The 3 event payload structs already live in `lib.rs` (`DfrostDkgProgressPayload`, `DfrostBeaconReadyPayload`, `DfrostRefreshProgressPayload`). The engine reads `event.kind` + `payload` and emits the right one.

- [ ] **Step 1: Write failing test for event emission on peer DKG rn=1**

Test pattern: subscribe to the AppHandle's event bus, drive an inbound rn=1 event through the subscriber channel, assert one `dfrost-dkg-progress` payload received with matching ceremony_id.

- [ ] **Step 2: Run, expect no emission**

- [ ] **Step 3: Wire the emit in `process_inbound` (after step 5 / before the final return)**

```rust
match event.kind {
    crate::community_dfrost_types::DfrostEventKind::DkgRound => {
        if let Ok(payload) = ciborium::de::from_reader::<
            crate::community_dfrost_types::DkgRoundPayload,
            _,
        >(&event.payload[..]) {
            let count = participants_after_dkg_round(&dfrost_log.lock().await, payload.round_num).await;
            let evt = DfrostDkgProgressPayload {
                ceremony_id: hex::encode(payload.ceremony_id),
                round_num: payload.round_num,
                participants_so_far: count,
            };
            let _ = app_handle.emit("dfrost-dkg-progress", evt);
        }
    }
    crate::community_dfrost_types::DfrostEventKind::ThresholdSign => {
        // No emit on plain ts; only the vb aggregation step emits beacon-ready.
    }
    crate::community_dfrost_types::DfrostEventKind::VrfBeacon => {
        if let Ok(payload) = ciborium::de::from_reader::<
            crate::community_dfrost_types::VrfBeaconPayload,
            _,
        >(&event.payload[..]) {
            let evt = DfrostBeaconReadyPayload {
                ceremony_id: hex::encode(payload.ceremony_id),
                vrf_output: hex::encode(payload.vrf_output),
            };
            let _ = app_handle.emit("dfrost-beacon-ready", evt);
        }
    }
    crate::community_dfrost_types::DfrostEventKind::ProactiveRefresh => {
        if let Ok(payload) = ciborium::de::from_reader::<
            crate::community_dfrost_types::RefreshRoundPayload,
            _,
        >(&event.payload[..]) {
            let evt = DfrostRefreshProgressPayload {
                ceremony_id: hex::encode(payload.ceremony_id),
                round_num: payload.round_num,
            };
            let _ = app_handle.emit("dfrost-refresh-progress", evt);
        }
    }
    _ => {}
}
```

Note: The 3 payload structs are currently `pub` (or `pub(crate)`) in `lib.rs`. If they're not exported, the implementer subagent moves them to `community_dfrost_types.rs` to be importable.

- [ ] **Step 4: Run tests + verify gates**

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(zeb-307): emit Tauri events from inbound apply"
```

---

### Task 5: `publish_event` — outbound publishing

**Files:**
- Modify: `src-tauri/src/community_dfrost_log_engine.rs`

Outbound side is much simpler than inbound: CBOR-encode the event, push onto `publisher_tx`. The receive-loop dedup on `record()` ensures self-loopback is silently dropped on the way back in.

- [ ] **Step 1: Write failing test for publish_event sending CBOR onto channel**

```rust
#[tokio::test]
async fn engine_publish_event_sends_cbor_on_publisher_tx() {
    let (pub_tx, mut pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    // ... start engine + build a SignedCommitteeEvent ...
    engine.publish_event(event.clone()).await.expect("publish");
    let bytes = pub_rx.recv().await.expect("packet");
    let decoded: SignedCommitteeEvent = ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(decoded.actor, event.actor);
    assert_eq!(decoded.kind as u8, event.kind as u8);
}
```

- [ ] **Step 2: Run, expect method-not-found**

- [ ] **Step 3: Implement publish_event**

```rust
impl<R: tauri::Runtime> DfrostLogEngine<R> {
    /// Publish a signed event onto the Zenoh-bridged publisher channel.
    /// Self-loopback is fine: the inbound dedup tracker silently drops
    /// it before re-applying.
    pub async fn publish_event(&self, event: SignedCommitteeEvent) -> Result<(), String> {
        // Record in our own tracker first so the loopback subscription
        // (which receives our own publish bytes) sees the dedup gate
        // and drops it instead of re-applying.
        {
            let mut t = self.tracker.lock().await;
            t.record(&event);
        }
        let mut packet = Vec::new();
        ciborium::ser::into_writer(&event, &mut packet)
            .map_err(|e| format!("publish_event encode: {e}"))?;
        self.publisher_tx
            .send(packet)
            .await
            .map_err(|e| format!("publish_event send: {e}"))
    }
}
```

- [ ] **Step 4: Run tests + verify gates**

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(zeb-307): publish_event outbound"
```

---

### Task 6: `DfrostLogRegistry`

**Files:**
- Modify: `src-tauri/src/community_dfrost_log_engine.rs`

Registry holds `Arc<DfrostLogEngine<R>>` keyed on `SpaceId`. Pattern mirrors `VotingLogRegistry`.

- [ ] **Step 1: Write failing tests for register + get + shutdown**

```rust
#[tokio::test]
async fn registry_register_and_get_round_trips() {
    let reg = DfrostLogRegistry::<tauri::test::MockRuntime>::new();
    let community_id = SpaceId([7u8; 16]);
    // ... build DfrostLogEngineParams ...
    let engine = reg.register(params).await;
    assert!(reg.get(community_id).await.is_some());
    assert!(reg.get(SpaceId([99u8; 16])).await.is_none());
}

#[tokio::test]
async fn registry_shutdown_drops_all_engines() {
    let reg = DfrostLogRegistry::<tauri::test::MockRuntime>::new();
    // ... register 2 engines ...
    reg.shutdown().await;
    assert!(reg.get(community_id_a).await.is_none());
}
```

- [ ] **Step 2: Run, expect missing types**

- [ ] **Step 3: Implement registry**

```rust
pub struct DfrostLogRegistry<R: tauri::Runtime> {
    engines: Mutex<std::collections::HashMap<SpaceId, Arc<DfrostLogEngine<R>>>>,
}

impl<R: tauri::Runtime> DfrostLogRegistry<R> {
    pub fn new() -> Self {
        Self {
            engines: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub async fn register(&self, params: DfrostLogEngineParams<R>) -> Arc<DfrostLogEngine<R>> {
        let community_id = params.community_id;
        let engine = DfrostLogEngine::start(params).await;
        let mut guard = self.engines.lock().await;
        guard.insert(community_id, engine.clone());
        engine
    }

    pub async fn get(&self, community_id: SpaceId) -> Option<Arc<DfrostLogEngine<R>>> {
        self.engines.lock().await.get(&community_id).cloned()
    }

    pub async fn shutdown(&self) {
        self.engines.lock().await.clear();
    }
}

impl<R: tauri::Runtime> Default for DfrostLogRegistry<R> {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Run tests + verify gates**

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(zeb-307): DfrostLogRegistry"
```

---

### Task 7: NodeState integration

**Files:**
- Modify: `src-tauri/src/lib.rs`

Add `dfrost_log_registry: Option<Arc<DfrostLogRegistry<R>>>` field on `NodeState`. The `R: tauri::Runtime` propagates through; if NodeState isn't already generic on `R`, factor that in (or use a type-erased trait object — implementer to decide based on existing shape).

**Note for implementer:** NodeState's existing fields (`channel_log_registry`, `voting_log_registry`) likely already establish the Tauri-runtime generic pattern. Mirror exactly.

- [ ] **Step 1: Locate NodeState + understand the runtime-generic shape**

```bash
grep -n "pub struct NodeState\|channel_log_registry\|voting_log_registry" src/lib.rs | head -10
```

- [ ] **Step 2: Add the field with matching shape**

```rust
pub dfrost_log_registry: Option<Arc<community_dfrost_log_engine::DfrostLogRegistry<R>>>,
```

(or non-generic equivalent if NodeState isn't parameterized — implementer adjusts).

- [ ] **Step 3: Initialize as `None` in `NodeState::default()` (or equivalent default impl)**

- [ ] **Step 4: Verify the binary still compiles + run gates**

```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(/dfrost/)'
```

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(zeb-307): NodeState dfrost_log_registry field"
```

---

### Task 8: Wire 5 IPCs to broadcast after local apply

**Files:**
- Modify: `src-tauri/src/lib.rs`

After each of the 5 D-FROST IPCs successfully applies locally, broadcast the same event via the registry. The broadcast call MUST be after the log lock is released — R8 + R10 lock-ordering discipline.

For each IPC: after the `{ let mut log = log_arc.lock().await; ... apply_with_identity(...)?; }` block closes, before the function returns, do:

```rust
if let Some(reg) = &state_lock.lock().map_err(...)?.dfrost_log_registry {
    if let Some(engine) = reg.get(space_id).await {
        if let Err(e) = engine.publish_event(event.clone()).await {
            tracing::warn!(error = %e, "dfrost broadcast failed");
        }
    }
}
```

Two subtle points:
1. **`event` ownership:** the existing code consumes `event` into `apply_with_identity`. Implementer threads it via `event.clone()` once at the apply site so a copy is available post-apply for broadcast.
2. **Best-effort broadcast:** failure to broadcast does NOT fail the IPC (the local apply already succeeded). Log + continue.

The 5 IPCs + their apply call-sites:

| IPC | Apply line | Broadcast site (after lock release) |
|---|---|---|
| `dfrost_initiate_dkg` | 22185 | after 22192 |
| `dfrost_contribute_dkg_round` (rn=1) | 22450 | after 22452 |
| `dfrost_contribute_dkg_round` (rn=3) | 22872 | after 22874 |
| `dfrost_request_vrf_beacon` | 23140 | after 23152 |
| `dfrost_contribute_threshold_sign` (share apply) | 23490 | after 23510 |
| `dfrost_contribute_threshold_sign` (vb aggregate) | 23647 | after 23667 |
| `dfrost_propose_refresh` | 24055 | after 24070 |

Note: `dfrost_contribute_dkg_round` has TWO apply sites (rn=1 + rn=3); both need broadcasts of their respective events.

- [ ] **Step 1: For each IPC, add broadcast call**

(Implementer makes 5 commits OR one bundled commit — implementer discretion. Recommend ONE commit per IPC for reviewability.)

- [ ] **Step 2: Verify every IPC compiles + gates green**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_dfrost_ipc_integration
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Existing IPC integration tests must still pass — they don't go through the registry (it'll be `None` in those test contexts), so the broadcast is a no-op.

- [ ] **Step 3: Commit**

```bash
git commit -am "feat(zeb-307): IPCs broadcast after local apply"
```

---

### Task 9: In-engine unit test — self-loopback dedup

**Files:**
- Modify: `src-tauri/src/community_dfrost_log_engine.rs`

Drive a publish→loopback→inbound cycle within one engine and assert the apply happens exactly once.

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn engine_self_loopback_no_double_apply() {
    let (pub_tx, mut pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    // ... start engine, publish event, take bytes from pub_rx, push to sub_tx ...
    // wait, then assert dfrost_log.events.len() == 1
}
```

- [ ] **Step 2: Run + verify dedup works**

- [ ] **Step 3: Commit**

```bash
git commit -am "test(zeb-307): self-loopback dedup unit test"
```

---

### Task 10: Two-engine integration test — peer-driven DKG completion

**Files:**
- Create: `src-tauri/tests/community_dfrost_transport_integration.rs`

Build two engines (Alice + Bob), wire `alice.publisher_tx → bob.subscriber_rx` and `bob.publisher_tx → alice.subscriber_rx`, then drive a 2-of-2 DKG using the existing IPC-style event builders (reuse helpers from `tests/community_dfrost_ipc_integration.rs`). Assert both nodes converge on identical `committee_state.joint_verifying_key`.

This is the load-bearing "transport actually works" demonstration.

- [ ] **Step 1: Stand up the two-engine fixture**

Helpers to lift / mirror from `tests/community_dfrost_ipc_integration.rs`: `ALICE`, `BOB`, `alice_sk()`, `bob_sk()`, `alice_x25519_priv()`, etc.

- [ ] **Step 2: Bridge the channels in both directions**

```rust
let (alice_pub_tx, alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
let (bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);
let (bob_pub_tx, bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);

// Forward Alice → Bob
let mut alice_pub_rx = alice_pub_rx;
tokio::spawn({
    let bob_sub_tx = bob_sub_tx.clone();
    async move {
        while let Some(p) = alice_pub_rx.recv().await {
            let _ = bob_sub_tx.send(p).await;
        }
    }
});
// Forward Bob → Alice (symmetric).
```

- [ ] **Step 3: Drive DKG via publish_event on both engines**

Alice publishes dr rn=1; allow ms for bridge propagation; Bob publishes dr rn=1; both publish dr rn=2 + dk; assert convergence.

- [ ] **Step 4: Verify convergence**

```rust
let alice_log = alice_dfrost_log.lock().await;
let bob_log = bob_dfrost_log.lock().await;
assert_eq!(
    alice_log.committee_state.joint_verifying_key,
    bob_log.committee_state.joint_verifying_key,
);
assert!(alice_log.committee_state.active);
assert!(bob_log.committee_state.active);
```

- [ ] **Step 5: Run + verify**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_dfrost_transport_integration
```

- [ ] **Step 6: Commit**

```bash
git commit -am "test(zeb-307): two-engine DKG convergence via transport"
```

---

### Task 11: Final 5-gate sweep + push + PR

**Files:** none (verification + git ops).

- [ ] **Step 1: Run all five gates from scratch**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run
```

Expected: all 5 green. Pre-existing ZEB-306 folder_ingest failures are acceptable (filed; not introduced by this branch — confirm by checking they reproduce on `origin/main`).

- [ ] **Step 2: Push branch**

```bash
git push -u origin zeb-307-dfrost-zenoh-transport
```

- [ ] **Step 3: Create PR via `gh pr create`**

PR title: `ZEB-307: D-FROST Zenoh transport (DfrostLogEngine + DfrostLogRegistry)`

PR body (HEREDOC, markdown-linked Linear refs per `feedback_linear_pr_auto_close`):

```markdown
## Summary

Closes [ZEB-307](https://linear.app/zeblith/issue/ZEB-307). Ships `DfrostLogEngine` + `DfrostLogRegistry` so the 5 D-FROST IPCs from [ZEB-305](https://linear.app/zeblith/issue/ZEB-305) broadcast over the Zenoh bridge and apply peer-received events.

- New `src-tauri/src/community_dfrost_log_engine.rs` mirrors `community_voting_log_engine.rs` (per-community topic, mpsc publisher/subscriber, dedup tracker keyed on `(actor, device_id)`).
- Inbound `process_inbound` decodes CBOR → verifies Ed25519 envelope sig → dedups → applies via `apply_with_identity`. Emits the 3 D-FROST Tauri events (`dfrost-dkg-progress`, `dfrost-beacon-ready`, `dfrost-refresh-progress`) on successful inbound apply so peer-driven progress is indistinguishable from local-driven progress on the frontend.
- All 5 dfrost IPCs (`dfrost_initiate_dkg`, `dfrost_contribute_dkg_round` rn={1,3}, `dfrost_request_vrf_beacon`, `dfrost_contribute_threshold_sign` ts+vb, `dfrost_propose_refresh`) now `publish_event` after the local apply (after the log lock releases — preserves R8 + R10 lock-ordering invariants from PR #143).
- Two-engine integration test (`tests/community_dfrost_transport_integration.rs`) drives a 2-of-2 DKG to convergence purely through the publisher/subscriber bridge.

## Test plan

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (modulo pre-existing [ZEB-306](https://linear.app/zeblith/issue/ZEB-306) folder_ingest tempfile flakiness)
- [ ] `npx tsc --noEmit`
- [ ] `npx vitest run`

## Out of scope (deferred)

- Zenoh adapter wiring in `event_loop.rs` — engine ships with mpsc channels exposed; the adapter that bridges `publisher_tx`/`subscriber_rx` to actual Zenoh `put`/subscribe is a separate ticket (voting punted this too at [ZEB-291](https://linear.app/zeblith/issue/ZEB-291) Task 19).
- Persistence — `dfrost_logs` remains in-memory.
- UI for D-FROST ceremonies — Phase 4a-main.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

(Implementer fills in the actual PR number, replaces "PR #143" with the actual reference, etc.)

- [ ] **Step 4: Verify PR is open with no missing references**

```bash
gh pr view <pr_number>
```

- [ ] **Step 5: Hand control back to controller**

Implementer reports DONE with PR URL. Controller starts the autonomous bot-review monitoring loop (matches PR #143 workflow).

---

## Self-review against the umbrella spec

This plan covers ZEB-307's scope:

| Scope item | Task(s) |
|---|---|
| 1. `DfrostLogEngine` skeleton + topic shape | Task 2 |
| 1. Outbound `broadcast_event` | Task 5 |
| 1. Inbound `process_inbound` (decode + verify + dedup + apply) | Task 3 |
| 2. `DfrostLogRegistry` | Task 6 |
| 3. Tauri events on inbound apply | Task 4 |
| 4. IPC integration with all 5 IPCs | Task 8 |
| 5. Wire-format pinning (no new fixtures expected) | implicit — Task 11 sweep |
| 6. Multi-engine integration test | Tasks 9 (in-file) + 10 (in `tests/`) |

Deferred items match the ticket's out-of-scope section (persistence, UI, sortition mechanism, Zenoh adapter wiring).

## Open question for Task 0 / Task 3

- Does an existing `verify_signed_committee_event` helper exist? Task 0 Step 3 surfaces this. If yes → call it. If no → engine receives an `IdentityResolver` in `DfrostLogEngineParams` and the implementer wires the ed25519 verify inline. This branch decision IS the load-bearing security work in this PR; implementer subagent must surface their finding to the controller before Task 3 dispatches.
