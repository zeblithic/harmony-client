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
use tokio::sync::Mutex as TokioMutex;

use crate::community_invite::{self, AppHandleEmit};
use crate::community_membership::{EventId, MembershipEventKind, SignedMembershipEvent};
use crate::community_state_sync::CommunitySyncRegistry;
use crate::dm_outbox::DmOutbox;
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
    /// ZEB-367: case-A pkarr publisher handle. When `Some`, a successful
    /// invite consumption (PendingJoin / counter-signed Join `Inserted`)
    /// unregisters the invite's case-A publication via
    /// `handle_unicast`, freeing the DHT slot. `None` in tests.
    pkarr_invite_publisher: Option<Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
    /// Per-handler timeouts. Production wiring constructs this via
    /// [`HandshakeAcceptorConfig::from_env`] so operators can override
    /// without recompiling; tests construct directly to keep wall-clock
    /// short. `Default::default()` uses the `DEFAULT_ACCEPTOR_*`
    /// constants.
    config: HandshakeAcceptorConfig,
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
        }
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

        // Read [u32 LE length-prefix][packet].
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut len_buf))
            .await
            .map_err(|_| HandshakeAcceptError::IoTimeout {
                step: "read length-prefix",
                deadline_ms: self.config.io_deadline.as_millis() as u64,
            })?
            .map_err(|e| HandshakeAcceptError::ReadPrefix(e.to_string()))?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > HANDSHAKE_MAX_PACKET_LEN {
            return Err(HandshakeAcceptError::PrefixOutOfBounds {
                len,
                max: HANDSHAKE_MAX_PACKET_LEN,
            });
        }
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
        let community_invite::CommunityInvitePacket::Invite { signed, .. } = packet else {
            // Open-join packets (0x11) are handled by the open-join admit
            // dispatcher, not this invite acceptor. Reject here so the
            // peek stays invite-only until that path is wired in.
            return Err(HandshakeAcceptError::Decode(
                "expected an invite packet on the invite-acceptor path".to_string(),
            ));
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
            self.pkarr_invite_publisher.as_ref(),
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
            let found: Option<SignedMembershipEvent> = {
                let g = state_arc.lock().await;
                g.events
                    .values()
                    .find(|e| {
                        e.actor == self_owner
                            && matches!(
                                &e.kind,
                                MembershipEventKind::JoinCountersign { target_event_id }
                                if *target_event_id == bootstrap_join_id
                            )
                    })
                    .cloned()
            };
            if let Some(cs) = found {
                break cs;
            }
            if Instant::now() >= deadline {
                return Err(HandshakeAcceptError::CountersignTimeout {
                    target_event_id: bootstrap_join_id,
                    deadline_ms: self.config.poll_deadline.as_millis() as u64,
                });
            }
            tokio::time::sleep(self.config.poll_interval).await;
        };

        // Encode the response: canonical CBOR of the SignedMembershipEvent.
        // We use ciborium directly rather than canonical_cbor_encode
        // because SignedMembershipEvent already has a stable canonical
        // form (it's signed; the wire bytes must be reproducible across
        // peers — Bob's engine will receive these bytes and call
        // insert_local_event_with_pubs against them).
        let mut response_bytes = Vec::new();
        ciborium::into_writer(&countersign, &mut response_bytes)
            .map_err(|e| HandshakeAcceptError::EncodeResponse(e.to_string()))?;
        if response_bytes.len() > HANDSHAKE_MAX_PACKET_LEN {
            return Err(HandshakeAcceptError::ResponseTooLarge {
                len: response_bytes.len(),
                max: HANDSHAKE_MAX_PACKET_LEN,
            });
        }
        let response_len = response_bytes.len() as u32;

        // Write [u32 LE length-prefix][cbor bytes] then finish().
        tokio::time::timeout(
            self.config.io_deadline,
            send.write_all(&response_len.to_le_bytes()),
        )
        .await
        .map_err(|_| HandshakeAcceptError::IoTimeout {
            step: "write length-prefix",
            deadline_ms: self.config.io_deadline.as_millis() as u64,
        })?
        .map_err(|e| HandshakeAcceptError::WritePrefix(e.to_string()))?;
        tokio::time::timeout(self.config.io_deadline, send.write_all(&response_bytes))
            .await
            .map_err(|_| HandshakeAcceptError::IoTimeout {
                step: "write response body",
                deadline_ms: self.config.io_deadline.as_millis() as u64,
            })?
            .map_err(|e| HandshakeAcceptError::WriteResponse(e.to_string()))?;
        // `send.finish()` is sync — no timeout needed.
        send.finish()
            .map_err(|e| HandshakeAcceptError::Finish(e.to_string()))?;

        Ok(bootstrap_join_id)
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
