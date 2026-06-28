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
        Ok(Err(reason)) => {
            tracing::debug!(%reason, "ZEB-473: inbound tunnel handshake failed");
            return;
        }
        Err(_) => {
            tracing::debug!("ZEB-473: inbound tunnel handshake timed out");
            return;
        }
    };

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

/// Responder handshake: accept the bi-stream, read `TunnelInit`, build the
/// session, write `TunnelAccept`. Returns the session + streams + the peer's
/// authenticated NodeId.
async fn responder_handshake(
    conn: &Connection,
    local_pq: &PqPrivateIdentity,
) -> Result<(TunnelSession, SendStream, RecvStream, [u8; 32]), String> {
    let (mut send_stream, mut recv_stream) = conn
        .accept_bi()
        .await
        .map_err(|e| format!("accept_bi: {e}"))?;

    let init_bytes = read_length_prefixed(&mut recv_stream, HANDSHAKE_MAX_MESSAGE)
        .await
        .map_err(|e| format!("read TunnelInit: {e}"))?;

    let mut rng = rand::rngs::OsRng;
    let now_ms = millis_since_start();
    let (session, actions) = TunnelSession::new_responder(&mut rng, local_pq, &init_bytes, now_ms)
        .map_err(|e| format!("new_responder: {e}"))?;

    // Extract the authenticated peer NodeId from the HandshakeComplete action,
    // and write the TunnelAccept before returning (so the bytes are on the wire
    // before we register the session).
    let mut peer_node_id = None;
    for action in &actions {
        match action {
            TunnelAction::OutboundBytes { data } => {
                write_length_prefixed(&mut send_stream, data)
                    .await
                    .map_err(|e| format!("write TunnelAccept: {e}"))?;
            }
            TunnelAction::HandshakeComplete {
                peer_node_id: id, ..
            } => {
                peer_node_id = Some(*id);
            }
            _ => {}
        }
    }

    let peer_node_id = peer_node_id.ok_or("responder handshake produced no peer NodeId")?;
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
    let handshake = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        initiator_handshake(&endpoint, &contact, &local_pq),
    )
    .await;

    let (session, send_stream, recv_stream) = match handshake {
        Ok(Ok(v)) => v,
        Ok(Err(reason)) => {
            tracing::debug!(%reason, "ZEB-473: outbound tunnel handshake failed");
            // Tell the manager the dial failed so it can drop the Dialing handle
            // (the pending DMs fall back to the always-deposit durability path).
            // Pass our epoch so a newer session that replaced us isn't evicted.
            mgr.note_dial_failed(peer_node_id, epoch);
            return;
        }
        Err(_) => {
            tracing::debug!("ZEB-473: outbound tunnel handshake timed out");
            mgr.note_dial_failed(peer_node_id, epoch);
            return;
        }
    };

    // Handshake reached Active: flip the manager handle to Active and flush any
    // DMs buffered while we were dialing (applies the lower-NodeId dedup if an
    // inbound session for this peer raced in).
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

/// Initiator handshake: build the `EndpointAddr` from the contact, dial over the
/// persistent endpoint, open a bi-stream, send `TunnelInit`, read `TunnelAccept`,
/// drive the state machine to Active. Returns the active session + streams.
async fn initiator_handshake(
    endpoint: &IrohEndpoint,
    contact: &DeviceTunnelContact,
    local_pq: &PqPrivateIdentity,
) -> Result<(TunnelSession, SendStream, RecvStream), String> {
    let addr = dial_addr(contact)?;
    let peer_pq = peer_pq_identity(contact)?;

    let conn = endpoint
        .inner()
        .connect(addr, crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let (mut send_stream, mut recv_stream) =
        conn.open_bi().await.map_err(|e| format!("open_bi: {e}"))?;

    let mut rng = rand::rngs::OsRng;
    let now_ms = millis_since_start();
    let (mut session, init_actions) =
        TunnelSession::new_initiator(&mut rng, local_pq, &peer_pq, now_ms)
            .map_err(|e| format!("new_initiator: {e}"))?;

    for action in init_actions {
        if let TunnelAction::OutboundBytes { data } = action {
            write_length_prefixed(&mut send_stream, &data)
                .await
                .map_err(|e| format!("write TunnelInit: {e}"))?;
        }
    }

    let accept_bytes = read_length_prefixed(&mut recv_stream, HANDSHAKE_MAX_MESSAGE)
        .await
        .map_err(|e| format!("read TunnelAccept: {e}"))?;
    let now_ms = millis_since_start();
    let actions = session
        .handle_event(TunnelEvent::InboundBytes {
            data: accept_bytes,
            now_ms,
        })
        .map_err(|e| format!("handle TunnelAccept: {e}"))?;

    // The accept-processing emits HandshakeComplete (and possibly nothing else);
    // there are no outbound bytes here. Confirm we reached Active.
    if session.state() != TunnelState::Active {
        return Err("initiator did not reach Active after TunnelAccept".to_string());
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
    // The write side is caller-bounded (handshake messages), so no cap here:
    // `usize::MAX` preserves the historical "always write" behavior. BE +
    // allow-empty matches this protocol's shipped wire format exactly.
    crate::iroh_framing::write_len_prefixed(
        stream,
        data,
        usize::MAX,
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

    /// Build a loopback-only iroh endpoint (no relays, no address lookup).
    async fn loopback_endpoint(secret: [u8; 32]) -> iroh::endpoint::Endpoint {
        use iroh::endpoint::{presets, Endpoint, RelayMode};
        use iroh::SecretKey;
        use std::net::Ipv4Addr;
        Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&secret))
            .alpns(vec![crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind_addr")
            .bind()
            .await
            .expect("bind loopback endpoint")
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
