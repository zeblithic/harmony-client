# ZEB-214 — Opt-in per-DM read receipts (design)

**Status:** approved design, 2026-08-06
**Ticket:** ZEB-214 (parent ZEB-206). Label: Feature. Priority: Low.
**Scope of this cut:** 1:1 DMs, ephemeral (live-only) delivery, send-only toggle.

## 1. Problem & premise refresh

DM read-state today is deliberately private: the owner-state CRDT `ReadMarker { space_id, last_read_at: Hlc }` (`owner_state_types.rs`) syncs only across the owner's own bound devices and is **never** sent to a DM peer (a deliberate departure from IRCv3 MARKREAD broadcast). ZEB-214 adds an **opt-in** way to tell a DM peer "I've read up to here."

Two premises in the original ticket are stale and are corrected here:

- **Transport.** The ticket says receipts go "via Reticulum." Reticulum was torn out (ZEB-474/475). The DM path is now the iroh PQ tunnel (`IrohTunnelDmTransport`, live/best-effort) layered over a deposit durability rung (`DepositOnlyDmTransport` fallback; butler / community-relay deposit). Receipts ride the **live tunnel only** (see §4).
- **Emission hook.** The ticket says "current/future `mark_read` calls also emit a receipt." There is no such call: the CRDT `ReadMarker` has **no local write path** (only remote-sync merges populate it). The read signal the DM UI actually uses is a frontend localStorage cursor (`dm-unread-service.ts` → `markThreadRead`, fired from `App.svelte:3617` when a DM is opened). The receipt is therefore a **new outbound fact**, not a re-use of `ReadMarker`, and this design does **not** wire up the dormant CRDT marker (left untouched — separate concern).

## 2. Semantics (the product shape)

- **Watermark, not per-message.** A receipt says "I have read this DM up to logical time `read_up_to`." One receipt covers every message at or before that time. `read_up_to` is an **HLC** so it orders correctly across devices despite wall-clock skew.
- **Ephemeral / live-only.** A receipt is a signed control frame pushed over the live iroh tunnel. It is **never** written to an outbox, **never** deposited, and leaves nothing at rest that records "A read B." If the peer is not live, the receipt is simply not sent (with a reconnect re-send to mitigate — §4).
- **Off by default, per-DM.** New per-`Space` preference `read_receipt_pref`, default `Off`.
- **Send-only.** The toggle controls only whether *you emit* receipts. You always display whatever receipts a peer sends you. No reciprocity coupling.
- **1:1 DMs only** this cut. The preference field is defined for group DMs too but is inert there (no emission, no render) until a fast-follow.

## 3. Data model & preference

New field on `Space` (`owner_state_types.rs`, adjacent to `notification_pref` / `custom_name`):

```rust
/// Per-DM read-receipt preference (owner-local; NOT propagated to other
/// members — like notification_pref). None ≡ Off.
#[serde(rename = "rr", skip_serializing_if = "Option::is_none", default)]
pub read_receipt_pref: Option<ReadReceiptPref>,

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadReceiptPref {
    #[serde(rename = "off")] Off,
    #[serde(rename = "b")]   Broadcast,
}
```

- **Back-compat.** Additive `Option` with `skip_serializing_if`/`default` keeps pre-existing owner-state wire bytes byte-identical (the `shared_in_profile` field is the documented template). `Space` is already a registered `CanonicalPayload` — no new registration.
- **Merge.** Carried in the existing `lww_merge_space` clone block (`owner_state_crdt.rs`), same as `custom_name` / `notification_pref` — the winner is the side with the strictly-newer `updated_at`. Owner-local; syncs across the owner's own devices via owner-state Flow A; never sent to the peer.
- **Write command.** `set_space_read_receipt_pref(space_id: String, pref: ReadReceiptPref)` in `lib.rs`, modeled exactly on `set_space_shared_in_profile` (snapshot Arcs under the `NodeState` mutex → no-op probe → `reserve_next_hlc_for_device` → re-lock, set field, bump `space.updated_at = new_hlc` → generation post-check → `publisher.notify_dirty()`). Rejects any space whose `kind` is not `Dm`/`GroupDm`.
- **Read-back.** The current pref must be visible to the frontend so the toggle reflects synced state. Surface it on whatever space/DM listing the DM header already consumes (a getter or an added field on the existing DM-list DTO — pinned in planning).

## 4. Wire format & crypto

- **Envelope.** New `DmPacket::ReadReceipt` variant, discriminant **`0x06`** (next free after `0x05` RevocationPush), in `dm_envelope.rs`. Wire layout is the uniform `[u8 discriminant][CBOR(signed_body)][64-byte Ed25519 sig]`; discriminant is routing-only and excluded from the signed bytes. New arms in `encode_packet` / `decode_packet`, a `build_signed_read_receipt` helper mirroring `build_signed_cidnotify`, and a receive arm in `ingest_dm_packet`. The external `harmony-tunnel-iroh` crate carries the packet as opaque bytes — **no external-crate change**.
- **Signed body.**

```rust
pub struct DmReadReceiptSigned {
    pub space_id: SpaceId,
    pub sender_owner_addr: OwnerAddr,   // the reader; bound-checked on ingest
    pub signing_device_hash: [u8; 32],  // device-key binding (inside signed bytes)
    pub read_up_to: Hlc,                // watermark: reader has read all msgs <= this
    pub sent_at: Hlc,                   // receipt's own timestamp (freshness / dedupe)
}
```

- **Sign / verify.** Signed with the **same per-device Ed25519 key** used for other DM packets (`dm_signing::sign_dm_packet`), and verified on ingest through the **same admission chain** as `CidNotify`: `verify_dm_packet_signature` (identity_pub hashes to the claimed `signing_device_hash`, then Ed25519 verify) → resolve owner from device cache and enforce `sender_owner_addr == resolved_owner` → space exists, `kind ∈ {Dm, GroupDm}`, resolved sender ∈ `space.members` → shared-community revocation cutoff (drop if the signer's device is revoked). No CAS/blob steps (a receipt carries no message body).

## 5. Flow (emit → reconnect → ingest → render)

**Emit.** A new backend command `mark_dm_read(space_id: String)`:
- Called from the frontend where it already marks a DM read (`App.svelte:3617`, alongside `markThreadRead`), and again when a message arrives while that DM is focused (so a live back-and-forth updates "Seen" promptly).
- Backend reads `read_receipt_pref`. If not `Broadcast`, returns (no-op). If `Broadcast`:
  - Computes the **authoritative** watermark `read_up_to = max(sent_at)` over messages in the space from the inbox CRDT (assumes the user read to the bottom — true when the DM is open/focused).
  - Builds + signs a `DmReadReceiptSigned`, encodes the `0x06` packet, and pushes it to the peer's **live** tunnel sessions via the RevocationPush-style direct push (`send_packet_to_owner_tunnels`) — **no `OutboxEntry`, no deposit**.
  - If the peer has no live session, nothing is sent (reconnect re-send covers it).

**Reconnect re-send.** When a live tunnel session to a peer is (re)established, re-send the current watermark for any opted-in 1:1 DM with that peer. This is what makes ephemeral receipts land in normal async use. Seam candidate: the iroh tunnel acceptor's session-established path (`iroh_tunnel_acceptor.rs`) / `TunnelManager`; the exact hook is pinned in planning. Bounded work: iterate opted-in `Broadcast` 1:1 DMs whose peer == the newly-live owner.

**Ingest.** The `0x06` arm in `ingest_dm_packet` verifies the frame (§4) and, on success, emits a Tauri `dm-read-receipt` event `{ spaceId, from, readUpTo, at }` built by a shared payload helper (mirroring `dm_received_event_payload`). Because receipts are live-only, only the tunnel ingest path emits — no deposit/sweeper duplication. Verify-fail → `warn!` + drop (like `dm-received`); no state written.

**Render.** The frontend keeps a per-space `peerReadUpTo` watermark (updated by the `dm-read-receipt` listener). `TextMessage.svelte` shows a subtle "Seen HH:MM" under the newest of *your own* sent messages whose `sent_at <= peerReadUpTo`, gated on `isSelf` and 1:1. HLC comparison uses the peer's locally-known own-sent HLCs (the frontend already receives `sentAt`; if the ordering value isn't on the DTO it is added — pinned in planning).

## 6. UI

- **Toggle.** A "Send read receipts" control in the `TextFeed.svelte` DM header (which already has the DM's `spaceId`), reflecting the synced `read_receipt_pref` and invoking `set_space_read_receipt_pref`. 1:1 DM headers only this cut.
- **Indicator.** The "Seen HH:MM" line per §5 — reuses the compact timestamp formatting already used by `CallEventLine.svelte` / `TextMessage.svelte`.

## 7. Error handling & privacy invariants

Asserted by tests:
- **Never emitted when pref ≠ Broadcast.**
- **Never creates an `OutboxEntry` and is never deposited** (the ephemerality invariant).
- **Group DMs are inert** this cut (field settable; no emission, no render).
- Verify-fail on ingest (bad sig / device-hash mismatch / owner mismatch / non-member / revoked device) → `warn!` + drop, no state change.
- A receipt reveals only read-time, to the one peer, end-to-end and transient.

## 8. Testing

**Rust:**
- `set_space_read_receipt_pref` sets the field, bumps `updated_at` HLC, and is gated to `Dm`/`GroupDm` (rejects a `Channel`/`Community` space).
- `read_receipt_pref` survives owner-state persist/reload; pre-field wire bytes decode unchanged (back-compat).
- `0x06` encode/decode round-trip; signature verifies; a tampered body/sig is rejected; unknown/mismatched device hash rejected; sender-owner mismatch rejected; non-member rejected.
- `mark_dm_read` computes `read_up_to = max(sent_at)`; emits a receipt **only** when pref is `Broadcast`; creates **no** `OutboxEntry` (ephemerality).
- Reconnect re-send fires for an opted-in peer becoming live and not for an `Off` DM.
- Ingest arm emits `dm-read-receipt` on a valid frame and drops on each failure mode.

**Frontend (vitest):**
- The `dm-read-receipt` listener updates the per-space watermark.
- "Seen HH:MM" renders under the correct own-message, only for 1:1, only when a receipt exists; not shown otherwise.
- The header toggle invokes `set_space_read_receipt_pref` and reflects the synced pref.

## 9. Scope

**In:** 1:1 DM read receipts; ephemeral live-only delivery + reconnect re-send; send-only per-DM toggle; preference synced across the owner's own devices.

**Out (explicit):**
- Group-DM emission/render (field defined but inert).
- Durable / deposit-backed receipts.
- Reciprocity coupling (display never gated on your own send-pref).
- Typing indicators; delivered-vs-read distinction (delivery already tracked via outbox `delivered_to`).
- Wiring the dormant CRDT `ReadMarker` local write path.

## 10. File structure

**New:**
- `src-tauri/src/dm_read_receipt.rs` — emit logic (pref check, watermark computation, build+sign+push), reconnect-resend helper, shared ingest-emit payload builder. Isolated and unit-testable.
- `src/lib/read-receipt-service.ts` — frontend `dm-read-receipt` listener + per-space watermark store.

**Modified:**
- `src-tauri/src/dm_envelope.rs` — `DmReadReceiptSigned`, `ReadReceipt` (`0x06`), encode/decode, `build_signed_read_receipt`.
- `src-tauri/src/dm_inbox_ingest.rs` — `0x06` ingest arm → emit event.
- `src-tauri/src/owner_state_types.rs` — `read_receipt_pref` field + `ReadReceiptPref` enum.
- `src-tauri/src/owner_state_crdt.rs` — merge in `lww_merge_space`.
- `src-tauri/src/lib.rs` — `set_space_read_receipt_pref`, `mark_dm_read`, `dm-read-receipt` event const, reconnect wiring, pref read-back on the DM listing.
- `src/lib/components/TextFeed.svelte` — header toggle wiring.
- `src/lib/components/TextMessage.svelte` — "Seen HH:MM" indicator.
- `src/lib/types.ts` — watermark/seen typing on the DM message shape as needed.

## 11. Global constraints

- Frontend CI gates (repo root): `npx tsc --noEmit` + `npx vitest run`.
- Rust CI gates (from `src-tauri/`): `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- Tauri IPC: Rust params `snake_case`, JS callers `camelCase`.
- Tauri IPC error extraction: `e instanceof Error ? e.message : String(e)`.
- Owner-state additive fields must preserve byte-compat (skip_serializing_if + default); any local `crdt_state` mutation must `notify_dirty()` to persist/replicate.
- Never call deterministic-nonce crypto helpers outside `#[cfg(any(test, feature = "test-fixtures"))]`.
- The DM wire envelope and the frozen owner-state format are extended additively only; existing `DmPacket` discriminants and owner-state field tags are untouched.
