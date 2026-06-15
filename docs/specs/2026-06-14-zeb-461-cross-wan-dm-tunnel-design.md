# ZEB-461 — Cross-WAN 1:1 DM byte-delivery via the friend-established Reticulum tunnel

- **Ticket:** ZEB-461 (parent ZEB-451 agent-testing ecosystem; sits inside the ZEB-321 cross-WAN transport story)
- **Date:** 2026-06-14
- **Status:** Draft — pending Jake's review
- **Author:** Koya
- **Approach:** "Complete the tunnel chain" (bundled Phase 1 + Phase 2), approved 2026-06-14

## Goal

Make a 1:1 DM between two **friends** actually deliver bytes end-to-end — both cross-WAN (different LANs / NAT) and between two co-located headless `serve` nodes (the e2e harness, scenario S2). Today the DM is accepted and CAS-queued, but the bytes never arrive: `send_dm` retries forever with `transport temporarily unavailable: no known devices for recipient`, and even if it got past that there is no network route between the peers.

The deliverable is that the e2e-harness S2 scenario can **hard-assert** `alice→bob` and `bob→alice` byte-delivery, instead of merely characterizing it.

## Background — how DM delivery actually works

`send_dm` → `DmOutbox` → `resolve_destinations(OwnerDeviceCache, recipient)` → device-destination hashes → `RuntimeUnicastTransport::send` → `unicast_send_tx` → `RuntimeEvent::SendUnicastToDevice` → **harmony-runtime** Reticulum router → **path table** lookup → `NodeAction::SendOnInterface`.

harmony-runtime routes a unicast packet over whichever interface the **path table** names for the destination hash, and it has three interface classes (`harmony-runtime/src/runtime.rs:958`, `harmony-node/src/event_loop.rs:1918`):

| Interface | Wire | WAN-capable? |
| --- | --- | --- |
| `udp0` | LAN UDP broadcast + mDNS unicast | No (LAN only) |
| `l2:*` | AF_PACKET raw Ethernet | No (LAN only) |
| `tunnel-*` | **iroh QUIC (NAT-traversing, DERP relay)** | **Yes — already wired** |

The path table is populated **only by inbound Reticulum ANNOUNCE packets**, arriving on *any* interface including `tunnel-*` (`harmony-reticulum/src/node.rs:1092`). A `tunnel-*` interface is registered when an iroh tunnel handshake completes (`RuntimeEvent::TunnelHandshakeComplete`); the tunnel is opened by `try_initiate_tunnel` (`runtime.rs:1559`) once a peer's reachability lands in the **contact store** as a `ContactAddress::Tunnel`. The tunnel ALPN is `harmony-tunnel/1` (separate ALPN, same iroh `Endpoint` as the friend and zenoh ALPNs).

**The transport already exists.** We are not building a carrier — we are completing a chain that currently never fires for friends.

## Problem — the two gaps, per relationship

DM byte-delivery requires **both** of:

1. **`OwnerDeviceCache` populated** for the recipient — so `resolve_destinations` yields device-destination hashes. The cache is written only by inbound `DmInviteSigned` / `DmCidNotifySigned` packets and intra-fleet CRDT replication — never by an announce, and **never by the friend handshake** (which writes only the friend graph). The friend token and `FriendLinkRequest`/`FriendLinkAccepted` carry no device bundle. → bootstrap chicken-and-egg.
2. **A path-table route over a `tunnel-*` interface** — which requires an iroh Reticulum tunnel established between the two peers **plus** Reticulum announces propagated over it.

Status today:

| Relationship | Cache (which devices) | Tunnel + announce (the route) |
| --- | --- | --- |
| Community co-members, real machines | ✗ still needs bootstrap | ✓ mostly works (discovery → `try_initiate_tunnel`) |
| **Friends without a community** | ✗ handshake carries no bundle | ✗ handshake writes friend graph only — no `Contact`/tunnel |
| Co-located harness (default) | ✗ | ✗ no discovery→tunnel trigger fires |
| Co-located + `--add-tunnel-peer` | ✗ | ✓ a manual tunnel works **even co-located** |

The last row is the key enabler: a `tunnel-*` can be established between two nodes on one host (`harmony-node/src/main.rs:764` constructs a `ContactAddress::Tunnel`, calls `contact_store_mut().add()`, then `push_event(ContactChanged)`), which means the harness can prove real byte-delivery on a single machine.

## Design — complete the chain at friend-handshake time

When two owners become friends over the iroh `harmony/friend/v1` handshake, that handshake will additionally **(a)** teach each side the other's device set (populating `OwnerDeviceCache`) and **(b)** register the other as a tunnel `Contact` so `try_initiate_tunnel` opens a `tunnel-*` between them. Once the tunnel is up, Reticulum announces propagate bidirectionally, the path table learns the route, and the already-queued DM drains over iroh QUIC.

### Wire changes — `FriendLinkRequest` and `FriendLinkAccepted`

Both structs live in harmony-client (`src-tauri/src/iroh_friend_acceptor.rs`). Add, to **both** (each side sends its own):

- `sender_devices: Vec<DeviceIdentityHash>` — the sender's bound device set (sorted), for `OwnerDeviceCache`.
- `device_identity_pubs: Vec<Option<[u8; 64]>>` — parallel `X25519||Ed25519` identity pubs (mirrors the `DmInviteSigned` shape consumed by `apply_owner_device_update`).
- `iroh_node_id: [u8; 32]` — the sender's iroh `EndpointId`, for the tunnel `Contact`.
- `home_relay_url: Option<String>` — the sender's DERP relay, if any.
- `pq_dsa_pubkey: Vec<u8>` / `pq_kem_pubkey: Vec<u8>` — the sender's ML-DSA / ML-KEM public keys (see "PQ-key sourcing" — this is the load-bearing open item).

All additive and `#[serde(default)]` for forward/backward compatibility (pre-alpha, both ends are the same build in practice, but defaulting keeps a mixed-version handshake from hard-failing).

### Handshake-completion actions (both requester and acceptor)

After the handshake verifies and the friend graph is written (`apply_friend_update`), at the two injection points — requester in `lib.rs` (`connectivity_link_friend_iroh_inner`, after `apply_friend_update`) and acceptor in `iroh_friend_acceptor.rs` (after `process_friend_request`):

1. **Populate the device cache:** `state.apply_owner_device_update(peer_addr, sender_devices, device_identity_pubs, learned_at = local_wall_now)` — reusing the existing CRDT merge, its sort/dedup, and its identity-pub→hash gate. Local wall-clock for `learned_at` mirrors the `DmInviteSigned` anti-forgery convention.
2. **Register the tunnel contact:** build `Contact { identity_hash: peer_addr_as_16, addresses: vec![ContactAddress::Tunnel { node_id: peer_iroh_node_id, relay_url, direct_addrs: vec![] }], peering: { enabled: true, .. }, .. }`, then `runtime.contact_store_mut().add(contact)` (or `get_mut` to merge if it exists), then `runtime.push_event(RuntimeEvent::ContactChanged { identity_hash })`. This is the exact `--add-tunnel-peer` sequence, invoked programmatically.
3. harmony-runtime auto-wires the rest: `ContactChanged` → `try_initiate_tunnel` → tunnel handshake on `harmony-tunnel/1` → `TunnelHandshakeComplete` → `tunnel-*` registered → announces flow → path table learns the route → the queued DM drains.

### Data available at the injection points

- **Requester** already decodes the inviter's `iroh_node_id` + `home_relay_url` from the friend-token routing blob, and now receives the inviter's device bundle + PQ keys from `FriendLinkAccepted`.
- **Acceptor** does **not** get the peer's iroh node id from the live `Connection`, so it must read it from the new `FriendLinkRequest` wire fields (hence carrying `iroh_node_id`/`home_relay_url` in the request, not just relying on the connection).

### PQ-key sourcing — the crux / main cross-repo question

`try_initiate_tunnel` needs the peer's **PQ public keys** (`peer_dsa_pubkey`, `peer_kem_pubkey`), which today it reads from the **Discovery cache** (populated by Zenoh discovery announces). Two friends-without-a-community have never exchanged discovery records, so those keys are absent — the tunnel can't initiate. We therefore carry the PQ keys in the friend handshake, but they must reach the place `try_initiate_tunnel` reads them. Two options:

- **Option A (no harmony schema change):** after the handshake, inject the peer's PQ keys into the discovery cache via an existing/near-existing runtime entry point (e.g. a `RuntimeEvent` that records a peer's public keys), then push `ContactChanged`. Preferred if such an entry point exists or is a small addition.
- **Option B (harmony-contacts change):** extend `ContactAddress::Tunnel` (or `Contact`) to carry the PQ keys, and have `try_initiate_tunnel` read them from the contact rather than the discovery cache. Cleaner data-flow, but a cross-repo schema change to `harmony-contacts` + `harmony-runtime`.

This is the one decision to settle during the implementation plan, after confirming `try_initiate_tunnel`'s exact key-source and whether a runtime PQ-key event already exists. **Also verify** the node has its own PQ keypair readily available to place in the outgoing handshake (where it is derived/stored).

## Security model

- The device bundle is **self-asserted** by the peer (an owner declares their own devices) and signed by the handshake's device signature; `apply_owner_device_update` re-derives each device hash from the provided identity pub and rejects mismatches, and uses local wall-clock for `learned_at`. This matches the existing trust model for `DmInviteSigned`. Worst case a peer lists a device that can't decrypt → that destination simply fails, no security loss.
- `iroh_node_id` / `relay_url` / PQ keys are routing hints; the `harmony-tunnel/1` handshake performs its own PQ-authenticated key agreement, so a wrong/forged hint yields a failed tunnel, not a compromised one.
- No new trust is extended beyond "this verified friend told me how to reach their devices."

## Cross-repo boundary

- **harmony-client (primary):** wire-struct fields; both handshake-completion injection points; contact construction + `contact_store_mut().add()` + `push_event(ContactChanged)` (existing public runtime APIs); sourcing self's device bundle, iroh node id/relay, and PQ keys for the outgoing handshake; e2e-harness S2 changes.
- **harmony (only if Option B, or if PQ-key injection needs a new event):** a `harmony-contacts`/`harmony-runtime` change to carry/consume PQ keys on the tunnel path. Kept minimal; confirmed during planning. Cross-repo PR is acceptable (Jake approved).

## Risks & open questions

1. **PQ-key plumbing (highest):** exact source that `try_initiate_tunnel` reads, and whether injecting handshake-carried keys needs a new harmony entry point (Option A vs B). Resolve first in the plan.
2. **Co-located tunnel viability:** confirm two iroh endpoints on one host actually complete the `harmony-tunnel/1` handshake (loopback/relay) — the `--add-tunnel-peer` evidence says yes; validate empirically early.
3. **Announce propagation over a freshly-registered tunnel:** confirm both the local announce is emitted over `tunnel-*` and inbound announces populate the path table with `interface=tunnel-*` (no `InterfaceMode` gating). Evidenced but verify.
4. **Bootstrap timing:** the DM is queued before the tunnel exists; rely on the outbox's existing retry/backoff to drain once the route appears. Confirm backoff window is short enough that S2 converges within its poll budget; if not, nudge a drain on `TunnelHandshakeComplete`.
5. **Subagent-reported file:line are a strong starting map, not verified ground truth** — re-confirm each call site during TDD before editing.

## Testing strategy

- **Unit (harmony-client):** handshake-completion populates `OwnerDeviceCache` (assert the recipient's entry holds the expected device hashes + identity pubs) and writes a `ContactAddress::Tunnel` + emits `ContactChanged`. Wire round-trip (CBOR) for the new fields incl. empty/default back-compat.
- **Integration:** a two-engine test that drives friend handshake → asserts cache + contact populated → (where feasible in-process) asserts a `tunnel-*` registers.
- **e2e-harness S2:** flip `s2_friend_dm_exchange` from characterize to **hard-assert** `alice→bob` + `bob→alice` byte-delivery (co-located), using the friend-handshake-established tunnel. This is the headline proof and the ZEB-461 DoD.
- **Cross-machine (ZEB-444/AVALON):** the canonical cross-WAN proof, run when AVALON is live; coordinate via ZEB-470.

## Implementation order (within the single bundled PR)

1. Wire fields + CBOR round-trip tests (additive, defaulted).
2. Cache population at both injection points (`apply_owner_device_update`) + unit tests — this alone removes "no known devices".
3. Tunnel contact registration + `ContactChanged` at both injection points; resolve the PQ-key option; verify `tunnel-*` establishes.
4. e2e-harness S2 hard-assert + any drain-nudge needed.

## Out of scope

- Device-cache **replication** semantics (Phase-2-of-original; not needed for friend tunnel + DM).
- Community-co-member (non-friend) DM bootstrap — different carrier question; tracked separately.
- Butler / community sealed-relay store-and-forward (ZEB-418/458) — the offline path, orthogonal.
- The owner-global-Zenoh-topic routing question (ZEB-466, Ildwyn) — related but separate; coordinate, don't absorb.
