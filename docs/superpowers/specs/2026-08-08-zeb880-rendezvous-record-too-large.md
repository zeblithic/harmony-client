# ZEB-880: community rendezvous pkarr record exceeds size cap (RecordTooLarge)

**Status:** implemented (2026-08-08).
**Branch:** `zeblith/zeb-880-v024-community-rendezvous-pkarr-record-exceeds-size-cap`

## Symptom (field, ZEB-878 v0.2.4 validation)

On AVALON, from community creation onward, the pkarr publisher failed every 60s:

```text
WARN harmony_pkarr::publisher: pkarr publish failed — retrying in 60s
     handle=rendezvous:<cid>:0 error=RecordTooLarge
```

The community's rendezvous record therefore **never published** → the community
was undiscoverable cross-WAN via the rendezvous path. Silent to the user (only a
log WARN).

## Root-cause investigation (systematic-debugging Phase 1)

**Seam map.** The node's reachability payload is built by a single shared
`blob_builder` closure (`lib.rs:9898`), cloned into all five pkarr publishers:
identity/case-B, community, **rendezvous**, friend/case-D, invite. `direct_addresses`
flows `iroh endpoint → direct_addr_filter::gather_routable_direct_addrs →
ReachabilityAnnouncePayload.direct_addresses` through a filter that drops
loopback/link-local/down-interface addresses but **never caps count or size**
(`direct_addr_filter.rs:69-86`). `direct_addresses` is the **only unbounded
field** — `butler_set` is capped at 2 entries (and elided when empty), the vouch
and envelope are fixed-shape.

**Where the cap trips.** Each publisher wraps the blob in a `PkarrRoutingRecord`
(`sign_new`), whose canonical CBOR is base64url-encoded (~×4/3) and DNS-framed
into a `SignedPacket`; `harmony_pkarr::wire::build_relay_payload` returns
`RecordTooLarge` when that exceeds `pkarr::SignedPacket::MAX_BYTES = 1104`
(frozen core; `wire.rs:68`).

**Measured byte budget** (AVALON shape: 2 IPv4 + 3 global IPv6, real relay URL,
2-entry butler set):

| record | CBOR | base64 | vs cap |
|---|---:|---:|---|
| rendezvous (butler + vouch + 5 addrs) | 902 | 1204 | ❌ busts hard |
| rendezvous (vouch, **no butler**) | 612 | 816 | ✅ fits |
| identity/case-B (butler, no vouch) | 764 | 1020 | ❌ also over cap |

Two findings that shaped the fix:

1. **`butler_set` (~290 B for 2 entries), not `direct_addresses` (~86 B), is the
   dominant driver.** Trimming *all 5 addresses* only drops rendezvous to base64
   ~1089 — still over; the rendezvous blob overflows even with **zero** addresses,
   so **address-bounding alone cannot fix it.** The single largest reclaimable
   chunk is `butler_set`. This also explains the ticket's A/B test (toggling relay
   opt-in OFF didn't clear it → the record was simply oversized).

   **But `butler_set` is NOT dead weight in the rendezvous record** (a
   convergence finding): the gateway dial driver seeds a resolved beacon's payload
   into the `ReachabilityResolver` via `seed_from_pkarr` (stored as `PkarrLive`,
   `community_gateway_dial_driver.rs:521`), and the offline-DM path reads *that*
   `butler_set` for seal targets (`butler_deposit::freshest_butler_set_by_source`,
   the `PkarrLive` arm). Stripping it entirely would strand offline-DM seal
   targets for rendezvous-discovered peers until durable/member reachability
   arrives. So the rendezvous blob **caps** `butler_set` to one entry (a full
   2-entry set cannot fit under the cap at any address count) rather than
   stripping it — keeping a seal target while still fitting.

2. **The case-B identity record also overflows on the same host** (base64 1020 →
   relay payload > 1104). It legitimately needs `butler_set` (offline DM
   delivery), so *there* the correct lever is address-bounding. Leaving it broken
   would be a second-order miss — it silently breaks the ZEB-879 discoverability
   flow on any multi-address host.

## Design

Client-side only; no change to the frozen `harmony-pkarr` core. New module
`reachability_bound.rs`:

- **Budget, derived not guessed.** The wire codec is base64url `URL_SAFE_NO_PAD`
  (unpadded, `⌈4n/3⌉`), but the budget is derived under the *padded* bound
  `4·⌈n/3⌉` — a strict upper bound — so it is safe regardless of padding:
  `MAX_RECORD_CBOR_BYTES = ((MAX_BYTES − FRAMING_RESERVE) / 4)·3 = 729 B`
  (`FRAMING_RESERVE = 130` covers DNS TXT framing + BEP44 sig/seq). Computing
  `((M−F)/4)·3` — the integer base64-block-count floor times 3 — rather than
  `(M−F)·3/4` matters: the latter's multiply-then-floor can round *up* past the
  true inverse and admit a one-byte-over record (Qodo/CodeRabbit finding).
  Field-anchored against AVALON's actual 902 B / 1204 B overflow. Satisfiability:
  even the worst butler-carrying record with **zero** addresses (friend: envelope
  + case-D seal + butler payload ≈ 714 B) fits, so the trim always converges
  (pinned by a test).
- **`bound_direct_addresses(payload, reserved) -> dropped`** trims
  `direct_addresses` until `encoded_len(payload) + reserved ≤ MAX_RECORD_CBOR_BYTES`,
  dropping the **largest-encoding** leg each round. This deliberately does *not*
  rank by scope (a CodeAnt finding: unconditionally demoting RFC1918/ULA can
  strand a peer's only same-LAN route). Since IPv6 octets are the largest, the
  trim keeps the compact IPv4 legs — naturally retaining both a public and a
  same-LAN IPv4 route when present. No-op (bytes unchanged) when already within
  budget — the common solo-node case pays nothing. `reserved` is passed by the
  caller for its record shape (envelope, + vouch or seal).
- **`cap_butler_set(payload, max_entries)`** truncates `butler_set` for the
  rendezvous blob (clearing `bs_at` only when emptied), preserving the
  highest-priority seal target(s) for the offline-DM seed path.

**Wiring:**

- **Shared `blob_builder` (`lib.rs`)** — bounds addresses reserving
  `RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES` (covers the bare-blob record types
  identity/community/invite *and* the sealed friend record — the largest bare
  consumer). A `debug!` notes any trim (expected behavior now, not an error).
- **Rendezvous `RecordBuilder` (`community_rendezvous_publisher.rs`)** — caps
  `butler_set` to 1 (a full 2-entry set + the `"mv"` vouch cannot fit at any
  address count; capping keeps a seal target for the seed path), then bounds
  addresses reserving `RECORD_ENVELOPE_BYTES + RENDEZVOUS_VOUCH_BYTES`.

### Degradation

Trimming `direct_addresses` is graceful: `home_relay_url` is always kept and the
dial is relay-assisted (iroh can `connect()` on relay + node_id, then holepunch
via surviving addresses). Trading fewer direct legs for "the record actually
publishes" is strictly better than an oversized record that never publishes.

On a heavy host (full 2-entry butler set + several IPv6 legs), the butler-carrying
identity/community records keep `butler_set` (offline delivery) and shed direct
addresses — the correct priority, since the relay is the reliable fallback and
`butler_set` enables offline DM. The rendezvous record instead keeps one butler
entry (seal-target seed) and as many addresses as then fit.

## What this does / does not do

- **Does:** guarantee every published pkarr record fits `MAX_BYTES` by construction,
  across all five record types, on any address count — fixing the reported
  rendezvous `RecordTooLarge` and the latent case-B identity overflow.
- **Does not:** shard an oversized record across `:0/:1/…` slots (the slot index
  is the advertiser-rank claim, not a payload shard — repurposing it would collide
  with that semantics); nor surface a publish failure in the in-app diagnostics
  (moot — the failure no longer happens). Both were ticket "suggested directions,"
  neither needed once the record is bounded.

## Test plan

`reachability_bound.rs` unit tests:
1. `budget_is_satisfiable_for_worst_butler_record` — the friend/zero-address case
   fits, so the trim converges.
2. `rendezvous_record_fits_after_cap_and_bound` — the AVALON record overflows
   pre-fix (even with zero addresses, proving the butler cap is required), fits
   post-fix, and keeps one butler seal target + ≥1 dial address.
3. `identity_record_fits_after_bounding_addresses` — case-B overflows pre-fix,
   fits after address-bounding, with `butler_set` preserved.
4. `trim_drops_largest_ipv6_before_compact_ipv4` — both IPv4 legs (public +
   RFC1918) survive one forced drop; an IPv6 leg is the one shed.
5. `small_payload_is_untouched` — under-budget payloads are byte-identical (no
   wire change; the pinned wire-format fixtures stay green).

`community_rendezvous_publisher.rs`:
6. `refresh_slot_bounds_oversized_record_to_fit` — end-to-end through the real
   publisher `RecordBuilder`: the registered record fits the budget, the vouch is
   still present, `butler_set` is capped to one entry, and ≥1 dial address is kept.

## Out of scope / follow-ups

- In-app network-diagnostics surface for reachability-publish health (ticket
  suggestion #3) — separate diagnostics work; the overflow it would have surfaced
  is now prevented.
- IPv6 same-prefix coalescing (ticket suggestion #2) — unnecessary once the record
  is bounded; the trim already keeps the most-useful legs.
