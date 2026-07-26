# Transport-02: harmony-client networking/transport stack — forensic map

**Date:** 2026-06-14
**Scope:** `harmony-client/src-tauri/src/` + `harmony-client/e2e-harness/`
**Method:** Read-only forensic analysis. No code changed. Evidence cited as `file:line`.
**Branch at analysis time:** `zeb-461-dm-device-cache` (ZEB-461 device-bundle/tunnel work is LANDED on this branch, not yet merged to main).

---

## 0. TL;DR

- The client's transport core is **"Zenoh pub/sub riding an iroh QUIC link layer"** and it is **real, wired, and exercised between two real nodes**, not scaffolding.
- The client does **NOT reimplement** the whole stack. It **depends on `harmony-runtime`** (the sans-I/O `NodeRuntime` engine) for routing/CRDT/Reticulum, and the client's `event_loop.rs` is the **I/O shell** that translates `RuntimeAction`/`RuntimeEvent` into real Zenoh/iroh calls. What the client *does* own is the **iroh link layer for Zenoh** (a vendored `zenoh-link` fork + a locally-defined `IrohZenohLinkManager`) and the iroh first-contact ALPN acceptors.
- **iroh is the proven first-contact link layer.** Five ALPN acceptors (invite, friend, friend-pex, butler-deposit, community-relay) are real, signed, consent-gated, wired into the boot path, and two-node integration-tested.
- **Reticulum is NOT being torn out** — it's still the DM unicast substrate (`RuntimeUnicastTransport` → `RuntimeEvent::SendUnicastToDevice` → harmony-runtime's Reticulum). ZEB-461 *adds* an iroh-tunnel route on top of Reticulum (a `tunnel-*` Reticulum interface), it does not replace Reticulum. The strategic direction is "Reticulum tunnels carried over iroh," not "rip out Reticulum."
- **Biggest working asset:** iroh first-contact + community-state CRDT sync over the live transport (e2e S1/S3 hard-assert it).
- **Biggest open gap:** owner-global Zenoh broadcast topics (profile cards) do **not** propagate co-located (e2e S5 characterizes this as broken — ZEB-466/ZEB-432), and cross-WAN re-peering is unproven (only co-located is proven).

---

## 1. Component inventory

| Component | Files | Purpose | Verdict | Evidence |
|---|---|---|---|---|
| iroh endpoint wrapper | `iroh_endpoint.rs` | Long-lived `iroh::Endpoint` (v0.98), persistent Ed25519 id in keychain w/ encrypted-file fallback, ALPN registry | **WORKING** | Lifecycle unit test `iroh_endpoint_inits_with_ephemeral_secret` (`:366`); 8 ALPNs registered at bind (`:122-131`); key fallback ZEB-449 (`:302-311`) |
| Vendored zenoh-link fork | `vendor/zenoh-link/src/lib.rs`; patched in `Cargo.toml:226` | Teaches zenoh's *closed* locator→transport dispatch about `iroh/<hex>` | **WORKING** | `LinkKind::Iroh` added to closed enum (`:124`); `LinkManagerBuilderUnicast::make` dispatches `iroh/` to the registered factory (`:444-451`); `[patch.crates-io] zenoh-link = { path = "vendor/zenoh-link" }` (`Cargo.toml:226`) |
| Zenoh-over-iroh link manager | `zenoh_iroh_transport.rs` (1121 ln) | Implements zenoh's `LinkManagerUnicastTrait`; outbound `new_link` dials iroh QUIC, inbound `spawn_accept_loop` accepts + routes by ALPN | **WORKING** | `struct IrohZenohLinkManager` (`:129`); `impl LinkManagerUnicastTrait` (`:536`); outbound `endpoint.connect(addr, HARMONY_ZENOH_V1)` + `open_bi` (~`:569-581`); inbound accept loop (`:356-512`). Unit + real-loopback-QUIC tests in-file |
| Zenoh link wrapper | `zenoh_iroh_link.rs` (355 ln) | Wraps iroh `(SendStream,RecvStream)` as zenoh `LinkUnicastTrait` (write/read/close/mtu) | **WORKING** | `paired_stream_roundtrip_via_loopback` integration test (two hermetic iroh endpoints, real QUIC, bidi round-trip) |
| Factory registration glue (ZEB-368) | `iroh_zenoh_registration.rs` | Process-global factory bridging the local `IrohZenohLinkManager` to the vendored zenoh-link; inbound-link forwarder; listen/connect locator builders | **WORKING** | `ensure_iroh_factory_registered` (`:54`); `set_iroh_session_ctx` (`:25`); `forward_inbound_links` (`:36`); `merge_iroh_listen_endpoints` unit tests (`:148+`). NB: imports `crate::zenoh_iroh_transport::IrohZenohLinkManager` (LOCAL), not harmony-core |
| Dynamic mid-session dial driver (ZEB-373/390) | `iroh_dial_driver.rs` (472 ln) | Consumes `DialHint`s from the resolver, dedups, dials each new peer once via `Runtime::connect_peer` with a **deterministic zid** derived from the peer's iroh node-id | **WORKING (unit-proven; two-node only via gated e2e S5c)** | `RuntimePeerDialer::dial` (`:199-218`); `deterministic_zid_hex` (`:98`) + the ZEB-390/455 invariant tests (`:406, :432`); driver logic tested with a `MockDialer` (`:276-365`). Real two-node dial exercised only behind `HARMONY_ZENOH_DISABLE_MULTICAST=1` (e2e S5c) |
| Event loop / node boot | `lib.rs::start_node_inner` (`:2643`), `event_loop.rs::run` (`:680`) | Builds iroh endpoint + link mgr, registers factory, opens zenoh session with iroh in connect+listen endpoints + deterministic zid, drives the `select!` loop bridging `NodeRuntime` actions↔events to Zenoh | **WORKING** | Link mgr built + accept loop spawned + factory set before `zenoh::open` (`lib.rs:3624-3658`); deterministic zid set in config (`event_loop.rs:1006-1014`); iroh merged into connect/listen endpoints (`event_loop.rs:1040-1078`); `zenoh::open` (`event_loop.rs:1080`); main `select!` (`event_loop.rs:3149`) |
| Reachability resolver/publisher | `reachability_resolver.rs`, `reachability_publisher.rs`, `reachability_record.rs`, `pkarr_*` | pkarr-published reachability records (iroh node-id, relay, direct addrs); resolver feeds both static seeds and the dial driver via a `DialHint` seam | **WORKING** | `DialHint` first-learn seam (`reachability_resolver.rs:57,123`); `iroh_connect_locators` (`iroh_zenoh_registration.rs:83`) |
| Community-state CRDT sync | `community_state_sync.rs` (7228 ln), `community_membership.rs`, `community_state_crdt.rs` | Community membership/roster/channel CRDT sync via zenoh query+subscriber adapters bridged from `NodeRuntime` | **WORKING (co-located proven)** | Zenoh adapter referenced throughout (`:790,889,2513,4686`); proven by e2e S1 (roster) + S3 (offline channel catch-up) |
| Channel logs | `community_channel_log*.rs`, `channel_backfill.rs` | Per-channel append-only logs + reconnect backfill | **WORKING** | e2e S3 offline→reconnect catch-up hard-asserted |
| CAS local store | `content_store.rs`, `content_index.rs` | `CasOp::{PutLocal,GetLocal,GetOrFetch}` over harmony-content `StorageTier` | **WORKING** | `CasOp` handlers (`event_loop.rs:3480-3522`) |
| CAS network serve/fetch | `event_loop.rs` queryable + fetch | Serve content via zenoh queryable on `harmony/content/*/**`; fetch via zenoh GET with hash-verify gate; encrypted-CID serve gate | **WORKING (two-node proven)** | serve queryable (`event_loop.rs:7999-8062`); fetch+verify (`:3523-3601`); encrypted gate `content_cid_servable` (`:7972`); two-node test `cas_serve_two_node_integration.rs::serves_public_cid_to_a_second_zenoh_node` + `does_not_serve_encrypted_cid` |
| `RuntimeAction::SendReply` | `event_loop.rs:5893` | Generic query-reply runtime action | **STUB** | `RRuntimeAction::SendReply { .. } => tracing::trace!("SendReply not yet implemented in client")` (`:5893-5896`). NB: CAS serve does NOT depend on this — it uses the dedicated zenoh queryable, so the stub is not currently load-bearing |
| iroh invite acceptor | `iroh_invite_acceptor.rs` (527 ln) | `harmony/handshake/v1` — community invite first-contact (decode `CommunityInviteSigned`, insert, auto-countersign, write `SignedMembershipEvent`) | **WORKING (two-node proven)** | `IrohInviteHandshakeAcceptor::handle_connection`; two-node `pkarr_iroh_redeem_full_integration.rs` (Phase 2c option A direct bi-stream) |
| iroh friend acceptor + multiplexer | `iroh_friend_acceptor.rs` (2992 ln) | `harmony/friend/v1` friend-link handshake; the `MultiplexHandshakeDispatcher` routing handshake/friend/pex ALPNs; **ZEB-461** device-bundle + tunnel-contact wiring | **WORKING (two-node proven)** | Signed `FriendLinkRequest`/`Accepted` with cert+device-sig verify; ZEB-461 fields on wire (`:275-303,:358-379`); `apply_owner_device_update` in handshake (`:960`); `try_register_tunnel_peer` in both paths (`:1467,:1511`); unit `process_friend_request_populates_owner_device_cache` (`:2752`); two-node `friend_token_roundtrip_integration.rs` |
| iroh PEX acceptor | `iroh_pex_acceptor.rs` (489 ln) | `harmony/friend-pex/v1` — signed referral catalog to authenticated friends; empty catalog to non-friends | **WORKING (unit + roundtrip integ)** | `IrohFriendPexAcceptor`; serve-decision unit tests (`:294-488`); `referral_catalog_roundtrip_integration.rs` |
| iroh butler acceptor | `iroh_butler_acceptor.rs` (1882 ln) | `harmony/butler-deposit/v1` — sealed-DM deposit to an online sibling (spec §4 verify order, persist-before-ack) | **WORKING (two-node proven)** | Full spec-§4 pipeline (`:480-686`); `butler_deposit_integration.rs` (45 KB) |
| iroh community-relay acceptor | `iroh_community_relay_acceptor.rs` (2447 ln) | `harmony/community-relay-deposit/v1` + `.../pull/v1` — opaque sealed-relay hold + pull for community members | **WORKING (two-node proven)** | Deposit keeps blob opaque (no decrypt) (`:285-295`); pull query→response→ack; `community_relay_integration.rs` (56 KB) |
| DM outbox / unicast transport | `dm_outbox.rs` (8200 ln), `dm_signing.rs`, `dm_envelope.rs` | `send_dm` → drain → `RuntimeUnicastTransport` emits one signed `DmCidNotify` per resolved device → `RuntimeEvent::SendUnicastToDevice` → harmony-runtime Reticulum | **WORKING (channel-boundary tested; real-wire = e2e S2 / manual)** | `RuntimeUnicastTransport` (`:207-292`); `resolve_destinations` from `OwnerDeviceCache` (`:74-86`); event-loop emit arm (`event_loop.rs:3606-3627`); `dm_unicast_integration.rs` mocks at the channel boundary |
| DM iroh-tunnel contact (ZEB-461) | `dm_tunnel_contact.rs` | Build/register a `ContactAddress::Tunnel` (peer iroh node-id + relay + PQ keys) so a friend handshake establishes a Reticulum tunnel without a shared community | **WORKING-ON-BRANCH (wired; e2e proof recent)** | `build_tunnel_contact` + `try_register_tunnel_peer` (`:52-107`); event-loop registration arm pushes `RuntimeEvent::ContactChanged` (`event_loop.rs:3629-3671`); wired via `with_tunnel_peer_tx` (`lib.rs:6836`) |
| Reticulum port handling | `event_loop.rs:24-43,:896-963` | `HARMONY_RETICULUM_PORT` (unset→4242, `0`→disabled LAN UDP, else custom); degradable bind | **WORKING** | `parse_reticulum_port` (`:24`); degradable LAN bind w/ loopback fallback (`:896-963`) |
| Profile-card / membership broadcast | `profile_card_broadcast.rs`, `profile_broadcast.rs` | Zenoh pub/sub on owner-keyed topics (`harmony/discovery/profile/owner/{id}/card`), EnrollmentCert-verified | **PARTIAL — publish/subscribe verbs work; propagation BROKEN co-located** | Sign/verify + topic real; e2e S5 characterizes co-located non-convergence (ZEB-466/ZEB-432). `verify_card` is self-contained, so it's a transport/routing gap not a crypto gap |
| Voice signaling/presence | `voice_signal.rs`, `voice_presence.rs`, `voice_crypto.rs` | DM-call signaling + presence over zenoh; two-engine integration tests | **WORKING** | `voice_dm_two_engine_integration.rs`, `voice_presence_two_engine_integration.rs` |

---

## 2. What is PROVEN end-to-end (the e2e hard-asserts)

The strongest "this really works" signal is `e2e-harness/tests/e2e_two_node.rs` (`--features e2e`), which spawns **two real `harmony-app serve` processes** under separate temp HOMEs and drives them over the live HTTP/WS API. Each scenario calls `run.mark_success()` only after its asserts pass.

**Hard-asserted (PROVEN co-located, two real nodes):**

- **S1 `s1_invite_join_roster_convergence`** (`:158`): Bob joins Alice's invite-only community over the **real iroh first-contact path** (`connectivity_redeem_invite_iroh` → pkarr-resolve Alice + open handshake-ALPN bi-stream), then the **joined roster converges in BOTH directions** (`roster_has_joined` polled both ways). Proves: iroh first-contact + community-membership CRDT sync over the live transport.
- **S2 `s2_friend_graph_and_dm_send`** (`:259`): friend-token iroh handshake → friendship `active` BOTH ways (the ZEB-431 DM-picker graph) → DM-space creation with verified id semantics → **DM bytes round-trip BOTH directions** over the friend-established iroh tunnel (`:334-370`). This is the ZEB-461 hard-assert. *Caveat: the committed `e2e-harness/README.md` still lists S2 DM-delivery as "characterized, not asserted" — the README lags the test file, which was upgraded to hard-assert on this branch.*
- **S3 `s3_offline_channel_reconnect_catchup`** (`:401`): Alice creates a channel while Bob is SIGKILL'd offline; Bob relaunches against persisted state and **catches up the offline-created channel** (`channels_contains`). Proves co-located **ongoing community-state CRDT sync** + reconnect re-peer (pkarr re-resolve + ZEB-373 iroh dial). Header documents this passed 3/3.
- **S4 `s4_restart_durability`** (`:478`): single-node mint→create-community→graceful-restart→community rehydrates in `list_owner_communities`. Proves owner-state durability (ZEB-393). Not a transport proof.
- **S5/S5b/S5c card verbs** (`:553,:668,:774`): the headless card publish/subscribe/unsubscribe verbs are hard-asserted (boot, join, accept publish+subscribe). See §6 for what they characterize.

**Two-node integration tests (in `src-tauri/tests/`, real iroh endpoints, run under the normal `--features test-fixtures` suite):**

- `community_reachability_two_engine_integration.rs` — two real iroh endpoints round-trip a **CRDT-shaped payload through the `IrohZenohLinkManager` link wrappers** (hermetic loopback). Proves the **link layer carries bytes between two nodes**. (This exercises the link wrappers directly, not a full zenoh-session route.)
- `pkarr_iroh_redeem_full_integration.rs` — two-party iroh handshake → invite redeem → `status == "joined"`. **Important:** this is Phase 2c **option A (direct iroh bi-stream on the handshake ALPN)** and explicitly **does NOT use a CRDT-sync round-trip** (`:552`). It proves first-contact, not zenoh-over-iroh CRDT sync.
- `cas_serve_two_node_integration.rs` — two zenoh sessions; node B GETs a CID and receives exact bytes; encrypted CID is refused. Proves **CAS serve/fetch over zenoh between two nodes**.
- `friend_token_roundtrip_integration.rs`, `butler_deposit_integration.rs`, `community_relay_integration.rs`, `referral_catalog_roundtrip_integration.rs`, `voice_*_two_engine_integration.rs` — two-node proofs for each respective acceptor.
- `zeb_373_dynamic_dial_integration.rs` — dynamic dial driver exercise.

---

## 3. The Zenoh-over-iroh core — status assessment

**Verdict: the link layer is REAL and the architecture is wired end-to-end; full zenoh-session-over-iroh CRDT sync is proven only co-located, and the dynamic dial that is the ONLY cross-WAN peering path is unit-proven but two-node-proven only behind a gated probe.**

The pipeline, top to bottom:

1. **Vendored zenoh-link fork** (`Cargo.toml:226` patch). zenoh 1.9.0 has no public/internal seam to register a custom unicast transport (the `LinkKind` enum + `LinkManagerBuilderUnicast::make` are a closed match — this matches the prior `zenoh_no_custom_transport` finding). The fork adds `LinkKind::Iroh` (`vendor/zenoh-link/src/lib.rs:124`) and routes `iroh/<hex>` locators to a process-global factory (`:444-451`). The fork **does not depend on harmony** — the factory closure is injected by the app (`:88-89`), so there is no crate cycle.
2. **Factory registration** (`iroh_zenoh_registration.rs`): the app registers a factory that hands zenoh the locally-built `IrohZenohLinkManager` for outbound `new_link` and spawns a forwarder draining accepted inbound links into zenoh's real `NewLinkChannelSender`.
3. **`IrohZenohLinkManager`** (`zenoh_iroh_transport.rs`): outbound `new_link` resolves the peer's reachability, dials `endpoint.connect(addr, HARMONY_ZENOH_V1)`, opens a bidi QUIC stream, wraps it as a zenoh `LinkUnicast`. Inbound `spawn_accept_loop` accepts iroh connections, filters by ALPN, and feeds `harmony/zenoh/v1` streams to zenoh (and routes the other ALPNs to the handshake/butler/relay acceptors).
4. **Boot wiring** (`lib.rs:3624-3658`, `event_loop.rs:1006-1080`): before `zenoh::open`, the app sets a **deterministic zenoh id** = `deterministic_zid_hex(iroh_node_id)` (so a dialer can compute the same zid and `connect_peer`'s post-handshake transport lookup matches — ZEB-390), and merges the node's own `iroh/<hex>` locator into both `connect/endpoints` and `listen/endpoints` (forcing the factory to run even on inbound-only nodes).
5. **Dynamic dial** (`iroh_dial_driver.rs`): on first-learn of a peer the resolver emits a `DialHint`; the driver dials once via `Runtime::connect_peer(deterministic_zid, iroh_locator)`. This is the **only Zenoh peering path that exists cross-WAN** (no LAN multicast there).

**What's proven vs not:**

- The **iroh link byte-carry between two nodes** is proven (`community_reachability_two_engine_integration.rs`, plus the in-file loopback-QUIC tests).
- **Full community-CRDT sync over the live transport** is proven **co-located** (e2e S1 roster, S3 offline channel catch-up). Co-located nodes can peer via **LAN multicast** *or* the iroh dial; the e2e harness leaves multicast ON by default, so a co-located pass does **not** isolate the iroh-dial path.
- The **iroh dial in isolation** (multicast off — i.e., the cross-WAN-representative path) is only exercised by **S5c, which SKIPS unless `HARMONY_ZENOH_DISABLE_MULTICAST=1`** (`:779`). Its convergence is **characterized, not hard-asserted**. *[Stale as of ZEB-809 / PR #558, 2026-07-26: that env var is retired — LAN scouting is now off by default (opt back in with `HARMONY_ZENOH_ENABLE_LAN_SCOUTING=1`, exactly `1`), S5c runs unconditionally on every sweep, and it is hard-asserted.]* Per the in-file ZEB-468 notes, the **mint-driven node restart** poisons co-located Zenoh peering (deterministic-zid remap + failed re-peer dial), and S5 propagation fails post-restart; S5b (clean relaunch, multicast) is the control. So the dial's clean cross-WAN behavior is the **open question the cross-machine run answers** — not yet closed here.

Net: the Zenoh-over-iroh stack is genuinely built and carries real bytes; "every node runs Zenoh over an iroh link" is implemented and co-located-proven, but the **cross-WAN dynamic-dial peering is not yet two-node-hard-proven** (it is the explicit subject of ZEB-468/ZEB-444 follow-ups).

---

## 4. What the client reimplements vs reuses from harmony-core

**Reuses (git-pinned to `harmony.git` rev `dddf1929`, `Cargo.toml:94-112`):**

- `harmony-runtime` — the sans-I/O `NodeRuntime` + `RuntimeAction`/`RuntimeEvent` engine. This is the actual routing/CRDT/Reticulum brain. `event_loop.rs:16` imports it; the whole `select!` loop is a translation shell that pushes `RuntimeEvent`s in and dispatches `RuntimeAction`s out to Zenoh/iroh. **DMs route through harmony-runtime's Reticulum** via `RuntimeEvent::SendUnicastToDevice`.
- `harmony-identity` (26 files), `harmony-owner` (23), `harmony-content` (15), `harmony-pkarr` (9), `harmony-mailbox`, `harmony-contacts` (`ContactAddress::Tunnel`), `harmony-compute`, `harmony-telemetry` — types and primitives.

**Reimplements / owns locally (this is the "client reinvented networking" kernel):**

- **The iroh link layer for Zenoh** — `IrohZenohLinkManager`, `IrohZenohLink`, the vendored `zenoh-link` fork, the factory glue, the dial driver. None of this comes from harmony-runtime (only 2 files `use harmony_runtime` at all; the link manager is `crate`-local). harmony-core's own transport is **not** used for the Zenoh carrier.
- **All five iroh first-contact ALPN acceptors** + the multiplex dispatcher.
- **The full Zenoh session lifecycle** (config build, deterministic zid, connect/listen endpoint merge, `open_session_with_runtime`) and the CAS serve queryable / fetch.

So: the client **does not bypass** harmony-runtime — it sits **on top of** it and **supplies the I/O + the iroh-backed Zenoh transport** that the sans-I/O runtime needs. The "reinvented the networking stack" story is accurate specifically for the **iroh link layer under Zenoh** and the **first-contact handshakes**, not for routing/CRDT/Reticulum (those are harmony-runtime).

---

## 5. The DM / Reticulum situation

**Reticulum is alive and is the DM unicast substrate — it is NOT torn out.** The picture (resolving an initial mis-read from stale plan docs against the current branch):

- DM send path: `send_dm` → `dm_outbox::drain` → `resolve_destinations(OwnerDeviceCache, recipient)` computes one Reticulum destination-hash per device (`dm_outbox.rs:74-86`) → `RuntimeUnicastTransport::send` builds a signed `DmCidNotify` per device and pushes `UnicastSendRequest`s (`:207-292`) → event loop emits `RuntimeEvent::SendUnicastToDevice` into `NodeRuntime` (`event_loop.rs:3606-3627`) → **harmony-runtime's Reticulum routes it**.
- The known co-located problem: two co-located nodes with the single fixed LAN UDP socket collide, so the harness sets `HARMONY_RETICULUM_PORT=0` to disable the `udp0` LAN socket (`e2e-harness/src/node.rs:95`, `event_loop.rs:939`). With udp0 off, the old behavior left `OwnerDeviceCache` empty and no route → `send_dm` retried forever ("no known devices for recipient"). This is the "DM-over-Reticulum can't route co-located because the tunnel transport isn't in the client" symptom.
- **ZEB-461 fix (LANDED on `zeb-461-dm-device-cache`, wired, e2e-asserted, not yet merged to main):** the **friend handshake now carries each side's device bundle + iroh node-id + relay + PQ keys on the wire** (`iroh_friend_acceptor.rs:275-303,:358-379`, bound into the handshake signature via `friend_devices_digest`), so on completion the acceptor (1) **populates `OwnerDeviceCache`** via `apply_owner_device_update` (`:960`) and (2) **registers a `ContactAddress::Tunnel`** via `try_register_tunnel_peer` (`:1467,:1511`) → event loop adds it to the runtime `ContactStore` + pushes `ContactChanged` (`event_loop.rs:3629-3671`) → harmony-runtime opens a **`tunnel-*` Reticulum interface over the iroh tunnel** → announces propagate over it → the DM routes **without udp0**. Unit-tested (`process_friend_request_populates_owner_device_cache`, `dm_tunnel_contact.rs` tests) and hard-asserted in e2e S2.
- Strategic read: **DM delivery = Reticulum carried over an iroh tunnel.** Reticulum is the addressing/announce layer; iroh provides the NAT-traversing carrier when peers share no community. The earlier "DM-over-Reticulum may be abandoned in favor of Zenoh-over-iroh" hypothesis is **not** what the code shows — DMs are unicast (per-device sealed envelopes), which is Reticulum's job; the community/workspace CRDT pub/sub is Zenoh's job. They're complementary layers, both riding iroh.
- `RuntimeAction::SendReply` is a **stub** (`event_loop.rs:5893`) — but CAS serve uses a dedicated zenoh queryable, so it is not load-bearing today.

---

## 6. Surprises / things to flag

1. **The e2e suite's own comments are a forensic goldmine — and self-correcting.** S3/S4 were long `#[ignore]`'d "blocked by ZEB-462," but the team discovered that was a **harness bug** (assertions checked `c.get("id")` while the DTOs are camelCase `channelId`/`spaceId`, so polls always timed out and *looked* like a sync failure). Co-located ongoing community-state sync actually works; the "no-responder re-peering" ZEB-462(A) was a non-bug. (Matches the `e2e_assertion_camelcase_keys` memory.)
2. **README lags the test file.** `e2e-harness/README.md` lists S2 DM byte-delivery as "characterized, not asserted (co-located gap ZEB-461)," but `e2e_two_node.rs` (this branch) **hard-asserts** the round-trip both ways. Anyone trusting the README would under-state what's proven.
3. **Profile-card propagation is the standout BROKEN path.** Owner-global Zenoh broadcast topics (cards) do **not** converge between two co-located community-only peers (e2e S5, ZEB-466), even though the *same two nodes'* community roster sync (also Zenoh) **does** converge, cards **are** signed/published, and `verify_card` is self-contained. So it's a **transport/routing gap for owner-global topics**, the likely substrate of ZEB-432 (member cards rendering as truncated hex). S5b/S5c are diagnostic probes (ZEB-468) isolating whether the cause is the mint-driven restart vs the dial.
4. **The mint flow restarts the node, which poisons co-located Zenoh peering.** ZEB-468 root-caused S5's card failure partly to the mint-driven restart (deterministic-zid remap + failed ZEB-373 re-peer dial + dead TCP accept loop). Every `two_minted_nodes` scenario starts from this restart-poisoned state — a latent reliability hazard worth tracking.
5. **Two distinct first-contact mechanisms coexist.** Phase 2c **option A** (direct iroh bi-stream on the handshake ALPN) is what the invite/friend redeem actually use and what `pkarr_iroh_redeem_full_integration` proves — explicitly *without* a CRDT round-trip. The CRDT sync that follows rides the separate Zenoh-over-iroh session. Don't conflate "first contact works" with "zenoh-over-iroh CRDT sync works" — they are proven by different tests.
6. **Cross-WAN is genuinely unproven here.** Every hard-assert in this tree is co-located (one machine, two processes, LAN multicast available). The dial-only / multicast-off path (the cross-WAN-representative one) is a *characterized* probe (S5c), and the cross-machine playbook (ZEB-444) is the only cross-WAN vehicle. The owner's belief that "harmony-client is the side that works" is well-supported **for co-located first-contact + CRDT sync + CAS**; cross-WAN re-peering remains the frontier.
7. **`zenoh` is pinned `=1.9.0`** because the client depends on its `internal` (unstable) runtime surface (`Cargo.toml:34-39`); the vendored `zenoh-link` fork must track it. Upgrades are deliberate and coupled.
