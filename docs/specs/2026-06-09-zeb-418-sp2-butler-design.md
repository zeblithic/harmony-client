# SP2 — Butler: async store-and-forward delivery via the owner's always-on devices

**Status:** Design approved in brainstorm 2026-06-09 (this doc is the write-up; pending Jake's spec review).
**Ticket:** [ZEB-418](https://linear.app/zeblith/issue/ZEB-418) · **Epic:** [ZEB-416](https://linear.app/zeblith/issue/ZEB-416) · **Framing:** `docs/specs/2026-06-09-multi-device-fleet-butler-framing.md` (approved 2026-06-09)
**Depends on:** SP1 Fleet Sync substrate ([ZEB-417](https://linear.app/zeblith/issue/ZEB-417), merged PR #218) · [ZEB-372](https://linear.app/zeblith/issue/ZEB-372) real birational X25519 in `PubKeyBundle` (in flight, parallel track)
**Reuses:** `seal_to_owner`/`open_from_owner` (`dm_signing.rs`) · `DmOutbox` ([ZEB-216](https://linear.app/zeblith/issue/ZEB-216)) · 30-day expiry ([ZEB-227](https://linear.app/zeblith/issue/ZEB-227)) · friend graph ([ZEB-370](https://linear.app/zeblith/issue/ZEB-370)/371) · pkarr first-contact record ([ZEB-382](https://linear.app/zeblith/issue/ZEB-382))

## 1. Goal

Whichever of the owner's devices is online acts as the **butler**: it accepts deliveries addressed to the owner while the owner's active device is offline, deposits them into a fleet dataset, and SP1 replication delivers them to every sibling. Identical code on every device — butler is a role, not a node type.

**V1 (Phase 1) demo:** Alice DMs Bob. Bob's phone is offline; Bob's desktop is online. Alice's client fails direct delivery, resolves Bob's butler-set, deposits the sealed DM at Bob's desktop, and gets an ack — her existing "delivered" state fires. When Bob's phone comes back online, SP1 backfills the DM into his normal conversation view.

## 2. Decisions (settled in this brainstorm)

| # | Decision | Choice |
|---|---|---|
| D1 | V1 slice | Inbound deposit for **1:1 DMs**, first-party path only |
| D2 | UX | **Minimal**: existing "delivered" state fires on butler ack (fleet-aware meaning). No new UI surface in P1 |
| D3 | Phasing | Capability-ordered 4 phases (§8); each independently demoable and merged separately |
| D4 | Envelope target | Seal to the **butler device's** birational X25519 (ZEB-372); butler decrypts on accept, SP1's KeyTree re-protects at rest/in transit. Multi-copy per-device sealing is deferred to the P4 relay path |
| D5 | Admission (first-party) | Butler's **local state lookup** (sender owner ∈ friend graph / existing DM thread). No cryptographic membership proofs until P4 |
| D6 | Dataset | New small `dm-inbox-v1` dataset (deposited-but-not-ingested deliveries). **No** migration of DM history onto the substrate |
| D7 | Ack ordering | Butler **persists the dm-inbox write, then acks**. An ack never lies |
| D8 | Inner payload | The existing signed DM wire format, unchanged. The butler is a transport waypoint, not a new message format |
| D9 | Advertisement | Extend the existing pkarr first-contact record with a `butler_set` section; no second record or polling path (size fallback: §3) |
| D10 | First-party butlering | Default-on, no consent surface (your own devices). Opt-in applies only to the P4 third-party relay |

Carried from the framing doc (consciously declined for v1): no Double Ratchet (per-message sealed ECDH), no per-sender UCAN/PoW on the first-party path, identity-keyed pkarr discovery (rotating `R(t)` rendezvous is P4-adjacent hardening).

## 3. Butler-set advertisement

Extend the owner's existing pkarr first-contact record with:

```text
butler_set: [                      # ordered priority list, max 2 entries in v1
  { d:  <device_id>,               # 16-byte identity hash of the device
    ep: <iroh EndpointID>,         # 32 bytes (transport key — NOT the identity key)
    vk: <device ed25519_verify>,   # 32 bytes — the cert-bound device identity key; senders
                                   #   derive the seal target as birational(vk) (ZEB-372)
    hr: <home relay URL>,          # string
    pin: <bool> },                 # pinned always-on device (UI for pinning lands P2; format carries it from day one)
]
bs_at: <unix_ms>                   # advertised_at freshness stamp for the whole set
```

- **Ordering:** pinned device first, then longest-uptime. (P1 has no pin UI, so in practice: uptime order.)
- **Publishing:** whichever online device observes a fleet-presence change via SP1's `list_online_devices()` republishes the record with BEP44 `seq+1`. Sibling publish races are harmless — last-writer-wins on seq, and both writers describe the same fleet.
- **Freshness:** senders treat a record with stale `bs_at` (threshold pinned in the plan, ~15 min) as *no butler-set* and fall through to the existing retry chain (§6). A stale ad can never make delivery worse than today.
- **Size budget (plan-time gate):** BEP44 caps record values at ~1000 bytes. Measure the real record + 2 butler entries. If it busts, fall back to a separate butler record at a domain-separated derived pkarr key (the `harmony-pkarr` `derive.rs` case-vector machinery already supports this); the first-contact record then carries only a 1-bit "butler record exists" hint.

## 4. Deposit protocol

New iroh ALPN `harmony/butler-deposit/1` (exact string matched to existing ALPN naming conventions at plan time). One round trip (verification order amended PR #221 round 1, 2026-06-10):

```text
sender                                   butler (recipient-owner device)
  │  DepositFrame                          │
  │ ──────────────────────────────────────▶│  0. recipient bind: frame is for THIS owner
  │                                        │  1. admission: sender_owner is an Active friend
  │                                        │     in the local friend graph
  │                                        │  2. verify sender EnrollmentCert chain
  │                                        │  3. verify sig over sealed_blob
  │                                        │  4. decrypt sealed_blob (butler device X25519)
  │                                        │  5. verify inner DM: device→owner binding +
  │                                        │     packet sig + CID bind (receive-path checks)
  │                                        │  6. atomic write to dm-inbox-v1 + persist (SP1);
  │  DepositAck { message_id }             │     quotas enforced inside the persist critical
  │ ◀──────────────────────────────────────│     section (already-stored keys exempt)
  │                                        │  7. ack only after persist succeeds
```

- **DepositFrame:** `{ recipient_owner_id, sender_owner_id, sender_enrollment_cert, sig, sealed_blob }`. `sig` is the sender's cert-bound device ed25519 over a domain-separated `(recipient_owner_id ‖ sealed_blob)` payload (exact bytes pinned by the wire fixture). Steps 0–3 run **before any decryption** — the butler never decrypts unauthenticated bytes.
- **Device→owner binding (amendment PR #221 round 1, 2026-06-10):** step 5 resolves the inner packet's signing DEVICE to its owner from the same `owner_device_cache` the normal receive path uses, and requires it to equal `sender_owner_id` (unknown/ambiguous devices reject). Without it the butler could persist+ack a deposit that ingestion — which reuses the normal receive path — rejects forever: the sender would see "delivered" for a message the recipient never gets (the ack must never lie, D7).
- **Admission rejections** are cheap, unauthenticated-rate-limited, and unlogged beyond a counter (no oracle for probing the friend graph).
- **Quotas/retention:** per-contact quota + global inbox cap (values pinned in the plan); 30-day TTL aligned with ZEB-227. **Amendment (PR #221 round 1, 2026-06-10):** the quotas bound LIVE dm-inbox entries (a storage quota) and are enforced atomically INSIDE the persist critical section rather than as a standalone pre-decrypt step — snapshot-then-insert raced under concurrent connections, and a redelivery of an already-stored entry at a full inbox must re-ack idempotently (already-stored keys bypass the caps) instead of stranding a delivered message.
- **Envelope:** the shipped sealed-ECDH construction byte-for-byte (`ephemeral X25519 ECDH → HKDF-SHA256 → ChaCha20Poly1305`, `32‖12‖ct` layout from `dm_signing.rs`), with a **new HKDF info string** `harmony-zeb-418-butler-deposit-v1` for domain separation. Sealed to the butler device's birational X25519 — derived by the sender as `birational(vk)` from the butler-set entry (§3); under ZEB-372's birational scheme this is identical to the `x25519_pub` field in the device's EnrollmentCert, so either source works and neither requires a cert exchange before depositing.
- **Idempotency:** deposits are keyed by the DM's message id end-to-end; redelivery after a lost ack is absorbed by §5's dedupe.

## 5. The `dm-inbox-v1` dataset + ingestion

A `FleetSyncEngine<DmInbox>` instance configured like the Notes dataset (`lookup_key_tag: b"dm-inbox-v1"`, `publish_seen: true`, Zenoh topic `harmony/owner/{addr_hex}/ds/dm-inbox-v1`), with `on_applied` driving ingestion.

```text
DmInbox = { entries: BTreeMap<message_id, InboxEntry> }
InboxEntry = {
  so: <sender_owner_id>,
  pl: <inner DM payload bytes>,     # the verified, existing-wire-format DM
  da: <deposited_at: Hlc>,
  db: <deposited_by: device_id>,
  ig: BTreeSet<device_id>,          # ingested_by — grow-only, merge = union
}
```

- **CRDT semantics:** entry insert is LWW-by-`da` per message_id (in practice insert-once — same id redeposited carries the same payload); `ig` merges by set union, so siblings ingesting concurrently never race. 2-char canonical CBOR renames, version byte, plaintext-CBOR-at-rest behind SP1's encryption — all per the SP1/Notes pattern.
- **Ingestion:** on `on_applied` (and once at startup), each device runs new entries through the **normal DM receive path** — the same decrypt/verify/store code a direct arrival takes — then adds itself to `ig`. Ingestion is idempotent on message id, which also resolves the both-paths race: a DM arriving direct *and* via butler dedupes at the DM store.
- **GC:** an entry is removed when `ig` covers the owner's enrolled device set, or at the 30-day TTL, whichever comes first. (Device-set coverage uses enrolled — not online — devices; a revoked device's absence must not pin entries forever, so revocation-pruning follows the existing device-revocation path.)
- **Delivered semantics (D2):** sender's existing "delivered" fires on `DepositAck`. Read-state stays on the existing read-marker dataset, untouched.

## 6. Sender fallback chain + failure modes

Strict chain — the butler is a new rung, never a replacement:

1. Direct delivery to the recipient's active device (today's path, unchanged).
2. On failure/timeout: resolve recipient's pkarr record → fresh `butler_set` → deposit in priority order.
3. All butlers unreachable / record stale: today's `DmOutbox` retry loop, exactly as before.

| Failure | Behavior |
|---|---|
| Stale butler-set | Freshness check fails → rung 2 skipped, rung 3 retries as today |
| Butler crash between persist and ack | Sender retries; message-id dedupe absorbs the duplicate |
| Whole recipient fleet offline | Graceful degrade to sender-retry (framing requirement: never errors) |
| Recipient comes online mid-deposit | Message may arrive both direct and via butler → §5 dedupe |
| Deposit rejected (admission/quota) | Sender treats as rung-2 failure → rung 3; no user-visible error for the sender |

## 7. Testing

- **Unit:** envelope round-trip with the new info string; admission accept/reject; quota; DmInbox CRDT merge (concurrent ingestion union, LWW insert, GC on coverage/TTL).
- **Wire pinning:** fixtures for `DepositFrame` and the extended pkarr record (`interop_fixtures.rs` hex-pin pattern with regeneration env-var gate).
- **Integration:** two-engine test — deposit accepted on engine A → SP1 fan-out → ingestion observed on engine B (same harness style as SP1's `notes_engine_publishes_on_local_write`).
- **Manual cross-WAN proof:** Koya ↔ Ildwyn, with a second instance standing in as the recipient's offline/online sibling (logistics at plan time).

## 8. Phases 2–4 (outline; each gets its own plan when its turn comes)

- **P2 — Outbound hold.** `DmOutbox` generalized to a fleet dataset: the owner's other online devices take over retrying to the recipient's butler-set after the sending device goes offline. Adds the pin-a-butler setting (UI in device admin, ZEB-170 surface). With P1+P2, two never-online-simultaneously owners exchange DMs — the headline epic demo.
- **P3 — Group DMs + community-post backfill.** Same deposit machinery, payload scope widened per the framing (group DM fan-out; offline community members' posts land and backfill). Coordinate with ZEB-403 pre-join backfill so there's one backfill path, not two.
- **P4 — Opt-in community-scoped sealed relay.** Volunteer relay advertisement within a community; **multi-copy envelopes sealed per recipient device** (the relay can't decrypt-and-rewrap); short-lived UCAN + PoW hardening on this path only; opt-in UX; per-community quotas; timing-correlation mitigations (padding + randomized polling). The full framing threat model applies here.

## 9. Threat-model delta (P1)

P1 is first-party only: deposits rest exclusively on the recipient owner's own devices. New exposures vs. today: (a) the butler learns sender↔recipient at deposit time — it's the recipient's own device, which learns this on delivery anyway; (b) the pkarr record now advertises which devices are online — bounded by capping at 2 entries and by the existing record already exposing endpoint reachability; rotating-rendezvous hardening is deferred per the framing. No third party touches content or metadata in P1–P3.

## 10. Open items deferred to the P1 plan

1. ALPN string conformance with existing ALPNs (handshake ALPN from ZEB-321 et al.).
2. BEP44 size measurement of the extended record (D9 fallback trigger) — note `vk` adds 32 B/entry; if friendship exchange (ZEB-370/371) already stores peer device certs, `vk` may be droppable for friends and the budget relaxes.
3. Exact freshness threshold, quota, and inbox-cap values.
4. Verify the DM store's dedupe key is the message id end-to-end (the ingestion idempotency anchor).
5. The friend-graph/DM-thread query API the admission check calls.
6. Koya two-instance test logistics (separate data dirs/ports).
7. Confirm `DepositAck` plumbs into the existing delivered-state transition without a new IPC event.
