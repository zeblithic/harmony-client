# Transport-03: harmony-core ↔ harmony-client Dependency + Duplication Map

**Date:** 2026-06-14
**Scope:** READ-ONLY forensic analysis. No code changed.
**Repos:**
- `harmony` core — `/Users/zeblith/work/zeblithic/harmony/crates` (`.worktrees/` ignored)
- `harmony-client` — `/Users/zeblith/work/zeblithic/harmony-client/src-tauri`
**Pin:** client depends on harmony git `rev = dddf1929` (`zeb-461-tunnel-contact-pq-keys`).

---

## TL;DR

The harmony-client **embeds the sans-I/O `NodeRuntime` from harmony-runtime as a state machine for a NARROW subset of message types** (Zenoh pub/sub setup, content-fetch routing, and a vestigial LAN-broadcast Reticulum path), and **runs its own complete real-I/O transport stack** (iroh `Endpoint`, a vendored **Zenoh-over-iroh** transport that does not exist in core, and 6+ direct-ALPN iroh acceptors) that **bypasses the runtime entirely** for all real networking (DMs, friends, butler, community relay, dialing).

harmony-core **does** ship a complete, working-looking transport event loop — `harmony-node` — but it is a **standalone binary** with a **divergent transport design** (plain `zenoh::open` + Reticulum-over-iroh tunnels), it is **not a library**, **nothing depends on it**, and its event loop is **frozen** (last real change ~2026-05-03). The client reimplemented rather than reused, and the client's version is the one that is wired, tested (real two-engine iroh round-trips), and shipped daily.

**Net: yes, the transport wheel was reinvented. The client's wheel is the one that turns.**

---

## (a) Dependency Table — what the client actually imports from core

The client Cargo.toml git-pins **9 harmony-* crates** (all at the same rev). The real coupling surface is far narrower than the dependency count suggests.

| Core crate | What the client imports (key types/fns) | Coupling depth | Notes |
|---|---|---|---|
| **harmony-runtime** | `NodeConfig`, `NodeRuntime`, `RuntimeAction`, `RuntimeEvent` (only — `lib.rs:7`, `event_loop.rs:16`) | **Deep but narrow** | The whole 9000-line runtime is reached through 4 type names. Driven via `tick()` / `push_event()` / `start()` / `storage_tier()` / `pin_content()` / `contact_store_mut()`. Runs `!Send` on a dedicated background thread (`lib.rs:614`). |
| **harmony-identity** | `PrivateIdentity::from_seed` (×46), `Identity::{from_public_bytes,to_public_bytes,address_hash}` | **Genuine reuse** | Core crypto-identity primitive. Client `identity.rs` is 3850 lines but references core types only ~9× — it *wraps* the primitive and builds owner-state on top. |
| **harmony-owner** | `certs::EnrollmentCert` (×27), `cbor::to_canonical` (×15), `pubkey_bundle::PubKeyBundle`, `lifecycle::{mint_owner,RecoveryArtifact}`, `trust::evaluate_trust`, `state::OwnerState` | **Genuine reuse** | Owner lifecycle, trust evaluation, recovery artifacts, canonical CBOR. Load-bearing. `features = ["recovery"]`. |
| **harmony-content** | `cid::{ContentId,ContentFlags,for_book}`, `chunker::ChunkerConfig`, `bundle::{BundleBuilder,parse_bundle}`, `book::{MemoryBookStore,BookStore}`, `cache::ContentStore`, `storage_tier` | **Genuine reuse** | CAS primitives + `MemoryBookStore` is the store handed to `NodeRuntime::new`. The keeper for content addressing. |
| **harmony-pkarr** | `PkarrResolver`, `PkarrPublisher`, `RelayClient`, `RelayPool`, `RelayHealth`, `derive_ephemeral_key`, `epoch_tolerance_window`, `PkarrCase` | **Genuine reuse** | Transport-agnostic DHT discovery. Real reuse, no client equivalent. `test-fixtures` (MockPkarrRelay) dev-only. |
| **harmony-owner** (recovery) | (see above) | — | — |
| **harmony-mailbox** | `message::HarmonyMessage`, `mailbox::{MessageEntry,MailPage,MailFolder,MailRoot}` | **Types-only reuse** | Message/folder wire types reused by client `mail.rs` / `mail_sync.rs`. Mailbox transport is client-built. |
| **harmony-contacts** | `ContactAddress::Tunnel`, `Contact`, `PeeringPriority`, `PeeringPolicy` | **Shallow** | Only used (ZEB-461) to construct a `ContactAddress::Tunnel` and hand it to the runtime's `ContactStore`. |
| **harmony-telemetry** | `TelemetryEvent`, `encode_event`, `decode_event` | **Shallow** | Telemetry wire types. |
| **harmony-compute** | `InstructionBudget` (×1) | **Trivial** | One config field (`NodeConfig.compute_budget`). |

**Crates conspicuously NOT depended on (core has them, client reimplemented or skipped):**
- **harmony-node** — the reference transport event loop. Not a lib; never imported. (THE reinvented wheel.)
- **harmony-tunnel** — sans-I/O Reticulum-over-iroh handshake crypto. Not imported; client wrote its own handshake protocols in `iroh_friend_acceptor.rs` / `iroh_invite_acceptor.rs`.
- **harmony-zenoh** — core's Zenoh keyspace/envelope/router. Not imported; the client only *mimics* its `fetch_key()` shard-prefix convention by comment (`event_loop.rs:5858`). Client reimplemented Zenoh wiring + vendored a custom `zenoh-link` fork.
- **harmony-peers / harmony-reticulum / harmony-discovery** — pulled transitively *inside* `harmony-runtime` (e.g. `harmony-runtime/Cargo.toml:44`), never directly. The client drives them only through the runtime's action surface, most of which it drops (see §c).
- **harmony-crypto** — NOT a direct dep; client *replicates* small helpers rather than import (`dm_signing.rs:713-715`).

---

## (b) Duplication Inventory

| Capability | Core location + status | Client location + status | Verdict |
|---|---|---|---|
| **Transport event loop** | `harmony-node/src/event_loop.rs` (3038 LoC). Real `zenoh::open` (`:398`), real `iroh::Endpoint::builder().bind()` (`:561`), full `tokio::select!` (`:829+`), dispatches the **entire** RuntimeAction surface incl. `InitiateTunnel`/`SendPathRequest`/`CloseTunnel`/`RunInference` (`:2082+`). **Standalone binary `harmony`; nothing depends on it; frozen since ~2026-05-03 (last touch was a tree-wide `cargo fmt`).** | `event_loop.rs` (466 KB, ~11k LoC). Real `iroh::Endpoint` + Zenoh-over-iroh (`open_session_with_runtime`, `:9044`), full `tokio::select!` (`:3149`), drives `runtime.tick()` (`:5296`) but handles **only 8 of 23** emitted action variants. **Wired, shipped, tested daily (touched 2026-06-14).** | **keep-client.** Divergent designs (see §transport). Client is the living, tested one. harmony-node is reference/dead. Back-port direction: **client → core** (or delete harmony-node's loop). |
| **iroh endpoint / ALPN transport** | `harmony-node/src/event_loop.rs:561` (+`tunnel_task.rs`): one ALPN, `harmony-tunnel/1` (Reticulum-over-iroh tunnel). | `iroh_endpoint.rs:120-134`: real `Endpoint::builder(presets::N0)`, **8 ALPNs** (`harmony/zenoh/v1`, `/handshake/v1`, `/friend/v1`, `/friend-pex/v1`, `/butler-deposit/v1`, `/community-relay-deposit/v1`, `/community-relay-pull/v1`, `/ping/v1`). | **keep-client.** Client's ALPN surface is the product's real protocol set; core's single tunnel ALPN is the abandoned design. |
| **Zenoh-over-iroh transport** | **Absent.** Core uses plain `zenoh::open(Config::default())` over Zenoh's own TCP/UDP/QUIC. | `zenoh_iroh_transport.rs` (~1400 LoC) + `zenoh_iroh_link.rs` + `iroh_zenoh_registration.rs`, backed by a **vendored `zenoh-link` fork** (`src-tauri/vendor/zenoh-link`, `[patch.crates-io]`). Runs Zenoh links over iroh QUIC bi-streams on `harmony/zenoh/v1`. | **client-only; back-port to core** if core ever needs WAN Zenoh. This is net-new capability the client invented; core has no equivalent. |
| **Peer/tunnel lifecycle** | `harmony-peers::PeerManager` (sans-I/O, embedded in runtime) emits `InitiateTunnel`/`SendPathRequest`/`CloseTunnel`; **harmony-node executes them** (`event_loop.rs:2082+`, `tunnel_task.rs`). | **Dropped.** The client never handles those actions (`_ => {}` at `event_loop.rs:5899`). Real peer connectivity is done by `iroh_dial_driver.rs` (Zenoh `connect_peer`) + the ALPN acceptors — a parallel, runtime-independent mechanism. | **divergent; keep-client mechanism, prune core path.** The runtime's whole peer/tunnel action surface is dead weight from the client's POV. |
| **DM / messaging / outbox** | **No equivalent.** Core has only key-namespace conventions (`harmony-zenoh/src/keyspace.rs`) and the generic `SendUnicastToDevice`/`UnicastReceived` runtime plumbing. | Entire stack is client-only: `dm_*.rs` = **14,270 LoC** (`dm_outbox.rs` alone 356 KB), plus `iroh_friend_acceptor.rs` (131 KB), `iroh_butler_acceptor.rs`, `dm_inbox_*`, `dm_outhold_*`. | **keep-client (no dup).** Not duplication — it's client-exclusive. Lives correctly in the client today; candidate for a shared crate only if a second client appears. |
| **Community / channel-log / voting / relay** | **No equivalent** (only `harmony-zenoh/src/namespace.rs` conventions). | Massive client-only stack: `community_*` + `owner_state_*` (hundreds of KB). | **keep-client (no dup).** Client-exclusive application layer. |
| **CAS / content store** | `harmony-content::cache::ContentStore` + `storage_tier` (the actual cache, owned by `NodeRuntime`). | `content_store.rs`: defines its own `ContentStore` **trait**, but the production impl `RuntimeContentStore` (`:162`) **delegates back into the runtime's `StorageTier`** via a `CasOp` channel. `InMemoryStub` is test-only. | **keep-core store, keep-client adapter.** Genuine reuse: the client adds an async/serve-allowlist adapter (`CommunityServeAllowlist`, ZEB-395) over the core cache. Not a duplicate store. |
| **Identity / crypto** | `harmony-identity` (`PrivateIdentity`, `Identity`), `harmony-owner` (certs/trust/recovery), `harmony-crypto` (aead/hkdf/ml_dsa/ml_kem). | `identity.rs` (3850 LoC) + `owner_state_crypto.rs`, `dm_crypto.rs`, `voice_crypto.rs`, `community_dfrost_crypto.rs`. Wraps core identity/owner; **replicates** a few `harmony_crypto` helpers because it isn't a direct dep (`dm_signing.rs:713-715`). | **keep-core primitives.** Minor smell: client replicates small crypto helpers (an hkdf wrapper) to avoid taking a `harmony-crypto` dep. Cheap fix: add `harmony-crypto` as a direct dep and delete the replica. |
| **Discovery (announce records)** | `harmony-discovery` (sans-I/O, embedded in runtime) + `harmony-pkarr` (DHT). | Uses `harmony-pkarr` directly (real reuse) + a client-side `reachability_*` / `pkarr_*_publisher.rs` layer for iroh reachability records. | **keep-both.** pkarr reused cleanly; reachability layer is client-specific glue over iroh, no core dup. |

---

## (c) Dangling Edges — client emits/expects X from core, X is dropped/stub/absent

The runtime can emit **23 distinct `RuntimeAction` variants**; the client's dispatcher (`event_loop.rs:5723-5900`) handles **8** and silently swallows the rest via `_ => {}` (`:5899`). The dropped variants are the entire peer/tunnel/replication/inference/DSD surface that `harmony-node` *does* execute.

**Handled by client (8):** `SendOnInterface`, `Publish`, `DeclareQueryable`, `Subscribe`, `FetchContent`, `FetchModule`, `SendReply` (**stub** — `:5894` logs "not yet implemented"), `UnicastReceived` (intercepted earlier at `:5536` for DM dispatch).

**Dropped by client (0 client references — confirmed via grep):**

| Dropped action | Emitted by core at | Consequence in client |
|---|---|---|
| **`InitiateTunnel`** | `runtime.rs:1631`, `:4290` (PeerManager wants a Reticulum-over-iroh tunnel) | **The flagship dangling edge.** The embedded runtime decides to dial a peer; the client never does. Real peer connectivity comes from `iroh_dial_driver` + ALPN acceptors instead. The runtime's whole peering brain is inert. |
| `CloseTunnel` | core peer mgr | dropped |
| `SendPathRequest` | core peer mgr | dropped |
| `QueryMemo` | runtime memo fetch | dropped (client has no memo layer) |
| `SendVerifyQuery` | runtime DSD | dropped |
| `ReplicaPush` / `ReplicaPullResponse` | runtime replication | dropped (client built its own butler/relay deposit acceptors instead) |
| `RunInference` | runtime tier-3 | dropped (client config sets no inference CIDs) |
| `PersistToDisk` / `DiskLookup` / `RemoveFromDisk` / `PersistToArchive` / `CascadeToArchive` / `ArchiveLookup` / `RemoveFromArchive` / `S3Lookup` | runtime storage tiers | never emitted — client `NodeConfig` sets `disk_enabled:false, archive_enabled:false, s3_enabled:false` (`lib.rs:7513-7521`), so these are dead-by-config rather than dropped. |

**Other dangling/vestigial edges:**
- **`SendReply` is a stub** in both core-node-adjacent client code and the client (`event_loop.rs:5894`) — content/stats query replies are silently dropped. (Cross-ref: known root gap @ historical `event_loop.rs:2813` SendReply stub.)
- **`SendOnInterface` → LAN broadcast only.** The Reticulum "interface" is a single UDP broadcast on port 4242 (`event_loop.rs:5731`, `:957`, `RETICULUM_UDP_PORT`). It is LAN-discovery-only, not WAN routing. `reticulum_identity_bytes` is populated (`lib.rs:3113`) but the code itself notes "the Reticulum (device #1) identity is **unused**" (`lib.rs:20808`). Setting `HARMONY_RETICULUM_PORT=0` disables it entirely (`event_loop.rs:939`).
- **`RuntimeEvent::SendUnicastToDevice`** is pushed by the DM outbox (`dm_outbox.rs:173`, `event_loop.rs:3622`) → becomes `UnicastReceived`/`SendOnInterface`, i.e. it rides the vestigial UDP path. Real DM delivery is the iroh butler/friend acceptors, not this.

---

## (d) Recommended Consolidation Direction (per area)

1. **Transport event loop — DELETE or DOWNGRADE `harmony-node`'s loop; canonicalize the client's.**
   harmony-node is a frozen, undepended-on binary implementing an abandoned transport design (Reticulum-over-iroh tunnels + plain Zenoh). The client's `event_loop.rs` + `iroh_endpoint.rs` + `zenoh_iroh_transport.rs` are the tested, shipped, daily-maintained truth. **Back-port client → core**: if core needs a node, extract the client's Zenoh-over-iroh + ALPN-acceptor design into a shared `harmony-transport`/`harmony-node-v2` lib that BOTH the client and any headless node import. Until then, harmony-node is a confusion magnet — flag it as reference-only or remove it from the default workspace build.

2. **Zenoh-over-iroh — promote the client's vendored fork into core.**
   The `vendor/zenoh-link` fork + `zenoh_iroh_transport.rs` is net-new capability core lacks. If core's transport future is "Zenoh-over-iroh pub/sub + Harmony CAS" (the stated target), this is the keeper and belongs in a shared crate, not vendored inside the Tauri app.

3. **Prune the runtime's dead action surface for the client's deployment.**
   The runtime emits `InitiateTunnel`/`CloseTunnel`/`SendPathRequest`/`QueryMemo`/`SendVerifyQuery`/`ReplicaPush`/`ReplicaPullResponse`/`RunInference` that the client structurally cannot consume. Either (a) gate these behind runtime features the client disables, or (b) if Reticulum is being removed (per the stated direction), delete the harmony-peers/harmony-reticulum/harmony-tunnel tunnel-initiation path from the runtime entirely. The `_ => {}` swallow at `event_loop.rs:5899` is currently hiding this mismatch.

4. **Reticulum/UDP path — remove.**
   The UDP broadcast "interface" is LAN-discovery-only and self-described as unused. With iroh+Zenoh as the real transport and Reticulum slated for removal, tear out the UDP socket, `RETICULUM_UDP_PORT`, `reticulum_identity_bytes`, and `SendOnInterface` handling. This eliminates a whole class of "why isn't my message routing over Reticulum" wild-goose-chases.

5. **CAS / identity / owner / pkarr — leave as-is (clean reuse).**
   These are correct, load-bearing dependency edges. Only minor cleanup: add `harmony-crypto` as a direct client dep and delete the replicated hkdf helper (`dm_signing.rs:713-715`).

6. **DM / community / voting stacks — not duplication.**
   They are client-exclusive application code with no core counterpart; no consolidation needed unless a second client emerges.

---

## Evidence index (load-bearing file:line)

- Client runtime coupling = 4 type names: `lib.rs:7`, `event_loop.rs:16`.
- Runtime is `!Send`, runs on its own thread: `lib.rs:614`.
- `NodeConfig` (disk/archive/s3 all false): `lib.rs:7488-7522`. `NodeRuntime::new`: `lib.rs:7787`.
- Client action dispatcher (8 handled, `_ => {}` swallow): `event_loop.rs:5723-5900` (swallow at `:5899`).
- `SendReply` stub: `event_loop.rs:5894`. `SendOnInterface` → UDP broadcast: `event_loop.rs:5731`.
- Zenoh-over-iroh open: `event_loop.rs:9044-9054`; vendored fork: `src-tauri/vendor/zenoh-link` + Cargo `[patch.crates-io]`.
- iroh endpoint + 8 ALPNs: `iroh_endpoint.rs:120-134`.
- Reticulum identity "unused": `lib.rs:20808`; UDP port/disable: `event_loop.rs:909-957`, `:939`.
- Core RuntimeAction/RuntimeEvent enums: `harmony-runtime/src/runtime.rs:195` (events), `:403` (actions); `InitiateTunnel` emit: `:1631`, `:4290`.
- harmony-node real I/O: `harmony-node/src/event_loop.rs:398` (`zenoh::open`), `:561` (`Endpoint::builder().bind()`), `:2082` (`InitiateTunnel` exec); single ALPN `harmony-tunnel/1`.
- harmony-node undepended-on, frozen: only `harmony/Cargo.toml:28` lists it; last event_loop change `2026-05-03 e25a696` (tree-wide fmt) vs client/runtime `2026-06-14`.
- Client real-transport tests: `tests/community_reachability_two_engine_integration.rs` (spins two real `IrohEndpoint`s), `voice_dm_two_engine_integration.rs`, `voice_presence_two_engine_integration.rs`, `iroh_zenoh_registration_integration.rs`, `pkarr_iroh_redeem_full_integration.rs`. harmony-node has only 3 CLI-level integration tests and zero inline event_loop tests.
- Content store reuse (adapter over core cache): `content_store.rs:162` (`RuntimeContentStore`), `:30-52` (`CommunityServeAllowlist`).
- Crypto-helper replication smell: `dm_signing.rs:713-715`.
