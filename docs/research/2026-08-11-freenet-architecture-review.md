# Freenet × Harmony: an architecture review

**Date:** 2026-08-11 · **Sources:** `freenet-core`, `river`, `paper-1` (Freenet's whitepaper source), synced at `~/work/freenet/`, reviewed against `harmony-client` main.
**Tracking:** epic ZEB-909; adoption tickets ZEB-910 (R1), ZEB-911 (R2), ZEB-912 (R3), ZEB-914 (R4), ZEB-913 (R5); study tickets ZEB-915/916/917 (R6a/b/c).
**Method:** whitepaper read in full; twelve parallel code-exploration passes over freenet-core and River, plus five over harmony-client for a symmetric baseline. Every mechanism below was verified to file:line; the 22 raw evidence reports are archived in the originating session. A rendered version of this review lives at <https://claude.ai/code/artifact/e735fa3e-1046-4bf4-8ab0-ff229e331247>.

---

**Verdict in one paragraph.** Freenet's whitepaper is unusually honest — nearly every claim survived code verification, often to the exact constant. Its deepest idea is not the ring; it is that **connectivity repair is the steady state, not an exception handler**: every peer continuously spends a small connection budget pulling the topology toward a provable routing property. Harmony's model (social-graph full mesh, membership-gated, end-to-end encrypted, relay-backed) is the right foundation for what Harmony is — and is ahead of Freenet on identity, transport auth, invites, and durability. But we have a verified island blind spot that Freenet's discipline would have caught, a live-inviter dependency their app layer proves unnecessary, and a pairwise-only delivery policy that our own stack (Zenoh linkstate + RBSR) is already capable of outgrowing. Five concrete adoptions below; four things to deliberately not import.

## TL;DR — the six findings that matter

1. **Repair-as-steady-state beats repair-as-escape-hatch.** Freenet peers run a permanent maintenance loop (60s tick, 5s when starved): gap-targeted `CONNECT`s toward under-represented regions, protected nearest-neighbor lattice edges, probabilistic topology swaps. Harmony's only cross-island repair (`community_gateway_dial_driver`) fires solely at *zero* live sessions — a two-island split reads `Healthy` on both sides, forever.
2. **Even Freenet has our island bug.** Their zero-connection escalation has the same blind spot (a node with ≥1 connection in a minority island is never rescued; healing is emergent, plus an incidental 4-hour gateway probe). Nobody has solved minority-island detection — we can leapfrog rather than copy.
3. **River kills the live-inviter dependency by construction.** An invite is a pre-signed membership certificate; redemption is GET-from-any-replica + submit a self-authorized delta; validity is a pure signature-chain walk. The whole ZEB-906/ZEB-908 failure class (redeem needs the inviter reachable; missed finalize strands you at PendingJoin) is unrepresentable in their design.
4. **Delivery does not require pairwise reachability.** Freenet's co-host mesh does merge-then-forward through neighbors with echo suppression; summary/delta reconciliation is a separate anti-entropy protocol on a ~5-minute heartbeat. Harmony already ships every ingredient (idempotent CRDT merges, RBSR anti-entropy, and — per this investigation — Zenoh peer-mode linkstate that *can* multi-hop today); our full mesh is a dial *policy*, not an architectural constraint.
5. **Their transport is the cautionary tale, not the inspiration.** A code comment cites a measured **~84% production hole-punch failure rate**; their answer is statistical retry across 8 candidate acceptors (≈72% eventual success) because they have no relay. iroh's relay fleet is precisely the asset that spares us this — and their port-forwarding requirement is the audience ceiling.
6. **Operational honesty pays.** Their production incidents are the best teachers in the corpus: nondeterministic summary bytes → ~20M spurious full-state re-syncs; pruned-message re-offers → 63.7% of network broadcast bytes; a too-small routing candidate window → 63% of failing GETs never visiting a subscriber. Each maps to a check worth running on our own reconciliation paths.

---

# Part I · Evidence — Freenet

## 1 · What Freenet is (`freenet-core`)

A single overlay network of peers on a ring `[0,1)`, storing *contracts*: WASM modules that carry their own CRDT algebra (merge = idempotent commutative monoid), validity predicate, and summary/delta sync functions. The platform routes, replicates, and syncs generically over whatever algebra the contract supplies. Public state only — confidentiality is the application's problem; private state lives in device-local *delegates*. Live network: ~443 active peers (604 over 24h with churn); GET median path 7 peers visited, consistent with log-diameter routing at that size.

### 1.1 Topology: the part worth studying closely

- **Locations** are hashed from the peer's external address *prefix* (/24 IPv4, /48 IPv6) — deliberate anti-grinding, with the acknowledged cost that all peers behind one NAT/prefix collapse to ring-distance zero (breaks replica independence for CGNAT/cloud cohorts). Location = f64 from a SplitMix-style mixer, not a cryptographic hash.
- **Connections bounded 25–200.** A joiner below 3 connections targets its own location (jitter escalating on failure); above it, **gap-targeting**: find the largest gap in *log-distance space* among current neighbors, target its midpoint ±25% jitter. This is the active mechanism pulling each node's link distribution toward Kleinberg's 1/d — the distribution that makes greedy routing O(log²N).
- **Acceptance chain** at the receiving side: forward greedily if strictly-closer exists; within 5% of the target, accept-while-forwarding with probability 0.6→0; at the terminus, a Kleinberg gap-score gate with a sliding floor (generous at 3 conns → selective near 25); saturated terminus re-forwards *uphill* with a budget of 8 extra hops.
- **Protected lattice**: each peer's strict nearest successor + predecessor are force-accepted even over capacity (+2 slack) and excluded from pruning/swaps — the short-range base lattice Kleinberg's bound assumes.
- **Maintenance loop**: 60s steady tick, 5s while under-connected (with no-progress backoff); peer-health eviction every 300s; a never-ending route-to-self lattice probe (5s→300s backoff, resets on improvement); topology swaps capped at 0.1 probability/tick.
- **Partition healing: none beyond two edges.** Zero-connection isolation escalates to direct gateway redial (30s cold / 120s steady thresholds). A 4-hourly gateway version-probe incidentally re-links across a split. **A node with ≥1 connection inside a minority island is never detected or rescued** — healing is emergent from churn plus gap-targeting through *already-reachable* peers.

### 1.2 Routing

- Greedy by ring distance, HTL cap 10, with a **512-bit per-transaction-keyed Bloom filter** as the visited set (fixed 64-byte wire cost; per-tx keying prevents cross-transaction correlation).
- Under 50 recorded events: pure distance with an untried-first exploration bias. Beyond that, an **adaptive ensemble**: three isotonic-regression estimators (latency / failure / throughput vs distance-to-target; 500-event window, per-peer EWMA corrections) blended with a literal external kNN crate (`renegade-ml`, 3-stage funnel, auto-selected k), weight ramping to a 0.5 cap. Score = expected total time with a 3× failure penalty. Always on; cold-starts on every restart.
- Production lesson embedded in a constant: the candidate window was raised 5→25 after telemetry showed **63% of failing GETs never visited any subscriber** — the window had been hiding reachable holders.

### 1.3 Transport (their weak flank)

- UDP, 1200-byte packets; X25519+ChaCha20-Poly1305 intro (port invisible to unsolicited probes), AES-128-GCM data phase. Authentication is **hop-by-hop only** — end-to-end integrity is delegated to contract validity predicates.
- Hole-punching by simultaneous open (200ms cadence, 3s deadline); the *introducer* is whichever peer routed the CONNECT — any ring peer, so public-IP gateways matter only for a node's very first join.
- **No relay path exists.** A code comment cites a measured **~84% hole-punch failure rate in production**; the uphill budget of 8 exists to convert that into ≈72% eventual join success by trying many acceptors. No symmetric-NAT handling, no TURN, no fallback: this is the architecture that forces their port-forwarding guidance — and caps their reachable audience.

### 1.4 State layer: contracts, mesh, leases

- **Contract ABI** is four WASM exports — validate, update (which *is* merge), summarize, get_delta — run under Wasmtime with a 256 MiB memory cap and epoch-based preemption (~5.1s); fuel metering exists but is off by default.
- **The runtime polices the algebra it cannot verify**: a 1/32-sampled re-apply probe plus a deterministic identical-input probe detect non-idempotent merges (comparing byte *multisets* to forgive HashMap reordering); a flagged contract has commits and all egress suppressed for 5 minutes, doubling to a 6-hour cap on re-detection, decaying when quiet.
- **Live propagation is merge-then-forward**, not per-hop summary/delta: updates push delta-or-state directly to neighbors that *advertised* co-hosting the contract (echo suppression + an originator covers-list). Advertisements are a 3-message protocol — add/remove announce, plus a full-snapshot exchange on connect and on a ~5-minute heartbeat, bounding staleness. Summary/delta reconciliation is a *separate anti-entropy protocol* (interest hashes first, then summaries, then targeted state-sync, storm-capped).
- **Interest-gated leases**: subscription lease 8 min, renewed every 2 min, but only while genuinely in use (local client subs OR downstream subscribers OR recent genuine client access — deliberately excluding the node's own upstream sub after a renewal-storm incident). Collapse = simply stop renewing; the demand path unwinds on its own. Anti-amplification: 512 downstream subs/contract; phantom-host repair bounded at ~32 min.
- **Hosting = cache, not durability**: budget min(RAM/8 clamped 128MiB–1GiB, disk-based clamp to 32GiB); eviction ranks by (client subs, downstream subs, genuine-access recency); a second cost-pressure axis sheds CPU/fan-out-hogging zero-demand contracts. Eviction deletes state+code from disk and retracts the advertisement; **no replacement is recruited — cold state can die network-wide**. They name this openly as unresolved.

## 2 · What River teaches (`river`)

River is their production chat app — the proof of what the contract model feels like to build on. Three designs and one pile of scar tissue are worth carrying home.

### 2.1 Membership without a live inviter

A room is a contract whose params hold only the owner's public key. Membership is an **invitation chain**: any member signs an `AuthorizedMember` for a newcomer; validity is a recursive signature walk up `invited_by` links to the owner — a pure function over bytes in state. **Joining requires nobody to be online**: redeem = GET room state from any replica, append a self-authorized delta (which may carry missing chain ancestors so it verifies standalone). The trade-offs they accepted and we should not: the invite is a **bearer credential containing the invitee's private key, minted by the inviter**; there is no expiry and no single-use enforcement; "revocation" is pre-banning the invite's known MemberId.

### 2.2 Removal as a derived view

Bans are a grow-only signed log; the removed-member set is **recomputed from converged state on every apply** — signature re-verified against current keys, authorization graded (owner / invite-subtree ancestor / deputy), then a cascade removal of the banned member's whole invite subtree. Deletion is never stored; it is derived. That, plus an idempotent `post_apply_cleanup` (they document why cap-then-enforce ordering must satisfy `cleanup(cleanup(S)) = cleanup(S)`), is what keeps moderation convergent under arbitrary delta ordering.

### 2.3 Bounded logs need retention-aware summaries

Messages are a bounded list (default 100) with deterministic oldest-eviction. Naive set-difference sync then re-offers pruned messages forever — which really happened: **63.7% of network-wide broadcast bytes** before they added a `RetentionHorizon` to the summary so a sender never offers what the receiver would immediately prune. Directly relevant the day we bound channel history.

### 2.4 The scar tissue

- **Determinism or die**: freenet-core byte-compares summaries for staleness; a HashMap in a summary made identical states look different → **~20M spurious anti-entropy heals**. Their rule now: BTree everything, and signed structs must re-serialize byte-identically forever.
- **Content-addressed code identity is a migration tax**: any WASM change re-keys the contract. They maintain registries of **31 legacy room-contract generations** (27 for delegates), probe newest-to-oldest for dormant rooms, forbid `git add -A` because WASM builds are non-reproducible (an accidental rebuild once broke room access; a wrong hash algorithm once lost user rooms). The saving grace — permissionless migration, any node can move fully-self-authorized state — is elegant, but the whole tax is optional if your code identity is versioned rather than hashed.
- **Platform races leak upward**: multiplexed WebSocket responses overtaking subscribe acks (299 failures in 4 hours against their flagship room), subscribe rejected when WASM isn't cached, freshly-PUT contracts briefly "not found". Every client grew classifier/retry armor. Multi-device is a manual armored key-export string.

---

# Part II · Evidence — Harmony baseline

## 3 · What Harmony actually has today (`harmony-client`)

Verified independently by the same method, so the comparison is symmetric.

- **Peer set** — Every device of every member of every joined community becomes a *persistent* dial target (address-book gossip → reachability resolver → reconnect supervisor). No degree bound anywhere; only concurrency caps (4 outbound dials; 8/source + 1024 global inbound handshakes). DM friends are deliberately outside the mesh (on-demand dials, deposit-only delivery).
- **Discovery** — Four pkarr record cases (invite / identity / community / friend) + per-community rendezvous beacons (max 4 advertiser slots). Steady state rides the encrypted community address-book channel; pkarr is the fallback. Strangers sharing no community cannot discover each other — by design.
- **Propagation** — One Zenoh session in peer mode over per-peer iroh QUIC links; scouting/gossip disabled (all peering app-directed). Channel logs heal via RBSR (range fingerprints, 16-element leaves, ≤32 rounds); membership/config state via debounced encrypted state-root full exchange. **Zenoh's peer-mode linkstate can multi-hop through intermediate peers today** — full mesh is our dial policy, not a hard constraint.
- **Island repair** — One mechanism (ZEB-824 gateway-dial driver), predicate = "any live session to any member". A two-island split reads Healthy on both sides; presence sweeps only re-arm already-known peers; and if all 4 rendezvous advertisers sit in one island, the other island can't find a bridge even when starved. Beneath the driver sits a pair-level trap: after 15 minutes of failed retries a peer slot goes **Dormant — never dialed again for process life** — and since pkarr stale-refresh fires only while dispatching a *Retrying* peer, a Dormant peer's address is never re-resolved either. Every revival trigger is edge-triggered (roster change, fresher remote record, drop, local address change) — a quiet, stable community keeps cross-island pairs Dormant indefinitely.
- **Content** — CID-addressed Zenoh queries, first reply wins; any holder serves public content; encrypted content gated by an in-memory per-node allowlist; validated downloaders join the swarm (ZEB-539); storage buddies = mutual dual-signed pledges with a budget-enforced pin planner. 100% pull.
- **Relay** — n0's stable iroh relay cluster by default (hot-swappable, ≤8 URLs); relay-vs-direct wholly iroh's choice, observed read-only; no self-hosted relay (deferred Phase 5+); known hazard: disjoint custom relay sets silently lose the relay path.

## 4 · Head-to-head

| Dimension | Freenet | Harmony | Edge |
|---|---|---|---|
| Topology formation | Structural: gap-targeted small-world ring, bounded 25–200, continuously maintained | Social: full mesh per community, unbounded degree | Freenet at scale; ours is fine ≤ low hundreds |
| Partition behavior | Emergent healing via churn; zero-conn escalation only | Zero-session escape hatch only; Healthy-if-any-member blind spot | both blind — open ground |
| Delivery model | Multi-hop routing + co-host mesh forwarding | Pairwise links only (linkstate capable, unused) | Freenet |
| NAT traversal | Hole-punch only; ~84% failure; no relay; port-forward culture | iroh: hole-punch + always-available relay fallback | Harmony, decisively |
| Transport auth | Hop-by-hop only; e2e pushed to app validity | End-to-end QUIC with peer identity keys | Harmony |
| Confidentiality | Contract state is public by construction | Epoch-encrypted communities, sealed DMs, encrypted content | Harmony, structurally |
| Membership/admission | River: offline signature-chain join, bearer invites, no single-use/expiry | Live-handshake join (ZEB-906/908 class), invitee-owned keys, single-use claims, typed errors | split — their liveness model, our credential hygiene |
| Identity & devices | Bearer keys; manual armored export between devices | Enrollment certs, vouching CRDT, quorum-signed revocation, device petnames/liveness | Harmony, by a wide margin |
| Replication discipline | Interest-gated leases; demand-proportional; self-collapsing paths | Ad-hoc: in-memory allowlists, explicit pledges, TTL docs | Freenet (the discipline, not the parameters) |
| Cold-state durability | None — evicted state can die network-wide (open problem) | Storage-buddy pledges, persisted pins, hosting receipts | Harmony |
| Code identity / upgrades | Content-addressed → re-key on every change; 31-generation migration registry | Versioned releases, signed auto-update | Harmony |
| Determinism testing | Turmoil-based deterministic simulation, seed-replayable multi-peer nets | Headless fleet e2e (real processes, real network) | complementary — study theirs |

---

# Part III · Verdicts

## 5 · What to take, what to leave

### ADOPT · R1 — Island-aware community health + coverage-based repair → **ZEB-910** (High)

Replace the gateway-dial driver's "any live member session ⇒ Healthy" predicate with a **membership-coverage measure**: of members with fresh reachability records, what fraction are live-or-recently-proven? A persistently unreachable *coherent subset* (while intra-subset gossip stays fresh on the other side) is the split signature. On suspicion: re-resolve rendezvous beacons, force pkarr refresh for the unreachable subset (bypassing the 24h/15min gates), and dial through the relay. The fix must also reach the pair-level trap underneath: **Dormant supervisor slots are never dialed and never pkarr-re-resolved again** — add a low-frequency Dormant "parole" tick (periodic, not edge-triggered) that re-arms a small batch of Dormant peers with a fresh record resolve, so recovery no longer depends on external churn or a restart. Two cheap hardenings ride along: raise/diversify the 4 rendezvous advertiser slots (select advertisers for network diversity so one island can't own all of them), and run beacon re-resolve as a slow steady-state tick rather than only on starvation — Freenet's core lesson applied where even Freenet didn't apply it. Precedent for the measurement principle: ZEB-805, where health had to be derived from merge *progress* rather than packet *arrival* — coverage should likewise count proven reachability, not the mere existence of sessions.

### ADOPT · R2 — Witnessed invite finalize: kill the live-inviter dependency → **ZEB-911** (High)

Keep everything we do better than River (invitee-minted keys, single-use claim materialization, expiry, typed redeem errors). Change one thing: redemption should be completable through **any reachable Joined member**, not specifically the inviter. Verification of an invite is already a pure signature check any member can perform; the missing piece is routing the finalize. Two composable options: (a) any member accepts the redeem package and relays it to the admin's deposit path (decouples admin liveness from inviter liveness — generalizes ZEB-254's offline counter-signer queue from "queue at admin" to "witness anywhere"); (b) admin-delegated counter-sign authority for designated members, for communities that opt in. Either way, the host-side PendingJoin re-promotion salvage (ZEB-906) becomes the backstop rather than the only path.

### ADAPT · R3 — Community mesh forwarding: delivery ≠ pairwise reachability → **ZEB-912**

Freenet's co-host mesh is merge-then-forward with echo suppression and an originator covers-list; anti-entropy is a separate heartbeat protocol. Harmony's equivalent: members forward channel events to members *they* can reach that the author's covers-list hasn't — RBSR already provides the dedupe/heal backstop, and CRDT merges make duplicate delivery harmless. Result: any connected *subgraph* delivers; an island becomes a latency problem instead of a partition. Sequencing note from the evidence: Zenoh peer-mode linkstate may provide part of this free once we stop dialing everyone — the investigation flagged that our full mesh masks whatever routing Zenoh would do on a sparser graph. Step 1 is a spike measuring linkstate behavior on a deliberately sparse fleet topology before building app-layer forwarding.

### ADAPT · R4 — Bounded-degree topology for large communities → **ZEB-914** (blocked by ZEB-912)

Full mesh is O(members²) connections community-wide and fine at our current size; it is not a thousand-member design. The transplant: per community, a bounded active set — nearest-K on an **identity-derived** ring (hash of stable node identity, *never* network address — deliberately avoiding Freenet's Sybil-grinding and CGNAT-collapse trade) plus a few gap-targeted long links, with protected lattice edges so the ring never fragments. Membership stays exact and CRDT-authoritative (unlike Freenet, we know the full roster — topology selection gets to be informed, not statistical). Only worth building once R3 makes sparse graphs deliver; until then the mesh is correct.

### ADOPT · R5 — Lease discipline for serving and pinning → **ZEB-913**

The pattern, not the parameters: replication effort should be *renewed by demonstrated demand and collapse by default*. Today our encrypted-content serve allowlist is in-memory and never expires; buddy pins persist without an in-use check; relay-holds have a flat 30-day TTL. Freenet's shape — short lease, cheap renewal gated on genuine use (with their hard-won exclusion: never let a node's *own* interest renew itself), collapse-by-non-renewal, anti-amplification caps, bounded phantom repair — is directly portable to all three, and would have prevented the class of bug where stale serving state outlives the intent that created it.

### STUDY · R6 — Three smaller mechanisms worth a ticket each

- **Adaptive dial ordering** → **ZEB-915**: per-peer observed-performance estimators (their isotonic + EWMA blend) applied to reconnect-supervisor scheduling and relay-vs-direct racing — and their #4222 lesson (a too-narrow candidate window silently hides reachable peers) applied to any K-limited selection we ever add.
- **Summary-first exchange with reverse delta** → **ZEB-916**: their PUT probe returns the holder's summary *and* a reverse delta — bidirectional convergence in one round trip. Our community state-root exchange (currently full-state on mismatch) could adopt this shape.
- **Deterministic simulation (Turmoil)** → **ZEB-917**: seeded, bit-replayable multi-node networks with fault injection (partitions, reorders, loss). Complements our real-process headless fleet; it is exactly the tool for testing R1's split-detection logic, which is painful to reproduce with real NATs.

### AVOID — Four things we should deliberately not import

- **Address-derived locations.** Sybil-grindable, and prefix-hashing collapses honest CGNAT/cloud cohorts to one point. We have real identities; any location we ever need derives from identity keys.
- **Bearer invites / inviter-minted keys.** No expiry, no single-use, whoever holds the link *is* the identity. Our claim-materialization model is strictly better; R2 takes their liveness property without their credential model.
- **Relay-free transport.** Their measured 84% hole-punch failure is the price; port forwarding as a user requirement is the audience ceiling. The iroh relay stays load-bearing (self-hosted relays remain the right Phase 5+ hedge).
- **Content-addressed code identity for app logic.** The 31-generation migration registry, non-reproducible-build landmines, and "wrong hash algorithm lost user rooms" incidents are the operating cost. Versioned, signed releases are the saner regime — reserve content-addressing for content.

## 6 · Where Harmony is simply ahead

Worth stating plainly, because the comparison is not one-directional: a real identity layer (enrollment certificates, multi-device vouching, quorum-signed revocation) versus bearer keys and manual export strings; end-to-end transport authentication versus hop-by-hop only; structural confidentiality (epoch-encrypted communities, sealed DMs) versus public-by-construction contract state; durable pledged storage versus cache-only hosting with network-wide cold-state death; single-use, expiring, typed-error invites; and a relay fallback their users pay for in port-forwarding friction. Freenet optimizes for a different goal — global, permissionless, publicly-addressable state. Harmony's communities are private and membership-gated; we don't need their global routing fabric. What we need from them is their *discipline*: topology as a maintained invariant, delivery through any connected subgraph, and replication effort that follows demonstrated demand.

## 7 · Coverage & sources

Whitepaper (paper-1, 614 lines of LaTeX) read in full directly. freenet-core and river explored by nine parallel code passes; harmony-client baseline re-verified by five more (peer-set formation, discovery, propagation, island risk, content, relay). Not covered: freenet-core's `topology-sim` crate internals (standalone research tool, not integrated in CI; judged low-value) and freenet-stdlib trait sources (not vendored locally; ABI reconstructed from the invocation side). One whitepaper claim was found to be aspirational: the documented "HTL>7 → random walk" behavior does not exist in code. Freenet constants cited (25/200 bounds, 5% band, uphill 8, HTL 10, 2/8-minute leases, RAM/8 hosting clamp, 5-minute→6-hour suppression ladder) were each verified at their definition sites.
