# DM-over-iroh (coalescence Move 1a) — design spec

**Tickets:** ZEB-472 (harmony-core PR-A: `FrameTag::Dm`) → ZEB-473 (harmony-client PR-B: tunnel driver). Parent ZEB-321 (transport coalescence). Surfaced by ZEB-461.

**Status:** APPROVED (Jake, 2026-06-15). All three §9 decisions resolved; design settled — ready for plan/impl. Driven by Koya as a single workstream (Ildwyn/AVALON agents down). Pin target: harmony main now carries #279 (`FrameTag::Dm`) + #280 (Reticulum teardown).

**Goal:** Give harmony-client a working live carrier for 1:1 DMs by driving the already-built post-quantum `harmony-tunnel` session (ML-KEM-768 + ML-DSA-65 + ChaCha20-Poly1305 over iroh QUIC) directly from the client — delivering live DMs **and** real PQ DMs in one move.

**Architecture (one line):** the friend handshake (ZEB-461 / PR #269) already learns a peer's iroh reachability + PQ keys on the wire; persist them, and on first DM lazily dial a per-peer PQ tunnel over the client's persistent iroh endpoint, carrying the existing sealed+signed DM bytes under a new `FrameTag::Dm`, delivering inbound frames into the existing DM verify/decrypt pipeline. Deposit (butler / community-relay CAS) remains the durability fallback for offline peers.

**Tech stack:** `harmony-tunnel` (sans-I/O PQ session state machine, harmony-core), iroh 0.98 QUIC (client's persistent endpoint + ALPN acceptor/dialer convention), `harmony-identity::PqPrivateIdentity`/`PqIdentity`, the client's `DmTransport` seam + owner-state CRDT.

---

## 1. Background

### 1.1 The dead-end ZEB-461 found

ZEB-461 set out to prove 1:1 DM byte-delivery between two co-located headless nodes (e2e-harness S2). It failed: friendship goes Active both ways, DM spaces create, `OwnerDeviceCache` populates — but **no DM byte ever arrives**. Root cause (forensic dive `docs/analysis/2026-06-14-transport-00..06`): the client has **no live DM carrier**. `RuntimeAction::InitiateTunnel` is dropped via `_ => {}` (one of 15/23 RuntimeActions the client ignores), and the iroh tunnel transport (`harmony-tunnel` driver + `HARMONY_TUNNEL_ALPN` + `tunnel_task.rs`) lives **only in harmony-node**, which the client does not run. Every DM is dropped `SendUnicastToDevice destination_hash unknown`.

### 1.2 What PR #269 already established (the reachability primitive)

PR #269 (ZEB-461, branch `zeb-461-reachability-primitive`) landed the transport-agnostic half of the fix:

- The friend handshake wire (`FriendLinkRequest` / `FriendLinkAccepted`) carries the sender's **device bundle** (`sender_devices` + `device_identity_pubs`, bound into the signature via `devices_digest`) **and** the sender's **iroh reachability + PQ keys** (`iroh_node_id: [u8;32]`, `home_relay_url: Option<String>`, `pq_dsa_pubkey: Vec<u8>`, `pq_kem_pubkey: Vec<u8>` — **unsigned routing hints**).
- On receipt, the device bundle is persisted into `OwnerDeviceCache` (anti-forgery: local HLC, skip-on-empty).
- **But the received reachability + PQ keys are dropped** (`iroh_friend_acceptor.rs` receive path uses them only self-side at accept-build; `OwnerDeviceEntry` has no fields to store the peer's).

So the wire already carries everything a dialer needs; PR-B persists it and consumes it.

### 1.3 The convergence

`harmony-tunnel` is the best-built core transport asset — a complete PQ session, currently wasted (inert in client, harmony-node-only). Driving it from the client carries DM bodies **and** makes DMs genuinely post-quantum (today DMs seal under X25519+ChaCha20+Ed25519 despite both peers advertising PQ keys — the "defaulting to Curve25519" Jake flagged). This is the elegant move: one carrier, two wins.

---

## 2. Goals / non-goals

**Goals**
- Live 1:1 DM byte-delivery between two friends over a direct PQ iroh tunnel.
- Reuse the existing sealed+signed DM packet and the existing inbound DM verify/decrypt/ingest pipeline unchanged.
- Per-peer lazy tunnels with keepalive, idle teardown, reconnect/backoff, and buffered-send-while-dialing.
- Deposit fallback preserved: offline/unreachable peer ⇒ DM still delivered via CAS deposit, never lost.
- e2e-harness S2 finally hard-asserts DM delivery.

**Non-goals (this move)**
- Reticulum teardown (Move 2, separate tickets) — PR-B only *bypasses* the inert Reticulum egress; full removal is later.
- Group-DM / channel traffic over the tunnel (`FrameTag::Zenoh`/`Replication` already exist; out of scope).
- pkarr PQ-key publication (the true PQ-discovery blocker; tracked under Move 4 / a separate ticket).
- Multi-device fan-out beyond the parallel-vec shape (alpha is single-device; structure supports N, traffic does not exercise it).

---

## 3. Approved design decisions (Jake, 2026-06-14 — "approve all")

1. **Persist into `OwnerDeviceEntry`.** The peer's reachability + PQ keys are stored on the friend's device entry on handshake receipt; the friend directory is the tunnel-contact source of truth.
2. **Lower-NodeId initiator** as the simultaneous-dial collision tie-break (see §6.2 for the precise reading).
3. **Per-device lazy tunnels**, keyed by peer NodeId = `blake3(ML-DSA pubkey)`; a friend with N devices = N tunnels; dial-on-first-DM.
4. **Two-PR cross-repo split** (ZEB-472 core first, ZEB-473 client bumps the pinned rev).

**Mixed-version handshake: flag-day-for-alpha** (Jake, 2026-06-14). No migration path; identities/state are resettable. This frees PR-B to change the owner-state CBOR shape *and* the handshake signature preimage (§7) without back-compat.

---

## 4. Data flow

### 4.1 Outbound (send a DM to a friend)

```text
app sends DM
  → DM outbox builds sealed+signed packet  [unchanged: build_signed_cidnotify/encode_packet]
  → IrohTunnelDmTransport::send(entry, recipient, _destinations)   [NEW DmTransport impl]
       resolve recipient OwnerAddr → peer device entries (OwnerDeviceEntry)
       for each device: NodeId + reachability + PQ keys
         TunnelManager.send_dm(node_id, reachability, pq_identity, packet_bytes):
           - session Active   → SendDm(packet) over the live tunnel
           - session Dialing  → enqueue in the per-session buffered queue
           - no session       → spawn dialer (lazy), enqueue, dial on persistent endpoint
  → ALSO deposit to butler / community-relay CAS   [unchanged durability fallback]
```

### 4.2 Inbound (receive a DM)

```text
peer dials our persistent endpoint with HARMONY_TUNNEL_V1
  → accept loop routes ALPN → late-installed tunnel acceptor (run_responder analogue)
       TunnelSession::new_responder → handshake → Active
       on decrypted FrameTag::Dm frame → TunnelAction::DmReceived{payload}
  → payload (the same sealed+signed bytes) → existing DM ingest:
       verify_cidnotify_admission → decrypt_and_bind_dm_blob → apply_inbox → emit dm-received
```

The inbound DM payload is byte-identical to what the Reticulum path delivered, so **no new ingest** — the tunnel receive side feeds the existing pipeline.

---

## 5. Component design

### 5.1 PR-A (harmony-core, ZEB-472): `FrameTag::Dm`

`harmony-tunnel`'s `FrameTag` (`frame.rs:10`) is a closed `#[repr(u8)]` enum; `from_byte` rejects unknown tags. The frame body is already opaque `Vec<u8>`. Add a dedicated DM tag (smuggling inside `FrameTag::Reticulum` is wrong and that tag dies in Move 2):

- `FrameTag::Dm = 0x04` (`frame.rs`).
- `TunnelEvent::SendDm { payload: Vec<u8>, now_ms: u64 }` (`event.rs`) → `handle_send(FrameTag::Dm, payload)`.
- `TunnelAction::DmReceived { payload: Vec<u8> }` (`event.rs`) → emitted from the inbound-frame path on a `Dm` frame.
- Tests: `Dm` frame round-trip (encrypt → decrypt → `DmReceived`); unknown-tag rejection still holds.

Small, additive, no wire break for existing tags. Bumps the client's pinned `harmony` rev.

### 5.2 `OwnerDeviceEntry` extension + receive-side persistence (client)

`OwnerDeviceEntry` (`owner_state_types.rs:439`) today: `devices: Vec<DeviceIdentityHash>`, `device_identity_pubs: Vec<Option<[u8;64]>>`, `learned_at: Hlc`. Add **one** new parallel-indexed field (mirrors the existing parallel-vec convention):

```rust
/// Per-device tunnel contact, parallel-indexed to `devices`. `None` = not yet learned.
pub device_tunnel_contacts: Vec<Option<DeviceTunnelContact>>,

pub struct DeviceTunnelContact {
    pub iroh_node_id: [u8; 32],
    pub home_relay_url: Option<String>,
    pub pq_dsa_pubkey: Vec<u8>,   // ML-DSA-65 pk (1952 B)
    pub pq_kem_pubkey: Vec<u8>,   // ML-KEM-768 pk (1184 B)
}
```

- `#[serde(default)]` on the new field (cheap insurance even under flag-day; keeps unrelated CBOR fixtures decodable).
- `apply_owner_device_update` (`owner_state_crdt.rs:600`) gains a parallel `device_tunnel_contacts` argument, validated for length-parity with `devices` (same rule as `device_identity_pubs`).
- Receive sites in `iroh_friend_acceptor.rs` (request handler ~`:1012` and accept handler) pass the received `req.{iroh_node_id, home_relay_url, pq_dsa_pubkey, pq_kem_pubkey}` as the contact for the sender's single device. Anti-forgery rule unchanged: local HLC, skip-on-empty.
- Owner-state CBOR shape changes (CRDT-synced + persisted) — covered by flag-day. Wire-format pinning fixtures regenerated.

### 5.3 Tunnel ALPN + inbound acceptor (client)

- New const `HARMONY_TUNNEL_V1 = b"harmony/tunnel/v1"` in `iroh_endpoint::alpn`, added to the `.alpns(vec![…])` bind list (invariant: a new protocol needs BOTH the ALPN in the bind list AND a dispatch arm).
- Accept-loop arm in `zenoh_iroh_transport.rs::spawn_accept_loop`: `else if conn.alpn() == HARMONY_TUNNEL_V1` → the late-installed `OnceLock` tunnel acceptor (butler/relay pattern — the acceptor needs `PqPrivateIdentity`, available only after identity boot).
- `IrohZenohLinkManager` gains `tunnel_acceptor: OnceLock<Arc<…>>` + `install_tunnel_acceptor(...)`, installed at the same boot point identity becomes available.
- Acceptor body = `run_responder` analogue: `conn.accept_bi()` → read length-prefixed `TunnelInit` → `TunnelSession::new_responder` → write `TunnelAccept` → run the select loop; on `DmReceived`, hand the payload to the inbound DM ingest (§5.6) and register the session in the `TunnelManager` (§5.5) so the bidirectional tunnel is reused for our outbound DMs to this peer.

### 5.4 Tunnel dialer (client)

- A new dialer fn adapted from the friend/butler dial primitive (NOT `iroh_dial_driver.rs`, which is zenoh-specific). Pattern: build `iroh::EndpointAddr::new(node_id).with_relay_url(..).with_ip_addr(..)` from the persisted `DeviceTunnelContact` → `iroh_endpoint.inner().connect(addr, HARMONY_TUNNEL_V1)` → `open_bi()` → `run_initiator` protocol (write `TunnelInit`, read `TunnelAccept`, feed via `handle_event` → Active).
- **Uses the client's one long-lived persistent endpoint** (diverging intentionally from harmony-node's throwaway-ephemeral-per-tunnel) so the peer can dial us back by our stable EndpointId.
- Peer `PqIdentity` constructed from the persisted `pq_dsa_pubkey` + `pq_kem_pubkey` via `PqIdentity::from_public_keys`. Self inputs from the client's existing `PqPrivateIdentity` (`identity.rs:60`) — no new key minting.

### 5.5 `TunnelManager` (client) — the session map + lifecycle

A new `TunnelManager` (on `IrohZenohLinkManager` or standalone, `Arc`-shared), keyed by **peer NodeId**:

```text
sessions: Mutex<HashMap<NodeId, TunnelHandle>>
TunnelHandle { cmd_tx, state: Dialing|Active|Closing, role: Initiator|Responder, pending: VecDeque<Vec<u8>> }
```

- **Lazy dial-on-first-DM:** `send_dm(node_id, contact, packet)` — if Active, `cmd_tx.SendDm`; if Dialing, push to `pending`; if absent, insert a `Dialing` handle, spawn the dialer task, push to `pending`. On handshake → Active, flush `pending`.
- **Keepalive:** the per-session task drives `TunnelEvent::Tick` on the session's jittered interval (harmony-tunnel: 25–35 s; dead-peer at 110 s). A dead peer → session closed → re-dial on next DM.
- **Idle teardown:** after a configurable idle window with no app DM traffic (proposed 5 min), close the tunnel to free resources; the next DM re-dials. (Keepalive detects death; idle-teardown reclaims healthy-but-unused tunnels.)
- **Reconnect/backoff:** dial failure → exponential backoff, bounded retries; on exhaustion the DM relies on the deposit fallback (§5.7).
- **Collision dedup (lower-NodeId, §6.2):** on a completed dial OR accept, if a session already exists for that peer NodeId, keep the one whose **initiator has the lower NodeId** and close the loser. Both sides apply the identical rule and converge on the same surviving tunnel. One (bidirectional) tunnel per peer NodeId suffices.

### 5.6 `IrohTunnelDmTransport` (client) — the `DmTransport` seam

`dm_outbox.rs:40` defines `trait DmTransport { async fn send(&self, entry, recipient: OwnerAddr, destinations: Vec<[u8;16]>) }`. Today the live impl is `RuntimeUnicastTransport` (Reticulum, egress-broken). Add `IrohTunnelDmTransport`:

- Ignores `destinations` (RNS 16-byte hashes — Reticulum-shaped). Instead resolves `recipient` OwnerAddr → its `OwnerDeviceEntry` device list → per device `(NodeId, DeviceTunnelContact)`.
- Builds the sealed+signed packet via the **shared** sign+seal helper (factor `build_signed_cidnotify`/`encode_packet` out of `RuntimeUnicastTransport` into a free fn both transports call — the packet is transport-independent opaque ciphertext).
- For each device NodeId: `TunnelManager.send_dm(node_id, contact, packet)`.
- Installed as the DM transport in place of `RuntimeUnicastTransport`. The inert Reticulum egress (`SendUnicastToDevice` at `event_loop.rs:~3614`) is bypassed; full Reticulum removal is Move 2.

### 5.7 Durability fallback (unchanged, made explicit)

DMs already deposit to butler / community-relay CAS (ZEB-418/458) as the durability layer for offline recipients. The tunnel is the **live** path. Recipients dedup (the inbox is a CRDT; DM packets are content-addressed/signed). Proposed alpha orchestration: **always deposit (durability) + attempt tunnel (liveness)**; recipient dedups. Optimization (suppress deposit on fast tunnel ack) deferred — flagged for the plan. A failed/slow dial therefore degrades to deposit, never to data loss.

---

## 6. Cross-cutting design points

### 6.1 Identity & NodeId

- Tunnel NodeId = `blake3(peer ML-DSA pubkey)` (`harmony-tunnel session.rs:33`) — this is the per-device tunnel key. Independent of the classical device-identity-hash used for DM destinations; the tunnel keys off PQ identity.
- Self `PqPrivateIdentity` already minted from the boot seed (`identity.rs:86`). Peer `PqIdentity` from the persisted PQ pubkeys.

### 6.2 Initiator collision rule — precise reading

I read "lower-NodeId initiator" (decision 2) as a **simultaneous-dial tie-break, not a dial prohibition** — either peer may lazily dial on its first outbound DM (otherwise the higher-NodeId peer could never initiate a conversation without a separate nudge channel). The rule resolves the rare race where both dial at once:

> Keep the tunnel whose **initiator has the numerically lower NodeId**; close the other.

The PQ tunnel is bidirectional once Active (directional `i2r`/`r2i` keys, both usable), so a single tunnel per peer NodeId carries both directions. Both sides apply the identical deterministic rule and converge on the same survivor (worked example: A<B, both dial → A→B survives on both sides; B drops its outbound and adopts A's inbound). **Confirm this reading.** (If you intended strict "only the lower NodeId ever dials," we'd need a dial-request nudge over an always-available channel — slower first-DM for the higher peer; I recommend against it for alpha.)

### 6.3 Security — unsigned hints vs. signing the PQ keys (decision point)

PR #269 ships reachability + PQ keys as **unsigned** hints. MITM analysis: if an attacker tampers the peer `pq_dsa_pubkey`, I derive the attacker's NodeId, dial the attacker, and the tunnel's built-in identity check (`session.rs:228`) "passes" against the attacker. **But** the DM payload is independently sealed (X25519-ECDH to the recipient's *classical* identity) and Ed25519-signed before it reaches the tunnel — so the attacker gets opaque ciphertext it cannot decrypt or forge, and cannot deliver to the real recipient ⇒ at worst a **DoS, covered by the deposit fallback** (recipient still receives via CAS). Confidentiality and liveness hold.

However: under active MITM the **PQ-transport property is silently downgraded** (you tunnel to an attacker). Since Jake wants real PQ (decision (ii) hybrid-everywhere) and flag-day makes a signature-preimage change free, **I recommend PR-B bind the reachability + PQ keys into the handshake signature** (extend the signed `devices_digest` to a `contact_digest` covering `(devices, pubs, iroh_node_id, relay, pq_dsa, pq_kem)`), upgrading them from unsigned hints to signed — making the PQ tunnel non-downgradeable. **Confirm: sign them (recommended) vs. ship unsigned for alpha + sign as fast-follow.**

### 6.4 Flag-day surface

PR-B changes: (a) owner-state CBOR (new `OwnerDeviceEntry` field), (b) optionally the handshake sig preimage (§6.3). Both are wire/persistence breaks, covered by flag-day-for-alpha. Wire-format pinning fixtures (`tests/wire_format_*`) regenerated; the regen is intentional and reviewed, not silent.

---

## 7. Testing strategy

- **PR-A unit (core):** `Dm` frame encrypt/decrypt round-trip; unknown-tag still rejected; `SendDm`→`OutboundBytes`, `InboundBytes(Dm)`→`DmReceived`.
- **PR-B unit (client):** `OwnerDeviceEntry` reachability persistence + length-parity validation; `apply_owner_device_update` parallel-vec rules; `TunnelManager` dedup convergence (lower-NodeId survivor); dialer address construction from `DeviceTunnelContact`; buffered-send flush on Active.
- **PR-B integration:** acceptor responder loop end-to-end (in-process two-session handshake + one `Dm` frame); `IrohTunnelDmTransport.send` routes to the manager.
- **e2e-harness S2 (the DoD):** `s2_friend_graph_and_dm_send` flips from characterization to **hard-assert** — two co-located headless `harmony-app` nodes exchange a real DM byte over the tunnel. (This is the exact test that failed pre-#269 and motivated the whole dive.)
- **Cross-WAN:** real two-machine proof (Koya ↔ AVALON/Ildwyn) per the standing AVALON-needed caveat — tracked, not gating PR merge.

---

## 8. PR breakdown

- **PR-A — ZEB-472 (harmony-core):** `FrameTag::Dm` + `SendDm`/`DmReceived` + tests. Merge first.
- **PR-B — ZEB-473 (harmony-client):** bump pinned rev + add `harmony-tunnel` dep; `OwnerDeviceEntry` extension + persistence; tunnel ALPN + acceptor; dialer; `TunnelManager`; `IrohTunnelDmTransport`; inbound→ingest wiring; (recommended) sign reachability/PQ; S2 hard-assert. One PR per the bundle-small-PRs rule.

---

## 9. Resolved (Jake, 2026-06-15 — "approve all", confirmed via Koya rehash)

1. **§6.2 — Tie-break, confirmed.** "Lower-NodeId" is a simultaneous-dial collision tie-break, NOT a dial prohibition: either peer may lazily dial on its first outbound DM; the rare double-dial race resolves to the tunnel whose initiator has the numerically lower NodeId (both sides apply the identical deterministic rule and converge on one survivor).
2. **§6.3 — Sign now, confirmed.** PR-B binds the reachability + PQ keys into the handshake signature (extend the signed `devices_digest` → `contact_digest` over `(devices, pubs, iroh_node_id, relay, pq_dsa, pq_kem)`), upgrading them from unsigned hints to signed → the PQ tunnel is non-downgradeable. Free under flag-day.
3. **§5.7 — Always-deposit + attempt-tunnel, confirmed.** Alpha orchestration always deposits (durability) AND attempts the tunnel (liveness); recipient dedups. The suppress-deposit-on-fast-ack optimization is deferred.

**Implementation note (post-#271 reconciliation):** this spec predates ZEB-474 (#271) merging. The live DM transport on current main is `DepositOnlyDmTransport` (not `RuntimeUnicastTransport`), so PR-B installs `IrohTunnelDmTransport` in place of `DepositOnlyDmTransport`, and additionally clears the now-dangling dormant refs the pin-bump exposes: the `unicast_send_rx → SendUnicastToDevice` arm (`event_loop.rs`) and `NodeConfig::reticulum_identity_bytes` (`lib.rs`). The plan reflects the current code, not this section's pre-#271 line references.
