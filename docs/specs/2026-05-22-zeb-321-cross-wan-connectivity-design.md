# ZEB-321 cross-WAN peer discovery, reconnection, and NAT traversal — cohesive story

**Linear:** [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) (umbrella; related [ZEB-172](https://linear.app/zeblith/issue/ZEB-172), [ZEB-46](https://linear.app/zeblith/issue/ZEB-46), [ZEB-47](https://linear.app/zeblith/issue/ZEB-47), [ZEB-210](https://linear.app/zeblith/issue/ZEB-210), [ZEB-169](https://linear.app/zeblith/issue/ZEB-169))
**Branch (Phase 1):** `zeb-321-phase1-iroh-foundation` off `origin/main` `e68599b` (post-ZEB-319)
**Status:** Approved 2026-05-22 (Jake), brainstormed inline in conversation grounded by Gemini Deep Research
**Phasing:** Phase 1 (this spec, full detail) → Phase 2-5+ (outlined; each phase gets its own brainstorm + spec when reached)

## 1. Goal

Make the PoC deployment scenario — *"two houses in different US states/countries, both behind residential NAT, members of the same harmony community, devices need to discover and connect to each other cross-WAN"* — work end-to-end. Today harmony-client works only on a single LAN/subnet via mDNS + Zenoh peer-mode; cross-WAN is unsolved.

Phase 1 ships the load-bearing foundation: Iroh as the cross-NAT QUIC transport, a custom Zenoh-over-Iroh transport plugin so all existing CRDT sync code keeps working unchanged, and a new `ReachabilityRecord` event in the community state CRDT so within-community devices can auto-discover each other's Iroh NodeIds.

Cross-community first-contact (pkarr + civic registry), liveness/rebinding protocol, mobile push-wake architecture, and community-operated relays are all out-of-scope for Phase 1 — they ship in later phases.

## 2. Background

### 2.1 What harmony-client has today

| Transport | Role today | Coverage |
|---|---|---|
| **Zenoh** (peer-mode) | Load-bearing CRDT sync (community state, channel log, voting log, DFROST log, mail, profile broadcast, owner-state, invites) | 20+ files use it directly; works on LAN, fails cross-NAT |
| **Reticulum** | DM transport (envelope/signing/outbox + inbound packet handling) | ~10 files; mesh-routable but requires fixed-gateway WAN entry |
| **mDNS** | LAN peer discovery (initial scout) | Same-subnet only |
| **Signed peer announces** | Authenticated presence in Zenoh discovery | LAN-scoped (rides Zenoh) |
| **Iroh** | **Not integrated.** Single stale `// No ... iroh tunnels.` comment in `event_loop.rs` | None |

`harmony-core` separately exposes Iroh, but harmony-client has not wired it in.

A free-tier self-hosted Iroh DERP relay runs at `i.q8.fyi` on Google Cloud (free-tier compute). It is unused by harmony-client today.

### 2.2 The cross-WAN gap

Zenoh peer-mode assumes mutual reachability between peers — i.e., one side can dial the other directly. Behind residential NAT (especially the CGNAT / NAT444 topology common on US ISPs in 2026), neither side can reliably dial the other. STUN-based hole-punching is not part of Zenoh's discovery layer.

The result: even when two harmony devices know they're in the same community (via shared community-state CRDT, exchanged out-of-band via invite codes or library directory bootstrap), they cannot actually exchange CRDT updates because no underlying transport session can be established.

### 2.3 What the Deep Research locked in

A Gemini Deep Research project on 2026-current cross-WAN P2P SOTA (commissioned 2026-05-22, results in conversation) produced these decisive findings:

1. **Iroh is the right cross-NAT transport.** Empirical direct-hole-punch success rate ~91% across a 5000-home heterogeneous sample (vs ~71% libp2p DCUtR, ~85% WebRTC). Stateless DERP relays. NodeId-as-identity (Ed25519). QUIC connection migration for IP changes. Rust-native, production-ready.
2. **Hetzner-class commodity VPS is the right relay host.** ~$5/mo CPX11 covers 100k concurrent connections; ~$50/mo for 1M users across 4 regions. GCP at ~$825/mo for the same workload is economically misaligned. (n0's hosted relays suffice through Phase 1-3; self-hosting begins Phase 4.)
3. **Stateless DERP relay = zero DMCA exposure** for community/civic hosts. Library Freedom Project precedent (libraries hosting Tor exits) directly applicable.
4. **iOS mobile = unavoidable alert-push + ciphertext-trust-anchor pattern.** Silent push is throttled to 3-4/hr; only high-priority alert push reliably wakes the app for a 30-second background-fetch window. Sender's payload must land at a trust anchor first; recipient pulls on wake.
5. **pkarr (pubkey → Mainline DHT signed DNS records) is the right *secondary* discovery layer for cross-community first-contact.** ~120ms cached / ~3.8s p99 miss. CPU-bound republish (~600k keys/day per 4-core). Aggressive local caching is load-bearing.

These five findings shape the architecture below; the spec does not re-derive them.

## 3. Architecture overview

### 3.1 The cohesive Phase 1 picture

```
        [Device A — CA, behind NAT]            [Device B — WA, behind NAT]
                │                                       │
                │   1. Each device publishes a signed   │
                │      ReachabilityRecord into the      │
                │      community state CRDT             │
                │      (Iroh NodeId + relay endpoint    │
                │       + liveness timestamp)           │
                │                                       │
                ▼                                       ▼
        ┌────────────────── Community State CRDT ──────────────────┐
        │   ReachabilityAnnounce events (kd="rch")                 │
        │   LWW-keyed by (actor_addr, "rch")                       │
        │   Inner sig binds Iroh NodeId to harmony identity        │
        └──────────────────────────────────────────────────────────┘
                ▲                                       ▲
                │   2. Both devices read each other's   │
                │      ReachabilityRecords from local   │
                │      CRDT replica                     │
                │                                       │
                ▼                                       ▼
        [Iroh Endpoint A]  ◄ ── ── QUIC + DERP ── ── ►  [Iroh Endpoint B]
                │                                       │
                │   3. Iroh races direct + relayed      │
                │      paths; ~91% direct, ~9% via      │
                │      n0's public DERP relays          │
                │                                       │
                ▼                                       ▼
        [Zenoh-over-Iroh transport link]
                │
                │   4. Custom Zenoh `LinkUnicastTrait`
                │      impl backed by an Iroh QUIC
                │      bidi stream keyed by NodeId.
                │      Zenoh wire format runs over it.
                │
                ▼
        [Existing Zenoh CRDT sync code — unchanged]
                │
                │   5. Community state, channel log,
                │      voting log, DFROST log, mail,
                │      profile broadcast, owner-state,
                │      invites — all flow through
                │      Iroh-tunneled Zenoh sessions.
                │
                ▼
        [Per-CRDT state converges across both devices]
```

### 3.2 Three new load-bearing primitives

1. **`ReachabilityRecord` CRDT event** — new signed event type in the community state CRDT (kind `rch`). Each device publishes its own; LWW-keyed by `(device_addr, "rch")`. Stays valid ~24h then must be re-announced. Inner signature binds the device's Iroh NodeId to its harmony identity.

2. **`IrohEndpoint` wrapper** — new harmony-client module wrapping `iroh::Endpoint`. Owns the device's persistent Iroh secret key (stored in OS keychain alongside harmony identity); exposes `connect(node_id)` + `accept()`; routes incoming streams by ALPN.

3. **Zenoh-over-Iroh transport plugin** — custom Zenoh transport implementation. Each Zenoh "link" maps to an Iroh QUIC bidi stream. Zenoh's CRDT-sync code keeps running unchanged; Zenoh's own scouting/discovery is replaced by "look up NodeId from local CRDT" (`ReachabilityResolver`).

### 3.3 What's NOT in Phase 1

| Concern | Lands in |
|---|---|
| Cross-community first-contact (pkarr + civic registry) | Phase 2 |
| Liveness / heartbeat / reconnection protocol | Phase 3 |
| Self-hosted DERP relays (migrate `i.q8.fyi` to Hetzner) | Phase 4 |
| Cross-WAN canary automation | Phase 4 |
| Mobile push-wake / ciphertext trust anchor service | Phase 5+ (separate sub-umbrella) |
| Community-operated relays / federated civic registry | Phase 5+ |
| Multi-device identity integration (`OwnerAddr` ↔ multiple `NodeId`s) | Out-of-umbrella; orthogonal to [ZEB-169](https://linear.app/zeblith/issue/ZEB-169) |
| Reticulum DM migration to Iroh-tunneled | Out-of-umbrella; evaluated between Phase 3 and 4 |

## 4. Phase 1 deliverables

### 4.1 Backend (Rust, lands in `harmony-client/src-tauri/src/`)

1. **`iroh_endpoint.rs` (NEW)** — `IrohEndpoint` struct (see §6).
2. **`reachability_record.rs` (NEW)** — wire-format type + CBOR encoding + signature verification (see §5).
3. **`reachability_publisher.rs` (NEW)** — background task that re-announces this device's record on a schedule + on network-change.
4. **`zenoh_iroh_link.rs` (NEW)** — `LinkUnicastTrait` impl (see §7).
5. **`zenoh_iroh_transport.rs` (NEW)** — `LinkManagerUnicastTrait` impl + `ReachabilityResolver` (see §7).
6. **Extend `community_state_crdt.rs`** — add `MembershipEventKindCode::ReachabilityAnnounce` discriminator, `apply_event` branch for `kd="rch"`, LWW projection rules, expiry semantics, 5 new verify rules (RCH1-RCH5).
7. **Extend `event_loop.rs`** — startup: initialize `IrohEndpoint`, start `reachability_publisher`, register Zenoh-over-Iroh transport with the running Zenoh session. Shutdown: graceful relay disconnect + last-known-state announce.
8. **Extend `lib.rs`** — 3 new IPCs: `connectivity_get_my_reachability_record`, `connectivity_list_peer_reachability`, `connectivity_force_republish`.

### 4.2 Frontend (TypeScript/Svelte, lands in `src/lib/`)

9. **`types/connectivity.ts` (NEW)** — `ReachabilityRecord` payload type matching the Rust wire format.
10. **`connectivity-adapter.ts` (NEW)** — 3 IPC bindings (no event subscribers in Phase 1; events arrive in Phase 3).
11. **`components/DevicePanel.svelte` or `DiagnosticsPanel.svelte` (NEW or extend existing)** — minimal "this device's NodeId + relay + last-published reachability + observed peer reachabilities" surface for debugging. Tucked behind a dev-mode flag; not user-facing in Phase 1.

### 4.3 Tests

12. **Wire-format pinning** — extend existing community-state CRDT CBOR fixture file with a `ReachabilityAnnounce` event.
13. **Unit tests** — record signing/verification round-trip; LWW projection; TTL/expiry behavior; `reachability_publisher` debouncing on rapid network changes; each of RCH1-RCH5 verify rules exercised positively + negatively.
14. **Two-engine integration test** — two harmony-client instances on the same loopback, each publishes a `ReachabilityRecord`, each reads the other's, opens an Iroh connection by NodeId, the connection succeeds, a Zenoh ping over the tunneled link round-trips successfully. End-to-end determinism: same outcome regardless of which engine starts first.

### 4.4 Manual validation (not blocking PR merge)

15. **Cross-WAN smoke test** — tracked in [ZEB-172](https://linear.app/zeblith/issue/ZEB-172) follow-up. Two physical machines, one CA, one WA, both behind residential NAT, same community, full end-to-end CRDT sync round-trip. Pass/fail recorded.

### 4.5 LOC budget

| Surface | Approx LOC |
|---|---|
| Rust (new modules + extensions) | 3500–4500 |
| TypeScript | 250–400 |
| Tests (Rust unit + integration + frontend) | 1200–1800 |
| **Total** | **~5000–6700** |

This is a large Phase 1. The two-stage subagent-driven-development review and the **DONE_WITH_CONCERNS escape hatch** per `feedback_implementer_gate_time_budget` are load-bearing. If implementer subagents hit the 10-min wall-clock kill switch on any single task, we split that task before reattempting.

## 5. Wire format: `ReachabilityAnnounce` event (kd="rch")

### 5.1 Payload type

Following harmony-client's existing CBOR convention (2-char keys, `kd` for kind discriminator, signed-event wrapper):

```rust
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ReachabilityAnnouncePayload {
    /// Iroh NodeId (Ed25519 public key) — distinct from harmony identity key.
    #[serde(rename = "nd")]
    pub iroh_node_id: [u8; 32],

    /// Home DERP relay URL (Phase 1: n0's public relays).
    #[serde(rename = "rl")]
    pub home_relay_url: String,

    /// Direct-traversal hint addresses (publicly routable if any; empty Vec is fine).
    #[serde(rename = "da")]
    pub direct_addresses: Vec<SocketAddr>,

    /// Wall-clock milliseconds when this record was authored.
    #[serde(rename = "ts")]
    pub announced_at_ms: u64,

    /// Inner Ed25519 signature by the device's HARMONY identity key
    /// over canonical CBOR(nd || rl || da || ts || actor_addr || hlc).
    /// Binds the Iroh NodeId to the harmony identity.
    #[serde(rename = "sg")]
    pub identity_signature: [u8; 64],
}
```

### 5.2 Wrapping CRDT event (existing envelope shape)

```cbor
{
    "kd": "rch",
    "ac": <actor owner_addr 16 bytes>,
    "hl": <hlc {phys_ms, lc, node_id}>,
    "pl": <ReachabilityAnnouncePayload as above>,
    "sg": <outer event-level Ed25519 sig over kd || ac || hl || pl>,
}
```

### 5.3 Two-signature rationale

The **outer signature** (`sg` at the event envelope level) is the standard CRDT integrity signature — verifies the event was authored by `ac` and has not been tampered with in transit. Standard pattern across all community-state CRDT events.

The **inner signature** (`pl.sg = identity_signature`) is new and load-bearing: it specifically binds the Iroh NodeId to the harmony identity. Without it, a malicious community member with CRDT write access could publish "actor X's reachability is at NodeId Y where I control Y" and intercept traffic intended for X. The inner sig forces the actual harmony identity key to attest the NodeId binding.

Both signatures use the same Ed25519 key. The inner sig over `nd || rl || da || ts || actor_addr || hlc` is verified during apply (RCH2); the outer sig is verified during the standard event-verify path.

### 5.4 LWW projection rules

- **Key:** `(actor_addr, "rch")` — each device has exactly one active reachability record.
- **Conflict resolution:** higher HLC wins. If HLC equal (shouldn't happen with monotonic clocks), `announced_at_ms` breaks tie; if still equal, lexicographic `iroh_node_id`.
- **Expiry:** a record where `now_ms - announced_at_ms > 24h` is **stale** but still readable. Stale records used as fallback if no fresher record exists; consumers SHOULD attempt Iroh connection using stale record info but log a warning. Phase 3 liveness protocol improves this.

### 5.5 Verify rules (added to `verify_event`)

- **RCH1**: outer signature valid over canonical CBOR of `kd || ac || hl || pl`. Standard for all community-state events.
- **RCH2**: inner `pl.sg` (`identity_signature`) valid over canonical CBOR of `nd || rl || da || ts || actor_addr || hlc` using the key derived from `actor_addr`.
- **RCH3**: `ac` (actor owner_addr) must equal the public-key-derived address of the key that produced the inner signature.
- **RCH4**: `announced_at_ms` within ±30 minutes of `hl.phys_ms` (sanity check; rejects obviously-tampered records). Drop silently if violated; **DO NOT advance last_hlc** (applies [ZEB-320](https://linear.app/zeblith/issue/ZEB-320) discipline).
- **RCH5**: `actor` must be a current community member (read via membership projection at `hl`). Drop silently if not a member.

Per `feedback_two_ipc_toctou` and `feedback_metadata_before_irreversible_write`, all 5 verify rules are read-only and complete BEFORE any state mutation.

### 5.6 Publication triggers (`reachability_publisher` behavior)

- **On startup:** always publish immediately (no debounce).
- **On network change** (`if-watch` event: interface up/down, default route change, IP address change): publish after 2s debounce (collapses rapid flapping).
- **On idle:** re-publish every 60min to keep the record fresh against the 24h TTL.
- **On Iroh home-relay change** (relay node failover detected by Iroh's internal logic): publish immediately (NodeId stable, relay changed — peers need to know).
- **Manual force:** `connectivity_force_republish` IPC triggers an immediate publish (debug surface only; not normally invoked).

### 5.7 Approximate wire size

~280 bytes per record (32 NodeId + ~80 URL + ~30 addr hints + 8 ts + 64 inner sig + envelope overhead). Negligible CRDT footprint even with 24h re-announce cadence.

## 6. `IrohEndpoint` wrapper

### 6.1 Surface

```rust
pub struct IrohEndpoint {
    inner: iroh::Endpoint,
    secret_key: iroh::SecretKey,           // persistent per-device, stored in keychain
    harmony_signer: HarmonyIdentitySigner, // for signing inner ReachabilityRecord sig
    relay_mode: RelayMode,                 // Phase 1: Default (n0's relays)
}

impl IrohEndpoint {
    pub async fn init(
        harmony_signer: HarmonyIdentitySigner,
        secret_key_storage: &SecretKeyStorage,
    ) -> Result<Self>;

    pub fn node_id(&self) -> NodeId;
    pub fn home_relay(&self) -> Option<RelayUrl>;
    pub fn direct_addresses(&self) -> Vec<SocketAddr>;

    /// Open a bidi stream to a peer. Caller is responsible for knowing the NodeId
    /// (typically read from a ReachabilityRecord via local CRDT).
    pub async fn open_bi(&self, peer: NodeId, alpn: &[u8]) -> Result<(SendStream, RecvStream)>;

    /// Accept incoming bidi streams; returns a stream of (NodeId, SendStream, RecvStream).
    pub fn incoming(&self) -> BoxStream<(NodeId, SendStream, RecvStream)>;
}
```

### 6.2 ALPN registry

- `"harmony/zenoh/v1"` — Zenoh-over-Iroh tunneled link (Phase 1 only protocol).
- `"harmony/handshake/v1"` — first-contact handshake (reserved for Phase 2; not implemented in Phase 1).

Incoming connections are dispatched to the appropriate handler by ALPN. Phase 1 ships only the Zenoh handler; the handshake ALPN is reserved so we don't have to re-version when Phase 2 lands.

### 6.3 SecretKey persistence

- **Storage:** OS keychain (macOS Keychain, Windows Credential Manager, Linux Secret Service), under a new keychain entry `harmony.iroh.secret_key`. Same backing store as the existing harmony identity key.
- **Generation:** fresh 32-byte Ed25519 secret on first launch; reused on every subsequent launch.
- **Loss handling:** if the key is lost (keychain corruption, OS reinstall), generate a new one. The device's `OwnerAddr` is unchanged (that's bound to the harmony identity key); the Iroh NodeId changes; peers see it as the device re-announcing with new reachability info — the LWW projection handles it gracefully.

### 6.4 Relay configuration

Phase 1: `RelayMode::Default` — uses n0's public DERP relays via Iroh's built-in resolver. No harmony-team relay configuration in Phase 1.

Phase 4 will introduce per-device + per-community relay configuration overrides. The `IrohEndpoint::init` signature is forward-compatible (adding an optional `relay_config` parameter is a non-breaking change).

## 7. Zenoh-over-Iroh transport plugin

This is the engineering-heaviest piece of Phase 1.

### 7.1 What we're implementing

Zenoh's pluggable transport surface (from `zenoh-link` crate; semi-internal API — see §10 risk #1) requires:

1. **`LinkUnicastTrait`** — per-link send/recv interface.
2. **`LinkManagerUnicastTrait`** — factory for opening/accepting links.

### 7.2 `LinkUnicastTrait` impl (`zenoh_iroh_link.rs`)

```rust
pub struct IrohZenohLink {
    send: Arc<Mutex<SendStream>>,    // Iroh QUIC send half
    recv: Arc<Mutex<RecvStream>>,    // Iroh QUIC recv half
    src: Locator,                    // iroh/<our_node_id>
    dst: Locator,                    // iroh/<peer_node_id>
}

#[async_trait]
impl LinkUnicastTrait for IrohZenohLink {
    async fn write(&self, buffer: &[u8]) -> ZResult<usize> { /* delegates to send.write */ }
    async fn write_all(&self, buffer: &[u8]) -> ZResult<()> { /* delegates to send.write_all */ }
    async fn read(&self, buffer: &mut [u8]) -> ZResult<usize> { /* delegates to recv.read */ }
    async fn read_exact(&self, buffer: &mut [u8]) -> ZResult<()> { /* delegates to recv.read_exact */ }
    async fn close(&self) -> ZResult<()> { /* graceful stream shutdown */ }
    fn get_mtu(&self) -> BatchSize { BatchSize::MAX }  // QUIC streams have no per-frame size limit
    fn get_src(&self) -> &Locator { &self.src }
    fn get_dst(&self) -> &Locator { &self.dst }
    fn is_reliable(&self) -> bool { true }              // QUIC = reliable
    fn is_streamed(&self) -> bool { true }
}
```

### 7.3 `LinkManagerUnicastTrait` impl (`zenoh_iroh_transport.rs`)

```rust
pub struct IrohZenohLinkManager {
    endpoint: Arc<IrohEndpoint>,
    resolver: Arc<ReachabilityResolver>,
    incoming_listener: ListenerHandle,
}

#[async_trait]
impl LinkManagerUnicastTrait for IrohZenohLinkManager {
    async fn new_link(&self, endpoint: &EndPoint) -> ZResult<LinkUnicast>;
    async fn new_listener(&self, endpoint: &EndPoint) -> ZResult<Locator>;
    async fn del_listener(&self, endpoint: &EndPoint) -> ZResult<()>;
    fn get_listeners(&self) -> Vec<EndPoint>;
    fn get_locators(&self) -> Vec<Locator>;
}
```

**`new_link` flow** (outbound connection establishment):

1. Parse locator: `iroh/<node_id_bs58>` or `iroh/<node_id_bs58>?relay=<url>`.
2. Resolve via `ReachabilityResolver::resolve(node_id)` for current home relay + direct address hints. Falls back to Iroh's gossiped discovery if local CRDT record is stale or missing.
3. Call `iroh_endpoint.open_bi(node_id, b"harmony/zenoh/v1")`.
4. Wrap returned `(SendStream, RecvStream)` in `IrohZenohLink`. Return.

**`new_listener` flow** (inbound stream acceptance):

1. Single global listener bound to `IrohEndpoint::incoming()` filtered to ALPN `"harmony/zenoh/v1"`.
2. Each accepted stream is wrapped as `IrohZenohLink` and pushed to Zenoh's link-accept queue via the manager's internal channel.
3. Locator returned: `iroh/<our_node_id_bs58>`.

### 7.4 `ReachabilityResolver` (Zenoh's scout replacement)

Zenoh normally scouts via UDP multicast or queries router peers. With the Iroh transport, we replace scouting with CRDT-driven discovery:

```rust
pub struct ReachabilityResolver {
    crdt_handle: Arc<CommunityStateCrdtRegistry>,
}

impl ReachabilityResolver {
    /// List all reachable peers across all communities this device belongs to.
    pub fn list_active_peers(&self) -> Vec<(OwnerAddr, NodeId, ReachabilityAnnouncePayload)>;

    /// Resolve a known peer's current reachability.
    pub fn resolve(&self, target: NodeId) -> Option<ReachabilityAnnouncePayload>;
}
```

The `event_loop` task that originally drove Zenoh peer-mode discovery now drives: "for each community I'm in, read all peers' ReachabilityRecords, ensure Zenoh has an active link to each peer's NodeId, retry failed connections with exponential backoff."

### 7.5 Connection lifecycle

```
[ReachabilityRecord arrives via CRDT sync]
      │
      ▼
[event_loop checks: active Iroh-Zenoh connection to this NodeId?]
      │
      ├─ Yes → no-op.
      │
      └─ No → enqueue link-establishment task
              │
              ▼
        [IrohZenohLinkManager.new_link(locator)]
              │
              ▼
        [iroh_endpoint.open_bi(node_id, "harmony/zenoh/v1")]
              │
              ▼
        [Zenoh transport layer wraps link, starts session]
              │
              ▼
        [Zenoh CRDT sync proceeds normally over the link]
```

### 7.6 Failure modes & retry

- **Iroh `open_bi` fails** (peer offline, no relay path, NodeId unknown): exponential backoff `1s → 2s → 4s → ... → 5min cap`. Retry only if peer's `ReachabilityRecord` is updated (HLC advance), or after the 5-min cap expires.
- **Established link drops mid-session**: Zenoh's session layer handles re-link by calling `new_link` again. QUIC connection migration handles transient IP changes within a single session without dropping the link in the first place.
- **Stale ReachabilityRecord (>24h old)**: attempt connection anyway, log warning. Phase 3's liveness protocol improves stale-record handling.

## 8. Phase 2-5+ outline

These are NOT specced in detail here. Each phase gets its own brainstorm + spec when reached. Outline only, so we know where Phase 1 is going.

### 8.1 Phase 2: Cross-community first-contact (Size: M)

Ships: ability for a device to discover the Iroh NodeId of a peer when NOT in a shared community yet — e.g., joining a community via an invite that references the admin's harmony pubkey.

Components:
- `pkarr_publisher.rs` — publishes signed Mainline-DHT record keyed by harmony identity pubkey. Body: current Iroh NodeId + home relay URL. TTL ~6h. Republish on network change + scheduled. CPU-bounded per DR (~600k keys/24h per 4-core); mitigated by long TTL + cache-then-refresh + republish-on-change-only.
- `pkarr_resolver.rs` — lookup-by-harmony-pubkey returning ReachabilityRecord-equivalent. Aggressive local cache (in-memory LRU + on-disk SQLite); only hits DHT on miss/expiry.
- First-contact handshake ALPN `"harmony/handshake/v1"`. Used when joining a community via invite: contact inviter, exchange membership material, transition to regular community-CRDT discovery.
- Civic-registry stub — data model + placeholder static-file directory at a known URL. Real federated registry comes in Phase 5+.

Key open question for Phase 2 brainstorm: how to minimize pkarr metadata leakage (queried pubkey is visible to routing nodes per DR). [ZEB-47](https://linear.app/zeblith/issue/ZEB-47) ZipPIR may eventually answer this; not solved in Phase 2.

### 8.2 Phase 3: Liveness + rebinding + reconnection (Size: M)

Ships: architecture that makes "laptop moves networks" and "phone wakes after 8 hours offline" graceful instead of producing long timeouts and stale state.

Components:
- Heartbeat protocol — low-bandwidth "I'm here" message every 5 minutes (idle) or 30s (active) over each established Iroh-Zenoh session.
- Reconnection orchestrator — on network change, wake-from-suspend, or heartbeat timeout: re-publish own ReachabilityRecord (debounced 2s), re-publish own pkarr record (debounced 30s), send rebind messages over existing sessions (QUIC connection migration handles this for active streams), enqueue new-link tasks for known-but-not-connected peers.
- Stale-record reconciliation — when reading a peer's record >24h old, attempt connection AND in-parallel issue freshness probe via pkarr.
- 3 new Tauri events for the UI: `peer-connected`, `peer-disconnected`, `peer-unreachable-after-N-attempts`.

Key open question for Phase 3 brainstorm: adaptive heartbeat cadence (battery vs responsiveness tradeoff, especially for mobile). Dedicated phase-3 brainstorm before implementation.

### 8.3 Phase 4: Cross-WAN empirical validation + self-hosted relays (Size: M)

Ships: confidence the architecture works in the PoC scenario, plus the operational pattern for self-hosted/community-operated relays.

Components:
- Hetzner-based relay deployment — provision 2-region (US East + EU Central) CPX11 boxes running Iroh DERP. Migrate `i.q8.fyi` workload. Rename for clarity (e.g., `relay-us.q8.fyi`). Configure harmony-client to prefer harmony-hosted relays when available.
- Relay configuration surface — IPC + UI for "configure which DERP relays this device uses." Default: use n0 + harmony defaults. Power-user: override per-community or globally.
- Cross-WAN canary — automated 2-machine smoke test running daily. Reports success rates of direct hole-punch, relay fallback, and Zenoh-over-Iroh CRDT round-trip. Extends [ZEB-172](https://linear.app/zeblith/issue/ZEB-172).
- Documented residential-NAT validation — manual test on at least one CGNAT-bound residential ISP + one Comcast-grade NAT. Record success rates against DR's 91% baseline.

Key open question for Phase 4 brainstorm: how much observability to invest in (Grafana/Prometheus vs daily-Linear-canary). Likely the latter until we have actual users.

### 8.4 Phase 5+: Relay governance + civic-infrastructure + community-operated relays (Size: L)

Ships: the polycentric story — communities can run their own relays, civic institutions can host trust-anchor directories, no required Harmony-team infrastructure for normal operation.

Components:
- Standalone Iroh DERP relay binary — single statically-linked Rust binary + ACME-based TLS + bandwidth caps + zero-log-retention. Distribution: docker image + systemd template + brew formula.
- Per-community relay configuration — communities specify "use these relays" in their state CRDT. Members pick up configuration and connect to those relays preferentially.
- Federated civic trust registry — wire format for "trust registry": signed list of vouched community/relay endpoints by civic institution. Phase 5+ ships data model + placeholder Internet-Archive-hosted directory (subject to actual partnership). Multiple registries federate; users pick which to trust.
- Privacy-hardening pass — relay log scrubbing audit, no peer-IP retention, optional Tor pluggable transports.

Key open question for Phase 5+ brainstorm: who curates the civic registry (Library Freedom Project pattern? Multi-stakeholder governance? Permissionless registration with reputation?). Major design effort in its own right; probably its own umbrella ticket when reached.

## 9. Out of scope (this umbrella)

- **Multi-device identity** ([ZEB-169](https://linear.app/zeblith/issue/ZEB-169) Track A) — owner→device binding. Orthogonal; integration-as-small-follow-up once it ships.
- **Reticulum global routing scaling** ([ZEB-210](https://linear.app/zeblith/issue/ZEB-210)) — separate research track.
- **ZipPIR private discovery** ([ZEB-47](https://linear.app/zeblith/issue/ZEB-47)) — privacy hardening for discovery, longer-horizon research.
- **EigenTrust dynamic rendezvous** ([ZEB-46](https://linear.app/zeblith/issue/ZEB-46)) — separate research track.
- **Reticulum DM migration** — evaluation between Phase 3 and Phase 4 (separate ticket if pursued).
- **Mobile push-wake signaling service** — separate sub-umbrella once we want to ship iOS/Android client. Designed-around in Phase 1 (storage decoupling); built when needed.

## 10. Dependencies

Current `src-tauri/Cargo.toml` declares `zenoh = "1"` and no Iroh/quinn/if-watch. Phase 1 adds:

- **`iroh` crate** — latest production version compatible with `zenoh = "1"`. n0's hosted DERP relays via `RelayMode::Default`. NEW dep; load-bearing.
- **`zenoh-link` semi-internal API** — re-exported from the `zenoh = "1"` major-version line we're already pinned to. Upgrade friction accepted (see risk #1).
- **`quinn` crate** — comes transitively via `iroh`. No direct usage in Phase 1.
- **`if-watch` crate** — for network-change detection. NEW dep; small + mature.
- **OS keychain access** — same `keyring` / platform-specific backend as existing harmony identity key persistence; no new crate needed.
- **n0's hosted DERP relays** — Phase 1 only. Phase 4 introduces our own with n0 as fallback.

**Cross-repo concern:** Most Phase 1 work lands in `harmony-client`. If the `IrohEndpoint` wrapper grows enough to warrant a shared abstraction with harmony-core, promote it in Phase 2.

## 11. Risks & mitigations

| # | Risk | Severity | Mitigation |
|---|---|---|---|
| 1 | Zenoh `LinkUnicastTrait` API changes between minor versions | Medium | Pin Zenoh version; document upgrade path; accept periodic friction |
| 2 | Backpressure mismatch (Zenoh batching vs QUIC stream flow control) | Medium | Validate in two-engine integration test; tune if needed |
| 3 | MTU/batching tuning | Low | Empirical tuning; set generous batch size; QUIC handles fragmentation |
| 4 | ALPN coexistence (Phase 2 adds handshake ALPN) | Low | Phase 1 ships only Zenoh ALPN; Phase 2 dispatcher is forward-compatible |
| 5 | Iroh secret-key loss (keychain corruption) | Medium | Document recovery; lost key = new NodeId; LWW projection handles re-announce |
| 6 | pkarr metadata leakage (Phase 2 concern only) | Medium | Phase 2 brainstorm dedicated to this; [ZEB-47](https://linear.app/zeblith/issue/ZEB-47) future track |
| 7 | Heartbeat cadence vs battery (Phase 3 concern only) | Medium | Dedicated Phase 3 brainstorm |
| 8 | iOS background gates breaking always-on assumptions | High | Phase 1 mobile-aware design (storage decoupling); Phase 5+ builds the path |
| 9 | n0 DERP relay outage during Phase 1-3 | Medium | Phase 4 introduces our own with n0 as fallback |
| 10 | Zenoh-over-Iroh complexity bloating Phase 1 timeline | Medium | DONE_WITH_CONCERNS escape hatch per `feedback_implementer_gate_time_budget`; sub-phase split (1a transport plugin + 1b reachability record + 1c integration) if any single task hits 10-min wall-clock |

## 12. Success criteria (umbrella-level, when ZEB-321 closes)

1. Two devices in different US states (or equivalent NAT-bound contexts), both running harmony-client, in the same community, reliably establish a cross-WAN connection and exchange CRDT state — direct hole-punch where topology permits, relay-fallback elsewhere.
2. A device offline for 1-7 days reconciles with its known peer set within ~30 seconds of network availability.
3. A device that changes networks mid-session does NOT lose its established Zenoh sessions (QUIC connection migration handles this).
4. Harmony-team-hosted relay infrastructure is replaceable by community-operated relays without protocol changes — communities have full sovereignty over relay choice.
5. The architecture remains polycentric: no single point of failure, no required harmony-team infrastructure for normal operation.

## 13. Phase 1 success criteria (PR-merge gate)

The Phase 1 PR is mergeable when:

1. All five harmony-client CI gates green (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo nextest run --workspace --all-targets --features test-fixtures` for both stable + MSRV).
2. Both frontend gates green (`npx tsc --noEmit`, `npx vitest run`).
3. Wire-format pinning fixture committed and exercises the `ReachabilityAnnounce` event.
4. Two-engine integration test passes deterministically: both engines establish a Zenoh-over-Iroh session and round-trip a Zenoh ping.
5. No regression on the 8 existing community-state CRDT event kinds.
6. Bot reviews (CodeRabbit, Cursor Bugbot, CodeAnt, Qodo) converged per `feedback_autonomous_pr_monitoring_loop`.

Manual cross-WAN smoke test ([ZEB-172](https://linear.app/zeblith/issue/ZEB-172)) is NOT a blocking gate for Phase 1 merge — it's the validation that confirms Phase 1 actually delivered. If it fails post-merge, a Phase 1.5 or Phase 1 follow-up ticket is filed.

## 14. References

- Gemini Deep Research project on 2026-current cross-WAN P2P SOTA — full results in conversation (2026-05-22); not duplicated here. Key findings summarized in §2.3.
- [Iroh](https://www.iroh.computer/) — primary cross-NAT transport.
- [Zenoh](https://zenoh.io/) — existing CRDT sync transport.
- [ZEB-172](https://linear.app/zeblith/issue/ZEB-172) — Track D: connectivity diagnostics (related, complementary).
- [ZEB-46](https://linear.app/zeblith/issue/ZEB-46) — EigenTrust dynamic rendezvous (research, separate).
- [ZEB-47](https://linear.app/zeblith/issue/ZEB-47) — ZipPIR private discovery (research, longer-horizon).
- [ZEB-169](https://linear.app/zeblith/issue/ZEB-169) — multi-device identity (orthogonal, future integration).
- [ZEB-210](https://linear.app/zeblith/issue/ZEB-210) — Reticulum global routing (research, separate).
- [ZEB-320](https://linear.app/zeblith/issue/ZEB-320) — `last_hlc` drop-path discipline (applied in RCH4 silent-drop rule).
- `i.q8.fyi` — free-tier Iroh relay on GCP (currently deployed; migrates to Hetzner in Phase 4).
