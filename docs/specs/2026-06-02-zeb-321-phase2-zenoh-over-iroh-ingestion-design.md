# ZEB-321 Phase 2 — iroh as a first-class Zenoh transport (cross-WAN sync)

- **Status:** design approved 2026-06-02; **revised 2026-06-04** after the Task-1 de-risking spike (see revision note)
- **Ticket:** [ZEB-368](https://linear.app/zeblith/issue/ZEB-368)
- **Branch:** `zeb-368-register-iroh-zenoh-transport` (off `origin/main` `2cc2b35`)
- **Spec B of the cross-WAN community arc** (Spec A = Phase 4 invite-only `generate_invite`, shipped — ZEB-367 PR #184)
- **Unblocks:** ongoing cross-WAN CRDT sync after a join; completes [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) cross-device validation alongside Spec A
- **Builds on:** ZEB-321 Phase 1 (iroh endpoint + `IrohZenohLinkManager`/`IrohZenohLink` + accept loop + `ReachabilityResolver`, all shipped)

> **Revision note (2026-06-04).** The original spec proposed registering `IrohZenohLinkManager`
> with Zenoh's `TransportManager` via one of two in-process seams (candidate 1: a `TransportManager`
> builder / `zenoh::init`; candidate 2: a transport plugin). The Task-1 de-risking spike **proved
> both impossible at zenoh 1.9.0** — Zenoh has no public *or* `#[internal]`-gated API to register a
> custom unicast transport, and its locator→transport dispatch is a **closed `LinkKind` enum** that
> rejects any unknown scheme before a manager ever runs. Jake chose the **vendored-fork** path. This
> document is rewritten around that approach, whose feasibility is now **empirically proven** (see
> "Spike result"). See also `reference_zenoh_no_custom_transport` in agent memory.

---

## Problem

ZEB-321 Phase 1 built the iroh transport plumbing but **never connected it to the running Zenoh
session**. Inbound iroh→Zenoh links are accepted by the accept loop and then **discarded** by a
drain task:

```rust
// lib.rs:2543 — Phase 1 hand-created channel (NOT Zenoh's)
let (new_link_tx, new_link_rx) = flume::unbounded::<zenoh_link::LinkUnicast>();
// lib.rs:2546 — manager built with that hand-created sender
let link_mgr = IrohZenohLinkManager::new(/* ep */, /* resolver */, new_link_tx);
// lib.rs:2559-2560 — Phase 1 placeholder: the accept loop's output is dropped
iroh_inbound_drain_handle = Some(tokio::spawn(async move {
    while let Ok(_link) = new_link_rx.recv_async().await { /* drop */ }
}));
// lib.rs:2575 — accept loop runs but feeds the drain
iroh_accept_handle = Some(link_mgr.spawn_accept_loop());
```

Root cause: `zenoh::open(config)` (zenoh 1.9.0) exposes **no public API to register a custom
`LinkManagerUnicastTrait`**. `IrohZenohLinkManager` is fully implemented and holds a
`NewLinkChannelSender`, but that channel was **hand-created** — it is *not* the channel Zenoh's
internal `TransportManager` owns and polls. So inbound links cannot reach the session, and outbound
`iroh/<hex>` locators aren't routed. Cross-WAN community state sync therefore does not work even
after a join.

---

## Spike result — why we fork `zenoh-link` (the proven facts)

The spike read the zenoh 1.9.0 registry source and built a proof-of-concept. Findings (each with a
source citation, all verified):

1. **Locator dispatch is a closed enum.** `zenoh-link 1.9.0` `LinkKind`
   (`src/lib.rs:84-96`) has variants only for tcp/udp/tls/quic/ws/serial/unixpipe/vsock.
   `LinkKind::try_from` (`lib.rs:136-189`) ends in `_ => bail!("Unicast not supported for {}
   protocol")`. An `iroh/<hex>` locator is rejected **there**, before any link manager runs, on both
   the accept and `open_link` paths.
2. **No injection seam.** `zenoh::open` builds the `TransportManager` privately inside
   `RuntimeBuilder::build()`; neither it, `TransportManagerBuilder`, nor `TransportManager` exposes
   any "add a custom `LinkManagerUnicast`" method, and the live managers map is `pub(super)`, filled
   only by the closed `LinkManagerBuilderUnicast::make`. `#[zenoh_macros::internal]` makes
   `zenoh::init`/`RuntimeBuilder` *reachable* but opens no link-registration seam. The plugin trait
   (`zenoh-plugin-trait`) has zero transport/link hooks.
3. **The fork works — proven empirically.** `zenoh-link` is a **single-file crate** (`src/lib.rs`).
   A `[patch.crates-io] zenoh-link = { path = "vendor/zenoh-link" }` replaces it **graph-wide** —
   `cargo tree` confirms `zenoh-transport`'s own edge resolves to our vendored copy, so
   *zenoh-transport's own dispatch compiles against and runs our code*. We don't inject a manager;
   **we become the dispatch.**
4. **The fork stays a single crate.** Every `LinkKind` consumer *outside* `zenoh-link` (in
   `zenoh-transport`) uses it only as a `HashMap` key / `Vec` element or as the output of
   `try_from`/`new_supported_links` (both in the crate we patch). There is **no exhaustive `match`
   on `LinkKind` variants anywhere but `zenoh-link` itself**, so adding `LinkKind::Iroh` cannot
   cascade compile errors into any crate we don't already own.
5. **No dependency cycle.** The patched `make` can't import harmony types (cycle). Instead the
   vendored crate exposes a process-global **factory `OnceLock`**; harmony-app registers a closure
   (capturing its iroh `Endpoint` + `ReachabilityResolver`) *before* `zenoh::open`, and the patched
   `make` calls it. zenoh-link depends on nothing of harmony's.

The PoC (`vendor/zenoh-link/`, **already on the branch**) adds the `Iroh` variant + the factory and
**compiles clean** (`cargo build -p zenoh-link` → `Finished`, exit 0). It is the real Task-1
deliverable, not throwaway.

---

## Approach — vendored fork of `zenoh-link` that teaches Zenoh the `iroh` scheme

Keep the **one existing Zenoh session**; make iroh "just another link." A vendored `zenoh-link`
fork adds `iroh/<hex>` to Zenoh's own closed dispatch, routed to a harmony-registered factory that
returns the already-complete `IrohZenohLinkManager`; a forwarder feeds accepted inbound links into
**Zenoh's own** `NewLinkChannelSender`. Inbound iroh links then surface as real Zenoh peer
transports; outbound peers seeded into `connect/endpoints` route through the existing `new_link()`
dialer. This is the clean end state the original spec wanted (vs. per-peer sessions or a loopback
byte-bridge, the considered fallbacks).

## What already exists (Phase 1, reusable as-is)

`IrohZenohLink` (full `LinkUnicastTrait` over an iroh QUIC bidi stream, `zenoh_iroh_link.rs`);
`IrohZenohLinkManager` (full `LinkManagerUnicastTrait`, `zenoh_iroh_transport.rs:129`) including a
**complete outbound `new_link()`** that resolves a peer's iroh routing via `ReachabilityResolver`
and dials `HARMONY_ZENOH_V1`; the accept loop (`spawn_accept_loop`); the boot-window handshake
queue; the live `ReachabilityResolver` (fed by `ReachabilityAnnounce` CRDT deltas + boot replay).
**The only missing pieces are the session-integration seam (now the fork) and the outbound driver.**

---

## Task 1 — vendored `zenoh-link` fork + iroh dispatch + factory seam  *(DONE — spike artifact)*

In `src-tauri/vendor/zenoh-link/` (verbatim copy of `zenoh-link` 1.9.0, version kept at `1.9.0` so
`[patch]` satisfies zenoh/zenoh-transport's `=1.9.0` pin), with `[patch.crates-io]` in
`src-tauri/Cargo.toml`. Eight coordinated additions teach the crate the `iroh` scheme **panic-safely**:

1. `pub const IROH_LOCATOR_PREFIX: &str = "iroh";`
2. `LinkKind::Iroh` enum variant.
3. `LinkKind::try_from`: `IROH_LOCATOR_PREFIX => Ok(LinkKind::Iroh)` (ungated — always present).
4. `LinkKind::new_supported_links`: push `Iroh` for `"iroh"`.
5. `ALL_SUPPORTED_LINKS`: include `LinkKind::Iroh` (so the default supported-link set carries iroh).
6. `LocatorInspector::is_reliable`: `LinkKind::Iroh => Ok(true)` (iroh QUIC streams are reliable).
   **Critical:** this and (7) have `_ => unreachable!()` catch-alls; `is_multicast` is called on
   every locator during routing, so an unhandled `Iroh` would *panic the session*, not just fail to
   route.
7. `LocatorInspector::is_multicast`: `LinkKind::Iroh => Ok(false)` (iroh is unicast).
8. `LinkManagerBuilderUnicast::make`: `LinkKind::Iroh =>` dispatch to the registered factory:

   ```rust
   pub type IrohLinkManagerFactory =
       Arc<dyn Fn(NewLinkChannelSender) -> ZResult<LinkManagerUnicast> + Send + Sync>;
   static IROH_LINK_MANAGER_FACTORY: OnceLock<IrohLinkManagerFactory> = OnceLock::new();
   pub fn register_iroh_link_manager_factory(f: IrohLinkManagerFactory) -> ZResult<()>; // set-once
   // in make(): LinkKind::Iroh => factory(manager_sender), else bail "factory not registered"
   ```

**Status: implemented and compiling on the branch** (the spike). Remaining Task-1 polish: a
unit test in the vendored crate (`try_from("iroh/<hex>") == Iroh`; `make` with a dummy factory
returns the dummy; `make` without a registered factory errors rather than panics), and a short
`vendor/zenoh-link/README` note explaining the fork + re-vendor procedure.

## Task 2 — harmony-side registration + inbound forwarder (replace the drain)

**Design constraint (from code exploration):** harmony's iroh accept loop handles ALL ALPNs on one
endpoint — `HARMONY_ZENOH_V1` *and* the device-handshake / friend / ping ALPNs. So we must NOT
rebuild the manager inside the factory (that would entangle the handshake path). Instead keep
Phase-1's `IrohZenohLinkManager` + `spawn_accept_loop` unchanged, and **replace the drain with a
forwarder** that feeds Zenoh's real sender.

- **Keep** the Phase-1 manager build (lib.rs:2545-2551) + accept loop (lib.rs:2575). Keep
  `new_link_rx` (do NOT drop it) — the accept loop pushes `HARMONY_ZENOH_V1` links into it.
- **Register the factory once, before `zenoh::open`.** Because `IROH_LINK_MANAGER_FACTORY` is a
  process-global `OnceLock` but the node restarts (identity switch) with a fresh endpoint/manager
  each cycle, the closure reads a **swappable context slot** — a module-level
  `static IROH_SESSION_CTX: OnceLock<Arc<Mutex<Option<IrohSessionCtx>>>>` (std `Mutex`, not
  `arc-swap` — not a dep) holding `{ manager: Arc<IrohZenohLinkManager>, new_link_rx:
  flume::Receiver<LinkUnicast> }`. `start_node` fills it; `stop_node` clears it. *(Reading a
  swappable slot, not capturing the endpoint, avoids the identity-switch staleness bug class — cf.
  ZEB-352/353 getCallSession/getVoiceSession.)*
- **The factory** (invoked once per session by Zenoh): reads the ctx; **spawns the forwarder** task
  `while let Ok(link) = new_link_rx.recv_async().await { if zenoh_sender.send_async(link).await
  .is_err() { break } }` (`zenoh_sender` is the `make` argument — Zenoh's real
  `NewLinkChannelSender`); and **returns `ctx.manager.clone()` as the `LinkManagerUnicast`** so Zenoh
  uses harmony's existing manager for outbound `new_link`. The forwarder exits when its send fails
  (old session's sender dropped) — clean across restarts.
- **Delete** the drain task (lib.rs:2559-2567) + the `iroh_inbound_drain_handle` field and its
  abort/move plumbing (lib.rs:750-755, 844-845, 999, 2523, 5078, 38335). Inbound iroh links now flow
  accept loop → `new_link_rx` → forwarder → Zenoh's accept queue → live peer transport.

## Task 3 — outbound dial via static `connect/endpoints` seeding

**Dial-mechanism finding (ZEB-368 spike):** zenoh 1.9.0 has NO public on-demand dial —
`session.open_link` does not exist; a runtime `connect/endpoints` insert is rejected (only
`plugins/` keys) and unwatched; the live `TransportManager` is `pub(crate)`. The one PROVEN public
path is **static `connect/endpoints` seeded before `zenoh::open`**, which dials through our manager
via the orchestrator's startup connect (`connect_peers` → `peer_connector` →
`open_transport_unicast` → `new_link_manager_unicast` → `LinkKind::try_from("iroh")` → our factory
manager → `new_link`). Truly-dynamic mid-session dial is deferred to
[ZEB-373](https://linear.app/zeblith/issue/ZEB-373) (needs zenoh's `internal` feature or a 2nd
vendored patch).

- In `event_loop::run`, **before** `zenoh::open` (event_loop.rs:585-603): enumerate community peers
  — `community_registry.known_ids()` → per community materialized `members` filtered to `Joined` →
  per joined `OwnerAddr` `resolver.resolve(owner)` → each `payload.iroh_node_id` →
  `iroh/<hex(node_id)>` locator. De-dup node-ids (a member may appear in several communities); skip
  our own node-id.
- Seed all such locators into `connect/endpoints` the same way the existing LAN endpoint is set:
  `config.insert_json5("connect/endpoints", "[\"iroh/<hex>\", ...]")`, merged with any existing
  connect endpoint.
- Runs once per session; on node restart the set is recomputed from the current resolver. Since a
  zenoh transport is bidirectional once formed, one side dialing suffices — combined with inbound
  accept, the known-peer graph connects and heals on each reconnect.

## Task 4 — inbound listener trigger + locator protocol + lifecycle

- **Factory-invocation trigger (NOT a new accept loop).** Add `iroh/<self EndpointId 64-hex>` to the
  Zenoh `Config` `listen/endpoints` (alongside the `connect/endpoints` set at event_loop.rs:585-603).
  This forces Zenoh to create the iroh manager via the factory at `zenoh::open` — which **starts the
  forwarder and registers harmony's manager** — even on inbound-only / no-known-peer nodes that would
  otherwise never invoke it (no `connect/endpoints` iroh entries). harmony's existing
  `spawn_accept_loop` still owns the accept loop (unchanged from Phase 1);
  `IrohZenohLinkManager::new_listener("iroh/<self>")` is a no-op returning the locator (the iroh
  `Endpoint` is already bound), so Zenoh records the listener locator without double-binding.
- **Locator protocol.** `iroh/<64-hex>` is registered by the Task-1 fork (`IROH_LOCATOR_PREFIX` +
  `try_from`); the scheme matches what `new_link()` already parses.
- **Lifecycle.** Give `del_listener` / link teardown real behavior: close iroh QUIC streams cleanly
  on session shutdown and when a peer drops out of the resolver, so links don't leak.

---

## Maintenance & risk (the fork tax — explicit)

- **The vendored crate is pinned to zenoh 1.9.0.** On any zenoh upgrade we must re-vendor
  `zenoh-link` at the new version and re-apply the 8 additions (one file, ~40 added lines). A
  `vendor/zenoh-link/README` documents the procedure and the diff. The version stays exactly
  matched so `[patch]` keeps satisfying zenoh/zenoh-transport's `=X.Y.Z` pin.
- **CI/build:** the `[patch]` invalidates the cached zenoh-link sub-chain once (the spike's 8m11s
  cold recompile); steady-state builds are normal. `clippy -p harmony-app` does **not** lint the
  patched dependency, so the fork can't trip the harmony `-D warnings` gate; the vendored crate
  nonetheless compiles warning-clean. MSRV: vendored crate `rust-version = 1.75` and it uses only
  `std::sync::OnceLock` (stable 1.70) — within harmony's 1.88 MSRV gate.
- **Security posture:** we now carry a fork of a networking dependency. Mitigation: the diff is
  tiny and additive (a new scheme + a factory hook; no change to existing transports), and the
  README pins the upstream commit so a reviewer can diff our copy against pristine 1.9.0.

## Error handling

- A missing factory registration → the patched `make` returns a Zenoh link error (NOT a panic) so a
  misconfigured boot surfaces cleanly.
- Outbound dial failure (peer unreachable / pkarr miss) → logged, backed-off retry; never blocks the
  session or LAN links.
- A malformed `iroh/<hex>` locator → reject at parse (existing `new_link` behavior), surfaced as a
  Zenoh link error.

## Testing (closes a real coverage gap)

Today the only iroh-transport test drives the link trait directly
(`paired_stream_roundtrip_via_loopback`); **nothing exercises `zenoh::open` + the iroh transport +
CRDT pub/sub end-to-end.** Spec B adds:
- **Vendored-crate unit tests:** `LinkKind::try_from("iroh/<hex>") == Iroh`; `make` dispatches to a
  registered dummy factory; `make` without registration errors (no panic); `is_multicast`/`is_reliable`
  return `Ok(false)`/`Ok(true)` for iroh (the panic-safety guard).
- **Registration test:** a session opened with an `iroh/<self>` listen endpoint reports the iroh
  transport/listener in its locator set.
- **Inbound:** an inbound `IrohZenohLink` handed to the registered manager surfaces as a Zenoh peer
  transport (not dropped).
- **Seam tests (in-process, automated):** (a) a single zenoh session opened with the factory
  registered + an `iroh/<self>` listen endpoint reports the iroh listener in its locator set
  (registration); (b) the inbound forwarder moves a link from `new_link_rx` into a stand-in
  `NewLinkChannelSender` (forwarder); (c) the `connect/endpoints` seeding builder produces the
  expected `iroh/<hex>` set from a seeded resolver (outbound). The existing
  `community_reachability_two_engine_integration` continues to cover the iroh link layer.
- **Full two-node cross-WAN sync** (A dials B, state-root merges both ways) is validated as a
  **two-machine smoke** (ZEB-330 / Koya↔KRILE bring-up), NOT in one process: the factory + swappable
  ctx are a process-global singleton (correct for production's one-node-per-process model), so two
  live sessions can't share them in a single test process.

## Relationship to Spec A & sequencing

Spec A (invite-only generate, ZEB-367) makes a cross-WAN **join** succeed (snapshot embedded in the
invite + iroh-handshake countersign). Spec B makes **ongoing** messages flow afterward. Ship order
was forced: **A → B**; A is merged, so B is unblocked. Together they close the cross-WAN community
loop for ZEB-330. DM-over-iroh remains a separate, later track.

## Out of scope

- DM transport migration to iroh (DMs stay on Reticulum; separate track).
- pkarr discovery changes (cases A/B/C already shipped in ZEB-323).
- The per-peer-session / loopback-bridge fallbacks (only revisited if the fork is later abandoned).
- **Dynamic mid-session outbound dial** — deferred to [ZEB-373](https://linear.app/zeblith/issue/ZEB-373).
  This PR dials only peers known at `zenoh::open` (static `connect/endpoints`); see Task 3.

## Resolved decisions (Jake, 2026-06-04)

1. **Vendored-crate form:** in-repo vendor at `src-tauri/vendor/zenoh-link/` (self-contained, one
   PR, no second repo). ✅
2. **Inbound trigger:** the `listen/endpoints` iroh entry forces the factory (→ forwarder + manager
   registration) to run at `zenoh::open` even for inbound-only / no-known-peer nodes; harmony's
   existing `spawn_accept_loop` still owns the accept loop. ✅
3. **Outbound dial:** static `connect/endpoints` seeding now; dynamic mid-session dial deferred to
   ZEB-373. ✅
