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

/// Default poll deadline for the counter-sign event. Slightly shorter
/// than the dialer's 30s timeout so the response stream tear-down
/// races with the dialer's read-timeout in a deterministic order
/// (acceptor closes first → dialer sees EOF rather than connection-
/// reset).
const ACCEPTOR_POLL_DEADLINE_MS: u64 = 25_000;

/// Poll interval while waiting for the auto-counter-sign to land in
/// `CommunityState`. 20 ms is short enough that the typical
/// counter-sign window (≤ 100 ms after PendingJoin insert) finishes
/// in ≤ 5 polls, and long enough that we don't burn CPU on the engine
/// mutex on the rare "admin offline" path.
const ACCEPTOR_POLL_INTERVAL_MS: u64 = 20;

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
}

impl<H> IrohInviteHandshakeAcceptor<H>
where
    H: AppHandleEmit + Send + Sync + 'static,
{
    pub fn new(
        community_registry: Arc<CommunitySyncRegistry>,
        dm_outbox: Arc<TokioMutex<DmOutbox>>,
        crdt_state: Arc<TokioMutex<OwnerState>>,
        app: Option<Arc<H>>,
    ) -> Self {
        Self {
            community_registry,
            dm_outbox,
            crdt_state,
            app,
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
        // Accept the bi-stream the dialer just opened. The dialer
        // writes-then-finish()es on the send half, so accept_bi() must
        // be the very first await after connection acceptance — any
        // delay risks the dialer's stream sitting in the QUIC receive
        // window with no consumer.
        let (mut send, mut recv) = conn
            .accept_bi()
            .await
            .map_err(|e| HandshakeAcceptError::AcceptBi(e.to_string()))?;

        // Read [u32 LE length-prefix][packet].
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf)
            .await
            .map_err(|e| HandshakeAcceptError::ReadPrefix(e.to_string()))?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > HANDSHAKE_MAX_PACKET_LEN {
            return Err(HandshakeAcceptError::PrefixOutOfBounds {
                len,
                max: HANDSHAKE_MAX_PACKET_LEN,
            });
        }
        let mut packet_bytes = vec![0u8; len];
        recv.read_exact(&mut packet_bytes)
            .await
            .map_err(|e| HandshakeAcceptError::ReadPacket(e.to_string()))?;

        // Peek-decode the request to extract the bootstrap_join.id we
        // need to filter the JoinCountersign poll on. decode_packet is
        // pure (no engine I/O); the authoritative verify happens inside
        // handle_unicast below.
        let packet = community_invite::decode_packet(&packet_bytes)
            .map_err(|e| HandshakeAcceptError::Decode(e.to_string()))?;
        let community_invite::CommunityInvitePacket::Invite { signed, .. } = packet;
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
        let state_arc = self
            .community_registry
            .state_for(&community_id)
            .await
            .ok_or(HandshakeAcceptError::CommunityNotFound { community_id })?;
        let deadline = Instant::now() + Duration::from_millis(ACCEPTOR_POLL_DEADLINE_MS);
        let countersign = loop {
            let found: Option<SignedMembershipEvent> = {
                let g = state_arc.lock().await;
                g.events
                    .values()
                    .find(|e| {
                        matches!(
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
                    deadline_ms: ACCEPTOR_POLL_DEADLINE_MS,
                });
            }
            tokio::time::sleep(Duration::from_millis(ACCEPTOR_POLL_INTERVAL_MS)).await;
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
        send.write_all(&response_len.to_le_bytes())
            .await
            .map_err(|e| HandshakeAcceptError::WritePrefix(e.to_string()))?;
        send.write_all(&response_bytes)
            .await
            .map_err(|e| HandshakeAcceptError::WriteResponse(e.to_string()))?;
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
        // Connection is dropped at the end of this scope; iroh
        // transparently tears it down. The bi-stream send half was
        // already finish()ed (or errored), and the recv half saw EOF
        // when the dialer finished its send half.
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
}
