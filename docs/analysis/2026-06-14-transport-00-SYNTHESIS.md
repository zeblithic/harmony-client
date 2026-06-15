# Harmony transport/network forensic synthesis (2026-06-14)

Synthesis of five read-only forensic streams (companion docs `…-01`…`…-05` in this dir).
Goal: one cohesive story of what our network/transport layers actually ARE, what works,
what's a mirage, and how to coalesce — per Jake's 2026-06-14 directive.

---

## The one-paragraph thesis

**We have two parallel transport stacks. The one in `harmony-client` works and is two-node
proven (co-located). The one in `harmony` core (the `harmony-node` event loop + the Reticulum
router + the tunnel-as-Reticulum-interface) is built from real pieces but was never driven to a
working two-node demonstration, is frozen, and the client does not use it.** The client embeds
core's `harmony-runtime` only as a *sans-I/O brain* (4 type names) and supplies its own real I/O:
iroh QUIC + a **vendored `zenoh-link` fork** that gives us genuine **Zenoh-over-iroh** + first-contact
ALPN acceptors + CAS. The confusion, the wild-goose-chases, and the ZEB-461 dead end all trace to
treating core's frozen/inert transport surface as if it were live. The fix is to **declare the
client the single source of truth for transport, deliver DMs over its working iroh layer, and tear
out the core/Reticulum parallel stack.**

---

## 1. What actually works (the keeper)

The **client's** transport stack, all wired into the headless `serve`/`start_node` path:

- **iroh QUIC link layer** — real `iroh::Endpoint`, 8 ALPNs, dial driver + 5 first-contact acceptors
  (invite / friend / pex). Carries bytes between two real nodes (proven by
  `community_reachability_two_engine_integration.rs` + loopback-QUIC tests).
- **Zenoh-over-iroh** — a **vendored `zenoh-link` fork** (`Cargo.toml:226` patch) that teaches zenoh's
  closed link dispatch about `iroh/<hex>` locators, plus a local `IrohZenohLinkManager` + registration
  glue. **This is the realized "global namespace pub/sub over iroh" value prop** — and it exists *only*
  in the client. (It's how we beat the zenoh-1.x "no custom-transport seam" wall that blocked the
  in-core approach.)
- **Harmony CAS** — `RuntimeContentStore` adapts core's `StorageTier` (clean reuse, not duplicated);
  serve + fetch are two-node proven.
- **e2e hard-asserts (two real `serve` processes, co-located):** S1 invite first-contact + roster CRDT
  sync both ways; S3 offline-channel catch-up on reconnect; S4 single-node restart durability. These are
  the genuine "this works end-to-end" evidence.

Clean reuse from core (keep as-is): **identity, owner-state CRDT, pkarr discovery, CAS/StorageTier.**

## 2. What's a mirage (the confusion sources)

- **harmony-node's transport event loop is effectively dead.** Real zenoh+iroh code, but a *divergent*
  design (plain Zenoh + Reticulum-over-iroh tunnels), **nothing depends on it**, and it's **frozen since
  ~2026-05-03** (last change = a tree-wide `cargo fmt`). Users run `harmony-app` (the client), not this.
- **The runtime's peer/tunnel action surface is inert in the client.** The embedded `NodeRuntime` emits
  `RuntimeAction::{InitiateTunnel, SendPathRequest, CloseTunnel, ReplicaPush, …}`; the client's dispatcher
  has **no arm** for them and swallows them via `_ => {}`. **8 of 23 emitted actions are handled.** This
  is the exact mechanism behind the ZEB-461 dead end: the runtime "decided to dial a tunnel," the client
  dropped the decision, the tunnel never dialed.
- **"Zenoh-over-iroh in core" does not exist.** In core, zenoh runs over its own stock TCP transport, the
  iroh tunnel is independent, and zenoh-frames-over-tunnel is a TODO. (Also: the `harmony-zenoh` crate has
  *no zenoh dependency* — it's a sans-I/O helper. Big naming-confusion source.)
- **Core has zero two-node integration proof.** Every "two-node" test in core is an in-memory hand-bridge;
  nothing stands up two sockets/QUIC endpoints. The pieces are rigorous (esp. crypto) but unassembled.
- **The best-built core piece is the `harmony-tunnel` crate** — a *complete, PQ* (ML-KEM-768 + ML-DSA-65 +
  HKDF + AEAD) iroh-QUIC session with a passing two-side handshake test. But it's only driven by
  harmony-node (dead) and only ever carries CRDT-replication frames, never DM bodies. **It is unused, not
  broken** — and it's a strong asset (see §5).

> **Cross-check correction.** The client-stream agent reported S2 "hard-asserts DM bytes round-trip and is
> proven." That is **wrong** — it read the *uncommitted* S2 hard-assert edit + the wired acceptor code and
> assumed it passes. I **ran** S2 against a freshly-built binary: it **FAILS** (tunnel never dials → every
> DM packet dropped `destination_hash unknown`). The other three streams + the empirical run agree DM
> delivery does **not** work in the client. Static "it's wired" ≠ "it runs." Trust the run.

## 3. Reticulum: large by line-count, tiny by dependency, nearly dead by usage

- `harmony-reticulum` ≈ 14.2k LOC (real RNS-binary-compatible router) but only **two** reverse deps
  (harmony-node, harmony-runtime); the client depends on it only **transitively**.
- **The only live user-facing consumer is DM unicast — and it's egress-broken in the client**: the
  dispatcher discards the resolved interface and always LAN-UDP-broadcasts (`event_loop.rs:5725`); off-LAN
  it silently drops at the path-table miss. So Reticulum DMs can't even reach an off-LAN peer.
- The runtime's Reticulum router runs **leaf-mode only** (never `new_transport()`), so its tested multi-hop
  relay code is dead at runtime.
- DMs already have a **non-Reticulum durability fallback** (butler / community-relay CAS deposit,
  ZEB-418/458) — this de-risks removing the Reticulum egress.
- **`OwnerDeviceCache` is salvageable / transport-agnostic.** It's an `OwnerAddr → [DeviceIdentityHash]`
  directory (hash = `SHA256(x25519_pub || ed25519_pub)[:16]`), populated over the **iroh friend handshake**
  (ZEB-461), CRDT-replicated. The *only* Reticulum-coupled line is `compute_dm_destination_hash`
  (`dm_signing.rs:243`), which prepends `"harmony.dm"` to make an RNS dest hash. **Swap that one function
  and the entire ZEB-461 directory carries over to an iroh/Zenoh DM path unchanged.**
- **Keep on teardown:** the device-address-hash *formula* in `identity.rs` (it's the canonical device ID,
  used everywhere) and `harmony-identity/tests/reticulum_interop.rs` (validates shared crypto, not the
  protocol). Delete the protocol/carrier, the udp0/l2/tunnel-as-Reticulum interfaces, `HARMONY_RETICULUM_PORT`,
  and the dead `TransportBinding::Reticulum` annotations.

## 4. PQ vs Curve25519: the fluctuation, explained

- **Two complete parallel identity systems**, both minted from one seed every boot: classical
  `Identity` (X25519 + Ed25519) and `PqIdentity` (ML-KEM-768 + ML-DSA-65). **Canonical = classical** — the
  node address used for DMs, friend rendezvous, mail, pkarr, (and Reticulum) is the Ed25519/X25519 hash.
- **PQ is used for real in exactly two places:** the `harmony-tunnel` session (genuinely PQ, no classical
  keys) and the **KEL** (ML-DSA signs every entry). Discovery/profile *verification* is suite-aware and can
  check ML-DSA.
- **Everywhere a user actually talks, PQ is minted-and-ignored.** The clearest example: **every DM is sealed
  X25519-ECDH + ChaCha20 and signed Ed25519** (`dm_crypto.rs:57`, `dm_signing.rs:50`) — even though both
  peers minted *and advertised* ML-KEM/ML-DSA in the friend handshake. And the one PQ tunnel that exists
  only carries CRDT replication, never the DM body — *and that tunnel is the inert path the client drops.*
  Net: **in the client's live paths, the only real PQ usage is KEL signing; conversational crypto is fully
  classical.** That is the "defaulting back to Curve25519" you felt.
- **Fluctuation hotspots:** `hybrid_kem.rs` (a correct, tested X25519‖ML-KEM combiner **wired in nowhere**);
  owner `PubKeyBundle.post_quantum` always `None`; **pkarr is hard-blocked** — its inner record is a fixed
  64-byte classical layout that *cannot* hold a 1952-byte ML-DSA key (the one true blocker for any
  PQ-*discovery* story); two different-crypto-class tunnels serve the same peer split by payload type.
- **Distance to a consistent story:** ~40% toward hybrid. It's *wiring + a pkarr v2 wire format*, not new
  crypto.

## 5. The coalescence plan

Theme: **client = single source of truth for transport. Deliver DMs over iroh. Retire the core/Reticulum
parallel stack. Make a deliberate PQ decision.**

**Move 1 — DM-over-iroh (the real ZEB-461 fix + cross-WAN DM DoD).** Deliver sealed DM unicast over the
client's working iroh layer using the peer `node_id` the friend handshake already learns (ZEB-461 Tasks
5–7). Two sub-options:
  - **1a (elegant, PQ-aligned):** repurpose the **`harmony-tunnel`** PQ session (ML-KEM + ML-DSA + AEAD over
    iroh QUIC) as the DM carrier — driven *from the client* (add the `InitiateTunnel` consumer to the client
    event loop using the client's iroh endpoint), routing DM **bodies** (not just replication frames). This
    delivers DMs **and** makes them genuinely PQ in one move, reusing our best-built core asset. It's the
    ~3,300-line integration I flagged as the ZEB-461 blocker — reframed from "support Reticulum" to "drive
    the PQ iroh tunnel from the client."
  - **1b (minimal):** a plain DM ALPN over iroh (classical seal, as today), bytes direct. Smaller; doesn't
    advance PQ.

**Move 2 — Reticulum teardown** (after Move 1 lands a DM path): delete the `harmony-reticulum` carrier,
prune the runtime Reticulum router + `SendUnicastToDevice` + udp0/l2 interfaces + `HARMONY_RETICULUM_PORT` +
dead annotations. Keep the device-hash formula + the interop crypto test.

**Move 3 — retire core's dead transport**: down-rank/remove harmony-node's frozen event loop; prune the
runtime's inert action surface (the 15-of-23 unhandled actions). Optionally back-port the client's
Zenoh-over-iroh + acceptor design into a shared crate so there's one transport home.

**Move 4 — PQ decision (deliberate, your call):** either **(i) "classical now, PQ later"** — accept classical
conversational crypto, stop minting/threading PQ keys we don't use, keep KEL-PQ; or **(ii) "hybrid
everywhere"** — wire the existing `hybrid_kem.rs` into DM/rendezvous/link seal + ML-DSA co-sigs, and solve
the pkarr fixed-record blocker (v2 wire format). Move 1a is a natural down-payment on (ii).

## 6. ZEB-461, reframed and de-risked

The literal bug ("OwnerDeviceCache unpopulated → DM not exercisable") is fixed by **Tasks 5–7** (handshake
carries + learns peer device bundle + iroh node_id + PQ keys → directory populated), which **Stream 4
confirms is transport-agnostic and survives the Reticulum teardown.** Land Tasks 5–7 as the foundational
**peer-reachability-exchange primitive** (decoupled, no harmony#278, no new Reticulum coupling). Drop Tasks
8–9 (Reticulum tunnel registration). Revert S2 to an honest characterization: friendship + DM-space +
`send_dm` accepted are real; byte-delivery awaits Move 1. This primitive is exactly what Move 1 consumes.

## Decisions needed from Jake

1. **DM transport:** Move **1a** (repurpose the PQ `harmony-tunnel` over iroh — delivers DMs *and* PQ) vs
   **1b** (plain classical DM-over-iroh ALPN — smaller).
2. **Reticulum:** confirm teardown (Move 2) — yes, and on what timeline relative to Move 1.
3. **PQ:** Move 4 **(i) classical-now** vs **(ii) hybrid-everywhere**. (1a is a down-payment on ii.)
4. **ZEB-461:** OK to land Tasks 5–7 now as the reachability primitive (recommended), with S2 honestly
   characterized?
