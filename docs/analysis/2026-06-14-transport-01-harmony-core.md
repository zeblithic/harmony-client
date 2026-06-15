# Harmony Core — Networking / Transport Stack Forensic Analysis

**Date:** 2026-06-14
**Scope:** `harmony` core repo (`/Users/zeblith/work/zeblithic/harmony`), `crates/` tree + `crates/harmony-node/`. `.worktrees/` excluded (unrelated ML branches).
**Method:** Read-only. No builds run. Verdicts backed by `file:line` citations, presence/absence of tests, and stub markers. "Has a passing unit test" is distinguished from "proven to work between two real nodes."

---

## TL;DR

Core's networking is **a set of largely-real, well-tested *components* that are NOT assembled into a proven end-to-end whole.** The two transports that matter both exist and both have a real wiring path in the live binary (`harmony-node`):

1. **Real upstream Zenoh** (`zenoh = "1"`) is opened and used directly by `harmony-node/event_loop.rs` for pub/sub, queryables, and CID content fetch. This is the working, exercised data plane.
2. **A real iroh-QUIC tunnel** (`harmony-tunnel` + `harmony-node/tunnel_task.rs`) with a complete PQ handshake, AEAD framing, keepalive, and a real dial→accept→route path in `event_loop.rs`.

But: **there is not a single integration test anywhere in core that brings up two real nodes (or even two real iroh endpoints / two real Zenoh sessions) and passes a message between them.** Every "two-party" test is a sans-I/O state-machine simulation with the transport hand-bridged in memory. Multiple action handlers in the node event loop that the architecture depends on are explicit `tracing::debug!` stubs. The Reticulum router runs in **leaf mode only** at runtime, so its (tested) multi-hop relay code is inert in production. "Zenoh-over-iroh" — the stated core value prop — is **not realized**: Zenoh runs over its own stock transport, and the tunnel's Zenoh-frame path is a TODO.

Bottom line for Jake's hypothesis: **"functional pieces, no full integration" is accurate.** Core is not proven non-functional, but it is **unproven end-to-end** and has real stubs on load-bearing paths (replication routing, path requests, tunnel close, Zenoh-over-tunnel). The client reinventing the stack on top of real Zenoh is consistent with core's tunnel/Reticulum layer never having been driven to a two-node proof.

---

## Component Inventory

| Component | Files | Purpose | Verdict | Evidence |
|---|---|---|---|---|
| **harmony-tunnel** (protocol) | `crates/harmony-tunnel/src/{handshake,session,frame,replication,event,error}.rs` | Sans-I/O iroh-QUIC tunnel: PQ handshake (ML-KEM-768 + ML-DSA-65 + HKDF), AEAD frames (ChaCha20-Poly1305), keepalive, multiplexed Reticulum/Zenoh/Replication frame tags | **WORKING (sans-I/O)** | Real crypto calls (`handshake.rs:45,124,306`); 26 unit tests incl. `paired_machines_exchange_data` (session.rs:452) doing full handshake + bidirectional encrypted frame exchange; MITM-rejection test `wrong_responder_identity_rejected`. Zero `todo!`/stub. But all tests run two in-memory `TunnelSession`s — no real iroh. |
| **harmony-node tunnel task** | `crates/harmony-node/src/tunnel_task.rs` (663) | Per-connection async driver: opens iroh bi-streams, length-prefixed framing, drives `TunnelSession`, keepalive timer, forwards decrypted msgs to bridge | **WORKING (untested at I/O layer)** | `run_initiator`/`run_responder` open real `conn.open_bi()`/`accept_bi()` (tunnel_task.rs:118,242), real `FramedRead`/`LengthDelimitedCodec` loop (300-435). Only tests are size-constant unit tests (615-662). `run_initiator`/`run_responder` never called from any test. |
| **harmony-node tunnel bridge** | `crates/harmony-node/src/tunnel_bridge.rs` (96) | mpsc plumbing types (`TunnelBridgeEvent`, `TunnelCommand`, `TunnelSender`, `ReadyConnection`) | **WORKING** | Plain types; `ReadyConnection` carries real `iroh::Endpoint` + `Connection`. |
| **harmony-node event loop** | `crates/harmony-node/src/event_loop.rs` (3038) | The live serve path. Binds iroh endpoint, opens real Zenoh session, accepts/dials tunnels, dispatches `RuntimeAction`s, feeds inbound back to runtime | **PARTIAL** | Real iroh bind (561), real dial w/ ephemeral endpoint + relay map (1810-1855), real accept arm (1232), inbound `ReticulumReceived → RuntimeEvent::TunnelReticulumReceived` (1132), real `zenoh::open` (398). BUT `SendPathRequest`/`CloseTunnel`/`ReplicaPush`/`ReplicaPullResponse`/`QueryMemo`/`SendReply` are `tracing::debug!` stubs (2109-2169); `ZenohReceived` over tunnel is a TODO (1140). Never invoked by any test. |
| **harmony-runtime** (router/dispatch) | `crates/harmony-runtime/src/runtime.rs` (9041) | `NodeRuntime::tick()` event→action translation; owns the Reticulum `Node`, `PeerManager`, Zenoh routers; emits `RuntimeAction`s | **PARTIAL** | `tick()` (runtime.rs:2234) real 3-tier dispatch. `InitiateTunnel`/`SendOnInterface`/`UnicastReceived` emitted by real logic. Reticulum router constructed **leaf-mode** `Node::new()` (954) — transport relay inert. `PeerAction::InitiateLink/CloseLink` = no-op stub (4315). Zenoh liveliness not wired (1997). |
| **harmony-reticulum** (RNS protocol) | `crates/harmony-reticulum/src/{node,path_table,packet,announce,link,ifac,loopback,...}.rs` (14k) | Full Reticulum impl: announce parse/validate, path table, packet routing, links, IFAC | **WORKING (leaf) / DEAD (transit) / wire-proven** | `route_packet` real (node.rs:632); ~100 unit tests via `LoopbackInterface` (sans-I/O); `tests/reticulum_interop.rs` = 16 byte-exact vectors vs Python RNS (hashes, sig, link-ID, IFAC). `is_transport()` paths (node.rs:680,1233,1297) never active because runtime uses `Node::new()` not `new_transport()`. No two-node live test. |
| **harmony-peers** | `crates/harmony-peers/src/{manager,event,state,lib}.rs` (1375) | PeerManager state machine: status transitions, backoff+jitter, emits `PeerAction` | **WORKING (bookkeeping only)** | 18 unit tests (manager.rs:392-906). Pure state machine — does no discovery/I/O itself; consumes `AnnounceReceived`, emits `InitiateTunnel`/`SendPathRequest`. Relayed-flag inferred from config not live path (TODO manager.rs:116). |
| **harmony-zenoh** (helper crate) | `crates/harmony-zenoh/src/{session,pubsub,namespace,keyspace,liveliness,queryable,subscription,unicast,envelope}.rs` (6257) | Sans-I/O routing/namespace/envelope helpers. **Not** a zenoh wrapper | **WORKING (sans-I/O), MISNAMED** | No `zenoh` dep — only `zenoh-keyexpr` (Cargo.toml). No `use zenoh::`, no iroh. ~82 unit tests, none open a real session. Provides key-expr namespace (used by node), `HarmonyEnvelope` E2E encryption, pub/sub *logic*. The real Zenoh session lives in `harmony-node`/`harmony-rawlink`/`harmony-mail`, not here. |
| **Real Zenoh (data plane)** | `crates/harmony-node/src/event_loop.rs:398,1953,2018,2053,2168,2960` | Actual `zenoh::open` + `put`/`get`/`declare_queryable`/`declare_subscriber` | **WORKING (over stock transport)** | Real upstream zenoh 1.x. CID fetch (`fetch_via_zenoh` 2958), content/identity queryables, compute capacity pub/sub. This is the exercised network path. NOT over iroh. |
| **harmony-node discovery (mDNS)** | `crates/harmony-node/src/discovery.rs` (672) | mdns-sd announce/browse of `_harmony._udp.local.` | **PARTIAL** | Real announce+browse; 8 unit tests. Bootstrap/pinned peers get synthetic `0xFE`-prefixed placeholder Reticulum addresses (97-131) → not routable by Reticulum. No two-node discovery test. |
| **harmony-rawlink** (L2 bridge) | `crates/harmony-rawlink/src/bridge.rs` | Bridges raw 802.11s/Ethernet frames ↔ a real `zenoh::Session` | **PARTIAL (feature-gated, optional)** | Opens real `zenoh::Session`, `declare_subscriber` (bridge.rs:119,158). Optional `rawlink` feature on node. Not on default path. |
| **harmony-content** (transport-relevant) | `crates/harmony-content/src/storage_tier.rs` | CID content tiers; emits `DeclareQueryables`, availability Bloom filters; served via Zenoh queryables | **WORKING (via Zenoh)** | Uses `harmony_zenoh::namespace::content` keys; fetch path real through event_loop `FetchContent`→`fetch_via_zenoh`. Pin-to-retain deferred (storage_tier.rs:1090). |

---

## (a) What Genuinely Works End-to-End

"End-to-end" here means: reachable in the real `harmony` binary's serve loop (`event_loop::run`, called from `main.rs:813`), built from real I/O, and at least mechanically complete — **even where no two-node test proves it.**

- **Real Zenoh pub/sub + CID content fetch.** `event_loop.rs:398` opens a genuine zenoh 1.x session. `Publish`/`Subscribe`/`DeclareQueryable`/`FetchContent`/`FetchModule` all map to real `session.put/declare_subscriber/declare_queryable/get` (1947-2076, `fetch_via_zenoh` 2958). This is the data plane the node actually uses. It runs over Zenoh's **stock transport**, not iroh.
- **The iroh tunnel handshake + session protocol.** `harmony-tunnel` is genuinely complete and the strongest-tested piece in the stack: real PQ crypto and a `paired_machines_exchange_data` test that runs both sides through the full handshake and exchanges encrypted Reticulum and Zenoh frames in both directions (session.rs:452). It is a sans-I/O state machine, so "works" = the *protocol logic* works; the *I/O binding* is separate.
- **The node's tunnel dial/accept/route plumbing — mechanically complete.** Tracing the live path:
  - Dial: `RuntimeAction::InitiateTunnel` (emitted by real discovery logic, runtime.rs:1631/4290) → deferred queue (event_loop.rs:2082, privacy jitter) → drain spawns a **real** ephemeral `iroh::Endpoint`, `connect()`s over the relay map, returns a `ReadyConnection` (1810-1855).
  - Accept: iroh `ep.accept()` arm spawns the QUIC handshake, yields `ReadyConnection` (1232-1300).
  - Both → spawn `run_initiator`/`run_responder` (1334/1345) which run the real handshake and stream loop.
  - Inbound Reticulum: `ReticulumReceived` → `RuntimeEvent::TunnelReticulumReceived` fed back into the runtime router (1122-1137).
  - Outbound Reticulum: `RuntimeAction::SendOnInterface` for `tunnel-*` interfaces → `TunnelSender::try_send_reticulum` (1925-1930).
  - This loop is **complete and reachable**, but **untested at the I/O layer** (see below).
- **Reticulum wire-format / crypto correctness.** `reticulum_interop.rs` proves byte-exact agreement with Python RNS on name/identity/destination hashes, Ed25519 announce signatures, link-ID + HKDF key derivation, and IFAC masking. The protocol *encoding* is real and reference-correct.
- **Single-node Reticulum routing logic.** Announce processing, path-table learning, local delivery, dedup, link request/proof forwarding — all real and unit-tested through `LoopbackInterface`.
- **PeerManager lifecycle logic.** Backoff, jitter, status transitions, contact enable/disable — real and unit-tested.

## (b) What Is Stubbed / Aspirational / Inert

- **No end-to-end / two-node test exists anywhere in core.** The only files that bind/connect/accept iroh endpoints are `tunnel_task.rs` and `event_loop.rs` — **both non-test source**. `run_initiator`/`run_responder` are never called from a test; `event_loop::run` is never called from a test. The "two-party" tests are all sans-I/O simulations:
  - `runtime.rs:5500 unicast_round_trip_a_to_b_surfaces_as_unicast_received` — two `NodeRuntime`s, A's `SendOnInterface` **hand-bridged in memory** into B's `InboundPacket`. No socket, no iroh, no tunnel. Comment admits `source == None` limitation.
  - `social_routing.rs` — two `harmony_zenoh::Session` **state machines** wired together; not a network.
  - `reticulum_interop.rs` — static vectors, not a live link.
- **Explicit node-event-loop stubs (just log + drop):** `SendPathRequest` (event_loop.rs:2109), `CloseTunnel` (2115), `ReplicaPush` (2125), `ReplicaPullResponse` (2140), `QueryMemo` (2154), `SendReply` (1969). The replication *receive* side IS wired (1143-1201), but the *send* side is a stub — so durable replication across peers cannot complete.
- **Zenoh-over-tunnel not wired.** `TunnelBridgeEvent::ZenohReceived` → `// TODO(harmony-h6k): Zenoh over tunnel` (event_loop.rs:1140). The tunnel can carry a Zenoh frame tag (protocol-complete) but the node never feeds tunnel-delivered Zenoh messages anywhere.
- **Reticulum runs leaf-mode only at runtime.** `runtime.rs:954` = `Node::new()`, never `new_transport()`. So `is_transport()` is always false in the binary → the (real, tested) multi-hop relay/rebroadcast code (node.rs:680,1233,1297, `relay_packet`) is **dead at runtime**. The node delivers locally and sends on known single-hop paths but won't transit-relay.
- **Reticulum link layer stubbed in runtime.** `PeerAction::InitiateLink | CloseLink => { /* stub for now */ }` (runtime.rs:4315) — acknowledged/encrypted RNS links never initiated.
- **Zenoh liveliness/presence not wired.** `runtime.rs:1997` "Zenoh liveliness token wiring — not yet implemented"; presence is faked by processing discovery hints directly. harmony-zenoh's `liveliness.rs` is a complete *logic* module with no live token backing it.
- **Bootstrap peers aren't routable.** Pinned/bootstrap peers get synthetic `0xFE`-prefixed placeholder Reticulum addresses (discovery.rs:97-131); they're connection targets but carry no real identity binding for Reticulum routing.
- **Manual outbound tunnel CLI not wired.** `main.rs:579` warns `--tunnel-peer: outbound connections not yet wired (needs contact store, Bead #3)`. Outbound dials only happen via the discovery→`InitiateTunnel` path.
- **Deferred-by-design reconnection.** `event_loop.rs:113/609` — config-tunnel reconnection "deferred to Bead harmony-h6k"; `ConfigTunnelPeers` tracks state but doesn't yet drive reconnects.

## (c) Reticulum vs Zenoh-over-iroh in Core

- **Both Reticulum and Zenoh are present and partly real, but the headline "Zenoh-over-iroh" is NOT realized in core.**
- **Reticulum** is a large, reference-correct protocol library (`harmony-reticulum`) plus an event-translation router in `harmony-runtime`. It's the identity/path-routing inspiration Jake mentioned. At runtime it is **leaf-only** and its link layer + path-request + transit-relay are stubbed/inert. It is wired enough to carry unicast over a tunnel interface if a path is seeded, but the autonomous discovery→path→route→reply loop is not proven.
- **Zenoh** appears in two distinct forms that are easy to conflate:
  1. `harmony-zenoh` crate — a **sans-I/O helper** (namespaces, key-expr matching, pub/sub *logic*, E2E `HarmonyEnvelope`). Despite the name it has **no `zenoh` dependency and no iroh** (only `zenoh-keyexpr`). Misnamed; it is not the transport.
  2. **Real upstream zenoh** — opened directly in `harmony-node/event_loop.rs:398` (and `harmony-rawlink`, `harmony-mail`, `harmony-s3`). This is the actual pub/sub data plane and it runs over **Zenoh's own stock transport (TCP/UDP/multicast)**, NOT over iroh.
- **The "over-iroh" coupling does not exist.** The iroh tunnel (`harmony-tunnel`) and the real Zenoh session are independent. The only bridge would be the tunnel's Zenoh frame tag — which is a TODO on both the node send and receive sides (event_loop.rs:1140). So today: Zenoh ⟂ iroh. This matches the prior finding that zenoh 1.x exposes no custom-transport seam (the documented `zenoh_no_custom_transport` blocker), i.e., "Zenoh-over-iroh" was aspirational and remains unbuilt in core.

## (d) The Tunnel Transport's True Completeness

- **Protocol (`harmony-tunnel`): genuinely complete and the best-tested component.** Real ML-KEM-768 encapsulation, ML-DSA-65 transcript signatures, HKDF directional keys, ChaCha20-Poly1305 AEAD frames with per-direction nonce counters, keepalive with jitter + dead-peer timeout, four multiplexed frame types. `paired_machines_exchange_data` drives a full two-side handshake and exchanges encrypted frames both ways. No `todo!`/stub. Verdict: **WORKING (sans-I/O).**
- **I/O binding (`tunnel_task.rs` + `event_loop.rs`): mechanically complete, but unproven.** The dial is **real, not a stub** — the "(stub)" comment at event_loop.rs:2081 is stale/misleading: `InitiateTunnel` only *defers* the dial; the actual dial (ephemeral endpoint bind + `connect` over relay map) fires in the deferred-queue drain at 1810-1855. Accept, handshake spawn, stream loop, inbound→runtime feedback, and outbound routing are all wired. **What's missing is proof:** nothing in the test suite ever stands up two iroh endpoints and runs `run_initiator` against `run_responder`. So the claim "the tunnel functions end-to-end (dial→handshake→route)" is **plausible and code-complete but unverified** — exactly the kind of integration gap Jake suspects.
- **Tunnel-adjacent stubs that limit usefulness:** `CloseTunnel`, `ReplicaPush`/`ReplicaPullResponse` (replication send), and Zenoh-over-tunnel are stubbed in the event loop. So a tunnel can carry Reticulum unicast, but cannot yet carry replication or Zenoh traffic, and can't be cleanly torn down via the runtime action.

## (e) Surprising / Noteworthy

- **The two "Zenoh" things are different layers and the crate name actively misleads.** A reader assuming `harmony-zenoh` is the zenoh integration would be wrong — it has no zenoh dep. The real integration is inline in `harmony-node`. This likely contributed to the "is Zenoh even in core?" confusion: **yes, but not where its name suggests.**
- **The dial "stub" comments are stale.** The most alarming-looking comment ("The actual dial (stub) fires when the deferred queue drains", event_loop.rs:2081) describes **real, working code**. By contrast, the genuinely stubbed handlers (`ReplicaPush`, `SendPathRequest`, `CloseTunnel`, `QueryMemo`) are correctly labeled. So skim-reading comments overstates the stubbing of dialing and understates nothing.
- **Reticulum is built leaf-only despite a full transport-mode implementation.** A lot of tested relay code (`new_transport`, `is_transport` branches, `relay_packet`) is shipped but never instantiated. That's substantial dead-at-runtime surface.
- **Strong unit/sans-I/O discipline, near-zero I/O integration.** The codebase is unusually rigorous at the state-machine level (PQ crypto, RNS interop vectors, jittered backoff, dedup) yet has **no test that touches a real socket or QUIC stream between two parties.** The competence is real; the assembly proof is absent. This is the precise shape of "a bunch of functional pieces but no full integration test."
- **The phase markers tell the story.** Comments reference ZEB-216 Phase 3a (basic unicast wiring done) and defer the rest to Phase 3b / ZEB-227 (source-identity binding, full tunnel/link lifecycle) and Bead harmony-h6k (reconnection, Zenoh-over-tunnel). Core's transport is mid-build at Phase 3a, not finished and abandoned.

---

## Evidence Index (key file:line)

- Real Zenoh open: `harmony-node/src/event_loop.rs:398`
- Real iroh tunnel dial: `harmony-node/src/event_loop.rs:1810-1855`; bind `:561`; accept `:1232`
- Inbound tunnel Reticulum → runtime: `harmony-node/src/event_loop.rs:1122-1137`
- Outbound tunnel routing: `harmony-node/src/event_loop.rs:1925-1930`
- Node-event-loop stubs: `:1969` (SendReply), `:1140` (Zenoh-over-tunnel TODO), `:2109` (SendPathRequest), `:2115` (CloseTunnel), `:2125` (ReplicaPush), `:2140` (ReplicaPullResponse), `:2154` (QueryMemo)
- Tunnel protocol e2e (in-memory) test: `harmony-tunnel/src/session.rs:452 paired_machines_exchange_data`
- Tunnel stream I/O driver: `harmony-node/src/tunnel_task.rs:34 run_initiator`, `:168 run_responder`, loop `:283`
- Runtime tick/dispatch: `harmony-runtime/src/runtime.rs:2234 tick()`
- Runtime builds leaf-mode router: `harmony-runtime/src/runtime.rs:954 Node::new()`
- Reticulum link stub in runtime: `harmony-runtime/src/runtime.rs:4315`
- Zenoh liveliness not wired: `harmony-runtime/src/runtime.rs:1997`
- Reticulum router: `harmony-reticulum/src/node.rs:632 route_packet`; transport-mode `:372 new_transport`, `:391 is_transport`
- Reticulum interop vectors: `harmony-reticulum/tests/reticulum_interop.rs` (16 tests)
- In-memory two-runtime "round trip": `harmony-runtime/src/runtime.rs:5500`
- harmony-zenoh has no zenoh dep: `harmony-zenoh/Cargo.toml` (only `zenoh-keyexpr`)
- Bootstrap placeholder addrs: `harmony-node/src/discovery.rs:97-131`
- Manual outbound tunnel not wired: `harmony-node/src/main.rs:579`
- Event loop is the live serve path: `harmony-node/src/main.rs:813 event_loop::run`
