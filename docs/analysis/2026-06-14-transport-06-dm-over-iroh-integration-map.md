# DM-over-iroh integration map (Move 1a: repurpose the PQ `harmony-tunnel` session, driven from harmony-client)

Read-only forensic map (2026-06-14). Companion to `…-00-SYNTHESIS` … `…-05`. Feeds the
DM-over-iroh design spec. Every integration point is tagged:

- **[REUSE]** — usable as-is, no change.
- **[ADAPT]** — existing pattern/code is the template; adapt it (new ALPN, new key, new arm).
- **[NEW]** — needs net-new code.

Repos: harmony core `/Users/zeblith/work/zeblithic/harmony` (ignore `.worktrees/`);
client `/Users/zeblith/work/zeblithic/harmony-client/src-tauri`.

---

## Section 1 — `harmony-tunnel` crate public API

Crate root: `harmony/crates/harmony-tunnel/src/`. `#![no_std]` + `alloc`, std feature default.
Pure **sans-I/O state machine** — it never touches sockets. Public surface (`lib.rs:1-13`):

```rust
pub use error::TunnelError;
pub use event::{TunnelAction, TunnelEvent};
pub use session::TunnelSession;   // also pub mod handshake / frame / replication
```

### (a) Perform the PQ handshake — `session.rs`  **[REUSE]**

Two constructors create a session AND drive the handshake; both are sans-I/O (return
`Vec<TunnelAction>` for the caller to put on the wire):

```rust
// session.rs:90  initiator
pub fn new_initiator(
    rng: &mut impl CryptoRngCore,
    local_identity: &PqPrivateIdentity,
    remote_identity: &PqIdentity,
    now_ms: u64,
) -> Result<(Self, Vec<TunnelAction>), TunnelError>
// → emits exactly [OutboundBytes{TunnelInit ~6381 B}]; state = Initiating

// session.rs:137  responder
pub fn new_responder(
    rng: &mut impl CryptoRngCore,
    local_identity: &PqPrivateIdentity,
    init_bytes: &[u8],
    now_ms: u64,
) -> Result<(Self, Vec<TunnelAction>), TunnelError>
// → emits [OutboundBytes{TunnelAccept ~5293 B}, HandshakeComplete{..}]; state = Active
```

The initiator finishes the handshake by feeding the accept bytes back in via the generic event
pump (`session.rs:192`), which returns `HandshakeComplete` and flips state to `Active`:

```rust
pub fn handle_event(&mut self, event: TunnelEvent) -> Result<Vec<TunnelAction>, TunnelError>
pub fn state(&self) -> TunnelState   // Initiating | Active | Closed
```

Handshake (`handshake.rs`): `TunnelInit` = `[ML-KEM ct 1088][ML-DSA-65 pk 1952][nonce 32][ML-DSA
sig 3309]` = **6381 B** (`TUNNEL_INIT_LEN`); `TunnelAccept` = `[pk 1952][nonce 32][sig 3309]` =
**5293 B** (`TUNNEL_ACCEPT_LEN`). Keys: HKDF-SHA256 over the ML-KEM shared secret, directional
`i2r`/`r2i`. AEAD = ChaCha20-Poly1305 (`harmony-crypto::aead`), AAD = remote NodeId
(`session.rs:262-302`), nonce = 64-bit counter. NodeId = `blake3(ML-DSA pubkey)` (`session.rs:33`).
Identity verification is built in (MITM check at `session.rs:228` compares accept pubkey to the
expected remote).

### (b) Send an app payload  **[REUSE for carry; ADAPT for a DM tag]**

```rust
// TunnelEvent (event.rs:5) — the three send variants, all symmetric:
SendReticulum   { packet:  Vec<u8>, now_ms: u64 }
SendZenoh       { message: Vec<u8>, now_ms: u64 }
SendReplication { message: Vec<u8>, now_ms: u64 }
// each → handle_send(tag, payload) → [TunnelAction::OutboundBytes{ encrypted }]
```

### (c) Receive an app payload  **[REUSE]**

```rust
// TunnelEvent::InboundBytes { data, now_ms }  → decrypt → one of:
// TunnelAction (event.rs:22):
ReticulumReceived   { packet:  Vec<u8> }
ZenohReceived       { message: Vec<u8> }
ReplicationReceived { message: Vec<u8> }
HandshakeComplete   { peer_dsa_pubkey: Vec<u8>, peer_node_id: [u8;32] }
OutboundBytes { data } | Error { reason } | Closed
```

Also `TunnelEvent::Tick{now_ms}` (jittered keepalive every 25-35 s; dead-peer at 110 s,
`session.rs:14-22`) and `TunnelEvent::Close`.

### FRAME ENUM — the load-bearing decision  **[ADAPT — needs a new DM variant]**

```rust
// frame.rs:10  — CLOSED enum, #[repr(u8)]
pub enum FrameTag { Keepalive=0x00, Reticulum=0x01, Zenoh=0x02, Replication=0x03 }
```

`FrameTag` is a **closed enum**; `from_byte` (`frame.rs:18`) rejects any unknown tag with
`UnknownFrameTag`. There is **no opaque/arbitrary app-payload variant.** A DM body would either
(i) be smuggled inside `FrameTag::Reticulum` (semantically wrong, and it is the very tag we are
deleting in Move 2), or (ii) **get a new `FrameTag::Dm = 0x04`** with a matching `SendDm` event +
`DmReceived` action wired through `handle_send`/`handle_encrypted_frame`/`handle_event`. **(ii) is
the clean path and is a small, mechanical change in `frame.rs` + `event.rs` + `session.rs`.** The
`payload: Vec<u8>` is already arbitrary bytes (the frame body is opaque to the tunnel), so the DM's
sealed+signed wire bytes drop straight in — only the *tag* is the gate. **This is the one required
`harmony-tunnel` API addition.**

### ALPN constant + value

**Not in the crate.** The ALPN string lives in the *driver* (harmony-node):
`harmony/crates/harmony-node/src/tunnel_task.rs:23`

```rust
pub const HARMONY_TUNNEL_ALPN: &[u8] = b"harmony-tunnel/1";
```

The client would re-declare its own constant (it does not depend on harmony-node) — recommend
matching the value or coining `b"harmony/tunnel/v1"` to fit the client's `harmony/<x>/v1` family.
**[NEW]** (a one-liner in the client's `iroh_endpoint::alpn` module).

### Identity / key inputs the handshake needs

- **Self:** `PqPrivateIdentity` (gives `signing_key()` ML-DSA-sk, `encryption_secret()` ML-KEM-sk,
  `public_identity()` for the ML-DSA pk). API at `harmony-identity/src/pq_identity.rs:171-327`.
- **Peer (initiator only):** `PqIdentity { encryption_key: MlKemPublicKey, verifying_key:
  MlDsaPublicKey }` (`pq_identity.rs:44`). Constructed from raw bytes via
  `PqIdentity::from_public_keys(kem_pk, dsa_pk)` or `from_public_bytes`.

Both are already mintable in the client — see §5 (the client holds its own `PqPrivateIdentity` at
`identity.rs:60`, and the friend handshake already transports the peer's `pq_dsa_pubkey` +
`pq_kem_pubkey`).

---

## Section 2 — How harmony-node drives the tunnel (reference pattern)

The **driver** is `harmony/crates/harmony-node/src/tunnel_task.rs` (per-connection async task) +
the orchestration in `event_loop.rs`. This is the "real" part the synthesis flagged.

### Per-connection task — `tunnel_task.rs`  **[ADAPT — this is the template]**

```rust
pub async fn run_initiator(conn: Connection, local_identity: &PqPrivateIdentity,
    remote_identity: &PqIdentity, bridge_tx: mpsc::Sender<TunnelBridgeEvent>,
    cmd_rx: mpsc::Receiver<TunnelCommand>, interface_name: String, connection_id: u64)   // :34
pub async fn run_responder(conn: Connection, local_identity: &PqPrivateIdentity,
    bridge_tx: …, cmd_rx: …, interface_name: String, connection_id: u64)                 // :168
```

Shape (directly reusable as a design template):
1. **Handshake phase** (`initiator_handshake`/`responder_handshake`, 15 s timeout): initiator
   `conn.open_bi()`, writes `TunnelInit` length-prefixed (4-byte BE), reads `TunnelAccept`, feeds
   it to `session.handle_event`. Responder `conn.accept_bi()`, reads init, builds session, writes
   accept. Wire framing helpers `write_length_prefixed`/`read_length_prefixed` (`:554`/`:583`).
2. **Main loop** `run_tunnel_loop` (`:283`): `tokio::select!` over (a) `FramedRead` +
   `LengthDelimitedCodec` (4-byte BE, cancel-safe) → `InboundBytes`; (b) `cmd_rx` →
   `Send{Reticulum,Zenoh}` ; (c) 10 s keepalive `interval` → `Tick`. Every returned action goes
   through `dispatch_tunnel_actions` (`:451`): two-pass — write all `OutboundBytes` first, then
   forward bridge events.

### Orchestration — `event_loop.rs`  (the dial→handshake→session→route plumbing)

- **Session map:** `tunnel_senders: HashMap<String, TunnelSender>` (`:515`) keyed by
  `interface_name = "tunnel-<hex of node_id[..8]>"`. `TunnelSender` (`tunnel_bridge.rs:52`) wraps
  the `cmd_tx` + a `connection_id` for stale-close detection. **[ADAPT — but key by peer, see §5
  open Q.]**
- **Inbound accept** (Arm 7, `:1246`): `incoming.accept()` → spawn QUIC handshake →
  `ReadyConnection{ remote_pq_identity: None }` (responder).
- **Spawn** (Arm 8, `:1303`): drains `ReadyConnection`, inserts a `TunnelSender`, spawns
  `run_initiator` (if `remote_pq_identity = Some`) or `run_responder`.
- **Outbound dial** (`:1748-1855`): on `RuntimeAction::InitiateTunnel` a `DeferredDial` is pushed
  (`:2082`, 0.5-4 s privacy jitter); when it fires, builds `iroh::NodeAddr` from
  `node_id`+`relay_url`, constructs the peer `PqIdentity` via `construct_pq_identity(dsa,kem)`
  (`:1762`), binds an **ephemeral** endpoint with `.alpns([HARMONY_TUNNEL_ALPN])`, `.connect(addr,
  ALPN)`, sends `ReadyConnection{ remote_pq_identity: Some, initiator_endpoint: Some }`.
- **Routing a payload to a session** — `dispatch_action::RuntimeAction::SendOnInterface`
  (`:1908`): if `interface_name.starts_with("tunnel-")` → `tunnel_senders.get(name)` →
  `sender.try_send_reticulum(raw)` (`tunnel_bridge.rs:90`).

### What's harmony-node-specific (do NOT carry over) vs reusable

- **Reticulum-carrier coupling [DROP]:** the *only* live send command is
  `TunnelCommand::SendReticulum` / `try_send_reticulum`; routing is decided by the runtime's
  Reticulum router emitting `SendOnInterface{interface_name="tunnel-…"}` with a *Reticulum* raw
  packet. The whole "runtime decides interface → string-keyed lookup" indirection is Reticulum-
  shaped. In the client, the DM outbox already resolves the recipient directly (§4), so the client
  drives `SendDm` by recipient, not by an interface string.
- **Reusable [ADAPT]:** the entire `tunnel_task.rs` task shape (handshake-over-bi-stream + select
  loop + length-prefixed framing + keepalive Tick + two-pass dispatch) and the
  `ReadyConnection`/`TunnelSender`/bridge-mpsc plumbing. This maps cleanly onto the client's own
  acceptor + dial conventions (§3).
- **Note:** harmony-node dials with a *throwaway ephemeral* endpoint per tunnel; the **client
  should reuse its one long-lived persistent endpoint** (so the peer dials us back by our stable
  EndpointId) — see §3.

---

## Section 3 — harmony-client iroh endpoint + ALPN wiring

### Endpoint ALPN set — `iroh_endpoint.rs`  **[ADAPT — add the tunnel ALPN]**

`new_with_secret` (`iroh_endpoint.rs:119-136`) binds ONE long-lived `iroh::Endpoint` (iroh 0.98,
`presets::N0`, persistent Ed25519 key from keychain/encrypted-file) with **8 ALPNs**:
`HARMONY_ZENOH_V1, HARMONY_HANDSHAKE_V1, HARMONY_PING_V1, HARMONY_FRIEND_V1,
HARMONY_FRIEND_PEX_V1, HARMONY_BUTLER_DEPOSIT_V1, HARMONY_COMMUNITY_RELAY_DEPOSIT_V1,
HARMONY_COMMUNITY_RELAY_PULL_V1` (constants in `mod alpn`, `:46-84`).

**Invariant (confirmed):** a new protocol needs **both** (1) its ALPN string added to the
`.alpns(vec![…])` bind list AND (2) a handler dispatched/installed in the accept loop. The
late-installed acceptors (butler, both community-relay) ARE in the bind list. So: add
`HARMONY_TUNNEL_V1` to `mod alpn` + the bind vec, AND wire a dispatch arm. **[NEW const + ADAPT
bind list]**

### Inbound dispatch (the acceptor router) — `zenoh_iroh_transport.rs`  **[ADAPT]**

`spawn_accept_loop` (`zenoh_iroh_transport.rs:356`) is the single `ep.accept()` loop. Per inbound
`Connection` it switches on `conn.alpn()`:
- `HARMONY_ZENOH_V1` → zenoh link;
- `HARMONY_HANDSHAKE_V1 | HARMONY_FRIEND_V1 | HARMONY_FRIEND_PEX_V1` → the installed
  `handshake_dispatcher` (a `MultiplexHandshakeDispatcher`, `iroh_friend_acceptor.rs:1628`, which
  re-reads `conn.alpn()` and routes via `route_handshake_alpn` `:1612`);
- `HARMONY_BUTLER_DEPOSIT_V1`, `HARMONY_COMMUNITY_RELAY_DEPOSIT_V1`,
  `HARMONY_COMMUNITY_RELAY_PULL_V1` → **late-installed** `OnceLock` acceptors (spawn handler if
  set, else `conn.close()`); `HARMONY_PING_V1` → `network_health::handle_ping_accept`.

A **tunnel ALPN acceptor** slots in as a new `else if conn.alpn() == HARMONY_TUNNEL_V1` arm using
the **late-installed `OnceLock` pattern** (butler/relay style is the closest fit — the tunnel
acceptor needs the node's `PqPrivateIdentity`, only available after identity boot). The inbound
handler is the `run_responder` analogue (§2): `conn.accept_bi()` → read `TunnelInit` →
`TunnelSession::new_responder` → write `TunnelAccept` → run the select loop; on a decrypted DM
frame, hand the sealed bytes to the inbound DM ingest path (§4).

### Late-installed acceptor registration — `IrohZenohLinkManager`  **[ADAPT]**

`IrohZenohLinkManager` (`zenoh_iroh_transport.rs:129`) holds the `OnceLock` acceptor slots +
`install_butler_deposit_acceptor` / `install_community_relay_deposit_acceptor` /
`install_community_relay_pull_acceptor` (each `set()`s a `OnceLock`). Add a
`tunnel_acceptor: OnceLock<Arc<…>>` field + `install_tunnel_acceptor(...)`, installed once at the
same boot point identity becomes available. **[NEW field + method, ADAPT the install call site.]**

Acceptor trait (all acceptors): `iroh_invite_acceptor.rs:165` —
`pub trait IrohHandshakeDispatcher { async fn handle_connection(&self, conn: Connection); }`.
Representative simple acceptor body: `iroh_pex_acceptor.rs:154` (`accept_bi` → read `[u32 LE len]
[body]` bounded + timeouts → respond → `finish`).

### Outbound dial — `iroh_dial_driver.rs` + the friend dial primitive  **[ADAPT]**

`iroh_dial_driver.rs` is **zenoh-specific** (drives `runtime.connect_peer` via `RuntimePeerDialer`,
`:122-217`) — **not** the right primitive for a tunnel. The right template is the **friend dial**
(`lib.rs:42101-42180`): pkarr `resolver.resolve_window(verifying_keys)` →
`ReachabilityAnnouncePayload` → build `iroh::EndpointAddr::new(node_id).with_relay_url(..)
.with_ip_addr(..)` → `iroh_endpoint.inner().connect(target_addr, ALPN)` → `conn.open_bi()`. Butler
deposit shows the same pattern more compactly (`butler_deposit.rs:455`:
`endpoint.inner().connect(addr, HARMONY_BUTLER_DEPOSIT_V1)` → `open_bi` → length-prefixed frame →
`finish`). A **tunnel dialer** is this pattern with the tunnel ALPN and the `run_initiator`
protocol after `open_bi`. **The dial uses the persistent endpoint** (`iroh_endpoint.inner()`), not
an ephemeral one — diverging from harmony-node intentionally (so the peer can dial us back). **[NEW
dialer fn, ADAPT from the friend/butler call sites.]**

---

## Section 4 — The client's DM send/receive path + the exact hook point

### Outbound DM — `dm_outbox.rs`

```rust
// dm_outbox.rs:40  the transport trait DMs go through
#[async_trait] pub trait DmTransport: Send + Sync {
    async fn send(&self, entry: &OutboxEntry, recipient: OwnerAddr,
                  destinations: Vec<[u8;16]>) -> Result<(), TransportError>;
}
// dm_outbox.rs:207  the live impl (Reticulum-bound)
pub struct RuntimeUnicastTransport {
    tx: mpsc::Sender<UnicastSendRequest>, self_owner: OwnerAddr,
    our_signing_device_hash: DeviceIdentityHash, signing_key: Arc<ed25519_dalek::SigningKey> }
// dm_outbox.rs:230  impl DmTransport for RuntimeUnicastTransport::send → signs + encrypts +
//   pushes one UnicastSendRequest per destination onto `tx`.

// dm_outbox.rs:74  recipient → device destination hashes
pub fn resolve_destinations(cache: &OwnerDeviceCache, recipient: OwnerAddr) -> Vec<[u8;16]>
//   per device: compute_dm_destination_hash(device_hash)  (the "harmony.dm" RNS hash)

// dm_outbox.rs:176  the DM unit at the transport boundary
pub struct UnicastSendRequest { pub destination_hash: [u8;16], pub packet: Vec<u8> }
```

### THE HOOK POINT  **[ADAPT — this is the precise seam]**

`RuntimeUnicastTransport::send` (`dm_outbox.rs:230`) builds the **sealed + signed** wire packet
(`build_signed_cidnotify` → `encode_packet`) and emits, per destination, a
`UnicastSendRequest{destination_hash, packet}` onto its mpsc `tx`. The event loop drains that
channel and re-emits it as the Reticulum egress event:

```rust
// event_loop.rs:~3614  the Reticulum egress (what DM-over-iroh replaces/supplements)
runtime.push_event(RuntimeEvent::SendUnicastToDevice {
    destination_hash: req.destination_hash, packet: req.packet });
```

**Two clean ways to hook (spec must choose):**
- **(A) New `DmTransport` impl** (cleanest): add an `IrohTunnelDmTransport` implementing the same
  `DmTransport` trait. `send()` gets `(entry, recipient, destinations)` — but for iroh we want the
  *recipient OwnerAddr → peer iroh node_id + peer PQ keys*, not the 16-byte RNS destination hash.
  So this impl ignores `destinations` and instead resolves the peer's reachability/PQ material
  (§5) and routes the already-built `packet` bytes over a tunnel. The `packet` is the **opaque
  sealed+signed DM** — exactly what `FrameTag::Dm` carries.
- **(B) Tee at the event-loop drain**: keep `RuntimeUnicastTransport` producing
  `UnicastSendRequest`, and at the drain site (event_loop ~3614) route to the tunnel instead of
  `SendUnicastToDevice`. Less clean (the `UnicastSendRequest` only carries the RNS hash, not the
  recipient OwnerAddr, so you'd lose the dial key). **(A) is recommended** — it keeps the
  OwnerAddr in scope, which is what you need to find the dial target.

**The DM unit is already opaque ciphertext** at this boundary (confirmed): outbound bytes are
`[disc][CBOR(signed_body)][64-byte sig]`, fully sealed (ChaCha20-Poly1305 over X25519-ECDH content
key) and Ed25519-signed before they reach any transport. The iroh carrier needs to move *bytes*,
nothing more. (`dm_crypto.rs:57` `encrypt_dm_message`, `dm_signing.rs:298` `sign_dm_packet`.)

### Inbound DM (symmetric receive side)

```rust
// inbound_packet.rs:28  discriminant peek: 0x10→community invite; 0x01-0x03→DM (caller handles)
pub async fn try_dispatch_community<H: AppHandleEmit>(…, packet_bytes: &[u8], app: Option<&H>) -> bool
// dm_inbox_ingest.rs:129  the inbox ingest entry (used by butler dm-inbox + normal path)
pub async fn ingest_pending(doc: &mut DmInboxDoc, ctx: &dyn DmInboxIngestCtx) -> bool
// dm_outbox.rs receive helpers:
//   verify_cidnotify_admission (:2540) — pubkey lookup→sig verify→space/membership gate
//   decrypt_and_bind_dm_blob   (:2594) — decrypt under space content key + sender binding
//   dm_received_event_payload  (:2621) — build the `dm-received` UI event JSON
```

Today the inbound carrier is the Reticulum unicast path landing at the event loop, peeked by
`try_dispatch_community`, then run through verify→decrypt→`apply_inbox`→emit. **The tunnel
responder's `DmReceived` action delivers the same opaque packet bytes into this same verify/decrypt
pipeline** — i.e. the iroh receive side feeds the *existing* DM ingest, not a new one. **[ADAPT —
new entry feeding existing pipeline.]**

---

## Section 5 — Cross-repo boundary + open design questions

### Cross-repo dependency delta

- **`harmony-tunnel` is NOT a client dependency today.** The client pins seven harmony crates by
  git rev `c982079980…` (`Cargo.toml:91-103`): `harmony-runtime, -identity, -content, -compute,
  -telemetry, -mailbox, -owner, -pkarr`. **`harmony-tunnel` is absent.** **[NEW dep]** — add
  `harmony-tunnel = { git = …, rev = … }` at the same pinned rev.
- All transitive deps `harmony-tunnel` needs are **already in the client lock**: `harmony-crypto`
  (transitively via identity), `harmony-identity` (direct), `rand_core`, `zeroize`, `thiserror`,
  `ml-dsa`/`ml-kem`/`sha3` (direct pins `Cargo.toml:123-126`), `iroh 0.98` (direct). No new
  third-party crates. Low-risk add.
- **One required `harmony-tunnel` API addition: `FrameTag::Dm` + `TunnelEvent::SendDm` +
  `TunnelAction::DmReceived`** (§1). Small, additive, no wire break for existing tags. This lands
  in harmony core and bumps the client's pinned rev. **[NEW in core.]**
- **A client-side tunnel ALPN constant** (§1/§3). **[NEW in client.]**

### Premise correction (load-bearing for the spec)

The brief states the friend handshake "exchanges the peer's iroh node_id + PQ keys **into
`OwnerDeviceCache`**." **That is half-right and the precise shape matters:**
- **On the wire — yes, present:** `FriendLinkRequest` and `FriendLinkAccepted` both carry
  `iroh_node_id: [u8;32]`, `home_relay_url: Option<String>`, `pq_dsa_pubkey: Vec<u8>`,
  `pq_kem_pubkey: Vec<u8>` as **unsigned routing hints** (`iroh_friend_acceptor.rs:281-303` and
  `:363-379`). Self-side is filled from `SelfHandshakeReachability` (`lib.rs:39922`, `:42208`).
- **On the receive side — NOT persisted into `OwnerDeviceCache`.** `OwnerDeviceCache` /
  `OwnerDeviceEntry` (`owner_state_types.rs:439-499`) store only `devices: Vec<DeviceIdentityHash>`,
  `device_identity_pubs: Vec<Option<[u8;64]>>` (classical X25519‖Ed25519), and `learned_at: Hlc`.
  **No iroh node_id, no PQ key fields.** `FriendEntry` (`friend_graph.rs:134-180`) likewise has no
  iroh/PQ fields. The received `req.iroh_node_id` / `.pq_*_pubkey` are currently **dropped** on the
  receive side (only test code reads them back).
- **There IS a reachability subsystem the dialer can use instead:** pkarr-published
  `ReachabilityAnnouncePayload{ iroh_node_id, home_relay_url, direct_addresses, … }`
  (`reachability_record.rs:74`) resolved via `resolver.resolve_window(...)` — this is exactly how
  `add_friend_by_key` finds a peer's iroh dial target today (`lib.rs:42101-42146`). It gives
  `iroh_node_id + relay + direct addrs` but **NOT the peer's PQ keys** (the announce is
  classical-iroh routing only).

**Net for the spec:** to dial a PQ tunnel to a friend you need *both* the iroh dial target (have:
pkarr `ReachabilityAnnouncePayload`, or the handshake hint) *and* the peer's `pq_kem_pubkey` +
`pq_dsa_pubkey` (have on the handshake wire, but **currently discarded — must be persisted**). The
cleanest fix is to **store the handshake's peer `(iroh_node_id, home_relay_url, pq_dsa_pubkey,
pq_kem_pubkey)` on receipt** — either as new optional fields on `OwnerDeviceEntry`/`FriendEntry`,
or in a dedicated per-friend tunnel-contact directory. This is the genuine ZEB-461 "directory"
work the carrier consumes. **[NEW — persistence of received peer tunnel-contact material.]**

### Self PQ identity — present  **[REUSE]**

The client mints its own `PqPrivateIdentity` from the boot seed: `AppIdentity { pq:
PqPrivateIdentity, … }` (`identity.rs:60`, `pq: PqPrivateIdentity::from_seed(seed)` `:86`). So the
handshake's *self* inputs (ML-DSA sk/pk, ML-KEM sk) are already available — no new key minting.

### Open design questions the spec must resolve

1. **One tunnel per friend vs per device?** Evidence: `OwnerDeviceEntry.devices` is a
   `Vec<DeviceIdentityHash>` (multi-device per owner), but alpha nodes are single-device
   (`dm_tunnel_contact.rs:20` `self_device_bundle` advertises exactly one). The PQ handshake binds
   to **one peer ML-DSA identity** (`PqIdentity`), so a tunnel is inherently *per-device-identity*.
   Recommendation hinted: per-device tunnel, keyed by peer NodeId(=blake3(ML-DSA pk)); a friend
   with N devices = N tunnels. Spec must define the per-owner→per-device fan-out (mirrors
   `resolve_destinations` fanning a DM across `devices`).

2. **Who initiates / collision handling?** Both sides can dial (symmetric ALPN + acceptor). The PQ
   handshake has no half-open dedup. harmony-node sidesteps with a string-keyed `tunnel_senders`
   map + `connection_id` stale-close. Spec must pick a deterministic-initiator rule
   (e.g. lower NodeId dials, à la the synthesis's "lowest-node-id") OR allow dual tunnels and
   dedup by NodeId, to avoid two simultaneous half-tunnels per pair.

3. **Where does the session map live + session reuse/caching?** harmony-node keeps
   `tunnel_senders: HashMap<String, TunnelSender>` in the event-loop stack frame (`event_loop.rs:515`).
   The client has no event-loop tunnel map yet. Spec must place an analogous map (likely on
   `IrohZenohLinkManager` or a new `TunnelManager`), keyed by **peer NodeId**, holding a
   `cmd_tx` per live session, with: lazy dial-on-first-DM, keepalive (`Tick`), idle teardown,
   reconnect/backoff, and a buffered-send-while-dialing queue. Reuse is the whole point (avoid a
   6 s PQ handshake per message).

4. **(corollary) Reachability/PQ-contact source of truth?** Resolve the §5 premise gap: decide
   between (a) persisting the handshake's peer `(iroh_node_id, relay, pq_dsa, pq_kem)` into the
   friend directory, vs (b) extending the pkarr `ReachabilityAnnouncePayload` to also carry PQ keys
   so the dialer resolves everything from one signed record. (b) is more robust to re-peering /
   address churn but needs a signed-record format bump; (a) is local and immediate.

5. **Durability fallback unchanged?** DMs already have a non-tunnel durability fallback (butler /
   community-relay CAS deposit, ZEB-418/458). Spec should state the tunnel is the *live* path and
   the existing deposit fallback covers offline peers — so a failed/slow tunnel dial degrades to
   deposit, not data loss.

---

## One-line tags index

| Integration point | Tag | Where |
|---|---|---|
| `TunnelSession` handshake/send/recv state machine | REUSE | `session.rs` |
| `FrameTag::Dm` + `SendDm`/`DmReceived` | NEW (core) | `frame.rs`,`event.rs`,`session.rs` |
| `tunnel_task.rs` task shape (handshake+select loop+framing) | ADAPT | template for client acceptor/dialer |
| Reticulum-carrier coupling (`SendReticulum`, interface-string routing) | DROP | harmony-node only |
| Tunnel ALPN const | NEW (client) | `iroh_endpoint::alpn` |
| Add ALPN to bind list | ADAPT | `iroh_endpoint.rs:122` |
| Inbound tunnel acceptor arm + `OnceLock` install | NEW/ADAPT | `zenoh_iroh_transport.rs` |
| Outbound tunnel dialer | NEW (ADAPT friend/butler dial) | `lib.rs:42101` / `butler_deposit.rs:455` |
| DM hook = new `DmTransport` impl (recipient-keyed) | ADAPT | `dm_outbox.rs:40/230` |
| Inbound DM → existing verify/decrypt pipeline | ADAPT | `dm_outbox.rs:2540/2594`, `inbound_packet.rs:28` |
| `harmony-tunnel` client dep | NEW | `src-tauri/Cargo.toml` |
| Persist peer tunnel-contact (node_id+relay+PQ keys) | NEW | friend directory / `OwnerDeviceEntry` |
| Self `PqPrivateIdentity` | REUSE | `identity.rs:60` |
