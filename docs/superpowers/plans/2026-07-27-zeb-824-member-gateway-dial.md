# ZEB-824 Member Rendezvous-Beacon Dial Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the ZEB-824 session-bootstrap deadlock: a standing per-community driver that, when a community has no live member session, resolves the community's rendezvous beacon from pkarr, verifies it, seeds it into the `ReachabilityResolver`, and kicks the reconnect supervisor — the existing machinery does the rest.

**Architecture:** One new driver module (`community_gateway_dial_driver.rs`, the relay-pull/vine-pull driver shape) + one new resolve entry point in `community_rendezvous.rs` that preserves the beacon's identity and filters self + one new telemetry block in `network_health.rs`. Boot wiring in `start_node_inner` alongside the other drivers. The driver is a feeder, not a dialer.

**Tech Stack:** Rust (tokio, async_trait), harmony-pkarr (unchanged), cargo-nextest.

**Spec:** `docs/superpowers/specs/2026-07-27-zeb-824-member-gateway-dial-design.md` — read §4 (predicate), §5 (pass), §6 (edges) before implementing.

> **⚠️ As-implemented deviations (2026-07-27).** This plan is the historical planning
> record; the spec (as amended) and the shipped module docs are authoritative for how the
> code works. Three things changed during implementation and review:
>
> 1. **The membership gate and `self_owner` were withdrawn.** Task 3's gate
>    (`beacon_owner == self.self_owner || !members.contains(&beacon_owner)` ⇒
>    `RejectedNonMember`), Task 4's 5-arg `CommunityGatewayDialDriver::new()` wiring, and
>    Task 5's `setup.bob_addr` fifth argument were unimplementable as written: materialized
>    membership is keyed by the master signing-only hash while the beacon yields the
>    composite device-address hash — the repo's two deliberately non-convergent notions —
>    so the gate would have rejected every legitimate beacon. Decision of record
>    (spec §5c): epoch-envelope trust, open-join parity; `new()` takes 4 args; the
>    resolve-layer endpoint-id self-filter is the only self-defense; ZEB-827 tracks the
>    principled binding.
> 2. **The telemetry vocabulary is 8 outcomes, not 5.** `soloCommunity` and
>    `engineUnregistered` were added at the Task 2 freeze; `resolveError` was added in
>    PR #565 review round 1 (pkarr resolve errors are no longer conflated with `noBeacon`).
> 3. **The backoff ladder clears on every non-starved transition** (heal, solo,
>    engine-unregistered, un-join — PR #565 review round 1), not only on heal as Task 3's
>    pseudocode shows.

## Global Constraints

- All cargo commands run from `src-tauri/`, always `--locked`, tests always `--features test-fixtures` (CLAUDE.md).
- Clippy gate is CI-exact: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; fmt gate `cargo fmt --all -- --check`.
- Epoch key for rendezvous resolve is **`engine.membership_key()`** (spawn-time), NEVER `live_epoch_key` — must match the rendezvous publisher (`lib.rs:11298`). Spec §5.3.
- The driver must never inline-await into an event-loop channel (start_node inline-await hazard, `lib.rs:6094-6117`). Its awaits: pkarr HTTP, engine mutexes, nothing else.
- No wall-clock in tests: the driver takes an injected `now_fn`; ladder tests use a settable clock.
- New snapshot DTO fields serialize camelCase (`#[serde(rename_all = "camelCase")]`), `Option` + `#[serde(default)]` for forward-compat (network_health.rs convention).
- Telemetry pass counter increments BEFORE any candidate read (ZEB-803, `community_relay_pull_driver.rs:285-293`).
- Self-filter compares 32-byte iroh endpoint ids (`payload.iroh_node_id`), not 16-byte device ids (ZEB-806 near-miss).
- Commit after each task with a `ZEB-824:` prefixed message.

---

### Task 1: `resolve_rendezvous_identified` — identity-preserving, self-filtering rendezvous resolve

**Files:**
- Modify: `src-tauri/src/community_rendezvous.rs`
- Test: `src-tauri/tests/misc/community_open_join_cross_wan_integration.rs` (reuse its mock-relay + publisher harness)

**Interfaces:**
- Consumes: `harmony_pkarr::rendezvous::{resolve_rendezvous_with, SlotResolver, RendezvousResolveConfig, RendezvousResolveOutcome}` (core crate, unchanged); `harmony_pkarr::PkarrResolver::resolve(&VerifyingKey) -> Result<Option<PkarrRoutingRecord>, _>`; `derive_ephemeral_key(PkarrCase::Community, ikm, info)`; `PkarrRoutingRecord { routing_blob, harmony_identity_pub: [u8;64], .. }` + `verify_freshness(now_ms)`.
- Produces: `pub struct IdentifiedBeacon { pub payload: ReachabilityAnnouncePayload, pub beacon_identity_pub: [u8; 64] }` and `pub async fn resolve_rendezvous_identified(pkarr: &Arc<PkarrResolver>, epoch_key: &EpochKey, self_endpoint_id: [u8; 32], now_ms: u64, cfg: &RendezvousResolveConfig) -> RendezvousResolveOutcome<IdentifiedBeacon>`. Task 3 consumes both.

Why not a decode-closure variant: the core `PkarrSlotResolver`'s decode closure receives only the routing **blob** (`Fn(&[u8]) -> Option<P>`, `harmony-pkarr/src/rendezvous.rs:181-190`) — the outer record, and with it `harmony_identity_pub`, is already discarded by the time the closure runs. So this task adds a small client-side `SlotResolver` impl instead. No core-crate change.

- [ ] **Step 1: Write the failing tests**

In `tests/misc/community_open_join_cross_wan_integration.rs`, after the existing open-join tests (reuse `setup_open_join()` / `await_rendezvous_slot_visible` — Alice is the slot-0 beacon):

```rust
/// ZEB-824: the identified resolve returns the beacon's identity alongside the
/// payload, so a member-side caller can derive the beacon's OwnerAddr.
#[tokio::test(flavor = "multi_thread")]
async fn identified_resolve_returns_beacon_identity() {
    let setup = setup_open_join().await;
    await_rendezvous_slot_visible(&setup.pkarr_resolver, &setup.epoch_key, 0).await;
    let outcome = harmony_app::community_rendezvous::resolve_rendezvous_identified(
        &setup.pkarr_resolver,
        &setup.epoch_key,
        [0xEE; 32], // NOT alice's endpoint id — no self-filtering here
        wall_ms(),
        &harmony_app::community_rendezvous::rendezvous_config_from_env(),
    )
    .await;
    let beacon = outcome.payload.expect("alice's slot-0 beacon must resolve");
    assert_eq!(
        beacon.payload.iroh_node_id,
        *setup.alice_ep.node_id().as_bytes(),
        "payload must be alice's endpoint"
    );
    // The outer record's identity must ride along (this is what the plain
    // resolve_rendezvous throws away).
    assert_ne!(beacon.beacon_identity_pub, [0u8; 64]);
}

/// ZEB-824 self-dial hazard: a member that IS the beacon must see its own slot
/// as empty (spec §5, decode-layer self-filter). With only slot 0 published,
/// filtering self leaves nothing to resolve.
#[tokio::test(flavor = "multi_thread")]
async fn identified_resolve_filters_own_endpoint() {
    let setup = setup_open_join().await;
    await_rendezvous_slot_visible(&setup.pkarr_resolver, &setup.epoch_key, 0).await;
    let outcome = harmony_app::community_rendezvous::resolve_rendezvous_identified(
        &setup.pkarr_resolver,
        &setup.epoch_key,
        *setup.alice_ep.node_id().as_bytes(), // we ARE alice
        wall_ms(),
        &harmony_app::community_rendezvous::rendezvous_config_from_env(),
    )
    .await;
    assert!(
        outcome.payload.is_none(),
        "own beacon record must be filtered, not returned as a dial candidate"
    );
}
```

Adjust field access to the actual `OpenJoinSetup` struct fields (they exist: `pkarr_resolver`, `epoch_key`, `alice_ep`). If `community_rendezvous` items aren't re-exported for integration tests, add `pub` visibility as needed (the module is already `pub` — check `lib.rs`'s `pub mod community_rendezvous`).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(identified_resolve)'`
Expected: FAIL — `resolve_rendezvous_identified` not found.

- [ ] **Step 3: Implement**

In `community_rendezvous.rs` (below `resolve_rendezvous`, mirroring its doc style):

```rust
/// ZEB-824: a resolved beacon with the outer record's identity preserved, so a
/// member-side caller can derive the beacon's `OwnerAddr` and gate on
/// membership. The plain [`resolve_rendezvous`] decode discards the outer
/// [`harmony_pkarr::PkarrRoutingRecord`]; open-join keeps using it (a joiner
/// defers identity trust to admission — module doc above).
#[derive(Debug, Clone)]
pub struct IdentifiedBeacon {
    pub payload: ReachabilityAnnouncePayload,
    /// The outer record's `harmony_identity_pub` (inner-sig-verified by
    /// `PkarrResolver::resolve`): X25519(32) ‖ Ed25519(32).
    pub beacon_identity_pub: [u8; 64],
}

/// Client-side [`SlotResolver`] that keeps the outer record's identity and
/// filters out our own endpoint. Mirrors the core `PkarrSlotResolver` probe
/// (derive slot vk → resolve → post-await freshness re-check → decode); it
/// exists because the core decode closure only sees the routing blob, so the
/// identity cannot be recovered from inside a closure.
struct IdentifiedSlotResolver {
    pkarr: Arc<PkarrResolver>,
    epoch_key_bytes: Vec<u8>,
    /// Our own iroh endpoint id: a record pointing at ourselves reads as an
    /// EMPTY slot, so the escalating-batch driver widens to the other slots
    /// (spec §5 self-dial hazard; ZEB-806 lesson — compare 32-byte endpoint
    /// ids, never 16-byte device ids).
    self_endpoint_id: [u8; 32],
}

#[async_trait::async_trait]
impl harmony_pkarr::rendezvous::SlotResolver<IdentifiedBeacon> for IdentifiedSlotResolver {
    async fn resolve_slot(&self, slot_index: u16, epoch_id: u64) -> Option<IdentifiedBeacon> {
        let info = rendezvous_info(slot_index, epoch_id);
        let vk = derive_ephemeral_key(PkarrCase::Community, &self.epoch_key_bytes, &info)
            .verifying_key();
        let rec = match self.pkarr.resolve(&vk).await {
            Ok(Some(rec)) => rec,
            Ok(None) => return None,
            Err(e) => {
                tracing::debug!(slot = slot_index, error = ?e,
                    "identified rendezvous probe errored — treating as a miss");
                return None;
            }
        };
        // Post-await freshness re-check, same as the core resolver (PR#306
        // stale-clock lesson).
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        rec.verify_freshness(now_ms).ok()?;
        let payload: ReachabilityAnnouncePayload =
            ciborium::from_reader(rec.routing_blob.as_slice()).ok()?;
        if payload.iroh_node_id == self.self_endpoint_id {
            return None;
        }
        Some(IdentifiedBeacon {
            payload,
            beacon_identity_pub: rec.harmony_identity_pub,
        })
    }
}

/// ZEB-824 production entry point: like [`resolve_rendezvous`], but yields an
/// [`IdentifiedBeacon`] and treats our own record as an empty slot.
pub async fn resolve_rendezvous_identified(
    pkarr: &Arc<PkarrResolver>,
    epoch_key: &EpochKey,
    self_endpoint_id: [u8; 32],
    now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> RendezvousResolveOutcome<IdentifiedBeacon> {
    let resolver = IdentifiedSlotResolver {
        pkarr: Arc::clone(pkarr),
        epoch_key_bytes: epoch_key.as_bytes().to_vec(),
        self_endpoint_id,
    };
    resolve_rendezvous_with(&resolver, now_ms, cfg).await
}
```

Note: `epoch_key_bytes` here is not `Zeroizing` like the core resolver's `ikm` — match the core's hygiene by wrapping it: `zeroize::Zeroizing<Vec<u8>>` if `zeroize` is already a direct dependency of harmony-app; if not, keep the plain `Vec<u8>` (the same key bytes already live unwrapped in `EpochKey` throughout this crate — do not add a new dependency for this).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(identified_resolve)'`
Expected: both PASS.

- [ ] **Step 5: Run the module's existing suite (no regression) and commit**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_rendezvous) or test(open_join)'`
Expected: all PASS.

```bash
git add src-tauri/src/community_rendezvous.rs src-tauri/tests/misc/community_open_join_cross_wan_integration.rs
git commit -m "ZEB-824: identity-preserving, self-filtering rendezvous resolve"
```

---

### Task 2: `GatewayBootstrapTelemetry` + `gatewayBootstrap` snapshot block

**Files:**
- Modify: `src-tauri/src/network_health.rs`

**Interfaces:**
- Consumes: the `CommunityRelayPullTelemetry` pattern (`network_health.rs:908-1010`) and the `NetworkHealthSnapshot` field conventions (`network_health.rs:37-95`).
- Produces (Task 3 + Task 4 consume):
  - `pub struct GatewayBootstrapTelemetry` with `pub fn new()`, `pub fn record_pass_start(&self)`, `pub fn record_outcome(&self, community: &[u8; 16], outcome: GatewayBootstrapOutcome)`, `pub fn summary(&self) -> GatewayBootstrapHealth`.
  - `#[derive(Debug, Clone, Copy, PartialEq)] pub enum GatewayBootstrapOutcome { Healthy, StarvedWaiting, NoBeacon, BeaconSeeded, RejectedNonMember }`.
  - `NetworkHealthSnapshot.gateway_bootstrap: Option<GatewayBootstrapHealth>` and `NetworkHealthService::set_gateway_bootstrap_source(Arc<GatewayBootstrapTelemetry>)`.

- [ ] **Step 1: Write the failing test**

In `network_health.rs`'s `#[cfg(test)] mod tests` (next to `snapshot_butler_deposits_section_serializes_camelcase`):

```rust
#[test]
fn gateway_bootstrap_health_serializes_camelcase() {
    let t = GatewayBootstrapTelemetry::new();
    t.record_pass_start();
    t.record_outcome(&[0xAB; 16], GatewayBootstrapOutcome::BeaconSeeded);
    t.record_outcome(&[0xCD; 16], GatewayBootstrapOutcome::NoBeacon);
    let json = serde_json::to_value(t.summary()).expect("serialize");
    assert_eq!(json["passesRun"], 1);
    assert_eq!(json["beaconsSeeded"], 1);
    assert_eq!(json["noBeacon"], 1);
    let per = json["perCommunity"].as_array().expect("perCommunity array");
    assert_eq!(per.len(), 2);
    // Sorted by communityShort for deterministic output.
    assert_eq!(per[0]["communityShort"], "abababab");
    assert_eq!(per[0]["outcome"], "beaconSeeded");
    assert!(per[0]["atMs"].as_u64().is_some());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(gateway_bootstrap_health)'`
Expected: FAIL — types not found.

- [ ] **Step 3: Implement**

Below the `CommunityRelayPullTelemetry` block, same idioms (AtomicU64 counters, `now_ms()` helper, 0-as-never sentinel):

```rust
/// ZEB-824: per-community session-bootstrap ("gateway dial") health. Written by
/// [`crate::community_gateway_dial_driver::CommunityGatewayDialDriver`], read by
/// `network_health_snapshot`. The pass counter proves the loop is alive even
/// when every community is healthy (ZEB-803 lesson).
#[derive(Debug, Default)]
pub struct GatewayBootstrapTelemetry {
    passes_run: AtomicU64,
    last_pass_ms: AtomicU64,
    beacons_seeded: AtomicU64,
    no_beacon: AtomicU64,
    rejected_non_member: AtomicU64,
    /// community bytes → (last outcome, stamped at). Bounded by the node's
    /// joined-community count (1–2 today); no ring needed.
    per_community: Mutex<HashMap<[u8; 16], (GatewayBootstrapOutcome, u64)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayBootstrapOutcome {
    Healthy,
    StarvedWaiting,
    NoBeacon,
    BeaconSeeded,
    RejectedNonMember,
}

impl GatewayBootstrapOutcome {
    fn wire(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::StarvedWaiting => "starvedWaiting",
            Self::NoBeacon => "noBeacon",
            Self::BeaconSeeded => "beaconSeeded",
            Self::RejectedNonMember => "rejectedNonMember",
        }
    }
}

impl GatewayBootstrapTelemetry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Recorded FIRST and unconditionally each pass — before the joined-set
    /// read — so an alive-but-idle loop is distinguishable from a dead task.
    pub fn record_pass_start(&self) {
        self.passes_run.fetch_add(1, Ordering::Relaxed);
        self.last_pass_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn record_outcome(&self, community: &[u8; 16], outcome: GatewayBootstrapOutcome) {
        match outcome {
            GatewayBootstrapOutcome::BeaconSeeded => {
                self.beacons_seeded.fetch_add(1, Ordering::Relaxed);
            }
            GatewayBootstrapOutcome::NoBeacon => {
                self.no_beacon.fetch_add(1, Ordering::Relaxed);
            }
            GatewayBootstrapOutcome::RejectedNonMember => {
                self.rejected_non_member.fetch_add(1, Ordering::Relaxed);
            }
            GatewayBootstrapOutcome::Healthy | GatewayBootstrapOutcome::StarvedWaiting => {}
        }
        self.per_community
            .lock()
            .expect("gateway bootstrap map lock")
            .insert(*community, (outcome, now_ms()));
    }

    pub fn summary(&self) -> GatewayBootstrapHealth {
        let last_pass = self.last_pass_ms.load(Ordering::Relaxed);
        let mut per: Vec<GatewayCommunityBootstrapHealth> = self
            .per_community
            .lock()
            .expect("gateway bootstrap map lock")
            .iter()
            .map(|(cid, (outcome, at))| GatewayCommunityBootstrapHealth {
                community_short: hex::encode(&cid[..4]),
                outcome: outcome.wire().to_string(),
                at_ms: *at,
            })
            .collect();
        per.sort_by(|a, b| a.community_short.cmp(&b.community_short));
        GatewayBootstrapHealth {
            passes_run: self.passes_run.load(Ordering::Relaxed),
            last_pass_ms: (last_pass != 0).then_some(last_pass),
            beacons_seeded: self.beacons_seeded.load(Ordering::Relaxed),
            no_beacon: self.no_beacon.load(Ordering::Relaxed),
            rejected_non_member: self.rejected_non_member.load(Ordering::Relaxed),
            per_community: per,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GatewayBootstrapHealth {
    pub passes_run: u64,
    pub last_pass_ms: Option<u64>,
    pub beacons_seeded: u64,
    pub no_beacon: u64,
    pub rejected_non_member: u64,
    pub per_community: Vec<GatewayCommunityBootstrapHealth>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GatewayCommunityBootstrapHealth {
    pub community_short: String,
    pub outcome: String,
    pub at_ms: u64,
}
```

Then wire the service (all four mirror `community_relay_pulling` exactly — copy its shape at `network_health.rs:1531/:1654/:1934`):
1. Field on `NetworkHealthService`: `gateway_bootstrap: Option<Arc<GatewayBootstrapTelemetry>>`, default `None`.
2. Setter: `pub(crate) fn set_gateway_bootstrap_source(&mut self, src: Arc<GatewayBootstrapTelemetry>) { self.gateway_bootstrap = Some(src); }`.
3. In `snapshot()` (`network_health.rs:1680`): `gateway_bootstrap: self.gateway_bootstrap.as_ref().map(|t| t.summary()),`.
4. Field on `NetworkHealthSnapshot` (after `vine_relay`):

```rust
    /// ZEB-824: member session-bootstrap ("gateway dial") health. `None` when
    /// the driver isn't wired (no iroh endpoint / no owner identity); `Some`
    /// with zeroed counters means wired-but-idle — the distinction matters for
    /// the same reason as `community_relay`'s. `#[serde(default)]` keeps
    /// pre-field snapshots forward-compatible.
    #[serde(default)]
    pub gateway_bootstrap: Option<GatewayBootstrapHealth>,
```

Fix every struct-literal construction site of `NetworkHealthSnapshot` the compiler reports (tests included) with `gateway_bootstrap: None`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(gateway_bootstrap_health) or test(snapshot_)'`
Expected: new test + all existing snapshot tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/network_health.rs
git commit -m "ZEB-824: GatewayBootstrapTelemetry + gatewayBootstrap snapshot block"
```

---

### Task 3: the driver — `community_gateway_dial_driver.rs`

**Files:**
- Create: `src-tauri/src/community_gateway_dial_driver.rs`
- Modify: `src-tauri/src/lib.rs` (one line: `pub mod community_gateway_dial_driver;` next to `pub mod community_relay_pull_driver;`)

**Interfaces:**
- Consumes: Task 1's `IdentifiedBeacon` + `resolve_rendezvous_identified`; Task 2's `GatewayBootstrapTelemetry` + `GatewayBootstrapOutcome`; `crate::community_relay_pull_driver::JoinedCommunitiesFn`; `ReachabilityResolver::{list_dialable_peers, seed_from_pkarr, supervisor}`; `SupervisorHandle::{states_snapshot, kick}` + `PeerStateWire::Connected` + `ReconnectTrigger::NewPeer`; `OwnerAddr`, `SpaceId`, `EpochKey`, `DeviceIdentityHash` (`owner_state_types`); `harmony_identity::Identity::from_public_bytes(&[u8;64]) -> Result<Identity, _>` (`.address_hash` is the `OwnerAddr` inner `[u8;16]` — `lib.rs:40161` pattern).
- Produces (Task 4 consumes): `pub struct CommunityGatewayDialDriver` with `pub fn new(ctx: Arc<dyn GatewayDialCtx>, beacons: Arc<dyn BeaconResolver>, reachability: Arc<ReachabilityResolver>, joined_communities: JoinedCommunitiesFn, self_owner: OwnerAddr) -> Self`, builder `pub fn with_telemetry(self, Arc<GatewayBootstrapTelemetry>) -> Self`, `#[cfg(test)] fn with_now_fn(self, ...)`, `pub fn wake_handle(&self) -> Arc<Notify>`, `pub async fn run_one_pass(&self)`, `pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()>`; plus `pub trait GatewayDialCtx` and `pub trait BeaconResolver` (below) and `pub struct ProdBeaconResolver { pub pkarr: Arc<harmony_pkarr::PkarrResolver>, pub self_endpoint_id: [u8; 32] }`.

- [ ] **Step 1: Create the module skeleton with seams and constants**

```rust
//! ZEB-824: member session-bootstrap driver ("gateway dial").
//!
//! Post-ZEB-815, a rebuilt member's address book starts empty, so the dial
//! supervisor has zero candidates and the node deadlocks sessionless
//! (no sessions → empty addrbook → no candidates → no sessions). This driver
//! is the session-independent escape hatch: for each joined community with no
//! live member session ("starved"), resolve the community's rendezvous beacon
//! from pkarr (the record open-join dials — knowledge-free, keyed only by the
//! epoch key), verify the beacon is a Joined member, seed it into the
//! [`ReachabilityResolver`], and kick the reconnect supervisor. Everything
//! downstream (record-gated dial, session, addrbook subscribe + snapshot,
//! state sync) is existing machinery.
//!
//! A feeder, not a dialer. Self-contained task — no inline awaits reach back
//! into start_node (see `crate::community_relay_pull_driver` for the shape).
//! Spec: docs/superpowers/specs/2026-07-27-zeb-824-member-gateway-dial-design.md

use crate::community_relay_pull_driver::JoinedCommunitiesFn;
use crate::community_rendezvous::{rendezvous_config_from_env, IdentifiedBeacon};
use crate::network_health::{GatewayBootstrapOutcome, GatewayBootstrapTelemetry};
use crate::owner_state_types::{DeviceIdentityHash, EpochKey, OwnerAddr, SpaceId};
use crate::reachability_resolver::ReachabilityResolver;
use crate::reconnect_supervisor::{PeerStateWire, ReconnectTrigger};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// Predicate-only tick cadence. Cheap (in-memory reads); pkarr IO happens only
/// for starved communities whose ladder is due.
pub const GATEWAY_DIAL_TICK_MS: u64 = 30_000;
/// Per-community resolve ladder while starved: base, doubling, cap (the
/// channel_backfill 30s→600s shape).
pub const GATEWAY_DIAL_RETRY_BASE_MS: u64 = 30_000;
pub const GATEWAY_DIAL_RETRY_CAP_MS: u64 = 600_000;

/// Community facts the driver needs from the sync engines. A trait seam (like
/// `RelayIngestCtx`) so unit tests stub it without a registry.
#[async_trait::async_trait]
pub trait GatewayDialCtx: Send + Sync {
    /// Joined members of `community` EXCLUDING self, from the locally
    /// materialized membership (persisted CRDT — survives rebuilds).
    async fn members_of(&self, community: &SpaceId) -> Vec<OwnerAddr>;
    /// The engine's spawn-time `membership_key()`. MUST match the rendezvous
    /// publisher's key choice (`lib.rs:11298`) — never the live epoch key
    /// (spec §5.3). `None` = engine not registered (transient); skip.
    async fn epoch_key_of(&self, community: &SpaceId) -> Option<EpochKey>;
}

/// The pkarr resolve seam. Prod = [`ProdBeaconResolver`]; tests stub it.
#[async_trait::async_trait]
pub trait BeaconResolver: Send + Sync {
    async fn resolve_beacon(&self, epoch_key: &EpochKey, now_ms: u64) -> Option<IdentifiedBeacon>;
}

/// Production [`BeaconResolver`]: Task 1's identified resolve with the
/// open-join env-knob config.
pub struct ProdBeaconResolver {
    pub pkarr: Arc<harmony_pkarr::PkarrResolver>,
    pub self_endpoint_id: [u8; 32],
}

#[async_trait::async_trait]
impl BeaconResolver for ProdBeaconResolver {
    async fn resolve_beacon(&self, epoch_key: &EpochKey, now_ms: u64) -> Option<IdentifiedBeacon> {
        crate::community_rendezvous::resolve_rendezvous_identified(
            &self.pkarr,
            epoch_key,
            self.self_endpoint_id,
            now_ms,
            &rendezvous_config_from_env(),
        )
        .await
        .payload
    }
}

struct LadderState {
    next_attempt_ms: u64,
    delay_ms: u64,
}

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

pub struct CommunityGatewayDialDriver {
    ctx: Arc<dyn GatewayDialCtx>,
    beacons: Arc<dyn BeaconResolver>,
    reachability: Arc<ReachabilityResolver>,
    joined_communities: JoinedCommunitiesFn,
    self_owner: OwnerAddr,
    wake: Arc<Notify>,
    interval: Duration,
    telemetry: Option<Arc<GatewayBootstrapTelemetry>>,
    ladders: Mutex<HashMap<SpaceId, LadderState>>,
    now_fn: NowFn,
}
```

`new()` fills the obvious defaults (`interval: Duration::from_millis(GATEWAY_DIAL_TICK_MS)`, `now_fn` = wall clock via the same `now_ms()` idiom the sibling drivers use); `with_telemetry` / `wake_handle` / `spawn` are byte-for-byte the `CommunityRelayPullDriver` shapes (`community_relay_pull_driver.rs:267/278/502`). Add `#[cfg(test)] pub fn with_now_fn(mut self, f: NowFn) -> Self` for ladder tests.

- [ ] **Step 2: Write the failing predicate + pass tests**

In the module's `#[cfg(test)] mod tests`. Build stubs once:

```rust
struct StubCtx {
    members: HashMap<SpaceId, Vec<OwnerAddr>>,
    keys: HashMap<SpaceId, EpochKey>,
}
#[async_trait::async_trait]
impl GatewayDialCtx for StubCtx {
    async fn members_of(&self, c: &SpaceId) -> Vec<OwnerAddr> {
        self.members.get(c).cloned().unwrap_or_default()
    }
    async fn epoch_key_of(&self, c: &SpaceId) -> Option<EpochKey> {
        self.keys.get(c).cloned()
    }
}

struct StubBeacons {
    hit: Option<IdentifiedBeacon>,
    calls: std::sync::atomic::AtomicU64,
}
#[async_trait::async_trait]
impl BeaconResolver for StubBeacons {
    async fn resolve_beacon(&self, _k: &EpochKey, _now: u64) -> Option<IdentifiedBeacon> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.hit.clone()
    }
}
```

Construct the `ReachabilityResolver` + `SupervisorHandle` the way `reachability_resolver.rs`'s own tests do (`~:1250`, `resolver.set_supervisor(handle.clone())`). Helper `fn beacon(owner_identity: &harmony_identity::Identity, node_id: [u8; 32]) -> IdentifiedBeacon` builds a payload with `iroh_node_id: node_id`, fresh `announced_at_ms`, and `beacon_identity_pub` = the identity's 64-byte public bytes (use the same identity-fixture helper the resolver tests use; the beacon's `OwnerAddr` must equal `OwnerAddr(identity.address_hash)`).

Tests (each: build driver with stubs + settable clock, call `run_one_pass().await`, assert):

Test 1 in full — it is the template the rest follow (same construction, different
stub contents + assertions):

```rust
// 1. Empty resolver, one starved community with a valid member beacon:
//    pass seeds + kicks.
#[tokio::test]
async fn starved_community_with_member_beacon_seeds_and_kicks() {
    let community = SpaceId([0x11; 16]);
    let (member_identity, member_identity_pub) = test_identity(1); // same fixture helper the resolver tests use
    let member_owner = OwnerAddr(member_identity.address_hash);
    let beacon_node_id = [0x22; 32];
    let resolver = test_reachability_resolver(); // as reachability_resolver.rs tests build it
    let handle = crate::reconnect_supervisor::SupervisorHandle::new();
    resolver.set_supervisor(handle.clone());
    let ctx = Arc::new(StubCtx {
        members: HashMap::from([(community, vec![member_owner])]),
        keys: HashMap::from([(community, EpochKey::new([0x33; 32]))]),
    });
    let beacons = Arc::new(StubBeacons {
        hit: Some(IdentifiedBeacon {
            payload: test_payload(beacon_node_id), // fresh announced_at_ms
            beacon_identity_pub: member_identity_pub,
        }),
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let driver = CommunityGatewayDialDriver::new(
        ctx,
        Arc::clone(&beacons) as Arc<dyn BeaconResolver>,
        Arc::clone(&resolver),
        Arc::new(move || vec![community]),
        OwnerAddr([0xFF; 16]), // self ≠ member
    );
    driver.run_one_pass().await;
    assert_eq!(beacons.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        resolver.resolve_by_node_id(&beacon_node_id).is_some(),
        "beacon record must be seeded into the resolver"
    );
    // seed_from_pkarr's update auto-kick may fire first and the explicit kick
    // coalesces — either way exactly NewPeer is pending.
    assert_eq!(
        handle.pending_trigger(beacon_node_id),
        Some(ReconnectTrigger::NewPeer)
    );
}

// 2. A Connected member ⇒ healthy ⇒ no resolve call.
#[tokio::test]
async fn connected_member_means_healthy_no_resolve() { /* ... */
    // Seed the resolver with the member's record, then handle.mark_connected(node_id).
    // assert: beacons.calls == 0
}

// 3. A Connected NON-member does not mask starvation.
#[tokio::test]
async fn connected_non_member_does_not_mask_starvation() { /* ... */
    // Seed + mark_connected a node whose owner is NOT in members_of(X).
    // assert: beacons.calls == 1
}

// 4. Solo community (members_of == []) is never starved.
#[tokio::test]
async fn solo_community_never_resolves() { /* beacons.calls == 0 */ }

// 5. Membership gate: beacon identity NOT in members_of ⇒ no seed, no kick,
//    outcome RejectedNonMember.
#[tokio::test]
async fn non_member_beacon_is_rejected_not_seeded() { /* ... */
    // assert: resolver.resolve_by_node_id(&beacon_node_id).is_none()
    // assert: handle.pending_trigger(beacon_node_id).is_none()
}

// 6. Ladder: repeated no-beacon passes back off 30s → 60s → ... → 600s cap;
//    a healed community resets to base. Drive with with_now_fn + an
//    Arc<AtomicU64> clock; count beacons.calls per simulated time step.
#[tokio::test]
async fn ladder_backs_off_and_resets_on_heal() { /* ... */ }

// 7. No supervisor installed: seed still lands, no panic.
#[tokio::test]
async fn missing_supervisor_still_seeds() { /* resolver without set_supervisor */ }

// 8. Pass counter increments even with zero joined communities (ZEB-803 shape).
#[tokio::test]
async fn pass_counter_advances_on_idle_pass() { /* telemetry.summary().passes_run == 1 */ }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_gateway_dial_driver)'`
Expected: FAIL — `run_one_pass` unimplemented (skeleton compiles, logic missing).

- [ ] **Step 4: Implement `run_one_pass` + the predicate + the ladder**

```rust
impl CommunityGatewayDialDriver {
    /// One pass over every joined community. Spec §5.
    pub async fn run_one_pass(&self) {
        if let Some(t) = self.telemetry.as_ref() {
            // ZEB-803: unconditionally, BEFORE the joined-set read.
            t.record_pass_start();
        }
        let now_ms = (self.now_fn)();
        // Connected node-ids, once per pass. No supervisor (pre-install race,
        // spec §6) ⇒ empty set: starved verdicts still stand, kicks skipped.
        let connected: HashSet<[u8; 32]> = self
            .reachability
            .supervisor()
            .map(|s| {
                s.states_snapshot()
                    .into_iter()
                    .filter(|(_, st)| matches!(st, PeerStateWire::Connected { .. }))
                    .map(|(id, _)| id)
                    .collect()
            })
            .unwrap_or_default();
        // owner → freshest node-id, once per pass (the dial view).
        let owner_nodes: HashMap<OwnerAddr, [u8; 32]> = self
            .reachability
            .list_dialable_peers()
            .into_iter()
            .map(|(owner, entry)| (owner, entry.payload.iroh_node_id))
            .collect();

        for community in (self.joined_communities)() {
            let members = self.ctx.members_of(&community).await;
            if members.is_empty() {
                // Solo community: nothing to dial, never starved (spec §4).
                continue;
            }
            let starved = !members.iter().any(|m| {
                owner_nodes
                    .get(m)
                    .is_some_and(|node_id| connected.contains(node_id))
            });
            if !starved {
                let healed = self
                    .ladders
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&community)
                    .is_some();
                if healed {
                    tracing::info!(community = ?community, "ZEB-824: community healed — member session live, bootstrap ladder reset");
                }
                self.record(&community, GatewayBootstrapOutcome::Healthy);
                continue;
            }
            // Starved. Due per the per-community ladder?
            let due = {
                let mut ladders = self.ladders.lock().unwrap_or_else(|p| p.into_inner());
                match ladders.get_mut(&community) {
                    None => {
                        // First starved sighting: attempt NOW, arm the base delay.
                        tracing::info!(community = ?community, "ZEB-824: community starved — no live member session; resolving rendezvous beacon");
                        ladders.insert(
                            community,
                            LadderState {
                                next_attempt_ms: now_ms + GATEWAY_DIAL_RETRY_BASE_MS,
                                delay_ms: GATEWAY_DIAL_RETRY_BASE_MS,
                            },
                        );
                        true
                    }
                    Some(l) if now_ms >= l.next_attempt_ms => {
                        l.delay_ms = (l.delay_ms * 2).min(GATEWAY_DIAL_RETRY_CAP_MS);
                        l.next_attempt_ms = now_ms + l.delay_ms;
                        true
                    }
                    Some(_) => false,
                }
            };
            if !due {
                self.record(&community, GatewayBootstrapOutcome::StarvedWaiting);
                continue;
            }
            let Some(epoch_key) = self.ctx.epoch_key_of(&community).await else {
                tracing::debug!(community = ?community, "ZEB-824: no engine registered; skipping this pass");
                continue;
            };
            let Some(hit) = self.beacons.resolve_beacon(&epoch_key, now_ms).await else {
                self.record(&community, GatewayBootstrapOutcome::NoBeacon);
                continue;
            };
            let Ok(identity) =
                harmony_identity::Identity::from_public_bytes(&hit.beacon_identity_pub)
            else {
                self.record(&community, GatewayBootstrapOutcome::RejectedNonMember);
                continue;
            };
            let beacon_owner = OwnerAddr(identity.address_hash);
            // Secondary self-guard (primary is the resolve-layer endpoint-id
            // filter): a same-owner sibling record is the fleet seed path's
            // job, not ours (spec §5.b).
            if beacon_owner == self.self_owner || !members.contains(&beacon_owner) {
                self.record(&community, GatewayBootstrapOutcome::RejectedNonMember);
                tracing::info!(community = ?community, "ZEB-824: beacon rejected (self-owner or non-member identity)");
                continue;
            }
            let node_id = hit.payload.iroh_node_id;
            self.reachability
                .seed_from_pkarr(beacon_owner, DeviceIdentityHash([0u8; 16]), hit.payload)
                .await;
            if let Some(sup) = self.reachability.supervisor() {
                // Explicit kick: idempotent with the seed's auto-kick (the
                // dirty set coalesces; NewPeer either way).
                sup.kick(node_id, ReconnectTrigger::NewPeer);
            }
            self.record(&community, GatewayBootstrapOutcome::BeaconSeeded);
            tracing::info!(community = ?community, "ZEB-824: rendezvous beacon seeded — reconnect supervisor kicked");
        }
    }

    fn record(&self, community: &SpaceId, outcome: GatewayBootstrapOutcome) {
        if let Some(t) = self.telemetry.as_ref() {
            t.record_outcome(&community.0, outcome);
        }
    }
}
```

(If `SpaceId`'s inner bytes aren't `pub` as `.0`, use its existing byte accessor — grep how `community_relay_pull_driver` telemetry calls pass `&[u8;16]`.) `spawn` is the sibling-driver shape: immediate `run_one_pass().await`, then `interval` + `Skip` + `select!` on wake/tick.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_gateway_dial_driver)'`
Expected: all 8 PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_gateway_dial_driver.rs src-tauri/src/lib.rs
git commit -m "ZEB-824: community gateway dial driver — starved predicate, ladder, seed+kick pass"
```

---

### Task 4: boot wiring — spawn the driver, install telemetry, abort on stop

**Files:**
- Modify: `src-tauri/src/lib.rs` (the iroh-endpoint gate that wires the relay-pull + vine drivers, `~10838-11000`; NodeState fields; the abort site `~13058`; the network-health source-install site — grep `set_community_relay_pull_source` for it)

**Interfaces:**
- Consumes: Task 3's driver + `ProdBeaconResolver`; Task 2's telemetry + setter; the `joined_snapshot` `Arc<Mutex<Vec<SpaceId>>>` already built at `lib.rs:10839` (clone a second `JoinedCommunitiesFn` from it, exactly like `lib.rs:10885-10890`); `registry` (`Arc<CommunitySyncRegistry>`), `crdt-state`-independent engine accessors (`engine_arc`, `state()`, `admin_addr()`, `membership_key()`, `MemberStatus::Joined` — the `pkarr_resolver_adapter.rs:199-215` pattern); `self_owner` (`OwnerAddr`), the iroh endpoint Arc (`ep_arc` — self endpoint id via the same accessor the vine wiring uses), `reachability_resolver`, `pkarr_resolver` Arc.
- Produces: a running driver in production; `ProdGatewayDialCtx` (in `community_gateway_dial_driver.rs`, prod-only impl).

- [ ] **Step 1: Implement `ProdGatewayDialCtx`** (add to `community_gateway_dial_driver.rs`; no unit tests — it is registry plumbing, integration-covered):

```rust
/// Production [`GatewayDialCtx`] over the community sync registry.
pub struct ProdGatewayDialCtx {
    pub registry: Arc<crate::community_state_sync::CommunitySyncRegistry>,
    pub self_owner: OwnerAddr,
}

#[async_trait::async_trait]
impl GatewayDialCtx for ProdGatewayDialCtx {
    async fn members_of(&self, community: &SpaceId) -> Vec<OwnerAddr> {
        let Some(engine) = self.registry.engine_arc(community).await else {
            return Vec::new();
        };
        let state_arc = engine.state();
        let st = state_arc.lock().await;
        let mat = st.materialized(engine.admin_addr());
        mat.members
            .iter()
            .filter(|(addr, m)| {
                m.status == crate::community_state_sync::MemberStatus::Joined
                    && **addr != self.self_owner
            })
            .map(|(addr, _)| *addr)
            .collect()
    }

    async fn epoch_key_of(&self, community: &SpaceId) -> Option<EpochKey> {
        // Spawn-time membership_key(), matching the rendezvous PUBLISHER
        // (lib.rs:11298) — deliberately NOT live_epoch_key (spec §5.3/§9).
        Some(self.registry.engine_arc(community).await?.membership_key())
    }
}
```

(Adjust the `MemberStatus` import path and the `members` map iteration to the actual types at `pkarr_resolver_adapter.rs:199-215` — same access, different filter.)

- [ ] **Step 2: Wire construction + spawn** in the iroh-endpoint gate, directly after the vine-pull driver block (`lib.rs:~10960+`), following its comment style:

```rust
// ZEB-824: member session-bootstrap driver ("gateway dial"). Spawned in
// the same iroh-endpoint gate; a feeder for the reconnect supervisor —
// resolves the community rendezvous beacon from pkarr when a community
// has no live member session and seeds it into the reachability
// resolver. `driver.spawn()` returns immediately (never inline-awaited;
// start_node inline-await hazard).
let gateway_bootstrap_telemetry =
    std::sync::Arc::new(crate::network_health::GatewayBootstrapTelemetry::new());
gateway_bootstrap_telemetry_for_state =
    Some(std::sync::Arc::clone(&gateway_bootstrap_telemetry));
let gateway_joined: crate::community_relay_pull_driver::JoinedCommunitiesFn = {
    let s = std::sync::Arc::clone(&joined_snapshot);
    std::sync::Arc::new(move || s.lock().unwrap_or_else(|p| p.into_inner()).clone())
};
let gateway_driver = std::sync::Arc::new(
    crate::community_gateway_dial_driver::CommunityGatewayDialDriver::new(
        std::sync::Arc::new(
            crate::community_gateway_dial_driver::ProdGatewayDialCtx {
                registry: std::sync::Arc::clone(&registry),
                self_owner,
            },
        ),
        std::sync::Arc::new(
            crate::community_gateway_dial_driver::ProdBeaconResolver {
                pkarr: std::sync::Arc::clone(&pkarr_resolver_for_gateway),
                self_endpoint_id: gateway_self_endpoint_id,
            },
        ),
        std::sync::Arc::clone(&reachability_resolver),
        gateway_joined,
        self_owner,
    )
    .with_telemetry(gateway_bootstrap_telemetry),
);
gateway_dial_driver_handle_opt = Some(gateway_driver.spawn());
```

Binding notes for the implementer: `self_owner` in this scope is the `OwnerAddr`-typed value the relay wiring uses as `self_owner.0` at `lib.rs:10900` (pass the full `OwnerAddr` here); `gateway_self_endpoint_id` = the same self-endpoint-id expression the vine driver wiring passes for its ZEB-806 filter (grep `self_endpoint_id` in the vine construction a few lines below); `pkarr_resolver_for_gateway` = clone of the same `Arc<harmony_pkarr::PkarrResolver>` handed to `OpenJoinIrohDeps` / the pkarr publishers in this scope (grep its local name); `registry` / `reachability_resolver` / `joined_snapshot` are already in scope here (the relay wiring uses all three). Declare `gateway_dial_driver_handle_opt` / `gateway_bootstrap_telemetry_for_state` as `let mut ... = None;` alongside `community_relay_pull_driver_handle_opt` / `community_relay_pull_telemetry_for_state` and store both on `NodeState` the same way those two are stored.

- [ ] **Step 3: Abort on stop + telemetry install.** (a) At the cleanup site that aborts `community_relay_refresher_handle_opt` / the pull-driver handle (`lib.rs:~13058`), abort `gateway_dial_driver_handle_opt` identically. (b) At the `set_community_relay_pull_source` call site (grep it in lib.rs), install ours: `svc.set_gateway_bootstrap_source(t)` when `gateway_bootstrap_telemetry_for_state` is `Some`.

- [ ] **Step 4: Compile + scoped gates**

Run: `cd src-tauri && cargo check --locked --features test-fixtures` then
`scripts/test-select --context task` (from repo root)
Expected: compiles; selected tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/community_gateway_dial_driver.rs
git commit -m "ZEB-824: boot wiring — spawn gateway dial driver, telemetry into network health, abort on stop"
```

---

### Task 5: gates, spec as-implemented notes, docs

**Files:**
- Modify: `docs/superpowers/specs/2026-07-27-zeb-824-member-gateway-dial-design.md` (§5 "The decode closure" — as-implemented note)

**Steps:**

- [ ] **Step 1: Spec as-implemented note.** In spec §5's decode-closure subsection, append: the core `PkarrSlotResolver` decode closure receives only the routing blob, so the identity-preserving variant shipped as a client-side `SlotResolver` impl (`IdentifiedSlotResolver` in `community_rendezvous.rs`) that mirrors the core probe and applies the self-filter — same architecture (client-only, no core change), different mechanism than the spec's "different closure" phrasing.

- [ ] **Step 2: Headline integration test (spec §8)** — driver + real mock relay, end to end.

In `tests/misc/community_open_join_cross_wan_integration.rs` (Bob = the starved member; Alice's beacon is already published by `setup_open_join`):

```rust
/// ZEB-824 headline scenario: a member with an EMPTY reachability resolver
/// (rebuilt node, no addrbook sidecar, scouting off) bootstraps a dial
/// candidate from the rendezvous beacon in one pass. Kick coverage lives in
/// the driver's unit test (the dirty-set accessor is cfg(test)-gated); here we
/// pin the pkarr → resolve → verify → seed chain against the real mock relay.
#[tokio::test(flavor = "multi_thread")]
async fn gateway_dial_driver_bootstraps_from_rendezvous_beacon() {
    let setup = setup_open_join().await;
    await_rendezvous_slot_visible(&setup.pkarr_resolver, &setup.epoch_key, 0).await;
    let alice_node_id = *setup.alice_ep.node_id().as_bytes();

    // A local GatewayDialCtx stub (the traits are pub; integration tests
    // can't see the lib's cfg(test) stubs): alice is the one Joined member.
    struct ItCtx { community: SpaceId, alice: OwnerAddr, key: EpochKey }
    #[async_trait::async_trait]
    impl harmony_app::community_gateway_dial_driver::GatewayDialCtx for ItCtx {
        async fn members_of(&self, c: &SpaceId) -> Vec<OwnerAddr> {
            if *c == self.community { vec![self.alice] } else { vec![] }
        }
        async fn epoch_key_of(&self, _c: &SpaceId) -> Option<EpochKey> {
            Some(self.key.clone())
        }
    }

    let resolver = /* fresh empty ReachabilityResolver, as its public constructor allows */;
    let community = setup.community_id;
    let driver = Arc::new(
        harmony_app::community_gateway_dial_driver::CommunityGatewayDialDriver::new(
            Arc::new(ItCtx { community, alice: setup.alice_addr, key: setup.epoch_key.clone() }),
            Arc::new(harmony_app::community_gateway_dial_driver::ProdBeaconResolver {
                pkarr: Arc::clone(&setup.pkarr_resolver),
                self_endpoint_id: *setup.bob_ep.node_id().as_bytes(),
            }),
            Arc::clone(&resolver),
            Arc::new(move || vec![community]),
            setup.bob_addr,
        ),
    );
    driver.run_one_pass().await;
    let (owner, _payload) = resolver
        .resolve_by_node_id(&alice_node_id)
        .expect("alice's beacon must be seeded from the mock relay in one pass");
    assert_eq!(owner, setup.alice_addr, "seeded under alice's derived OwnerAddr");
}
```

Fill the `resolver` construction from `ReachabilityResolver`'s public API (its integration-visible constructor — grep how other integration tests build one; if none is public, add a `pub fn new_for_tests()` gated behind `feature = "test-fixtures"` following the crate's established test-fixtures pattern). If `setup.alice_addr`'s identity doesn't match the identity the rendezvous publisher signs records with, assert against `OwnerAddr(Identity::from_public_bytes(&record identity).address_hash)` sourced from the resolved record instead — the load-bearing assertions are "seeded" and "owner derived from the record's identity".

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(gateway_dial_driver_bootstraps)'`
Expected: PASS. Commit with the Task 5 docs commit below.

- [ ] **Step 3: Full module suites**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_gateway_dial_driver) or test(community_rendezvous) or test(identified_resolve) or test(gateway_bootstrap_health) or test(snapshot_) or test(open_join) or test(reconnect_supervisor) or test(reachability_resolver)'`
Expected: all PASS.

- [ ] **Step 4: fmt + CI-exact clippy**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean. (fmt failure ⇒ run `cargo fmt --all` and re-check.)

- [ ] **Step 5: Full sweep (final pre-PR backstop)**

Run: `scripts/test-select --full` (repo root; this is the CI-parity `--workspace --all-targets` sweep)
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add docs/superpowers/specs/2026-07-27-zeb-824-member-gateway-dial-design.md src-tauri/tests/misc/community_open_join_cross_wan_integration.rs
git commit -m "ZEB-824: headline bootstrap integration test + spec as-implemented note"
```
