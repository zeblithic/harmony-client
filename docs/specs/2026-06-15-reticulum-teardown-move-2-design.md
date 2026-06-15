# Reticulum teardown (coalescence Move 2) — design spec

**Tickets:** ZEB-474 (harmony-client: deposit-only DM stub + drop UDP/annotations) → ZEB-475 (harmony-core: delete `harmony-reticulum` + remove the runtime router surface). Parent ZEB-321 (transport coalescence).

**Status:** DRAFT — awaiting Jake's review before plan/impl. (Design decisions already settled with Jake 2026-06-15, recorded in §3.)

**Goal:** Remove Reticulum from the Harmony codebase — the ~14.2k-LOC `harmony-reticulum` crate, the runtime router that drives it, and the client's UDP transport + dead annotations — eliminating the single largest source of transport confusion while preserving every load-bearing piece (the device directory, the device-hash formula, the shared-crypto interop coverage, and the tunnel/rawlink transports).

**Architecture (one line):** Reticulum's only live consumer is DM unicast, which is already egress-broken (LAN-broadcast only). Replace that consumer with a deposit-only DM stub (the butler/CAS store-and-forward that already exists), then delete the router and the crate; Move 1a later restores live DM delivery via the iroh tunnel.

---

## 1. Background

The forensic dive (`docs/analysis/2026-06-14-transport-04-reticulum-footprint.md`, plus `-00..-03`) established:

- **`harmony-reticulum` is large by line-count but cleanly isolated by dependency** — ~14.2k LOC, depended on by exactly two crates (`harmony-node`, `harmony-runtime`); the client pulls it only transitively via `harmony-runtime`. (Verified on `main` 2026-06-15.)
- **Almost nothing user-facing works over Reticulum today.** DM unicast is the only feature on a live Reticulum path, and that path is egress-broken in the Tauri client — `dispatch_action` discards the resolved interface and always UDP-broadcasts on the LAN (`event_loop.rs:5725`), so off-LAN DMs cannot deliver. The other "Reticulum" features (voice presence, profile broadcast) carry a `TransportBinding::Reticulum` annotation with **no packet logic** — dead metadata.
- **The load-bearing pieces are transport-agnostic.** `OwnerDeviceCache` is an `OwnerAddr → [DeviceIdentityHash]` directory populated over the iroh friend handshake (ZEB-461), not Reticulum announces; only `compute_dm_destination_hash` couples its *consumption* to Reticulum. The `DeviceIdentityHash` formula (`SHA256(x25519‖ed25519)[:16]`) is just a hash of identity pubkeys reused as the canonical device ID.
- **Crypto is safe.** `harmony-reticulum` *consumes* `harmony-identity`/`harmony-crypto`; it does not own them. Direct `ed25519`/`x25519` use is confined to the crate's `link.rs`/`ifac.rs` (deleted with the crate).

This is the coalescence Jake called for: "I don't want to support Reticulum anymore… tear out all these parts that are continuing to confuse/mislead us."

## 2. Goals / non-goals

**Goals**
- Delete `harmony-reticulum` and the runtime router surface that drives it.
- Remove the client's Reticulum transport (UDP socket, `udp0`, the interface-dropping egress) and the dead `TransportBinding::Reticulum` annotations.
- Preserve DM capability at no-worse-than-today via a deposit-only interim, and preserve all transport-agnostic machinery.
- Leave the tree green (workspace builds + tests) after each PR.

**Non-goals (this move)**
- Move 1a (DM-over-iroh) live delivery — separate (ZEB-472/473); this move only stubs DM to deposit until 1a lands.
- Full `harmony-node` retirement (the "mirage stack") — separate move; here we remove only harmony-node's *Reticulum usage*.
- Removing `harmony-tunnel` / `harmony-rawlink` — they survive as transports; only their Reticulum-carrier branches go.
- Any change to the `DeviceIdentityHash` derivation or the identity crate's crypto.

## 3. Settled design decisions (Jake, 2026-06-15)

1. **Do the teardown next, before Move 1a** (DMs are already non-functional, so decluttering-first carries no regression and hands 1a a clean `DmTransport` seam).
2. **DM-unicast interim = deposit-only stub** — route DMs to the existing butler/community-relay CAS deposit; restore live delivery in 1a.
3. **harmony-node scope = remove its Reticulum usage only**, not a full node retirement.
4. **Keep `harmony-identity/tests/reticulum_interop.rs` as-is** — it validates shared crypto (hash/sig/HKDF/encrypt) against Python vectors, not the protocol; keeping it avoids silently dropping crypto coverage. The crate-level `harmony-reticulum/tests/reticulum_interop.rs` dies with the crate.

## 4. Scope

### 4.1 REMOVE — harmony-client (ZEB-474, lands first)

- **DM transport:** replace `RuntimeUnicastTransport` (`dm_outbox.rs:207/231`, feeds the Reticulum unicast channel `tx`) with a deposit-routing `DmTransport` (see §5).
- **UDP/Reticulum transport surface (`event_loop.rs`):** `parse_reticulum_port` + `HARMONY_RETICULUM_PORT` (`:24-60`), the UDP socket bind (`:896-959`), `udp0` inbound → `InboundPacket` (`:3155-3168`), the `SendOnInterface` UDP egress (`:5725-5732`), and the `UnicastReceived` handling (`:5536-5570`). The client's event-loop match drops 15/23 runtime actions via `_ => {}` already, so removing these dispatch arms is safe even while the runtime still emits them (pre-core-removal).
- **Dead annotations:** the `TransportBinding::Reticulum` uses in `voice_presence.rs:1475` + `profile_broadcast.rs:852`, and the ZEB-367 legacy-Reticulum-path remnant in `inbound_packet.rs:62-66`. (If `TransportBinding` the enum is client-owned, drop the variant; if core-owned, drop the variant in ZEB-475 and the client uses in ZEB-474.)
- **Tests:** `tests/dm_unicast_integration.rs` + Reticulum-path assertions in the other `dm_*_integration.rs`.

### 4.2 REMOVE — harmony-core (ZEB-475, after the client stops using the APIs)

- **`harmony-runtime` router surface (`runtime.rs`):** `router: Node` field (`:754`), `Node::new`/`udp0` registration (`:954-962`), `register_announcing_destination` (`:967-988`), `SendUnicastToDevice` handling + `pending_unicast_sends` + the defer-then-drop drain (`:2206-2435`), the `SendOnInterface`/`UnicastReceived` action variants, and the `register_interface(...)` calls on `TunnelHandshakeComplete`/`L2InterfaceReady`/close (`:1926-2131`). **Keep** the tunnel/L2 lifecycle + PeerManager wiring (only the `router.*` calls go) and the Discovery(zenoh) `DiscoveryAnnounceReceived` ingestion (`:1947-2004` — distinct from Reticulum wire announces). Remove the `reticulum_identity_bytes` config field (`:79-84`) + its secret derivation (trace the client/node boot construction so no dangling `Zeroizing` secret is left).
- **`harmony-node`:** the `tunnel_senders.try_send_reticulum` branch + `l2:` Reticulum-outbound branch (`event_loop.rs:1908-1944`) + router wiring. NOT a full node retirement.
- **Cargo:** drop the `harmony-reticulum` dep in `harmony-node/Cargo.toml` + `harmony-runtime/Cargo.toml`.
- **Crate:** delete `crates/harmony-reticulum/` entirely (incl. its `tests/reticulum_interop.rs`).
- **Carrier branches:** in `harmony-tunnel`/`harmony-rawlink`, remove only the Reticulum frame-type (rawlink `0x00`) / Reticulum-payload branches; keep the crates.

#### 4.2.1 Reticulum-concept cruft (blast-radius scan 2026-06-15 — the forensic `-04` under-scoped this)

A workspace-wide scan found Reticulum references in **four more core crates** beyond `harmony-node`/`harmony-runtime`. None declare a `harmony-reticulum` Cargo dep — they reference Reticulum as a *concept* (an address kind, a fallback, a bridge, a comment), so they do **not** block the crate removal, but they are part of "tear out Reticulum" and leaving them is exactly the half-removed cruft we're trying to eliminate. All core-side; fold into ZEB-475:

- **harmony-content:** delete the dead `reticulum_bridge` module — `crates/harmony-content/src/reticulum_bridge.rs` + the `pub mod reticulum_bridge;` in `lib.rs:21`. Exposed but referenced nowhere else (verified) and the crate has no `harmony-reticulum` dep → dead. Clean delete.
- **harmony-contacts:** remove the `ContactAddress::Reticulum` variant (`contact.rs:8`) + its postcard round-trip tests (`contact.rs:160/203/221`, `store.rs:295`). With no Reticulum transport, a `Reticulum` contact address is unusable.
- **harmony-peers:** remove the Reticulum-fallback dial branch in `manager.rs` (the `has_reticulum` path, ~`:229-256`) + the `with_reticulum` test helper (~`:420-430`). This is the one piece of *live* logic among the cruft.
- **harmony-node `main.rs` + harmony-runtime:** update the `ContactAddress` match arms for the removed `Reticulum` variant.
- **harmony-platform `network.rs:11`:** fix the stale doc comment referencing the deleted crate's `Interface` trait.

This expands ZEB-475's surface but keeps it within the same intent. **Flagged for Jake** (§9): include all of it in Move 2 (recommended — leaving `ContactAddress::Reticulum` behind is confusing dead weight) vs. split the contacts/peers cleanup into a small follow-on.

### 4.3 KEEP (load-bearing, do not touch)

- `OwnerDeviceCache` + `OwnerDeviceEntry` + the CRDT replication + the ZEB-461 handshake population — all transport-agnostic.
- The `DeviceIdentityHash` formula `SHA256(x25519‖ed25519)[:16]` in `harmony-identity/src/identity.rs` — keep the formula, drop the "Reticulum address" comment framing. **Do not** "clean up" the derivation during teardown.
- `harmony-identity/tests/reticulum_interop.rs` (shared-crypto coverage).
- `harmony-tunnel`, `harmony-rawlink` (transports), and the tunnel/L2 lifecycle in the runtime/node.

## 5. The DM-unicast interim (deposit-only stub)

Today `RuntimeUnicastTransport::send` builds the signed `cidnotify` packet and `try_send`s a `UnicastSendRequest` onto the event-loop channel, which forwards it as `RuntimeEvent::SendUnicastToDevice` → the runtime router (resolve-or-drop). The butler/community-relay CAS deposit (ZEB-418/458) is a separate store-and-forward rung driven by the `DmOutbox` retry/outcome loop (`dm_outbox.rs:931-983`).

**Interim design:** the production `DmTransport` becomes a deposit-routing transport — DMs go straight to the butler/community-relay deposit path; the Reticulum unicast channel + event-loop drain + router are removed. The exact wiring (a new `DepositOnlyDmTransport` whose `send()` performs the deposit and returns `Ok`, vs. reusing the existing outcome-driven deposit rung) is a plan decision; the constraint is that it **reuses the existing deposit machinery** (no new transport protocol) and keeps `OwnerDeviceCache`/`resolve_destinations` as the directory (the device identities are what the deposit addresses).

**Honest behavior:** DMs deliver whenever the recipient has a butler / community relay configured and online; otherwise they durably queue (or surface a clear "queued for store-and-forward" outcome) until reachable. This is **no worse than today** (off-LAN already drops) and is fully restored to live delivery by Move 1a's `IrohTunnelDmTransport`, which slots into the same `DmTransport` seam.

## 6. Sequencing & PR breakdown

The client uses runtime APIs (`SendUnicastToDevice`, `SendOnInterface`, `UnicastReceived`) that ZEB-475 removes, so the order is **client-stops-using → core-removes → client-rev-bumps**:

1. **PR-1 (ZEB-474, harmony-client):** deposit-only DM stub + remove the UDP/`udp0`/`SendOnInterface`/`UnicastReceived` surface + dead annotations + delete Reticulum DM tests. Compiles against the *current* pinned harmony rev (the runtime APIs still exist; the client just stops calling/handling them). DMs → deposit-only. Full local gates + bot loop.
2. **PR-2 (ZEB-475, harmony):** remove the runtime router surface + harmony-node Reticulum usage + drop the Cargo deps + delete `crates/harmony-reticulum/` + remove the tunnel/rawlink carrier branches. New harmony rev.
3. **PR-3 (small, harmony-client):** bump the pinned harmony rev to PR-2's; trivial since the client no longer references the removed APIs. Can be folded into the next client PR if timing aligns. (One PR per repo at a time per the bundling rule; PR-1 and PR-3 are sequential on the client.)

## 7. Testing strategy

- **Non-regression (the main risk):** the workspace must build + all non-Reticulum tests pass after each PR. The teardown is mostly deletion; the proof is a green `cargo nextest run --workspace` + clippy on both repos, with the Reticulum tests removed (not skipped).
- **DM interim:** a test asserting the deposit-routing transport routes a DM to the deposit path (and surfaces the durable-queue outcome when no butler/relay is configured) — reusing existing butler/relay deposit test fixtures.
- **Directory preserved:** existing `OwnerDeviceCache` + ZEB-461 handshake-population tests must remain green untouched (proof the directory survived).
- **Crypto coverage preserved:** `harmony-identity/tests/reticulum_interop.rs` still passes (renamed framing optional; not required).
- **No dangling secret:** confirm `reticulum_identity_bytes` removal leaves no constructed-but-unused identity-secret derivation in client/node boot.

## 8. Entanglement risks (from forensic §e — carry into the plan)

1. Shared identity is **not** Reticulum-owned — removing the crate must not touch `harmony-identity`/`harmony-crypto`.
2. The device-address-hash formula is load-bearing — keep it; only restyle the comment.
3. `reticulum_identity_bytes` secret derivation — trace + remove cleanly (no dangling `Zeroizing`).
4. `compute_dm_destination_hash` is referenced in multiple DM call sites (outbox resolve, inbound ack fan-out, `handle_cidnotify_lifted`) — the deposit-routing replacement must cover all of them or inbound/outbound DM will diverge.
5. Butler/relay deposit payloads are keyed by owner/device hashes (transport-independent) — safe across the migration.

## 9. Open questions for Jake

1. **Reticulum-concept cruft scope (§4.2.1)** — the 2026-06-15 blast-radius scan found Reticulum cruft in four more core crates (harmony-content dead `reticulum_bridge`, `ContactAddress::Reticulum` in harmony-contacts, the harmony-peers Reticulum-fallback, a harmony-platform comment) that the forensic `-04` missed. Include all of it in ZEB-475 (recommended — leaving `ContactAddress::Reticulum` behind is confusing dead weight that defeats the point) vs. split the contacts/peers cleanup into a small follow-on? *(My lean: include it all.)*

Minor items I'll resolve in the plan (not design): the client UDP socket exists only for Reticulum (so it goes); whether `TransportBinding` is client- or core-owned (determines which PR drops the enum variant).
