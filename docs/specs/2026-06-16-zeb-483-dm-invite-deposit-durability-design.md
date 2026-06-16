# ZEB-483 — DmInvite deposit durability (sealed-payload piggyback)

**Ticket:** ZEB-483 (Move 1b durability; parent ZEB-321 transport coalescence)
**Branch:** `zeb-483-dm-invite-deposit-durability` off `main` `ae47634d`
**Date:** 2026-06-16
**Repo:** harmony-client (single-repo; no harmony-core PR)
**Follows:** ZEB-482 (Move 1b tunnel invite carrier), ZEB-473 (Move 1a always-deposit + attempt-tunnel), ZEB-484 (Move 1c blob carrier)

---

## Context — the gap

ZEB-482 re-pointed the DM-Space `DmInvite` onto the iroh PQ tunnel: two friend-owners share a DM `Space` (random per-owner `SpaceId` + per-`Space` `content_key`), the invite rides the live tunnel right before the first `CidNotify`, and co-located 1:1 DM delivery works (`s2_dm_delivery_over_tunnel_hard_assert` green).

That invite carrier is **tunnel-only / best-effort liveness**. By contrast, `CidNotify` packets follow the **always-deposit + attempt-tunnel** durability pattern (ZEB-473): even with the tunnel down, the notify is deposited (butler fleet rung and/or community-relay rung) and recovered when the peer comes online.

The `DmInvite` is **not** deposited — the deposit rung carries only `CidNotify`. The send path is explicit about this at `lib.rs:10673-10676`: *"On a deposit-only node (`tunnel_manager: None`) this is skipped entirely … the invite's offline/cross-WAN durability rung is ZEB-483."*

**Consequence:** if the recipient is offline when the sender creates the DM Space (tunnel never establishes), the bootstrap invite is lost. A later *deposited* `CidNotify` the peer *does* recover still rejects at `verify_cidnotify_admission` (`dm_outbox.rs:2675-2679`) with `SpaceNotFound`, because the `Space` was never delivered. The always-deposit durability story is incomplete for brand-new cross-owner DM Spaces.

## Goal

Give the `DmInvite` the same always-deposit + attempt-tunnel durability as the `CidNotify`, so an offline-at-create recipient can bootstrap the DM `Space` from the deposit rung when the tunnel was unavailable at Space-creation time.

---

## Design decision (settled with Jake, 2026-06-16)

Two axes were decided:

1. **Carrier shape: piggyback the signed invite inside the sealed `DepositPayload`** (NOT a separate deposit item). Rationale below.
2. **Test scope: deterministic Rust integration tests in this PR; defer the live offline→recover e2e to the AVALON cross-WAN session** (the full DoD's live leg needs AVALON regardless, same as the rest of the epic).

### Why piggyback, not a separate invite-deposit item

Deposits are per-packet, content-addressed items keyed by `(space_id, message_cid)`. The butler acceptor `iroh_butler_acceptor::handle_deposit_core` (`:602-638`) **hard-requires** the deposited packet to be a `DmPacket::CidNotify` *and* binds its `storage_blob` CID to `signed.message_cid` (`_ => Err(BadPayload)` otherwise). A separate invite item (no `message_cid`, no blob) would force **relaxing that validation** — the one check guaranteeing an untrusted butler only ever stores well-formed, blob-bound CidNotifys — plus a new deposit-store key form and recover-side dispatch.

One level up, the **sealed** `DepositPayload { cidnotify_packet, storage_blob }` (`butler_deposit.rs:144`) is already a multi-field bundle, end-to-end sealed to the recipient and **opaque to the butler**. The invite rides *inside* that sealed bundle:

- The butler's CidNotify + blob-CID validation stays **exactly as-is** (the invite is extra sealed bytes it never inspects).
- Recovery is **atomic**: the Space bootstraps in the same item that admits the notify — no invite-before-notify ordering concern (recovery is otherwise unordered / retry-until-verifies).
- The community-relay rung needs **zero change**: it holds the still-sealed blob (`RelayHoldEntry` keyed on `ContentId(sealed_blob)`), so the invite rides opaquely and the recipient unseals it natively.

**Cost (accepted):** invite durability is coupled to message deposits (no message ⇒ no invite deposit — a non-scenario for DMs), and the sender rebuilds+signs the invite at deposit time (symmetric with how it already rebuilds the CidNotify via `build_cidnotify_packet_bytes`).

---

## Architecture

```
SEND (sender, tunnel down → deposit rung fires)
  dm_outbox::push_deposit_candidate (dm_outbox.rs:1379)
    ├─ build_cidnotify_packet_bytes(entry)         [existing]
    └─ build_invite_packet_bytes(state, entry)     [NEW] — rebuild+sign DmInvite from Space record
        → ButlerDepositRequest { …, invite_packet: Option<Vec<u8>> }   [NEW field]

  Butler rung:  IrohButlerDepositClient::deposit (butler_deposit.rs:491)
    → builds DepositPayload { cidnotify_packet, storage_blob, invite_packet }  [NEW field]
       sealed to butler device key
  Relay rung:   ProdCommunityRelayDepositClient::deposit (community_relay_prod.rs:714)
    → seals the SAME DepositPayload (invite rides inside, opaque to relay)

BUTLER ACCEPTOR (recipient's always-on device)
  iroh_butler_acceptor::handle_deposit_core (:480, :602-638)
    ├─ validate cidnotify_packet == CidNotify + blob CID == message_cid   [UNCHANGED]
    ├─ size-bound payload.invite_packet                                    [NEW]
    └─ persist DmInboxEntry { …, invite_packet }                          [NEW field]

RECOVER (recipient comes online)
  Butler/fleet: ingest_pending (dm_inbox_ingest.rs:129) → DmInboxIngestCtx::verify (:97)
     if entry.invite_packet.is_some(): apply_invite(inviter == entry.sender_owner)   [NEW]
     then verify_cidnotify_admission  → now admits (Space bootstrapped)
  Relay:       ingest_recovered(payload) (community_relay_prod.rs:386)
     if payload.invite_packet.is_some(): apply_invite(inviter == cidnotify.sender_owner_addr) [NEW]
     then the same CidNotify ingest
```

---

## Detailed design

### 1. Struct / wire changes (all new fields `#[serde(default)]`, backward-compatible)

All three are canonical-CBOR structs; a new trailing `Option<Vec<u8>>` with `#[serde(default)]` decodes old wire/persisted bytes as `None`, so old senders, old persisted `DmInboxDoc`/`RelayHoldDoc` entries, and mixed-version fleets all keep working with no migration.

- **`DepositPayload`** (`butler_deposit.rs:144`): add `invite_packet: Option<Vec<u8>>` — the signed `DmPacket::Invite` wire bytes (or `None`). Sealed end-to-end; the butler never inspects it. `encode_deposit_payload`/`decode_deposit_payload` (`:214`/`:219`) get the field for free via serde; `decode` already rejects trailing bytes, which is preserved.
- **`DmInboxEntry`** (`dm_inbox_crdt.rs:14`): add `invite_packet: Option<Vec<u8>>`. The butler persists it from the unsealed `DepositPayload`. `DmInboxDoc::merge_from` (`:57`) stays insert-once on key `(space_id:message_cid)`; the grow-only `ingested_by` union is unchanged.
- **`ButlerDepositRequest`** (`butler_deposit.rs:296`): add `invite_packet: Option<Vec<u8>>` — the in-process hand-off from the outbox to whichever deposit client(s) fire. Both clients copy it into the `DepositPayload` they seal.

### 2. Send side — `build_invite_packet_bytes` + always-attach (`dm_outbox.rs`)

`push_deposit_candidate` (`:1379`) already receives `state: &OwnerState`, and `DmOutbox` already holds the signing material (`signing_key` `:475`, `private_identity` `:489`, `enrollment_cert` `:497`). So the invite can be rebuilt+signed at deposit time exactly as `build_cidnotify_packet_bytes` (`:1361`) rebuilds the CidNotify — no need to stash the invite at Space-create time.

Add a sibling helper:

```rust
/// Rebuild the signed DmInvite wire bytes for a DM Space deposit — the
/// IDENTICAL DmInviteSigned that add_space_dm_inner built for the tunnel
/// carrier (lib.rs:10410-10424), reconstructed from the persisted Space
/// record so a deposited copy bootstraps the Space byte-for-byte like a
/// tunnel arrival. Returns None for non-DM Spaces or if the Space record
/// is missing (skip the invite; the CidNotify still deposits).
fn build_invite_packet_bytes(&self, state: &OwnerState, entry: &OutboxEntry) -> Option<Vec<u8>>
```

It reads the Space record (`state.spaces.get(&entry.space_id)`) for the fields the invite carries (space kind, member set, `content_key`, inviter = `self.self_owner`), constructs the `DmInviteSigned`, and signs via `dm_envelope::build_signed_invite` + `encode_packet` (mirroring `build_dm_packet` `:336`). An implementation-verification step in the plan asserts byte-parity (or admission-equivalence) with the `add_space_dm_inner` invite so a deposited invite and a tunnel invite bootstrap the same Space.

`push_deposit_candidate` sets `ButlerDepositRequest.invite_packet = self.build_invite_packet_bytes(state, entry)`.

**Gate: always attach for DM Spaces.** The deposit rung only fires when the live tunnel is down (the offline-fallback exception, not steady state — steady-state DMs go over the tunnel and never deposit). The invite is small (member set + `content_key` + 64-byte sig ≈ a few hundred bytes), and re-application on recover is idempotent (LWW on `space_id`, §6). So an ack-gated "omit the invite once the recipient has bootstrapped" optimization is not worth new per-(space, recipient) ack state (YAGNI). A redundant invite on a re-deposit after the Space already bootstrapped is a no-op apply.

### 3. Butler rung — acceptor pass-through + size bound (`iroh_butler_acceptor.rs`)

`handle_deposit_core` (`:480`) unseals to a `DepositPayload`, then at `:602-638` validates `cidnotify_packet` is a `CidNotify` and binds `storage_blob` CID to `signed.message_cid`. **This validation is unchanged.** Two additions:

- **Size bound:** reject the deposit (`BadPayload`-class) when `payload.invite_packet` exceeds `MAX_DEPOSIT_INVITE_BYTES` (set to **4096** — a generous ceiling for a DM invite, which is a few hundred bytes; prevents a malicious sender inflating deposit/hold storage).
- **Pass-through:** carry `payload.invite_packet` into the persisted `DmInboxEntry.invite_packet`. The butler does **not** validate the invite — that is the recipient's job on recover (§5).

### 4. Relay rung — zero acceptor change

`ProdCommunityRelayDepositClient::deposit` (`community_relay_prod.rs:714`) seals the **same** `DepositPayload` (now carrying `invite_packet`). The relay holds the still-sealed blob (`RelayHoldEntry` keyed on `ContentId(sealed_blob)`); it never unseals, so there is **no relay-acceptor change**. The invite reaches the recipient inside the sealed payload and is read on recover (§5).

### 5. Recover side — apply invite before notify, bind inviter (two entry points)

The invariant at both entry points: **if an invite_packet is present, apply it (bootstrapping the Space) before attempting CidNotify admission, and bind the invite's `inviter` to the deposit's verified sender; on mismatch or invalid signature, reject the invite and leave the CidNotify pending (fail-closed).**

- **Butler/fleet:** `ingest_pending` (`dm_inbox_ingest.rs:129`) → `DmInboxIngestCtx::verify` (`:97`) / `apply_inbox` (`:105`). Before the existing `verify_cidnotify_admission`, if `entry.invite_packet.is_some()`, decode it and call `apply_invite(expected_inviter = Some(entry.sender_owner))` (`dm_outbox.rs:1971`). `entry.sender_owner` is the butler-verified deposit sender (from the `DepositFrame` enrollment cert), so this is a strong inviter binding — at least as strong as the tunnel path's `resolve_owner_for_peer` → `InviterMismatch` check (ZEB-482 F1).
- **Relay:** `ingest_recovered(payload)` (`community_relay_prod.rs:386`) already holds the full unsealed `DepositPayload`. Before the CidNotify ingest, if `payload.invite_packet.is_some()`, call `apply_invite(expected_inviter = Some(cidnotify.sender_owner_addr))` — both the invite and the CidNotify are signed by the same sender and ride the same sealed payload, so binding the invite's inviter to the CidNotify's signed `sender_owner_addr` ensures the message sender also issued the invite.

`apply_invite` is idempotent on `space_id` (LWW via `apply_space_with_canonicalization`, `owner_state_crdt.rs:479`), so a deposited invite + a tunnel invite for the same Space merge rather than double-create, and a redundant invite on a later recover is a no-op.

### 6. Idempotency / dedup (no double-apply, single UI event)

Unchanged and relied upon:

- **Invite:** dedup on `space_id` via `apply_space_with_canonicalization` (CRDT LWW). Deposited + tunnel invite → merge.
- **Message:** `OwnerState::apply_inbox` keyed `(space_id, message_cid)` (`owner_state_crdt.rs:412`); first arrival `Inserted` (emits `dm-received`), second `Merged` (no re-emit). Tunnel copy and deposit copy of the same DM deliver **one** UI event (`dm_outbox.rs:1778`, `dm_inbox_ingest.rs:188`).
- **Deposit store:** `DmInboxDoc::merge_from` insert-once on `(space_id:message_cid)`; `RelayHoldDoc` keyed on `(recipient_owner, ContentId(sealed_blob))`. Redeposits absorbed; per-device `ingested_by` guard skips already-consumed entries.

---

## Error handling (fail-closed)

| Condition | Behavior |
|---|---|
| `payload.invite_packet` > `MAX_DEPOSIT_INVITE_BYTES` | Butler acceptor rejects the deposit (`BadPayload`-class), same path as an oversized/invalid payload today. |
| Invite signature invalid on recover | `apply_invite` returns `Err`; log + leave the CidNotify entry **pending** (retries each sweep, 30-day TTL). No Space, no delivery. |
| `invite.inviter != verified sender` on recover | Invite rejected (`InviterMismatch`-class); entry stays pending. No spurious Space. |
| Old deposit with `invite_packet = None` | Exactly today's behavior — CidNotify pending with `SpaceNotFound` until an invite arrives via tunnel or a newer deposit. No regression. |
| Non-DM Space / missing Space record on send | `build_invite_packet_bytes` returns `None`; the CidNotify still deposits as today. |

## Security analysis

- **Untrusted-relay invariant preserved.** The butler's CidNotify + blob-CID validation is unchanged; the invite is opaque sealed bytes it stores but never trusts. The relay never unseals at all. No widening of what an untrusted relay is asked to vouch for.
- **Invite authenticity is recipient-verified.** On recover the invite's own Ed25519 signature is checked (via `decode_packet`/`apply_invite`) and its `inviter` is bound to the deposit's verified sender — strictly ≥ the tunnel path's binding.
- **DoS bound.** `MAX_DEPOSIT_INVITE_BYTES` caps the extra storage a sender can push into a butler/relay per deposit.
- **Confidentiality.** The invite (carrying the Space `content_key`) rides only inside the end-to-end sealed `DepositPayload`; neither butler nor relay sees it in cleartext.

---

## Testing

**In-tree (deterministic Rust integration/unit tests, this PR):**

1. **Serde backward-compat:** `DepositPayload` and `DmInboxEntry` round-trip with `invite_packet: Some(_)` and `None`; an old-format CBOR (no `invite_packet` field) decodes to `None`.
2. **Send side:** depositing a CidNotify for a DM Space sets `ButlerDepositRequest.invite_packet` to a valid signed invite (`build_invite_packet_bytes` produces an admission-equivalent invite to `add_space_dm_inner`); a non-DM Space / missing record yields `None`.
3. **Butler acceptor:** a deposit carrying an invite passes the unchanged CidNotify+blob validation and persists `DmInboxEntry.invite_packet`; an oversized invite (> `MAX_DEPOSIT_INVITE_BYTES`) is rejected.
4. **Recover (butler):** an entry carrying invite + CidNotify for a Space **not** yet bootstrapped → invite applied first (Space created), CidNotify then admits (no `SpaceNotFound`), single `dm-received`. Negative: `invite.inviter != entry.sender_owner` → rejected, no Space, CidNotify stays pending.
5. **Recover (relay):** `ingest_recovered` with `payload.invite_packet` bootstraps the Space then ingests the CidNotify; inviter-mismatch rejected.
6. **Idempotency:** deposited invite after the Space already exists is a no-op; deposit + tunnel copies of the same DM emit one `dm-received`.

**Deferred to AVALON (cross-WAN, the DoD's live leg):** the full offline-at-create → relaunch → recover-invite+CidNotify → `dm-received` scenario across two machines. Standing up a co-located deposit rung in the two-node harness (a butler sibling or co-located relay, cloning the S3 SIGKILL-offline scaffold) is explicitly out of scope for this PR.

## Definition of done

- DmInvite is deposited (butler and/or relay) alongside a DM Space's CidNotify deposit(s), riding inside the sealed `DepositPayload` (always-attach for DM Spaces).
- A recovered deposit bootstraps the Space (invite applied before notify) so the CidNotify admits instead of `SpaceNotFound`.
- All in-tree tests above pass; `cargo fmt`/`clippy`/`nextest` green.
- Backward-compatible (old deposits/persisted docs decode cleanly; no migration).
- Live cross-WAN offline→recover proof tracked for the AVALON session (ZEB-444 / ZEB-447), not this PR.

## Out of scope

- Relaxing the butler acceptor's CidNotify-only contract / a separate invite-deposit item (rejected design A).
- An ack-gated "omit invite once bootstrapped" optimization (YAGNI; always-attach).
- Co-located harness deposit rung + the live two-machine e2e (AVALON).
- ZEB-484 content-blob delivery (separate, merged) and ZEB-461 reachability (merged).

## File-touch map

| File | Change |
|---|---|
| `src-tauri/src/butler_deposit.rs` | `DepositPayload` + `ButlerDepositRequest` gain `invite_packet`; `MAX_DEPOSIT_INVITE_BYTES`; both deposit clients copy request→payload. |
| `src-tauri/src/dm_inbox_crdt.rs` | `DmInboxEntry` gains `invite_packet` (`#[serde(default)]`). |
| `src-tauri/src/dm_outbox.rs` | `build_invite_packet_bytes`; `push_deposit_candidate` attaches it; recover-path inviter binding helper. |
| `src-tauri/src/iroh_butler_acceptor.rs` | `handle_deposit_core` size-bounds + passes invite through to `DmInboxEntry`. |
| `src-tauri/src/dm_inbox_ingest.rs` | `ingest_pending`/`DmInboxIngestCtx::verify` apply invite-before-notify (butler rung). |
| `src-tauri/src/community_relay_prod.rs` | `ingest_recovered` applies invite-before-notify (relay rung). |
| `src-tauri/src/dm_envelope.rs` | (reuse only) `build_signed_invite` / `encode_packet` for the rebuild. |
