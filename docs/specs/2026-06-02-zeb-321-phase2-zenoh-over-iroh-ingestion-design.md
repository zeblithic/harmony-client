# ZEB-321 Phase 2 — iroh as a first-class Zenoh transport (cross-WAN sync)

- **Status:** design approved 2026-06-02
- **Spec B of the cross-WAN community arc** (Spec A = Phase 4 invite-only `generate_invite`, separate doc — ships first)
- **Unblocks:** ongoing cross-WAN CRDT sync after a join; completes [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) cross-device validation alongside Spec A
- **Builds on:** ZEB-321 Phase 1 (iroh endpoint + `IrohZenohLinkManager`/`IrohZenohLink` + accept loop + `ReachabilityResolver`, all shipped)

## Problem

ZEB-321 Phase 1 built the iroh transport plumbing but **never connected it to the running Zenoh session**. Inbound iroh→Zenoh links are accepted by the accept loop and then **discarded** by a drain task:

```rust
// lib.rs:2544-2552 — Phase 1 placeholder
// "ZEB-321 Phase 1: discarding inbound iroh/zenoh link (Zenoh session ingestion deferred to Phase 2)"
tokio::spawn(async move { while let Ok(_link) = new_link_rx.recv_async().await { /* drop */ } });
```

Root cause: `zenoh::open(config)` (zenoh 1.9.0) exposes **no public API to register a custom `LinkManagerUnicastTrait`**. `IrohZenohLinkManager` is fully implemented and holds a `NewLinkChannelSender`, but that channel was **hand-created** in `lib.rs:2528` — it is *not* the channel Zenoh's internal `TransportManager` owns and polls. So inbound links cannot reach the session, and outbound `iroh/<hex>` locators aren't routed. Cross-WAN community state sync therefore does not work even after a join.

## Approach — register iroh as a real Zenoh unicast transport

Make the **existing** Zenoh session own the iroh transport, so iroh is "just another link": inbound iroh links become real peer transports, and `session.open_link("iroh/<hex>")` routes through the already-built `IrohZenohLinkManager::new_link()` dialer. The community engines keep publishing to the one session unchanged; Zenoh simply has more links. This is the clean end state (vs. per-peer sessions or a loopback byte-bridge, which were the considered fallbacks).

## What already exists (Phase 1, reusable as-is)

`IrohZenohLink` (full `LinkUnicastTrait` over an iroh QUIC bidi stream, `zenoh_iroh_link.rs`); `IrohZenohLinkManager` (full `LinkManagerUnicastTrait`, `zenoh_iroh_transport.rs:129`) including a **complete outbound `new_link()`** that resolves a peer's iroh routing via `ReachabilityResolver` and dials `HARMONY_ZENOH_V1`; the accept loop (`spawn_accept_loop`); the boot-window handshake queue; the live `ReachabilityResolver` (fed by `ReachabilityAnnounce` CRDT deltas at `lib.rs:3427` + boot replay at `lib.rs:3915`). **The only missing pieces are the session-integration seam and the outbound driver.**

## Task 1 — de-risking spike: confirm the registration seam (the known-unknown)

The whole spec rests on getting `IrohZenohLinkManager` registered with Zenoh's `TransportManager`. Zenoh 1.9.0 has no documented public injection point, so Task 1 establishes the seam, trying candidates in order of cleanliness:

1. **`zenoh-transport` `TransportManager` builder.** harmony already depends on `zenoh-link` (it implements `LinkManagerUnicastTrait`). Check whether it can depend on `zenoh-transport` and construct/open the session via a `TransportManager` that has the iroh `LinkManagerUnicast` added — replacing bare `zenoh::open(config)` with the lower-level builder path (if `zenoh` re-exports or feature-gates it).
2. **Compiled-in transport plugin** via `zenoh-plugin-trait` (already a transitive dep, `Cargo.lock`): register the iroh transport through Zenoh's plugin/config initialization before `open()` completes, so Zenoh hands the manager its own internal `NewLinkChannelSender`.
3. **Blocked at 1.9.0 → escalate.** If neither in-process path is reachable, stop and re-evaluate the **per-peer-session fallback** (open a `zenoh::Session` over each iroh connection + bridge pub/sub into the engines) before proceeding. The fallback is a materially different spec, so this gate prevents discovering the blocker mid-build.

**Deliverable:** the concrete registration call site + which candidate worked. Everything below assumes Task 1 succeeded with candidate 1 or 2.

## Task 2 — inbound ingestion (delete the drain)

Once registered, **Zenoh's `TransportManager` supplies its own `NewLinkChannelSender`** to `IrohZenohLinkManager` at construction (replacing the hand-created channel at `lib.rs:2528`). The accept loop already sends each established `IrohZenohLink` into that sender — so it now lands in Zenoh's transport-accept queue. **Delete the `lib.rs:2544-2552` drain task** and the manual channel. Inbound iroh links become live peer transports carrying state-root pub/sub.

## Task 3 — outbound link driver

`new_link()` exists but nothing calls it for peers, and the manager wasn't registered. Add a small event-loop task:
- Trigger: `ReachabilityResolver` updates (the `connectivity-reachability-changed` signal already fires from the delta consumer at `lib.rs:3427`).
- Action: for each peer in communities I belong to, if no live Zenoh link to their iroh `NodeId` exists, `session.open_link("iroh/<hex node id>")` (routes through the now-registered `new_link()`). De-dup against existing links; back off + retry on dial failure (reuse the iroh dial timeouts).
- This closes the loop: discovery (resolver) → dial (new_link) → Zenoh peer link → CRDT sync.

## Task 4 — locator protocol + lifecycle

- Register the `iroh/<64-hex>` locator protocol so Zenoh's config/locator parser accepts and dispatches it to the iroh manager (part of Task 1's registration, called out separately because the locator scheme must match what `new_link()` already parses — a 64-char hex `EndpointId`).
- Lifecycle: give `del_listener` / link teardown real behavior — close iroh QUIC streams cleanly on session shutdown and when a peer drops out of the resolver, so links don't leak.

## Error handling

- Spike escalation (Task 1 candidate failure) → hard stop + documented fallback decision, not a silent partial wiring.
- Outbound dial failure (peer unreachable / pkarr miss) → logged, backed-off retry; never blocks the session or LAN links.
- A malformed `iroh/<hex>` locator → reject at parse (existing `new_link` behavior), surfaced as a Zenoh link error.

## Testing (closes a real coverage gap)

Today the only iroh-transport test drives the link trait directly (`paired_stream_roundtrip_via_loopback`); **nothing exercises `zenoh::open` + the iroh transport + CRDT pub/sub end-to-end.** Spec B adds:
- **Registration test:** a session opened via the Task 1 seam reports the iroh transport/listener in its locator set.
- **Inbound:** an inbound `IrohZenohLink` handed to the registered manager surfaces as a Zenoh peer transport (not dropped).
- **Two-node, LAN-disabled integration test (the acceptance test for the arc):** nodes A and B with LAN/mDNS scouting disabled, `ReachabilityResolver` seeded with each other's iroh routing; A `open_link`s B over iroh; a community state-root published on A merges on B (and vice-versa). This is the proof that cross-WAN sync works.

## Relationship to Spec A & sequencing

Spec A (invite-only generate) makes a cross-WAN **join** succeed (snapshot embedded in the invite + iroh-handshake countersign). Spec B makes **ongoing** messages flow afterward. Ship order is forced: **A → B**. Together they close the cross-WAN community loop for ZEB-330. DM-over-iroh remains a separate, later track.

## Out of scope

- DM transport migration to iroh (DMs stay on Reticulum; separate track).
- pkarr discovery changes (cases A/B/C already shipped in ZEB-323).
- The per-peer-session / loopback-bridge fallbacks (only revisited if Task 1 escalates).
