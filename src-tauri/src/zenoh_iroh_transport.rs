//! ZEB-321 Phase 1 Task 6: `zenoh_link::LinkManagerUnicastTrait` impl
//! backed by an iroh `Endpoint` and a CRDT-driven `ReachabilityResolver`.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §7.3.
//!
//! ## Design intent
//!
//! Zenoh's transport plugin surface (see `zenoh-link-commons::unicast`)
//! exposes two pluggable bits:
//!
//! 1. `LinkManagerUnicastTrait` — the upper transport stack calls
//!    `new_link(endpoint)` to open an outbound link by locator and
//!    `new_listener(endpoint)` to advertise an inbound listener.
//! 2. `NewLinkChannelSender` (a `flume::Sender<LinkUnicast>`) — the
//!    accept side dispatches inbound links into Zenoh via this channel.
//!
//! [`IrohZenohLinkManager`] implements the first and owns one of the
//! second. On `new_link()` it parses the locator's address as a hex iroh
//! `EndpointId`, looks up the matching `ReachabilityAnnouncePayload` via
//! [`ReachabilityResolver::resolve_by_node_id`], synthesizes an
//! `EndpointAddr` (id + optional relay + direct addrs), opens a QUIC
//! bidi stream on ALPN `harmony/zenoh/v1`, and wraps the
//! `(SendStream, RecvStream)` pair in [`IrohZenohLink`] (Task 5).
//!
//! ## Locator format (load-bearing choice)
//!
//! Locator address is the **hex-encoded iroh EndpointId**:
//!
//! ```text
//! iroh/<lowercase-hex-32-bytes>
//! ```
//!
//! - Matches Task 5's `paired_stream_roundtrip_via_loopback` test, which
//!   uses `Locator::new("iroh", ep_id.to_string(), "")` — iroh 0.98's
//!   `Display for PublicKey` emits lowercase hex (see
//!   `iroh_base::key`), so `ep_id.to_string()` and
//!   `hex::encode(ep_id.as_bytes())` are identical. Picking either makes
//!   Task 5's link + Task 6's link manager round-trip cleanly.
//! - The locator carries the iroh `EndpointId` (32 bytes), NOT the
//!   harmony `OwnerAddr` (16 bytes). The spec text in §7.3 is
//!   intentional on this point — the resolver's reverse lookup is what
//!   bridges back to the harmony actor.
//!
//! ## Resolver lookup approach
//!
//! [`ReachabilityResolver::resolve`] is keyed by `OwnerAddr`. The
//! locator carries `EndpointId`. We added
//! [`ReachabilityResolver::resolve_by_node_id`] (a linear scan over
//! `list_active_peers()`) rather than indexing-by-node-id from the
//! transport here — the resolver is the source of truth for the LWW
//! projection, so the secondary lookup belongs alongside it.
//!
//! ## API adaptations from the plan draft
//!
//! The plan draft (`docs/plans/2026-05-22-zeb-321-phase1-iroh-foundation-plan.md`
//! lines 1670-1860) was written against unverified zenoh-link / iroh
//! 0.98 surfaces. Adaptations:
//!
//! - **`LinkManagerUnicastTrait` method signatures** — `new_link`,
//!   `new_listener` take `EndPoint` by *value* (not `&EndPoint`).
//!   `get_listeners` and `get_locators` are *async* and return `Vec<…>`.
//! - **`NewLinkChannelSender`** is `flume::Sender<LinkUnicast>` (not
//!   `tokio::mpsc`). Required pulling `flume = "0.11"` in directly.
//! - **iroh 0.98 renames** — `NodeId` → `EndpointId`, `NodeAddr` →
//!   `EndpointAddr`, `Endpoint::connect` takes
//!   `impl Into<EndpointAddr>` (not a builder chain via
//!   `.with_direct_addresses`).
//! - **`EndpointAddr` builders** — `with_relay_url(url)` and
//!   `with_ip_addr(addr)` are the right calls; the plan's
//!   `.with_direct_addresses(std::iter::once(...))` was wrong (the real
//!   API takes a `BTreeSet`-collected `with_addrs(impl IntoIterator)`).
//! - **`Connection::alpn()`** returns `&[u8]` directly (not
//!   `Option<Vec<u8>>` / `Result<…>`).
//! - **`Connection::remote_id()`** — not `.remote_node_id()`. Both
//!   refer to the peer's `EndpointId`.
//! - **`ZResult` / `zerror!`** — come from `zenoh_result`, not from
//!   `zenoh_link` (already discovered in Task 5).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use iroh::endpoint::Connection;
use iroh::{EndpointAddr, EndpointId};
use tokio::sync::Mutex as TokioMutex;
use zenoh_link::{
    EndPoint, LinkManagerUnicastTrait, LinkUnicast, LinkUnicastTrait, Locator, NewLink,
    NewLinkChannelSender,
};
use zenoh_result::{zerror, ZResult};

use crate::iroh_endpoint::{alpn, IrohEndpoint};
use crate::iroh_invite_acceptor::IrohHandshakeDispatcher;
use crate::reachability_resolver::ReachabilityResolver;
use crate::reconnect_supervisor::{ReconnectTrigger, SupervisorHandle};
use crate::zenoh_iroh_link::IrohZenohLink;

/// Locator protocol identifier for harmony's zenoh-over-iroh links.
const IROH_LOCATOR_PROTOCOL: &str = "iroh";

/// ZEB-325 PR #159 R2: bounded grace-period queue for inbound
/// `harmony/handshake/v1` connections that arrive before the
/// `IrohInviteHandshakeAcceptor` has been installed. The link manager
/// is constructed early at app boot (so no inbound iroh traffic is
/// dropped during the bind window), but the acceptor depends on the
/// owner identity + community registry + dm outbox + CRDT state, all
/// of which load later. Without this queue, every inbound handshake
/// arriving in that boot window was silently dropped — Bob's redeem
/// then surfaced `inviter_unreachable` despite Alice being online.
///
/// 32 entries is well above any realistic boot-window collision rate
/// (a community typically sees < 1 inbound redemption per minute), and
/// small enough that a runaway dialer cannot OOM the queue. Drop-
/// oldest when full: a slow boot plus spam from an adversarial peer
/// shouldn't displace fresh legitimate traffic indefinitely, but the
/// 30s aging below means stale entries fall out anyway.
const HANDSHAKE_PENDING_QUEUE_CAP: usize = 32;

/// ZEB-325 PR #159 R2: maximum age for a queued handshake connection.
/// Once the dispatcher is installed, drain skips entries older than
/// this — the dialer's own response-read timeout (default 30s, see
/// `HandshakeDialConfig`) will already have fired, so processing
/// them would only write a JoinCountersign into a closed connection.
const HANDSHAKE_PENDING_MAX_AGE: Duration = Duration::from_secs(30);

/// ZEB-616: how long to wait for a stale connection's close to complete
/// before admitting the reconnect anyway. Bounded so a wedged old connection
/// can't stall the accept path; on timeout we proceed and fall back to today's
/// behavior (stale face lingers until the lease reaps it).
const STALE_CONN_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Plug-in link manager that lets Zenoh open + accept links over an
/// iroh `Endpoint`, using [`ReachabilityResolver`] to translate a
/// locator's iroh `EndpointId` into a dialable [`EndpointAddr`].
pub struct IrohZenohLinkManager {
    endpoint: Arc<IrohEndpoint>,
    resolver: ReachabilityResolver,
    /// Channel into Zenoh's transport stack for inbound links the
    /// accept loop (spawned via [`IrohZenohLinkManager::spawn_accept_loop`])
    /// produces. Not used by the outbound `new_link` path.
    new_link_tx: NewLinkChannelSender,
    /// ZEB-325 Phase 2c (option A): late-populated dispatcher for
    /// inbound connections on ALPNs other than `harmony/zenoh/v1`.
    /// Currently only `harmony/handshake/v1` is dispatched here;
    /// future sub-protocols can layer on the same trait.
    ///
    /// **Lifecycle.** The link manager is constructed early at app
    /// boot (before the owner identity / community registry / dm
    /// outbox are ready); the accept loop is spawned immediately so no
    /// inbound iroh traffic is dropped during the (possibly long)
    /// boot window. The handshake dispatcher gets installed later
    /// (typically inside the `if let Some(seed)` owner-loaded branch
    /// of `start_node`) once the `community_registry` / `dm_outbox` /
    /// `crdt_state` / `app` handles are available.
    ///
    /// Pre-installation, inbound connections on the handshake ALPN are
    /// pushed onto [`Self::pending_handshakes`] (a bounded grace-
    /// period queue) and drained when the dispatcher installs.
    /// `OnceCell` ensures the install happens at most once — if a
    /// future code path needs to swap dispatchers, build a fresh
    /// manager (cheap) rather than mutating live state.
    ///
    /// ## Invariant
    ///
    /// `pending_handshakes` is only ever non-empty while
    /// `handshake_dispatcher.get().is_none()`. The accept path and
    /// install path both take `pending_handshakes`'s mutex around the
    /// OnceCell observation/mutation to enforce this.
    handshake_dispatcher: tokio::sync::OnceCell<Arc<dyn IrohHandshakeDispatcher>>,
    /// ZEB-325 PR #159 R2: grace-period queue for inbound
    /// `harmony/handshake/v1` connections that arrive before the
    /// dispatcher installs. Bounded to `HANDSHAKE_PENDING_QUEUE_CAP`
    /// (32) with drop-oldest semantics; drained by
    /// `install_handshake_dispatcher` with `HANDSHAKE_PENDING_MAX_AGE`
    /// (30s) aging applied. See the module-level constants for
    /// rationale.
    pending_handshakes: TokioMutex<VecDeque<(Connection, Instant)>>,
    /// ZEB-418 P1: late-installed acceptor for inbound
    /// `harmony/butler-deposit/v1` connections (Task 7 builds the
    /// production `ButlerDepositCtx` once `NodeState`'s dm-inbox engine
    /// handles exist, then installs via
    /// [`Self::install_butler_deposit_acceptor`]). Unlike the handshake
    /// ALPNs there is NO boot-window queue: a deposit arriving before
    /// install is closed without reply, which the sender's fallback chain
    /// (spec §6) treats as a rung-2 failure — it retries or falls back to
    /// the existing DmOutbox loop, so dropping is graceful by design.
    butler_deposit_acceptor:
        std::sync::OnceLock<Arc<crate::iroh_butler_acceptor::IrohButlerDepositAcceptor>>,
    /// ZEB-458 P4: late-installed acceptor for inbound
    /// `harmony/community-relay-deposit/v1` connections. Same lifecycle as
    /// `butler_deposit_acceptor` — no boot-window queue; a deposit arriving
    /// before install is closed and the sender's fallback chain retries.
    community_relay_deposit_acceptor: std::sync::OnceLock<
        Arc<crate::iroh_community_relay_acceptor::IrohCommunityRelayDepositAcceptor>,
    >,
    /// ZEB-458 P4: late-installed acceptor for inbound
    /// `harmony/community-relay-pull/v1` connections. Same lifecycle as the
    /// deposit acceptor.
    community_relay_pull_acceptor: std::sync::OnceLock<
        Arc<crate::iroh_community_relay_acceptor::IrohCommunityRelayPullAcceptor>,
    >,
    /// ZEB-811: late-installed acceptor for inbound `harmony/vine-relay/v1`
    /// connections (public-read vine descriptor+content serve). Same
    /// lifecycle as the community-relay pull acceptor — no boot-window
    /// queue; a pull arriving before install is closed and the follower's
    /// pull driver retries on its next cadence.
    vine_relay_acceptor: std::sync::OnceLock<Arc<crate::vine_relay::VineRelayAcceptor>>,
    /// ZEB-473 (Move 1a): late-installed acceptor for inbound
    /// `harmony/tunnel/v1` connections (the PQ DM tunnel). Same lifecycle as the
    /// butler/relay acceptors — no boot-window queue; a tunnel dial arriving
    /// before install is closed without reply, which the sender's
    /// always-deposit durability path covers (a missed live tunnel never makes
    /// delivery worse). Typed as the generic `IrohHandshakeDispatcher` trait
    /// object so the transport layer stays decoupled from the tunnel module.
    tunnel_acceptor: std::sync::OnceLock<Arc<dyn IrohHandshakeDispatcher>>,
    /// ZEB-616: the live inbound zenoh-ALPN iroh `Connection` per peer, keyed
    /// by the peer's iroh `EndpointId` (== its deterministic zenoh zid). A
    /// same-zid reconnect for a peer already present here closes the prior
    /// connection before the new link is admitted, so the stale zenoh face is
    /// reaped before the reconnect's declarations install — avoiding the
    /// upstream "Remapping unsupported" collision (ZEB-390 gives every node a
    /// stable zid, so a reconnect reuses it). A `std::sync::Mutex` is correct:
    /// it is only ever held for synchronous map ops, never across an `.await`
    /// (the async close operates on the *returned* prior connection after the
    /// guard is dropped).
    ///
    /// ZEB-620 Task 3: `Arc`-wrapped so a per-connection drop-watcher can be
    /// spawned from BOTH the inbound accept path (which holds `Arc<Self>`) and
    /// the outbound `new_link` path (which only holds `&self`) via one shared
    /// helper — the watcher captures a clone of this map handle, not the whole
    /// manager.
    zenoh_conns: Arc<std::sync::Mutex<std::collections::HashMap<EndpointId, Connection>>>,
    /// ZEB-620 Task 3: optional reconnect supervisor handle. When installed via
    /// [`Self::set_reconnect_handle`], a registry drop-watcher that evicts a
    /// genuinely-gone connection `kick`s the supervisor with
    /// [`ReconnectTrigger::Dropped`] (re-arming the reconnect ladder), and a
    /// successful inbound-accept / outbound-dial swap `mark_connected`s the peer
    /// (cancelling further dialing until it drops). Optional (`OnceLock`) so the
    /// manager boots before the supervisor is wired: pre-install, drop events
    /// are simply not raised — today's behavior, where a stale face lingers
    /// until the zenoh lease reaps it. `Arc`-wrapped for the same
    /// spawn-from-either-path reason as `zenoh_conns`.
    reconnect: Arc<std::sync::OnceLock<SupervisorHandle>>,
    /// ZEB-622 Task 2: optional per-peer liveness handle. When installed via
    /// [`Self::set_liveness_handle`], every registry install — inbound accept and
    /// outbound `new_link`, both funneling through [`Self::swap_zenoh_conn`] —
    /// reports an `on_transport_up` edge and spawns exactly one
    /// [`crate::peer_liveness::run_conn_path_watcher`] for the new conn, and every
    /// identity-guarded eviction in [`Self::spawn_drop_watcher`] reports the
    /// `on_transport_down` edge. Optional (`OnceLock`) so the manager boots before
    /// liveness is wired: pre-install, no liveness edges are raised. `Arc`-wrapped
    /// for the same spawn-from-either-path reason as `zenoh_conns` / `reconnect`.
    liveness: Arc<std::sync::OnceLock<crate::peer_liveness::LivenessHandle>>,
}

/// ZEB-616 identity guard for the drop-watcher: only evict a peer's registry
/// entry if the currently-stored connection IS the one whose watcher is firing.
/// `stored` is the registered connection's `stable_id` (None if the peer has no
/// entry); `watcher` is the firing connection's `stable_id`. Prevents a
/// superseded connection's watcher from evicting the live connection that
/// replaced it.
fn should_evict_on_close(stored: Option<usize>, watcher: usize) -> bool {
    stored == Some(watcher)
}

impl IrohZenohLinkManager {
    pub fn new(
        endpoint: Arc<IrohEndpoint>,
        resolver: ReachabilityResolver,
        new_link_tx: NewLinkChannelSender,
    ) -> Self {
        Self {
            endpoint,
            resolver,
            new_link_tx,
            handshake_dispatcher: tokio::sync::OnceCell::new(),
            pending_handshakes: TokioMutex::new(VecDeque::with_capacity(
                HANDSHAKE_PENDING_QUEUE_CAP,
            )),
            butler_deposit_acceptor: std::sync::OnceLock::new(),
            community_relay_deposit_acceptor: std::sync::OnceLock::new(),
            community_relay_pull_acceptor: std::sync::OnceLock::new(),
            vine_relay_acceptor: std::sync::OnceLock::new(),
            tunnel_acceptor: std::sync::OnceLock::new(),
            zenoh_conns: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            reconnect: Arc::new(std::sync::OnceLock::new()),
            liveness: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// ZEB-616: register `conn` as the live zenoh-ALPN connection for
    /// `peer_id`, returning the prior connection (if any) for the caller to
    /// close. Pure synchronous map op — the lock is not held across any await;
    /// the async close + await happens on the returned connection.
    fn swap_zenoh_conn(&self, peer_id: EndpointId, conn: Connection) -> Option<Connection> {
        let mut map = self.zenoh_conns.lock().unwrap();
        let prev = map.insert(peer_id, conn.clone());
        // ZEB-622 Task 2: this is the single choke point BOTH the inbound accept
        // and outbound `new_link` paths pass through with the `Connection` in
        // hand, so raise the liveness up-edge and spawn exactly one path watcher
        // for the new conn here. `on_transport_up` keys off `stable_id`, so a
        // superseding swap (same peer, new conn) re-arms as a fresh up-edge; the
        // superseded conn's watcher is silenced by the slot's conn-id guard.
        //
        // The up-edge is raised while STILL holding the map lock so per-peer
        // liveness installs serialize in map-insert order. `on_transport_up`
        // rebinds the slot on ANY differing conn id (a superseding swap must
        // win), so an insert-A/insert-B/notify-B/notify-A interleave — possible
        // when an inbound accept races an outbound dial for the same peer —
        // would bind the slot to the evicted conn, silencing the live conn's
        // path reports AND its eventual down-edge (conn-id mismatch), which
        // would in turn skip the epoch re-arm on the next reconnect. Lock order
        // is one-way (conn-map → liveness slots): liveness never takes the
        // conn-map lock, and the drop watcher releases the map lock before its
        // identity-guarded down-edge. Nothing here awaits under the lock —
        // `on_transport_up` and `tokio::spawn` are synchronous.
        if let Some(lh) = self.liveness.get() {
            lh.on_transport_up(*peer_id.as_bytes(), conn.stable_id());
            tokio::spawn(crate::peer_liveness::run_conn_path_watcher(
                lh.clone(),
                *peer_id.as_bytes(),
                conn,
            ));
        }
        prev
    }

    /// ZEB-616: is `conn_id` still the registered live connection for
    /// `peer_id`? The accept path uses this after `accept_bi()` to drop a
    /// stale link when a same-zid reconnect superseded this connection while
    /// its bi stream was being accepted — same identity predicate the
    /// drop-watcher applies via [`should_evict_on_close`]. Best-effort
    /// (lock-check, no mutation): worst case on a race is today's behavior.
    fn is_active_zenoh_conn(&self, peer_id: EndpointId, conn_id: usize) -> bool {
        self.zenoh_conns
            .lock()
            .unwrap()
            .get(&peer_id)
            .map(|c| c.stable_id())
            == Some(conn_id)
    }

    /// ZEB-620 Task 3: install the reconnect supervisor handle. Install-once
    /// (`OnceLock`, mirroring the acceptor-install idiom); a second install
    /// returns the supplied handle back as `Err`. Once installed, the registry
    /// drop-watchers spawned by [`Self::spawn_drop_watcher`] kick
    /// [`ReconnectTrigger::Dropped`] on a guard-passing eviction, and a
    /// successful swap marks the peer connected via
    /// [`Self::mark_supervisor_connected`].
    pub fn set_reconnect_handle(&self, handle: SupervisorHandle) -> Result<(), SupervisorHandle> {
        self.reconnect.set(handle)
    }

    /// ZEB-622 Task 2: install the per-peer liveness handle. Install-once
    /// (`OnceLock`, mirroring [`Self::set_reconnect_handle`]); a second install
    /// returns the supplied handle back as `Err`. Once installed,
    /// [`Self::swap_zenoh_conn`] raises an `on_transport_up` edge + spawns the
    /// iroh path watcher on every registry install, and
    /// [`Self::spawn_drop_watcher`]'s identity-guarded eviction raises the
    /// matching `on_transport_down` edge.
    pub fn set_liveness_handle(
        &self,
        handle: crate::peer_liveness::LivenessHandle,
    ) -> Result<(), crate::peer_liveness::LivenessHandle> {
        self.liveness.set(handle)
    }

    /// ZEB-620 Task 3: mark `peer_id` connected on the reconnect supervisor (if
    /// one is installed), cancelling further dialing until the peer drops.
    /// Called on a successful inbound-accept / outbound-dial registry swap.
    /// No-op when no supervisor is installed (pre-wire boot / hermetic tests).
    fn mark_supervisor_connected(&self, peer_id: EndpointId) {
        if let Some(handle) = self.reconnect.get() {
            handle.mark_connected(*peer_id.as_bytes());
        }
    }

    /// ZEB-620 Task 3: spawn the identity-guarded drop-watcher for a registered
    /// zenoh connection, shared verbatim by the inbound accept path and the
    /// outbound `new_link` path so both register identically. When `conn`
    /// finally closes, evict the peer's registry entry IFF it still points at
    /// THIS connection ([`should_evict_on_close`]) — and only when that eviction
    /// fires, `kick` the reconnect supervisor with [`ReconnectTrigger::Dropped`]
    /// so a genuinely-gone transport is re-armed. The guard doubles as kick
    /// suppression: a superseded connection's watcher neither evicts nor kicks
    /// the live connection that replaced it. Captures clones of the `Arc`-backed
    /// registry + supervisor handles (not `Arc<Self>`) so the outbound path,
    /// which only has `&self`, can spawn it too.
    fn spawn_drop_watcher(&self, peer_id: EndpointId, conn_id: usize, conn: Connection) {
        let conns = Arc::clone(&self.zenoh_conns);
        let reconnect = Arc::clone(&self.reconnect);
        let liveness = Arc::clone(&self.liveness);
        tokio::spawn(async move {
            conn.closed().await;
            let evicted = {
                let mut map = conns.lock().unwrap();
                let stored = map.get(&peer_id).map(|c| c.stable_id());
                if should_evict_on_close(stored, conn_id) {
                    map.remove(&peer_id);
                    true
                } else {
                    false
                }
            };
            // Kick only on a guard-passing eviction: a superseded watcher must
            // not re-arm a peer whose live connection replaced this one.
            if evicted {
                if let Some(handle) = reconnect.get() {
                    handle.kick(*peer_id.as_bytes(), ReconnectTrigger::Dropped);
                }
                // ZEB-622 Task 2: same identity guard, same edge — a genuinely-
                // gone conn raises the liveness down-edge (the watcher spawned in
                // `swap_zenoh_conn` for THIS conn also exits on its own when the
                // event stream ends, but the Disconnected transition is owned
                // here so a superseded conn can't clobber the live slot).
                if let Some(lh) = liveness.get() {
                    lh.on_transport_down(*peer_id.as_bytes(), conn_id);
                }
            }
        });
    }

    /// ZEB-418 P1: install the butler-deposit acceptor used by the accept
    /// loop to route inbound `harmony/butler-deposit/v1` connections.
    /// Install-once (mirrors the handshake dispatcher's lifecycle); a second
    /// install returns the supplied acceptor back as `Err`. No pending
    /// queue — see the field docs for why dropping pre-install is graceful.
    pub fn install_butler_deposit_acceptor(
        &self,
        acceptor: Arc<crate::iroh_butler_acceptor::IrohButlerDepositAcceptor>,
    ) -> Result<(), Arc<crate::iroh_butler_acceptor::IrohButlerDepositAcceptor>> {
        self.butler_deposit_acceptor.set(acceptor)
    }

    /// ZEB-458 P4: install the community-relay deposit acceptor used by the
    /// accept loop to route inbound `harmony/community-relay-deposit/v1`
    /// connections. Install-once (mirrors the butler-deposit acceptor); a
    /// second install returns the supplied acceptor back as `Err`. No pending
    /// queue — dropping pre-install is graceful (the sender's relay rung is a
    /// last-resort retry that can never make delivery worse).
    pub fn install_community_relay_deposit_acceptor(
        &self,
        acceptor: Arc<crate::iroh_community_relay_acceptor::IrohCommunityRelayDepositAcceptor>,
    ) -> Result<(), Arc<crate::iroh_community_relay_acceptor::IrohCommunityRelayDepositAcceptor>>
    {
        self.community_relay_deposit_acceptor.set(acceptor)
    }

    /// ZEB-458 P4: install the community-relay pull acceptor used by the
    /// accept loop to route inbound `harmony/community-relay-pull/v1`
    /// connections. Same install-once lifecycle as the deposit acceptor.
    pub fn install_community_relay_pull_acceptor(
        &self,
        acceptor: Arc<crate::iroh_community_relay_acceptor::IrohCommunityRelayPullAcceptor>,
    ) -> Result<(), Arc<crate::iroh_community_relay_acceptor::IrohCommunityRelayPullAcceptor>> {
        self.community_relay_pull_acceptor.set(acceptor)
    }

    /// ZEB-811: install the vine-relay acceptor used by the accept loop to
    /// route inbound `harmony/vine-relay/v1` connections. Same install-once
    /// lifecycle as the community-relay pull acceptor.
    pub fn install_vine_relay_acceptor(
        &self,
        acceptor: Arc<crate::vine_relay::VineRelayAcceptor>,
    ) -> Result<(), Arc<crate::vine_relay::VineRelayAcceptor>> {
        self.vine_relay_acceptor.set(acceptor)
    }

    /// ZEB-473 (Move 1a): install the PQ DM tunnel acceptor used by the accept
    /// loop to route inbound `harmony/tunnel/v1` connections. Install-once
    /// (mirrors the butler-deposit acceptor); a second install returns the
    /// supplied acceptor back as `Err`. No pending queue — dropping a tunnel
    /// dial pre-install is graceful (the sender's always-deposit durability path
    /// covers a missed live tunnel).
    pub fn install_tunnel_acceptor(
        &self,
        acceptor: Arc<dyn IrohHandshakeDispatcher>,
    ) -> Result<(), Arc<dyn IrohHandshakeDispatcher>> {
        self.tunnel_acceptor.set(acceptor)
    }

    /// ZEB-368: expose the resolver so the event loop can enumerate known peers
    /// for static `connect/endpoints` seeding. `ReachabilityResolver` is a cheap
    /// Arc-backed handle (`Clone`).
    pub fn resolver(&self) -> crate::reachability_resolver::ReachabilityResolver {
        self.resolver.clone()
    }

    /// ZEB-325 Phase 2c (option A) + PR #159 R2: install the
    /// `IrohHandshakeDispatcher` used by the accept loop to route
    /// inbound `harmony/handshake/v1` connections, AND drain any
    /// connections that arrived during the boot window.
    ///
    /// Locking order (see `pending_handshakes` doc-comment): the
    /// pending-queue mutex is acquired BEFORE setting the OnceCell,
    /// then held across the drain. The accept loop takes the same
    /// mutex around its OnceCell observation, so the dispatcher
    /// transition Some↔None is atomic w.r.t. enqueue/drain: future
    /// inbound connections see the dispatcher and take the fast path,
    /// while everything queued before install is owned by this drain.
    ///
    /// Returns Err (carrying the supplied Arc back to the caller, then
    /// dropped) if a dispatcher was already installed — OnceCell::set
    /// rejects the second write. In that case the queue is NOT
    /// drained (the prior install would have done so).
    pub async fn install_handshake_dispatcher(
        self: &Arc<Self>,
        dispatcher: Arc<dyn IrohHandshakeDispatcher>,
    ) -> Result<(), Arc<dyn IrohHandshakeDispatcher>> {
        // Acquire the queue lock FIRST so any racing accept-side
        // enqueue blocks on this critical section. Inside the lock,
        // set the OnceCell and snapshot the queue contents.
        let mut queue = self.pending_handshakes.lock().await;
        if let Err(set_err) = self.handshake_dispatcher.set(Arc::clone(&dispatcher)) {
            return Err(match set_err {
                tokio::sync::SetError::AlreadyInitializedError(d) => d,
                tokio::sync::SetError::InitializingError(d) => d,
            });
        }
        let drained: Vec<(Connection, Instant)> = queue.drain(..).collect();
        // Drop the lock before dispatching so accept-side fast-path
        // observers don't have to wait for handle_connection to run.
        drop(queue);

        if !drained.is_empty() {
            let queued_count = drained.len();
            let dispatcher_for_task = dispatcher;
            tokio::spawn(async move {
                let mut dispatched = 0usize;
                let mut aged_out = 0usize;
                for (conn, enqueued_at) in drained {
                    let age = enqueued_at.elapsed();
                    if age > HANDSHAKE_PENDING_MAX_AGE {
                        tracing::warn!(
                            age_ms = age.as_millis() as u64,
                            max_age_ms = HANDSHAKE_PENDING_MAX_AGE.as_millis() as u64,
                            "ZEB-325 PR #159 R2: dropping aged-out queued handshake \
                             connection (dialer's read timeout will already have fired)"
                        );
                        // Close defensively in case the remote is
                        // still alive — gives them a clean
                        // CONNECTION_CLOSE frame instead of a stall.
                        conn.close(0u32.into(), b"boot-queue-aged-out");
                        aged_out += 1;
                        continue;
                    }
                    // ZEB-325 PR #159 R3-3 (CodeRabbit MAJOR): spawn
                    // each drained connection so a single slow
                    // handshake can't block every later queued one
                    // for its full IO+poll deadline (the dispatcher's
                    // own per-connection timeouts from PR #159 R2/R3
                    // bound each task's lifetime — fire-and-forget is
                    // safe here, no JoinSet needed).
                    let dispatcher_per_conn = Arc::clone(&dispatcher_for_task);
                    tokio::spawn(async move {
                        dispatcher_per_conn.handle_connection(conn).await;
                    });
                    dispatched += 1;
                }
                tracing::info!(
                    queued_count,
                    dispatched,
                    aged_out,
                    "ZEB-325 PR #159 R2: drained boot-window handshake queue"
                );
            });
        }
        Ok(())
    }

    /// Spawn the inbound-link accept loop. Each accepted connection is
    /// filtered on ALPN `harmony/zenoh/v1`, an `accept_bi` stream pair
    /// is wrapped in [`IrohZenohLink`], and the result is dispatched to
    /// Zenoh via the [`NewLinkChannelSender`] this manager owns.
    ///
    /// ZEB-325 Phase 2c (option A): when a handshake dispatcher is
    /// installed via [`Self::with_handshake_dispatcher`], inbound
    /// connections that negotiate `harmony/handshake/v1` are routed
    /// there instead of being dropped.
    ///
    /// Returns the join handle; callers should hold it to drive
    /// shutdown explicitly. Endpoint shutdown causes `accept()` to
    /// return `None`, which ends the loop.
    pub fn spawn_accept_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let ep = mgr.endpoint.inner().clone();
            while let Some(incoming) = ep.accept().await {
                let mgr = Arc::clone(&mgr);
                tokio::spawn(async move {
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("iroh accept→connect failed: {e}");
                            return;
                        }
                    };
                    // ALPN filter — `alpn()` returns the negotiated
                    // peer ALPN as &[u8] directly. iroh has already
                    // checked it's one of ours (the ones we registered
                    // at bind time), but cross-check defensively in
                    // case future code registers handshake or other
                    // sub-protocols on the same endpoint.
                    let alpn_used = conn.alpn();
                    if alpn_used == alpn::HARMONY_ZENOH_V1 {
                        // ZEB-616: `remote_id()` is available immediately
                        // post-handshake (before the bi stream), so swap this
                        // connection into the per-peer registry FIRST and close
                        // the stale one it replaces. Doing it here — one beat
                        // before the reconnect opens its bi stream and zenoh
                        // re-declares resources — reaps the old face ahead of
                        // the collision window (avoids "Remapping
                        // unsupported"). Reordered above `accept_bi()` from the
                        // pre-ZEB-616 code, which read `remote_id()` after it.
                        let peer_id = conn.remote_id();
                        // ZEB-912: test-only sever seam (see iroh_dial_driver).
                        // Reject BEFORE the registry swap so a denied peer never
                        // reaches mark_supervisor_connected — which would cancel
                        // our own dialing and leave a half-formed link.
                        if crate::iroh_dial_driver::is_zenoh_test_denied(peer_id.as_bytes()) {
                            tracing::info!(
                                peer = %peer_id,
                                "ZEB-912 test denylist: rejecting inbound"
                            );
                            conn.close(0u32.into(), b"zeb912-test-denylist");
                            return;
                        }
                        let conn_id = conn.stable_id();
                        if let Some(old) = mgr.swap_zenoh_conn(peer_id, conn.clone()) {
                            tracing::debug!(
                                peer = %peer_id,
                                "ZEB-616: same-zid reconnect; closing stale zenoh \
                                 connection before admitting new link"
                            );
                            old.close(0u32.into(), b"zeb616-reconnect");
                            // Bounded: guarantee the old iroh conn is gone (→
                            // its zenoh link read-errors → zenoh reaps the stale
                            // face) before admitting the new link's
                            // declarations. Best-effort on timeout.
                            if tokio::time::timeout(STALE_CONN_CLOSE_TIMEOUT, old.closed())
                                .await
                                .is_err()
                            {
                                tracing::debug!(
                                    peer = %peer_id,
                                    "ZEB-616: stale connection close timed out; \
                                     admitting new link anyway"
                                );
                            }
                        }
                        // ZEB-620 Task 3: the swap registered this connection —
                        // mark the peer connected so the supervisor cancels
                        // dialing until it drops.
                        mgr.mark_supervisor_connected(peer_id);
                        // ZEB-616/620: evict this peer's registry entry when THIS
                        // connection finally closes (map stays bounded to live
                        // peers; a drop-and-never-return peer still gets its face
                        // reaped) AND kick the supervisor `Dropped`. Identity-
                        // guarded so a superseded connection's watcher can neither
                        // evict nor kick for the connection that replaced it.
                        mgr.spawn_drop_watcher(peer_id, conn_id, conn.clone());

                        let (send, recv) = match conn.accept_bi().await {
                            Ok(s) => s,
                            Err(e) => {
                                // ZEB-616: accept_bi failed. If this is still the
                                // registered connection for the peer, close it so
                                // the drop-watcher evicts the registry entry — a
                                // stream-level failure can leave the connection
                                // open, and we must not leave a faceless
                                // connection occupying the peer's slot. Guarded so
                                // it's a no-op if a reconnect already superseded
                                // us (that reconnect's swap already closed us).
                                tracing::warn!("iroh accept_bi failed: {e}");
                                if mgr.is_active_zenoh_conn(peer_id, conn_id) {
                                    conn.close(0u32.into(), b"zeb616-accept-bi-failed");
                                }
                                return;
                            }
                        };
                        // ZEB-616: a same-zid reconnect may have superseded this
                        // connection while accept_bi() was awaiting (its swap
                        // closed us and installed a newer conn). Do NOT admit a
                        // stale link — that would re-introduce the collision this
                        // fix prevents. Only the current registry entry proceeds.
                        if !mgr.is_active_zenoh_conn(peer_id, conn_id) {
                            tracing::debug!(
                                peer = %peer_id,
                                "ZEB-616: connection superseded during accept_bi; \
                                 dropping stale link"
                            );
                            return;
                        }
                        let src = locator_from_endpoint_id(&mgr.endpoint.node_id());
                        let dst = locator_from_endpoint_id(&peer_id);
                        let link: Arc<dyn LinkUnicastTrait> =
                            Arc::new(IrohZenohLink::new(send, recv, src, dst));
                        // zenoh-link 1.9.0: LinkUnicast now wraps the
                        // NewLink enum (Single or MixedReliability). We
                        // open one QUIC bidi stream → one link → Single.
                        if let Err(e) = mgr
                            .new_link_tx
                            .send_async(LinkUnicast(NewLink::Single(link)))
                            .await
                        {
                            tracing::warn!("zenoh new_link channel closed: {e}");
                        }
                    } else if alpn_used == alpn::HARMONY_HANDSHAKE_V1
                        || alpn_used == alpn::HARMONY_FRIEND_V1
                        || alpn_used == alpn::HARMONY_FRIEND_PEX_V1
                    {
                        // ZEB-325 Phase 2c (option A) + PR #159 R2:
                        // inbound invite handshake. ZEB-370 Task 9: the
                        // friend-link ALPN (`harmony/friend/v1`) shares this
                        // dispatch path — the installed dispatcher is a
                        // `MultiplexHandshakeDispatcher` that re-reads
                        // `conn.alpn()` and routes friend → friend acceptor,
                        // else → invite acceptor. Take the pending-
                        // queue mutex around the dispatcher OnceCell
                        // observation so install_handshake_dispatcher's
                        // drain sees a consistent
                        // queue-empty-while-dispatcher-set invariant.
                        // Fast path: dispatcher already installed →
                        // dispatch directly without enqueue.
                        // Slow path: queue the connection with the
                        // current Instant; install_handshake_dispatcher
                        // will drain and dispatch (or age-out + close)
                        // once the owner identity loads.
                        let mut queue = mgr.pending_handshakes.lock().await;
                        if let Some(dispatcher) = mgr.handshake_dispatcher.get().cloned() {
                            drop(queue);
                            dispatcher.handle_connection(conn).await;
                        } else {
                            // Drop-oldest when at capacity. The aging
                            // pass during drain prunes stale entries
                            // too, but the bound guards against runaway
                            // dialers in the pre-install window.
                            if queue.len() >= HANDSHAKE_PENDING_QUEUE_CAP {
                                if let Some((stale, _)) = queue.pop_front() {
                                    tracing::warn!(
                                        cap = HANDSHAKE_PENDING_QUEUE_CAP,
                                        "ZEB-325 PR #159 R2: handshake queue at capacity; \
                                         evicting oldest queued connection"
                                    );
                                    stale.close(0u32.into(), b"boot-queue-evicted");
                                }
                            }
                            queue.push_back((conn, Instant::now()));
                            tracing::debug!(
                                queue_depth = queue.len(),
                                "ZEB-325 PR #159 R2: queued inbound handshake (dispatcher \
                                 not yet installed; will drain after owner identity loads)"
                            );
                        }
                    } else if alpn_used == alpn::HARMONY_BUTLER_DEPOSIT_V1 {
                        // ZEB-418 P1: butler deposit. Spawn so a slow/hung
                        // depositor can't block the accept loop (same
                        // rationale as HARMONY_PING_V1 below). No boot-window
                        // queue: pre-install deposits are closed and the
                        // sender's fallback chain retries (spec §6 — a failed
                        // deposit rung can never make delivery worse).
                        if let Some(acceptor) = mgr.butler_deposit_acceptor.get().cloned() {
                            tokio::spawn(async move {
                                acceptor.handle_connection(conn).await;
                            });
                        } else {
                            tracing::debug!(
                                "ZEB-418: butler deposit before acceptor installed; closing"
                            );
                            conn.close(0u32.into(), b"");
                        }
                    } else if alpn_used == alpn::HARMONY_COMMUNITY_RELAY_DEPOSIT_V1 {
                        // ZEB-458 P4: community-relay deposit. Spawn so a
                        // slow/hung depositor can't block the accept loop. No
                        // boot-window queue: pre-install deposits are closed
                        // and the sender's fallback chain retries.
                        if let Some(acceptor) = mgr.community_relay_deposit_acceptor.get().cloned()
                        {
                            tokio::spawn(async move {
                                acceptor.handle_connection(conn).await;
                            });
                        } else {
                            tracing::debug!(
                                "ZEB-458: community relay deposit before acceptor installed; closing"
                            );
                            conn.close(0u32.into(), b"");
                        }
                    } else if alpn_used == alpn::HARMONY_COMMUNITY_RELAY_PULL_V1 {
                        // ZEB-458 P4: community-relay pull. Spawn so a slow/hung
                        // requester can't block the accept loop.
                        if let Some(acceptor) = mgr.community_relay_pull_acceptor.get().cloned() {
                            tokio::spawn(async move {
                                acceptor.handle_connection(conn).await;
                            });
                        } else {
                            tracing::debug!(
                                "ZEB-458: community relay pull before acceptor installed; closing"
                            );
                            conn.close(0u32.into(), b"");
                        }
                    } else if alpn_used == alpn::HARMONY_VINE_RELAY_V1 {
                        // ZEB-811: public-read vine-relay fan-out. Spawn so a
                        // slow/hung requester can't block the accept loop;
                        // admission control lives inside `handle_connection`
                        // (accept-then-close-at-capacity, so the "busy"
                        // signal costs a saturated relay nothing).
                        if let Some(acceptor) = mgr.vine_relay_acceptor.get().cloned() {
                            tokio::spawn(async move {
                                acceptor.handle_connection(conn).await;
                            });
                        } else {
                            tracing::debug!(
                                "ZEB-811: vine relay connection before acceptor installed; closing"
                            );
                            conn.close(0u32.into(), b"");
                        }
                    } else if alpn_used == alpn::HARMONY_TUNNEL_V1 {
                        // ZEB-473 (Move 1a): inbound PQ DM tunnel. Spawn so a
                        // slow/hung peer can't block the accept loop (the
                        // responder driver owns the connection for the tunnel's
                        // whole lifetime). No boot-window queue: a tunnel dial
                        // arriving before the acceptor installs is closed, and
                        // the sender's always-deposit durability path covers the
                        // missed live tunnel.
                        if let Some(acceptor) = mgr.tunnel_acceptor.get().cloned() {
                            tokio::spawn(async move {
                                acceptor.handle_connection(conn).await;
                            });
                        } else {
                            tracing::debug!(
                                "ZEB-473: tunnel dial before acceptor installed; closing"
                            );
                            conn.close(0u32.into(), b"");
                        }
                    } else if alpn_used == alpn::HARMONY_TUNNEL_V2 {
                        // ZEB-623: inbound PQ DM tunnel, generation 2 (versioned
                        // `TunnelHello` capabilities frame precedes the
                        // TunnelInit). Same acceptor + spawn-so-a-slow-peer-can't-
                        // block rationale as the `/v1` branch above; the responder
                        // driver reads `conn.alpn()` to learn the generation and
                        // runs the hello exchange. Deliberate duplication of the
                        // `/v1` dispatch body over premature factoring of the
                        // per-ALPN branch chain (ZEB-623 plan scope note).
                        if let Some(acceptor) = mgr.tunnel_acceptor.get().cloned() {
                            tokio::spawn(async move {
                                acceptor.handle_connection(conn).await;
                            });
                        } else {
                            tracing::debug!(
                                "ZEB-623: tunnel v2 dial before acceptor installed; closing"
                            );
                            conn.close(0u32.into(), b"");
                        }
                    } else if alpn_used == alpn::HARMONY_PING_V1 {
                        // ZEB-329 Task 5 (option B): fold HARMONY_PING_V1
                        // dispatch into the single accept loop. iroh 0.98's
                        // `Endpoint::accept()` is backed by a shared
                        // mutex-protected queue, so a separate ping accept
                        // loop would round-robin connections with this one.
                        // Spawn so a slow/hung peer doesn't block the
                        // accept loop on the next connection.
                        tracing::debug!("dispatching HARMONY_PING_V1 to handle_ping_accept");
                        tokio::spawn(crate::network_health::handle_ping_accept(conn));
                    } else {
                        tracing::debug!(
                            "ignoring unknown ALPN: {:?}",
                            std::str::from_utf8(alpn_used).unwrap_or("<binary>")
                        );
                    }
                });
            }
        })
    }

    /// Parse a `Locator`'s address as a hex iroh `EndpointId`.
    /// Returns `None` for any malformed input (wrong hex length, bad
    /// hex digits, not a valid Ed25519 public key).
    fn parse_endpoint_id(endpoint: &EndPoint) -> Option<EndpointId> {
        let addr = endpoint.address();
        let addr_str: &str = addr.as_str();
        let bytes = hex::decode(addr_str).ok()?;
        let arr: [u8; 32] = bytes.try_into().ok()?;
        EndpointId::from_bytes(&arr).ok()
    }
}

/// Build the canonical locator for a given iroh `EndpointId`.
/// Format: `iroh/<lowercase-hex-32-bytes>`. Matches
/// `iroh::EndpointId`'s `Display` impl (hex), so peers reading the
/// locator can `addr_str.parse::<EndpointId>()` and round-trip.
fn locator_from_endpoint_id(id: &EndpointId) -> Locator {
    Locator::new(IROH_LOCATOR_PROTOCOL, hex::encode(id.as_bytes()), "")
        .expect("iroh locator format is well-known")
}

#[async_trait]
impl LinkManagerUnicastTrait for IrohZenohLinkManager {
    async fn new_link(&self, endpoint: EndPoint) -> ZResult<LinkUnicast> {
        let peer_id = Self::parse_endpoint_id(&endpoint).ok_or_else(|| {
            zerror!(
                "iroh locator address is not a 64-char hex EndpointId: {:?}",
                endpoint.address().as_str()
            )
        })?;
        let (_, record) = self
            .resolver
            .resolve_by_node_id(peer_id.as_bytes())
            .ok_or_else(|| zerror!("no ReachabilityRecord for iroh EndpointId {peer_id}"))?;

        // Build the EndpointAddr from the resolver payload. Skip
        // malformed relay URLs silently — direct addrs alone may still
        // succeed; logging at trace keeps the failure visible without
        // spamming production logs.
        let mut addr = EndpointAddr::new(peer_id);
        if !record.home_relay_url.is_empty() {
            match record.home_relay_url.parse() {
                Ok(url) => addr = addr.with_relay_url(url),
                Err(e) => {
                    tracing::trace!(
                        "skip malformed home_relay_url {:?}: {e}",
                        record.home_relay_url
                    );
                }
            }
        }
        for da in record.direct_addresses {
            addr = addr.with_ip_addr(da);
        }

        let conn = self
            .endpoint
            .inner()
            .connect(addr, alpn::HARMONY_ZENOH_V1)
            .await
            .map_err(|e| zerror!("iroh connect: {e}"))?;

        // ZEB-620 Task 3: register this outbound connection in the per-peer
        // zenoh registry with the SAME semantics as the inbound accept path
        // (ZEB-616) — swap it in, close-stale-first so the old face is reaped
        // before the new stream declares, mark the peer connected, and spawn the
        // identity-guarded drop-watcher. Without this an outbound-dialed peer
        // never joined the registry, so a same-zid inbound reconnect couldn't
        // find the outbound conn to close (reopening the collision this guards)
        // and its later drop would never re-arm the supervisor.
        let conn_id = conn.stable_id();
        if let Some(old) = self.swap_zenoh_conn(peer_id, conn.clone()) {
            tracing::debug!(
                peer = %peer_id,
                "ZEB-616/620: outbound dial superseded a registered connection; \
                 closing it before opening the new stream"
            );
            old.close(0u32.into(), b"zeb616-reconnect");
            if tokio::time::timeout(STALE_CONN_CLOSE_TIMEOUT, old.closed())
                .await
                .is_err()
            {
                tracing::debug!(
                    peer = %peer_id,
                    "ZEB-616/620: stale connection close timed out; opening new \
                     stream anyway"
                );
            }
        }
        self.mark_supervisor_connected(peer_id);
        self.spawn_drop_watcher(peer_id, conn_id, conn.clone());

        let (send, recv) = match conn.open_bi().await {
            Ok(s) => s,
            Err(e) => {
                // ZEB-620 Task 3: the stream failed to open after we registered
                // this connection. Close it (if still the active entry) so the
                // drop-watcher evicts the registry entry and kicks `Dropped` —
                // otherwise the peer would linger as phantom-connected with no
                // zenoh link. Mirrors the inbound accept_bi-failure handling.
                if self.is_active_zenoh_conn(peer_id, conn_id) {
                    conn.close(0u32.into(), b"zeb620-open-bi-failed");
                }
                return Err(zerror!("iroh open_bi: {e}").into());
            }
        };
        // ZEB-627: a same-zid reconnect may have superseded this connection
        // while `open_bi()` was awaiting (its swap closed us and installed a
        // newer conn). Do NOT hand zenoh a stale link — mirrors the inbound
        // accept path's post-`accept_bi` recheck (ZEB-616). The supersessor's
        // swap already closed this conn; the supervisor's normal kick/dial
        // path owns recovery, so failing the link here is safe.
        if !self.is_active_zenoh_conn(peer_id, conn_id) {
            tracing::debug!(
                peer = %peer_id,
                "ZEB-627: connection superseded during open_bi; not admitting stale link"
            );
            return Err(zerror!("iroh connection superseded during open_bi").into());
        }
        let src = locator_from_endpoint_id(&self.endpoint.node_id());
        let dst = locator_from_endpoint_id(&peer_id);
        let link: Arc<dyn LinkUnicastTrait> = Arc::new(IrohZenohLink::new(send, recv, src, dst));
        Ok(LinkUnicast(NewLink::Single(link)))
    }

    async fn new_listener(&self, _endpoint: EndPoint) -> ZResult<Locator> {
        // The iroh endpoint is already bound + listening from
        // `IrohEndpoint::new_with_secret`; the accept loop spawned by
        // `spawn_accept_loop` dispatches inbound links into Zenoh. We
        // have nothing to do here beyond returning the local locator
        // so the Zenoh transport stack can record it in its listener
        // set.
        Ok(locator_from_endpoint_id(&self.endpoint.node_id()))
    }

    async fn del_listener(&self, _endpoint: &EndPoint) -> ZResult<()> {
        // No-op — listener lifetime is bound to the underlying
        // `IrohEndpoint`. ZEB-368: iroh-link teardown is handled by
        // `endpoint.shutdown()` in stop_node, so this no-op is intentional
        // (there is no per-listener resource to release here).
        Ok(())
    }

    async fn get_listeners(&self) -> Vec<EndPoint> {
        vec![locator_from_endpoint_id(&self.endpoint.node_id()).to_endpoint()]
    }

    async fn get_locators(&self) -> Vec<Locator> {
        vec![locator_from_endpoint_id(&self.endpoint.node_id())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use crate::reachability_record::ReachabilityAnnouncePayload;
    use crate::reconnect_supervisor::PeerStateWire;
    use iroh::endpoint::{presets, Endpoint, RelayMode};
    use iroh::SecretKey;
    use rand::RngCore;
    use std::net::Ipv4Addr;

    /// Build a hermetic iroh endpoint on loopback with no
    /// address-lookup / relay traffic. Mirrors the pattern in
    /// `zenoh_iroh_link::tests` so the test never touches the network.
    ///
    /// Goes through `IrohEndpoint::from_endpoint_for_test` (the
    /// `#[cfg(any(test, feature = "test-fixtures"))]`-gated ctor on
    /// `IrohEndpoint`) so the production path `new_with_secret`
    /// (which uses `presets::N0` + pkarr + DNS and hangs offline)
    /// stays untouched.
    async fn build_hermetic_iroh_endpoint() -> Arc<IrohEndpoint> {
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        build_hermetic_iroh_endpoint_with_secret(SecretKey::from_bytes(&buf)).await
    }

    /// ZEB-616: like `build_hermetic_iroh_endpoint` but with a
    /// caller-supplied identity. Two endpoints built from the SAME secret
    /// share one `EndpointId` (hence one deterministic zenoh zid) — the
    /// shape of a reconnect after a socket rebind. Registers BOTH ALPNs so
    /// the accept loop routes zenoh + handshake connections (mirrors the
    /// production bind set, iroh_endpoint.rs:88/:252).
    async fn build_hermetic_iroh_endpoint_with_secret(secret: SecretKey) -> Arc<IrohEndpoint> {
        let inner = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![
                alpn::HARMONY_ZENOH_V1.to_vec(),
                alpn::HARMONY_HANDSHAKE_V1.to_vec(),
            ])
            .relay_mode(RelayMode::Disabled)
            .dns_resolver(crate::iroh_endpoint::hermetic_dns_resolver())
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind_addr loopback")
            .bind()
            .await
            .expect("bind iroh endpoint");
        Arc::new(IrohEndpoint::from_endpoint_for_test(inner))
    }

    /// ZEB-616 Component B identity guard (pure): a connection's drop-watcher
    /// may only evict the registry entry when the entry still points at THAT
    /// connection. A superseded watcher (stored != its own id) must not evict
    /// the connection that replaced it.
    #[test]
    fn should_evict_on_close_is_identity_guarded() {
        assert!(
            should_evict_on_close(Some(7), 7),
            "own conn still stored → evict"
        );
        assert!(
            !should_evict_on_close(Some(9), 7),
            "superseded (9 replaced 7) → keep"
        );
        assert!(
            !should_evict_on_close(None, 7),
            "already gone → nothing to evict"
        );
    }

    /// ZEB-616 Component A: a same-zid reconnect closes the stale connection
    /// it replaces before admitting the new link, and the registry ends with
    /// exactly the reconnect (no stale duplicate). The two bob endpoints
    /// share one secret → one `EndpointId` → one deterministic zid, modelling
    /// a silent mid-session drop + socket rebind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn zenoh_reconnect_closes_stale_connection() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            Duration::from_secs(45),
            zenoh_reconnect_closes_stale_connection_inner(),
        )
        .await
        .expect("test must finish within 45s");
    }

    async fn zenoh_reconnect_closes_stale_connection_inner() {
        // Alice: link manager + accept loop.
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            ReachabilityResolver::new(),
            new_link_tx,
        ));
        let _accept = alice_mgr.spawn_accept_loop();

        // Bob's stable identity across two endpoints (a rebind that keeps the
        // node-id → same deterministic zid). `buf` is Copy, so re-deriving the
        // key three times avoids depending on `SecretKey: Clone`.
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        let bob_id = SecretKey::from_bytes(&buf).public();
        let bob_ep1 = build_hermetic_iroh_endpoint_with_secret(SecretKey::from_bytes(&buf)).await;
        let bob_ep2 = build_hermetic_iroh_endpoint_with_secret(SecretKey::from_bytes(&buf)).await;

        // Alice's dialable loopback address.
        let alice_node_id = alice_ep.node_id();
        let alice_socket = *alice_ep
            .bound_sockets()
            .first()
            .expect("alice has a bound socket");
        let alice_addr = EndpointAddr::new(alice_node_id).with_ip_addr(alice_socket);

        // First connection → alice registers bob under his node-id.
        let conn1 = bob_ep1
            .inner()
            .connect(alice_addr.clone(), alpn::HARMONY_ZENOH_V1)
            .await
            .expect("bob1 dial alice on zenoh ALPN");
        for _ in 0..300 {
            if alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "alice must register bob's first connection"
        );
        assert!(
            conn1.close_reason().is_none(),
            "conn1 open before reconnect"
        );

        // Reconnect: second endpoint, SAME node-id.
        let conn2 = bob_ep2
            .inner()
            .connect(alice_addr, alpn::HARMONY_ZENOH_V1)
            .await
            .expect("bob2 dial alice on zenoh ALPN (same node-id)");

        // THE FIX: alice closes the stale conn1 on the reconnect. Pre-fix this
        // times out (alice never closes it).
        tokio::time::timeout(Duration::from_secs(10), conn1.closed())
            .await
            .expect("ZEB-616: alice must close the stale connection on reconnect");

        // The reconnect stays live; registry holds exactly one entry for bob.
        assert!(conn2.close_reason().is_none(), "reconnect must stay open");
        for _ in 0..300 {
            if alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            alice_mgr.zenoh_conns.lock().unwrap().len(),
            1,
            "registry holds exactly the reconnect, no stale duplicate"
        );

        alice_ep.shutdown().await;
        bob_ep1.shutdown().await;
        bob_ep2.shutdown().await;
    }

    /// ZEB-616 Component B: when a registered connection closes, its watcher
    /// evicts the registry entry (map stays bounded to live peers).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn zenoh_conn_registry_evicts_on_drop() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            Duration::from_secs(45),
            zenoh_conn_registry_evicts_on_drop_inner(),
        )
        .await
        .expect("test must finish within 45s");
    }

    async fn zenoh_conn_registry_evicts_on_drop_inner() {
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            ReachabilityResolver::new(),
            new_link_tx,
        ));
        let _accept = alice_mgr.spawn_accept_loop();

        let bob_ep = build_hermetic_iroh_endpoint().await;
        let bob_id = bob_ep.node_id();

        let alice_socket = *alice_ep
            .bound_sockets()
            .first()
            .expect("alice has a bound socket");
        let alice_addr = EndpointAddr::new(alice_ep.node_id()).with_ip_addr(alice_socket);

        let conn = bob_ep
            .inner()
            .connect(alice_addr, alpn::HARMONY_ZENOH_V1)
            .await
            .expect("bob dial alice on zenoh ALPN");
        for _ in 0..300 {
            if alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "alice must register bob's connection"
        );

        // Bob closes → alice's watcher evicts the entry.
        conn.close(0u32.into(), b"test-drop");
        for _ in 0..300 {
            if !alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "watcher must evict the registry entry when the connection closes"
        );

        alice_ep.shutdown().await;
        bob_ep.shutdown().await;
    }

    /// ZEB-620 Task 3: when a registered inbound connection closes, its
    /// drop-watcher both evicts the registry entry AND kicks the installed
    /// reconnect supervisor with `Dropped` (re-arming the peer's ladder); and a
    /// successful accept marks the peer connected. Extends
    /// `zenoh_conn_registry_evicts_on_drop` with the supervisor wiring.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn drop_watcher_kicks_supervisor() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            Duration::from_secs(45),
            drop_watcher_kicks_supervisor_inner(),
        )
        .await
        .expect("test must finish within 45s");
    }

    async fn drop_watcher_kicks_supervisor_inner() {
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            ReachabilityResolver::new(),
            new_link_tx,
        ));
        let handle = SupervisorHandle::new();
        assert!(
            alice_mgr.set_reconnect_handle(handle.clone()).is_ok(),
            "install reconnect handle once"
        );
        let _accept = alice_mgr.spawn_accept_loop();

        let bob_ep = build_hermetic_iroh_endpoint().await;
        let bob_id = bob_ep.node_id();

        let alice_socket = *alice_ep
            .bound_sockets()
            .first()
            .expect("alice has a bound socket");
        let alice_addr = EndpointAddr::new(alice_ep.node_id()).with_ip_addr(alice_socket);

        let conn = bob_ep
            .inner()
            .connect(alice_addr, alpn::HARMONY_ZENOH_V1)
            .await
            .expect("bob dial alice on zenoh ALPN");
        for _ in 0..300 {
            if alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "alice must register bob's connection"
        );

        // A successful inbound accept marks bob connected on the supervisor.
        for _ in 0..300 {
            if handle
                .states_snapshot()
                .iter()
                .any(|(p, _)| *p == *bob_id.as_bytes())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            handle
                .states_snapshot()
                .iter()
                .any(|(p, s)| *p == *bob_id.as_bytes()
                    && matches!(s, PeerStateWire::Connected { .. })),
            "inbound accept must mark bob connected on the supervisor"
        );

        // Bob closes → alice's watcher evicts the entry AND kicks Dropped.
        conn.close(0u32.into(), b"test-drop");
        for _ in 0..300 {
            if !alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "watcher must evict the registry entry on close"
        );
        for _ in 0..300 {
            if handle.pending_trigger(*bob_id.as_bytes()).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            handle.pending_trigger(*bob_id.as_bytes()),
            Some(ReconnectTrigger::Dropped),
            "drop-watcher must kick the supervisor with Dropped after a guarded eviction"
        );

        alice_ep.shutdown().await;
        bob_ep.shutdown().await;
    }

    /// ZEB-620 Task 3: the outbound `new_link` path registers its connection in
    /// the per-peer zenoh registry (parity with the inbound accept path), marks
    /// the peer connected on the supervisor, and installs a drop-watcher that
    /// evicts + kicks `Dropped` when the peer goes away.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn outbound_new_link_registers_and_watches() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            Duration::from_secs(45),
            outbound_new_link_registers_and_watches_inner(),
        )
        .await
        .expect("test must finish within 45s");
    }

    async fn outbound_new_link_registers_and_watches_inner() {
        // Alice: the dialer under test, with a supervisor handle installed.
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let alice_resolver = ReachabilityResolver::new();
        let (alice_tx, _alice_rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            alice_resolver.clone(),
            alice_tx,
        ));
        let handle = SupervisorHandle::new();
        assert!(
            alice_mgr.set_reconnect_handle(handle.clone()).is_ok(),
            "install reconnect handle once"
        );

        // Bob: the accept side — a full manager + accept loop so alice's
        // connect() completes the handshake and its open_bi() stream is
        // serviced (otherwise open_bi could stall waiting for a peer).
        let bob_ep = build_hermetic_iroh_endpoint().await;
        let (bob_tx, _bob_rx) = flume::unbounded::<LinkUnicast>();
        let bob_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&bob_ep),
            ReachabilityResolver::new(),
            bob_tx,
        ));
        let _bob_accept = bob_mgr.spawn_accept_loop();

        // Seed alice's resolver so new_link resolves bob's node-id → loopback.
        let bob_id = bob_ep.node_id();
        let bob_socket = *bob_ep
            .bound_sockets()
            .first()
            .expect("bob has a bound socket");
        alice_resolver.update(
            OwnerAddr([0xBB; 16]),
            ReachabilityAnnouncePayload {
                iroh_node_id: *bob_id.as_bytes(),
                home_relay_url: String::new(),
                direct_addresses: vec![bob_socket],
                announced_at_ms: 1,
                identity_signature: [0u8; 64],
                butler_set: vec![],
                bs_at: 0,
            },
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: String::new(),
            },
        );

        // Outbound dial via new_link.
        let link = alice_mgr
            .new_link(locator_from_endpoint_id(&bob_id).to_endpoint())
            .await
            .expect("outbound new_link must succeed");

        assert!(
            alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "outbound new_link must register the connection in the zenoh registry"
        );
        assert!(
            handle
                .states_snapshot()
                .iter()
                .any(|(p, s)| *p == *bob_id.as_bytes()
                    && matches!(s, PeerStateWire::Connected { .. })),
            "outbound swap success must mark the peer connected on the supervisor"
        );

        // Drop the accept side → alice's outbound connection closes → watcher
        // evicts the entry + kicks Dropped. iroh 1.0 moved endpoint drain onto
        // Drop, so merely dropping a handle may not promptly signal the remote;
        // close bob's registered inbound connection explicitly (the accept-side
        // drop) so alice's outbound `closed()` fires fast.
        drop(link);
        let bob_inbound = bob_mgr
            .zenoh_conns
            .lock()
            .unwrap()
            .get(&alice_ep.node_id())
            .cloned();
        if let Some(c) = bob_inbound {
            c.close(0u32.into(), b"test-accept-side-drop");
        }
        bob_ep.shutdown().await;
        for _ in 0..300 {
            if !alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "watcher must evict the outbound entry when the peer drops"
        );
        for _ in 0..300 {
            if handle.pending_trigger(*bob_id.as_bytes()).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            handle.pending_trigger(*bob_id.as_bytes()),
            Some(ReconnectTrigger::Dropped),
            "outbound drop-watcher must kick the supervisor with Dropped"
        );

        alice_ep.shutdown().await;
        // bob_ep already shut down above.
    }

    /// ZEB-620 Task 3: a superseded connection's drop-watcher must NOT kick the
    /// supervisor. When a same-zid reconnect swaps conn2 in and closes conn1,
    /// the identity guard suppresses conn1's eviction — so it must equally
    /// suppress conn1's `Dropped` kick, otherwise the live, replaced peer would
    /// be spuriously re-armed. The two bob endpoints share one secret → one
    /// node-id → one deterministic zid, modelling a silent drop + rebind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn superseded_conn_drop_does_not_kick() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            Duration::from_secs(45),
            superseded_conn_drop_does_not_kick_inner(),
        )
        .await
        .expect("test must finish within 45s");
    }

    async fn superseded_conn_drop_does_not_kick_inner() {
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            ReachabilityResolver::new(),
            new_link_tx,
        ));
        let handle = SupervisorHandle::new();
        assert!(
            alice_mgr.set_reconnect_handle(handle.clone()).is_ok(),
            "install reconnect handle once"
        );
        let _accept = alice_mgr.spawn_accept_loop();

        // Bob's stable identity across two endpoints (same node-id → same zid).
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        let bob_id = SecretKey::from_bytes(&buf).public();
        let bob_ep1 = build_hermetic_iroh_endpoint_with_secret(SecretKey::from_bytes(&buf)).await;
        let bob_ep2 = build_hermetic_iroh_endpoint_with_secret(SecretKey::from_bytes(&buf)).await;

        let alice_socket = *alice_ep
            .bound_sockets()
            .first()
            .expect("alice has a bound socket");
        let alice_addr = EndpointAddr::new(alice_ep.node_id()).with_ip_addr(alice_socket);

        // First connection → alice registers bob under his node-id.
        let conn1 = bob_ep1
            .inner()
            .connect(alice_addr.clone(), alpn::HARMONY_ZENOH_V1)
            .await
            .expect("bob1 dial alice on zenoh ALPN");
        for _ in 0..300 {
            if alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "alice must register bob's first connection"
        );

        // Reconnect: same node-id → alice closes conn1 (close-stale-first) and
        // swaps conn2 in. conn1's watcher fires but the guard suppresses it.
        let conn2 = bob_ep2
            .inner()
            .connect(alice_addr, alpn::HARMONY_ZENOH_V1)
            .await
            .expect("bob2 dial alice on zenoh ALPN (same node-id)");
        tokio::time::timeout(Duration::from_secs(10), conn1.closed())
            .await
            .expect("alice must close the stale conn1 on reconnect");

        // Let conn1's watcher run its (suppressed) eviction branch to the end.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The registry still holds exactly the reconnect (conn2), and NO
        // Dropped kick was raised — the superseded conn1 close is silent.
        assert!(
            alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            "the reconnect (conn2) must remain registered"
        );
        assert_eq!(
            handle.pending_trigger(*bob_id.as_bytes()),
            None,
            "a superseded connection's close must NOT kick the supervisor"
        );
        assert!(conn2.close_reason().is_none(), "reconnect must stay open");

        alice_ep.shutdown().await;
        bob_ep1.shutdown().await;
        bob_ep2.shutdown().await;
    }

    /// Resolver-miss → `new_link` returns an error. This is the only
    /// path we exercise here; a full round-trip lives in Task 10
    /// (two-engine integration) — Task 5 burned six hours on a QUIC
    /// teardown deadlock, and the rule for Task 6 is no actual iroh
    /// connections (see implementer brief, hard rule 6).
    #[tokio::test]
    async fn new_link_errors_on_resolver_miss() {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            new_link_errors_on_resolver_miss_inner(),
        )
        .await
        .expect("test must finish within 5s");
    }

    async fn new_link_errors_on_resolver_miss_inner() {
        let endpoint = build_hermetic_iroh_endpoint().await;
        let resolver = ReachabilityResolver::new();
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let mgr = IrohZenohLinkManager::new(Arc::clone(&endpoint), resolver, new_link_tx);

        // Build a locator with a *different* iroh EndpointId (one the
        // resolver has never seen). new_link must fail before any QUIC
        // traffic is attempted.
        //
        // We derive the EndpointId from a fresh SecretKey rather than
        // calling `EndpointId::from_bytes(random)` directly:
        // `EndpointId` (an Ed25519 public key) validates input as a
        // canonical curve point, which random 32-byte buffers only
        // satisfy roughly half the time. SecretKey::generate() always
        // produces a valid pub key.
        let mut bogus_seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bogus_seed);
        let bogus_id = SecretKey::from_bytes(&bogus_seed).public();
        let bogus_locator = locator_from_endpoint_id(&bogus_id);

        let result = mgr.new_link(bogus_locator.to_endpoint()).await;
        assert!(
            result.is_err(),
            "expected resolver miss to surface as Err, got Ok"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("no ReachabilityRecord"),
            "error message should mention missing record, got: {err_msg}"
        );

        endpoint.shutdown().await;
    }

    /// Malformed locator address (not 64-char hex) → `new_link` returns
    /// an error and never touches the resolver.
    #[tokio::test]
    async fn new_link_errors_on_malformed_locator() {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            new_link_errors_on_malformed_locator_inner(),
        )
        .await
        .expect("test must finish within 5s");
    }

    async fn new_link_errors_on_malformed_locator_inner() {
        let endpoint = build_hermetic_iroh_endpoint().await;
        let resolver = ReachabilityResolver::new();
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let mgr = IrohZenohLinkManager::new(Arc::clone(&endpoint), resolver, new_link_tx);

        // "not-hex" → hex::decode fails inside parse_endpoint_id.
        let bad = Locator::new(IROH_LOCATOR_PROTOCOL, "not-a-hex-endpoint-id", "")
            .expect("locator string itself is structurally valid");
        let result = mgr.new_link(bad.to_endpoint()).await;
        assert!(result.is_err(), "expected malformed-locator Err");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("not a 64-char hex"),
            "error message should mention hex parse failure, got: {err_msg}"
        );

        endpoint.shutdown().await;
    }

    /// `resolve_by_node_id` finds the matching `OwnerAddr` + payload
    /// when the resolver has a record for the given iroh EndpointId.
    /// (The actual `new_link` connect path beyond this point requires
    /// a real iroh peer, which is Task 10's territory.)
    #[test]
    fn resolver_lookup_by_node_id_finds_match() {
        let resolver = ReachabilityResolver::new();
        let actor = OwnerAddr([0xAB; 16]);
        let payload = ReachabilityAnnouncePayload {
            iroh_node_id: [0xCD; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        };
        let hlc = Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test".into(),
        };
        resolver.update(actor, payload.clone(), hlc);

        let found = resolver.resolve_by_node_id(&[0xCD; 32]);
        assert!(found.is_some(), "node-id lookup must hit");
        let (found_actor, found_payload) = found.unwrap();
        assert_eq!(found_actor, actor);
        assert_eq!(found_payload, payload);

        // Wrong node id → None.
        assert!(resolver.resolve_by_node_id(&[0xEE; 32]).is_none());
    }

    /// ZEB-325 PR #159 R2: inbound `harmony/handshake/v1` connections
    /// that arrive BEFORE the dispatcher is installed are queued and
    /// then drained when install runs. The drain dispatches each
    /// queued connection on the newly-installed dispatcher.
    ///
    /// Without the queue, those connections were silently dropped
    /// (Bob saw `inviter_unreachable` despite Alice being online).
    /// This test simulates that boot-window race: dial first, install
    /// second, assert the dispatcher receives the connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn handshake_connection_queued_pre_install_dispatched_on_install() {
        // ZEB-347: prime the one-time process-global iroh bind init before
        // the asserted timeout (see `iroh_endpoint::warm_up_iroh_global_init`).
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        // 45s outer timeout — uses real iroh QUIC + hermetic
        // loopback bind; under heavy nextest contention each iroh
        // bind can take 10-15s. The existing integration test
        // (`pkarr_iroh_redeem_full_integration`) uses a 60s outer
        // timeout for similar reasons.
        tokio::time::timeout(
            std::time::Duration::from_secs(45),
            handshake_connection_queued_pre_install_dispatched_on_install_inner(),
        )
        .await
        .expect("test must finish within 45s");
    }

    async fn handshake_connection_queued_pre_install_dispatched_on_install_inner() {
        // Alice: link manager with accept loop, no dispatcher installed.
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let resolver = ReachabilityResolver::new();
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            resolver,
            new_link_tx,
        ));
        let _accept_handle = alice_mgr.spawn_accept_loop();

        // Bob: bare iroh endpoint to dial alice on the handshake ALPN.
        let bob_ep = build_hermetic_iroh_endpoint().await;

        // Build bob's EndpointAddr pointing at alice's loopback bound
        // socket. iroh's `connect` needs at least a direct addr (no
        // pkarr / relay in this hermetic build).
        let alice_node_id = alice_ep.node_id();
        let mut alice_addr = EndpointAddr::new(alice_node_id);
        let alice_socket = *alice_ep
            .bound_sockets()
            .first()
            .expect("alice has a bound socket");
        alice_addr = alice_addr.with_ip_addr(alice_socket);

        // Dial bob → alice on the handshake ALPN. The accept loop will
        // observe no dispatcher and push the Connection onto the
        // pending queue.
        let _conn = bob_ep
            .inner()
            .connect(alice_addr, alpn::HARMONY_HANDSHAKE_V1)
            .await
            .expect("dial alice on handshake ALPN");

        // Wait for the accept-side task to enqueue. The accept loop is
        // async — we poll the queue depth with a short backoff until
        // it observes the connection (or the outer 15s timeout fires).
        for _ in 0..100 {
            let depth = alice_mgr.pending_handshakes.lock().await.len();
            if depth >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            alice_mgr.pending_handshakes.lock().await.len(),
            1,
            "handshake connection should be queued pre-install"
        );

        // Install a stub dispatcher; drain should hand the queued
        // connection off to it.
        let dispatched = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stub = Arc::new(StubDispatcher {
            count: Arc::clone(&dispatched),
        });
        let install_res = alice_mgr.install_handshake_dispatcher(stub).await;
        assert!(install_res.is_ok(), "install must succeed");

        // Drain runs in a spawned task; wait briefly for it to land.
        for _ in 0..100 {
            if dispatched.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            dispatched.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "drain must dispatch the queued connection"
        );
        // Post-drain the queue is empty (invariant: empty whenever
        // dispatcher is set).
        assert_eq!(alice_mgr.pending_handshakes.lock().await.len(), 0);

        alice_ep.shutdown().await;
        bob_ep.shutdown().await;
    }

    /// Stub dispatcher: counts invocations, drops the connection on
    /// receipt. Used by the queue-drain test above.
    struct StubDispatcher {
        count: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl IrohHandshakeDispatcher for StubDispatcher {
        async fn handle_connection(&self, _conn: Connection) {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// ZEB-325 PR #159 R4-2 (CodeRabbit NITPICK): regression dispatcher
    /// for the parallel-drain test. Each invocation:
    ///   1. Atomically bumps an "observed" counter + records its index.
    ///   2. The FIRST call (index 0) blocks on a Notify until the
    ///      second call has been observed — proving the drain is NOT
    ///      sequential (a sequential drain would never reach the
    ///      second call before the first returns).
    ///   3. Bumps a separate "completed" counter when it returns.
    struct GatingDispatcher {
        observed: Arc<std::sync::atomic::AtomicUsize>,
        completed: Arc<std::sync::atomic::AtomicUsize>,
        release_first: Arc<tokio::sync::Notify>,
        // Signals that the second observation has happened, so the
        // test (or the first call itself) knows when to release the
        // gate. Kept separate from `observed` so we don't conflate
        // "second seen" with "ordering of bumps". `Notify::notify_one`
        // permit semantics: missed notifies before await still wake.
        second_observed: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl IrohHandshakeDispatcher for GatingDispatcher {
        async fn handle_connection(&self, _conn: Connection) {
            // fetch_add returns the prior value, so index 0 is the
            // first observation.
            let idx = self
                .observed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if idx == 0 {
                // Block until the second connection is observed.
                // 5s ceiling so a regression (sequential drain) fails
                // fast with a clear assertion instead of hanging the
                // test's outer 15s timeout.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    self.second_observed.notified(),
                )
                .await;
                // Then wait for the test to release us.
                self.release_first.notified().await;
            } else {
                // Second (or later) call signals first to unblock.
                self.second_observed.notify_one();
            }
            self.completed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// ZEB-325 PR #159 R4-2 (CodeRabbit NITPICK): regression for the
    /// R3-3 spawn-per-conn fix. The original sequential drain would
    /// have processed boot-window queued handshake connections one at
    /// a time — a single slow handshake (real production case: 8s
    /// engine bootstrap inside the acceptor) would have pinned every
    /// queued connection behind it for its full IO+poll deadline.
    ///
    /// The R3-3 fix replaced the sequential drain with
    /// `tokio::spawn` per-connection. Without this test, a future
    /// revert to sequential drain would still pass the original
    /// one-connection boot-window test (which only enqueues one).
    ///
    /// Strategy:
    ///   - Pre-install: enqueue TWO connections.
    ///   - Install a GatingDispatcher whose first call blocks until
    ///     its second call has been observed.
    ///   - Assert both connections are observed (proves parallelism).
    ///   - Release the gate, assert both complete.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn drain_dispatches_queued_connections_in_parallel() {
        // ZEB-347: prime the one-time process-global iroh bind init before
        // the asserted timeout (see `iroh_endpoint::warm_up_iroh_global_init`).
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(45),
            drain_dispatches_queued_connections_in_parallel_inner(),
        )
        .await
        .expect("test must finish within 45s");
    }

    async fn drain_dispatches_queued_connections_in_parallel_inner() {
        // Alice: link manager with accept loop, no dispatcher installed.
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let resolver = ReachabilityResolver::new();
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            resolver,
            new_link_tx,
        ));
        let _accept_handle = alice_mgr.spawn_accept_loop();

        // Two bob endpoints — each dials independently so we get two
        // distinct queued connections on alice. Using two endpoints
        // (rather than two streams over one conn) matches what the
        // production accept loop enqueues: one Connection per inbound
        // dial.
        let bob1_ep = build_hermetic_iroh_endpoint().await;
        let bob2_ep = build_hermetic_iroh_endpoint().await;

        let alice_node_id = alice_ep.node_id();
        let alice_socket = *alice_ep
            .bound_sockets()
            .first()
            .expect("alice has a bound socket");
        let mut alice_addr = EndpointAddr::new(alice_node_id);
        alice_addr = alice_addr.with_ip_addr(alice_socket);

        // Dial both bobs sequentially. (Dialing in parallel would
        // race the accept-side enqueue and risk one being processed
        // by the fast path if install happened to land in between,
        // which the test doesn't exercise.)
        let _conn1 = bob1_ep
            .inner()
            .connect(alice_addr.clone(), alpn::HARMONY_HANDSHAKE_V1)
            .await
            .expect("dial #1 alice on handshake ALPN");
        let _conn2 = bob2_ep
            .inner()
            .connect(alice_addr, alpn::HARMONY_HANDSHAKE_V1)
            .await
            .expect("dial #2 alice on handshake ALPN");

        // Wait for both to land on the queue.
        for _ in 0..200 {
            let depth = alice_mgr.pending_handshakes.lock().await.len();
            if depth >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            alice_mgr.pending_handshakes.lock().await.len(),
            2,
            "both handshake connections should be queued pre-install"
        );

        // Install the gating dispatcher.
        let observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release_first = Arc::new(tokio::sync::Notify::new());
        let second_observed = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(GatingDispatcher {
            observed: Arc::clone(&observed),
            completed: Arc::clone(&completed),
            release_first: Arc::clone(&release_first),
            second_observed: Arc::clone(&second_observed),
        });
        let install_res = alice_mgr.install_handshake_dispatcher(gate).await;
        assert!(install_res.is_ok(), "install must succeed");

        // Wait for BOTH dispatches to be observed (the parallel
        // invariant). The first is blocked inside handle_connection,
        // so a sequential drain would never get to the second within
        // any reasonable budget — observed would stay at 1 until the
        // (untriggered) gate releases.
        for _ in 0..200 {
            if observed.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let observed_now = observed.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            observed_now, 2,
            "drain must dispatch both queued connections in parallel; \
             observed only {observed_now}/2 within 2s — this is the regression \
             signal for the R3-3 spawn-per-conn fix"
        );

        // At this point the second dispatch has completed (only the
        // first is blocked on release_first). Verify completed has
        // advanced past the gate for the second call.
        for _ in 0..50 {
            if completed.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            completed.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "the second dispatch must have completed while the first is blocked"
        );

        // Release the first dispatch and assert it completes too.
        release_first.notify_one();
        for _ in 0..200 {
            if completed.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "both dispatches must complete after release"
        );

        // Post-drain the queue is empty.
        assert_eq!(alice_mgr.pending_handshakes.lock().await.len(), 0);

        alice_ep.shutdown().await;
        bob1_ep.shutdown().await;
        bob2_ep.shutdown().await;
    }

    /// Sanity: `locator_from_endpoint_id` round-trips through
    /// `parse_endpoint_id`.
    ///
    /// Deriving the `EndpointId` from a fresh `SecretKey` (not from
    /// random bytes) — `EndpointId::from_bytes` validates the input is
    /// a canonical Ed25519 public key, which random bytes only happen
    /// to be ~50% of the time (the y-coordinate sign bit must match a
    /// curve point), so the original `from_bytes(random)` form is
    /// flaky. Going through `SecretKey::generate().public()`
    /// guarantees a valid pub key.
    #[test]
    fn locator_round_trips_through_parser() {
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        let id = SecretKey::from_bytes(&buf).public();
        let locator = locator_from_endpoint_id(&id);

        let parsed = IrohZenohLinkManager::parse_endpoint_id(&locator.to_endpoint())
            .expect("locator parses back into EndpointId");
        assert_eq!(parsed, id);
    }

    /// Poll `cond` every `step` until it returns `true` or `max` elapses.
    /// Returns whether the condition became true within the budget. Each
    /// invocation of `cond` is a synchronous lock-and-read (never held across
    /// the sleep) — the same poll-with-interval idiom the ZEB-616 tests above
    /// spell out inline, factored out for the multi-phase acceptance test.
    async fn poll_until<F: FnMut() -> bool>(mut cond: F, max: Duration, step: Duration) -> bool {
        let deadline = Instant::now() + max;
        loop {
            if cond() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(step).await;
        }
    }

    /// ZEB-620 Task 7: a hermetic [`PeerDialer`] that dials through a real
    /// [`IrohZenohLinkManager`]'s outbound `new_link` — the same per-peer
    /// registry-registration seam production's `RuntimePeerDialer` reaches via
    /// zenoh's `connect_peer`. Returns whether the iroh link established
    /// (registered + bi-stream open), the hermetic analogue of `connect_peer`'s
    /// success bool. A literal second in-process zenoh `Runtime` (which
    /// `connect_peer` + a real zenoh GET would require) is impossible here: the
    /// iroh session ctx is a process-global singleton (see
    /// `iroh_zenoh_registration` and `tests/zeb_373_dynamic_dial_integration.rs`),
    /// so the dialer exercises the registry seam directly.
    struct SupervisorLinkDialer {
        mgr: Arc<IrohZenohLinkManager>,
    }

    #[async_trait]
    impl crate::iroh_dial_driver::PeerDialer for SupervisorLinkDialer {
        async fn dial(&self, node_id: [u8; 32], _locator: String) -> bool {
            let id = match EndpointId::from_bytes(&node_id) {
                Ok(id) => id,
                Err(_) => return false,
            };
            self.mgr
                .new_link(locator_from_endpoint_id(&id).to_endpoint())
                .await
                .is_ok()
        }
    }

    /// ZEB-620 Task 7 (acceptance): a live zenoh-over-iroh link that is
    /// hard-dropped on the acceptor side is recovered by the reconnect
    /// supervisor WITHOUT any manual re-dial. The real `run_reconnect_supervisor`
    /// task — driven only by the registry drop-watcher's `Dropped` kick —
    /// re-dials the peer through the outbound registry seam (`new_link`, the
    /// same seam production's `RuntimePeerDialer` reaches via `connect_peer`),
    /// reinstalling the peer's connection (exactly one live conn, stale reaped)
    /// and marking it `Connected`.
    ///
    /// Acceptance assertions (ZEB-620 amendment — the `reconnected` ring marker
    /// is unwired, fires on first-connects too, so it is NOT asserted): after
    /// recovery the supervisor telemetry shows ≥1 dial attempt and a `succeeded`
    /// hit FOR THE PEER, and the supervisor's `states_snapshot` reports the peer
    /// `Connected`. The registry holds exactly one live connection for the peer.
    ///
    /// Extends ZEB-616's `zenoh_reconnect_closes_stale_connection`: alice = the
    /// dialer under test (manager + accept loop + real supervisor); bob = a bare
    /// acceptor (manager + accept loop, no supervisor). A second in-process
    /// zenoh `Runtime` for a literal zenoh GET is impossible (the iroh session
    /// ctx is a process-global singleton — see [`SupervisorLinkDialer`]), so the
    /// recovered link's usability is proven at the link layer: a reinstalled,
    /// live connection (`close_reason().is_none()`) established end-to-end
    /// through `new_link`'s `open_bi()` against bob's accept loop.
    ///
    /// Runtime is ~60-120s: two real iroh handshakes (establish + re-dial) under
    /// the `iroh-endpoint` nextest-group throttle plus iroh 1.0's Drop-drain
    /// teardown. Generous outer budget + fat per-phase poll windows per the
    /// ZEB-616 idiom; no assertion is weakened to fit the clock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn supervisor_redials_after_drop_and_get_answers() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            Duration::from_secs(180),
            supervisor_redials_after_drop_and_get_answers_inner(),
        )
        .await
        .expect("test must finish within 180s");
    }

    async fn supervisor_redials_after_drop_and_get_answers_inner() {
        use crate::network_health::DialTelemetry;
        use crate::reconnect_supervisor::{run_reconnect_supervisor, SupervisorConfig};

        // ── Alice: the dialer under test — manager + accept loop + a real
        // reconnect supervisor. ──
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let alice_id = alice_ep.node_id();
        let alice_resolver = ReachabilityResolver::new();
        let (alice_tx, _alice_rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            alice_resolver.clone(),
            alice_tx,
        ));
        let handle = SupervisorHandle::new();
        assert!(
            alice_mgr.set_reconnect_handle(handle.clone()).is_ok(),
            "install reconnect handle once"
        );
        let _alice_accept = alice_mgr.spawn_accept_loop();

        // ── Bob: a bare acceptor — manager + accept loop, no supervisor. Alice
        // dials bob; bob only accepts. ──
        let bob_ep = build_hermetic_iroh_endpoint().await;
        let bob_id = bob_ep.node_id();
        let (bob_tx, _bob_rx) = flume::unbounded::<LinkUnicast>();
        let bob_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&bob_ep),
            ReachabilityResolver::new(),
            bob_tx,
        ));
        let _bob_accept = bob_mgr.spawn_accept_loop();

        // Seed alice's resolver so BOTH the supervisor's dispatch gate and the
        // dialer's `new_link` resolve bob's node-id → loopback socket. Left in
        // place across the drop so the supervisor's autonomous re-dial resolves
        // bob again (an evicted-from-resolver peer would soft-fail instead).
        let bob_socket = *bob_ep
            .bound_sockets()
            .first()
            .expect("bob has a bound socket");
        alice_resolver.update(
            OwnerAddr([0xBB; 16]),
            ReachabilityAnnouncePayload {
                iroh_node_id: *bob_id.as_bytes(),
                home_relay_url: String::new(),
                direct_addresses: vec![bob_socket],
                announced_at_ms: 1,
                identity_signature: [0u8; 64],
                butler_set: vec![],
                bs_at: 0,
            },
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: String::new(),
            },
        );

        // ── Real supervisor on alice, dialing through the registry seam. Fast
        // config so both the first dial and the post-drop re-dial land in
        // seconds regardless of the alice/bob NodeId dial-role ordering (base +
        // fallback both 400ms). The 400ms base also keeps the post-eviction /
        // pre-reinstall window comfortably observable by the 25ms poll below. ──
        let telemetry = Arc::new(DialTelemetry::new());
        let dialer = Arc::new(SupervisorLinkDialer {
            mgr: Arc::clone(&alice_mgr),
        });
        let config = SupervisorConfig {
            retry_base: Duration::from_millis(400),
            retry_cap: Duration::from_secs(4),
            dormant_after: Duration::from_secs(3600),
            presence_sweep_cooldown: Duration::from_secs(30),
            max_concurrent_dials: 4,
            // Real-time test over live endpoints under the nextest iroh-endpoint
            // throttle group: a slow-but-progressing `new_link` is NOT a hung
            // dial. A tight bound here cancels it mid-`open_bi` — after the
            // registry swap already marked the peer Connected — recording a
            // spurious `failed` whose stale-epoch result is discarded, so no
            // retry ever fires. Keep the bound far above worst contention.
            dial_timeout: Duration::from_secs(300),
            higher_id_fallback_delay: Duration::from_millis(400),
            // ZEB-910: parole disabled for this real-time test (never fires
            // inside its wall-clock window).
            parole_interval: Duration::from_secs(3600),
            parole_batch: 2,
            jitter_seed: Some(0x2E_B6_20),
        };
        let _supervisor = tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer,
            Arc::new(alice_resolver.clone()),
            Arc::clone(&telemetry),
            *alice_id.as_bytes(),
            config,
        ));

        // First-learn kick → supervisor dials bob → `new_link` registers bob in
        // alice's registry + marks him Connected. Kicking directly keeps the
        // establish deterministic; the recovery under test is driven by the REAL
        // drop-watcher wiring, not this kick.
        handle.kick(*bob_id.as_bytes(), ReconnectTrigger::NewPeer);

        // ── Phase 1: establish. ──
        let established = poll_until(
            || alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            Duration::from_secs(60),
            Duration::from_millis(25),
        )
        .await;
        assert!(
            established,
            "supervisor's first dial must register bob in alice's registry"
        );
        let connected_1 = poll_until(
            || {
                handle.states_snapshot().iter().any(|(p, s)| {
                    *p == *bob_id.as_bytes() && matches!(s, PeerStateWire::Connected { .. })
                })
            },
            Duration::from_secs(15),
            Duration::from_millis(25),
        )
        .await;
        assert!(
            connected_1,
            "supervisor must mark bob Connected after the first dial"
        );
        // `Connected` becomes visible at the registry swap INSIDE `new_link`,
        // before the dial task's `record_succeeded` runs (the tail of `new_link`
        // — `open_bi` — sits between them, and can take seconds under the
        // throttle group). Poll for the telemetry instead of instant-asserting:
        // the ring push is the LAST step of `record_succeeded`, so once it is
        // visible the `succeeded` counter is too.
        let bob_short = hex::encode(&bob_id.as_bytes()[..4]);
        let succeeded_recorded = poll_until(
            || {
                telemetry
                    .summary()
                    .recent
                    .iter()
                    .any(|h| h.outcome == "succeeded" && h.node_id_short == bob_short)
            },
            Duration::from_secs(15),
            Duration::from_millis(25),
        )
        .await;
        assert!(
            succeeded_recorded,
            "first dial must record a succeeded hit for bob"
        );
        let succeeded_before = telemetry.summary().succeeded;
        assert!(
            succeeded_before >= 1,
            "succeeded counter must be visible with the ring hit, got {succeeded_before}"
        );

        // ── Phase 2: hard-drop on the acceptor side. iroh 1.0 drains on Drop, so
        // a handle drop may not promptly signal the remote — close bob's
        // registered inbound conn from alice EXPLICITLY. Alice's outbound conn's
        // `closed()` then fires → drop-watcher evicts + kicks `Dropped`. ──
        let bob_has_inbound = poll_until(
            || bob_mgr.zenoh_conns.lock().unwrap().contains_key(&alice_id),
            Duration::from_secs(15),
            Duration::from_millis(25),
        )
        .await;
        assert!(
            bob_has_inbound,
            "bob's accept loop must have registered alice's inbound connection"
        );
        let inbound = bob_mgr
            .zenoh_conns
            .lock()
            .unwrap()
            .get(&alice_id)
            .cloned()
            .expect("bob's inbound conn from alice");
        inbound.close(0u32.into(), b"zeb620-task7-hard-drop");

        // Alice detects the drop and evicts bob (registry goes empty for him).
        let evicted = poll_until(
            || !alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id),
            Duration::from_secs(45),
            Duration::from_millis(25),
        )
        .await;
        assert!(
            evicted,
            "alice's drop-watcher must evict bob after the acceptor-side hard drop"
        );

        // ── Phase 3: recovery — WITHOUT manual intervention the supervisor
        // re-dials (Dropped kick → re-armed ladder) and reinstalls bob. Recovery
        // is a NEW succeeded hit AND bob back in the registry; `new_link`
        // registers the conn strictly before `record_succeeded` fires, so the
        // combined predicate never observes the success ahead of the conn. ──
        let recovered = poll_until(
            || {
                alice_mgr.zenoh_conns.lock().unwrap().contains_key(&bob_id)
                    && telemetry.summary().succeeded > succeeded_before
            },
            Duration::from_secs(60),
            Duration::from_millis(25),
        )
        .await;
        assert!(
            recovered,
            "supervisor must autonomously re-dial and reinstall bob after the drop"
        );

        // Same-zid reinstall: exactly one live connection for the peer.
        {
            let map = alice_mgr.zenoh_conns.lock().unwrap();
            assert_eq!(
                map.len(),
                1,
                "registry holds exactly one connection after recovery (stale reaped)"
            );
            let conn = map
                .get(&bob_id)
                .expect("bob present in registry after recovery");
            assert!(
                conn.close_reason().is_none(),
                "the reinstalled connection must be live (open bi-stream, GET-ready)"
            );
        }

        // Snapshot reports bob Connected post-recovery.
        let connected_2 = poll_until(
            || {
                handle.states_snapshot().iter().any(|(p, s)| {
                    *p == *bob_id.as_bytes() && matches!(s, PeerStateWire::Connected { .. })
                })
            },
            Duration::from_secs(15),
            Duration::from_millis(25),
        )
        .await;
        assert!(
            connected_2,
            "supervisor's states_snapshot must report bob Connected post-recovery"
        );

        // Telemetry: a re-dial attempt AND a re-connect success beyond the
        // establish (bob is the only dialed peer, so the aggregate counts are
        // his). Stronger than the amendment's ≥1 floor — proves the recovery
        // dial specifically, not just the first connect.
        let summary = telemetry.summary();
        assert!(
            summary.attempts >= 2,
            "supervisor must record a re-dial attempt after the drop, got {}",
            summary.attempts
        );
        assert!(
            summary.succeeded >= 2,
            "supervisor must record a re-connect success after the drop, got {}",
            summary.succeeded
        );

        alice_ep.shutdown().await;
        bob_ep.shutdown().await;
    }

    /// ZEB-622 acceptance (full production chain): a real link registers
    /// Connected with a live mode + RTT in the liveness map; an explicit remote
    /// close lands Disconnected; and — with ZERO manual intervention — the real
    /// reconnect supervisor autonomously re-dials the same peer, re-registering
    /// Connected and re-arming the transport-epoch watch. Every up-edge bumps the
    /// registered epoch (the same-zid flap the accumulating seen-zid gate could
    /// never re-fire), proving the end-to-end chain: drop → supervisor re-dial →
    /// same-zid reinstall → liveness up-edge → transport-epoch re-arm.
    ///
    /// The full topology of `supervisor_redials_after_drop_and_get_answers` PLUS
    /// the liveness handle: alice installs BOTH `set_reconnect_handle` (drives
    /// dials + recovery through the `SupervisorLinkDialer` registry seam) and
    /// `set_liveness_handle` (owns the up/down edges wired into `swap_zenoh_conn`
    /// and `spawn_drop_watcher`); bob = a bare acceptor. The establish is a
    /// supervisor `NewPeer` kick; the recovery is driven only by the
    /// drop-watcher's `Dropped` kick — no manual re-link. Runtime is ~60-120s:
    /// two real iroh handshakes (establish + re-dial) under the `iroh-endpoint`
    /// nextest-group throttle plus iroh 1.0's Drop-drain teardown. Generous outer
    /// budget + fat per-phase poll windows per the ZEB-616/620 idiom.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn liveness_tracks_link_lifecycle_and_flap_bumps_epoch() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            Duration::from_secs(180),
            liveness_tracks_link_lifecycle_and_flap_bumps_epoch_inner(),
        )
        .await
        .expect("test must finish within 180s");
    }

    async fn liveness_tracks_link_lifecycle_and_flap_bumps_epoch_inner() {
        use crate::network_health::DialTelemetry;
        use crate::peer_liveness::{LivenessHandle, LivenessMode, LivenessStateWire};
        use crate::reconnect_supervisor::{run_reconnect_supervisor, SupervisorConfig};

        // ── Alice: the dialer under test — manager + accept loop + BOTH the
        // reconnect supervisor (drives dials + autonomous recovery) and a
        // liveness handle whose transport-epoch sink is wired. ──
        let alice_ep = build_hermetic_iroh_endpoint().await;
        let alice_id = alice_ep.node_id();
        let alice_resolver = ReachabilityResolver::new();
        let (alice_tx, _alice_rx) = flume::unbounded::<LinkUnicast>();
        let alice_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            alice_resolver.clone(),
            alice_tx,
        ));

        // Install BOTH production handles before the accept loop and the first
        // dial: the reconnect supervisor owns dials + autonomous recovery, the
        // liveness handle owns the up/down edges and the transport-epoch watch.
        // This is the full production topology — the recovery under test is
        // driven end-to-end, with zero manual re-link.
        let handle = SupervisorHandle::new();
        assert!(
            alice_mgr.set_reconnect_handle(handle.clone()).is_ok(),
            "install the reconnect handle once"
        );
        let liveness = LivenessHandle::new();
        let (etx, erx) = tokio::sync::watch::channel(0u64);
        liveness.set_transport_epoch_tx(etx);
        assert!(
            alice_mgr.set_liveness_handle(liveness.clone()).is_ok(),
            "install the liveness handle once"
        );

        let _alice_accept = alice_mgr.spawn_accept_loop();

        // ── Bob: a bare acceptor — manager + accept loop, no liveness. Alice
        // dials bob; bob only accepts. ──
        let bob_ep = build_hermetic_iroh_endpoint().await;
        let bob_id = bob_ep.node_id();
        let (bob_tx, _bob_rx) = flume::unbounded::<LinkUnicast>();
        let bob_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&bob_ep),
            ReachabilityResolver::new(),
            bob_tx,
        ));
        let _bob_accept = bob_mgr.spawn_accept_loop();

        // Seed alice's resolver so `new_link` resolves bob's node-id → loopback
        // socket. Left in place across the close so the re-link resolves bob
        // again.
        let bob_socket = *bob_ep
            .bound_sockets()
            .first()
            .expect("bob has a bound socket");
        alice_resolver.update(
            OwnerAddr([0xBB; 16]),
            ReachabilityAnnouncePayload {
                iroh_node_id: *bob_id.as_bytes(),
                home_relay_url: String::new(),
                direct_addresses: vec![bob_socket],
                announced_at_ms: 1,
                identity_signature: [0u8; 64],
                butler_set: vec![],
                bs_at: 0,
            },
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: String::new(),
            },
        );

        // ── Real reconnect supervisor on alice, dialing through the registry
        // seam — the same `SupervisorLinkDialer` + config as the ZEB-620
        // acceptance test (`supervisor_redials_after_drop_and_get_answers`).
        // This is what turns the DROP into an autonomous recovery: no manual
        // re-link, just the drop-watcher's `Dropped` kick re-arming the ladder. ──
        let dialer = Arc::new(SupervisorLinkDialer {
            mgr: Arc::clone(&alice_mgr),
        });
        let config = SupervisorConfig {
            retry_base: Duration::from_millis(400),
            retry_cap: Duration::from_secs(4),
            dormant_after: Duration::from_secs(3600),
            presence_sweep_cooldown: Duration::from_secs(30),
            max_concurrent_dials: 4,
            // Real-time test over live endpoints under the nextest iroh-endpoint
            // throttle group: a slow-but-progressing `new_link` is NOT a hung
            // dial. A tight bound would cancel it mid-`open_bi` — after the
            // registry swap already marked the peer Connected — recording a
            // spurious `failed` whose stale-epoch result is discarded, so no
            // retry ever fires. Keep the bound far above worst contention.
            dial_timeout: Duration::from_secs(300),
            higher_id_fallback_delay: Duration::from_millis(400),
            // ZEB-910: parole disabled for this real-time test (never fires
            // inside its wall-clock window).
            parole_interval: Duration::from_secs(3600),
            parole_batch: 2,
            jitter_seed: Some(0x2E_B6_20),
        };
        let _supervisor = tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer,
            Arc::new(alice_resolver.clone()),
            Arc::new(DialTelemetry::new()),
            *alice_id.as_bytes(),
            config,
        ));

        let bob_peer = *bob_id.as_bytes();
        let is_connected_direct = |liveness: &LivenessHandle| {
            liveness.states_snapshot().iter().any(|(p, s)| {
                *p == bob_peer
                    && matches!(
                        s,
                        LivenessStateWire::Connected {
                            mode: LivenessMode::Direct,
                            rtt_ms: Some(_),
                            ..
                        }
                    )
            })
        };

        // ── Phase 1: establish. A first-learn `NewPeer` kick drives the
        // supervisor to dial bob through the `SupervisorLinkDialer` registry seam
        // (`new_link`); the swap's up-edge + spawned path watcher land bob in the
        // liveness map. The watcher promotes Degraded → Connected once a path is
        // selected — a hermetic loopback link with RelayMode::Disabled selects a
        // direct Ip path (mode Direct). Kicking directly keeps the establish
        // deterministic; the recovery under test is driven by the REAL
        // drop-watcher wiring, not this kick. ──
        handle.kick(*bob_id.as_bytes(), ReconnectTrigger::NewPeer);
        let connected_1 = poll_until(
            || is_connected_direct(&liveness),
            Duration::from_secs(60),
            Duration::from_millis(50),
        )
        .await;
        assert!(
            connected_1,
            "first link must register bob Connected(Direct, rtt) in the liveness map"
        );
        assert_eq!(
            *erx.borrow(),
            1,
            "the establish up-edge bumps the transport epoch exactly once"
        );

        // ── Phase 2: explicit remote close (same pattern as the ZEB-620 test's
        // Phase 2). Closing bob's registered inbound conn makes alice's outbound
        // conn's `closed()` fire → the drop-watcher evicts + raises the liveness
        // down-edge. ──
        let bob_has_inbound = poll_until(
            || bob_mgr.zenoh_conns.lock().unwrap().contains_key(&alice_id),
            Duration::from_secs(15),
            Duration::from_millis(25),
        )
        .await;
        assert!(
            bob_has_inbound,
            "bob's accept loop must have registered alice's inbound connection"
        );
        let inbound = bob_mgr
            .zenoh_conns
            .lock()
            .unwrap()
            .get(&alice_id)
            .cloned()
            .expect("bob's inbound conn from alice");
        inbound.close(0u32.into(), b"zeb622-liveness-close");

        let disconnected = poll_until(
            || {
                liveness.states_snapshot().iter().any(|(p, s)| {
                    *p == bob_peer && matches!(s, LivenessStateWire::Disconnected { .. })
                })
            },
            Duration::from_secs(45),
            Duration::from_millis(50),
        )
        .await;
        assert!(
            disconnected,
            "an explicit remote close must land bob Disconnected in the liveness map"
        );
        assert_eq!(
            *erx.borrow(),
            1,
            "a down-edge is not an up-edge — the transport epoch stays at 1"
        );

        // ── Phase 3: recovery — WITHOUT manual intervention. The drop-watcher's
        // `Dropped` kick (raised in the same guarded block as the eviction +
        // down-edge above) re-armed the supervisor's ladder; it autonomously
        // re-dials bob through the registry seam, whose swap raises a FRESH
        // up-edge — the exact same-zid case the accumulating seen-zid gate could
        // never re-fire. Observing Disconnected already proved the eviction (the
        // down-edge fires only on a guard-passing removal), so this reinstall is
        // a genuine fresh install, not a same-conn no-op — confirmed by the epoch
        // bump below (a no-op swap would never bump it). ──
        let connected_2 = poll_until(
            || is_connected_direct(&liveness),
            Duration::from_secs(60),
            Duration::from_millis(50),
        )
        .await;
        assert!(
            connected_2,
            "the supervisor's autonomous re-dial must re-register bob Connected(Direct, rtt)"
        );
        // The re-dial's up-edge bumps the epoch 1 → 2 (the transport-level flap
        // re-arm proof). The bump fires synchronously in `swap_zenoh_conn`; poll
        // defensively for scheduling.
        let epoch_re_armed = poll_until(
            || *erx.borrow() == 2,
            Duration::from_secs(15),
            Duration::from_millis(25),
        )
        .await;
        assert!(
            epoch_re_armed,
            "the same-zid flap re-arms the transport epoch (1 → 2), got {}",
            *erx.borrow()
        );
        // The supervisor's own state machine agrees: bob is Connected
        // post-recovery — the full chain (drop → re-dial → reinstall → up-edge →
        // epoch re-arm) closed with zero manual intervention.
        let supervisor_agrees = poll_until(
            || {
                handle
                    .states_snapshot()
                    .iter()
                    .any(|(p, s)| *p == bob_peer && matches!(s, PeerStateWire::Connected { .. }))
            },
            Duration::from_secs(15),
            Duration::from_millis(25),
        )
        .await;
        assert!(
            supervisor_agrees,
            "the supervisor's states_snapshot must report bob Connected post-recovery"
        );

        alice_ep.shutdown().await;
        bob_ep.shutdown().await;
    }
}
