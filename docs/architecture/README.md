# Harmony client architecture

A map of how the pieces fit together, for engineers arriving at this repo cold.
Read the [at-a-glance diagram](#the-client-at-a-glance) first; each numbered
section after it zooms into one layer. Per-feature design history lives in
[`docs/specs/`](../specs/) (200+ dated design docs — this document is the
entry point they hang off). Developer workflow (tests, CI, conventions) is
[`CLAUDE.md`](../../CLAUDE.md).

Everything below names real symbols and files, so you can jump from any box
to the code with a grep. Counts (commands, events) drift as features land;
treat them as orders of magnitude, and trust the *shape*.

---

## The client at a glance

One process, one node. A Svelte webview talks through a two-method adapter to
a Rust backend; the same backend is equally drivable headless over a localhost
API. All peer traffic — messages, membership, presence, voice, files — flows
over a single zenoh pub/sub session that rides iroh QUIC end-to-end.
There are no application servers: the only third parties are rendezvous
infrastructure (iroh relays for NAT traversal, pkarr relays for discovery),
which see ciphertext and routing metadata only.

```mermaid
flowchart TB
    subgraph FE["Svelte 5 frontend — src/"]
        UI["Views and modes<br/>Messages · Notes · Vines · Files …"]
        SVC["Service layer<br/>~40 plain-class services"]
        UI --> SVC
    end

    ADP["TauriAdapter seam<br/>two methods: invoke + listen"]
    SVC --> ADP

    subgraph BE["Rust backend — src-tauri/ (one process, one NodeState)"]
        IPC["Tauri IPC<br/>~290 commands"]
        API["localhost HTTP/WS API<br/>curated ~120-command subset"]
        IMPL["shared *_impl seams"]
        NS["NodeState hub<br/>+ engines: owner-state · community ·<br/>channel-log · voting · fleet datasets"]
        EL["event loop<br/>dedicated 'harmony-runtime' thread"]
        NR["NodeRuntime<br/>sans-I/O core from harmony crates"]
        IPC --> IMPL
        API --> IMPL
        IMPL --> NS
        NS --> EL
        EL --> NR
    end

    ADP -- "Tauri invoke" --> IPC
    AGENT["headless agents · api CLI ·<br/>e2e-harness · fleet tooling"] -- "HTTP + WS" --> API

    subgraph NET["network"]
        ZEN["zenoh session<br/>pub/sub + queryables"]
        IROH["iroh QUIC endpoint<br/>11 ALPNs"]
        ZEN --> IROH
    end

    EL --> ZEN
    IROH --> PEERS(("peers"))
    PKARR["pkarr DHT relays<br/>peer discovery"] -.-> IROH

    SINK["NodeEventSink"]
    NS --> SINK
    SINK -- "tauri emit" --> ADP
    SINK -- "WS /v1/events" --> AGENT
```

Key invariants to carry through the rest of the doc:

- **One node, two front doors.** `harmony-app` (GUI) and `harmony-app serve`
  (headless) run the *same* node; they differ only in which state-access and
  event-sink implementations the API server is handed (§2).
- **Single-writer everywhere.** Contended state is owned by exactly one
  engine/task and mutated through it; the shared `NodeState` mutex is a
  hand-off point, not a place work happens (§3).
- **Every durable plane is a log or CRDT with its own replication discipline**
  — there is no general database (§4).
- **The transport is client-owned.** Zenoh-over-iroh exists because the client
  vendors a `zenoh-link` fork; the harmony-core runtime is embedded only as a
  sans-I/O brain (§5).

---

## 1. Frontend — Svelte 5 + a plain-TypeScript service layer

**Where:** `src/`. Entry `src/main.ts` mounts `App.svelte`; a second
entry `src/network-main.ts` mounts the dev-only network-viz window.

`App.svelte` is deliberately the hub: it constructs the one real adapter,
boots the node, instantiates and cross-wires ~40 services, and hosts every
modal. Components below it are largely presentational.

```mermaid
flowchart LR
    subgraph modes["App modes (bottom-left rail)"]
        M1["messages<br/>(+ Notes selection)"]
        M2["vines"]
        M3["files"]
        M4["hidden by default:<br/>spellbook · mail · mint · network"]
    end
    NAV["NavPanel<br/>tree · unread badges · footer chips"]
    LAYOUT["Layout.svelte<br/>pure CSS-grid slot host"]
    modes --> LAYOUT
    NAV --> LAYOUT

    subgraph services["src/lib/ services (plain classes)"]
        S1["message / channel-message /<br/>community / nav / friend …"]
        S2["presence · member-card ·<br/>unread · voting adapter"]
        S3["voice-session · call-session ·<br/>group-call-session (singletons)"]
    end
    LAYOUT --> services
    services -- "invoke(cmd, camelCase args)" --> T["Tauri IPC"]
    T -- "listen: ~50 event names" --> services
```

The load-bearing ideas:

- **The entire frontend↔backend contract is two methods.**
  `TauriAdapter { invoke(cmd, args); listen(event, handler) }` (declared in
  `src/lib/zenoh-service.ts`). Every service takes this interface; the sole
  real construction site is `App.svelte`'s boot IIFE. Rust declares params
  `snake_case`, JS sends `camelCase`; Tauri converts (see `CLAUDE.md`).
- **Services are framework-free classes; reactivity is bolted on at the App
  boundary** — either a snapshot copy (`navNodes = [...navService.nodes]`)
  or a `$state` version counter (`cardVersion++`) that `$derived` reads
  depend on. This keeps services unit-testable against a mock adapter.
- **Browser mode is designed in.** With no Tauri present (`isTauri()` false —
  e.g. plain `npm run dev`), services keep their pre-seeded mock data
  (`src/lib/mock-data.ts` and friends) and the app renders fully. On connect,
  services delete exactly the mock keys *before* registering listeners.
- **Modes:** `AppMode` in `src/lib/types.ts` lists seven; feature flags
  (`src/lib/feature-flags.ts`) hide spellbook/mail/mint/network by default,
  so the shipped alpha surface is Messages (+ Notes), Vines, Files.
- **There is no frontend WebSocket adapter.** Headless parity is achieved
  Rust-side at the event sink (§2), not by teaching the UI a second backend.

Sharp edge: `App.svelte`'s boot ordering is load-bearing in several places
(unread services before their feeders connect; own-address fetch before DM
rehydration). The file documents each with the bug it prevents — read the
comments before reordering.

## 2. One node, two front doors — the IPC boundary and the headless API

**Where:** `src-tauri/src/api/` (`mod.rs`, `rpc.rs`, `gui_host.rs`,
`events.rs`, `auth.rs`, `lock.rs`, `cli.rs`, `watch.rs`) and
`src-tauri/src/node_event_sink.rs`.

```mermaid
flowchart LR
    FE["webview"] -- "invoke" --> TC["tauri command wrappers<br/>(~290, generate_handler)"]
    AG["agent / api CLI / harness"] -- "POST /v1/rpc/:cmd<br/>bearer token" --> REG["RpcRegistry<br/>curated ~120 commands"]
    TC --> IMPL["the same *_impl functions"]
    REG --> IMPL
    IMPL --> NS["NodeState + engines"]
    NS --> SINK["impl NodeEventSink for AppHandle"]
    SINK -- "tauri emit" --> FE
    SINK -- "mirror to broadcast" --> WS["WS /v1/events firehose"]
    WS --> AG
```

- **The HTTP surface is a curated subset, not a mirror.** Each RPC
  registration in `rpc.rs` is a deliberate line added per ticket; a few verbs
  exist *only* headless. The two surfaces also intentionally diverge in
  strictness (`deny_unknown_fields` on HTTP only — independently-versioned
  clients must fail loudly on skew) and error shape.
- **Mode-agnosticism is one trait.** `NodeStateAccess` has two impls:
  `serve` owns the `Arc<Mutex<NodeState>>`; the GUI borrows Tauri's managed
  state (`GuiStateAccess`). The GUI opts into hosting the same API by setting
  `HARMONY_API_PORT` (`gui_host.rs`).
- **Event parity lives at exactly one place.** `impl NodeEventSink for
  AppHandle` mirrors every emission onto the WS broadcast *at the sink*, so
  no emission site — current or future — can miss the agent-visible stream.
  The webview and the WS firehose see one vocabulary (~50 event names).
- **Discovery and mutual exclusion:** the server writes `api/token` (0600)
  and `api/port` under the profile's data dir; `api/serve.lock` (fd-lock) is
  the one-node-per-profile boundary that a GUI-with-API and a `serve` contend
  for.
- **`harmony-app api`** is a thin HTTP/WS client over those discovery files;
  **`harmony-app watch`** turns the event firehose plus backfill into
  resumable NDJSON per channel (HLC cursor).

## 3. Core runtime — boot, the event loop, and the single-writer discipline

**Where:** `src-tauri/src/lib.rs` (`start_node_inner`, `stop_inner`),
`src-tauri/src/event_loop.rs`, plus one engine module per state plane.

There is no type named "kernel". The split people mean by "engine/kernel" is
three layers:

```mermaid
flowchart TB
    subgraph L3["App-side engines — contended state, callers write THROUGH them"]
        E1["owner-state SyncEngine"]
        E2["CommunitySyncEngine<br/>(one per community)"]
        E3["ChannelLogEngine<br/>(one per channel)"]
        E4["VotingLogEngine"]
        E5["FleetSyncEngine ×10<br/>(notes, dm-inbox, relay-hold …)"]
    end
    subgraph L2["I/O driver — event_loop::run"]
        EL["owns the zenoh session, the 250ms tick,<br/>and every request-channel rx<br/>(one 'harmony-runtime' OS thread)"]
    end
    subgraph L1["Sans-I/O core — harmony-runtime crate"]
        NR["NodeRuntime: pure state machine,<br/>tier-2/3 content+compute bookkeeping only"]
    end
    IPCH["IPC / RPC handlers"] -- "bounded mpsc channels" --> EL
    L3 -- "adapter request channels" --> EL
    EL -- "push_event / tick" --> NR
    NR -- "RuntimeAction (6 of 24 handled)" --> EL
```

- **`NodeState`** (`lib.rs`) is a ~190-field struct behind one `std::Mutex` —
  the hand-off hub. Handlers lock, clone the `Arc`s they need, drop the
  guard, then await. The documented cross-lock order for the DM path is
  `dm_outbox → crdt_state → hlc_tracker`.
- **The event loop owns the network.** `event_loop::run` takes `NodeRuntime`
  *by value* on a dedicated 8 MiB OS thread (single-worker multi-thread
  tokio — zenoh's `.wait()` panics on a current-thread scheduler). Its
  `select!` arms service the timer, zenoh events, and a dozen bounded
  request channels (publish, fetch, ingest, CAS ops, voice, mail …).
- **The core runtime is a brain, not a transport.** The client handles only
  NodeRuntime's zenoh-facing actions (publish / subscribe / queryable /
  content fetch) and drops the rest — tunnel lifecycle, replication, disk
  tiers — with an explicit `_ => {}`, because the client runs its own tunnel
  and storage tiers (§5, §6).
- **Boot** (`start_node_inner`, ~11k lines) is stop-first, then: identity +
  vault unlock → owner state load → iroh endpoint + accept loop → *register
  the iroh link factory before `zenoh::open`* → per-plane engines and
  acceptors → spawn the runtime thread → await the loop's ready signal →
  post-ready reconciles (relay pools, discoverability) under a generation
  guard. A monotonic `install_seq` + `generation` pair makes concurrent
  start/stop races detectable.
- **Shutdown** (`stop_inner`) tears down in reverse dependency order —
  channel logs before community engines (verify-on-receive needs membership),
  iroh endpoint *last* (a bare Arc-drop leaks the relay actor) — and every
  engine honors a `closing` flag checked **under the same lock its flush
  takes**: local mints fail loudly (`EngineShuttingDown`), re-fetchable
  inbound drops silently.
- **The relay-acceptor watchdog** (`relay_acceptor_watchdog.rs`) is a pure
  decision core with a tiered ladder — hold → probe network (in-place
  `network_change()`) → gen-guarded node restart → escalate — gated on
  *unserved demand*, never raw staleness, so an idle node never self-restarts.
  Its memory is process-global on purpose: a tier-2 restart re-spawns the
  watchdog itself.

## 4. State and persistence — logs, CRDTs, and two disk roots

**Where:** one module pair per plane (`*_crdt.rs` / `*_sync.rs` /
`*_persist.rs`), `identity.rs` (vault), `content_store.rs`.

**Two disk roots, never merged:**

```text
IDENTITY root  ~/.harmony[/profiles/<p>]/          # vault + every CRDT (*.cbor)
  identity.enc                    # HRMI vault: node seed, iroh key, device key, owner master seed
  owner_state_crdt.cbor           # the owner-state CRDT + its replay tracker sibling
  notes.cbor, dm_inbox.cbor, …    # one file pair per fleet dataset
  communities/<id>/               # per community:
    crdt.cbor · replay.cbor · voting.cbor · segments.cbor
    channels/<id>/  manifest.cbor · tail.cbor · segments/NNNNNNNN.cbor

APP-DATA root  <data_dir>/net.zeblith.harmony[/profiles/<p>]/   # non-CRDT stores
  content-index.json · storage_records.json · storage_ledger.json
  connectivity-settings.json · vine_feed.json · follows.json
  mail/ (index + blobs) · avatars/ · mint/ (SQLite)
  api/  (token · port · serve.lock)      logs/
```

The planes and their disciplines:

| Plane | Type | Replication | Persistence |
|---|---|---|---|
| Owner state | merge-CRDT (`owner_state_crdt::OwnerState`: spaces, outbox/inbox, friend graph, device cache, grants…) | `FleetSyncEngine` — own devices only, sealed under the owner key tree | `owner_state_crdt.cbor` |
| Fleet datasets ×10 | small merge-docs (notes, dm-inbox, relay-hold, fleet-net, quorum…) | same generic `FleetSyncEngine<S>`, topic `harmony/owner/{addr}/ds/{tag}` | one cbor pair each |
| Community state | **verified append-only event log** + materialized cache (`CommunityState`) — membership, channels, governance events | per-community engine; state-root publish + prepare/resolve/apply ingest | `communities/<id>/crdt.cbor` |
| Channel logs | per-channel signed event log (tail + sealed segments) | live pub/sub + RBSR set-reconcile + since-backfill | `channels/<id>/…` |
| Voting log | per-community signed event log + materialized polls | live pub/sub + RBSR + full-dump backstop | `voting.cbor` (+ `poll_restore` overlay) |
| Content | CAS (SHA-256-truncated CIDs, FastCDC chunking) | zenoh queryable `harmony/content/…`, gated by a serve-allowlist | runtime cache + sidecar `content-index.json` |

Cross-cutting mechanisms worth internalizing:

- **The mutation pipeline is uniform:** mutate through the engine →
  `notify_dirty()` → 250 ms debounce → publish + persist (atomic
  write-rename). *A local CRDT mutation that skips `notify_dirty` is lost.*
  Community state splits its persists three ways (both files / crdt-only when
  publish failed / replay-only) so an unpublished HLC advance is never
  recorded as published.
- **One HLC, several roles.** `NodeState.hlc_tracker` is a single
  `Arc<Mutex<ReplayTracker>>` shared by owner-state replay, DM minting, and
  channel-log/voting stamping — one reservation seam, atomic under one lock.
  Per-community and per-channel replay trackers are *separate* keyspaces.
  A session-scoped `HlcAdoptFloor` bounds forward clock adoption.
- **Ingest is verify-then-apply, with the replay tracker advanced only on
  successful apply** — so a fetch miss is safely retryable (the ZEB-805/937
  prepare → resolve → apply split moves network fetches off the single-writer
  task while keeping admission on it).
- **The vault** (`identity.rs`) is one encrypted envelope (XChaCha20 +
  Argon2id) holding all root secrets; OS-keychain backend for the default
  profile, encrypted-file backend (passphrase env) for named profiles —
  keychain names are machine-global, which is why *named profiles are
  file-vault-only by construction* and tests refuse the real keychain.
- **Serving encrypted content is opt-in per CID.** The serve queryable
  answers an encrypted CID only if the process-local `CommunityServeAllowlist`
  (a 30-day lease map) contains it — and a refusal is *silence*, not an
  error, so "no successful reply" on a fetch is indistinguishable from
  "nobody has it" by design.

## 5. Transport — zenoh-over-iroh, admission, and discovery

**Where:** `src-tauri/src/iroh_endpoint.rs`, `zenoh_iroh_transport.rs`,
`zenoh_iroh_link.rs`, `iroh_zenoh_registration.rs`, `reconnect_supervisor.rs`,
`admission_oracle.rs`, `reachability_publisher.rs`, `pkarr_*.rs`, and
`src-tauri/vendor/` (two vendored crates).

```mermaid
flowchart TB
    PLANES["data planes<br/>owner datasets · community state · channel logs ·<br/>voting · presence · CAS · voice · mail"]
    ZEN["zenoh session<br/>scouting off · deterministic zid ·<br/>peer mode (router = opt-in linkstate hat)"]
    FORK["vendored zenoh-link fork<br/>adds LinkKind::Iroh + factory hook"]
    LM["IrohZenohLinkManager<br/>accept loop = ALPN demux ·<br/>per-peer conn registry (swap + drop-watch)"]
    EP["iroh QUIC endpoint<br/>persistent Ed25519 node key · 11 ALPNs"]
    N0["iroh relays (n0 or custom)<br/>NAT-traversal fallback"]

    PLANES --> ZEN --> FORK --> LM --> EP --> N0

    DIRECT["direct ALPN protocols (not zenoh):<br/>tunnel v1/v2 · butler-deposit · community-relay<br/>deposit/pull · handshake · friend · friend-pex ·<br/>vine-relay · ping"] --> EP

    SUP["reconnect supervisor<br/>the single dial authority"] -- "bounded dials (4) ·<br/>admission-oracle veto → Dormant" --> LM
    RES["ReachabilityResolver"] --> SUP
    PK["pkarr DHT relays<br/>(pkarr.q8.fyi first)"] -. "publish + resolve<br/>reachability records" .-> RES
```

- **Why a fork:** zenoh 1.x has no seam for a custom unicast transport
  (closed `LinkKind` enum). The vendored `zenoh-link` adds an `Iroh` variant
  and a process-global factory hook — "we don't inject a manager; we become
  the dispatch." The second vendored crate, `netdev`, removes a ~44 s
  macOS CoreWLAN stall from every first `Endpoint::bind()`.
- **One accept loop, eleven ALPNs.** `harmony/zenoh/v1` carries *all* zenoh
  traffic; the other ten are direct framed protocols (DM tunnel, the two
  store-and-forward rungs, invite/open-join handshake, friend + PEX,
  public vine-relay, ping). Inbound admission is layered cheapest-first —
  per-source rate shields, per-source concurrency caps, global semaphores,
  boot-window queues — each with RAII permits held for the session's life.
- **Dialing has exactly one enforcement point.** Every trigger (resolver
  learn/change, drop-watchers, boot seeding, presence sweeps) just *kicks*
  the reconnect supervisor; its dispatch pass applies the admission-oracle
  veto (router-mode bounded-degree; peers admit everything), the record gate,
  and the concurrency bound — vetoed peers park as Dormant, they are never
  filtered upstream.
- **Discovery is harmony-owned, not iroh-owned.** No iroh discovery service
  is registered. Peers publish signed reachability records (node id, home
  relay, filtered direct addrs, butler set) into pkarr DHT relays under
  per-case derived keys (invite / identity / community / friend / vines),
  re-derived every epoch; the resolver seeds the supervisor.
- **Three unrelated "relay" concepts** — keep them apart: (a) *iroh relays*
  (NAT-traversal fallback for QUIC), (b) *pkarr relays* (discovery
  rendezvous), (c) *harmony community-relays and butlers* (application-layer
  store-and-forward peers with their own ALPNs, §6).
- The embedded core runtime's tunnel/replication actions are inert (§3):
  what actually runs is this client-owned stack. Anti-entropy for the two
  high-churn logs (channel + voting) is RBSR set-reconcile with a
  fingerprint-bisect protocol and a full-sync fallback ladder.

## 6. Feature subsystems

Each subsystem is a thin vertical: UI surface → IPC family → engine/module →
one of the transport primitives above. The interesting couplings:

**Messaging (channels).** Posts are Ed25519-signed events in the per-channel
log, sealed with a key derived from the community epoch key. Ordering and
dedup key is `(wall_ms, logical, device_id, element_hash)`. Live pub/sub +
RBSR + backfill heal divergence; a replay-drop counter plus same-key ordering
sentinel make delivery testable.

**DMs.** Always-deposit, opportunistically-live:

```mermaid
flowchart LR
    OUT["DmOutbox drain<br/>(250ms tick, 3-phase)"]
    T["rung 0 — live PQ tunnel<br/>harmony/tunnel (ML-KEM + ML-DSA)"]
    B["rung 1 — butler deposit<br/>recipient's own online device"]
    R["rung 2 — community relay hold<br/>sealed blob, relay cannot open it"]
    OUT --> T
    OUT --> B
    B -- "only if butler did not ack" --> R
    IN["recipient CRDT inbox<br/>dedups tunnel copy vs deposit copy"]
    T --> IN
    B --> IN
    R -- "pull driver" --> IN
```

The tunnel transport *always* returns an error by contract — `Transient`
(live attempt in flight, grace one backoff window) vs `TransientNoLiveAttempt`
(deposit immediately) — because returning `Ok` would steer into a
weaker-durability path. Group DMs are the same path with a different
membership gate; there is no separate group message pipeline.

**Invites & join.** One-time invite links carry a signed token; one-time-ness
is enforced twice — a local claim-bound insert, and the authoritative
*convergent* fence at materialization (earliest countersign per token wins).
Countersigning of pending joins is automatic for any member above the invite
power threshold; there is no manual approve. First contact runs over the
handshake ALPN (invite redemption and tokenless open-join), with pkarr
resolving the inviter/community rendezvous.

**Presence — three notions, never merged.** (1) Community roster dots:
sealed signed beacons every 10 s on a presence topic, in-memory map, TTL
sweep; the three-state dot (solid/hollow/offline) is computed *frontend-side*
from beacon age. (2) Transport liveness: iroh path events fused with
supervisor state, rendered in the footer chip ("N peers" = live transport
sessions, not roster). (3) Device-liveness certs: hourly self-signed,
replicated in owner state, shown in the Devices panel.

**Voice (and Town Hall).** Not WebRTC. Opus runs in the webview
(AudioWorklet capture/playback, jitter buffer, VAD, PTT); Rust is a sealing
relay — AEAD + per-frame signature + zenoh put/sub — and never sees PCM.
Moderation directives are re-verified by *every* receiver (power > target
for punitive actions) and re-asserted on a TTL so they lapse when the issuer
stops. Town Hall is a third `ChannelKind` on the same stack plus raise-hand
beacon bits and Tier-1 motion polls; only its dominant-speaker spotlight is
local inference. Call outcomes become durable via a typed system DM
(`application/x-harmony-call-event+json`) written once by the caller.

**Files & sharing.** Channel attachments: whole-blob sealed under the
community epoch key, CAS-ingested serveable, referenced by CID inside the
*signed* post; fetch authorizes by "CID must be referenced in this channel's
log". Friend file grants: per-file DEK sealed per grantee device, granted /
lazily-revoked as an LWW element set in owner state. Storage buddies: a
reciprocal signed-pledge pact with dual-signed (device + binding countersig)
records, physical-CID refcounting, and a pure pin planner.

**Governance.** Three voting tiers on the per-community voting log — Approval
(fixed-window), Conviction (continuous charge/decay with delegation and a
24 h contestability window; its finalize is the one *execution* path, minting
real membership `SetPower` events, routed through M-of-N admin proposals when
admin-affecting), and Sortition (VRF-selected mini-public → deliberation →
drafting → STAR ratification, optional threshold-ElGamal secret ballots).
Sortition randomness comes from D-FROST threshold Schnorr on Ristretto —
whose zenoh adapter is not yet wired, so multi-node Tier-3 completion is
pending. The "Charter" view is a *projection* of materialized membership +
finalized Tier-3 polls — there is no stored charter document. The exit
option: any member can fork a community (with a mandatory reason), and the
fork is a signed lineage event in the parent's log.

**The rest.** Mail is a plaintext, gateway-mediated inbox (separate SMTP
gateway publishes a CAS Merkle tree; local state never syncs) — nothing like
DMs. Notes are a self-only fleet-synced LWW dataset. Vines are deliberately
public short-video broadcasts (unencrypted zenoh mesh + an unauthenticated
pull relay for followers of offline creators). Spellbook has no Rust at all —
it loads an optional sibling-repo WASM module. "Submit Feedback" opens a
pre-filled GitHub issue in the browser; the app transmits nothing itself.

## 7. The headless / agent-testing stack

**Where:** `e2e-harness/` (Rust, cross-platform, the one that matters),
`e2e/` (Playwright-over-CDP, Windows-only, human-triggered), plus the
`serve` / `api` / `watch` subcommands from §2.

```mermaid
flowchart LR
    H["e2e-harness tests<br/>(scenarios s1..s15)"]
    N1["harmony-app serve --api-port 0<br/>(temp HOME, file vault,<br/>fresh profile per node)"]
    N2["second node …"]
    H -- "spawn + discover port/token" --> N1
    H --> N2
    H -- "HTTP /v1/rpc" --> N1
    H -- "WS /v1/events → await_event" --> N1
    N1 <-- "real zenoh-over-iroh traffic" --> N2
```

- The harness spawns *real* release/debug binaries (with a freshness gate
  against source mtimes), isolates each node via temp `HOME` +
  `HARMONY_DATA_DIR` + `HARMONY_PASSPHRASE`, and drives them over the same
  HTTP/WS surface any agent uses. `poll_until` + WS `await_event` replace
  sleeps.
- The GUI itself is equally drivable: launch with `HARMONY_API_PORT` and the
  full RPC surface + event firehose light up next to the webview — this is
  how the [visual tutorial](../tutorial/README.md) was produced.
- `e2e/` (Playwright) attaches over CDP to a running WebView2 `tauri dev` —
  the only path that exercises the webview side of the boundary, and
  Windows-only because WKWebView exposes no CDP.

---

## Repo map

| Path | What lives there |
|---|---|
| `src/` | Svelte 5 frontend: `App.svelte` hub, `lib/` services + components |
| `src-tauri/src/lib.rs` | node lifecycle, `NodeState`, most IPC impls (large by design — seams are extracted as they stabilize) |
| `src-tauri/src/event_loop.rs` | the I/O driver: zenoh session + runtime thread |
| `src-tauri/src/api/` | the localhost control surface (§2) |
| `src-tauri/src/*_crdt.rs, *_sync.rs, *_persist.rs` | one triple per state plane (§4) |
| `src-tauri/src/community_*.rs` | membership, channels, voting, forks, relays |
| `src-tauri/vendor/` | `zenoh-link` fork (iroh transport) + patched `netdev` |
| `src-tauri/Cargo.toml` | ~14 harmony-core crates pinned to one lockstep git rev |
| `e2e-harness/` | headless two-node integration suite |
| `docs/specs/` | dated per-feature design docs — the detailed history |

## Keeping this document honest

Diagrams rot. When a structural change lands (a new engine, a new ALPN, a
moved boundary), update the affected section in the same PR — the diagrams
are Mermaid source in this file, so the diff reviews like code. Numbers
(command counts, event counts) are deliberately approximate; symbol and file
names are exact and greppable, and are the things worth fixing when they
drift.
