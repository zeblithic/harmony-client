# Transport-04: Reticulum Footprint & Teardown Scope

**Date:** 2026-06-14
**Scope:** READ-ONLY forensic analysis. No code changed.
**Repos:** `harmony` core (`crates/`, ignoring `.worktrees/`) + `harmony-client` (`src-tauri/`).
**Decision this informs:** Jake has decided Reticulum is likely more trouble than it's worth and wants to deprecate/remove it. Replacement substrate: **Zenoh-over-iroh pub/sub**. This doc scopes the teardown and identifies what a Zenoh-over-iroh path must replace.

---

## TL;DR

- The Reticulum footprint is **large by line-count but cleanly isolated by dependency**. The `harmony-reticulum` crate is ~14.2k LOC (5.6k in `node.rs` alone) and is depended on by exactly **two** core crates (`harmony-node`, `harmony-runtime`); the client depends on it only **transitively** via `harmony-runtime`.
- **Almost nothing user-facing actually works over Reticulum today.** DM unicast is the only feature wired to a live Reticulum path, and that path is **egress-broken in the Tauri client**: the client's `dispatch_action` discards the resolved interface and always UDP-broadcasts on the LAN (`event_loop.rs:5725`). Off-LAN DMs over Reticulum cannot deliver. This matches the reported symptom.
- The features that "bind to Reticulum" (voice presence, profile broadcast) carry a `TransportBinding::Reticulum` **annotation only** — no packet logic. Dead metadata.
- **OwnerDeviceCache survives the teardown.** It is a transport-agnostic `OwnerAddr -> [DeviceIdentityHash]` directory populated over the **iroh friend handshake** (ZEB-461), not Reticulum announces. Only the consuming `resolve_destinations -> compute_dm_destination_hash` step bakes in the Reticulum `"harmony.dm"` app+aspect; that one function is the only Reticulum-coupled line in the DM directory path.
- **Crypto is safe.** Reticulum reuses the shared Ed25519/X25519 identity (`harmony-identity`) but does not own it. Removing Reticulum does not touch identity. The one wrinkle: the device-address-hash formula (`SHA256(x25519_pub || ed25519_pub)[:16]`) is documented as "Reticulum address derivation" and is reused everywhere as the canonical device ID — keep the formula, drop the protocol.
- **Difficulty: moderate, and front-loaded by one thing** — DM unicast must get a working Zenoh-over-iroh delivery path *before or with* the teardown, because it is the single live consumer. Everything else is delete-and-stub. The butler/community-relay deposit rungs (ZEB-418/458) already provide a non-Reticulum store-and-forward fallback for DMs, which de-risks the migration.

---

## (a) Reticulum Surface Inventory

### A1. The `harmony-reticulum` crate — `crates/harmony-reticulum/`

A partial-but-substantive, **sans-I/O** Reticulum (RNS) implementation. Binary-compatible with Python RNS at the packet/hash/signature/HKDF level (proven by `tests/reticulum_interop.rs` against Python test vectors). Implements packet wire format, announces, path table, link handshake, IFAC, cooperation scoring, and (partial) resource transfer. It does **not** do its own I/O (relies on an `Interface` trait) and the resource/channel protocols are incomplete.

| File | LOC | Role | Crypto entanglement |
|---|---|---|---|
| `lib.rs` | 38 | Public re-exports (44 types) | — |
| `node.rs` | 5628 | **Core router.** `Node` struct: `register_interface`/`unregister_interface`, `register_destination`, `register_announcing_destination`, `announce`, `route_packet`, `handle_event` (InboundPacket/TimerTick), `path_table()`. Does announce validation+rebroadcast, path discovery, data relay, link-request forwarding, proof routing, rate limiting, cooperation scoring. | via `harmony_identity::{Identity,PrivateIdentity}`, `harmony_crypto::hash` (no direct dalek) |
| `packet.rs` | 695 | Wire format: Type1/Type2 headers, flags bitfield, 500B MTU, `from_bytes`/`to_bytes` | `harmony_crypto::hash` (SHA256 trunc) |
| `path_table.rs` | 820 | Distance-vector path table, hop count, replay detection, `PathEntry`/`PathUpdateResult` | hash only |
| `resource.rs` | 3449 | Chunked resource transfer state machines (partial; much `dead_code`); `LinkCrypto` trait | hash; crypto abstracted via trait |
| `link.rs` | 1099 | Link handshake (3-way: X25519 ECDH + Ed25519 sig → 64B Fernet key) | **direct** `ed25519_dalek` + `x25519_dalek` + `harmony_crypto::{hkdf,fernet}` |
| `ifac.rs` | 418 | Interface Access Control — derive Ed25519 IFAC identity, XOR-mask packets | `harmony_identity::PrivateIdentity`, `harmony_crypto::{hash,hkdf}` |
| `announce.rs` | 607 | Build/validate signed announce packets (`ValidatedAnnounce`) | `harmony_identity::{Identity,PrivateIdentity}`, hash |
| `cooperation.rs` | 412 | EMA-weighted broadcast-interface scoring (`CooperationTable`) | — |
| `destination.rs` | 212 | App-layer naming `app.aspect -> name_hash + destination_hash` (`DestinationName`) | hash |
| `interface.rs` | 169 | `Interface` trait + `InterfaceMode` (Full/PointToPoint/...) — sans-I/O transport abstraction | — |
| `loopback.rs` | 199 | In-memory test `Interface` | — |
| `packet_hashlist.rs` | 238 | Two-generation dedup set | — |
| `context.rs` | 117 | `PacketContext` wire byte enum | — |
| `error.rs` | 93 | `ReticulumError` (wraps Identity/Crypto errors) | wrapped |

**Reverse deps (the entire blast radius of the crate):**
- `crates/harmony-node/Cargo.toml`
- `crates/harmony-runtime/Cargo.toml`
- (client pulls it transitively via `harmony-runtime`; no direct dep in `harmony-client/src-tauri/Cargo.toml`)

### A2. The Reticulum router in `harmony-runtime` — `crates/harmony-runtime/src/runtime.rs`

The runtime embeds a `Node` as **Tier 1** (`router: Node`, `runtime.rs:754`) and drives it sans-I/O.

| Surface | Location | Notes |
|---|---|---|
| `Node::new()` + register `udp0` (Full mode) | `runtime.rs:954-962` | Every node registers a `udp0` interface at boot. |
| `register_announcing_destination` (`harmony.node`, 30s) | `runtime.rs:967-988` | Gated on `config.reticulum_identity_bytes` (64B = X25519+Ed25519 secret, `runtime.rs:79-84`). Periodic announces. |
| `RuntimeEvent::SendUnicastToDevice { destination_hash:[u8;16], packet }` | `runtime.rs:259-266` | Client→runtime DM unicast request. Queued into `pending_unicast_sends`, resolved on `tick()`. |
| Unicast drain + **defer-then-drop** | `runtime.rs:2369-2435` | If `path_table().get(&destination_hash)` hits → `route_packet`; else if router queue non-empty → defer; else **WARN + drop**. The drop is the silent-failure point when no announce ever populated a route. |
| `register_interface(InterfaceMode::PointToPoint)` on tunnel handshake | `runtime.rs:1926-1930` (`TunnelHandshakeComplete`) | Tunnel becomes a routable Reticulum interface. |
| `register_interface(Full)` on `L2InterfaceReady` | `runtime.rs:2120-2127` | `l2:*` raw-link interfaces. |
| `unregister_interface` on tunnel/L2 close | `runtime.rs:1943-1945`, `2128-2131` | |
| Announce ingestion `DiscoveryAnnounceReceived` | `runtime.rs:1947-2004` | Note: this is the **Discovery/zenoh** announce, fed to `DiscoveryManager` + tunnel-hint processing — distinct from Reticulum wire announces. |
| `NodeAction::SendOnInterface{interface_name, raw, weight}` → `RuntimeAction::SendOnInterface` | `runtime.rs:2608-2625` | Router emits egress carrying the resolved interface name. |
| `RuntimeAction::UnicastReceived{destination_hash, source, packet}` | `runtime.rs:436-445` | Inbound packet delivered to a locally-registered destination. `source` is always `None` (link-identity binding never landed). |

### A3. The tunnel-as-Reticulum-interface bridge

| Component | Location | Role |
|---|---|---|
| Tunnel handshake → bridge event | `harmony-node/src/session.rs` / `tunnel_task.rs:500-520`, `tunnel_bridge.rs:11-48` | Tunnel completes (ML-DSA + ML-KEM, BLAKE3 NodeId) → `TunnelBridgeEvent::HandshakeComplete`. Tunnel crypto is **PQ, entirely separate from Reticulum's Ed25519/X25519.** |
| `harmony-node` event loop wiring | `harmony-node/src/event_loop.rs:1085-1225` | Maps bridge events to `RuntimeEvent::{TunnelHandshakeComplete, TunnelReticulumReceived, TunnelClosed}`. Maintains `tunnel_senders: HashMap<String, TunnelSender>` keyed by interface name (`event_loop.rs:515`). |
| **`harmony-node` egress (interface-aware)** | `harmony-node/src/event_loop.rs:1908-1944` | `SendOnInterface` routes by name: `l2:*` → rawlink `ret_outbound_tx`; `tunnel-*` → `tunnel_senders.get(name).try_send_reticulum`; else UDP broadcast + per-peer unicast. **This is the working multi-interface path — but only in the standalone `harmony-node` binary.** |
| Interface naming | `tunnel-<8hex>` (`harmony-node/src/event_loop.rs:1264-1273`), `l2:<ifname>` (`event_loop.rs:483`), `udp0` (`harmony-runtime/src/adapter.rs:69`) | |

### A4. `harmony-rawlink` and `harmony-tunnel` — adjacent, NOT Reticulum-owned

- **`harmony-rawlink`** (`lib.rs`, `bridge.rs`, `frame.rs`, `batch.rs`, `padded_socket.rs`): a **general L2 Ethernet framing layer** (`HARMONY_ETHERTYPE 0x88B5`) with frame-type discriminators: RETICULUM(0x00), SCOUT(0x01), DATA/zenoh(0x02), BATCH(0x03) (`rawlink/src/lib.rs:26-35`). Only frame-type 0x00 carries Reticulum. **Survives teardown** as a standalone L2 transport (Zenoh-over-L2 keeps working); the 0x00 branch orphans.
- **`harmony-tunnel`** (`event.rs`, `frame.rs`, `session.rs`): the iroh-QUIC tunnel. Payloads are **opaque** to the tunnel (`event.rs:5-40`: `ReticulumReceived`/`ZenohReceived`/`ReplicationReceived` are just decrypted byte buffers). The tunnel **exposes itself to** Reticulum as an interface; it does not depend on Reticulum. **Survives teardown** intact.

### A5. `HARMONY_RETICULUM_PORT` and the client UDP socket

| Surface | Location |
|---|---|
| `parse_reticulum_port` (unset→4242; `0`→disabled; garbage→warn+default) | `harmony-client/src-tauri/src/event_loop.rs:24-60` |
| UDP bind (degradable, ZEB-446) | `event_loop.rs:896-959` |
| Inbound UDP → `RuntimeEvent::InboundPacket{interface_name:"udp0"}` | `event_loop.rs:3155-3168` |
| **Egress: `RuntimeAction::SendOnInterface{raw, weight, ..}` → `udp.send_to(raw, broadcast_addr)`** | `event_loop.rs:5725-5732` |
| Test toggle `HARMONY_RETICULUM_PORT=0/=port` | `tests/profile_isolation.rs:16,31,103` |

**Decisive asymmetry (the core bug):** the client's `dispatch_action` (`event_loop.rs:5725`) **discards `interface_name`** and always UDP-broadcasts to `broadcast_addr`. The runtime's router can resolve a `tunnel-*` or `l2:*` interface and emit `SendOnInterface` for it, but the client egress ignores that and LAN-broadcasts. So in the actual Tauri app, Reticulum unicast can only reach a same-LAN peer; there is no off-LAN delivery. (The `harmony-node` binary, A3, does route by interface — but users run the client, not `harmony-node`.)

---

## (b) Feature-Dependency Table

| Feature | How it uses Reticulum | Working over Reticulum? | Teardown class |
|---|---|---|---|
| **DM unicast delivery** | `dm_outbox::RuntimeUnicastTransport::send` (production transport, `lib.rs:3867`) resolves `OwnerAddr`→dest hashes via `resolve_destinations` (`dm_outbox.rs:64-86`) → `compute_dm_destination_hash` (`dm_signing.rs:243-273`, embeds `"harmony.dm"`), pushes `UnicastSendRequest` → `RuntimeEvent::SendUnicastToDevice` (`event_loop.rs:3616-3627`) → runtime path-table resolve-or-drop. | **No (off-LAN). Partial (same-LAN).** Wire delivery never tested end-to-end (`tests/dm_unicast_integration.rs:1-18` mocks at the channel boundary, defers real-wire to a smoke ticket). Client egress LAN-broadcasts only (`event_loop.rs:5725`). Silent drop when no path-table route (`runtime.rs:2421`). | **(c) load-bearing — needs Zenoh-over-iroh replacement.** This is the one live consumer. |
| **DM fallback: butler / community-relay deposit** | NOT Reticulum. Store-and-forward CAS deposit rungs (ZEB-418 P1 / ZEB-458 P4) run when the direct send fails. `dm_outbox.rs:931-983`. | **Yes** (separate transport). | n/a — already the non-Reticulum durability path; de-risks DM migration. |
| **DM inbound ack fan-out** | `compute_dm_destination_hash` per sender device → `unicast_send_tx` (`dm_outbox.rs:1836-1849`). Same egress fate as outbound DM. | Same as DM unicast. | (c) — rides the DM replacement. |
| **Voice presence** | `TransportBinding::Reticulum { participants: vec![] }` annotation only (`voice_presence.rs:1475`). No send/recv/encode. | **No — dead metadata.** Voice rides iroh. | **(a) safe delete** (drop the enum variant / annotation). |
| **Profile broadcast** | `TransportBinding::Reticulum` annotation only (`profile_broadcast.rs:852`). No broadcast logic. Profile rides zenoh/iroh. | **No — dead metadata.** | **(a) safe delete.** |
| **Reachability records** | `announced_at_ms` timestamps; LAN-path unregister deferred as "disproportionate for the legacy Reticulum path" (`inbound_packet.rs:62-66`, ZEB-367). | **No / vestigial.** iroh path preferred. | **(a) safe delete** of the Reticulum-path remnants. |
| **Periodic node announces** | `register_announcing_destination("harmony.node", 30s)` (`runtime.rs:967-988`) gated on `reticulum_identity_bytes`. Emits announces over `udp0` → LAN broadcast. | **Partial (LAN only).** Populates peers' path tables on-LAN; no value off-LAN. Peer discovery off-LAN is pkarr+Discovery/zenoh, not Reticulum announces. | **(b) replaceable** — peer/route discovery is already done by Discovery(zenoh)+pkarr+friend-handshake; the Reticulum announce is redundant on-LAN convenience. |
| **Inbound Reticulum packet receive** | `RuntimeAction::UnicastReceived` interception (`event_loop.rs:5536-5570`, `inbound_packet.rs`). | Only fires for same-LAN broadcast packets addressed to a registered local dest. | (c) — the inbound DM side rides the DM replacement. |
| **L2 rawlink interface** | `l2:*` registered as Full Reticulum interface (`runtime.rs:2120`). | Experimental; not on the client UDP path. | **(b)** — rawlink survives; its Reticulum frame-type orphans. |
| **Tunnel-as-Reticulum interface** | `tunnel-*` PointToPoint interface (`runtime.rs:1926`). Routable in `harmony-node` only. | Works in `harmony-node` binary; **not in client** (egress ignores interface). | **(b)** — the tunnel survives; its role as a Reticulum carrier is what's removed. |

---

## (c) Teardown Scope + Sequencing

### What to DELETE (safe — class a)
1. `crates/harmony-reticulum/` — entire crate (~14.2k LOC) once the two reverse deps are cut.
2. `harmony-runtime` Reticulum router surface: `router: Node` field, `Node::new`/`udp0` registration (`runtime.rs:954-962`), `register_announcing_destination` (`967-988`), `SendUnicastToDevice` handling + `pending_unicast_sends` + the defer-then-drop drain (`2206-2435`), `SendOnInterface`/`UnicastReceived` action variants, `TunnelReticulumReceived`/`L2InterfaceReady` → `register_interface` calls (`1926-2131`), the `AnnounceReceived`/`AnnounceNeeded` Node-action dispatch (`2626-2677`). Keep the **tunnel/L2 lifecycle and PeerManager** wiring — only the `router.*` calls go.
3. `harmony-node/Cargo.toml` + `harmony-runtime/Cargo.toml` reticulum dep lines.
4. Client: `HARMONY_RETICULUM_PORT` parse + UDP bind + `udp0` inbound + `SendOnInterface` UDP egress (`event_loop.rs:24-60, 896-959, 3155-3168, 5725-5732`). Decide whether to keep a UDP socket at all (only Reticulum used it).
5. Dead annotations: `TransportBinding::Reticulum` variant and its uses in `voice_presence.rs:1475`, `profile_broadcast.rs:852`; ZEB-367 legacy-Reticulum-path remnant in `inbound_packet.rs`.
6. `node.rs` (`harmony-node`) `tunnel_senders.try_send_reticulum` branch + `l2:` reticulum-outbound branch (`event_loop.rs:1919-1930`).
7. Reticulum-interop tests: `harmony-reticulum/tests/reticulum_interop.rs`, `harmony-identity/tests/reticulum_interop.rs` (the identity one cross-checks shared crypto vs Python RNS — see entanglement note before deleting).
8. Client DM-over-Reticulum tests: `tests/dm_unicast_integration.rs` and the Reticulum-path assertions in `dm_*_integration.rs`.

### What BREAKS (must replace — class c)
- **DM unicast send + inbound ack** is the only live functional consumer. Removing the runtime unicast path with no replacement leaves only the butler/community-relay deposit rungs (which require a butler/relay to be set and online). A direct-delivery Zenoh-over-iroh DM path should land first.

### What needs a Zenoh-over-iroh replacement to PRESERVE capability (class b/c)
1. **DM unicast (c):** replace `RuntimeUnicastTransport` so that, instead of producing a Reticulum dest hash and dropping when no path exists, it publishes/queries the recipient's per-device DM key on the Zenoh-over-iroh mesh (every node has its own pub/sub over an iroh QUIC link). The recipient device subscribes to its own DM key-expr. Keep `OwnerDeviceCache` as the directory (see d).
2. **Peer/route discovery (b):** already covered by Discovery(zenoh) + pkarr + friend handshake; the Reticulum 30s announce is redundant. No replacement needed beyond confirming on-LAN discovery still works without it.
3. **Tunnel + rawlink (b):** survive as transports. Under Zenoh-over-iroh they carry zenoh, not Reticulum frames — the carrier role is what's removed, not the crates.

### Suggested sequencing
1. **Land the Zenoh-over-iroh direct-DM path** (new `DmTransport` impl) alongside the existing one, behind the same `OwnerDeviceCache` directory. Verify two-machine delivery.
2. **Cut over** `lib.rs:3867` to the new transport; keep butler/relay rungs as fallback.
3. **Delete** the client Reticulum egress/ingress + `HARMONY_RETICULUM_PORT` + UDP socket.
4. **Delete** the runtime `router` surface and the `harmony-reticulum` Cargo deps.
5. **Delete** the crate + tests + dead annotations.
6. Keep `harmony-tunnel` and `harmony-rawlink`; remove only their Reticulum carrier branches.

---

## (d) OwnerDeviceCache Verdict

**Classification: (c) MIXED, but effectively salvageable — the directory is transport-agnostic; only one consuming function is Reticulum-coupled.**

- **Definition:** `OwnerDeviceCache { devices: BTreeMap<OwnerAddr, OwnerDeviceEntry> }` (`owner_state_types.rs:439-442`). Value carries `devices: Vec<DeviceIdentityHash>` + parallel `device_identity_pubs: Vec<Option<[u8;64]>>` + `learned_at: Hlc` (`owner_state_types.rs:445-499`).
- **The stored hash is generic, not Reticulum-specific.** `DeviceIdentityHash = [u8;16] = SHA256(x25519_pub || ed25519_pub)[:16]` (`dm_signing.rs:275-289` → `harmony_identity::Identity::address_hash`, `identity.rs:52-69` → `harmony_crypto::hash::truncated_hash`). It is a per-device identity fingerprint any transport can address.
- **Population is over iroh, not Reticulum.** ZEB-461 fills the cache from the **friend handshake**: the requester advertises `self_device_bundle` (`dm_tunnel_contact.rs:20-26`), the acceptor calls `apply_owner_device_update` (`iroh_friend_acceptor.rs:950-971`). Also replicated by CRDT owner-state sync (`owner_state_crdt.rs`). No Reticulum announce involved. **ZEB-461 work is transport-agnostic and survives the teardown.**
- **Only the consumption step is coupled.** `resolve_destinations` (`dm_outbox.rs:64-86`) maps each `DeviceIdentityHash` through `compute_dm_destination_hash` (`dm_signing.rs:243-273`), which prepends `SHA256("harmony.dm")[:10]` to produce a **Reticulum destination hash**. That single function is the only Reticulum-bound line in the DM directory path.
- **Migration cost: small.** Keep the cache as-is. Add a transport-neutral `resolve_device_identities(cache, recipient) -> Vec<DeviceIdentityHash>` and let the Zenoh-over-iroh adapter derive its own key-expr/address from the device identity hash (instead of `compute_dm_destination_hash`). The directory, its CRDT replication, and the ZEB-461 handshake population all carry over unchanged.

---

## (e) Entanglement Risks

1. **Shared identity, not Reticulum-owned.** `harmony-reticulum` consumes `harmony-identity::{Identity, PrivateIdentity}` and `harmony-crypto::{hash, hkdf, fernet}`; it does not define them. Identity, owner state, DM signing, tunnels, and discovery all use the same crate. **Removing Reticulum does not touch identity.** Direct `ed25519_dalek`/`x25519_dalek` use inside the crate is confined to `link.rs` and `ifac.rs` (deleted with the crate).
2. **The device-address-hash formula is load-bearing and Reticulum-flavored.** `SHA256(x25519_pub || ed25519_pub)[:16]` is documented in `harmony-identity/src/identity.rs` as "matches Reticulum's address derivation" and is reused as the canonical `DeviceIdentityHash` across owner state, DM directory, and friend graph. **Keep the formula** (it's just a hash of identity pubkeys); only stop calling it a Reticulum address. Do not "clean up" `identity.rs` derivation during teardown.
3. **`reticulum_identity_bytes` config field** (`runtime.rs:79-84`) is a 64B X25519+Ed25519 secret derived from the owner identity. It only feeds `register_announcing_destination`. Removing it is safe but trace its construction in the client/node boot to avoid leaving a dangling `Zeroizing` secret derivation.
4. **`harmony-identity/tests/reticulum_interop.rs`** cross-checks the shared crypto (hashes, signatures, HKDF, encrypt) against Python RNS vectors. These vectors validate `harmony-identity`/`harmony-crypto` correctness, not the Reticulum protocol per se. **Consider retaining** (or porting the assertions to non-RNS-framed tests) so the teardown doesn't silently drop crypto-correctness coverage of the identity crate.
5. **`compute_dm_destination_hash` is referenced in multiple DM call sites** (outbox resolve, inbound ack fan-out `dm_outbox.rs:1838`, `handle_cidnotify_lifted`). The replacement addressing function must cover all of them, or inbound/outbound DM will diverge.
6. **Butler/relay deposit payloads** are keyed by `recipient_owner` / device hashes, not Reticulum dest hashes — they already work transport-independently, so the fallback rung is safe across the migration.

---

## Appendix: key file:line anchors

- Client egress drops interface name: `harmony-client/src-tauri/src/event_loop.rs:5725`
- Runtime defer-then-drop unicast: `harmony/crates/harmony-runtime/src/runtime.rs:2369-2435`
- `node` binary interface-aware egress: `harmony/crates/harmony-node/src/event_loop.rs:1908-1944`
- Production DM transport = `RuntimeUnicastTransport`: `harmony-client/src-tauri/src/lib.rs:3867`
- DM Reticulum dest-hash coupling: `harmony-client/src-tauri/src/dm_signing.rs:243-273`
- OwnerDeviceCache def: `harmony-client/src-tauri/src/owner_state_types.rs:439-499`
- ZEB-461 cache population (iroh handshake): `harmony-client/src-tauri/src/iroh_friend_acceptor.rs:950-971`
- DM wire test mocks at channel boundary: `harmony-client/src-tauri/tests/dm_unicast_integration.rs:1-18`
- Reticulum crate reverse deps: `harmony-node/Cargo.toml`, `harmony-runtime/Cargo.toml` only
