# ZEB-739 — iroh library tier extraction (slice 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the harmony-client's iroh transport substrate into two new core crates (`harmony-iroh`, `harmony-tunnel-iroh`), bump the core workspace to iroh 1.0, un-fork harmony-node onto the new crates, and rewire the client as thin delegators — with behavior pinned by the existing transport tests.

**Architecture:** Extract-and-replace, core-first, across two repos. Phase 1 (core repo `zeblithic/harmony`, one atomic PR) creates the crates at iroh 1.0 (seeded from the client's already-1.0 code), then bumps the workspace and rewires harmony-node. Phase 2 (client repo `zeblithic/harmony-client`, gated on the core PR merging) bumps the lockstep pins and delegates. See design: `harmony-client/docs/specs/2026-07-23-zeb-739-iroh-tier-extraction-design.md`.

**Tech Stack:** Rust, iroh 1.0, tokio, tokio-util, blake3, futures-util, `harmony-tunnel` (no_std sans-I/O PQ session, unchanged), `harmony-identity`. Harmony 44-crate workspace conventions.

## Global Constraints

- **Core repo paths** are under `/Users/zeblith/work/zeblithic/harmony`; **client repo paths** under `/Users/zeblith/work/zeblithic/harmony-client`. Every path below is relative to its repo root.
- **iroh 1.0 everywhere in core after Phase 1.** Workspace `iroh` dep moves `0.91 → 1.0`. During Phase 1 tasks 1–4 the new crates pin `iroh = "1.0"` **directly** (not `workspace = true`) so they compile in isolation while the workspace/harmony-node stay on 0.91; task 5 flips the workspace to 1.0 and switches the crates to `iroh = { workspace = true }`.
- **Byte/wire preservation is load-bearing.** Preserve exactly: the PQ tunnel wire format (owned by `harmony-tunnel`, unchanged), 4-byte **big-endian** length framing, the `iroh/<hex EndpointId>` locator convention, and `node_id = blake3(ML-DSA-65 pubkey)`.
- **No new third-party dependencies.** Only iroh, tokio, tokio-util, blake3, futures-util, async-trait, thiserror, and in-tree harmony crates.
- **New crates are std, I/O-bearing** — declared in `[workspace.dependencies]` **without** `default-features = false` (the `harmony-rawlink` form), and are NOT `no_std`.
- **Zenoh bridge is out of scope** and must remain untouched + functional client-side (`zenoh_iroh_link.rs`, `zenoh_iroh_transport.rs`, `iroh_zenoh_registration.rs`, `vendor/zenoh-link/`).
- **Preserve existing tests.** Tests move with the code they cover; the existing client transport suite is the behavior anchor and must pass unchanged post-delegation.
- **Core gates:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run` (scope to affected crates during iteration; full workspace before opening the PR).
- **Client gates (Phase 2):** `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures -- -D warnings`, and scoped nextest via `scripts/test-select` first (a lib change relinks ~97 integ binaries ≈ 50 min — do NOT run the full client suite per-iteration; full only as the final gate).
- **Branches:** core `zeb-739-iroh-tier` off `488a58b`; client `zeb-739-delegate-iroh-tier` (created in Phase 2, off latest `origin/main`). No worktrees — `git checkout -b` in the main repo.
- **Do not commit onto `main`.** Branch first. Do not commit the spec/plan docs to `main`; they ride in with the implementation branch (Phase 1 core branch carries no client docs — commit these two docs on the Phase 2 client branch, or leave uncommitted; they are reference, not code).

---

## PHASE 1 — Core PR (`zeblithic/harmony`, branch `zeb-739-iroh-tier`)

> Pre-flight: `git -C /Users/zeblith/work/zeblithic/harmony checkout -b zeb-739-iroh-tier 488a58b`. All Phase-1 tasks commit onto this branch.

### Task 1: Scaffold `harmony-iroh` crate (compiles in isolation at iroh 1.0)

**Files:**
- Create: `crates/harmony-iroh/Cargo.toml`
- Create: `crates/harmony-iroh/src/lib.rs`
- Modify: `Cargo.toml` (workspace root — add member + dependency-table entry)

**Interfaces:**
- Produces: the `harmony-iroh` crate skeleton with empty `endpoint`, `dispatch`, `error` modules.

- [ ] **Step 1: Create the crate manifest.** `crates/harmony-iroh/Cargo.toml`:

```toml
[package]
name = "harmony-iroh"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version.workspace = true
description = "Reusable iroh endpoint + ALPN-dispatch substrate for Harmony transports"

[dependencies]
iroh = "1.0"                      # DIRECT pin during tasks 1–4; switched to workspace in task 5
tokio = { workspace = true }
async-trait = { workspace = true }
thiserror = { workspace = true }
futures = { workspace = true }    # confirm the exact futures/futures-util crate the client's iroh_endpoint uses

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "time"] }
```

(Verify each `{ workspace = true }` dep exists in the root `[workspace.dependencies]`; if `async-trait`/`futures` are not there, add them or pin directly to match the client's versions.)

- [ ] **Step 2: Create `src/lib.rs`** with module declarations and crate docs:

```rust
//! Reusable iroh transport substrate: a persistent `Endpoint` wrapper with
//! relay/rebind lifecycle, plus an ALPN-keyed inbound-connection dispatch seam.
//!
//! App-agnostic: no identity/keychain, no hardcoded ALPN wire strings, no
//! concrete protocol acceptors. Extracted from harmony-client (ZEB-739).

pub mod dispatch;
pub mod endpoint;
mod error;

pub use error::IrohEndpointError;
```

- [ ] **Step 3: Create placeholder modules** so the crate compiles: `src/endpoint.rs`, `src/dispatch.rs`, `src/error.rs` each with a minimal stub (e.g. `error.rs` defines `pub enum IrohEndpointError {}` with `#[derive(Debug, thiserror::Error)]` — real variants land in Task 2).

- [ ] **Step 4: Register in the workspace.** In root `Cargo.toml`: add `"crates/harmony-iroh"` to `[workspace] members`, and `harmony-iroh = { path = "crates/harmony-iroh" }` to `[workspace.dependencies]` (WITHOUT `default-features = false` — I/O-bearing, harmony-rawlink form).

- [ ] **Step 5: Verify it compiles in isolation.**

Run: `cargo build -p harmony-iroh`
Expected: builds clean (harmony-node still on iroh 0.91, unaffected — nothing links the two).

- [ ] **Step 6: Commit.** `git add crates/harmony-iroh Cargo.toml && git commit -m "harmony-iroh: scaffold crate at iroh 1.0 (ZEB-739)"`

---

### Task 2: Port `IrohEndpoint` into `harmony-iroh` with the SecretKey + ALPN seams

**Files:**
- Modify: `crates/harmony-iroh/src/endpoint.rs`, `crates/harmony-iroh/src/error.rs`
- Reference (copy source from): `harmony-client/src-tauri/src/iroh_endpoint.rs` (`IrohEndpoint` struct + impl `:114-425`; `IrohEndpointError` `:128-142`; tests `:585+`)

**Interfaces:**
- Consumes: `iroh::{Endpoint, SecretKey, RelayUrl}` (1.0).
- Produces: `harmony_iroh::endpoint::{IrohEndpoint, RelayConfig, AlpnConfig}`; `IrohEndpoint::new_with_secret(secret, relays, alpn) -> Result<Self, IrohEndpointError>`; `pub fn inner(&self) -> &iroh::Endpoint`; the lifecycle methods (`node_id`, `home_relay`, `direct_addresses`, `bound_sockets`, `apply_relay_urls`, `relay_map_urls`, `watch_addr_stream`, `network_change`, `shutdown`).

- [ ] **Step 1: Copy the struct + error + lifecycle impl** from `harmony-client/src-tauri/src/iroh_endpoint.rs` (`:114-425`, `:128-142`) into `endpoint.rs`/`error.rs`. Do NOT copy the `alpn` module (`:51-103`) or `load_or_create_secret_key` (`:461-510`) — those stay app-side.

- [ ] **Step 2: Apply Seam 1a (inject SecretKey).** Replace any internal key-loading with a constructor that takes the key:

```rust
pub struct AlpnConfig {
    /// ALPNs this endpoint advertises/accepts on inbound connections.
    pub advertised: Vec<Vec<u8>>,
}

pub struct RelayConfig {
    /// Configured relay URLs (the ZEB-624 relay map). Empty = relay-disabled.
    pub urls: Vec<iroh::RelayUrl>,
    // carry over the exact relay-mode fields the client's constructor used
    // (Custom vs Disabled) — confirm against iroh_endpoint.rs:163-260.
}

impl IrohEndpoint {
    pub async fn new_with_secret(
        secret: iroh::SecretKey,
        relays: RelayConfig,
        alpn: AlpnConfig,
    ) -> Result<Self, IrohEndpointError> {
        // body ported from new_with_secret_and_relays (iroh_endpoint.rs:163),
        // with .alpns(alpn.advertised) instead of a hardcoded list,
        // and .secret_key(secret) instead of a loaded key.
    }
}
```

- [ ] **Step 3: Apply Seam 1b (ALPN from config).** Every place the old code referenced the hardcoded `alpn::HARMONY_*` constants for `.alpns(...)` now uses `alpn.advertised`. The endpoint no longer knows any wire strings.

- [ ] **Step 4: Make `inner()` cross-crate visible.** Change `pub(crate) fn inner()` → `pub fn inner(&self) -> &iroh::Endpoint` (Risk #3 in the spec — the link manager + tunnel driver need the raw endpoint). Add a rustdoc note on the escape-hatch contract.

- [ ] **Step 5: Carry the endpoint's own unit tests.** Copy the `#[cfg(test)] mod tests` from `iroh_endpoint.rs:585+` that exercise the endpoint (relay reconciliation, node_id/home_relay snapshots). Drop any test asserting the removed keychain/alpn-const behavior (those stay client-side). Adjust imports to the new module path.

- [ ] **Step 6: Build + test the crate in isolation.**

Run: `cargo nextest run -p harmony-iroh`
Expected: PASS (endpoint construction + lifecycle tests green at iroh 1.0).

- [ ] **Step 7: Commit.** `git commit -am "harmony-iroh: port IrohEndpoint with injected SecretKey + caller ALPN config (ZEB-739)"`

---

### Task 3: Port the dispatch trait + generalize the multiplexer

**Files:**
- Modify: `crates/harmony-iroh/src/dispatch.rs`
- Reference: `harmony-client/src-tauri/src/iroh_invite_acceptor.rs:167-173` (trait); `iroh_friend_acceptor.rs:2454-2540` (`route_handshake_alpn`, `FriendDispatchTarget`, `MultiplexHandshakeDispatcher`)

**Interfaces:**
- Produces: `harmony_iroh::dispatch::{IrohHandshakeDispatcher, AlpnDispatchTable}`; a generic accept helper `spawn_accept(endpoint, table)` usable by harmony-node.

- [ ] **Step 1: Define the trait** (lifted verbatim from `iroh_invite_acceptor.rs:167-173`):

```rust
use std::collections::HashMap;
use std::sync::Arc;
use async_trait::async_trait;

#[async_trait]
pub trait IrohHandshakeDispatcher: Send + Sync {
    async fn handle_connection(&self, conn: iroh::endpoint::Connection);
}
```

- [ ] **Step 2: Write the failing test for the routing table:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    struct Recorder(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl IrohHandshakeDispatcher for Recorder {
        async fn handle_connection(&self, _c: iroh::endpoint::Connection) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[test]
    fn table_routes_by_alpn() {
        let mut t = AlpnDispatchTable::new();
        let r = Arc::new(Recorder(Default::default()));
        t.insert(b"proto/a".to_vec(), r.clone());
        assert!(t.dispatch_for(b"proto/a").is_some());
        assert!(t.dispatch_for(b"proto/unknown").is_none());
    }
}
```

Run: `cargo test -p harmony-iroh dispatch::tests::table_routes_by_alpn` → FAIL (`AlpnDispatchTable` not defined).

- [ ] **Step 3: Implement `AlpnDispatchTable`:**

```rust
#[derive(Default)]
pub struct AlpnDispatchTable {
    routes: HashMap<Vec<u8>, Arc<dyn IrohHandshakeDispatcher>>,
}
impl AlpnDispatchTable {
    pub fn new() -> Self { Self::default() }
    pub fn insert(&mut self, alpn: Vec<u8>, d: Arc<dyn IrohHandshakeDispatcher>) {
        self.routes.insert(alpn, d);
    }
    pub fn dispatch_for(&self, alpn: &[u8]) -> Option<Arc<dyn IrohHandshakeDispatcher>> {
        self.routes.get(alpn).cloned()
    }
}
```

- [ ] **Step 4: Add the generic accept helper** for harmony-node (and the future bridge). Model the accept/spawn/ALPN-read loop on the client's `spawn_accept_loop` (`zenoh_iroh_transport.rs:568-829`) but reduced to: `ep.accept()` → read `conn.alpn()` → `table.dispatch_for(alpn)` → spawn `handle_connection`. Signature:

```rust
pub fn spawn_accept(
    endpoint: std::sync::Arc<crate::endpoint::IrohEndpoint>,
    table: std::sync::Arc<AlpnDispatchTable>,
) -> tokio::task::JoinHandle<()>;
```

(Keep the connection-cap / offloaded-handshake structure the client uses if it is load-bearing for harmony-node; otherwise keep it minimal — node has a single ALPN.)

- [ ] **Step 5: Run tests.** `cargo nextest run -p harmony-iroh` → PASS.

- [ ] **Step 6: Commit.** `git commit -am "harmony-iroh: dispatch trait + generic ALPN dispatch table + accept helper (ZEB-739)"`

---

### Task 4: Create `harmony-tunnel-iroh` (TunnelManager + tunnel_task with seams)

**Files:**
- Create: `crates/harmony-tunnel-iroh/Cargo.toml`, `crates/harmony-tunnel-iroh/src/lib.rs`, `.../src/manager.rs`, `.../src/driver.rs`, `.../src/peer.rs`
- Modify: root `Cargo.toml` (member + dep entry)
- Reference: `harmony-client/src-tauri/src/tunnel_manager.rs` (1305 LOC) + `tunnel_task.rs` (1856 LOC)

**Interfaces:**
- Consumes: `harmony-iroh` (`IrohEndpoint`), `harmony-tunnel` (`TunnelSession`, `TunnelEvent`, `TunnelAction`), `harmony-identity` (`PqIdentity`, `PqPrivateIdentity`), iroh 1.0, tokio, tokio-util, blake3, futures-util.
- Produces: `harmony_tunnel_iroh::{TunnelManager, TunnelPeer, CompatSink, run_tunnel_initiator, run_tunnel_responder, InboundDm, node_id_from_dsa_pubkey}`.

- [ ] **Step 1: Manifest + workspace registration.** `crates/harmony-tunnel-iroh/Cargo.toml` mirrors Task 1's form (`iroh = "1.0"` direct pin for now) plus deps: `harmony-iroh = { workspace = true }`, `harmony-tunnel = { workspace = true }` (enable its `std` feature), `harmony-identity = { workspace = true }` (std), `tokio-util = { workspace = true }`, `blake3 = { workspace = true }`, `futures-util = { workspace = true }`, `async-trait`, `thiserror`. Add member + dep-table entry to root `Cargo.toml`.

- [ ] **Step 2: Define the peer seam** (`src/peer.rs`) — replaces `owner_state_types::DeviceTunnelContact`:

```rust
/// Everything the tunnel driver needs to dial and authenticate a peer.
/// The app maps its own DeviceTunnelContact into this at the call boundary.
#[derive(Clone)]
pub struct TunnelPeer {
    pub node_id: [u8; 32],
    pub pq_identity: harmony_identity::PqIdentity,
    pub home_relay: Option<iroh::RelayUrl>,
    pub direct_addrs: Vec<std::net::SocketAddr>,
}
```

(Confirm the exact fields `dial_addr`/`peer_pq_identity` in `tunnel_task.rs:520-560` consume; add only those.)

- [ ] **Step 3: Define the compat sink seam** (in `src/manager.rs`) — replaces the concrete `ProtocolCompatRegistry` dependency:

```rust
/// Narrow sink for handshake/version outcomes (ZEB-623). The client's
/// ProtocolCompatRegistry implements this; harmony-node uses a no-op.
pub trait CompatSink: Send + Sync {
    fn record_handshake_outcome(&self, peer: [u8; 32], outcome: HandshakeOutcome);
}
```

(Define `HandshakeOutcome` from the exact set `tunnel_task.rs` reports — port `HandshakeFailure` `:70-80` as the payload. Confirm the precise consumed surface of `ProtocolCompatRegistry` before finalizing this trait — Risk #2.)

- [ ] **Step 4: Port `tunnel_task.rs` → `src/driver.rs`.** Copy `run_tunnel_initiator`/`run_tunnel_responder`/`run_tunnel_initiator_inner`/`dial_addr`/`peer_pq_identity`/`millis_since_start` and the framing helpers. Replace `crate::iroh_endpoint::IrohEndpoint` → `harmony_iroh::endpoint::IrohEndpoint`; `crate::owner_state_types::DeviceTunnelContact` → `crate::peer::TunnelPeer`; `crate::tunnel_manager::{InboundDm, TunnelCommand, TunnelManager}` → `crate::manager::…`. Preserve the 4-byte-BE framing and `LengthDelimitedCodec` config byte-for-byte. Keep the "Adapted from harmony-node" provenance note, updated to reflect the crate now being the shared home.

- [ ] **Step 5: Port `tunnel_manager.rs` → `src/manager.rs`.** Copy `TunnelManager` + `TunnelHandle`/`TunnelRole`/`TunnelHandleState`/`InboundDm`/`TunnelCommand` + `node_id_from_dsa_pubkey` (`blake3(ML-DSA-65 pubkey)` — preserve exactly). Constructor takes `Arc<IrohEndpoint>`, `Arc<PqPrivateIdentity>`, `ingest_tx`, and `Arc<dyn CompatSink>` (replacing `Arc<ProtocolCompatRegistry>`). Replace `DeviceTunnelContact` params with `TunnelPeer`.

- [ ] **Step 6: Carry the tunnel tests.** Move the `#[cfg(test)]` modules from both source files (round-trip, simultaneous-dial collision resolution, framing, `AwaitingInbound` state) into the new crate; adjust imports; supply a test `CompatSink` no-op + `TunnelPeer` fixtures.

- [ ] **Step 7: Build + test in isolation.**

Run: `cargo nextest run -p harmony-tunnel-iroh`
Expected: PASS (round-trip, collision, framing tests green at iroh 1.0).

- [ ] **Step 8: Commit.** `git commit -am "harmony-tunnel-iroh: port TunnelManager + tunnel_task with TunnelPeer/CompatSink seams (ZEB-739)"`

---

### Task 5: Bump workspace to iroh 1.0 and rewire harmony-node (the atomic un-fork)

**Files:**
- Modify: root `Cargo.toml` (`iroh = "0.91"` → `"1.0"`); `crates/harmony-iroh/Cargo.toml` + `crates/harmony-tunnel-iroh/Cargo.toml` (`iroh = "1.0"` → `iroh = { workspace = true }`)
- Modify: `crates/harmony-node/Cargo.toml` (add `harmony-iroh`, `harmony-tunnel-iroh` deps), `crates/harmony-node/src/event_loop.rs`, `crates/harmony-node/src/tunnel_bridge.rs`
- Delete: `crates/harmony-node/src/tunnel_task.rs`

**Interfaces:**
- Consumes: the two new crates.
- Produces: harmony-node on iroh 1.0, its forked transport deleted, full workspace green.

- [ ] **Step 1: Survey the node's residual iroh 0.91→1.0 API delta.** Grep `crates/harmony-node/src/event_loop.rs` for the iroh calls with no client analog: the relay-map construction (`:509-522`), the listener/acceptor endpoint builder (`:524-549`), the per-dial ephemeral endpoints (`:1721-1753`), and the accept-arm (`:1179-1247`). For each, determine the iroh 1.0 form or confirm it is replaced by a call into `harmony-iroh`/`harmony-tunnel-iroh`. Record findings inline as comments/notes before editing.

- [ ] **Step 2: Flip the versions.** Root `Cargo.toml`: `iroh = "1.0"`. Both new crates: `iroh = { workspace = true }`. Add `harmony-iroh.workspace = true` + `harmony-tunnel-iroh.workspace = true` to `crates/harmony-node/Cargo.toml`.

- [ ] **Step 3: Rewire the node's endpoint + accept.** Replace the three inline `Endpoint` builders + single-ALPN accept arm in `event_loop.rs` with: build a `harmony_iroh::IrohEndpoint::new_with_secret(node_secret, relay_config, AlpnConfig{advertised: vec![HARMONY_TUNNEL_ALPN.to_vec()]})`, and drive inbound via `harmony_iroh::dispatch::spawn_accept` with a one-entry `AlpnDispatchTable` (or a direct single-ALPN accept if simpler for the node's one protocol). Keep `HARMONY_TUNNEL_ALPN` node-side.

- [ ] **Step 4: Rewire the node's tunnel driver.** Replace calls into the deleted `tunnel_task.rs` with `harmony_tunnel_iroh::{run_tunnel_responder, run_tunnel_initiator}` and a `harmony_tunnel_iroh::TunnelManager` (with a no-op `CompatSink` and node-built `TunnelPeer`s). Update `tunnel_bridge.rs` to the new types. Where the node dropped `DmReceived` (`tunnel_task.rs:516-530`), keep dropping it (that behavior stays — the shared driver surfaces the action; the node ignores it).

- [ ] **Step 5: Delete `crates/harmony-node/src/tunnel_task.rs`** and its `mod tunnel_task;` declaration. Move any node-only tunnel tests worth keeping into the node's test tree, retargeted at the shared crate; delete tests that only covered the now-deleted duplicate.

- [ ] **Step 6: Build the whole workspace.**

Run: `cargo build --workspace`
Expected: clean — harmony-node compiles at iroh 1.0 against the new crates; no residual 0.91 references (`grep -rn 'iroh = "0.9' . || true` returns nothing; `cargo tree -i iroh@0.91` errors "package not found").

- [ ] **Step 7: Run the affected tests.**

Run: `cargo nextest run -p harmony-node -p harmony-iroh -p harmony-tunnel-iroh`
Expected: PASS.

- [ ] **Step 8: Commit.** `git commit -am "core: bump workspace to iroh 1.0, rewire harmony-node onto harmony-iroh + harmony-tunnel-iroh, delete forked tunnel_task (ZEB-739)"`

---

### Task 6: Core gates + rustdoc + open PR

**Files:** rustdoc in the two new crates' `lib.rs`/module heads.

- [ ] **Step 1: Rustdoc pass.** Ensure `harmony-iroh` and `harmony-tunnel-iroh` have crate-level + key-type rustdoc (module purpose, the seam contracts, the wire-preservation notes, an "extracted from harmony-client (ZEB-739)" provenance line).

- [ ] **Step 2: Full gates.**

Run: `cargo fmt --all -- --check`
Run: `cargo clippy --workspace --all-targets -- -D warnings`
Run: `cargo nextest run --workspace`
Expected: all clean/green. (If the workspace has a `HARMONY_LARGE_TESTS`/large-suite gate, run per its convention.)

- [ ] **Step 3: Commit any fmt/clippy fixups**, then push and open the core PR.

Run: `git push -u origin zeb-739-iroh-tier`
Then open PR `zeblithic/harmony` with the body from a drafted PR-body file; trigger the manual bots per the convergence playbook (CodeRabbit is throttled — sequence-aware; Qodo + CodeAnt auto on core). PR title: `iroh tier: extract harmony-iroh + harmony-tunnel-iroh, bump core to iroh 1.0, un-fork harmony-node (ZEB-739)`.

- [ ] **Step 4: Converge bots + CI to green; hand to Jake to merge.** Record the merged rev **R** for Phase 2. **Do NOT auto-merge.**

---

## PHASE 2 — Client PR (`zeblithic/harmony-client`, branch `zeb-739-delegate-iroh-tier`)

> **GATED:** do not start until the core PR is merged and rev **R** is known. Pre-flight: `git -C /Users/zeblith/work/zeblithic/harmony-client fetch origin && git checkout -b zeb-739-delegate-iroh-tier origin/main`.

### Task 7: Bump pins + delegate the client transport modules

**Files:**
- Modify: `src-tauri/Cargo.toml` (10 lockstep harmony pins → **R**; extend the pin-genealogy comment), `src-tauri/Cargo.lock`
- Modify: `src-tauri/src/iroh_endpoint.rs`, `src-tauri/src/tunnel_manager.rs`, `src-tauri/src/tunnel_task.rs`
- Keep unchanged: the `alpn` const module, key-provisioning, and the entire zenoh bridge

**Interfaces:**
- Consumes: `harmony_iroh::{IrohEndpoint, AlpnConfig, RelayConfig, dispatch::*}`, `harmony_tunnel_iroh::{TunnelManager, TunnelPeer, ...}` at rev **R**.

- [ ] **Step 1: Bump the pins.** In `src-tauri/Cargo.toml`, move the 10 lockstep `harmony-*` git pins `488a58bf… → R` (leave `harmony-pkarr` on its own pin); extend the pin-genealogy comment head (chain R/#<core-PR> → 488a58bf/#290 → …). `cargo update -p <each>` / `cargo build` to refresh `Cargo.lock` to R.

- [ ] **Step 2: Delegate `iroh_endpoint.rs`.** Replace the moved `IrohEndpoint` struct/impl with a re-export/delegator to `harmony_iroh::endpoint::IrohEndpoint`. KEEP the `alpn` const module and `load_or_create_secret_key` here; the client's construction site now calls `harmony_iroh::IrohEndpoint::new_with_secret(load_or_create_secret_key(...)?, RelayConfig{...}, AlpnConfig{ advertised: alpn::all_client_alpns() })`. Adjust the ~17 consumer modules only if a path/type name changed (goal: zero behavioral edits — they reach it via `crate::iroh_endpoint::…`).

- [ ] **Step 3: Delegate `tunnel_manager.rs` + `tunnel_task.rs`.** Re-export `TunnelManager`/`run_tunnel_*` from `harmony_tunnel_iroh`. Provide the `From<DeviceTunnelContact> for harmony_tunnel_iroh::TunnelPeer` mapping at the client boundary, and impl `harmony_tunnel_iroh::CompatSink for ProtocolCompatRegistry`. Consumers (`iroh_tunnel_acceptor.rs`, `iroh_tunnel_dm_transport.rs`, `dm_inbox_ingest.rs`, `owner_commands.rs`, `network_health.rs`) keep reaching them via `crate::tunnel_manager::…`.

- [ ] **Step 4: Confirm the zenoh bridge still consumes the new types.** `zenoh_iroh_transport.rs` holds `Arc<IrohEndpoint>` and installs a handshake dispatcher — verify it now points at `harmony_iroh` types with no behavioral change; the bridge otherwise untouched.

- [ ] **Step 5: Scoped gates (do NOT run the full client suite yet).**

Run: `cargo fmt --all -- --check`
Run: `cargo clippy --locked --all-targets --features test-fixtures -- -D warnings`
Run the transport-focused tests via `scripts/test-select --context task` (endpoint, tunnel round-trip, simultaneous-dial collision, framing).
Expected: fmt clean, clippy clean, targeted transport tests green (the preservation anchor).

- [ ] **Step 6: Commit.** `git commit -am "iroh_endpoint/tunnel: delegate to harmony-iroh + harmony-tunnel-iroh; bump pins to R (ZEB-739)"`

- [ ] **Step 7: Full client gate + open PR.** Run the full suite once (`scripts/test-select --context round` or the full nextest) as the final gate; push `zeb-739-delegate-iroh-tier`; open the client PR (body notes "Closes ZEB-739"); converge Qodo (auto) + CI (3-shard + gate roll-up); CodeRabbit only if un-throttled. Hand to Jake to merge. **Do NOT auto-merge.**

---

## Post-merge (both PRs merged)

- [ ] Verify ZEB-739 auto-closes ("Closes ZEB-739" in the client PR); housekeeping on both repos (sync main, delete branches, prune); verify ZEB-571 parent stays Backlog.
- [ ] Confirm slice-1 DoD met; note the deferred zenoh bridge as a candidate future ZEB-571 child.

---

## Self-review notes (author)

- **Spec coverage:** every DoD item in the spec maps to a task — crates exist (T1–T4), node rewired + iroh 1.0 (T5), core gates (T6), client delegation + tests retained (T7), bridge untouched (T7 step 4). ✓
- **Type consistency:** `IrohEndpoint::new_with_secret(secret, relays, alpn)`, `AlpnConfig{advertised}`, `RelayConfig{urls}`, `AlpnDispatchTable::{new,insert,dispatch_for}`, `TunnelPeer{node_id,pq_identity,home_relay,direct_addrs}`, `CompatSink::record_handshake_outcome` are used consistently across tasks. ✓
- **Known deferrals (flagged in-task, not placeholders):** exact `RelayConfig` fields (T2), exact `ProtocolCompatRegistry`/`DeviceTunnelContact` consumed surface (T4 steps 2–3), and the node iroh 0.91→1.0 delta (T5 step 1) are pin-at-implementation items — each names the source location to confirm against. The implementer reads the cited source; these are not hand-wavy gaps.
- **Granularity note for the executor:** T5 is the one large, atomic task (no compiling intermediate) — it cannot be split without a non-compiling state. T1–T4 are independently buildable/testable. T7 is gated on the human merge of the core PR.
