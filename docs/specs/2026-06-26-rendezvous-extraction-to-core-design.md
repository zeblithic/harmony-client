# DHT-rendezvous primitive: extraction to core Harmony — design

> **Status:** DRAFT for review. Tracks [ZEB-571](https://linear.app/zeblith/issue/ZEB-571) Tier-1 item 1 (flagship extraction). No code changed yet. Cross-repo (`harmony` core + `harmony-client`).

## Goal

Lift the generic **DHT-rendezvous resolve kernel** out of `harmony-client` into core Harmony, so any P2P app — and both of the client's existing consumers — can do *"find a live serving peer for a topic via signed DHT slots derived from a shared key"* without re-deriving the subtle parts (escalating concurrent probe, first-responder-wins, hung-probe immunity, per-batch deadline, freshness-after-await).

The rendezvous mechanism is **already instantiated twice** in the client (the textbook signal it wants to be shared substrate):

1. **Community open-join** (ZEB-570) — `community_rendezvous.rs` + `community_rendezvous_publisher.rs`
2. **Friend Case-D** (ZEB-371) — `friend_rendezvous.rs`

Core has the building blocks (`derive_ephemeral_key`, `PkarrResolver`, `epoch_tolerance_window`, `PkarrPublisher`) but **zero** notion of enumerated slots, slot assignment, or escalating multi-key resolve.

## The constraint: the two consumers are NOT the same shape

This is the load-bearing design fact — any core primitive must fit **both**, so the generic kernel is narrower than the full community machinery.

| Axis | Community open-join | Friend Case-D |
| --- | --- | --- |
| `ikm` | community `epoch_key` (`PkarrCase::Community`) | per-friendship ECDH secret (`PkarrCase::Friend`) |
| Slot multiplicity | **N enumerated** slots (`N = COMMUNITY_RELAY_ADVERTISERS_MAX = 4`) | **1 slot per direction** |
| Slot selector | rank in CRDT-replicated advertiser set | the peer's `owner_id` |
| `info` layout | `"harmony.rendezvous.v1" ‖ slot_u16_be ‖ epoch_u64_be` | `epoch_u64_be ‖ owner_id_16` |
| Resolve strategy | escalating-batch over slots `0..N` (curve `[1,2,N]`) | single direct resolve |
| Payload auth | BEP44 envelope only (joiner doesn't know beacon identity) | BEP44 envelope **+** ChaCha20Poly1305 seal to the friendship secret |

What's shared between them — and therefore generic — is: *derive a signing key per (slot, epoch) from a shared secret under a `PkarrCase` + `info`; publish reachability under it; resolve it (possibly across several enumerated slots, concurrently, first-live-wins) within the epoch-tolerance window, re-sampling freshness after the await.* The single-slot friend case is just the degenerate `N=1`, curve `[1]` instance of the community case.

## What is already in core (do NOT move — the kernel builds on it)

All in `harmony-pkarr`:
- `derive_ephemeral_key(case: PkarrCase, ikm: &[u8], info: &[u8]) -> SigningKey` — HKDF-SHA256 slot-key derivation. **The keying already lives in core**; this extraction moves only the *driver* on top, so no key bytes change.
- `PkarrResolver::resolve(&vk)`, `PkarrPublisher::register/unregister`, `PkarrRoutingRecord::sign_new`, `current_epoch_id`, `epoch_tolerance_window`, `RecordBuilder`/`EphemeralKeyBuilder` aliases.

## What to extract (the generic kernel)

A new module **`harmony-pkarr::rendezvous`** (recommended) — see "Crate vs module" below. Generic over the resolved payload type `P`.

```rust
// harmony-pkarr/src/rendezvous.rs

/// Probe one rendezvous slot at one epoch. Returns Some(P) only for a live,
/// freshness-valid record. The pkarr impl derives the slot verifying-key and
/// queries the DHT; tests inject a deterministic stub.
#[async_trait::async_trait]
pub trait SlotResolver<P> {
    async fn resolve_slot(&self, slot_index: u16, epoch_id: u64) -> Option<P>;
}

pub struct RendezvousResolveConfig {
    pub batch_curve: Vec<usize>,      // e.g. [1,2,N] (community) or [1] (friend)
    pub per_batch_deadline: Duration,
}

#[derive(Debug, Default)]
pub struct RendezvousResolveOutcome<P> {
    pub payload: Option<P>,
    pub winning_slot: Option<u16>,
    pub elapsed_ms: u64,
    pub batches_tried: usize,
}

/// Escalating-batch resolve: for each width w in `cfg.batch_curve`, probe slots
/// 0..w across the epoch-tolerance window CONCURRENTLY, return on the FIRST live
/// record (first-responder-wins, hung-probe-immune), bounded per batch by
/// `cfg.per_batch_deadline`. `now_ms` injected so the driver stays clock-free.
pub async fn resolve_rendezvous_with<P, R: SlotResolver<P> + Sync>(
    resolver: &R,
    now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> RendezvousResolveOutcome<P>;

/// The pkarr-backed SlotResolver, generic over a payload decoder so the app
/// keeps its own wire type (ReachabilityAnnouncePayload / sealed friend blob).
pub struct PkarrSlotResolver<P, F: Fn(&[u8]) -> Option<P>> {
    pub pkarr: Arc<PkarrResolver>,
    pub case: PkarrCase,
    pub ikm: Vec<u8>,                 // epoch_key (community) or friendship secret (friend)
    pub info_for: Arc<dyn Fn(u16, u64) -> Vec<u8> + Send + Sync>,  // app supplies the info layout
    pub decode: F,                    // app supplies routing_blob -> P (ciborium / unseal)
}
// resolve_slot: derive vk via derive_ephemeral_key(case, ikm, info_for(slot,epoch)),
//   pkarr.resolve(vk), re-sample SystemTime::now() AFTER the await (PR#306 fix),
//   verify_freshness(now), decode(routing_blob).

/// Pure slot-assignment helper (genuinely generic — rank in a sorted set).
/// Optional: apps that assign by rank (community) use it; owner-id-keyed apps
/// (friend) don't. The *source* of the advertiser set stays app-side.
pub fn slot_for_advertiser<A: Ord + Copy>(advertisers: &[A], me: &A, cap: usize) -> Option<u16>;
```

The env-var config + width-clamping (`HARMONY_OPEN_JOIN_RESOLVE_*`) moves with `RendezvousResolveConfig` (or stays a client convenience that builds the core config — decision for Jake; leaning: keep the env names app-side, since they're open-join-flavored).

## What stays app-specific (do NOT move)

- **`RENDEZVOUS_INFO_PREFIX` + the community info layout** (`PREFIX ‖ slot ‖ epoch`) and the friend layout (`epoch ‖ owner_id`) — each app supplies its own `info_for` closure. The *length-disjointness* guarantee (rendezvous keys can't alias the 72-byte member-keyed records) is a community-specific property and its proof-test stays client-side.
- **`slot_for_advertiser`'s data source** — the advertiser set comes from the community membership CRDT (ZEB-380). The *ranking function* is generic (offered as the optional helper above); the wiring is not.
- **`CommunityRendezvousPublisher::refresh_slot` + `RendezvousSink`** — the register/unregister-on-rank-change lifecycle. The `RendezvousSink`-over-`PkarrPublisher` seam is generic, but the slot-handle naming + advertiser-set integration is community policy. **Deferred to a follow-up extraction** (more entangled; lower marginal value than the resolve driver). See "Phasing".
- **`should_self_promote` + `RendezvousObservability` + `PowerTier`** — power-aware fill-in election + counters; tied to community semantics, and the self-promotion *driver* is itself a deferred follow-up. Leave in client.
- **Payload types** — `ReachabilityAnnouncePayload` (community) and the sealed Case-D blob (friend) are app wire types; the kernel is generic over `P`.

## Invariants to preserve (pin with tests moved into core)

1. **Key bytes unchanged.** Derivation already lives in core; moving only the driver cannot change keys. Move the friend reference vector (`case_d_reference_vector` → `cd972d4f9b4dc0…`) and the community determinism + member-key-disjointness tests into the core crate's suite so any drift is caught.
2. **Behavior unchanged.** First-responder-wins; one hung/slow probe never stalls discovery (`hung_probe_does_not_block_a_live_higher_slot`); per-batch deadline widens rather than hangs; freshness re-sampled *after* the await (PR#306 stale-clock fix); width-clamping to `1..=N`.
3. **Cross-WAN integration green.** The ZEB-570 `community_open_join_cross_wan_integration` suite must pass unchanged against the core-backed kernel — it's the end-to-end proof.

## Phasing (each step independently shippable)

1. **Core PR (`harmony`):** add `harmony-pkarr::rendezvous` — `SlotResolver<P>`, `resolve_rendezvous_with`, config, outcome, `PkarrSlotResolver<P, F>`, the pure `slot_for_advertiser`. Port the existing unit tests (genericized over `P`) + add a friend-shape test (1-slot curve resolves a Case-D slot, proving the kernel fits both shapes).
2. **Client PR (`harmony-client`):** bump the `harmony-pkarr` git-rev; reduce `community_rendezvous.rs` to the community `info_for` layout + `ReachabilityAnnouncePayload` decoder over the core kernel; delete the duplicated driver. Keep `RENDEZVOUS_INFO_PREFIX`, the disjointness proof, and the publisher lifecycle client-side.
3. **(Fast-follow, optional)** Converge `friend_rendezvous.rs` Case-D resolve onto the kernel (curve `[1]`, friend `info_for`, unseal decoder), removing its bespoke single-resolve path.
4. **(Deferred)** Extract the publisher slot-claim lifecycle (`refresh_slot` + `RendezvousSink`) as a generic `RendezvousPublisher` once the community advertiser-set wiring has a clean seam.

Merge order: core first, then the client rev-bump.

## Open decisions for review

1. **New crate `harmony-rendezvous` vs module `harmony-pkarr::rendezvous`?** Recommend the **module**: the kernel depends only on harmony-pkarr's own primitives (derive/resolve/epoch/publisher) plus `futures` + `tokio::time` (pkarr already pulls tokio) + `async-trait`; a module avoids crate proliferation and a new dep edge. New crate is the alternative if we want to keep harmony-pkarr's surface minimal or expect non-pkarr rendezvous backends later.
2. **Scope now:** just the resolve driver + config/outcome + pure `slot_for_advertiser` (minimal, highest-value), deferring the publisher lifecycle? Recommend **yes** — the resolve driver is the flagship; the publisher half is more entangled.
3. **Friend convergence now or later?** Recommend **make the kernel fit it + add the proving test now**, actual `friend_rendezvous.rs` convergence as the fast-follow (step 3).
4. **Env-var config ownership** — keep `HARMONY_OPEN_JOIN_RESOLVE_*` names client-side (open-join-flavored) building a core `RendezvousResolveConfig`, or move them to core with neutral names? Leaning client-side.

## Scope note

Internal architecture refactor; **no user-facing behavior change**. Not urgent vs. alpha validation — sequenced for when there's appetite (ZEB-571). Good iron-is-hot timing since ZEB-570 just shipped the second instantiation.
