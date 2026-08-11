//! ZEB-325 Phase 2c (option A): inbound iroh bi-stream handshake
//! acceptor for the `harmony/handshake/v1` ALPN.
//!
//! Wire protocol (mirrors `connectivity_redeem_invite_iroh_inner` on
//! the dialing side):
//!
//! 1. Bob (joiner) opens an iroh bi-stream on `alpn::HARMONY_HANDSHAKE_V1`
//!    and writes `[u32 LE length-prefix][CommunityInviteSigned packet bytes]`
//!    (the same `0x10` discriminant `community_invite::encode_packet`
//!    produces).
//! 2. Alice (this side) decodes the packet to recover
//!    `signed.join_event.id`, registers a pending-redemption oneshot
//!    keyed on that event id, then delegates to
//!    `community_invite::handle_unicast` — which verifies + inserts the
//!    `PendingJoin` into her engine, triggering the existing auto-
//!    counter-sign (ZEB-254 Task 10) post-Inserted hook.
//! 3. Alice polls her engine state for a `JoinCountersign` whose
//!    `target_event_id` matches the registered key. The auto-counter-
//!    sign helper inserts directly into `CommunityState` and does NOT
//!    fire `notify_pending_redemption_in_map` (see
//!    `community_state_sync::spawn_auto_counter_sign_task`), so the
//!    registered oneshot is for cleanup symmetry only — the
//!    authoritative signal is the poll.
//! 4. Alice canonical-CBOR-encodes the `SignedMembershipEvent` and
//!    writes `[u32 LE length-prefix][cbor bytes]` to the bi-stream's
//!    send half, then `finish().await`s it.
//!
//! Bob's side decodes the response, then calls
//! `redeem_invite_inner_with_overrides` with both `pre_minted`
//! (so the freshly-minted `bootstrap_join.id` it sent matches what
//! Alice counter-signed) and `pre_delivered_countersign` (so the
//! engine's post-Inserted hook fires the oneshot the inner registers).
//!
//! ## Why this is not folded into `IrohZenohLinkManager`'s accept loop
//!
//! The link manager's accept loop is consumed by Zenoh transport
//! plumbing — `LinkManagerUnicastTrait`'s `new_link` is its primary
//! interface. Sharing the iroh `Endpoint::accept` queue requires
//! dispatching on negotiated ALPN; we extend the link manager with a
//! `with_handshake_dispatcher` builder that plumbs an
//! `Arc<dyn IrohHandshakeDispatcher>` into the accept loop, and the
//! production wiring in `lib.rs` constructs an
//! `IrohInviteHandshakeAcceptor` that implements the trait against
//! NodeState's `community_registry` / `dm_outbox` / `crdt_state` /
//! `app` handles.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use iroh::endpoint::Connection;
use serde::Serialize;
use tokio::sync::Mutex as TokioMutex;

use crate::community_invite::{self, AppHandleEmit, OpenJoinResponse};
use crate::community_membership::{EventId, MembershipEventKind, SignedMembershipEvent};
use crate::community_state_sync::CommunitySyncRegistry;
use crate::dm_outbox::DmOutbox;
use crate::open_join_admit::{
    OpenJoinConnLimiter, OpenJoinRateLimiter, OPEN_JOIN_FRESHNESS_WINDOW_MS,
};
use crate::owner_state_crdt::OwnerState;

/// Default per-await IO deadline for the inbound handshake. Each
/// `accept_bi` / `read_exact` / `write_all` / `send.finish()` /
/// `conn.closed()` call is wrapped in `tokio::time::timeout` with this
/// duration so a peer that completes the ALPN handshake but then
/// stalls indefinitely on the wire cannot leak a per-connection task.
///
/// 30s is far longer than any legitimate inbound packet roundtrip on
/// loopback or WAN; chosen to be larger than the dialer's default 30s
/// `HARMONY_INVITE_HANDSHAKE_TIMEOUT_MS` so the dialer's read-side
/// timeout usually fires first under normal failure modes.
pub const DEFAULT_ACCEPTOR_IO_DEADLINE_MS: u64 = 30_000;

/// Default poll deadline for the counter-sign event. Slightly shorter
/// than the dialer's 30s timeout so the response stream tear-down
/// races with the dialer's read-timeout in a deterministic order
/// (acceptor closes first → dialer sees EOF rather than connection-
/// reset).
pub const DEFAULT_ACCEPTOR_POLL_DEADLINE_MS: u64 = 25_000;

/// Default poll interval while waiting for the auto-counter-sign to
/// land in `CommunityState`. 20 ms is short enough that the typical
/// counter-sign window (≤ 100 ms after PendingJoin insert) finishes
/// in ≤ 5 polls, and long enough that we don't burn CPU on the engine
/// mutex on the rare "admin offline" path.
pub const DEFAULT_ACCEPTOR_POLL_INTERVAL_MS: u64 = 20;

/// Tunable timeouts for the inbound handshake handler. Tests can
/// construct this directly (sub-second values to keep the suite
/// fast); production reads the env-var overrides described on
/// [`HandshakeAcceptorConfig::from_env`] and falls back to the
/// `DEFAULT_ACCEPTOR_*` constants.
#[derive(Debug, Clone, Copy)]
pub struct HandshakeAcceptorConfig {
    /// Per-await IO timeout: bounds `accept_bi`, both `read_exact`
    /// calls, both `write_all` calls, `send.finish()`, and
    /// `conn.closed()`.
    pub io_deadline: Duration,
    /// Maximum time to wait for the JoinCountersign to land in
    /// `CommunityState` after `handle_unicast` inserts the
    /// `PendingJoin` (which triggers the auto-counter-sign post-
    /// Inserted hook).
    pub poll_deadline: Duration,
    /// Sleep between polls of the engine state for the JoinCountersign.
    pub poll_interval: Duration,
}

impl Default for HandshakeAcceptorConfig {
    fn default() -> Self {
        Self {
            io_deadline: Duration::from_millis(DEFAULT_ACCEPTOR_IO_DEADLINE_MS),
            poll_deadline: Duration::from_millis(DEFAULT_ACCEPTOR_POLL_DEADLINE_MS),
            poll_interval: Duration::from_millis(DEFAULT_ACCEPTOR_POLL_INTERVAL_MS),
        }
    }
}

impl HandshakeAcceptorConfig {
    /// Production constructor: reads optional env overrides
    /// `HARMONY_INVITE_HANDSHAKE_ACCEPTOR_IO_DEADLINE_MS`,
    /// `HARMONY_INVITE_HANDSHAKE_ACCEPTOR_POLL_DEADLINE_MS`,
    /// `HARMONY_INVITE_HANDSHAKE_ACCEPTOR_POLL_INTERVAL_MS`. Any
    /// unparseable or unset value falls back to the corresponding
    /// `DEFAULT_ACCEPTOR_*` constant. Tests should construct
    /// `HandshakeAcceptorConfig { .. }` directly instead of mutating
    /// process env (`std::env::set_var` is unsafe in multithreaded
    /// contexts — see ZEB-325 PR #159 round-1 review).
    pub fn from_env() -> Self {
        fn read_ms(name: &str, default_ms: u64) -> Duration {
            // ZEB-325 PR #159 R3: clamp to >= 1ms. A zero from env
            // override would otherwise produce instant `tokio::time::
            // timeout(0, …)` failures + a tight retry loop on the
            // caller. The minimum is intentionally tiny rather than
            // capped at a sane floor (e.g. 100ms) so tests can still
            // force fast timeouts; production operators are expected
            // to set reasonable values.
            let ms = std::env::var(name)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(default_ms)
                .max(1);
            Duration::from_millis(ms)
        }
        Self {
            io_deadline: read_ms(
                "HARMONY_INVITE_HANDSHAKE_ACCEPTOR_IO_DEADLINE_MS",
                DEFAULT_ACCEPTOR_IO_DEADLINE_MS,
            ),
            poll_deadline: read_ms(
                "HARMONY_INVITE_HANDSHAKE_ACCEPTOR_POLL_DEADLINE_MS",
                DEFAULT_ACCEPTOR_POLL_DEADLINE_MS,
            ),
            poll_interval: read_ms(
                "HARMONY_INVITE_HANDSHAKE_ACCEPTOR_POLL_INTERVAL_MS",
                DEFAULT_ACCEPTOR_POLL_INTERVAL_MS,
            ),
        }
    }
}

/// Pluggable dispatcher invoked by `IrohZenohLinkManager`'s accept
/// loop when an inbound connection negotiates an ALPN other than
/// `harmony/zenoh/v1`. The link manager passes the accepted
/// `Connection` directly — implementations are responsible for opening
/// any bi-streams and consuming the connection.
#[async_trait]
pub trait IrohHandshakeDispatcher: Send + Sync + 'static {
    /// Called once per inbound connection that survives the ALPN
    /// filter. Implementations may run synchronously or spawn a task;
    /// the accept loop awaits this call. Errors are not propagated —
    /// implementations should log and return.
    async fn handle_connection(&self, conn: Connection);
}

/// Maximum bytes the acceptor accepts per request packet. The wire
/// shape is `[u32 LE length-prefix][packet]`; we reject any prefix
/// that exceeds this to defend against memory-exhaustion against an
/// adversarial dialer. 256 KiB is far larger than any legitimate
/// CommunityInviteSigned packet (the snapshot, sealed epoch key, and
/// invite token together fit in single-digit KB) and small enough to
/// stay safe on memory-pressured devices.
pub const HANDSHAKE_MAX_PACKET_LEN: usize = 256 * 1024;

/// Production dispatcher: wires `handle_connection` into the existing
/// `community_invite::handle_unicast` path against NodeState handles.
///
/// Generic over the `AppHandleEmit` impl so tests can stub with
/// `()`/`None`-emit semantics while production uses `tauri::AppHandle`.
pub struct IrohInviteHandshakeAcceptor<H>
where
    H: AppHandleEmit + Send + Sync + 'static,
{
    community_registry: Arc<CommunitySyncRegistry>,
    dm_outbox: Arc<TokioMutex<DmOutbox>>,
    crdt_state: Arc<TokioMutex<OwnerState>>,
    /// `Some(app)` enables `community-state-sync-degraded` Tauri event
    /// emission via `community_invite::handle_unicast`'s `emit_degraded`
    /// path; `None` falls through to the warn-log-only branch (matches
    /// the test-stub convention).
    app: Option<Arc<H>>,
    /// ZEB-367 / ZEB-874: case-A pkarr publisher handle. When `Some`, the
    /// invite's case-A publication is unregistered (freeing the DHT slot) once
    /// the countersign response has been successfully written back to the
    /// joiner — see `handle_invite_handshake_inbound`. ZEB-874 moved this burn
    /// off `handle_unicast`'s local insert so a failed delivery no longer
    /// consumes the single-use invite. `None` in tests.
    pkarr_invite_publisher: Option<Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
    /// Per-handler timeouts. Production wiring constructs this via
    /// [`HandshakeAcceptorConfig::from_env`] so operators can override
    /// without recompiling; tests construct directly to keep wall-clock
    /// short. `Default::default()` uses the `DEFAULT_ACCEPTOR_*`
    /// constants.
    config: HandshakeAcceptorConfig,
    /// Per-acceptor rate limiter + nonce-replay cache shared across all
    /// inbound open-join requests this node serves. Guarded by a
    /// `TokioMutex` because [`OpenJoinRateLimiter::allow`]/`is_replay`
    /// mutate it and the dispatcher may handle concurrent connections;
    /// the lock is held only across the synchronous
    /// `verify_and_admit_open_join` call, never across the engine apply
    /// or the response write.
    open_join_limiter: TokioMutex<OpenJoinRateLimiter>,
    /// ZEB-853 (B7, Half 1): pre-auth Tier-1 connection shield keyed on the
    /// un-spoofable transport `remote_id`. Gates `handle_invite_handshake_inbound`
    /// right after `accept_bi` — BEFORE any stream read / decode / crypto — so a
    /// flooding source can't force unbounded pre-consent ed25519 verification
    /// NOR exhaust the shared per-source admission budget by opening connections.
    /// Mirrors the friend/v1 acceptor's `rate_limiter` (ZEB-700). Interior
    /// `std::sync::Mutex`, never held across an `.await`. `with_config` seeds the
    /// production caps; the shed behavior is unit-tested against
    /// `OpenJoinConnLimiter` directly via `with_caps` (see `open_join_admit`).
    open_join_conn_limiter: OpenJoinConnLimiter,
    /// ZEB-804: per-peer served-traffic registry, keyed by the FULL 32-byte
    /// iroh endpoint id. `None` (tests, pre-boot) — stamping is then a no-op.
    traffic: Option<Arc<crate::network_health::PeerTrafficRegistry>>,
}

impl<H> IrohInviteHandshakeAcceptor<H>
where
    H: AppHandleEmit + Send + Sync + 'static,
{
    /// Build an acceptor with the default timeouts. Equivalent to
    /// `with_config(.., HandshakeAcceptorConfig::default())`.
    pub fn new(
        community_registry: Arc<CommunitySyncRegistry>,
        dm_outbox: Arc<TokioMutex<DmOutbox>>,
        crdt_state: Arc<TokioMutex<OwnerState>>,
        app: Option<Arc<H>>,
        pkarr_invite_publisher: Option<Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
    ) -> Self {
        Self::with_config(
            community_registry,
            dm_outbox,
            crdt_state,
            app,
            pkarr_invite_publisher,
            HandshakeAcceptorConfig::default(),
        )
    }

    /// Build an acceptor with explicit timeouts. Production wiring
    /// passes `HandshakeAcceptorConfig::from_env()`; tests pass a
    /// short-deadline config to keep the suite fast and to assert on
    /// `HandshakeAcceptError::IoTimeout` without races.
    pub fn with_config(
        community_registry: Arc<CommunitySyncRegistry>,
        dm_outbox: Arc<TokioMutex<DmOutbox>>,
        crdt_state: Arc<TokioMutex<OwnerState>>,
        app: Option<Arc<H>>,
        pkarr_invite_publisher: Option<Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
        config: HandshakeAcceptorConfig,
    ) -> Self {
        Self {
            community_registry,
            dm_outbox,
            crdt_state,
            app,
            pkarr_invite_publisher,
            config,
            open_join_limiter: TokioMutex::new(OpenJoinRateLimiter::new()),
            // ZEB-853 (B7): production Tier-1 caps by default (like the friend
            // acceptor's `rate_limiter`); the shield is therefore ON in
            // production with no start_node wiring change. The shed behavior is
            // unit-tested against `OpenJoinConnLimiter` directly (`with_caps`).
            // Honest single-join peers never hit them.
            open_join_conn_limiter: OpenJoinConnLimiter::new(),
            traffic: None,
        }
    }

    /// ZEB-804: install the per-peer served-traffic registry. Builder-style so
    /// the boot wiring can attach it without a second constructor arity
    /// (mirrors `IrohCommunityRelayPullAcceptor::with_traffic_registry`).
    pub fn with_traffic_registry(
        mut self,
        traffic: Arc<crate::network_health::PeerTrafficRegistry>,
    ) -> Self {
        self.traffic = Some(traffic);
        self
    }

    /// ZEB-864: override the pre-auth connection shield's limiter. Production
    /// wiring never calls this (the default `OpenJoinConnLimiter::new()` carries
    /// production caps); the acceptor-harness shed test injects a zero-cap
    /// limiter to force a deterministic shed. Builder-style so it composes with
    /// the other `with_*` seams.
    pub fn with_open_join_conn_limiter(mut self, limiter: OpenJoinConnLimiter) -> Self {
        self.open_join_conn_limiter = limiter;
        self
    }

    /// Inbound bi-stream handler shared by the trait dispatch and the
    /// integration-test direct-drive helper. Reads the length-prefixed
    /// request packet, runs `handle_unicast`, polls for the auto-
    /// counter-sign, and writes the length-prefixed response.
    ///
    /// Returns the bootstrap_join.id from the inbound packet so callers
    /// (tests) can assert on the registry's pending_redemptions state
    /// after dispatch. Errors surface as `Err` with a description; the
    /// caller is responsible for logging.
    pub async fn handle_invite_handshake_inbound(
        &self,
        conn: &Connection,
    ) -> Result<EventId, HandshakeAcceptError> {
        // Snapshot self_owner from dm_outbox up-front so the
        // JoinCountersign poll below can filter on the local owner's
        // signature (ZEB-325 PR #159 F5: without this, the poll picks
        // an arbitrary JoinCountersign for the same target_event_id,
        // which becomes nondeterministic once a second already-joined
        // member also countersigns the same pending join).
        let self_owner = {
            let outbox_g = self.dm_outbox.lock().await;
            outbox_g.self_owner
        };

        // Accept the bi-stream the dialer just opened. The dialer
        // writes-then-finish()es on the send half, so accept_bi() must
        // be the very first await after connection acceptance — any
        // delay risks the dialer's stream sitting in the QUIC receive
        // window with no consumer.
        //
        // ZEB-325 PR #159 F2/F4: wrap every await in tokio::time::timeout
        // bounded by config.io_deadline so a stalled peer can't leak the
        // per-connection task indefinitely.
        let (mut send, mut recv) = tokio::time::timeout(self.config.io_deadline, conn.accept_bi())
            .await
            .map_err(|_| HandshakeAcceptError::IoTimeout {
                step: "accept_bi",
                deadline_ms: self.config.io_deadline.as_millis() as u64,
            })?
            .map_err(|e| HandshakeAcceptError::AcceptBi(e.to_string()))?;

        // ZEB-853 (B7, Half 1) — pre-auth Tier-1 connection shield. Shed a
        // flooding endpoint BEFORE any stream read, decode, or crypto — so a
        // caller holding only the PUBLIC open-invite link can't force unbounded
        // pre-consent ed25519 verification (cert-verify + envelope-sig inside
        // `verify_and_admit_open_join`) simply by opening connections, and can't
        // exhaust the shared per-source admission budget on the far side of it.
        // Keyed on the connecting endpoint's authenticated `remote_id` —
        // un-spoofable. This gate sits above the `0x10` invite / `0x11`
        // open-join split, so it shields BOTH arms.
        //
        // RESPONSE-BYTE EQUIVALENCE: the gate runs pre-decode, so the packet
        // type isn't yet known and there is no single typed reply that fits both
        // arms. On a shed we LOG ("no silent truncation") and write NOTHING —
        // returning `ConnectionShed`, which `handle_connection` treats exactly
        // like the acceptor's existing benign no-response outcomes
        // (`CountersignTimeout`, `CommunityNotFound`): warn-log + `conn.closed()`,
        // ZERO response bytes. A shed therefore emits the SAME zero response
        // bytes as those benign outcomes — no rejection-CONTENT oracle — and it
        // leaks nothing, with ZERO state effect (nothing read, decoded, verified,
        // or applied).
        //
        // NOT timing-indistinguishable: this is the fast path — the shed returns
        // IMMEDIATELY (pre-decode), whereas `CountersignTimeout` only returns
        // after its deadline elapses, so the two remain separable by RESPONSE
        // LATENCY. We deliberately do NOT attempt uniform-timing here; closing
        // that timing side-channel is out of scope for this shield and tracked
        // separately.
        //
        // An honest peer that trips the cap self-heals by re-dialing after the
        // window. ZEB-711: the limiter timeline is the limiter's OWN monotonic
        // clock, never wall time (a wall step would distort the window).
        let limiter_now_ms = self.open_join_conn_limiter.monotonic_now_ms();
        if let Err(reason) = self
            .open_join_conn_limiter
            .admit_connection(*conn.remote_id().as_bytes(), limiter_now_ms)
        {
            tracing::warn!(
                reason,
                remote_id = ?conn.remote_id(),
                "ZEB-853: open-join/invite handshake shed by pre-auth connection shield"
            );
            return Err(HandshakeAcceptError::ConnectionShed);
        }

        // Read [u32 LE length-prefix][packet].
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut len_buf))
            .await
            .map_err(|_| HandshakeAcceptError::IoTimeout {
                step: "read length-prefix",
                deadline_ms: self.config.io_deadline.as_millis() as u64,
            })?
            .map_err(|e| HandshakeAcceptError::ReadPrefix(e.to_string()))?;
        let len = crate::iroh_framing::decode_len_prefix(
            len_buf,
            HANDSHAKE_MAX_PACKET_LEN,
            crate::iroh_framing::Endian::Le,
            false,
        )
        .map_err(|e| HandshakeAcceptError::PrefixOutOfBounds {
            len: e.len,
            max: e.max,
        })?;
        let mut packet_bytes = vec![0u8; len];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut packet_bytes))
            .await
            .map_err(|_| HandshakeAcceptError::IoTimeout {
                step: "read packet body",
                deadline_ms: self.config.io_deadline.as_millis() as u64,
            })?
            .map_err(|e| HandshakeAcceptError::ReadPacket(e.to_string()))?;

        // Peek-decode the request to extract the bootstrap_join.id we
        // need to filter the JoinCountersign poll on. decode_packet is
        // pure (no engine I/O); the authoritative verify happens inside
        // handle_unicast below.
        let packet = community_invite::decode_packet(&packet_bytes)
            .map_err(|e| HandshakeAcceptError::Decode(e.to_string()))?;
        let signed = match packet {
            community_invite::CommunityInvitePacket::Invite { signed, .. } => signed,
            // Open-join packets (0x11) take a separate verify+admit path
            // (no invite token; capability is the `epoch_auth` MAC). It
            // owns the rest of the bi-stream — it writes the typed
            // `OpenJoinResponse` and returns directly.
            community_invite::CommunityInvitePacket::OpenJoin {
                req,
                signature,
                signed_bytes,
            } => {
                return self
                    .handle_open_join_inbound(conn, send, req, signature, signed_bytes)
                    .await;
            }
        };
        let bootstrap_join_id = signed.join_event.id;
        let community_id = signed.community_id;

        // Run the existing receive-side verify + insert pipeline. This
        // verifies envelope sig, decodes the inner PendingJoin, runs
        // the verify_packet_pure chain, and inserts PendingJoin into
        // the engine — which fires the auto-counter-sign post-Inserted
        // hook (ZEB-254 Task 10).
        let unicast_result = community_invite::handle_unicast(
            &self.community_registry,
            &self.dm_outbox,
            &self.crdt_state,
            packet_bytes,
            self.app.as_deref(),
        )
        .await;
        if let Err(e) = unicast_result {
            return Err(HandshakeAcceptError::HandleUnicast(format!("{e:?}")));
        }

        // Poll for the JoinCountersign landing in CommunityState. The
        // auto-counter-sign helper inserts via `state.insert_event`
        // directly (bypasses the post-Inserted hook), so we cannot
        // wait on the pending_redemptions oneshot; the engine's state
        // is the canonical signal.
        //
        // ZEB-325 PR #159 F5: filter on `actor == self_owner` so a
        // second member also countersigning the same pending join can't
        // race us into responding with their signature (map-iteration
        // order is unspecified).
        let state_arc = self
            .community_registry
            .state_for(&community_id)
            .await
            .ok_or(HandshakeAcceptError::CommunityNotFound { community_id })?;
        let deadline = Instant::now() + self.config.poll_deadline;
        let countersign = loop {
            // ZEB-874: check the deadline BEFORE scanning so a zero/elapsed
            // poll_deadline times out deterministically without a racy "free"
            // scan (a zero budget means do-not-wait). Production uses a 10s
            // deadline, so this ordering is a no-op there; it makes the
            // `invite_not_burned_when_handshake_fails_after_insert` negative test
            // a deterministic post-insert timeout rather than a scheduler race
            // against the async auto-counter-sign task.
            if Instant::now() >= deadline {
                return Err(HandshakeAcceptError::CountersignTimeout {
                    target_event_id: bootstrap_join_id,
                    deadline_ms: self.config.poll_deadline.as_millis() as u64,
                });
            }
            let found: Option<SignedMembershipEvent> = {
                let g = state_arc.lock().await;
                let matched = g
                    .events()
                    .find(|e| {
                        e.actor == self_owner
                            && matches!(
                                &e.kind,
                                MembershipEventKind::JoinCountersign { target_event_id }
                                if *target_event_id == bootstrap_join_id
                            )
                    })
                    .cloned();
                drop(g);
                matched
            };
            if let Some(cs) = found {
                break cs;
            }
            tokio::time::sleep(self.config.poll_interval).await;
        };

        // ZEB-911: a WITNESS's countersign is unverifiable on the joiner's
        // fresh CRDT (only the admin is known there) unless the response also
        // carries this node's own admission chain — its cert-carrying
        // PendingJoin plus the countersigns that ratified it, recursively.
        // The admin's chain is empty by construction, so the admin path keeps
        // the legacy single-event response byte-for-byte; a witness responds
        // with a CBOR array [chain ..., countersign], which the joiner
        // distinguishes from the legacy map by major type.
        let admission_chain = {
            let engine_arc = self
                .community_registry
                .engine_arc(&community_id)
                .await
                .ok_or(HandshakeAcceptError::CommunityNotFound { community_id })?;
            let admin_addr = engine_arc.admin_addr();
            let g = state_arc.lock().await;
            let events: Vec<SignedMembershipEvent> = g.events().cloned().collect();
            drop(g);
            crate::community_membership::admission_chain_for(&events, self_owner, admin_addr)
        };

        // Write [u32 LE length-prefix][canonical CBOR of the
        // SignedMembershipEvent, or of the chain array] then finish(). We use
        // the shared CBOR-writer helper: SignedMembershipEvent already has a
        // stable canonical form (it's signed; the wire bytes must be
        // reproducible across peers — Bob's engine receives these bytes and
        // re-verifies them at insert).
        if admission_chain.is_empty() {
            self.write_len_prefixed_cbor(&mut send, &countersign)
                .await?;
        } else {
            let mut response: Vec<SignedMembershipEvent> = admission_chain;
            response.push(countersign);
            // Encode ONCE and measure (the open-join snapshot pattern below): a
            // deep countersigner graph of cert-carrying events can exceed the
            // handshake cap, and `write_len_prefixed_cbor`'s ResponseTooLarge
            // would fire AFTER the PendingJoin insert — every retry then hits
            // the same oversize error while the invite stays live. Degrade to
            // the legacy single-countersign shape instead: the joiner cannot
            // verify it chain-less, but its redeem parks at the honest ZEB-254
            // `pending: true` fallback and converges over Zenoh.
            let mut resp_bytes = Vec::new();
            ciborium::into_writer(&response, &mut resp_bytes)
                .map_err(|e| HandshakeAcceptError::EncodeResponse(e.to_string()))?;
            if resp_bytes.len() > HANDSHAKE_MAX_PACKET_LEN {
                tracing::warn!(
                    encoded_len = resp_bytes.len(),
                    max = HANDSHAKE_MAX_PACKET_LEN,
                    chain_events = response.len() - 1,
                    "ZEB-911: admission-chain response exceeds handshake cap; \
                     sending countersign alone (joiner degrades to pending + Zenoh)"
                );
                let countersign_only = response.pop().expect("countersign was just pushed");
                self.write_len_prefixed_cbor(&mut send, &countersign_only)
                    .await?;
            } else {
                self.write_len_prefixed_bytes(&mut send, &resp_bytes)
                    .await?;
            }
        }

        // ZEB-874: burn the single-use invite ONLY now that the countersign
        // response has been handed to the transport. The `?` above means any
        // delivery failure (CountersignTimeout returns earlier; a failed
        // write / io-timeout / lost connection returns here) leaves the invite
        // live for the joiner to retry. Fires on both Inserted and
        // AlreadyKnown (a retransmit re-delivers the countersign); the
        // unregister is idempotent, so a repeat burn is a safe no-op. Moved
        // here from `community_invite::handle_unicast`, which used to burn on
        // local insert before delivery was known.
        if let Some(pubr) = self.pkarr_invite_publisher.as_ref() {
            pubr.unregister_invite(&signed.invite_token.sig).await;
        }

        Ok(bootstrap_join_id)
    }

    /// Encode `value` as CBOR and write `[u32 LE length-prefix][cbor bytes]`
    /// to `send`, then `finish()`. Shared by the invite countersign response
    /// and the open-join `OpenJoinResponse` write. Each await is bounded by
    /// `config.io_deadline`; oversized responses surface as
    /// `ResponseTooLarge` rather than being written.
    async fn write_len_prefixed_cbor<T: Serialize>(
        &self,
        send: &mut iroh::endpoint::SendStream,
        value: &T,
    ) -> Result<(), HandshakeAcceptError> {
        let mut response_bytes = Vec::new();
        ciborium::into_writer(value, &mut response_bytes)
            .map_err(|e| HandshakeAcceptError::EncodeResponse(e.to_string()))?;
        self.write_len_prefixed_bytes(send, &response_bytes).await
    }

    /// Write already-CBOR-encoded `response_bytes` with a `[u32 LE length-prefix]`
    /// frame, each I/O bounded by `config.io_deadline`; oversized responses
    /// surface as `ResponseTooLarge` rather than being written. Split out from
    /// `write_len_prefixed_cbor` so a caller that must MEASURE the encoded size
    /// first (the open-join snapshot cap guard) can encode ONCE and hand the
    /// buffer straight here instead of serializing the same (potentially large)
    /// value twice.
    async fn write_len_prefixed_bytes(
        &self,
        send: &mut iroh::endpoint::SendStream,
        response_bytes: &[u8],
    ) -> Result<(), HandshakeAcceptError> {
        let response_prefix = crate::iroh_framing::encode_len_prefix(
            response_bytes.len(),
            HANDSHAKE_MAX_PACKET_LEN,
            crate::iroh_framing::Endian::Le,
            false,
        )
        .map_err(|e| HandshakeAcceptError::ResponseTooLarge {
            len: e.len,
            max: e.max,
        })?;

        tokio::time::timeout(self.config.io_deadline, send.write_all(&response_prefix))
            .await
            .map_err(|_| HandshakeAcceptError::IoTimeout {
                step: "write length-prefix",
                deadline_ms: self.config.io_deadline.as_millis() as u64,
            })?
            .map_err(|e| HandshakeAcceptError::WritePrefix(e.to_string()))?;
        tokio::time::timeout(self.config.io_deadline, send.write_all(response_bytes))
            .await
            .map_err(|_| HandshakeAcceptError::IoTimeout {
                step: "write response body",
                deadline_ms: self.config.io_deadline.as_millis() as u64,
            })?
            .map_err(|e| HandshakeAcceptError::WriteResponse(e.to_string()))?;
        // `send.finish()` is sync — no timeout needed.
        send.finish()
            .map_err(|e| HandshakeAcceptError::Finish(e.to_string()))?;
        Ok(())
    }

    /// Write a typed `OpenJoinResponse::Rejected { reason }` and — once the
    /// write+finish completes — stamp the ZEB-804 served-traffic registry: a
    /// completed typed rejection is still a COMPLETED iroh-authenticated
    /// exchange (review r1 ruling — staleness evidence, not a trust or
    /// success signal). Transport failures propagate via `?` BEFORE the
    /// stamp, so an exchange that never completed never stamps.
    async fn write_open_join_rejection(
        &self,
        conn: &Connection,
        send: &mut iroh::endpoint::SendStream,
        reason: String,
    ) -> Result<(), HandshakeAcceptError> {
        let resp = OpenJoinResponse::Rejected { reason };
        self.write_len_prefixed_cbor(send, &resp).await?;
        if let Some(reg) = self.traffic.as_ref() {
            reg.record_served(
                *conn.remote_id().as_bytes(),
                crate::network_health::now_ms(),
            );
        }
        Ok(())
    }

    /// Inbound handler for a tokenless open-join request (`0x11` packet).
    ///
    /// Snapshots the beacon engine's verification inputs (`epoch_key`,
    /// `admin_addr`, the signed-membership log) under the state lock, then
    /// drops the lock and runs the pure
    /// [`crate::open_join_admit::verify_and_admit_open_join`] gate
    /// (capability, identity, freshness, replay, rate-limit, ban-check, then
    /// open-admission) while holding only the per-acceptor limiter lock. On
    /// admission it applies the joiner's self-signed Join to the engine via
    /// `insert_local_event` — which runs `verify_event` (the open Join is
    /// self-authorizing via its carried EnrollmentCert) and fires the publish
    /// loop so the Join propagates to every member over Zenoh — then writes an
    /// `OpenJoinResponse::Admitted { member_events }` snapshot so the joiner
    /// converges immediately. On rejection it writes a typed
    /// `OpenJoinResponse::Rejected { reason }` and returns the
    /// `OpenJoinRejected` error.
    ///
    /// Returns the admitted Join's `EventId` (mirroring the invite path's
    /// `bootstrap_join_id`) so the trait dispatch can log it.
    async fn handle_open_join_inbound(
        &self,
        conn: &Connection,
        mut send: iroh::endpoint::SendStream,
        req: community_invite::OpenJoinRequest,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
    ) -> Result<EventId, HandshakeAcceptError> {
        let community_id = req.community_id;
        // The engine owns `membership_key()`/`admin_addr()` and the
        // `insert_local_event` apply; its inner `CommunityState` (reached via
        // `engine.state()`) owns the signed-membership `events` log. Fetch the
        // engine once and reuse it for both the snapshot and the apply.
        let engine = self
            .community_registry
            .engine_arc(&community_id)
            .await
            .ok_or(HandshakeAcceptError::CommunityNotFound { community_id })?;
        let epoch_key = engine.membership_key();
        let admin_addr = engine.admin_addr();

        // SECURITY: tokenless open-join is only valid for OPEN communities. An
        // `epoch_key` holder targeting an INVITE-ONLY community must still pass
        // the invite/countersign gate; admitting an open-join here would bypass
        // that gate. Reject BEFORE any admission processing.
        if engine.is_invite_only() {
            tracing::warn!(
                community_id = %hex::encode(community_id.0),
                remote_id = ?conn.remote_id(),
                "open-join rejected: community is invite-only (countersign-gated)"
            );
            self.write_open_join_rejection(conn, &mut send, "community is invite-only".to_string())
                .await?;
            return Err(HandshakeAcceptError::OpenJoinNotPermitted);
        }

        // Snapshot the event log under the inner-state lock, then DROP the
        // lock before the (synchronous) verify+admit and the engine apply.
        let current_events = {
            let g = engine.state();
            let g = g.lock().await;
            g.events().cloned().collect::<Vec<_>>()
        };

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before UNIX_EPOCH")
            .as_millis() as u64;

        // Hold the per-acceptor limiter only across the synchronous gate (it
        // mutates the rate-limit counter + nonce-replay cache). Drop it before
        // the engine apply / response write so a slow peer can't pin it.
        let admit = {
            let mut limiter = self.open_join_limiter.lock().await;
            // ZEB-711: the rate-limit window + nonce horizon key on the limiter's
            // OWN monotonic clock, never the beacon wall clock `now_ms` (which
            // stays for the `created_at` freshness bound only). Bind the
            // immutable borrow before the `&mut limiter` below.
            let limiter_now_ms = limiter.monotonic_now_ms();
            crate::open_join_admit::verify_and_admit_open_join(
                &req,
                &signature,
                &signed_bytes,
                &epoch_key,
                community_id,
                admin_addr,
                &current_events,
                now_ms,
                OPEN_JOIN_FRESHNESS_WINDOW_MS,
                limiter_now_ms,
                // ZEB-853 (B7, Half 2): key the admission budget on the
                // un-spoofable transport `remote_id` so one flooding source
                // can't exhaust every open-joiner's shared budget.
                *conn.remote_id().as_bytes(),
                &mut limiter,
            )
        };

        let admit = match admit {
            Ok(ok) => ok,
            Err(reject) => {
                tracing::warn!(
                    ?reject,
                    community_id = %hex::encode(community_id.0),
                    remote_id = ?conn.remote_id(),
                    "open-join rejected"
                );
                // Surface a typed rejection so the joiner can show a reason.
                self.write_open_join_rejection(conn, &mut send, format!("{reject:?}"))
                    .await?;
                return Err(HandshakeAcceptError::OpenJoinRejected);
            }
        };

        // Apply the admitted Join to the engine. `insert_local_event` runs
        // `verify_event` (the open Join is self-authorizing via its carried
        // EnrollmentCert — actor may be a remote owner, which verify_event
        // resolves from the cert) and notifies the publish loop, so the Join
        // reaches every member over Zenoh. Mirrors the open-redeem arm in
        // `redeem_invite_inner_with_overrides`.
        let join_event_id = req.join_event.id;
        // `insert_local_event` runs `verify_event` and returns
        // `Ok(InsertOutcome::Rejected(v))` for a SEMANTIC rejection (only `Err`
        // is a transport failure). A Rejected insert means the Join did not land
        // in the engine, so we must NOT report `Admitted` (which would tell the
        // joiner it's a member while the beacon never published its Join). Match
        // the outcome and surface a typed rejection on Rejected.
        match engine.insert_local_event(req.join_event).await {
            Ok(crate::community_state_crdt::InsertOutcome::Inserted)
            | Ok(crate::community_state_crdt::InsertOutcome::AlreadyKnown) => {}
            Ok(crate::community_state_crdt::InsertOutcome::Rejected(v)) => {
                tracing::warn!(
                    ?v,
                    community_id = %hex::encode(community_id.0),
                    remote_id = ?conn.remote_id(),
                    "open-join: admitted Join rejected by engine verify_event; not reporting Admitted"
                );
                self.write_open_join_rejection(
                    conn,
                    &mut send,
                    format!("join rejected on apply: {v:?}"),
                )
                .await?;
                return Err(HandshakeAcceptError::OpenJoinRejected);
            }
            Err(e) => {
                return Err(HandshakeAcceptError::HandleUnicast(format!("{e:?}")));
            }
        }

        // Respond with the membership snapshot so the joiner converges fast.
        // For a large-membership community the snapshot can exceed the 256KiB
        // handshake cap, which would make `write_len_prefixed_cbor` fail with
        // `ResponseTooLarge` AFTER admission already succeeded — breaking
        // open-join for big communities. Guard it: if the encoded `Admitted`
        // response would exceed the cap, fall back to an EMPTY snapshot. The
        // joiner already locally inserted its own Join in the open-redeem arm
        // and converges the rest over Zenoh, so an empty snapshot is a valid
        // degraded path (its Task-10 apply loop treats an empty list as a
        // no-op and still returns "joined").
        let resp = OpenJoinResponse::Admitted {
            member_events: admit.member_events_snapshot,
        };
        // Encode the snapshot ONCE. If it fits the cap, hand the buffer straight
        // to the writer (no second serialization of the potentially large
        // member-event Vec); only the rare oversize path re-encodes — and that
        // re-encode is the tiny empty fallback, not the big snapshot.
        let mut resp_bytes = Vec::new();
        ciborium::into_writer(&resp, &mut resp_bytes)
            .map_err(|e| HandshakeAcceptError::EncodeResponse(e.to_string()))?;
        if resp_bytes.len() > HANDSHAKE_MAX_PACKET_LEN {
            tracing::warn!(
                encoded_len = resp_bytes.len(),
                max = HANDSHAKE_MAX_PACKET_LEN,
                community_id = %hex::encode(community_id.0),
                "open-join admitted snapshot exceeds handshake cap; sending empty snapshot \
                 (joiner converges via Zenoh)"
            );
            let empty = OpenJoinResponse::Admitted {
                member_events: Vec::new(),
            };
            self.write_len_prefixed_cbor(&mut send, &empty).await?;
        } else {
            self.write_len_prefixed_bytes(&mut send, &resp_bytes)
                .await?;
        }

        Ok(join_event_id)
    }
}

#[async_trait]
impl<H> IrohHandshakeDispatcher for IrohInviteHandshakeAcceptor<H>
where
    H: AppHandleEmit + Send + Sync + 'static,
{
    async fn handle_connection(&self, conn: Connection) {
        match self.handle_invite_handshake_inbound(&conn).await {
            Ok(bootstrap_join_id) => {
                // ZEB-804: stamp the FULL-id served-traffic registry once per
                // completed handshake (invite counter-sign or open-join admit —
                // both wrote a response to an iroh-authenticated peer).
                if let Some(reg) = self.traffic.as_ref() {
                    reg.record_served(
                        *conn.remote_id().as_bytes(),
                        crate::network_health::now_ms(),
                    );
                }
                tracing::info!(
                    bootstrap_join_id = %hex::encode(bootstrap_join_id),
                    remote_id = ?conn.remote_id(),
                    "ZEB-325 Phase 2c: invite handshake completed (counter-sign delivered)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    remote_id = ?conn.remote_id(),
                    "ZEB-325 Phase 2c: invite handshake failed"
                );
            }
        }
        // CRITICAL: wait for the dialer to drive the connection close
        // before letting the `conn` Arc drop. Without this, dropping
        // the Connection here races the QUIC layer's in-flight delivery
        // of the response bytes — Bob sees `read length-prefix:
        // connection lost` despite our `send.finish()` flushing
        // locally. Pattern lifted verbatim from
        // `zenoh_iroh_link::IrohZenohLink::tests::paired_stream_roundtrip_via_loopback`,
        // which observed an identical 6-hour symptom during Phase 1
        // Task 5 (2026-05-22).
        //
        // ZEB-325 PR #159 F2/F4: bound conn.closed() by the same
        // io_deadline used for the per-stream awaits above so a peer
        // that successfully reads the response but never tears the
        // connection down (intentionally or otherwise) can't pin our
        // task forever. Timeout here is best-effort tear-down: we
        // already finished writing the response, so a late close is
        // a leak-of-task concern, not a correctness one.
        let _ = tokio::time::timeout(self.config.io_deadline, conn.closed()).await;
    }
}

/// Errors that can short-circuit the inbound handshake. All variants
/// are logged at warn-level by the trait dispatch; tests can match on
/// the variant to assert on specific failure modes.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeAcceptError {
    #[error("accept_bi failed: {0}")]
    AcceptBi(String),
    #[error("read length-prefix: {0}")]
    ReadPrefix(String),
    #[error("length-prefix out of bounds: len={len} max={max}")]
    PrefixOutOfBounds { len: usize, max: usize },
    #[error("read packet body: {0}")]
    ReadPacket(String),
    #[error("decode_packet: {0}")]
    Decode(String),
    #[error("handle_unicast: {0}")]
    HandleUnicast(String),
    #[error("community not found in registry for incoming request: {community_id:?}")]
    CommunityNotFound {
        community_id: crate::owner_state_types::SpaceId,
    },
    /// ZEB-853 (B7, Half 1): the inbound connection was shed by the pre-auth
    /// Tier-1 connection shield BEFORE any stream read / decode / crypto. No
    /// response was written — the SAME zero response bytes as the benign
    /// no-response outcomes (`CountersignTimeout` / `CommunityNotFound`), so
    /// there is no rejection-CONTENT oracle. It is NOT timing-indistinguishable,
    /// though: the shed is the fast path (returns immediately, pre-decode) while
    /// `CountersignTimeout` returns only after its deadline — uniform-timing is
    /// out of scope for this shield and tracked separately. The trait dispatch
    /// just warn-logs this and closes the connection.
    #[error("connection shed by pre-auth Tier-1 connection shield")]
    ConnectionShed,
    /// An inbound open-join (`0x11`) request failed
    /// [`crate::open_join_admit::verify_and_admit_open_join`]. The typed
    /// `OpenJoinResponse::Rejected` was already written to the dialer; this
    /// variant just records the failure for the trait dispatch's warn log.
    #[error("open-join rejected")]
    OpenJoinRejected,
    /// A tokenless open-join (`0x11`) request targeted an INVITE-ONLY community.
    /// Open-join is only valid for open communities; an `epoch_key` holder must
    /// still pass the invite/countersign gate, so this is rejected before any
    /// admission processing. The typed `OpenJoinResponse::Rejected` was already
    /// written to the dialer.
    #[error("open-join not permitted for an invite-only community")]
    OpenJoinNotPermitted,
    #[error(
        "JoinCountersign for target_event_id={target_event_id:?} did not land within {deadline_ms}ms"
    )]
    CountersignTimeout {
        target_event_id: EventId,
        deadline_ms: u64,
    },
    #[error("encode response: {0}")]
    EncodeResponse(String),
    #[error("response too large: len={len} max={max}")]
    ResponseTooLarge { len: usize, max: usize },
    #[error("write length-prefix: {0}")]
    WritePrefix(String),
    #[error("write response body: {0}")]
    WriteResponse(String),
    #[error("send.finish: {0}")]
    Finish(String),
    /// ZEB-325 PR #159 F2/F4: a per-await IO timeout fired before the
    /// QUIC operation completed. `step` identifies which stage stalled
    /// (`accept_bi`, `read length-prefix`, `read packet body`,
    /// `write length-prefix`, `write response body`).
    #[error("IO timeout in {step} after {deadline_ms}ms")]
    IoTimeout {
        step: &'static str,
        deadline_ms: u64,
    },
}

#[cfg(test)]
mod tests {
    use crate::community_invite::{
        build_signed_invite_packet, build_signed_open_join_packet, decode_packet,
        device_hash_from_identity_pub, encode_packet, CommunityInvitePacket, CommunityInviteSigned,
        InviteToken, OpenJoinRequest,
    };
    use crate::community_membership::{
        mint_test_owner, MembershipEventKind, SignedMembershipEvent,
    };
    use crate::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, SpaceId};

    fn sample_join_event() -> SignedMembershipEvent {
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0u8; 16],
            community_id: SpaceId([1u8; 16]),
            kind: MembershipEventKind::Join,
            actor: OwnerAddr([0u8; 16]),
            at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "j".to_string(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: Some(mint_test_owner(0x4A).cert),
        }
    }

    /// `decode_packet` must route a `0x11` wire to the `OpenJoin` variant and a
    /// `0x10` wire to the `Invite` variant. This guards the match arm the
    /// inbound dispatcher branches on (`handle_invite_handshake_inbound`); the
    /// full admit round-trip is proven by the Task 12 integration test.
    #[test]
    fn dispatch_routes_open_join_discriminant() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        // The 0x10 invite decode enforces signing_device_hash ==
        // SHA256(joiner_identity_pub)[..16], so derive the hash honestly for
        // both packets rather than using a placeholder.
        let identity_pub = [4u8; 64];
        let device_hash = DeviceIdentityHash(device_hash_from_identity_pub(&identity_pub));

        // 0x11 open-join wire (Task 4 builder).
        let open_req = OpenJoinRequest {
            community_id: SpaceId([1u8; 16]),
            join_event: sample_join_event(),
            joiner_identity_pub: identity_pub,
            signing_device_hash: device_hash,
            epoch_auth: [9u8; 32],
            nonce: [2u8; 16],
            created_at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "j".to_string(),
            },
        };
        let open_wire =
            encode_packet(&build_signed_open_join_packet(open_req, &sk).expect("build open-join"))
                .expect("encode open-join");

        // 0x10 invite wire (existing builder). decode_packet is signature-
        // agnostic (verify happens later in handle_unicast), so dummy token /
        // sig bytes still decode structurally to the Invite variant.
        let invite_signed = CommunityInviteSigned {
            community_id: SpaceId([1u8; 16]),
            join_event: sample_join_event(),
            invite_token: InviteToken {
                inviter: OwnerAddr([5u8; 16]),
                invitee_hint: None,
                minted_at: Hlc {
                    wall_ms: 900,
                    logical: 0,
                    device_id: "i".to_string(),
                },
                expires_at: None,
                sig: [0u8; 64],
            },
            joiner_identity_pub: identity_pub,
            signing_device_hash: device_hash,
            created_at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "j".to_string(),
            },
        };
        let invite_wire =
            encode_packet(&build_signed_invite_packet(invite_signed, &sk).expect("build invite"))
                .expect("encode invite");

        assert_eq!(open_wire[0], 0x11, "open-join discriminant byte");
        assert_eq!(invite_wire[0], 0x10, "invite discriminant byte");
        assert!(
            matches!(
                decode_packet(&open_wire).expect("decode open-join"),
                CommunityInvitePacket::OpenJoin { .. }
            ),
            "0x11 wire must decode to the OpenJoin variant"
        );
        assert!(
            matches!(
                decode_packet(&invite_wire).expect("decode invite"),
                CommunityInvitePacket::Invite { .. }
            ),
            "0x10 wire must decode to the Invite variant"
        );
    }
}
