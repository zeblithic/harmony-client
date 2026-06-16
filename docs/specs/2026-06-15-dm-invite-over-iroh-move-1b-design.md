# Move 1b — DM-Space `DmInvite` over the iroh tunnel (design)

- **Ticket:** ZEB-482 (Move 1b: re-wire the DM-Space `DmInvite` carrier onto the iroh tunnel)
- **Parent epic:** ZEB-321 (transport coalescence)
- **Builds on:** ZEB-473 (Move 1a — the PQ tunnel DM carrier, merged as #272 / `0539f801`)
- **Durability follow-up:** ZEB-483 (deposit the invite for offline / cross-WAN bootstrap parity — explicitly out of scope here)
- **Date:** 2026-06-15
- **Status:** Draft (for review)
- **Scope:** single harmony-client PR. **No harmony core change.**

---

## 1. Problem & context

ZEB-473 restored a live DM byte-carrier: two co-located nodes establish a PQ iroh tunnel, and `IrohTunnelDmTransport` routes a signed `CidNotify` over `TunnelManager::send_dm`. The S2 hard-assert proved the `CidNotify` reaches the peer's `ingest_dm_packet` — **but every tunnel DM is then rejected at `verify_cidnotify_admission: SpaceNotFound`.**

Root cause: a DM `Space` is minted with a random per-owner `SpaceId` + a per-`Space` `content_key`. To admit and decrypt a DM, the recipient must already hold the *same* `Space` the sender signed/encrypted under. That `Space` used to reach the peer via a `DmInvite` packet carried over the Reticulum unicast removed in harmony#280. `add_space_impl` still *builds* the per-device invite packets but then **discards** them (`lib.rs:10573`, comment: "re-wiring the invite fan-out onto the iroh tunnel is a later move"). The only surviving `Space` carrier is owner-state-root CRDT sync, which is per-owner KeyTree-keyed (same-owner devices only) — so two *friend owners* share **no** cross-owner DM-`Space` carrier.

**Move 1b re-points the already-built invite fan-out at the ZEB-473 tunnel.**

## 2. Goal & Definition of Done

Two co-located nodes friend → DM → the recipient bootstraps the DM `Space` from the invite and the message delivers end-to-end.

**DoD:** un-ignore `e2e-harness/tests/e2e_two_node.rs::s2_dm_delivery_over_tunnel_hard_assert` and it passes — the recipient fires a `dm-received` event carrying the body, and the plaintext lands in the DM thread (both hard asserts green).

## 3. Key decision — client-only framing (no core `FrameTag`)

The tunnel frame layer (`harmony-tunnel::FrameTag`: `Keepalive`/`Zenoh`/`Replication`/`Dm`) is the **transport** multiplex layer; its `Dm` payload is opaque to core (emitted as `TunnelAction::DmReceived { payload }`). Within `FrameTag::Dm`, the payload is a client-side `DmPacket` whose **existing** discriminant already distinguishes `Invite` / `CidNotify` / `Ack`, and `dm_envelope::decode_packet` already parses all three.

Today `ingest_dm_packet` decodes the packet and *hard-rejects* anything that isn't a `CidNotify` ("tunnel DM packet is not a CidNotify"). The whole change is to **relax that to dispatch on the decoded `DmPacket` variant**.

Adding a core `FrameTag::DmInvite` (as one earlier analysis suggested) would hoist an application-DM-protocol distinction into the transport frame layer — splitting one protocol across two layers and adding a needless cross-repo dependency. The `DmPacket` enum is the correct seam, and it already exists.

**Consequence:** the harmony pin stays at `8b870ae`; ZEB-482 is a single harmony-client PR.

## 4. Durability scope — tunnel-only now (ZEB-483 owns deposit parity)

The invite rides the live tunnel only. `CidNotify` follows the "always-deposit + attempt-tunnel" pattern, but the deposit rung does **not** carry invites today, so a peer offline at Space-creation time cannot bootstrap the `Space` from a later-recovered deposited `CidNotify`. Closing that needs deposit-carrier wiring for invites + cross-WAN proof (AVALON), tracked as **ZEB-483**. For the co-located DoD here, tunnel-only is sufficient and correct.

## 5. Architecture / data flow

```text
SENDER (Alice creates DM Space with Bob)
  add_space_dm_inner  ── builds per-recipient-device DmInvite wire bytes (already exists)
  add_space_impl      ── STOP discarding `sends`; route each invite over the tunnel:
                          for each recipient owner → DeviceTunnelContact
                            → NodeId = blake3(pq_dsa_pubkey)
                            → TunnelManager::send_dm(node_id, contact, invite_wire)   [FrameTag::Dm]
  (later) first message → IrohTunnelDmTransport → send_dm(CidNotify)                  [FrameTag::Dm]
                          ↑ invite is enqueued BEFORE the first CidNotify → tunnel `pending` FIFO

RECIPIENT (Bob)
  run_tunnel_loop → dispatch_tunnel_actions → TunnelAction::DmReceived { payload }
    → ingest channel → ingest_dm_packet:
        decode_packet(payload) →
          DmPacket::Invite    → apply_invite (reuses handle_invite: write Space + cache + identity_pub)
          DmPacket::CidNotify → existing admission → CAS fetch → CID rebind → Phase-C → decrypt → apply_inbox → emit dm-received
          DmPacket::Ack       → existing / ignore
```

## 6. The seam (forensic anchors)

| Concern | Location | Notes |
|---|---|---|
| Invite fan-out built then discarded | `src-tauri/src/lib.rs:10573` (discard `_sends`); built `:10409–10430` | `sends: Vec<UnicastSendRequest>` — `{destination_hash, packet: invite_wire}` per recipient device |
| Invite packet type | `src-tauri/src/dm_envelope.rs:68–115` (`DmInviteSigned`) | space_id, kind, members, inviter, `content_key`, sender_devices, `inviter_identity_pub` (inline 64B), created_at, signing_device_hash; Ed25519-signed; ~500–700B |
| Wire envelope + decode | `src-tauri/src/dm_envelope.rs` (`DmPacket` enum, `decode_packet`) | discriminated `Invite`/`CidNotify`/`Ack`; `Invite` discriminant `0x01` |
| Inbound auto-accept handler | `src-tauri/src/dm_outbox.rs:1527–1641` (`handle_invite`) | verifies sig w/ inline pubs, writes `Space` + `apply_owner_device_update` + identity_pub; idempotent on `space_id`; **intact + unit-tested, just uncalled** |
| Inbound DM ingest (the gate to relax) | `src-tauri/src/dm_inbox_ingest.rs:354–364` | currently CidNotify-only reject; generalize to dispatch on `DmPacket` |
| Recipient→tunnel resolution | `src-tauri/src/iroh_tunnel_dm_transport.rs` | resolves owner → `device_tunnel_contacts` → `blake3(pq_dsa)` NodeId → `send_dm`; factor a shared helper |
| Send ordering buffer | `src-tauri/src/tunnel_manager.rs:89–90, 405–409` | per-peer `pending: VecDeque` flushed FIFO on `Active` |
| DoD test | `e2e-harness/tests/e2e_two_node.rs:441–451` | `#[ignore]`'d with the SpaceNotFound diagnosis |

## 7. Detailed design

### 7.1 Send path — re-wire the discarded fan-out

`add_space_dm_inner` already returns `sends` (per-recipient-device `DmInvite` wire bytes). Today `add_space_impl` binds it to `_sends` and drops it. Instead:

1. `add_space_dm_inner`'s `sends` is repurposed to carry, per non-self recipient **owner** (not Reticulum `destination_hash`), the signed `invite_wire` bytes. (The invite body is identical per recipient; addressing differs.)
2. After the owner-state write lock is released, `add_space_impl`'s caller routes each recipient's invite over the tunnel using the **same recipient→`DeviceTunnelContact`→`blake3(pq_dsa)`→`TunnelManager::send_dm`** resolution that `IrohTunnelDmTransport` already performs. Factor that resolution+send into one shared helper (e.g. `tunnel_send_packet_to_owner(crdt_state, mgr, owner, packet_bytes)`) so both the invite path and the CidNotify path call it.
3. The `TunnelManager` is reachable via `NodeState` (published in ZEB-473). If the node is deposit-only (no `TunnelManager`), the invite send is a no-op for now (ZEB-483 adds the deposit rung); the `Space` is still applied locally, so the sender is unaffected.
4. **Lock discipline:** building `invite_wire` happens under the state lock (it reads owner-device cache + signs); `send_dm` (which locks the session map and may spawn a dial) runs **after** the state lock is dropped — never nest the two.

### 7.2 Receive path — generalize the inbound DM dispatch

`ingest_dm_packet` currently:

```rust
let packet = decode_packet(packet_bytes)?;
let DmPacket::CidNotify { signed, signature, signed_bytes } = packet
else { return Err("tunnel DM packet is not a CidNotify".into()); };
// … admission → CAS → decrypt → apply …
```

Generalize to dispatch on the decoded variant:

```rust
match decode_packet(packet_bytes)? {
    DmPacket::Invite { signed, signature, signed_bytes } => {
        // reuse the existing auto-accept logic (handle_invite)
        apply_tunnel_invite(crdt_state, signed, signature, &signed_bytes, now_ms).await
    }
    DmPacket::CidNotify { signed, signature, signed_bytes } => {
        // existing path unchanged (admission, CAS rebind, Phase-C, decrypt, apply_inbox, emit)
    }
    DmPacket::Ack { .. } => { /* existing behavior / ignore */ }
}
```

`handle_invite` is a `&mut self` method on the outbox carrying `&mut OwnerState`. The ingest path holds the `crdt_state` lock but may not have the outbox handle, so extract `handle_invite`'s body into a free function `apply_invite(state: &mut OwnerState, signed, signature, signed_bytes, wall_now_ms)` that both the (dormant) outbox method and the new ingest arm call — no behavior change, single source of truth. Verification uses the invite's inline `inviter_identity_pub`, so it is self-contained (no prior cache entry required).

### 7.3 Sequencing & idempotency

- The invite is sent at **Space-creation** time; the first `CidNotify` is sent later at **message-send** time. So for a given peer the invite is enqueued strictly before any `CidNotify` for that `Space`, and the tunnel's `pending` FIFO + in-order frame delivery means the recipient applies the `Space` before it processes the first `CidNotify`. No explicit barrier needed for the common path.
- `apply_invite` is idempotent on `space_id` (`apply_space_with_canonicalization` dedups), so a re-sent invite (e.g. Space re-mint on outbox race, or multiple recipient devices) is harmless.
- If a `CidNotify` ever arrives before its invite (pathological reordering / future deposit path), it rejects `SpaceNotFound` and the sender's normal retry re-delivers once the `Space` is present — no corruption, just a retry. (Robust ordering across the deposit rung is a ZEB-483 concern.)

### 7.4 Reused machinery (no new transport)

- `TunnelManager::send_dm` / per-device tunnel keyed by `blake3(pq_dsa_pubkey)` — unchanged.
- `DeviceTunnelContact` persisted during the friend handshake (ZEB-473) — the sender already holds the recipient's tunnel contact at DM-create time.
- `handle_invite` auto-accept — reused via the extracted `apply_invite`.

## 8. Out of scope / explicitly NOT changed

- **Invite durability / deposit** → ZEB-483 (offline/cross-WAN bootstrap parity).
- **Member rotation** — the protocol has no post-creation member mutation; the invite carries the full sorted member list for validation, unchanged.
- **GroupDm fan-out beyond what `add_space_dm_inner` already builds** — same code path; no group-specific work added here.
- **No core (`harmony`) change**; the harmony pin is untouched.

## 9. Testing

1. **DoD:** un-ignore `s2_dm_delivery_over_tunnel_hard_assert`; it must pass (dm-received event + plaintext in thread).
2. **Send routing (unit):** `add_space_impl` for a DM with a recipient that has a `DeviceTunnelContact` invokes `send_dm` with the recipient's `blake3(pq_dsa)` NodeId and the `DmInvite` wire bytes; a deposit-only node (no `TunnelManager`) is a no-op and still applies the `Space` locally.
3. **Receive dispatch (unit):** `ingest_dm_packet` given a `DmPacket::Invite` payload applies the `Space` + caches the inviter (via `apply_invite`) and does **not** emit `dm-received`; given a `CidNotify`, the existing path is unchanged (regression guard).
4. **Ordering (unit/integration):** an invite enqueued before a `CidNotify` to the same peer flushes invite-first; the recipient admits the subsequent `CidNotify` (no `SpaceNotFound`).
5. **Idempotency (unit):** applying the same invite twice yields one `Space` (no divergence), mirroring `handle_invite`'s existing idempotence test.

## 10. Risks & mitigations

- **Outbox handle not available in ingest** → extract `apply_invite` free function (single source of truth); low risk, mechanical.
- **Lock nesting (state lock vs session-map lock)** → enforce "build under state lock, send after drop"; covered by the send-routing test + clippy.
- **Invite/CidNotify race in pathological reordering** → idempotent apply + admission-reject-then-retry; acceptable for co-located scope, robust ordering deferred to ZEB-483.
- **Recipient lacks a `DeviceTunnelContact`** → invite send is a no-op (Space applied locally); full handling is the ZEB-483 deposit path. For S2 the contact is present (persisted at handshake).
