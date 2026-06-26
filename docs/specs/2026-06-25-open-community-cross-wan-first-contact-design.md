# Open-community cross-WAN first-contact — design

**Status:** approved design, pre-implementation
**Ticket:** ZEB-570 (parent epic ZEB-327, v0.1.0-alpha)
**Date:** 2026-06-25
**Author:** Koya (with Jake)

## Goal

Let a remote user holding an **open/public community invite link** make first contact and bootstrap-sync the community **across the internet (cross-WAN)** — without depending on any one specific person (admin or inviter) being online. Today this only works on the same LAN (Zenoh multicast); cross-WAN open join fails because the open path has no dialer.

## Background / problem

Harmony has no central servers. A client finds and reaches peers via **pkarr over the Mainline BitTorrent DHT** (signed `pubkey → routing record`, TTL-bounded), dials over **iroh** (QUIC + hole-punch + relay fallback), and syncs over **Zenoh**. Community membership is a **CRDT** authorized by enrollment certs.

Two join modes exist today:

- **Invite-only (works cross-WAN).** The invite token's signature derives ephemeral pkarr keying material **and** the URL carries the **admin's `identity_pub`**. The joiner resolves the admin's per-community routing record from the DHT (`derive_ephemeral_key(epoch_key, admin_identity_pub‖epoch)`), dials the admin over the `HARMONY_HANDSHAKE_V1` ALPN, sends a signed `CommunityInviteSigned`, the admin countersigns, and sync proceeds.
- **Open/public (fails cross-WAN).** `join_open_community_inner → redeem_invite_inner` snapshots **no** pkarr/reachability/iroh capability — it does a purely *local* membership insert and relies on Zenoh LAN peering. The open URL carries the community `epoch_key` but **no member `identity_pub`**, so even the existing per-community pkarr discovery (`pkarr_community_publisher` / `pkarr_resolver_adapter`) is unusable: resolving a member requires `HKDF(epoch_key, member_identity_pub‖epoch)`, and the joiner knows no member's `identity_pub`.

### What the codebase already provides (so this design is additive, not new infrastructure)

A code recon established that the durability properties we want are **already in place**, which narrows the work to *first-contact only*:

- **The inviter/admin is not load-bearing after join.** Once admitted, a member syncs the CRDT with *any* co-member over Zenoh; the admin is only involved at invite-only join time. So "don't lose the community when your inviter leaves/is kicked" is already true, and there is no invite cascade.
- **Per-member community reachability records already exist.** `pkarr_community_publisher.rs` — every joined member publishes its iroh `ReachabilityAnnouncePayload` under `derive_ephemeral_key(PkarrCase::Community, epoch_key, identity_pub‖epoch_id)`, TTL-bounded, epoch-rotating.
- **A capped, freshness-gated serving *set* already exists.** `community_relay_resolver.rs` (ZEB-380) keeps a CRDT-replicated LWW set of relay advertisers, capped at `COMMUNITY_RELAY_ADVERTISERS_MAX`, dropped on staleness (~7.5 min TTL).
- **Open admission by signature alone already exists.** `bootstrap_admit_open_publisher` (ZEB-558/#336) admits an open `Join` authorized by the joiner's signature, no admin countersign.
- Per-community presence beacons (ZEB-537), Zenoh per-community sync, and a transport-epoch re-arm that retries discovery on any new peer.

The **only** missing bridge is: turn "I hold the link (`epoch_key`)" into "I can resolve and reach *some* live serving peer." Everything after first contact already works.

## Decisions (locked in design review)

1. **Discovery model: URL-capability.** "Anyone with the invite link can join." The link carries the `epoch_key` as a bearer secret. Holders can resolve a live serving member and dial in; non-holders cannot enumerate or resolve the community. No public/browsable directory (that is a separate later effort).
2. **First-contact mechanism: a community-keyed rendezvous record** on the DHT, resolvable from `epoch_key` alone.
3. **Serving set: a beacon subset = the existing relay-advertiser set** (reuse ZEB-380), not every member and not the admin.
4. **Admission (v1): frictionless accept-and-moderate.** The link is the capability; abuse is handled by the existing kick/ban tombstones (checked at admission) plus a cheap beacon-side rate-limit.
5. **No invite cascade** on kick/leave (already true; an opt-in community policy could add it later).
6. **Friend-graph discovery, public directory, and PoW/WoT/ZK hardening are explicit v2 follow-ups**, out of scope here.

## Architecture

### Component 1 — Community-keyed rendezvous record (enumerated slots)

A DHT/pkarr record is one signed value per key, and a link-holder does not know any beacon's `identity_pub`. To let a holder resolve a *set* of beacons from `epoch_key` alone, use **enumerated rendezvous slots**:

- `N` = `COMMUNITY_RELAY_ADVERTISERS_MAX` (~8).
- Slot key: `rendezvous_key(i) = derive_ephemeral_key(PkarrCase::Rendezvous, epoch_key, i ‖ epoch_id)` for `i ∈ 0..N`, reusing the existing epoch-tolerance scheme (`epoch_id` + adjacent windows). A new `PkarrCase::Rendezvous` (or equivalent domain-separation tag, e.g. info-string `"rendezvous"`) keeps these keys disjoint from the existing member-keyed records.
- Payload: the same `ReachabilityAnnouncePayload` (iroh endpoint id + relay URLs) the member publisher already emits.
- **Publisher per slot:** each beacon claims exactly one slot by its deterministic rank in the CRDT-known advertiser ordering (sort advertisers by `identity_pub`; rank = slot index). Because the advertiser set is CRDT-replicated, every member computes the same ordering → each slot has exactly one writer → no DHT write races.
- **Resolution:** a joiner derives `rendezvous_key(0..N-1)` from the URL `epoch_key`, resolves them in parallel (existing `ReachabilityResolver` path), and dials the first live beacon.

This is a small, consistent extension of `pkarr_community_publisher`: same payload, same resolver, same iroh dial path; only the **key derivation** (slot index instead of member identity) and **who publishes** (beacons) change. Member-keyed records and post-join sync are unchanged.

### Component 2 — Tokenless open-join handshake

A sibling message on the existing `HARMONY_HANDSHAKE_V1` ALPN, distinct from `CommunityInviteSigned`:

```
OpenJoinRequest {
  community_id,
  joiner_identity_pub,
  joiner_enrollment_cert,   // joiner's own identity→device binding
  epoch_auth,               // capability proof, see below
  nonce,                    // anti-replay
  timestamp,                // anti-replay (bounded window)
  joiner_sig,               // over the full request, by joiner's device key
}
```

**Capability proof (the link is the capability at admission, not only at discovery):**
`epoch_auth = HMAC( HKDF(epoch_key, "open-join-auth"), community_id ‖ joiner_identity_pub ‖ nonce ‖ timestamp )`.
The beacon also holds `epoch_key`, recomputes `epoch_auth`, and rejects on mismatch — so a party who learned a beacon's iroh address by other means but does not hold the link cannot join.

**Beacon-side flow** (a beacon is an ordinary member, *not* an admin):
1. Confirm it serves `community_id`.
2. Verify `epoch_auth` (capability), `joiner_sig` + `joiner_enrollment_cert` (identity control), and the `nonce`/`timestamp` freshness window (replay guard).
3. **Ban-check:** reject if `joiner_identity_pub` has a ban tombstone in the membership CRDT.
4. Apply rate-limit (per-source / per-window); shed excess.
5. Hand off to the **existing open-admission gate** (`bootstrap_admit_open_publisher`): insert the joiner's self-authorizing open `Join` into the CRDT (no countersign).
6. Serve the bootstrap (current state-root / CRDT snapshot). The joiner then continues via Zenoh + relay-pool + member-keyed reachability for ongoing sync.

The only new server-side logic is parse + verify `OpenJoinRequest`; admission and sync reuse shipping code. Beacons being plain members is what makes joins churn-durable.

### Component 3 — Beacon election + self-healing

- **Beacon set = online, reachable members of the `community_relay` advertiser set** (ZEB-380). No new election system.
- **Slot claim** = deterministic rank in the advertiser ordering (sorted by `identity_pub`).
- **Self-healing (guarantees a reachable beacon whenever anyone is online):** any online member that observes the live rendezvous slots are under-filled self-promotes to a relay-advertiser/beacon and begins publishing; to avoid a thundering herd, the **lowest-ranked eligible online member promotes first**. Eligibility is **power-aware**: opted-in / high-availability / desktop members are preferred; low-power / mobile members are deprioritized and self-promote only as a last resort.
- Beacons republish their slot on the existing epoch/TTL refresh cadence and on reachability change.

### Component 4 — Joiner UX + cold-start

- The open-redeem path snapshots the pkarr/reachability/iroh capability (which it does not today), derives the rendezvous slot keys from the URL `epoch_key`, resolves, dials, and sends `OpenJoinRequest`.
- **True cold-start (every member offline):** the slots expire by TTL and first contact cannot complete — inherent to a serverless mesh. This is a **retryable, non-error state**: the redeem surfaces "no one's reachable right now — we'll keep trying" rather than an error; the existing transport-epoch re-arm retries automatically; the moment any member comes online and self-promotes, the waiting joiner connects.
- The same-LAN Zenoh open-join path is unaffected and continues to work co-located.

## Data flow (cross-WAN open join, happy path)

1. A beacon (relay-advertiser, slot `i`) publishes `ReachabilityAnnouncePayload` to `rendezvous_key(i)` on the DHT.
2. Joiner opens `harmony://join/<community_pubkey>#<epoch_key>`, derives `rendezvous_key(0..N-1)`, resolves them in parallel, picks a live beacon.
3. Joiner dials the beacon over iroh (`HARMONY_HANDSHAKE_V1`) and sends `OpenJoinRequest` with `epoch_auth` + self-signature + enrollment.
4. Beacon verifies capability + identity + freshness, ban-checks, rate-limits, then admits the open `Join` via `bootstrap_admit_open_publisher` and serves the bootstrap snapshot.
5. Joiner syncs the CRDT + content via Zenoh + relay pool; ongoing reachability of other members resolves through the existing member-keyed records. Done — admin never involved.

## Privacy & abuse posture

**Privacy.** Resolution is link-gated: slot keys derive from `epoch_key` via HKDF, so only link-holders can derive/resolve them; passive DHT observers without `epoch_key` see only opaque mutable records they cannot link to a community or to each other. Beacon iroh endpoints are exposed only to link-holders (inside the trust boundary). The admission `epoch_auth` enforces the capability even if a beacon address leaks. *(v2: ZK/VRF unlinkable beacons + mixnet IP privacy.)*

**Sybil/abuse (v1).** Frictionless accept-and-moderate — the link is the capability; abuse handled by the existing kick/ban tombstones (propagate + ban-check at admission) plus a cheap beacon-side `OpenJoinRequest` rate-limit. *(v2: PoW join challenge, WoT-knock N-member endorsement, rate-limited delegated invite capabilities.)*

## Scope

**In (this spec / ZEB-570):**
- `PkarrCase::Rendezvous` slot-key derivation + community-keyed rendezvous record.
- Beacon publish of rendezvous slots from the relay-advertiser set, with deterministic slot claim + power-aware self-promotion.
- `OpenJoinRequest` handshake message + beacon-side verify (capability, identity, freshness, ban-check, rate-limit) → existing open-admission gate.
- Open-redeem path: snapshot pkarr/reachability/iroh, resolve rendezvous, dial, send `OpenJoinRequest`; retryable cold-start UI state.

**Out (documented follow-ups):**
- Friend-graph fallback discovery (v2 — Sybil-resistant complement when DHT/beacons fail).
- Public / browsable community directory (separate effort).
- Invite-cascade on kick/leave (opt-in community policy).
- PoW / WoT-knock / ZK-VRF hardening.

**Unchanged:** invite-only path, post-join sync, member-keyed reachability records, same-LAN open-join path.

## Testing

**Unit**
- `rendezvous_key(i, epoch)` derivation: determinism across processes; correct epoch-tolerance windows; disjoint from member-keyed `PkarrCase::Community` keys.
- `epoch_auth` HMAC: accept valid; reject wrong `epoch_key`; reject replay (stale `timestamp` / reused `nonce`).
- Slot-claim ranking: every member computes the same advertiser ordering → same slot assignment; cap respected.
- `OpenJoinRequest` verify: reject bad `joiner_sig`, bad/expired enrollment cert, banned identity; rate-limit sheds excess.
- Self-promotion: under an under-filled slot set, the lowest-ranked *eligible online* member promotes; power-deprioritized members defer.

**Integration**
- **Cross-WAN open join (must FAIL on main, PASS after):** two nodes with **no LAN multicast** — beacon publishes a slot, a URL-only joiner resolves + dials + is admitted + syncs. Mirror the existing invite-only integration test, open variant.
- **Beacon-offline failover:** kill the slot-0 beacon; joiner resolves and dials slot-1.
- **Cold-start:** all members offline → join is retryable (non-error); bring a member online → it self-promotes and the waiting joiner connects.

**E2E**
- A cross-WAN open-join scenario in the ZEB-447 agent harness, to validate live on the fleet (Koya / Ildwyn / AVALON).

## Open questions / risks — to be tuned with measured data

These are deliberately left as **tunable, instrumented knobs** rather than hard-coded constants, so we can let real success-rate and latency data drive the values rather than guessing up front.

- **Resolve strategy: escalating-batch, not all-`N`-at-once.** Default the joiner to an **escalating widening** resolve: try slot 0, and if it does not yield a live beacon within a short deadline, widen to slots 0–1, then 0–2, … up to `N`. This avoids both extremes — hammering slot 0 alone (hot-spot / single point of contention) and firing all `N` DHT resolves unconditionally (3–8× the discovery traffic). The batch size, per-batch deadline, and widening curve are **config constants** (not literals scattered in code), and the resolve path **emits metrics** for *which slot succeeded* and *time-to-first-live-beacon* so we can answer the two real questions empirically: (a) does spreading load across slots beat always hitting slot 0, and (b) how much does parallelism-of-2/3/`N` actually buy in success rate **and** latency to justify the extra traffic. `N` stays `COMMUNITY_RELAY_ADVERTISERS_MAX`.
- **Advertiser-set churn vs. slot stability.** If the advertiser ordering changes (a higher-ranked advertiser appears/leaves), slot assignments shift; ensure the TTL/refresh cadence keeps at least one slot reliably live during reordering, and that a joiner resolving mid-reorder still finds a live beacon (escalating widening tolerates transient per-slot gaps because it keeps widening until *some* slot answers).
- **Self-promotion convergence — observe, then tune.** Keep the simple "lowest-ranked eligible online member promotes" rule for v1, but **instrument it** (log/metric: promotion events, observed-online-set size, slot-fill latency, any demote/re-promote churn) so observational data tells us whether it converges cleanly or oscillates when membership/online-state changes concurrently (the CRDT-observed online set may differ briefly across members). No strong a-priori preference on how conservative the promotion debounce should be — the measured oscillation rate decides it.
