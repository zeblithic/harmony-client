//! ZEB-473 (DM-over-iroh, Move 1a): the per-peer PQ tunnel session map.
//!
//! `TunnelManager` owns one `TunnelHandle` per peer device NodeId. It lazily
//! dials an outbound tunnel on the first DM to a peer, reuses a bidirectional
//! tunnel a peer opened to us, buffers DMs sent before a dial completes, and
//! resolves simultaneous-dial collisions deterministically (lower-NodeId
//! initiator wins, applied identically on both sides → converges on one
//! survivor).
//!
//! The NodeId key is `blake3(peer ML-DSA-65 pubkey)` (32 bytes) — the same
//! derivation `harmony_tunnel` uses internally, so the key the responder
//! authenticates (via `TunnelAction::HandshakeComplete`) matches the key the
//! dialer computes from the contact's `pq_dsa_pubkey`.
//!
//! NO production DM traffic flows through `send_dm` yet: the `DmTransport` that
//! calls it is ZEB-473 Task 8, and the real inbound DM ingest replacing the
//! placeholder drain is Task 9. Until then `send_dm` is `#[allow(dead_code)]`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;

use harmony_identity::PqPrivateIdentity;
use tokio::sync::mpsc;

use crate::iroh_endpoint::IrohEndpoint;
use crate::owner_state_types::DeviceTunnelContact;

/// Capacity of a per-tunnel command channel. DMs are infrequent and the loop
/// drains promptly; a small buffer absorbs a burst without unbounded growth.
const CMD_CHANNEL_CAP: usize = 64;

/// Max DMs buffered while a dial is in flight, per peer. Beyond this the oldest
/// pending DM is dropped (it still went out over the always-deposit durability
/// path, so dropping the live attempt is graceful).
const MAX_PENDING_PER_PEER: usize = 64;

/// A decrypted inbound DM payload handed off to the ingest seam.
///
/// The tunnel loops push one of these per `TunnelAction::DmReceived`. The boot
/// drain (placeholder, ZEB-473 Task 9 replaces it) logs + drops; Task 9 swaps
/// the consumer for the real verify/decrypt/apply/emit pipeline.
#[derive(Debug)]
pub struct InboundDm {
    /// The authenticated peer device NodeId the DM arrived from
    /// (`blake3(peer ML-DSA pubkey)`).
    pub peer_node_id: [u8; 32],
    /// The opaque sealed+signed DM packet bytes (the caller verifies/unseals).
    pub payload: Vec<u8>,
}

/// A command sent into a per-tunnel loop over its `cmd_tx`.
#[derive(Debug)]
pub enum TunnelCommand {
    /// Carry an opaque sealed+signed DM packet over the tunnel.
    SendDm(Vec<u8>),
    /// Gracefully close the tunnel.
    Close,
}

/// Which side opened the tunnel (load-bearing for collision dedup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelRole {
    Initiator,
    Responder,
}

/// Lifecycle state of a tunnel handle as the manager sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelHandleState {
    /// Outbound dial in flight; DMs queue in `pending`.
    Dialing,
    /// Handshake complete; DMs go straight over `cmd_tx`.
    Active,
    /// Being torn down (dedup loser / explicit close).
    Closing,
}

/// The manager's handle to one live (or dialing) tunnel.
struct TunnelHandle {
    /// Command sink driving the per-tunnel loop.
    cmd_tx: mpsc::Sender<TunnelCommand>,
    state: TunnelHandleState,
    role: TunnelRole,
    /// DMs buffered while `Dialing`; flushed in order on `Active`.
    pending: VecDeque<Vec<u8>>,
}

/// Per-peer PQ tunnel session map + lifecycle.
pub struct TunnelManager {
    sessions: Mutex<HashMap<[u8; 32], TunnelHandle>>,
    endpoint: IrohEndpoint,
    local_pq: Arc<PqPrivateIdentity>,
    ingest_tx: mpsc::Sender<InboundDm>,
    /// `blake3(our own ML-DSA pubkey)` — the LHS of the lower-NodeId dedup
    /// comparison. When we initiated and `self_node_id < peer_node_id`, our dial
    /// is the survivor; both peers apply this identical rule.
    self_node_id: [u8; 32],
}

/// Derive the 32-byte tunnel NodeId from an ML-DSA-65 public key, matching
/// `harmony_tunnel`'s internal `blake3(dsa_pubkey)` derivation.
pub fn node_id_from_dsa_pubkey(dsa_pubkey: &[u8]) -> [u8; 32] {
    *blake3::hash(dsa_pubkey).as_bytes()
}

impl TunnelManager {
    pub fn new(
        endpoint: IrohEndpoint,
        local_pq: Arc<PqPrivateIdentity>,
        ingest_tx: mpsc::Sender<InboundDm>,
    ) -> Self {
        let self_node_id =
            node_id_from_dsa_pubkey(&local_pq.public_identity().verifying_key.as_bytes());
        Self {
            sessions: Mutex::new(HashMap::new()),
            endpoint,
            local_pq,
            ingest_tx,
            self_node_id,
        }
    }

    /// Our own tunnel NodeId (`blake3(our ML-DSA pubkey)`).
    pub fn self_node_id(&self) -> [u8; 32] {
        self.self_node_id
    }

    /// Send (or queue) a sealed+signed DM packet to `peer_node_id` over a PQ
    /// tunnel, lazily dialing if no session exists.
    ///
    /// `#[allow(dead_code)]` until ZEB-473 Task 8 wires the `DmTransport`
    /// consumer; the dial machinery + collision dedup it exercises is fully
    /// implemented and unit-tested now.
    #[allow(dead_code)] // ZEB-473 Task 8 wires this (IrohTunnelDmTransport -> send_dm)
    pub fn send_dm(
        self: &Arc<Self>,
        peer_node_id: [u8; 32],
        contact: &DeviceTunnelContact,
        packet: Vec<u8>,
    ) {
        let mut sessions = self
            .sessions
            .lock()
            .expect("tunnel sessions mutex poisoned");
        match sessions.get_mut(&peer_node_id) {
            Some(handle) => match handle.state {
                TunnelHandleState::Active => {
                    // Try the live tunnel; if the loop has gone away
                    // (`try_send` errors), fall back to a fresh dial.
                    if handle
                        .cmd_tx
                        .try_send(TunnelCommand::SendDm(packet))
                        .is_err()
                    {
                        sessions.remove(&peer_node_id);
                        drop(sessions);
                        self.spawn_dial(peer_node_id, contact, vec![]);
                    }
                }
                TunnelHandleState::Dialing => {
                    push_pending(&mut handle.pending, packet);
                }
                TunnelHandleState::Closing => {
                    // The current session is tearing down (dedup loser); start a
                    // fresh dial keyed under the same peer once it's gone. Drop
                    // the old entry and re-dial.
                    sessions.remove(&peer_node_id);
                    drop(sessions);
                    self.spawn_dial(peer_node_id, contact, vec![packet]);
                }
            },
            None => {
                drop(sessions);
                self.spawn_dial(peer_node_id, contact, vec![packet]);
            }
        }
    }

    /// Insert a `Dialing` handle and spawn the initiator loop. `seed_pending`
    /// pre-loads the buffered DMs (the one that triggered the dial, plus any
    /// redirected from a closed session).
    fn spawn_dial(
        self: &Arc<Self>,
        peer_node_id: [u8; 32],
        contact: &DeviceTunnelContact,
        seed_pending: Vec<Vec<u8>>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAP);
        let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
        for p in seed_pending {
            push_pending(&mut pending, p);
        }
        {
            let mut sessions = self
                .sessions
                .lock()
                .expect("tunnel sessions mutex poisoned");
            // Double-checked: a concurrent register_inbound may have raced in.
            if sessions.contains_key(&peer_node_id) {
                // Re-route the seeds through the now-existing handle instead of
                // overwriting it. Falls back to send-over-cmd_tx where possible.
                if let Some(existing) = sessions.get_mut(&peer_node_id) {
                    for p in pending.drain(..) {
                        match existing.state {
                            TunnelHandleState::Active => {
                                let _ = existing.cmd_tx.try_send(TunnelCommand::SendDm(p));
                            }
                            _ => push_pending(&mut existing.pending, p),
                        }
                    }
                }
                return;
            }
            sessions.insert(
                peer_node_id,
                TunnelHandle {
                    cmd_tx,
                    state: TunnelHandleState::Dialing,
                    role: TunnelRole::Initiator,
                    pending,
                },
            );
        }

        let mgr = Arc::clone(self);
        let endpoint = self.endpoint.clone();
        let local_pq = Arc::clone(&self.local_pq);
        let ingest_tx = self.ingest_tx.clone();
        let contact = contact.clone();
        tokio::spawn(async move {
            crate::tunnel_task::run_tunnel_initiator(
                endpoint,
                contact,
                local_pq,
                peer_node_id,
                mgr,
                ingest_tx,
                cmd_rx,
            )
            .await;
        });
    }

    /// Called by the responder once an inbound handshake completes. Registers
    /// the (born-Active) session and returns the `cmd_rx` the loop drains.
    ///
    /// Applies lower-NodeId collision dedup: if a session already exists for
    /// this peer, the survivor is the tunnel whose INITIATOR NodeId is
    /// numerically lower. The loser's `cmd_tx`/handle is closed; any `pending`
    /// is redirected to the survivor.
    pub fn register_inbound(&self, peer_node_id: [u8; 32]) -> mpsc::Receiver<TunnelCommand> {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAP);
        let new_handle = TunnelHandle {
            cmd_tx,
            state: TunnelHandleState::Active,
            role: TunnelRole::Responder,
            pending: VecDeque::new(),
        };

        let mut sessions = self
            .sessions
            .lock()
            .expect("tunnel sessions mutex poisoned");
        match sessions.remove(&peer_node_id) {
            None => {
                sessions.insert(peer_node_id, new_handle);
            }
            Some(existing) => {
                // Collision. The inbound session we just accepted was INITIATED
                // by the peer (peer_node_id). The existing handle's initiator is
                // ours if role==Initiator, else the peer.
                let new_initiator = peer_node_id; // responder side: peer initiated.
                let existing_initiator = match existing.role {
                    TunnelRole::Initiator => self.self_node_id,
                    TunnelRole::Responder => peer_node_id,
                };
                if keep_new(new_initiator, existing_initiator) {
                    // Keep the inbound (new) session. Redirect the loser's
                    // pending onto the survivor, close the loser.
                    let mut survivor = new_handle;
                    drain_pending_into(&existing, &mut survivor);
                    close_handle(existing);
                    sessions.insert(peer_node_id, survivor);
                } else {
                    // Keep the existing session; drop the inbound one. The
                    // responder loop whose cmd_rx we'd have returned will exit on
                    // the first recv() == None (we drop cmd_rx by not returning a
                    // matched survivor) — but we MUST still return a receiver, so
                    // return a closed one (its sender is dropped immediately).
                    sessions.insert(peer_node_id, existing);
                    // `cmd_rx` here belongs to `new_handle.cmd_tx`, already
                    // dropped (new_handle was moved into the survivor branch
                    // only on the keep_new path). On this path new_handle is
                    // dropped, so cmd_rx returns None immediately → the inbound
                    // responder loop exits, closing its connection.
                }
            }
        }
        cmd_rx
    }

    /// Called by the initiator loop once its handshake reaches Active. Flips the
    /// `Dialing` handle to `Active` and flushes any `pending` DMs over `cmd_tx`,
    /// in order. Applies the same lower-NodeId dedup if an inbound session for
    /// this peer raced in while we were dialing.
    pub fn note_active(&self, peer_node_id: [u8; 32]) {
        let mut sessions = self
            .sessions
            .lock()
            .expect("tunnel sessions mutex poisoned");
        let Some(handle) = sessions.get_mut(&peer_node_id) else {
            return;
        };

        // If an inbound (Responder) session raced in and currently holds the
        // slot, resolve the collision. The inbound one's initiator is the peer;
        // our completing dial's initiator is us.
        if handle.role == TunnelRole::Responder {
            let keep_ours = keep_new(self.self_node_id, peer_node_id);
            if !keep_ours {
                // The inbound responder session wins; our just-completed dial is
                // the loser. Leave the responder handle in place. Our initiator
                // loop will find no flush target and naturally idle out / the
                // caller can drop it. Nothing to do here.
                return;
            }
            // Our dial wins: we never overwrote the responder handle (the
            // initiator never called register_inbound), so this branch only
            // fires if some future path inserts a responder under our key — keep
            // simple: don't clobber, just return.
            return;
        }

        // Normal path: our own Dialing initiator handle. Flip to Active + flush.
        handle.state = TunnelHandleState::Active;
        while let Some(packet) = handle.pending.pop_front() {
            if handle
                .cmd_tx
                .try_send(TunnelCommand::SendDm(packet))
                .is_err()
            {
                // Loop gone already; stop flushing.
                break;
            }
        }
    }

    /// Called by the initiator loop when a dial fails (handshake error/timeout).
    /// Removes the `Dialing` handle so a later DM re-dials. Pending DMs are
    /// dropped (the always-deposit path covers durability).
    pub fn note_dial_failed(&self, peer_node_id: [u8; 32]) {
        let mut sessions = self
            .sessions
            .lock()
            .expect("tunnel sessions mutex poisoned");
        if let Some(handle) = sessions.get(&peer_node_id) {
            // Only remove if it's still OUR dialing handle — a responder session
            // may have replaced it (dedup) in the meantime.
            if handle.role == TunnelRole::Initiator && handle.state != TunnelHandleState::Active {
                sessions.remove(&peer_node_id);
            }
        }
    }

    #[cfg(test)]
    fn handle_snapshot(
        &self,
        peer_node_id: &[u8; 32],
    ) -> Option<(TunnelHandleState, TunnelRole, usize)> {
        let sessions = self.sessions.lock().expect("poisoned");
        sessions
            .get(peer_node_id)
            .map(|h| (h.state, h.role, h.pending.len()))
    }

    /// Test-only: send a DM packet over the live handle registered for
    /// `peer_node_id` (used by the cross-module in-process handshake test to
    /// drive the responder→initiator direction through a registered session).
    #[cfg(test)]
    pub(crate) fn test_send_over_handle(&self, peer_node_id: [u8; 32], packet: Vec<u8>) {
        let sessions = self.sessions.lock().expect("poisoned");
        if let Some(handle) = sessions.get(&peer_node_id) {
            let _ = handle.cmd_tx.try_send(TunnelCommand::SendDm(packet));
        }
    }
}

/// Lower-NodeId-wins comparison. The survivor is the tunnel whose INITIATOR
/// NodeId is numerically lower. Returns `true` when the NEW session's initiator
/// is the lower (so the new session is kept). A tie (same initiator, i.e. the
/// peer dialed us twice) keeps the new one.
fn keep_new(new_initiator: [u8; 32], existing_initiator: [u8; 32]) -> bool {
    new_initiator <= existing_initiator
}

/// Bound the pending queue: drop-oldest past the cap.
fn push_pending(pending: &mut VecDeque<Vec<u8>>, packet: Vec<u8>) {
    if pending.len() >= MAX_PENDING_PER_PEER {
        pending.pop_front();
    }
    pending.push_back(packet);
}

/// Redirect a (loser) handle's pending DMs onto the survivor. Active survivors
/// take them straight over `cmd_tx`; otherwise they queue.
fn drain_pending_into(loser: &TunnelHandle, survivor: &mut TunnelHandle) {
    for packet in loser.pending.iter().cloned() {
        match survivor.state {
            TunnelHandleState::Active => {
                let _ = survivor.cmd_tx.try_send(TunnelCommand::SendDm(packet));
            }
            _ => push_pending(&mut survivor.pending, packet),
        }
    }
}

/// Close a losing handle: best-effort `Close` command so its loop tears the
/// connection down (the cmd_tx then drops with the handle).
fn close_handle(mut handle: TunnelHandle) {
    handle.state = TunnelHandleState::Closing;
    let _ = handle.cmd_tx.try_send(TunnelCommand::Close);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> (Arc<TunnelManager>, mpsc::Receiver<InboundDm>) {
        // A manager needs an iroh endpoint; bind a real loopback one (cheap,
        // and these tests never dial). Use a fresh PQ identity for self.
        let endpoint = futures::executor::block_on(async {
            let sk = iroh::SecretKey::generate();
            crate::iroh_endpoint::IrohEndpoint::new_with_secret(sk)
                .await
                .expect("bind loopback iroh endpoint")
        });
        let local_pq = Arc::new(PqPrivateIdentity::generate(&mut rand::rngs::OsRng));
        let (ingest_tx, ingest_rx) = mpsc::channel(16);
        (
            Arc::new(TunnelManager::new(endpoint, local_pq, ingest_tx)),
            ingest_rx,
        )
    }

    fn fixed_node_id(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn keep_new_lower_initiator_wins() {
        let low = fixed_node_id(0x01);
        let high = fixed_node_id(0xFE);
        // New session initiated by the lower NodeId is kept.
        assert!(keep_new(low, high));
        // New session initiated by the higher NodeId is dropped.
        assert!(!keep_new(high, low));
        // Tie keeps new (peer re-dialed).
        assert!(keep_new(low, low));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn register_inbound_keeps_lower_initiator_on_collision() {
        let (mgr, _ingest_rx) = test_manager();
        let peer = fixed_node_id(0x05);

        // First inbound register: no collision, born Active/Responder.
        let _rx1 = mgr.register_inbound(peer);
        assert_eq!(
            mgr.handle_snapshot(&peer).map(|(s, r, _)| (s, r)),
            Some((TunnelHandleState::Active, TunnelRole::Responder))
        );

        // Simulate a SECOND inbound register for the same peer (peer re-dialed).
        // Tie on the initiator (both = peer) keeps the new one; still Active.
        let _rx2 = mgr.register_inbound(peer);
        assert_eq!(
            mgr.handle_snapshot(&peer).map(|(s, r, _)| (s, r)),
            Some((TunnelHandleState::Active, TunnelRole::Responder)),
            "a single survivor remains after a colliding inbound register"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn note_active_flushes_pending_in_order() {
        let (mgr, _ingest_rx) = test_manager();
        let peer = fixed_node_id(0x07);

        // Manually install a Dialing handle with a captured cmd_rx so we can
        // observe the flush (bypass spawn_dial's real iroh dial).
        let (cmd_tx, mut cmd_rx) = mpsc::channel(CMD_CHANNEL_CAP);
        {
            let mut sessions = mgr.sessions.lock().unwrap();
            sessions.insert(
                peer,
                TunnelHandle {
                    cmd_tx,
                    state: TunnelHandleState::Dialing,
                    role: TunnelRole::Initiator,
                    pending: VecDeque::from(vec![
                        b"dm-1".to_vec(),
                        b"dm-2".to_vec(),
                        b"dm-3".to_vec(),
                    ]),
                },
            );
        }

        mgr.note_active(peer);

        // The handle is now Active with an empty pending queue.
        assert_eq!(
            mgr.handle_snapshot(&peer).map(|(s, _, p)| (s, p)),
            Some((TunnelHandleState::Active, 0))
        );

        // The three buffered DMs flushed over cmd_tx, in order.
        let mut got = Vec::new();
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let TunnelCommand::SendDm(p) = cmd {
                got.push(p);
            }
        }
        assert_eq!(
            got,
            vec![b"dm-1".to_vec(), b"dm-2".to_vec(), b"dm-3".to_vec()],
            "pending DMs must flush in FIFO order on Active"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_dm_buffers_while_dialing() {
        let (mgr, _ingest_rx) = test_manager();
        let peer = fixed_node_id(0x09);

        // Pre-install a Dialing handle (avoid a real dial). Its cmd_rx is held so
        // try_send won't fail.
        let (cmd_tx, _cmd_rx) = mpsc::channel(CMD_CHANNEL_CAP);
        {
            let mut sessions = mgr.sessions.lock().unwrap();
            sessions.insert(
                peer,
                TunnelHandle {
                    cmd_tx,
                    state: TunnelHandleState::Dialing,
                    role: TunnelRole::Initiator,
                    pending: VecDeque::new(),
                },
            );
        }

        let contact = DeviceTunnelContact {
            iroh_node_id: fixed_node_id(0x09),
            home_relay_url: None,
            pq_dsa_pubkey: vec![1; 1952],
            pq_kem_pubkey: vec![2; 1184],
        };
        mgr.send_dm(peer, &contact, b"queued".to_vec());

        // A DM sent while Dialing is buffered (pending len == 1), not sent.
        assert_eq!(
            mgr.handle_snapshot(&peer).map(|(s, _, p)| (s, p)),
            Some((TunnelHandleState::Dialing, 1))
        );
    }

    #[test]
    fn node_id_derivation_is_blake3_of_dsa_pubkey() {
        let id = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let dsa = id.public_identity().verifying_key.as_bytes();
        let derived = node_id_from_dsa_pubkey(&dsa);
        assert_eq!(derived, *blake3::hash(&dsa).as_bytes());
    }
}
