# ZEB-458 — SP2 P4: opt-in community-scoped sealed relay (DM store-and-forward)

The last piece of ZEB-418. The first-party butler (P1 deposit → P2 outhold →
P3a backfill → P3b group-DM) is complete and proven, but it relies on the
butler being one of the recipient's **own** fleet devices. P4 is the fallback
for when two fleets **never overlap online**: an opt-in co-community volunteer
holds a sealed blob it cannot read and forwards it when the recipient appears.

SP2 decision numbering continues: P3a `D19–D26` → P3b (ZEB-424) `D27–D34` →
**this doc `D35–D45`**.

## Core reframing (read this first)

**P4 is a P1 deposit whose *transport* is "hold-and-pull via a community
volunteer" instead of "deliver to R's butler now."** Verified at source: P1
seals the `DepositPayload` to `birational(butler_device_ed25519_verify)` and
the butler opens it with `ed25519_priv_to_x25519(device_signing_key)`
(`dm_signing.rs`) — the seal target is a **device** key; there is no
device-openable owner-level X25519. So P4 reuses that crypto verbatim:

1. The sender seals the **same `DepositPayload`** (`{cidnotify_packet,
   storage_blob}`) to **R's advertised butler-set device key(s)** — the very
   set it already resolved for the direct P1 attempt that just failed.
2. It hands the sealed blob to a co-community **relay** to **hold opaque** (the
   relay is not one of R's devices → it cannot open it).
3. R's butler device, when next online, **pulls** the blob from the relay and
   **opens** it via the P1 open path, then ingests through the **normal receive
   path**, which CRDT-replicates the structured entry across R's fleet.

The relay is a dumb, co-membership-gated, opaque holding box. The only genuinely
new machinery is: a community-relay **advertisement**, a co-membership-gated
**holding store**, a **pull** protocol, and a last-resort **sender rung**.

Threat-model bar (settled with Jake): **working fallback first, harden later.**
The relay MAY learn coarse metadata (sender S and recipient R are co-members of
community C, and S deposited *something* for R). Full unlinkability is `D44`.

## Decisions

### D35 — Seal target: R's butler-set device key(s), exactly P1 (not an owner key)

The relay deposit seals the identical `DepositPayload` to **each device in R's
advertised butler-set** (≤2) with a P4-specific HKDF info string, using the
existing `seal_to_owner_with_info(birational(device_vk), payload, INFO)`. The
relay holds the per-device copies opaque; R's butler device opens its copy with
`open_from_owner_with_info(ed25519_priv_to_x25519(device_sk), …)` — byte-for-byte
the P1 open path — then runs the normal receive path. The sender already holds
R's butler-set from the failed direct attempt, so there is **zero new
resolution**.

- **Why ≤2 fan-out (not 1):** resilience. A blob sealed only to an R device
  that never returns is unrecoverable (TTL-GC'd). R's butler-set is already
  capped at 2 (primary + secondary), so the fan-out is bounded and cheap.
- **Residual (accepted):** if R later comes online only on a device that was
  **not** in the advertised butler-set, it cannot open the held blob; it waits
  for an advertised device to return, or the sender re-deposits to the freshly
  advertised set on the next drain. R re-advertising on every online session
  keeps the target current for future deposits.

### D36 — Admission: local co-membership gate against the relay's own C-membership

The relay is a `Joined` member of C, so it already replicates C's membership
CRDT. On a deposit (new ALPN, before holding), the relay, in spec-§4 order:

0. **Recipient bind is N/A** (the relay is not the recipient) — instead, **step
   0 = community bind:** `frame.community_id` must name a community the relay is
   a `Joined` member of and is actively advertising relay service for. Else
   reject. (Cheapest local check first.)
0.5. **Byte ceiling:** `frame.sealed_blob.len() <= RELAY_MAX_SEALED_BLOB_BYTES`
   (`DEPOSIT_MAX_FRAME_BYTES`, 256 KiB), else `TooLarge`. O(1), before the
   co-member scan and any crypto, so the count caps (`D38`) also bound byte
   footprint. No oracle (size is sender-chosen, not membership-revealing).
1. **Co-membership gate:** look up `frame.sender_owner` and
   `frame.recipient_owner` in C's materialized membership; **both** must be
   `Joined` (the P3b `shares_live_group_dm_in` pattern, against community
   membership instead of group-DM spaces). Else uniform reject (no oracle).
2. **Cert:** decode + `verify()` the sender's `EnrollmentCert`; require
   `cert.owner_id == frame.sender_owner` and Master-issued; bind the issuing
   master via the owner-id-derived anchor `owner_id_from_master_ed25519(master)
   == sender_owner` (the ZEB-424 D29.1 anchor — the relay has no friend pin for
   a stranger sender).
3. **Frame sig:** `device_vk.verify_strict(DOMAIN ‖ recipient_owner ‖
   community_id ‖ sealed_blob)`.

The relay does **not** (cannot) open the blob or verify the inner space-bind —
that is R's job at ingest (`D39`). Admission is purely "a `Joined` member of C
is depositing for another `Joined` member of C," which bounds who can consume
holding capacity.

### D37 — Discovery: `CommunityRelayAnnounce` in C's replicated state

A member opts in via a per-community toggle. Opting in publishes a signed relay
advertisement into C's community-state through a new
`MembershipEventKind::CommunityRelayAnnounce`, mirroring P2's
`ReachabilityAnnounce` / `fleet-net` butler-set exactly:

```text
CommunityRelayAnnounce {
  community_id: SpaceId,
  relay: CommunityRelayEntry {
    relay_device_id: [u8;16],
    iroh_endpoint_id: [u8;32],
    relay_device_ed25519_verify: [u8;32],
    home_relay: String,
  },
  ad_at: u64,            // freshness stamp (epoch ms)
}
```

Refreshed on the P2 cadence (`~7.5 min`), freshness-windowed (`~15 min`);
stale ads are skipped. Every member of C replicates and reads the set; senders
pick relays from it (`D40`), recipients poll relays from it (`D39`). The relay
device is authenticated by its `relay_device_ed25519_verify` (an enrolled
device of the advertising member — verifiable against C's membership). Cap the
advertised relay set per community (e.g. 4) to bound fan-out.

### D38 — Holding store: `RelayHoldDoc`, opaque, `ContentId`-keyed, caps + TTL

New `RelayHoldDoc { entries: BTreeMap<String, RelayHoldEntry> }`, persisted via
a `FleetSyncEngine` on the relay (replicated across the **relay's own** fleet,
so the relay's siblings can also serve R's pull — same pattern, same lock
discipline, as the butler's dm-inbox; the blobs are opaque to the relay's fleet
too).

> **Implementation amendment (2026-06-13):** the `recipient_device` field was
> dropped during build. Each seal uses a fresh ephemeral key → a unique
> `sealed_blob` → a unique `ContentId`, so the content id alone distinguishes
> per-device copies; a device label is redundant in both the key and the entry.
> Coverage GC becomes "any ack" (`pulled_by` non-empty) — only the device a blob
> was sealed to can open + ack its content id, so a single ack means the intended
> device received it.

```text
RelayHoldEntry {
  recipient_owner: [u8;16],
  sender_owner: [u8;16],
  community_id: SpaceId,
  sealed_blob: Vec<u8>,        // opaque to the relay
  held_at: Hlc,
  held_by: String,             // relay device id
  pulled_by: BTreeSet<String>, // R device ids that have pulled+acked
}
```

- **Key** = `{recipient_owner_hex}:{ContentId(sealed_blob)_hex}`
  (`space_id`/`message_cid` are sealed and unavailable, so the content address
  of the sealed blob is the dedup key — unique per seal).
- **Caps** (enforced atomically inside the persist critical section, reusing the
  dm-inbox cap pattern): per-`(community_id, sender_owner)` cap
  (`RELAY_HOLD_PER_SENDER_CAP`, e.g. 64) + global cap
  (`RELAY_HOLD_GLOBAL_CAP`, e.g. 1024). An occupied key bypasses caps
  (idempotent redelivery).
- **Per-blob byte ceiling** (`RELAY_MAX_SEALED_BLOB_BYTES` =
  `DEPOSIT_MAX_FRAME_BYTES`, 256 KiB), enforced at admission **before any
  crypto** (step 0.5 of `D36`). Without it the count caps bound only the entry
  count, not the byte footprint — a co-member could store max-transport-sized
  blobs to exhaust the relay before the count cap trips. Together with the
  per-sender count cap this bounds a single sender's footprint to
  `RELAY_HOLD_PER_SENDER_CAP * RELAY_MAX_SEALED_BLOB_BYTES` (mirroring
  `DM_OUTHOLD_DATASET_MAX_BYTES`). Implemented in Phase A.
- **TTL** = 30 days (`RELAY_HOLD_TTL_MS`, reuse `INBOX_TTL_MS`).
- **ZEB-924 (2026-08-12):** TTL expiry now leaves a bounded LOCAL tombstone
  (`expired_at_ms`: 2×TTL retention, cap 4×`RELAY_HOLD_GLOBAL_CAP`,
  `relay_hold_expired.cbor` sidecar) that suppresses resurrection-by-merge
  from a still-holding peer, so a never-acked hold's lifetime on a replica is
  bounded by first-observation + TTL + one sweep. Coverage GC is unchanged
  (fleet-deterministic, needs no tombstones) and deposits are ungated (a fresh
  send mints a fresh content-id key). See
  `docs/superpowers/specs/2026-08-12-zeb924-relay-hold-tombstone-retention-design.md`.
- **GC** (reuse the dm-inbox sweep + one-sweep deferral): each entry is sealed
  to exactly one device, so an entry is **covered** once it has been pulled+acked
  (`pulled_by` non-empty — only the sealed-to device can open + ack it); remove a
  covered **or** TTL-expired entry, with a one-sweep deferral so the `pulled_by`
  update replicates across the relay's fleet before removal.

### D39 — Retrieval: PULL over a query ALPN

The relay is **not** in R's fleet (no CRDT path to R), so R must pull. When any
R device is online it reads C's state, finds the relay ads (`D37`), and for each
fresh relay opens a query connection on `harmony/community-relay-pull/v1`:

```text
RelayPullQuery  { recipient_owner, community_id, requester_enrollment_cert, sig }
RelayPullResponse { entries: Vec<RelayHeldBlob { sender_owner, sealed_blob }> }
RelayPullAck    { content_ids: Vec<[u8;32]> }   // blobs successfully opened+ingested
```

1. The relay authenticates the query: `verify()` the requester cert, require
   `cert.owner_id == recipient_owner` and the requester to be a `Joined` member
   of C, and the frame sig to verify against the requester's device key. This
   gates pull to R's own devices (defeats held-mail enumeration / traffic
   analysis by third parties; the blobs are sealed regardless).
2. The relay returns all held entries whose `recipient_owner == R` — R opens
   only the copies sealed to one of its devices and ignores the rest. (The pull
   response is intentionally recipient-scoped, not community-scoped: the
   `community_id` on the query is the membership-liveness gate of step 1, not a
   response filter. Confidentiality holds regardless — every blob is sealed to
   R's device.)
3. R opens each blob it can (`open_from_owner_with_info` with each local device
   X25519), then feeds the recovered `DepositPayload` through the **normal
   receive path** — `verify_cidnotify_admission` (cidnotify sig, device→owner
   binding, **space membership**, sender/CID consistency), `apply_inbox`, the
   `dm-received` emit. This is the authoritative trust boundary; the relay
   verified nothing about the content.
4. R sends `RelayPullAck` for the blobs it ingested; the relay records
   `pulled_by += R_device` and GCs once covered (`D38`).
5. Anything R cannot open or that fails ingest, R simply drops (only R saw it).

**Ack trust boundary (intentional, not relay-enforceable).** The relay holds
`sealed_blob` opaque (the core confidentiality property), so it can never verify
that an ack's signer actually *opened* the blob — `content_id =
hash(sealed_blob)` is computable from the opaque bytes. The ack therefore proves
only "a `Joined`-member device of `recipient_owner` received these bytes," and
`ack ⟹ opened+ingested` is a **client-honesty contract** enforced by
`open_and_ingest`, which acks a content id only after a successful open + decode
+ ingest. A buggy/malicious ack-without-open is bounded to R's **own** fleet (the
ack cert anchors to `recipient_owner`; no other owner can forge one) — R can only
harm R's own delivery. *Phase B requirement:* the background pull driver MUST ack
a content id only **after** its ingest is durably persisted, so a crash between
ack and persist cannot both GC the entry and lose the deposit.

**GC scope is recipient-owner-scoped, not community-scoped (intentional).** The
hold key is `(recipient_owner, content_id)` with no community component, and
`held_for` is recipient-scoped (step 2). A recipient who is a `Joined` member of
community A can therefore ack — and GC — a blob that was deposited via community
B's gate, even if R has since left B. This is acceptable and correct: the ack
means R's fleet has the bytes (R could open it because it was sealed to R's
device at deposit time, independent of current B membership), so the relay's
delivery job is done. The `community_id` on the pull/ack is a membership-liveness
**spam gate** (R must be a live member of *some* community the relay serves to
talk to it), not a per-blob authorization. The blast radius is self-harm only
(requires R's own device key).

**Wire-envelope shape (Phase B shell decision).** `RelayPullQuery` is a
self-contained frame (embeds `recipient_owner`, `community_id`, cert, sig);
`RelayPullAck` carries only `content_ids`, and the Phase A core
`handle_relay_pull_ack` takes the recipient owner / community / cert / sig as
separate parameters (it re-authenticates each ack independently rather than
trusting pull-session state). Phase B's pull-acceptor shell decides whether to
make `RelayPullAck` self-contained (symmetric with the query) or to thread those
fields from the authenticated session; the core handler's signature already
accepts them explicitly so either envelope shape wires in without a core change.

Poll cadence: on coming online, on C-state sync (a new relay ad appears), and a
periodic floor; no exponential backoff latch is needed (unlike P3a backfill —
this is a direct request/response, not a queryable-holder race).

### D40 — Sender rung: last-resort, after first-party butler, gated on shared community

A new rung in `drain_phase_c`, **after** the P2 butler-set deposit rung, fired
when the butler-set rung produced no ack after `DEPOSIT_NOACK_WINDOWS` **and**
`find_shared_communities(state, self_owner, recipient)` is non-empty:

```text
find_shared_communities(state, self, R):
  state.spaces.iter()
    .filter(kind == Community ∧ self ∈ Joined members ∧ R ∈ Joined members)
    .map(|s| s.id)
```

For each shared community C (priority by member-count / freshness), resolve C's
relay-set (`D37`), seal the **same `DepositPayload`** to R's butler-set
device(s) (`D35`), and deposit on `harmony/community-relay-deposit/v1`. First
relay-ack wins → mark the recipient deposited (the rung outcome mirrors the P2
butler rung: it never mutates `AttemptState`, and a deposited recipient is
treated as delivered-pending-pull). Rung order overall: **direct → first-party
butler-set → community relay** (lowest trust cost first; relay is strangers).
Preserves the "never worse than today" guarantee — the relay rung only adds a
path where today there is none.

### D41 — Wire: new ALPNs + HKDF info + sig domains, byte-pinned

- Deposit ALPN `harmony/community-relay-deposit/v1`; frame
  `RelayDepositFrame { recipient_owner:[u8;16], sender_owner:[u8;16],
  community_id:SpaceId, sender_enrollment_cert:Vec<u8>, sig:Vec<u8>,
  sealed_blob:Vec<u8> }` (the P1 `DepositFrame` + `community_id`; no
  `recipient_device` per the amendment above). Canonical CBOR, strict decode,
  byte-pinned fixture.
- Pull ALPN `harmony/community-relay-pull/v1`; `RelayPullQuery` /
  `RelayPullResponse` / `RelayPullAck` as in `D39`. Canonical CBOR, strict
  decode, byte-pinned fixtures.
- HKDF info `b"harmony-zeb-458-community-relay-v1"`; deposit sig domain
  `b"harmony-zeb-458-community-relay-deposit-v1"`; pull-query sig domain
  `b"harmony-zeb-458-community-relay-pull-v1"`; pull-**ack** sig domain
  `b"harmony-zeb-458-community-relay-pull-ack-v1"` (the ack signs
  `domain ‖ recipient_owner ‖ community_id ‖ sorted(content_ids)` so it is
  self-authenticating + replay-distinct from the query — review hardening). All
  distinct from the P1 butler strings (no cross-protocol confusion).

### D42 — Scope: DM + group-DM only

The relay holds and forwards DM / group-DM deposits (the `DepositPayload` shape).
Community **channel** catch-up stays on P3a's pull-from-online-holders path;
ZEB-425 is the documented no-holder gap there. P4 does not relay channel posts.

### D43 — Opt-in lifecycle + trust scoping

- **Opt in:** a per-community user toggle ("Volunteer as a relay for this
  community"). Default OFF. Opting in starts the advertisement refresh loop
  (`D37`) and installs the deposit + pull acceptors for that community.
- **Opt out / leave:** retract the advertisement (publish a tombstone /
  let it go stale) and stop serving; existing held blobs are served until
  pulled or TTL, then GC'd. Leaving community C (status ≠ `Joined`) forces
  opt-out.
- **Trust scoping:** a relay only admits/serves for communities it is a
  `Joined` member of and advertising for. It cannot relay across communities,
  and content is sealed regardless, so a malicious volunteer's power is limited
  to withholding (an availability nuisance — R falls back to other relays / a
  later first-party overlap) and observing coarse metadata (the deferred-`D44`
  surface).

### D44 — Deferred hardening (separate follow-up ticket)

File a follow-up (e.g. "P4 hardening: unlinkable community relay",
related-to ZEB-458): sealed-sender (the outer frame carries only a destination
token + a blind co-membership proof, so the relay cannot learn S→R), UCAN
short-lived capability tokens, PoW anti-spam on deposit, and timing padding +
randomized polling for the residual correlation. Out of scope here per the
working-fallback bar.

### D45 — Test plan

Unit:
- Admission: `(S,R) ∈ Joined(C)` → accept; either not `Joined` (Left/Banned/
  Invited/`PendingJoin`/absent) → reject; community the relay doesn't serve →
  reject; cert/owner-id/master-anchor mismatch → `BadCert`; frame-sig mismatch →
  `BadSig`. Reject path is pre-hold (no blob stored on reject).
- Byte ceiling: `sealed_blob` one byte over `RELAY_MAX_SEALED_BLOB_BYTES` →
  `TooLarge`, rejected at step 0.5 (before the co-member scan / any crypto /
  any persist); a blob exactly at the ceiling passes the size gate (inclusive
  boundary).
- Seal/open round-trip: a blob sealed to an R butler device opens with that
  device's X25519 and **fails** to open with the relay's key (assert the relay
  cannot read it).
- `RelayHoldDoc`: per-`(C,sender)` cap, global cap, idempotent redelivery
  bypasses caps, 30-day TTL eviction, coverage GC (one-sweep deferral).
- `find_shared_communities`: both `Joined` → included; one `Left` → excluded;
  not shared → empty.
- Drain rung: relay rung fires **only** after the butler-set rung's
  `DEPOSIT_NOACK_WINDOWS` failure **and** a shared `Joined` community exists;
  order direct → butler → relay; rung outcome never mutates `AttemptState`.
- Pull auth: R's cert required, `owner_id == R`, `Joined` in C; wrong owner →
  reject; returns only R's entries.
- Advertisement: opt-in publishes a fresh `CommunityRelayAnnounce`; stale ads
  skipped by both sender and recipient; opt-out / leave retracts.

Integration (extend the two-engine butler harness with a third "relay" engine):
- E2E happy path: S's butler-set unreachable → drain reaches the relay rung →
  S deposits to the relay (admitted, held opaque) → R's butler comes online,
  pulls, opens, ingests via the normal path, acks → relay GCs. Assert R received
  exactly the message and the relay never opened the blob.
- Non-member sender → relay rejects, nothing held.
- Held blob survives a relay restart (persisted) and is still pullable.

Wire pins: `RelayDepositFrame`, `RelayPullQuery`, `RelayPullResponse` byte
fixtures.

## Non-goals

- **Unlinkability / sealed-sender / UCAN / PoW / timing padding** (`D44`).
- **Channel-post relaying** (`D42`; stays P3a + ZEB-425).
- **Relay reputation / load balancing / selection optimization** — senders try
  advertised relays in a simple priority order; recipients poll all fresh ones.
- **Cross-community relaying** — a relay serves only its own `Joined`
  communities.
- **Owner-level X25519 seal target** — ruled out (`D35`); no device-openable
  owner key exists.

## Rollout

Additive: new ALPNs, a new dataset, a new advertisement event, a new opt-in
toggle. No migration; an older build simply doesn't advertise/serve relays and
its senders skip the rung (the recipient's first-party butler path is
unaffected). Phasing (sequential PRs if the single PR is too large — **one PR in
flight at a time**, per the bundling rule; decide in the plan):
- **Phase A:** advertisement (`D37`) + deposit admission + holding store
  (`D36`/`D38`) + pull (`D39`) + wire (`D41`) + opt-in lifecycle (`D43`).
- **Phase B:** the sender rung (`D40`) + the full E2E integration test.
