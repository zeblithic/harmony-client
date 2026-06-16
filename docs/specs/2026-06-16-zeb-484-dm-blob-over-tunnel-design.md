# ZEB-484 (Move 1c): tunnel-inline DM content-blob carrier — design

**Status:** approved 2026-06-16 (Koya)
**Parent:** ZEB-321 (transport coalescence) · **Follows:** ZEB-473 (Move 1a tunnel carrier), ZEB-482 (Move 1b DmInvite carrier)
**Branch:** `zeb-484-dm-blob-over-tunnel` off `cb661691`

## 1. Problem

Cross-owner 1:1 DM delivery needs three pieces to reach the recipient: the DM `Space`
(ZEB-482 ✓), the `CidNotify` reference = `space_id` + `message_cid` (ZEB-473 ✓), and the
encrypted message **blob** itself. After ZEB-482 the recipient bootstraps the `Space` and
tunnel-DM admission succeeds, but the e2e `s2_dm_delivery_over_tunnel_hard_assert` is still
red: it advanced from `verify_cidnotify_admission: SpaceNotFound` →
`CAS fetch: no successful reply`. The recipient has admitted the DM but cannot obtain its
content.

## 2. Investigation verdict (the "investigate FIRST" open question)

**The durability blob carrier already exists and is fully wired for 1:1 DMs — but only fires
when the recipient advertises a butler.**

- The butler deposit (ZEB-418) carries the actual ciphertext, not a reference:
  `DepositPayload { cidnotify_packet, storage_blob }` — `butler_deposit.rs:144-151`
  (`storage_blob` = the CAS storage blob `[ver][nonce][ct][tag]`).
- On receive, the inbox sweeper CAS-puts the deposited blob **before** ingest, so blob
  delivery never depends on the content-serve queryable:
  `ctx.cas_put(&entry.storage_blob)` — `dm_inbox_ingest.rs:160` ("CAS-put FIRST so the
  message blob is fetchable exactly like a direct arrival").
- The deposit rung **silently skips when no fresh butler set is advertised**:
  `DepositRungOutcome::SkippedNoFreshButlerSet` — `butler_deposit.rs:497-499`.
- The live PQ tunnel carries **only** the `CidNotify` reference; there is no blob-carrying
  `DmPacket` variant (`DmPacket` = `Invite | CidNotify | Ack`, `dm_envelope.rs:182-199`).
- The content-serve queryable refuses encrypted CIDs by design (privacy):
  `content_cid_servable(cid, …) = !cid.flags().encrypted || allowlist.contains(cid)` —
  `event_loop.rs:7437-7442`; "private encrypted content stays unservable".

**Conclusion.** The *durability* path (butler deposit) is done. The genuine gap is the
**liveness** path: there is no live, peer-to-peer blob carrier, so **two online peers with
no butler cannot exchange DM content** — exactly the S2 scenario. This is the missing
half of the hybrid the ticket predicted ("tunnel-inline (liveness) + sealed deposit
(durability)"); the deposit half already exists, so ZEB-484 is purely the liveness half.

## 3. Decision

Add a **tunnel-inline DM blob carrier**: the sender's existing best-effort "attempt-tunnel"
rung additionally carries the encrypted `storage_blob` alongside the `CidNotify`; the
receiver CAS-puts the inline blob and then runs the **existing** `CidNotify` ingest, which
now finds the blob in local CAS instead of round-tripping to the refusing content-serve
queryable. The butler deposit rung is **unchanged** and still always fires for
durability/offline. This mirrors ZEB-473's proven "always-deposit + attempt-tunnel" split,
now extended from the reference to the blob.

Rejected alternatives:
- **Harness-only (butler is the model):** add a butler node to the S2 topology to exercise
  the existing deposit path. Minimal code, but leaves two online friends (even same-LAN)
  unable to DM content without an always-on butler, and makes the "over_tunnel" test
  actually prove "via butler".
- **Authorized content-serve:** gate the content queryable to serve encrypted CIDs to
  authenticated `Space` members. Heaviest and most privacy-sensitive — the queryable has no
  per-query auth today, and a mistake leaks DM ciphertext to non-recipients.

## 4. Architecture & data flow

```
SENDER (online)                               RECIPIENT (online, no butler)
  send_dm → encrypt MessagePayload
          → storage_blob in local CAS
          → message_cid = ContentId::for_book(storage_blob, encrypted)
  outbox drain:
    ├─ attempt-tunnel rung (IrohTunnelDmTransport)         tunnel ingest (ingest_dm_packet)
    │    read storage_blob from CAS by message_cid   ──▶     decode DmPacket::CidNotifyWithBlob
    │    if packet fits frame budget:                        cas_put(storage_blob)  ◀── blob local
    │      send DmPacket::CidNotifyWithBlob                  run existing CidNotify ingest:
    │    else: send bare DmPacket::CidNotify (today)           Phase 2 admission (Space ✓)
    │    returns Transient (so deposit still fires)            Phase 3 content_store.get(cid) ── HIT cache
    │                                                          Phase 3b blob↔cid binding ✓
    └─ butler deposit rung — UNCHANGED                        Phase 4 decrypt/apply
         skips if no butler; else deposits blob (durability)  Phase 6 emit dm-received ✓
```

The two rungs are independent and idempotent on `(space_id, message_cid)`: if both a live
tunnel copy and a deposited copy arrive, the second dedups harmlessly.

## 5. Wire shape

New variant `DmPacket::CidNotifyWithBlob` (discriminant `0x04`):

```rust
DmPacket::CidNotifyWithBlob {
    signed: DmCidNotifySigned,   // same signed body as CidNotify (space_id + message_cid + sender)
    signature: [u8; 64],         // Ed25519 over signed_bytes — authenticates the CidNotify
    signed_bytes: Vec<u8>,       // canonical CBOR of `signed`
    storage_blob: Vec<u8>,       // the encrypted CAS storage blob [ver][nonce][ct][tag]
}
```

- The blob needs **no separate signature**. It is **content-addressed**: on receive,
  `cas_put(storage_blob)` stores it under `ContentId::for_book(storage_blob, encrypted)`. A
  tampered or substituted blob lands under a *different* key, the subsequent
  `get(message_cid)` misses, and delivery **fails closed** — never substitutes content. The
  existing Phase-3b blob↔packet binding check is belt-and-suspenders on top.
- No new sealing. The PQ tunnel session (ML-KEM-768 + ML-DSA-65 + ChaCha20-Poly1305) already
  provides in-transit confidentiality, and `storage_blob` is ciphertext at rest. The
  integrity/authenticity guarantees are identical to the CAS-fetch path.
- **Wire layout.** Unlike the existing `[disc][signed_bytes][64-byte sig]` packets (which
  recover the body as "everything but the trailing 64 bytes"), this variant has two
  variable-length fields, so it is explicitly length-delimited:
  `[0x04][u32 BE len(signed_bytes)][signed_bytes][64-byte sig][storage_blob]`. `decode_packet`
  reads the length prefix to split `signed_bytes` | `sig` | `storage_blob`. The existing
  variants' layout is untouched.

## 6. Send path (`iroh_tunnel_dm_transport.rs`)

The attempt-tunnel rung today builds + sends the bare `CidNotify` and returns `Transient`
(so the butler deposit rung always fires for durability). Change:

1. Build the signed `CidNotify` as today.
2. Read `storage_blob` from local CAS by `message_cid`.
3. If the **assembled `CidNotifyWithBlob` packet** is within the inline budget
   (`INLINE_BLOB_MAX`, see §8) and the blob is present, send `CidNotifyWithBlob`.
   Otherwise send the bare `CidNotify` (today's behavior).
4. Still return `Transient` — the butler deposit rung is unchanged and still fires.

A single CAS *book* maxes at 1 MiB by construction (larger objects are split into
Merkle/B-trees of ≤1 MiB pieces), so a DM message blob is one book and the inline path
covers it in practice; the budget is a frame-safety ceiling, not an expected-common reject.

## 7. Receive path (`dm_inbox_ingest.rs`)

`ingest_dm_packet` dispatches on the `DmPacket` variant. Add the `CidNotifyWithBlob` arm:

1. `cas_put(storage_blob)` first (mirrors the butler sweeper at `dm_inbox_ingest.rs:160`).
2. Delegate to the **existing** `CidNotify` handling using the inner `signed` / `signature`
   / `signed_bytes` — Phase-3's `content_store.get(message_cid)` now hits local CAS (no
   zenoh round-trip), Phase-3b binding still verifies content-addressing, Phase-4 decrypts
   and applies, Phase-6 emits `dm-received`.
3. If `cas_put` fails, fall through to the existing `CidNotify` path (which then attempts the
   CAS-fetch and errors as today) — the inline blob is strictly best-effort.

No change to admission, decrypt, binding, or emit logic — they are reused verbatim.

## 8. Edge cases & error handling

- **Oversize blob** (assembled packet > `INLINE_BLOB_MAX`): send bare `CidNotify`; rely on
  butler/CAS. No user-visible error — graceful degradation to the durability path.
  `INLINE_BLOB_MAX` is a named constant on the **assembled packet size**, comfortably below
  the tunnel frame cap (`DATA_MAX_MESSAGE = 2 MiB`, `tunnel_task.rs:57`). **Proposed value:
  1.5 MiB (`1_572_864` bytes)** — a full 1 MiB storage book plus envelope, the `CidNotify`,
  and framing fit with ~0.5 MiB of headroom under the 2 MiB frame, and a single book never
  exceeds it. (The guard is a frame-safety ceiling, not an expected-common reject.)
- **Blob missing from sender CAS** (should not happen — the sender just wrote it): send bare
  `CidNotify`.
- **Receiver `cas_put` failure:** fall through to the existing CAS-fetch path (errors as
  today). Best-effort.
- **Mismatched/tampered inline blob:** fails closed via content-addressing (§5).
- **Large attachment with no butler and no live fit:** the one uncovered combination —
  acceptable; large-attachment durability is out of scope (it needs the deposit/CAS infra
  regardless).

## 9. Testing & DoD

Unit (`dm_envelope.rs`, `dm_inbox_ingest.rs`, `iroh_tunnel_dm_transport.rs`):
- `CidNotifyWithBlob` encode/decode round-trip (incl. the length-delimited layout).
- `ingest_dm_packet` with `CidNotifyWithBlob` CAS-puts the blob and fires `dm-received`
  without any zenoh content query.
- A `CidNotifyWithBlob` whose `storage_blob` does not hash to the signed `message_cid` fails
  closed (no `dm-received`, no inbox/sink side effects).
- Send path: an oversize blob falls back to a bare `CidNotify` packet.

E2E (DoD):
- Un-ignore `e2e-harness/tests/e2e_two_node.rs::s2_dm_delivery_over_tunnel_hard_assert` →
  green: two co-located headless peers, **no butler**, friend → DM → recipient fires
  `dm-received` and the plaintext lands in the DM thread. Fix the test comment, which
  currently misattributes the fix to ZEB-483 (it is ZEB-484).

## 10. Out of scope

- **ZEB-483** — invite *deposit* durability (offline/cross-WAN bootstrap of the `Space`).
  Distinct: that is the `DmInvite`'s durability; this is content-blob delivery.
- **Attachment delivery** beyond the single message book (multi-book Merkle trees).
- **Authorized content-serve** (serving encrypted CIDs to authenticated members).
- **Cross-WAN / two-machine proof** (needs AVALON; co-located S2 is the DoD here).

## 11. File map

| File | Change |
|---|---|
| `src-tauri/src/dm_envelope.rs` | `DmPacket::CidNotifyWithBlob` variant + `encode_packet`/`decode_packet` arm (disc `0x04`, length-delimited) + unit tests |
| `src-tauri/src/iroh_tunnel_dm_transport.rs` | send rung reads CAS blob, emits `CidNotifyWithBlob` when it fits, else bare `CidNotify`; still returns `Transient` |
| `src-tauri/src/dm_inbox_ingest.rs` | `CidNotifyWithBlob` dispatch arm: `cas_put` then delegate to existing `CidNotify` ingest + unit tests |
| `e2e-harness/tests/e2e_two_node.rs` | un-ignore `s2_dm_delivery_over_tunnel_hard_assert`; fix the ZEB-483→ZEB-484 comment |
