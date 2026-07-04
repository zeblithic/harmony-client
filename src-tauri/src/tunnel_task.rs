//! ZEB-473 (DM-over-iroh, Move 1a): per-connection async driver bridging an
//! iroh QUIC bi-stream to a `harmony_tunnel::TunnelSession`.
//!
//! One task runs per active tunnel connection (inbound responder or outbound
//! initiator). It owns the sans-I/O `TunnelSession` state machine and drives it
//! by:
//!   1. reading length-prefixed frames off the iroh `RecvStream` →
//!      `TunnelEvent::InboundBytes`,
//!   2. servicing a command channel (`TunnelCommand::SendDm` / `Close`) →
//!      `TunnelEvent::SendDm` / `Close`,
//!   3. a ~10s keepalive `interval` → `TunnelEvent::Tick`.
//!
//! Every batch of `TunnelAction`s is applied through a two-pass dispatch:
//! ALL `OutboundBytes` are written to the bi-stream FIRST (so e.g. a
//! `TunnelAccept` is on the wire before `HandshakeComplete` registers the
//! session), THEN `DmReceived` is pushed onto the ingest seam,
//! `HandshakeComplete` flips the manager handle to Active + flushes the pending
//! queue, and `Closed`/`Error` exit the loop.
//!
//! Adapted from harmony-node `crates/harmony-node/src/tunnel_task.rs`. The
//! deliberate divergence from the node template: the initiator dials over the
//! client's PERSISTENT iroh endpoint (`IrohEndpoint::inner()`), not an ephemeral
//! one, so the peer can dial us back by our stable `EndpointId` and the
//! collision-dedup in `TunnelManager` can converge on a single survivor.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use harmony_identity::{PqIdentity, PqPrivateIdentity};
use harmony_tunnel::session::{TunnelSession, TunnelState};
use harmony_tunnel::{TunnelAction, TunnelEvent};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};

use crate::iroh_endpoint::IrohEndpoint;
use crate::owner_state_types::DeviceTunnelContact;
use crate::tunnel_manager::{InboundDm, TunnelCommand, TunnelManager};

/// Maximum time allowed for the handshake phase (stream open/accept +
/// length-prefixed `TunnelInit`/`TunnelAccept` exchange + state-machine
/// creation). The main loop has its own keepalive dead-peer timeout and does
/// not need this.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum message size during the pre-authentication handshake.
///
/// `TunnelInit` is ~6381 bytes (1088 CT + 1952 DSA pk + 32 nonce + 3309 sig);
/// `TunnelAccept` is ~5293 bytes. 8 KiB gives comfortable headroom and caps
/// pre-auth allocation per connection.
const HANDSHAKE_MAX_MESSAGE: usize = 8 * 1024;

/// Maximum message size during the authenticated data phase. A DM packet is a
/// sealed+signed CidNotify (single-digit KB); 2 MiB is a generous cap that
/// bounds per-peer allocation against a misbehaving authenticated peer.
const DATA_MAX_MESSAGE: usize = 2 * 1024 * 1024;

/// Keepalive tick cadence. The `TunnelSession` state machine decides when to
/// actually emit a keepalive frame (jittered 25–35s) and when to declare a dead
/// peer (110s); the 10s tick just gives it responsive timeout detection.
const KEEPALIVE_TICK: Duration = Duration::from_secs(10);

/// Why a tunnel handshake failed (ZEB-623). Splitting the failure lets the
/// caller record a *protocol-incompatible* peer loudly in the
/// [`ProtocolCompatRegistry`](crate::protocol_versioning::ProtocolCompatRegistry)
/// — surfaced in Network Health — while every pre-existing dial/stream/crypto
/// failure stays an opaque `Other` that just drops the attempt (the
/// always-deposit rung covers durability).
pub(crate) enum HandshakeFailure {
    /// The peer's tunnel hello advertised a `protocol_version` below our
    /// minimum. BOTH sides record `reason` in the compat registry, keyed by the
    /// peer's authenticated iroh EndpointId (the key Network Health joins on):
    /// the initiator knows it up front (`addr.id`); the responder reads it from
    /// `conn.remote_id()`. iroh authenticates the remote endpoint key in its TLS
    /// handshake, so this id is trustworthy even before the PQ tunnel handshake
    /// completes — unlike the tunnel NodeId (`blake3(ML-DSA pubkey)`), which is
    /// not authenticated until then.
    Incompatible { reason: String },
    /// Any other handshake failure (connect, stream, decode, crypto, timeout).
    Other(String),
}

impl std::fmt::Display for HandshakeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeFailure::Incompatible { reason } => {
                write!(f, "incompatible protocol: {reason}")
            }
            HandshakeFailure::Other(reason) => write!(f, "{reason}"),
        }
    }
}

/// Run the **responder** side of an inbound tunnel connection.
///
/// `accept_bi()` → read `TunnelInit` → `TunnelSession::new_responder` → write
/// `TunnelAccept` → register the live session into `TunnelManager` (so our
/// outbound DMs to this peer reuse the bidirectional tunnel) → `run_tunnel_loop`.
pub async fn run_tunnel_responder(
    conn: Connection,
    local_pq: Arc<PqPrivateIdentity>,
    mgr: Arc<TunnelManager>,
    ingest_tx: mpsc::Sender<InboundDm>,
) {
    let handshake =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, responder_handshake(&conn, &local_pq)).await;

    let (session, send_stream, recv_stream, peer_node_id) = match handshake {
        Ok(Ok(v)) => v,
        Ok(Err(failure)) => {
            // ZEB-623: record a *protocol-incompatible* inbound peer in the compat
            // registry (surfaced in Network Health) keyed by the peer's iroh
            // EndpointId. iroh authenticates the remote endpoint key in its TLS
            // handshake, so `conn.remote_id()` is trustworthy even though the PQ
            // tunnel handshake never completed — and it is the SAME key the reader
            // in `network_health.rs` joins on (`record.iroh_node_id`). ONLY the
            // Incompatible arm records: `Other` covers transient dial/stream/
            // decode/crypto/timeout failures, which are NOT protocol
            // incompatibility, and a v1-generation inbound carries no hello so it
            // can never produce Incompatible (keeping the N-1 window unflagged).
            if let HandshakeFailure::Incompatible { reason } = &failure {
                let remote_id = conn.remote_id();
                mgr.compat_registry()
                    .note_incompatible(*remote_id.as_bytes(), reason.clone());
            }
            tracing::debug!(%failure, "ZEB-473: inbound tunnel handshake failed");
            // Explicitly close before dropping (mirrors the accept loop's
            // pre-install close) so the peer sees a prompt application close
            // rather than waiting out an idle timeout.
            conn.close(0u32.into(), b"tunnel-handshake-failed");
            return;
        }
        Err(_) => {
            tracing::debug!("ZEB-473: inbound tunnel handshake timed out");
            conn.close(0u32.into(), b"tunnel-handshake-failed");
            return;
        }
    };

    // ZEB-623 round-2: clear any stale incompatibility record for this peer — a
    // successful INBOUND handshake (over v1 or v2) proves we can now speak a
    // compatible protocol (e.g. the peer has since upgraded, or this is a v1
    // N-1 inbound that carries no hello). This mirrors the initiator, which
    // clears on ANY successful handshake. The record is keyed by the peer's
    // IROH EndpointId (`conn.remote_id()` — the Network Health join key), the
    // SAME key the failure arm above records under, so we clear symmetrically;
    // otherwise a previously-flagged peer that reconnects INBOUND stays flagged
    // until we happen to dial it outbound. `conn` is still in scope here (the
    // handshake fns borrow it via `&conn`, so ownership stayed with us).
    mgr.compat_registry()
        .note_compatible(*conn.remote_id().as_bytes());

    // Register the live responder session so our outbound DMs to this peer
    // reuse the bidirectional tunnel. The manager applies lower-NodeId collision
    // dedup: if it already holds a session for this peer, the loser is closed
    // and this `cmd_rx` may be dropped immediately — `run_tunnel_loop` then
    // exits cleanly on the first `recv()` returning `None`. The returned `epoch`
    // identifies THIS session so loop-exit evicts only our own entry (CR12).
    let (cmd_rx, epoch) = mgr.register_inbound(peer_node_id);

    run_tunnel_loop(
        session,
        send_stream,
        recv_stream,
        peer_node_id,
        Arc::clone(&mgr),
        epoch,
        ingest_tx,
        cmd_rx,
    )
    .await;
}

/// Responder handshake: accept the bi-stream, (v2) read + gate the peer hello,
/// read `TunnelInit`, build the session, (v2) write our own hello, then write
/// `TunnelAccept`. ZEB-623: the generation is read from the negotiated ALPN
/// (`conn.alpn()`); a `/v1` connection carries no hello (unchanged wire
/// format). Returns the session + streams + the peer's authenticated NodeId.
async fn responder_handshake(
    conn: &Connection,
    local_pq: &PqPrivateIdentity,
) -> Result<(TunnelSession, SendStream, RecvStream, [u8; 32]), HandshakeFailure> {
    // ZEB-623: the negotiated ALPN tells us the generation. Read it the same way
    // the accept loop reads `alpn_used` (`Connection::alpn()` → &[u8]).
    let is_v2 = conn.alpn() == crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V2;

    let (mut send_stream, mut recv_stream) = conn
        .accept_bi()
        .await
        .map_err(|e| HandshakeFailure::Other(format!("accept_bi: {e}")))?;

    // v2: the peer's capabilities hello precedes its TunnelInit on the wire.
    if is_v2 {
        let peer_hello_bytes = read_length_prefixed(
            &mut recv_stream,
            crate::protocol_versioning::TUNNEL_HELLO_MAX,
        )
        .await
        .map_err(|e| HandshakeFailure::Other(format!("read hello: {e}")))?;
        let peer_hello = crate::protocol_versioning::decode_hello(&peer_hello_bytes)
            .map_err(|e| HandshakeFailure::Other(format!("decode hello: {e}")))?;
        if let Err(reason) = crate::protocol_versioning::check_hello_compatible(&peer_hello) {
            // The peer's *tunnel* NodeId (`blake3(ML-DSA pubkey)`) isn't
            // authenticated until the PQ handshake completes — but iroh HAS
            // authenticated the remote *endpoint* key in its TLS handshake, so the
            // caller (`run_tunnel_responder`) DOES record this incompatibility in
            // the compat registry, keyed by `conn.remote_id()` (the same iroh
            // EndpointId Network Health joins on). Here we just log LOUDLY with
            // that id and reject; the registry write happens on the returned
            // Incompatible.
            tracing::warn!(
                remote = %conn.remote_id(),
                %reason,
                "ZEB-623: inbound tunnel peer speaks an incompatible protocol hello; rejecting"
            );
            return Err(HandshakeFailure::Incompatible { reason });
        }
    }

    let init_bytes = read_length_prefixed(&mut recv_stream, HANDSHAKE_MAX_MESSAGE)
        .await
        .map_err(|e| HandshakeFailure::Other(format!("read TunnelInit: {e}")))?;

    let mut rng = rand::rngs::OsRng;
    let now_ms = millis_since_start();
    let (session, actions) = TunnelSession::new_responder(&mut rng, local_pq, &init_bytes, now_ms)
        .map_err(|e| HandshakeFailure::Other(format!("new_responder: {e}")))?;

    // v2: write our own hello BEFORE the TunnelAccept (the initiator reads the
    // peer hello first, mirroring our read order above).
    if is_v2 {
        let hello = crate::protocol_versioning::encode_hello(
            &crate::protocol_versioning::TunnelHello::current(),
        )
        .map_err(|e| HandshakeFailure::Other(format!("encode hello: {e}")))?;
        write_length_prefixed(&mut send_stream, &hello)
            .await
            .map_err(|e| HandshakeFailure::Other(format!("write hello: {e}")))?;
    }

    // Extract the authenticated peer NodeId from the HandshakeComplete action,
    // and write the TunnelAccept before returning (so the bytes are on the wire
    // before we register the session).
    let mut peer_node_id = None;
    for action in &actions {
        match action {
            TunnelAction::OutboundBytes { data } => {
                write_length_prefixed(&mut send_stream, data)
                    .await
                    .map_err(|e| HandshakeFailure::Other(format!("write TunnelAccept: {e}")))?;
            }
            TunnelAction::HandshakeComplete {
                peer_node_id: id, ..
            } => {
                peer_node_id = Some(*id);
            }
            _ => {}
        }
    }

    let peer_node_id = peer_node_id.ok_or_else(|| {
        HandshakeFailure::Other("responder handshake produced no peer NodeId".to_string())
    })?;
    Ok((session, send_stream, recv_stream, peer_node_id))
}

/// Run the **initiator** side of an outbound tunnel connection.
///
/// Dials the peer over the PERSISTENT iroh endpoint, opens a bi-stream,
/// completes the PQ handshake, then enters `run_tunnel_loop`. On a successful
/// handshake the manager handle for `peer_node_id` is flipped to Active and its
/// `pending` queue flushed (driven inside `run_tunnel_loop`'s dispatch).
#[allow(clippy::too_many_arguments)]
pub async fn run_tunnel_initiator(
    endpoint: IrohEndpoint,
    contact: DeviceTunnelContact,
    local_pq: Arc<PqPrivateIdentity>,
    peer_node_id: [u8; 32],
    mgr: Arc<TunnelManager>,
    epoch: u64,
    ingest_tx: mpsc::Sender<InboundDm>,
    cmd_rx: mpsc::Receiver<TunnelCommand>,
) {
    // Resolve the dial target + peer PQ identity from the contact. A malformed
    // contact is a hard failure: drop the Dialing handle so the pending DMs fall
    // back to the always-deposit durability path (previously these were surfaced
    // as handshake errors inside `initiator_handshake`; same net effect).
    let addr = match dial_addr(&contact) {
        Ok(a) => a,
        Err(reason) => {
            tracing::debug!(%reason, "ZEB-473: outbound tunnel dial-addr build failed");
            mgr.note_dial_failed(peer_node_id, epoch);
            return;
        }
    };
    let peer_pq = match peer_pq_identity(&contact) {
        Ok(p) => p,
        Err(reason) => {
            tracing::debug!(%reason, "ZEB-473: outbound tunnel peer-identity build failed");
            mgr.note_dial_failed(peer_node_id, epoch);
            return;
        }
    };
    run_tunnel_initiator_inner(
        endpoint,
        addr,
        peer_pq,
        local_pq,
        peer_node_id,
        mgr,
        epoch,
        ingest_tx,
        cmd_rx,
    )
    .await;
}

/// Inner initiator driver over a pre-resolved dial `addr` + `peer_pq` (the
/// production entry [`run_tunnel_initiator`] derives both from the contact).
/// Split out (ZEB-623) so hermetic tests can inject a connectable loopback
/// `addr` — a node-id-only `dial_addr` can't resolve over loopback in iroh
/// 1.0's discovery-only dial — and thereby exercise the real v2/v1 negotiation
/// plus the compat-registry wiring end to end.
#[allow(clippy::too_many_arguments)]
async fn run_tunnel_initiator_inner(
    endpoint: IrohEndpoint,
    addr: iroh::EndpointAddr,
    peer_pq: PqIdentity,
    local_pq: Arc<PqPrivateIdentity>,
    peer_node_id: [u8; 32],
    mgr: Arc<TunnelManager>,
    epoch: u64,
    ingest_tx: mpsc::Sender<InboundDm>,
    cmd_rx: mpsc::Receiver<TunnelCommand>,
) {
    // ZEB-623: capture the peer's IROH EndpointId (the key Network Health joins
    // on) before `addr` is moved into the handshake. The compat registry is keyed
    // by THIS id — not the tunnel `peer_node_id` (`blake3(ML-DSA pubkey)`) — so
    // the incompat/compat records line up with the reader in `network_health.rs`,
    // which looks up by `record.iroh_node_id`.
    let iroh_join_key = *addr.id.as_bytes();
    let handshake = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        initiator_handshake(&endpoint, addr, &peer_pq, &local_pq),
    )
    .await;

    let (session, send_stream, recv_stream) = match handshake {
        Ok(Ok(v)) => v,
        Ok(Err(HandshakeFailure::Incompatible { reason })) => {
            // ZEB-623: the peer speaks a tunnel protocol below our minimum.
            // Record it LOUDLY (surfaced in Network Health) keyed by the peer's
            // IROH EndpointId — the key Network Health joins on — then drop the
            // Dialing handle so DMs fall back to always-deposit durability. The
            // dial-failure bookkeeping stays on the tunnel `peer_node_id` (the
            // TunnelManager session-map key); pass our epoch so a newer session
            // that replaced us isn't evicted.
            mgr.compat_registry()
                .note_incompatible(iroh_join_key, reason);
            mgr.note_dial_failed(peer_node_id, epoch);
            return;
        }
        Ok(Err(HandshakeFailure::Other(reason))) => {
            tracing::debug!(%reason, "ZEB-473: outbound tunnel handshake failed");
            mgr.note_dial_failed(peer_node_id, epoch);
            return;
        }
        Err(_) => {
            tracing::debug!("ZEB-473: outbound tunnel handshake timed out");
            mgr.note_dial_failed(peer_node_id, epoch);
            return;
        }
    };

    // Handshake reached Active. ZEB-623: clear any stale incompatibility record
    // for this peer — a successful handshake, over v1 or v2, proves we can speak
    // a compatible protocol (e.g. the peer has since upgraded). The record is
    // keyed by the peer's IROH EndpointId (the Network Health join key), so we
    // clear it under that same key. Then flip the manager handle to Active
    // (keyed by the tunnel `peer_node_id`) and flush any DMs buffered while we
    // were dialing (applies the lower-NodeId dedup if an inbound session for this
    // peer raced in).
    mgr.compat_registry().note_compatible(iroh_join_key);
    mgr.note_active(peer_node_id);

    run_tunnel_loop(
        session,
        send_stream,
        recv_stream,
        peer_node_id,
        Arc::clone(&mgr),
        epoch,
        ingest_tx,
        cmd_rx,
    )
    .await;
}

/// Initiator handshake over a pre-resolved `addr`. ZEB-623: dials the newest
/// tunnel generation (`/v2`) first and falls back to `/v1` on ANY connect error
/// (a peer that only registered the `/v1` ALPN rejects the `/v2` negotiation).
/// On a `/v2` connection the first stream frame each side writes is the
/// versioned [`TunnelHello`](crate::protocol_versioning::TunnelHello): the
/// initiator pipelines `[hello][TunnelInit]`, then reads the peer's
/// `[hello][TunnelAccept]` and gates the peer hello through
/// [`check_hello_compatible`](crate::protocol_versioning::check_hello_compatible)
/// before driving the state machine to Active. `/v1` connections carry no hello
/// (unchanged wire format). Returns the active session + streams.
async fn initiator_handshake(
    endpoint: &IrohEndpoint,
    addr: iroh::EndpointAddr,
    peer_pq: &PqIdentity,
    local_pq: &PqPrivateIdentity,
) -> Result<(TunnelSession, SendStream, RecvStream), HandshakeFailure> {
    // Try v2 first; fall back to v1 on ANY connect error. `EndpointAddr` is
    // Clone, so the v2 attempt keeps a copy for the possible v1 retry.
    let (conn, gen2) = match endpoint
        .inner()
        .connect(addr.clone(), crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V2)
        .await
    {
        Ok(c) => (c, true),
        Err(e2) => {
            tracing::debug!(err = %e2, "ZEB-623: tunnel v2 connect failed; falling back to v1");
            match endpoint
                .inner()
                .connect(addr, crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1)
                .await
            {
                Ok(c) => (c, false),
                Err(e1) => {
                    return Err(HandshakeFailure::Other(format!(
                        "connect v2: {e2}; v1: {e1}"
                    )))
                }
            }
        }
    };
    let (mut send_stream, mut recv_stream) = conn
        .open_bi()
        .await
        .map_err(|e| HandshakeFailure::Other(format!("open_bi: {e}")))?;

    // v2: our capabilities hello precedes the TunnelInit on the wire.
    if gen2 {
        let hello = crate::protocol_versioning::encode_hello(
            &crate::protocol_versioning::TunnelHello::current(),
        )
        .map_err(|e| HandshakeFailure::Other(format!("encode hello: {e}")))?;
        write_length_prefixed(&mut send_stream, &hello)
            .await
            .map_err(|e| HandshakeFailure::Other(format!("write hello: {e}")))?;
    }

    let mut rng = rand::rngs::OsRng;
    let now_ms = millis_since_start();
    let (mut session, init_actions) =
        TunnelSession::new_initiator(&mut rng, local_pq, peer_pq, now_ms)
            .map_err(|e| HandshakeFailure::Other(format!("new_initiator: {e}")))?;

    for action in init_actions {
        if let TunnelAction::OutboundBytes { data } = action {
            write_length_prefixed(&mut send_stream, &data)
                .await
                .map_err(|e| HandshakeFailure::Other(format!("write TunnelInit: {e}")))?;
        }
    }

    // v2: read + gate the peer's hello BEFORE the TunnelAccept (the responder
    // pipelines them in the same order).
    if gen2 {
        let peer_hello_bytes = read_length_prefixed(
            &mut recv_stream,
            crate::protocol_versioning::TUNNEL_HELLO_MAX,
        )
        .await
        .map_err(|e| HandshakeFailure::Other(format!("read hello: {e}")))?;
        let peer_hello = crate::protocol_versioning::decode_hello(&peer_hello_bytes)
            .map_err(|e| HandshakeFailure::Other(format!("decode hello: {e}")))?;
        if let Err(reason) = crate::protocol_versioning::check_hello_compatible(&peer_hello) {
            // ZEB-623: explicitly close before returning so the peer sees a prompt
            // application close instead of an idle timeout. The caller
            // (`run_tunnel_initiator_inner`) records the incompatibility in the
            // compat registry keyed by the peer's iroh EndpointId.
            conn.close(0u32.into(), b"tunnel-protocol-incompatible");
            return Err(HandshakeFailure::Incompatible { reason });
        }
    }

    let accept_bytes = read_length_prefixed(&mut recv_stream, HANDSHAKE_MAX_MESSAGE)
        .await
        .map_err(|e| HandshakeFailure::Other(format!("read TunnelAccept: {e}")))?;
    let now_ms = millis_since_start();
    let actions = session
        .handle_event(TunnelEvent::InboundBytes {
            data: accept_bytes,
            now_ms,
        })
        .map_err(|e| HandshakeFailure::Other(format!("handle TunnelAccept: {e}")))?;

    // The accept-processing emits HandshakeComplete (and possibly nothing else);
    // there are no outbound bytes here. Confirm we reached Active.
    if session.state() != TunnelState::Active {
        return Err(HandshakeFailure::Other(
            "initiator did not reach Active after TunnelAccept".to_string(),
        ));
    }
    debug_assert!(actions
        .iter()
        .any(|a| matches!(a, TunnelAction::HandshakeComplete { .. })));

    Ok((session, send_stream, recv_stream))
}

/// Build the iroh `EndpointAddr` to dial from a peer's [`DeviceTunnelContact`].
///
/// Made `pub(crate)` so the manager's unit tests can assert the constructed
/// address carries the right node id + relay.
pub(crate) fn dial_addr(contact: &DeviceTunnelContact) -> Result<iroh::EndpointAddr, String> {
    let ep_id = iroh::EndpointId::from_bytes(&contact.iroh_node_id)
        .map_err(|e| format!("tunnel endpoint id: {e}"))?;
    let mut addr = iroh::EndpointAddr::new(ep_id);
    if let Some(relay) = contact.home_relay_url.as_deref() {
        if !relay.is_empty() {
            match relay.parse::<iroh::RelayUrl>() {
                Ok(url) => addr = addr.with_relay_url(url),
                Err(e) => tracing::trace!(
                    relay = %relay,
                    "ZEB-473: skip malformed tunnel home_relay_url: {e}"
                ),
            }
        }
    }
    Ok(addr)
}

/// Reconstruct the peer's PQ identity from the contact's KEM + DSA public-key
/// bytes. `PqIdentity::from_public_bytes` expects `[ML-KEM pub (1184)][ML-DSA
/// pub (1952)]`, which is exactly the order the contact carries.
pub(crate) fn peer_pq_identity(contact: &DeviceTunnelContact) -> Result<PqIdentity, String> {
    let mut combined =
        Vec::with_capacity(contact.pq_kem_pubkey.len() + contact.pq_dsa_pubkey.len());
    combined.extend_from_slice(&contact.pq_kem_pubkey);
    combined.extend_from_slice(&contact.pq_dsa_pubkey);
    PqIdentity::from_public_bytes(&combined).map_err(|e| format!("peer PQ identity: {e:?}"))
}

/// Main read/write/keepalive loop. Runs until the tunnel closes, errors, or the
/// command channel drops.
///
/// `FramedRead` + `LengthDelimitedCodec` make the read arm cancel-safe: the
/// codec buffers partial reads internally, so dropping the read future mid-frame
/// (when `select!` fires another arm) does not discard consumed bytes — the next
/// `.next()` resumes from the buffered position.
#[allow(clippy::too_many_arguments)]
async fn run_tunnel_loop(
    mut session: TunnelSession,
    mut send_stream: SendStream,
    recv_stream: RecvStream,
    peer_node_id: [u8; 32],
    mgr: Arc<TunnelManager>,
    epoch: u64,
    ingest_tx: mpsc::Sender<InboundDm>,
    mut cmd_rx: mpsc::Receiver<TunnelCommand>,
) {
    let codec = LengthDelimitedCodec::builder()
        .length_field_length(4)
        .big_endian()
        .max_frame_length(DATA_MAX_MESSAGE)
        .new_codec();
    let mut framed = FramedRead::new(recv_stream, codec);

    let mut keepalive = tokio::time::interval(KEEPALIVE_TICK);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            frame = framed.next() => {
                match frame {
                    Some(Ok(bytes)) => {
                        let data = bytes.to_vec();
                        let now_ms = millis_since_start();
                        match session.handle_event(TunnelEvent::InboundBytes { data, now_ms }) {
                            Ok(actions) => {
                                if !dispatch_tunnel_actions(
                                    &actions, &mut send_stream, peer_node_id, &ingest_tx,
                                ).await {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::debug!(err = %e, "ZEB-473: tunnel inbound error; closing");
                                break;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::debug!(err = %e, "ZEB-473: tunnel stream read error; closing");
                        break;
                    }
                    None => {
                        tracing::debug!("ZEB-473: tunnel stream closed by peer");
                        break;
                    }
                }
            }

            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(TunnelCommand::SendDm(payload)) => {
                        let now_ms = millis_since_start();
                        match session.handle_event(TunnelEvent::SendDm { payload, now_ms }) {
                            Ok(actions) => {
                                if !dispatch_tunnel_actions(
                                    &actions, &mut send_stream, peer_node_id, &ingest_tx,
                                ).await {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(err = %e, "ZEB-473: tunnel SendDm error");
                            }
                        }
                    }
                    Some(TunnelCommand::Close) | None => {
                        if let Ok(actions) = session.handle_event(TunnelEvent::Close) {
                            let _ = dispatch_tunnel_actions(
                                &actions, &mut send_stream, peer_node_id, &ingest_tx,
                            ).await;
                        }
                        break;
                    }
                }
            }

            _ = keepalive.tick() => {
                let now_ms = millis_since_start();
                match session.handle_event(TunnelEvent::Tick { now_ms }) {
                    Ok(actions) => {
                        if !dispatch_tunnel_actions(
                            &actions, &mut send_stream, peer_node_id, &ingest_tx,
                        ).await {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(err = %e, "ZEB-473: tunnel tick error; closing");
                        break;
                    }
                }
            }
        }
    }

    // Loop exit = session over. Best-effort: the send half is finished here; the
    // recv half drops with `framed`.
    let _ = send_stream.finish();
    // CR12: evict THIS session from the manager so dead entries don't accumulate
    // under peer churn. `note_closed` is epoch-guarded: it removes only if the
    // current entry is still this same (now-dead) session, never an ABA
    // replacement that a redial/dedup installed for the same peer.
    mgr.note_closed(peer_node_id, epoch);
}

/// Two-pass dispatch of `TunnelAction`s.
///
/// Pass 1 writes every `OutboundBytes` to the bi-stream (length-prefixed) and
/// RECORDS (without acting on) the first terminal `Error`/`Closed`. Pass 2
/// forwards EVERY `DmReceived` payload to the ingest seam — even when this
/// batch also carries a terminal action — and only THEN does the recorded
/// terminal decision take effect. Returns `true` to continue the loop, `false`
/// to exit.
///
/// G2 (ZEB-473): the terminal bail must NOT gate `DmReceived` forwarding. The
/// `TunnelSession` state machine can legitimately emit `[DmReceived(x), Error]`
/// in a single `poll` (the DM's bytes were already processed); dropping `x`
/// because an `Error` followed it in the same batch would silently lose a
/// fully-received DM. So we drain all `DmReceived` first, then honor the bail.
///
/// `TunnelSession` guarantees `OutboundBytes` precede `Error`/`Closed`, so a
/// terminal action never strands a trailing frame. An outbound WRITE error,
/// however, is itself a hard stop: there is no point forwarding further bytes
/// onto a broken stream, but we still drain any already-received DMs first.
async fn dispatch_tunnel_actions(
    actions: &[TunnelAction],
    send_stream: &mut SendStream,
    peer_node_id: [u8; 32],
    ingest_tx: &mpsc::Sender<InboundDm>,
) -> bool {
    // Pass 1: write all outbound bytes; RECORD (don't act on) terminal actions
    // and write failures so Pass 2 can still drain inbound DMs first.
    let mut terminal = false;
    for action in actions {
        match action {
            TunnelAction::OutboundBytes { data } => {
                if let Err(e) = write_length_prefixed(send_stream, data).await {
                    tracing::debug!(err = %e, "ZEB-473: tunnel write error; closing");
                    terminal = true;
                    break;
                }
            }
            TunnelAction::Error { reason } => {
                tracing::debug!(%reason, "ZEB-473: tunnel session error; closing");
                terminal = true;
            }
            TunnelAction::Closed => {
                tracing::debug!("ZEB-473: tunnel session closed");
                terminal = true;
            }
            _ => {}
        }
    }

    // Pass 2: forward DM payloads to the ingest seam — ALWAYS, even on a
    // terminal batch, so a DmReceived that arrived alongside an Error isn't
    // dropped. HandshakeComplete on the initiator/responder is already handled
    // before entering the loop (responder registers in the manager; initiator
    // reaches Active in the handshake fn); we ignore it here. Zenoh/Replication
    // frames are not expected on a DM tunnel and are dropped (logged).
    for action in actions {
        match action {
            TunnelAction::DmReceived { payload } => {
                if ingest_tx
                    .send(InboundDm {
                        peer_node_id,
                        payload: payload.clone(),
                    })
                    .await
                    .is_err()
                {
                    tracing::debug!("ZEB-473: DM ingest channel closed; dropping inbound DM");
                }
            }
            TunnelAction::ZenohReceived { .. } | TunnelAction::ReplicationReceived { .. } => {
                tracing::debug!("ZEB-473: unexpected non-DM frame on DM tunnel; dropping");
            }
            _ => {}
        }
    }

    // Now honor the recorded terminal decision (after inbound DMs were drained).
    !terminal
}

// ── Wire helpers (4-byte big-endian length prefix) ──────────────────────────

/// Write a length-prefixed message: `[4 bytes big-endian length][payload]`.
///
/// Prefix and payload are written from a single buffer so a partial write can't
/// leave the peer's `LengthDelimitedCodec` mid-frame.
async fn write_length_prefixed(
    stream: &mut SendStream,
    data: &[u8],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // The write side is caller-bounded (handshake messages), so cap at the
    // wire-representable max (u32::MAX): preserves the historical "always
    // write" behavior while keeping the prefix u32-safe. BE + allow-empty
    // matches this protocol's shipped wire format exactly.
    crate::iroh_framing::write_len_prefixed(
        stream,
        data,
        u32::MAX as usize,
        crate::iroh_framing::Endian::Be,
        true,
    )
    .await
    .map_err(Into::into)
}

/// Read a length-prefixed message: `[4 bytes big-endian length][payload]`.
///
/// `max_bytes` caps the allocation so an unauthenticated peer can't trigger a
/// huge allocation during the handshake phase. Used only in the handshake (the
/// data phase reads through `FramedRead`).
async fn read_length_prefixed(
    stream: &mut RecvStream,
    max_bytes: usize,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    // BE + allow-empty: this protocol accepts zero-length frames (the cap is
    // read-side only). Preserve the exact "message too large" error text.
    crate::iroh_framing::read_len_prefixed(stream, max_bytes, crate::iroh_framing::Endian::Be, true)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            match e {
                crate::iroh_framing::FramingError::OutOfBounds(f) => {
                    format!("message too large: {} bytes (max {})", f.len, f.max).into()
                }
                crate::iroh_framing::FramingError::Io(io) => Box::new(io),
            }
        })
}

/// Monotonic milliseconds since first call (process-global epoch). Used for
/// `TunnelSession` timestamps; shared with the manager via re-export.
pub(crate) fn millis_since_start() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::DeviceTunnelContact;

    fn pq_pubkeys() -> (Vec<u8>, Vec<u8>) {
        let id = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let pubid = id.public_identity();
        (
            pubid.encryption_key.as_bytes().to_vec(),
            pubid.verifying_key.as_bytes().to_vec(),
        )
    }

    #[test]
    fn dial_addr_carries_node_id_and_relay() {
        // A valid EndpointId is any ed25519 public key; generate one via iroh.
        let sk = iroh::SecretKey::generate();
        let node_id = *sk.public().as_bytes();
        let (kem, dsa) = pq_pubkeys();
        let contact = DeviceTunnelContact {
            iroh_node_id: node_id,
            home_relay_url: Some("https://relay.example/".to_string()),
            pq_dsa_pubkey: dsa,
            pq_kem_pubkey: kem,
        };
        let addr = dial_addr(&contact).expect("dial addr");
        assert_eq!(
            *addr.id.as_bytes(),
            node_id,
            "addr must carry the contact node id"
        );
        assert!(
            addr.relay_urls()
                .any(|u| u.to_string().contains("relay.example")),
            "addr must carry the contact relay url"
        );
    }

    #[test]
    fn dial_addr_tolerates_missing_relay() {
        let sk = iroh::SecretKey::generate();
        let node_id = *sk.public().as_bytes();
        let (kem, dsa) = pq_pubkeys();
        let contact = DeviceTunnelContact {
            iroh_node_id: node_id,
            home_relay_url: None,
            pq_dsa_pubkey: dsa,
            pq_kem_pubkey: kem,
        };
        let addr = dial_addr(&contact).expect("dial addr");
        assert_eq!(*addr.id.as_bytes(), node_id);
        assert_eq!(addr.relay_urls().count(), 0);
    }

    #[test]
    fn peer_pq_identity_roundtrips_through_contact() {
        // A contact built from a real PQ identity's pubkeys must reconstruct an
        // identity with the same address hash (so the initiator dials the right
        // peer and the responder identity check passes).
        let id = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let pubid = id.public_identity();
        let sk = iroh::SecretKey::generate();
        let contact = DeviceTunnelContact {
            iroh_node_id: *sk.public().as_bytes(),
            home_relay_url: None,
            pq_dsa_pubkey: pubid.verifying_key.as_bytes().to_vec(),
            pq_kem_pubkey: pubid.encryption_key.as_bytes().to_vec(),
        };
        let rebuilt = peer_pq_identity(&contact).expect("rebuild peer pq identity");
        assert_eq!(
            rebuilt.address_hash, pubid.address_hash,
            "reconstructed peer identity must match the source identity"
        );
    }

    #[test]
    fn millis_since_start_is_monotonic() {
        let a = millis_since_start();
        let b = millis_since_start();
        assert!(b >= a);
    }

    // ── In-process two-session handshake (the must-have validation) ──────────
    //
    // Drives the REAL responder path (`run_tunnel_responder` over a connected
    // iroh loopback pair) against an initiator side driven through the same
    // `TunnelSession` + `run_tunnel_loop` the production initiator uses. One
    // `FrameTag::Dm` frame round-trips each direction; we assert the responder's
    // ingest channel receives the BYTE-IDENTICAL payload (initiator→responder),
    // then responder→initiator.

    use crate::tunnel_manager::{node_id_from_dsa_pubkey, InboundDm, TunnelCommand, TunnelManager};
    use harmony_tunnel::session::TunnelState;
    use harmony_tunnel::TunnelEvent;
    use std::sync::Arc;

    /// Build a loopback-only iroh endpoint (no relays, no address lookup)
    /// binding BOTH tunnel ALPN generations (`/v1` + `/v2`) — ZEB-623: mirrors
    /// the production bind list so the negotiation tests exercise the real
    /// v2-first dial path (and existing v1 tests, which dial `/v1` explicitly,
    /// still land on the unchanged v1 wire path).
    async fn loopback_endpoint(secret: [u8; 32]) -> iroh::endpoint::Endpoint {
        loopback_endpoint_with_alpns(
            secret,
            vec![
                crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1.to_vec(),
                crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V2.to_vec(),
            ],
        )
        .await
    }

    /// ZEB-623: a loopback endpoint binding ONLY the `/v1` tunnel ALPN — a
    /// one-generation-behind peer. A v2 dialer's `/v2` connect fails ALPN
    /// negotiation against it and must fall back to `/v1`.
    async fn loopback_endpoint_v1_only(secret: [u8; 32]) -> iroh::endpoint::Endpoint {
        loopback_endpoint_with_alpns(
            secret,
            vec![crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1.to_vec()],
        )
        .await
    }

    async fn loopback_endpoint_with_alpns(
        secret: [u8; 32],
        alpns: Vec<Vec<u8>>,
    ) -> iroh::endpoint::Endpoint {
        use iroh::endpoint::{presets, Endpoint, RelayMode};
        use iroh::SecretKey;
        use std::net::Ipv4Addr;
        Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&secret))
            .alpns(alpns)
            .relay_mode(RelayMode::Disabled)
            .dns_resolver(crate::iroh_endpoint::hermetic_dns_resolver())
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind_addr")
            .bind()
            .await
            .expect("bind loopback endpoint")
    }

    /// ZEB-623 test helper: a `TunnelManager` over a loopback endpoint with a
    /// caller-supplied compat registry (so a test can assert on the registry the
    /// initiator writes). Never dials on its own.
    fn tunnel_manager_with_compat(
        endpoint: iroh::endpoint::Endpoint,
        local_pq: Arc<harmony_identity::PqPrivateIdentity>,
        ingest_tx: mpsc::Sender<InboundDm>,
        compat: Arc<crate::protocol_versioning::ProtocolCompatRegistry>,
    ) -> Arc<TunnelManager> {
        Arc::new(TunnelManager::new(
            crate::iroh_endpoint::IrohEndpoint::from_endpoint_for_test(endpoint),
            local_pq,
            ingest_tx,
            compat,
        ))
    }

    // ── ZEB-623: tunnel/v2 hello negotiation ─────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v2_dialer_to_v2_acceptor_exchanges_hello_and_reaches_active() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(std::time::Duration::from_secs(30), v2_to_v2_inner())
            .await
            .expect("v2↔v2 hello negotiation must complete within 30s");
    }

    async fn v2_to_v2_inner() {
        use iroh::{EndpointAddr, TransportAddr};

        let initiator_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));
        let responder_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));

        // Both endpoints bind v1+v2: the dialer's v2 connect succeeds.
        let ep_a = loopback_endpoint([0x41; 32]).await;
        let ep_b = loopback_endpoint([0x42; 32]).await;
        let ep_b_addr = EndpointAddr::from_parts(
            ep_b.id(),
            ep_b.bound_sockets().into_iter().map(TransportAddr::Ip),
        );

        // Responder side: production responder driver on the accepted connection.
        // ZEB-623 round-2: pre-seed the ACCEPTOR-side registry with a STALE
        // incompatibility keyed by the dialer's iroh EndpointId (what the acceptor
        // sees as `conn.remote_id()`), simulating a peer that was previously
        // flagged and has since upgraded. A successful INBOUND handshake must
        // clear it symmetrically (mirrors the initiator path).
        let resp_compat = Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default());
        resp_compat.note_incompatible(*ep_a.id().as_bytes(), "stale".into());
        let (resp_ingest_tx, mut resp_ingest_rx) = mpsc::channel::<InboundDm>(8);
        let resp_mgr = tunnel_manager_with_compat(
            ep_b.clone(),
            Arc::clone(&responder_pq),
            resp_ingest_tx.clone(),
            Arc::clone(&resp_compat),
        );
        let resp_pq = Arc::clone(&responder_pq);
        let ep_b_accept = ep_b.clone();
        let responder = tokio::spawn(async move {
            let incoming = ep_b_accept
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("connection established");
            run_tunnel_responder(incoming, resp_pq, resp_mgr, resp_ingest_tx).await;
        });

        // Initiator side: a FRESH compat registry we assert on.
        let compat = Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default());
        let (init_ingest_tx, _init_ingest_rx) = mpsc::channel::<InboundDm>(8);
        let init_mgr = tunnel_manager_with_compat(
            ep_a.clone(),
            Arc::clone(&initiator_pq),
            init_ingest_tx.clone(),
            Arc::clone(&compat),
        );

        let peer_node_id =
            node_id_from_dsa_pubkey(&responder_pq.public_identity().verifying_key.as_bytes());
        let init_node_id =
            node_id_from_dsa_pubkey(&initiator_pq.public_identity().verifying_key.as_bytes());
        let (init_cmd_tx, init_cmd_rx) = mpsc::channel::<TunnelCommand>(8);

        let endpoint = crate::iroh_endpoint::IrohEndpoint::from_endpoint_for_test(ep_a.clone());
        let peer_pq = responder_pq.public_identity().clone();
        let init_mgr_task = Arc::clone(&init_mgr);
        let initiator = tokio::spawn(async move {
            run_tunnel_initiator_inner(
                endpoint,
                ep_b_addr,
                peer_pq,
                initiator_pq,
                peer_node_id,
                init_mgr_task,
                0,
                init_ingest_tx,
                init_cmd_rx,
            )
            .await;
        });

        // Prove Active by round-tripping a DM initiator→responder.
        let dm_payload = b"v2-negotiated-dm".to_vec();
        init_cmd_tx
            .send(TunnelCommand::SendDm(dm_payload.clone()))
            .await
            .expect("queue SendDm");
        let received =
            tokio::time::timeout(std::time::Duration::from_secs(10), resp_ingest_rx.recv())
                .await
                .expect("responder ingest within 10s")
                .expect("responder ingest payload");
        assert_eq!(received.payload, dm_payload);
        assert_eq!(received.peer_node_id, init_node_id);

        // A compatible v2 handshake records NO incompatibility for the peer.
        assert!(
            init_mgr
                .compat_registry()
                .incompat_reason(&peer_node_id)
                .is_none(),
            "a compatible v2 handshake must leave the compat registry clean"
        );

        // ZEB-623 round-2: the successful INBOUND handshake must have CLEARED the
        // stale incompatibility pre-seeded above, keyed by the dialer's iroh
        // EndpointId (the acceptor's `conn.remote_id()`). Without the responder's
        // symmetric `note_compatible`, a previously-flagged peer that reconnects
        // inbound would stay flagged in Network Health.
        assert!(
            resp_compat.incompat_reason(ep_a.id().as_bytes()).is_none(),
            "a successful inbound handshake must clear a stale incompatibility \
             for the peer's iroh EndpointId on the acceptor side"
        );

        drop(init_cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), initiator).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), responder).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v2_dialer_falls_back_to_v1_only_acceptor() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(std::time::Duration::from_secs(30), v2_fallback_inner())
            .await
            .expect("v2→v1 fallback must complete within 30s");
    }

    async fn v2_fallback_inner() {
        use iroh::{EndpointAddr, TransportAddr};

        let initiator_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));
        let responder_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));

        // Dialer binds v1+v2; acceptor binds ONLY v1 → the v2 connect fails and
        // the dialer must fall back to v1.
        let ep_a = loopback_endpoint([0x51; 32]).await;
        let ep_b = loopback_endpoint_v1_only([0x52; 32]).await;
        let ep_b_addr = EndpointAddr::from_parts(
            ep_b.id(),
            ep_b.bound_sockets().into_iter().map(TransportAddr::Ip),
        );

        let (resp_ingest_tx, mut resp_ingest_rx) = mpsc::channel::<InboundDm>(8);
        let resp_mgr = tunnel_manager_with_compat(
            ep_b.clone(),
            Arc::clone(&responder_pq),
            resp_ingest_tx.clone(),
            Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default()),
        );
        let resp_pq = Arc::clone(&responder_pq);
        let ep_b_accept = ep_b.clone();
        let responder = tokio::spawn(async move {
            // The dialer's v2 attempt lands FIRST and fails ALPN negotiation on
            // this v1-only endpoint (the crypto handshake aborts with "peer
            // doesn't support any known protocol"). Mirror the production accept
            // loop: skip the failed incoming and wait for the v1 fallback dial.
            let conn = loop {
                let incoming = ep_b_accept.accept().await.expect("incoming");
                match incoming.await {
                    Ok(conn) => break conn,
                    Err(e) => {
                        tracing::debug!(err = %e, "skipping failed v2 incoming on v1-only acceptor");
                        continue;
                    }
                }
            };
            run_tunnel_responder(conn, resp_pq, resp_mgr, resp_ingest_tx).await;
        });

        let compat = Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default());
        let (init_ingest_tx, _init_ingest_rx) = mpsc::channel::<InboundDm>(8);
        let init_mgr = tunnel_manager_with_compat(
            ep_a.clone(),
            Arc::clone(&initiator_pq),
            init_ingest_tx.clone(),
            Arc::clone(&compat),
        );

        let peer_node_id =
            node_id_from_dsa_pubkey(&responder_pq.public_identity().verifying_key.as_bytes());
        let init_node_id =
            node_id_from_dsa_pubkey(&initiator_pq.public_identity().verifying_key.as_bytes());
        let (init_cmd_tx, init_cmd_rx) = mpsc::channel::<TunnelCommand>(8);

        let endpoint = crate::iroh_endpoint::IrohEndpoint::from_endpoint_for_test(ep_a.clone());
        let peer_pq = responder_pq.public_identity().clone();
        let init_mgr_task = Arc::clone(&init_mgr);
        let initiator = tokio::spawn(async move {
            run_tunnel_initiator_inner(
                endpoint,
                ep_b_addr,
                peer_pq,
                initiator_pq,
                peer_node_id,
                init_mgr_task,
                0,
                init_ingest_tx,
                init_cmd_rx,
            )
            .await;
        });

        // Fallback still reaches Active: a DM round-trips over the v1 tunnel.
        let dm_payload = b"v1-fallback-dm".to_vec();
        init_cmd_tx
            .send(TunnelCommand::SendDm(dm_payload.clone()))
            .await
            .expect("queue SendDm");
        let received =
            tokio::time::timeout(std::time::Duration::from_secs(10), resp_ingest_rx.recv())
                .await
                .expect("responder ingest within 10s")
                .expect("responder ingest payload");
        assert_eq!(received.payload, dm_payload);
        assert_eq!(received.peer_node_id, init_node_id);

        // A v1 fallback is a COMPATIBLE peer (within N-1) → no registry entry.
        assert!(
            init_mgr
                .compat_registry()
                .incompat_reason(&peer_node_id)
                .is_none(),
            "a v1 fallback is compatible and must record no incompatibility"
        );

        drop(init_cmd_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), initiator).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), responder).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incompatible_hello_is_rejected_and_recorded() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            incompatible_hello_inner(),
        )
        .await
        .expect("incompatible-hello rejection must complete within 30s");
    }

    async fn incompatible_hello_inner() {
        use iroh::{EndpointAddr, TransportAddr};

        let initiator_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));
        // A valid peer identity so `new_initiator` builds a TunnelInit; the fake
        // acceptor never completes the PQ handshake (the initiator bails on the
        // low hello first).
        let responder_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));

        let ep_a = loopback_endpoint([0x61; 32]).await;
        let ep_b = loopback_endpoint([0x62; 32]).await;
        let ep_b_addr = EndpointAddr::from_parts(
            ep_b.id(),
            ep_b.bound_sockets().into_iter().map(TransportAddr::Ip),
        );

        // Fake acceptor: accept the v2 connection, drain the initiator's hello,
        // then reply with a hello advertising protocol_version 0 (below MIN).
        let ep_b_accept = ep_b.clone();
        let fake_responder = tokio::spawn(async move {
            let incoming = ep_b_accept.accept().await.expect("incoming");
            let conn = incoming.await.expect("connection established");
            let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi");
            // Drain the initiator's hello frame (it pipelines [hello][TunnelInit]).
            let _ =
                read_length_prefixed(&mut recv, crate::protocol_versioning::TUNNEL_HELLO_MAX).await;
            let low_hello = crate::protocol_versioning::encode_hello(
                &crate::protocol_versioning::TunnelHello {
                    protocol_version: 0,
                    capabilities: 0,
                },
            )
            .expect("encode low hello");
            write_length_prefixed(&mut send, &low_hello)
                .await
                .expect("write low hello");
            // Hold the connection open until the initiator hangs up.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        });

        let compat = Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default());
        let (init_ingest_tx, _init_ingest_rx) = mpsc::channel::<InboundDm>(8);
        let init_mgr = tunnel_manager_with_compat(
            ep_a.clone(),
            Arc::clone(&initiator_pq),
            init_ingest_tx.clone(),
            Arc::clone(&compat),
        );

        let peer_node_id =
            node_id_from_dsa_pubkey(&responder_pq.public_identity().verifying_key.as_bytes());
        let (_init_cmd_tx, init_cmd_rx) = mpsc::channel::<TunnelCommand>(8);
        let endpoint = crate::iroh_endpoint::IrohEndpoint::from_endpoint_for_test(ep_a.clone());
        let peer_pq = responder_pq.public_identity().clone();

        // ZEB-623: the compat registry is keyed by the peer's IROH EndpointId
        // (the Network Health join key), which is DISTINCT from the tunnel
        // `peer_node_id` (`blake3(ML-DSA pubkey)`). `ep_b_addr` is moved into the
        // driver below, so snapshot the join key first. The two ids must genuinely
        // differ or the regression assertion below is vacuous.
        let iroh_join_key = *ep_b_addr.id.as_bytes();
        assert_ne!(
            iroh_join_key, peer_node_id,
            "test fixture must derive distinct iroh EndpointId vs tunnel node id"
        );

        // Drive the real initiator: it must fail Incompatible and record it.
        run_tunnel_initiator_inner(
            endpoint,
            ep_b_addr,
            peer_pq,
            initiator_pq,
            peer_node_id,
            Arc::clone(&init_mgr),
            0,
            init_ingest_tx,
            init_cmd_rx,
        )
        .await;

        // The incompat record MUST land under the peer's IROH EndpointId — the
        // key `network_health.rs` joins on (`record.iroh_node_id`) — so the badge
        // can actually fire. Pre-fix (ZEB-623 I-1) the initiator keyed this by the
        // tunnel `peer_node_id`, which never matches the reader's key: this test
        // fails against that code (the `is_some()` on the join key would be false,
        // and the `is_none()` on the tunnel id would be false).
        assert!(
            init_mgr
                .compat_registry()
                .incompat_reason(&iroh_join_key)
                .is_some(),
            "an incompatible peer hello must be recorded under the peer's IROH \
             EndpointId (the Network Health join key)"
        );
        assert!(
            init_mgr
                .compat_registry()
                .incompat_reason(&peer_node_id)
                .is_none(),
            "the incompat record must NOT be keyed by the tunnel node id \
             (blake3 of the ML-DSA pubkey) — that key never joins Network Health"
        );

        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), fake_responder).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incompatible_hello_inbound_is_recorded_by_responder() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            incompatible_hello_inbound_inner(),
        )
        .await
        .expect("responder incompatible-hello recording must complete within 30s");
    }

    /// ZEB-623 (PR #395 review): mirror of `incompatible_hello_inner` for the
    /// RESPONDER path. A scripted dialer connects `/v2`, opens the bi-stream, and
    /// sends a hello advertising protocol_version 0 (below MIN). The real
    /// `run_tunnel_responder` must reject it AND record the incompatibility under
    /// the DIALER's authenticated iroh EndpointId (`conn.remote_id()`) — the key
    /// Network Health joins on. Pins the new responder-side registry write.
    async fn incompatible_hello_inbound_inner() {
        use iroh::{EndpointAddr, TransportAddr};

        let responder_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));

        let ep_dialer = loopback_endpoint([0x71; 32]).await;
        let ep_resp = loopback_endpoint([0x72; 32]).await;
        let ep_resp_addr = EndpointAddr::from_parts(
            ep_resp.id(),
            ep_resp.bound_sockets().into_iter().map(TransportAddr::Ip),
        );

        // Real responder driver on the accepted connection, sharing a compat
        // registry we can assert on.
        let compat = Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default());
        let (resp_ingest_tx, _resp_ingest_rx) = mpsc::channel::<InboundDm>(8);
        let resp_mgr = tunnel_manager_with_compat(
            ep_resp.clone(),
            Arc::clone(&responder_pq),
            resp_ingest_tx.clone(),
            Arc::clone(&compat),
        );
        let ep_resp_accept = ep_resp.clone();
        let responder = tokio::spawn(async move {
            let incoming = ep_resp_accept.accept().await.expect("incoming");
            let conn = incoming.await.expect("connection established");
            run_tunnel_responder(conn, responder_pq, resp_mgr, resp_ingest_tx).await;
        });

        // Scripted dialer: connect `/v2`, open_bi, send a low hello. The responder
        // reads + gates the hello FIRST, so it rejects before ever reading a
        // TunnelInit — the dialer never needs to send one.
        let dialer_iroh_id = *ep_dialer.id().as_bytes();
        let conn = ep_dialer
            .connect(ep_resp_addr, crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V2)
            .await
            .expect("dialer connect v2");
        let (mut send, _recv) = conn.open_bi().await.expect("open_bi");
        let low_hello =
            crate::protocol_versioning::encode_hello(&crate::protocol_versioning::TunnelHello {
                protocol_version: 0,
                capabilities: 0,
            })
            .expect("encode low hello");
        write_length_prefixed(&mut send, &low_hello)
            .await
            .expect("write low hello");

        // The responder must record the incompat under the DIALER's iroh
        // EndpointId within a few seconds (poll; the responder runs concurrently).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if compat.incompat_reason(&dialer_iroh_id).is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "responder must record an incompatible inbound peer under its \
                 authenticated iroh EndpointId (the Network Health join key)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), responder).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_process_handshake_round_trips_dm_both_directions() {
        // iroh stack size: PQ handshake messages are large; give the tokio test
        // runtime real worker threads (default stack) and a wall-clock cap.
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(std::time::Duration::from_secs(30), handshake_inner())
            .await
            .expect("in-process tunnel handshake must complete within 30s");
    }

    async fn handshake_inner() {
        use iroh::{EndpointAddr, TransportAddr};

        let initiator_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));
        let responder_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));

        // Loopback endpoints: ep_a dials (initiator), ep_b accepts (responder).
        let ep_a = loopback_endpoint([0x11; 32]).await;
        let ep_b = loopback_endpoint([0x22; 32]).await;
        let ep_b_id = ep_b.id();
        let ep_b_addr = EndpointAddr::from_parts(
            ep_b_id,
            ep_b.bound_sockets().into_iter().map(TransportAddr::Ip),
        );

        // Responder-side TunnelManager + ingest channel. The manager's endpoint
        // is only used for OUTBOUND dials (never triggered on the responder
        // side here), so wrapping ep_b is fine.
        let (resp_ingest_tx, mut resp_ingest_rx) = mpsc::channel::<InboundDm>(8);
        let resp_mgr = Arc::new(TunnelManager::new(
            crate::iroh_endpoint::IrohEndpoint::from_endpoint_for_test(ep_b.clone()),
            Arc::clone(&responder_pq),
            resp_ingest_tx.clone(),
            Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default()),
        ));

        // Spawn the production responder driver on the accepted connection.
        let resp_pq = Arc::clone(&responder_pq);
        let resp_mgr_task = Arc::clone(&resp_mgr);
        let resp_ingest_task = resp_ingest_tx.clone();
        let ep_b_accept = ep_b.clone();
        let responder = tokio::spawn(async move {
            let incoming = ep_b_accept
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("connection established");
            run_tunnel_responder(incoming, resp_pq, resp_mgr_task, resp_ingest_task).await;
        });

        // Initiator side: dial, handshake, then run the production loop. We
        // construct the session via `new_initiator` against the responder's REAL
        // PQ identity (so the responder's identity check passes), and dial the
        // loopback EndpointAddr directly (the `dial_addr` helper omits direct IPs,
        // which loopback-with-relays-disabled needs — covered by its own unit
        // test). This exercises the same handshake + `run_tunnel_loop` the
        // production initiator runs.
        let conn = ep_a
            .connect(ep_b_addr, crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1)
            .await
            .expect("connect");
        let (mut send_stream, mut recv_stream) = conn.open_bi().await.expect("open_bi");

        let mut rng = rand::rngs::OsRng;
        let (mut init_session, init_actions) =
            harmony_tunnel::session::TunnelSession::new_initiator(
                &mut rng,
                &initiator_pq,
                responder_pq.public_identity(),
                millis_since_start(),
            )
            .expect("new_initiator");
        for action in init_actions {
            if let TunnelAction::OutboundBytes { data } = action {
                write_length_prefixed(&mut send_stream, &data)
                    .await
                    .expect("write TunnelInit");
            }
        }
        let accept_bytes = read_length_prefixed(&mut recv_stream, HANDSHAKE_MAX_MESSAGE)
            .await
            .expect("read TunnelAccept");
        init_session
            .handle_event(TunnelEvent::InboundBytes {
                data: accept_bytes,
                now_ms: millis_since_start(),
            })
            .expect("handle TunnelAccept");
        assert_eq!(init_session.state(), TunnelState::Active);

        // Initiator NodeId for the responder→initiator assertion (the responder
        // registered the session under the initiator's NodeId).
        let init_node_id =
            node_id_from_dsa_pubkey(&initiator_pq.public_identity().verifying_key.as_bytes());

        // Drive the initiator loop via a command channel + its own ingest seam.
        let (init_cmd_tx, init_cmd_rx) = mpsc::channel::<TunnelCommand>(8);
        let (init_ingest_tx, mut init_ingest_rx) = mpsc::channel::<InboundDm>(8);
        let resp_node_id =
            node_id_from_dsa_pubkey(&responder_pq.public_identity().verifying_key.as_bytes());
        // Initiator-side manager so `run_tunnel_loop` can `note_closed` on exit.
        let init_mgr = Arc::new(TunnelManager::new(
            crate::iroh_endpoint::IrohEndpoint::from_endpoint_for_test(ep_a.clone()),
            Arc::clone(&initiator_pq),
            init_ingest_tx.clone(),
            Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default()),
        ));
        let initiator_loop = tokio::spawn(async move {
            run_tunnel_loop(
                init_session,
                send_stream,
                recv_stream,
                resp_node_id,
                init_mgr,
                0,
                init_ingest_tx,
                init_cmd_rx,
            )
            .await;
        });

        // (1) initiator → responder: send a Dm; assert byte-identical receipt.
        let dm_payload = b"sealed+signed-dm-bytes-A".to_vec();
        init_cmd_tx
            .send(TunnelCommand::SendDm(dm_payload.clone()))
            .await
            .expect("queue SendDm");
        let received =
            tokio::time::timeout(std::time::Duration::from_secs(10), resp_ingest_rx.recv())
                .await
                .expect("responder ingest within 10s")
                .expect("responder ingest payload");
        assert_eq!(
            received.payload, dm_payload,
            "responder must receive the byte-identical DM payload"
        );
        assert_eq!(
            received.peer_node_id, init_node_id,
            "responder must attribute the DM to the initiator's NodeId"
        );

        // (2) responder → initiator: the responder loop registered the session in
        // `resp_mgr` under the initiator's NodeId; send a DM back over it.
        let dm_back = b"sealed+signed-dm-bytes-B".to_vec();
        {
            // The responder loop owns the cmd_rx returned by register_inbound; the
            // manager holds the cmd_tx. Drive it through the manager's send path
            // by locating the handle and sending on its cmd_tx.
            send_dm_via_manager_handle(&resp_mgr, init_node_id, dm_back.clone());
        }
        let back = tokio::time::timeout(std::time::Duration::from_secs(10), init_ingest_rx.recv())
            .await
            .expect("initiator ingest within 10s")
            .expect("initiator ingest payload");
        assert_eq!(
            back.payload, dm_back,
            "initiator must receive the byte-identical reply DM payload"
        );

        // Tear down: closing the initiator command channel ends its loop; the
        // responder loop ends when the stream closes.
        drop(init_cmd_tx);
        let _ = initiator_loop.await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), responder).await;
    }

    /// Test helper: route a DM through a responder-registered handle by reusing
    /// the manager's public `register_inbound`-installed `cmd_tx`. We expose a
    /// thin test-only accessor on the manager rather than reaching into private
    /// fields from another module.
    fn send_dm_via_manager_handle(
        mgr: &Arc<TunnelManager>,
        peer_node_id: [u8; 32],
        packet: Vec<u8>,
    ) {
        mgr.test_send_over_handle(peer_node_id, packet);
    }

    // ── ZEB-473 Task 9: tunnel → drain → ingest, end to end ──────────────────
    //
    // A REAL loopback PQ tunnel (initiator → responder) carries one
    // `FrameTag::Dm` frame whose payload is a REAL sealed+signed CidNotify
    // packet. The responder's ingest channel feeds the PRODUCTION drain body
    // (`dm_inbox_ingest::ingest_dm_packet` against the receiver's state — the
    // SAME call `start_node`'s boot drain makes). We assert the tunnel-delivered
    // DM lands in the inbox CRDT and fires exactly one `dm-received` event —
    // i.e. the inbound tunnel path delivers a DM identically to the deposit path.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tunnel_delivered_dm_ingests_end_to_end() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(std::time::Duration::from_secs(30), tunnel_ingest_inner())
            .await
            .expect("tunnel→ingest end-to-end must complete within 30s");
    }

    async fn tunnel_ingest_inner() {
        use crate::dm_inbox_ingest::ingest_dm_packet;
        use crate::dm_inbox_ingest::test_fixture::build_dm_ingest_fixture;
        use iroh::{EndpointAddr, TransportAddr};

        // Receive-side (Bob) fixture + the real signed packet Alice carries.
        let fx = build_dm_ingest_fixture(b"tunnel-delivered DM body").await;

        let initiator_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));
        let responder_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));

        let ep_a = loopback_endpoint([0x31; 32]).await;
        let ep_b = loopback_endpoint([0x32; 32]).await;
        let ep_b_addr = EndpointAddr::from_parts(
            ep_b.id(),
            ep_b.bound_sockets().into_iter().map(TransportAddr::Ip),
        );

        // Responder-side TunnelManager + ingest channel.
        let (resp_ingest_tx, mut resp_ingest_rx) = mpsc::channel::<InboundDm>(8);
        let resp_mgr = Arc::new(TunnelManager::new(
            crate::iroh_endpoint::IrohEndpoint::from_endpoint_for_test(ep_b.clone()),
            Arc::clone(&responder_pq),
            resp_ingest_tx.clone(),
            Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default()),
        ));

        // The PRODUCTION drain: each InboundDm runs through `ingest_dm_packet`
        // against Bob's state — exactly the boot wiring in `start_node`.
        let drain_state = Arc::clone(&fx.crdt_state);
        let drain_cas = Arc::clone(&fx.content_store);
        let drain_sink = Arc::clone(&fx.sink);
        let drain_device = fx.bob_device_id.clone();
        let drain_self_owner = fx.bob;
        let drain = tokio::spawn(async move {
            while let Some(dm) = resp_ingest_rx.recv().await {
                let _ = ingest_dm_packet(
                    &drain_state,
                    &drain_cas,
                    &drain_sink,
                    drain_self_owner,
                    &drain_device,
                    dm.peer_node_id,
                    &dm.payload,
                )
                .await;
            }
        });

        // Spawn the production responder driver on the accepted connection.
        let resp_pq = Arc::clone(&responder_pq);
        let resp_mgr_task = Arc::clone(&resp_mgr);
        let resp_ingest_task = resp_ingest_tx.clone();
        let ep_b_accept = ep_b.clone();
        let responder = tokio::spawn(async move {
            let incoming = ep_b_accept
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("connection established");
            run_tunnel_responder(incoming, resp_pq, resp_mgr_task, resp_ingest_task).await;
        });

        // Initiator: dial + handshake to Active, then run the production loop.
        let conn = ep_a
            .connect(ep_b_addr, crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1)
            .await
            .expect("connect");
        let (mut send_stream, mut recv_stream) = conn.open_bi().await.expect("open_bi");
        let mut rng = rand::rngs::OsRng;
        let (mut init_session, init_actions) =
            harmony_tunnel::session::TunnelSession::new_initiator(
                &mut rng,
                &initiator_pq,
                responder_pq.public_identity(),
                millis_since_start(),
            )
            .expect("new_initiator");
        for action in init_actions {
            if let TunnelAction::OutboundBytes { data } = action {
                write_length_prefixed(&mut send_stream, &data)
                    .await
                    .expect("write TunnelInit");
            }
        }
        let accept_bytes = read_length_prefixed(&mut recv_stream, HANDSHAKE_MAX_MESSAGE)
            .await
            .expect("read TunnelAccept");
        init_session
            .handle_event(TunnelEvent::InboundBytes {
                data: accept_bytes,
                now_ms: millis_since_start(),
            })
            .expect("handle TunnelAccept");
        assert_eq!(init_session.state(), TunnelState::Active);

        let (init_cmd_tx, init_cmd_rx) = mpsc::channel::<TunnelCommand>(8);
        let (init_ingest_tx, _init_ingest_rx) = mpsc::channel::<InboundDm>(8);
        let resp_node_id =
            node_id_from_dsa_pubkey(&responder_pq.public_identity().verifying_key.as_bytes());
        // Initiator-side manager so `run_tunnel_loop` can `note_closed` on exit.
        let init_mgr = Arc::new(TunnelManager::new(
            crate::iroh_endpoint::IrohEndpoint::from_endpoint_for_test(ep_a.clone()),
            Arc::clone(&initiator_pq),
            init_ingest_tx.clone(),
            Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default()),
        ));
        let initiator_loop = tokio::spawn(async move {
            run_tunnel_loop(
                init_session,
                send_stream,
                recv_stream,
                resp_node_id,
                init_mgr,
                0,
                init_ingest_tx,
                init_cmd_rx,
            )
            .await;
        });

        // Carry Alice's real signed packet over the tunnel as a Dm frame.
        init_cmd_tx
            .send(TunnelCommand::SendDm(fx.packet.clone()))
            .await
            .expect("queue SendDm");

        // The drain ingests it: the inbox entry appears (poll on the CRDT).
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id: fx.space_id,
            message_cid: fx.message_cid,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let has_entry = fx.crdt_state.lock().await.inbox.contains_key(&inbox_key);
            if has_entry {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "tunnel-delivered DM did not reach the inbox in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        // ...and exactly one dm-received event fired (the shared UI event).
        let dm_emits = fx
            .sink_handle
            .frames()
            .iter()
            .filter(|(n, _)| n == crate::dm_outbox::DM_RECEIVED_EVENT)
            .count();
        assert_eq!(dm_emits, 1, "tunnel DM delivers exactly one dm-received");

        // Teardown.
        drop(init_cmd_tx);
        let _ = initiator_loop.await;
        drop(resp_ingest_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), responder).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), drain).await;
    }

    /// G2 (ZEB-473): a `DmReceived` that arrives in the SAME batch as a terminal
    /// `Error` must STILL be forwarded to the ingest seam before the dispatch
    /// returns `false` (loop-exit). The prior two-pass code bailed in Pass 1 and
    /// skipped Pass 2 entirely, silently dropping a fully-received DM.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dm_received_before_error_in_same_batch_is_still_forwarded() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;

        // A real loopback bi-stream gives us a live `SendStream` to satisfy the
        // signature; this test only drives the dispatch's action handling.
        let ep_a = loopback_endpoint([0x31; 32]).await;
        let ep_b = loopback_endpoint([0x32; 32]).await;
        let ep_b_id = ep_b.id();
        let ep_b_addr = iroh::EndpointAddr::from_parts(
            ep_b_id,
            ep_b.bound_sockets()
                .into_iter()
                .map(iroh::TransportAddr::Ip),
        );
        // Keep the responder side alive so the stream stays open.
        let ep_b_accept = ep_b.clone();
        let acceptor = tokio::spawn(async move {
            if let Some(incoming) = ep_b_accept.accept().await {
                let _conn = incoming.await;
                // Hold the connection open until the test drops the dialer.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
        let conn = ep_a
            .connect(ep_b_addr, crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1)
            .await
            .expect("connect");
        let (mut send_stream, _recv_stream) = conn.open_bi().await.expect("open_bi");

        let peer_node_id = [0xAB; 32];
        let (ingest_tx, mut ingest_rx) = mpsc::channel::<InboundDm>(8);

        // The load-bearing batch: a received DM FOLLOWED by a terminal Error.
        let dm_payload = b"the-dm-that-must-not-be-dropped".to_vec();
        let actions = vec![
            TunnelAction::DmReceived {
                payload: dm_payload.clone(),
            },
            TunnelAction::Error {
                reason: "peer closed right after delivering a DM".to_string(),
            },
        ];

        let keep_going =
            dispatch_tunnel_actions(&actions, &mut send_stream, peer_node_id, &ingest_tx).await;

        // The dispatch reports loop-exit (terminal Error honored)...
        assert!(
            !keep_going,
            "a terminal Error in the batch must still exit the loop"
        );
        // ...but the DM was forwarded to ingest FIRST (not dropped).
        let got = ingest_rx
            .try_recv()
            .expect("the DmReceived must reach ingest even though an Error followed it");
        assert_eq!(got.peer_node_id, peer_node_id);
        assert_eq!(got.payload, dm_payload);
        assert!(
            ingest_rx.try_recv().is_err(),
            "exactly one DM should have been forwarded"
        );

        drop(conn);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), acceptor).await;
    }
}
