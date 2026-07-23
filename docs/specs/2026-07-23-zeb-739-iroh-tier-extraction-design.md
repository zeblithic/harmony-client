# ZEB-739 — iroh library tier extraction (slice 1) design

**Ticket:** [ZEB-739](https://linear.app/zeblith/issue/ZEB-739) (child of [ZEB-571](https://linear.app/zeblith/issue/ZEB-571) item 4)
**Status:** design — awaiting review
**Repos:** `zeblithic/harmony` (core) + `zeblithic/harmony-client`
**Author:** Koya, 2026-07-23

## Goal

Extract the harmony-client's reusable iroh transport substrate into two new core library crates — `harmony-iroh` (endpoint + ALPN dispatch) and `harmony-tunnel-iroh` (the per-connection PQ-tunnel driver) — reconciling the cross-repo iroh major-version split (core 0.91 → 1.0) and collapsing the harmony-node ↔ client `tunnel_task` fork in the same move. The zenoh-over-iroh bridge is explicitly **out of scope** (deferred to a later ZEB-571 child).

## Architecture

Extract-and-replace, core-first. The core PR bumps the harmony workspace to iroh 1.0, adds the two crates (seeded from the client's already-1.0 code), and rewires the `harmony-node` binary onto them — deleting node's inline 0.91 transport rather than porting it. A follow-up client PR bumps the lockstep harmony pins and rewrites the client's `iroh_endpoint`/`tunnel_manager`/`tunnel_task` modules into thin delegators. Behavior is pinned by the existing client transport test suite, which must pass unchanged against the delegated types.

## Tech stack

Rust, iroh 1.0, tokio, `harmony-tunnel` (no_std sans-I/O PQ session — unchanged), the harmony 44-crate workspace conventions. New crates are std, I/O-bearing, declared in `[workspace.dependencies]` **without** `default-features = false` (the `harmony-rawlink` form).

---

## Global constraints

- **iroh 1.0 everywhere in core after this lands.** The workspace `iroh` dep moves `0.91 → 1.0`; harmony-node is the only current consumer and must compile + pass its tests against 1.0 by adopting the new crates. No crate may remain on 0.91.
- **Byte/wire preservation is load-bearing.** Divergent framing or addressing bytes = silent cross-peer tunnel failure. The PQ tunnel wire format (owned by `harmony-tunnel`, unchanged), the 4-byte big-endian length framing, the `iroh/<hex EndpointId>` locator convention, and the `blake3(ML-DSA-65 pubkey) → NodeId` derivation must all be preserved exactly.
- **No new dependencies beyond iroh/tokio.** Both crates compose types already in-tree (`harmony-tunnel`, `harmony-identity`, iroh, tokio, tokio-util, blake3, futures-util).
- **Zenoh bridge untouched.** `IrohZenohLinkManager`, `spawn_accept_loop`, `iroh_zenoh_registration.rs`, `zenoh_iroh_link.rs`, and the vendored `zenoh-link` fork stay client-side and must remain functional. The client's zenoh bridge will consume the *new* `harmony-iroh` `IrohEndpoint` and dispatcher trait (it holds an `Arc<IrohEndpoint>` and installs a handshake dispatcher today), so the extracted types must preserve exactly the surface the bridge relies on.
- **Existing transport tests are the preservation anchor** — retained and green on both sides.

---

## The version-skew linchpin

Verified directly against both manifests (2026-07-23):

| | iroh | zenoh |
|---|---|---|
| client (`src-tauri/Cargo.toml`) | `1.0` (L203) | `=1.9.0` + vendored `zenoh-link` patch (L45, L272) |
| core (`harmony/Cargo.toml`) | `0.91` (L141) | `1` (L139) |

iroh 0.91 and 1.0 are different majors; their `Endpoint`/`Connection`/`SecretKey` types do not unify. Therefore **no core iroh crate is consumable by the client until core is at iroh 1.0.** This is why the core PR must bump the workspace and rewire harmony-node in one atomic step (§ "Why the core PR is atomic").

---

## Why the core PR is atomic

`harmony-node` is a single crate pinned to a single `iroh` version via `iroh = { workspace = true }`. Once the workspace `iroh` dep flips to 1.0, *all* of harmony-node's iroh code must compile against 1.0 simultaneously — there is no half-bumped intermediate state (endpoint on 1.0, `tunnel_task` on 0.91 does not type-check). Because we chose extract-and-replace (not hand-port), the only way node compiles at 1.0 is to consume the new 1.0 crates. Hence: **workspace iroh bump + both new crates + node rewire + deletion of node's forked transport all land in one core PR.** This is large but mechanical (move + rewire + delete). harmony-node's transport is a frozen, never-two-node-proven mirage whose DM path is already dropped (client-side, ZEB-473), so the rewire may favor thin adoption and dead-path elision over faithful reproduction wherever a path is provably unused — reducing the true rewire surface.

---

## Crate 1: `harmony-iroh`

**Responsibility:** own an iroh `Endpoint` with relay/rebind lifecycle, and provide the ALPN-keyed inbound-connection dispatch seam. App-agnostic: no identity/keychain, no ALPN wire strings, no concrete acceptors.

### Modules

- `endpoint` — the `IrohEndpoint` wrapper (seeded from `iroh_endpoint.rs`, minus the `alpn` module and `load_or_create_secret_key`).
- `dispatch` — the `IrohHandshakeDispatcher` trait + a generic ALPN→dispatcher multiplexer.
- `error` — `IrohEndpointError` (ported).

### Seam 1a — `SecretKey` is injected, key-provisioning stays app-side

Today `IrohEndpoint::load_or_create_secret_key` (`iroh_endpoint.rs:461-510`) binds `crate::identity::{app_key_or_create_with_fallback, VaultSlot::Iroh, EncryptedFileStore}` + keychain coordinates. That is app identity plumbing, not transport. The extracted constructor takes an already-materialized key:

```rust
// harmony-iroh
impl IrohEndpoint {
    pub async fn new_with_secret(
        secret: iroh::SecretKey,
        relays: RelayConfig,       // configured relay URLs (ZEB-624 relay map)
    ) -> Result<Self, IrohEndpointError>;
}
```

The client keeps `load_or_create_secret_key` (renamed to a provisioning free-fn in `iroh_endpoint` or `identity`) and passes the resulting `SecretKey` in. harmony-node likewise supplies its own ephemeral/random key (it deliberately uses a fresh key unlinked from the node identity — `event_loop.rs:525`).

### Seam 1b — ALPN config is caller-supplied

Today the `alpn` module (`iroh_endpoint.rs:51-103`) hardcodes ten `HARMONY_*` byte strings, two re-exported from `crate::community_relay`. These are app protocol identifiers, not substrate. The endpoint constructor takes the ALPN set to advertise:

```rust
pub struct AlpnConfig {
    pub advertised: Vec<Vec<u8>>,   // ALPNs the endpoint accepts
}
```

The `HARMONY_*` constants stay app-side (client `iroh_endpoint::alpn`, and a small node-side const set). The endpoint is constructed with whichever ALPNs the app registers.

### Seam 1c — the dispatch trait + generic multiplexer

The clean seam already exists as `IrohHandshakeDispatcher` (`iroh_invite_acceptor.rs:167-173`, only `iroh::endpoint::Connection`). Lift it verbatim:

```rust
// harmony-iroh::dispatch
#[async_trait]
pub trait IrohHandshakeDispatcher: Send + Sync {
    async fn handle_connection(&self, conn: iroh::endpoint::Connection);
}
```

Today's `MultiplexHandshakeDispatcher` (`iroh_friend_acceptor.rs:2489-2540`) is a fixed 3-way (invite/friend/pex) fan-out with a pure `route_handshake_alpn` helper. Generalize to a table so the fan-out set is caller-defined:

```rust
pub struct AlpnDispatchTable {
    routes: HashMap<Vec<u8>, Arc<dyn IrohHandshakeDispatcher>>,
}
impl AlpnDispatchTable {
    pub fn insert(&mut self, alpn: Vec<u8>, d: Arc<dyn IrohHandshakeDispatcher>);
    pub fn dispatch_for(&self, alpn: &[u8]) -> Option<Arc<dyn IrohHandshakeDispatcher>>;
}
```

The concrete acceptors (invite/friend/pex/butler/community-relay), which are `NodeState`-saturated, **stay app-side** and are inserted into the table by the app. `route_handshake_alpn`'s pure routing test moves with the table.

> **Scope boundary vs. the zenoh bridge.** The client's *actual* accept loop (`spawn_accept_loop` on `IrohZenohLinkManager`) that consults this dispatch table stays client-side in slice 1 (it is part of the deferred bridge). `harmony-iroh` provides the trait + table + a *generic* accept helper usable by node and by the future bridge extraction; the client's bridge continues to own its own loop and simply installs the dispatch table into it. harmony-node uses `harmony-iroh`'s generic accept helper directly (it has no zenoh bridge).

### Consumers after extraction

- **client:** `iroh_endpoint.rs` becomes a thin re-export/delegation; the `alpn` const module + key provisioning stay; the zenoh bridge holds `Arc<harmony_iroh::IrohEndpoint>` unchanged.
- **harmony-node:** replaces its three inline `Endpoint` builders + single-ALPN accept arm with `harmony_iroh::IrohEndpoint` + a one-entry dispatch table (or direct accept for its single tunnel ALPN).

---

## Crate 2: `harmony-tunnel-iroh`

**Responsibility:** the per-peer tunnel manager + per-connection async driver that pumps iroh QUIC bi-streams into the shared no_std `harmony-tunnel` `TunnelSession`. Depends on `harmony-iroh` + `harmony-tunnel` + `harmony-identity`.

### Modules

- `manager` — `TunnelManager` (from `tunnel_manager.rs`, 1305 LOC): per-peer `HashMap<[u8;32], TunnelHandle>`, lazy dial + inbound reuse, DM buffering, deterministic simultaneous-dial collision (lower-NodeId initiator wins), ZEB-485 `AwaitingInbound` state.
- `driver` — `tunnel_task` (from `tunnel_task.rs`, 1856 LOC): `run_tunnel_initiator` / `run_tunnel_responder`, 4-byte-BE length framing (`tokio_util::LengthDelimitedCodec`), `node_id_from_dsa_pubkey = blake3(ML-DSA-65 pubkey)`.

`manager` and `driver` move together (mutual dependency: the manager calls `run_tunnel_initiator`; the driver calls back `register_inbound`/`note_*`/`compat_registry`).

### Seam 2a — `DeviceTunnelContact` parameterized

Both units reference `crate::owner_state_types::DeviceTunnelContact` (a plain data struct: peer node-id, home relay, direct addrs, PQ identity). It is the only owner-state tendril. Replace with a slim, app-agnostic dial-target struct owned by the crate (or a `TunnelPeer` trait the app's `DeviceTunnelContact` implements). The driver's only consumers are `dial_addr` / `peer_pq_identity`, so the required surface is: node-id, dial addresses, home relay, and the peer's `PqIdentity`.

```rust
// harmony-tunnel-iroh
pub struct TunnelPeer {
    pub node_id: [u8; 32],
    pub pq_identity: harmony_identity::PqIdentity,
    pub home_relay: Option<iroh::RelayUrl>,
    pub direct_addrs: Vec<std::net::SocketAddr>,
}
```

The client maps its `DeviceTunnelContact` → `TunnelPeer` at the call boundary (a `From` impl); harmony-node builds `TunnelPeer` directly.

### Seam 2b — `ProtocolCompatRegistry` behind a `CompatSink` trait

`TunnelManager` holds `Arc<crate::protocol_versioning::ProtocolCompatRegistry>` and the driver reports `HandshakeFailure`/version info into it (ZEB-623). It is shared by `Arc` with network-health, so a narrow sink trait suffices:

```rust
pub trait CompatSink: Send + Sync {
    fn record_handshake_outcome(&self, peer: [u8; 32], outcome: HandshakeOutcome);
    // ...exact surface confirmed against ProtocolCompatRegistry's consumed methods at impl time
}
```

The client's `ProtocolCompatRegistry` implements `CompatSink`; harmony-node supplies a no-op impl (it has no compat UI).

### Seam 2c — inbound ingest channel

`TunnelManager::new` takes an `ingest_tx` channel for inbound DMs. This is already a plain `flume`/`mpsc` sender of a crate-defined `InboundDm` type — moves with the crate unchanged; the app owns the receiver.

### Consumers after extraction

- **client:** `tunnel_manager.rs` + `tunnel_task.rs` become thin re-exports; `iroh_tunnel_acceptor.rs`, `iroh_tunnel_dm_transport.rs`, `dm_inbox_ingest.rs`, `owner_commands.rs`, `network_health.rs` continue to reach `TunnelManager` through `crate::tunnel_manager::…` (zero consumer edits, items-2/3/5 blast-radius pattern).
- **harmony-node:** deletes its `crates/harmony-node/src/tunnel_task.rs`; the event-loop tunnel arms call `harmony_tunnel_iroh::{run_tunnel_responder, run_tunnel_initiator}` / hold a `TunnelManager`.

---

## Behavior / wire preservation

1. **PQ tunnel bytes:** unchanged — owned by `harmony-tunnel`. `harmony-tunnel-iroh` only re-hosts the iroh-stream glue; the handshake `TunnelAction`/`TunnelEvent` sequencing and AEAD framing are byte-identical because the state machine is the same crate.
2. **Length framing:** 4-byte big-endian prefix preserved (both the hand-rolled `write/read_length_prefixed` and the `LengthDelimitedCodec` config).
3. **Addressing:** `iroh/<hex EndpointId>` locator + `blake3(ML-DSA-65 pubkey) → node_id` preserved exactly.
4. **Tests:** existing client transport tests (endpoint lifecycle, tunnel round-trip, simultaneous-dial collision resolution, framing round-trip) retained and green against the delegated types; harmony-node's own tests green post-rewire.

---

## Cross-repo mechanics

1. **Core PR** (`zeblithic/harmony`): workspace `iroh` 0.91→1.0; add `harmony-iroh` + `harmony-tunnel-iroh` (members + `[workspace.dependencies]` path entries + `[workspace.package]` inheritance, `harmony-rawlink` form); rewire harmony-node onto both; delete node's forked `tunnel_task.rs` + inline endpoint/accept code. Verify: `cargo build`, `cargo fmt --all --check`, `cargo clippy --all-targets -D warnings`, `cargo nextest run` for the affected crates. Merge → rev **R**.
2. **Client PR** (`zeblithic/harmony-client`): bump the 10 lockstep harmony pins → **R** (extend the pin-genealogy comment; `harmony-pkarr` stays on its own pin); delegate `iroh_endpoint`/`tunnel_manager`/`tunnel_task` to the new crates; keep the `alpn` const module + key provisioning + zenoh bridge client-side; retain all transport tests.

CodeRabbit is throttled (~1/hr; split-repo doubles spend) — sequence the two PRs, core-first, and expect to lean on Qodo (client, auto) / Qodo+CodeAnt (core) for the second PR if CodeRabbit is unavailable.

---

## Risks & open implementation questions

1. **iroh 0.91 → 1.0 API delta in harmony-node.** The rewire replaces node's inline code with the extracted crates, so most of the delta is absorbed by re-seeding from the client's 1.0 code. Residual risk: node-specific iroh calls (relay map construction `event_loop.rs:509-522`, ephemeral per-dial endpoints `:1721-1753`) that have no client analog and must be updated to 1.0 by hand or elided if dead. **Action:** during planning, diff the 0.91 vs 1.0 iroh API for the exact calls harmony-node makes and confirm each has a 1.0 form or is provably unused.
2. **Exact `ProtocolCompatRegistry` / `DeviceTunnelContact` surface.** The seam traits above are sketches; the plan must pin the precise consumed method/field set from source before writing the trait.
3. **`IrohEndpoint` inner-escape-hatch.** `pub(crate) fn inner()` hands the raw `Endpoint` to the link manager + tunnel driver. Post-extraction this must be `pub` (cross-crate) with a clear contract, or the consumers restructured to not need the raw endpoint. Confirm the minimal public escape surface.
4. **Core PR size.** Large but mechanical. Mitigate with tidy commit hygiene (one commit per crate-add, one for node rewire, one for the workspace bump) so review reads as move + rewire, not new logic.

---

## Definition of done

1. `harmony-iroh` + `harmony-tunnel-iroh` exist in core at iroh 1.0 with rustdoc; harmony-node rewired onto them; node's forked transport deleted; core workspace fully on iroh 1.0; core gates green.
2. Client delegates `iroh_endpoint`/`tunnel_manager`/`tunnel_task` to the new crates; existing transport tests retained + green; client gates green.
3. CI + bots converged on both repos.
4. Zenoh bridge untouched and still functional client-side.

## Out of scope (future ZEB-571 child)

The zenoh-over-iroh bridge — `IrohZenohLinkManager`, `spawn_accept_loop`, `iroh_zenoh_registration.rs`, `zenoh_iroh_link.rs`, and the vendored `zenoh-link` fork. Rationale: `[patch.crates-io]` is workspace-root-global and cannot be encapsulated in a library, so extraction gives consumers no isolation; and it is the most app-woven, highest-churn transport code. Revisit as a coherent unit if/when the isolation math changes.
