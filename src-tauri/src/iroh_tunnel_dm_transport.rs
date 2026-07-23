//! Live DM carrier over the post-quantum iroh tunnel (ZEB-473 / DM-over-iroh
//! Move 1a, Task 8).
//!
//! `IrohTunnelDmTransport` is the production `DmTransport` installed on the
//! outbox drain seam (replacing the interim `DepositOnlyDmTransport`). For
//! every DM it resolves the recipient `OwnerAddr` → its cached
//! `OwnerDeviceEntry` → each bound device that advertised a
//! [`DeviceTunnelContact`], builds the **same** sealed+signed CidNotify packet
//! the deposit path builds (`dm_outbox::build_dm_packet`), and fires it through
//! [`TunnelManager::send_dm`] as a best-effort liveness attempt.
//!
//! ## Always-deposit + attempt-tunnel (spec §5.7)
//!
//! `send` always returns `Err` (never `Ok`) so the outbox drain's deposit rung
//! fires for EVERY DM. Returning `Ok` would instead steer the pair into the
//! "sent-but-never-acked" arm, which only deposits from `DEPOSIT_NOACK_WINDOWS`
//! (= 2) windows onward — strictly *later*, weakening durability. So the tunnel
//! is a pure parallel liveness path layered on top of the unchanged deposit
//! path; the recipient dedups (CRDT inbox) the tunnel copy against the deposit
//! copy.
//!
//! Which `Err` flavor matters (ZEB-525):
//!
//! * A tunnel attempt was actually spawned → `Transient`. The drain's Err-arm
//!   grants one backoff window of grace (`pre_failure_count >= 1`) before
//!   deposit candidacy — deliberately, so an online recipient's tunnel ack can
//!   complete the entry first and the deposit (butler storage + a second
//!   transfer) never fires.
//! * NO attempt was spawned (no reachable tunnel targets, or the
//!   `MAX_CONCURRENT_TUNNEL_SENDS` cap shed it) → `TransientNoLiveAttempt`.
//!   Nothing is in flight for the grace window to protect, so the drain
//!   deposits on the FIRST failure — offline-from-the-start recipients get
//!   durability one backoff window (~5-6 s) sooner.
//!
//! Inbound ingest is Task 9 (the placeholder drain on the manager's ingest
//! channel stays for now); this module only carries DMs outbound.

use std::sync::Arc;

use async_trait::async_trait;

use crate::content_store::ContentStore;
use crate::dm_outbox::{build_dm_packet, build_dm_packet_with_blob, DmTransport, TransportError};
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_types::{DeviceIdentityHash, OutboxEntry, OwnerAddr};
use crate::tunnel_manager::{node_id_from_dsa_pubkey, TunnelManager};

/// ZEB-484: inline-blob ceiling on the ASSEMBLED `CidNotifyWithBlob` packet size.
/// Comfortably below the tunnel frame cap (`DATA_MAX_MESSAGE = 2 MiB`,
/// `tunnel_task.rs`): a full 1 MiB CAS book + storage envelope + the CidNotify +
/// framing fit with ~0.5 MiB headroom, and a single book never exceeds it. Over
/// this, `send` falls back to a bare `CidNotify` and the deposit rung carries it.
pub(crate) const INLINE_BLOB_MAX: usize = 1_572_864; // 1.5 MiB

/// ZEB-485 (CodeAnt): cap on concurrent in-flight tunnel send tasks. `send`
/// SPAWNS the CAS-backed packet build + `send_dm` (to avoid the `get_local`
/// event-loop re-entrancy deadlock), so a large outbox-backlog drain would
/// otherwise spawn one unbounded detached task per DM, piling work onto CAS /
/// the event loop. Beyond this many concurrent attempts, `send` SHEDS the
/// best-effort tunnel attempt onto the always-firing deposit rung instead of
/// spawning more — bounding both task count and memory under burst.
const MAX_CONCURRENT_TUNNEL_SENDS: usize = 64;

/// Resolve a recipient owner's reachable per-device tunnel targets:
/// `(NodeId = blake3(pq_dsa_pubkey), peer)` for each bound device that advertised
/// a [`DeviceTunnelContact`](crate::owner_state_types::DeviceTunnelContact).
/// Devices with no contact yet (`None`) are skipped — the tunnel can't reach
/// them, and the deposit rung covers them.
///
/// ZEB-739: the app's `DeviceTunnelContact` is mapped into the crate-owned
/// [`TunnelPeer`](crate::tunnel_manager::TunnelPeer) here (Seam A), so the
/// `send_dm` boundary takes the already-reconstructed peer. The tunnel
/// session-map key stays `blake3(pq_dsa_pubkey)`; the `TunnelPeer` carries the
/// peer's iroh EndpointId + reconstructed PQ identity. A contact whose PQ keys
/// can't be reconstructed is skipped (unreachable over the tunnel; deposit rung
/// covers durability — same net effect as the old driver's `note_dial_failed`
/// bail).
///
/// Pure over the passed `&OwnerState` (no lock taken here); the caller holds
/// (or has snapshotted out of) the owner-state lock and must NOT hold it across
/// any subsequent `send_dm` `.await` — `send_dm` itself does not await, so a
/// short read lock spanning the resolve+send pair is fine.
pub(crate) fn resolve_owner_tunnel_targets(
    state: &OwnerState,
    recipient: OwnerAddr,
) -> Vec<([u8; 32], crate::tunnel_manager::TunnelPeer)> {
    let Some(entry) = state.owner_device_cache.devices.get(&recipient) else {
        return Vec::new();
    };
    entry
        .device_tunnel_contacts
        .iter()
        .filter_map(|maybe| maybe.as_ref())
        .filter_map(|contact| {
            let node_id = node_id_from_dsa_pubkey(&contact.pq_dsa_pubkey);
            match crate::tunnel_manager::tunnel_peer_from_contact(contact) {
                Ok(peer) => Some((node_id, peer)),
                Err(reason) => {
                    tracing::trace!(
                        %reason,
                        "ZEB-739: skip tunnel target with unmappable device contact"
                    );
                    None
                }
            }
        })
        .collect()
}

/// ZEB-482: fire `packet` (any pre-built `DmPacket` wire bytes — e.g. a signed
/// `DmInvite`) to every reachable tunnel device of `recipient`, best-effort.
///
/// Takes a SHORT `crdt_state` read lock to resolve the recipient's tunnel
/// targets, RELEASES it, and only then routes through [`TunnelManager::send_dm`]
/// (which locks the session map internally + lazily dials). Owner-state is never
/// held across the tunnel work — mirroring
/// [`IrohTunnelDmTransport::resolve_tunnel_targets`] — so a DM-space-creation
/// fan-out can't stall unrelated CRDT reads/writes (Qodo). Devices with no
/// tunnel contact are skipped (durability is the deposit rung's job — ZEB-483).
pub(crate) async fn send_packet_to_owner_tunnels(
    crdt_state: &Arc<tokio::sync::Mutex<OwnerState>>,
    mgr: &Arc<TunnelManager>,
    recipient: OwnerAddr,
    packet: &[u8],
) {
    let targets = {
        let state = crdt_state.lock().await;
        resolve_owner_tunnel_targets(&state, recipient)
    };
    for (node_id, contact) in targets {
        mgr.send_dm(node_id, &contact, packet.to_vec());
    }
}

/// Production DM transport that carries each DM through the PQ tunnel while the
/// outbox's deposit rung continues to guarantee durability. See module docs.
pub struct IrohTunnelDmTransport {
    /// Per-peer-NodeId PQ tunnel session map (lazy dial + collision dedup).
    mgr: Arc<TunnelManager>,
    /// The owner-state CRDT holding `owner_device_cache` — the recipient →
    /// device-contacts resolution source. Shared with the outbox/sync engine;
    /// the transport takes a short `lock().await` only to snapshot the
    /// recipient's contacts, then releases it before any tunnel work.
    crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
    /// Reticulum device signing key — signs the CidNotify packet (the device
    /// hash peers cache in `OwnerDeviceCache`; signing with any other key fails
    /// the receiver's `verify_dm_packet_signature`).
    signing_key: Arc<ed25519_dalek::SigningKey>,
    /// Our own owner address (the `sender_owner_addr` in the signed packet).
    self_owner: OwnerAddr,
    /// Our signing device hash (the single-device `sender_devices` + the
    /// `signing_device_hash` claim).
    our_signing_device_hash: DeviceIdentityHash,
    /// ZEB-504: this node's 64-byte device-Identity public bytes
    /// (`X25519_pub(32) || Ed25519_pub(32)`). Carried so the live-tunnel send
    /// can rebuild a bootstrap `DmInvite` from the Space record (the invite's
    /// `inviter_identity_pub` ships the inviter's pubs inline so a first-contact
    /// recipient can verify the signature before it has an `OwnerDeviceCache`
    /// entry) — mirroring the deposit rung's `build_invite_packet_bytes`.
    inviter_identity_pub: [u8; 64],
    /// ZEB-484: the local CAS, read on `send` to inline the encrypted DM blob
    /// over the tunnel for live delivery (when it fits `INLINE_BLOB_MAX`).
    cas: Arc<dyn ContentStore>,
    /// ZEB-485 (CodeAnt): bounds concurrent in-flight tunnel send tasks. See
    /// [`MAX_CONCURRENT_TUNNEL_SENDS`] — a `try_acquire` gate in `send` sheds the
    /// best-effort tunnel attempt onto the deposit rung once saturated rather
    /// than spawning unbounded background work during an outbox-backlog drain.
    tunnel_send_sem: Arc<tokio::sync::Semaphore>,
}

impl IrohTunnelDmTransport {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mgr: Arc<TunnelManager>,
        crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
        signing_key: Arc<ed25519_dalek::SigningKey>,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        inviter_identity_pub: [u8; 64],
        cas: Arc<dyn ContentStore>,
    ) -> Self {
        Self::with_tunnel_send_cap(
            mgr,
            crdt_state,
            signing_key,
            self_owner,
            our_signing_device_hash,
            inviter_identity_pub,
            cas,
            MAX_CONCURRENT_TUNNEL_SENDS,
        )
    }

    /// Construct with an explicit concurrent-tunnel-send cap. `new` uses
    /// [`MAX_CONCURRENT_TUNNEL_SENDS`]; tests use this to exercise the
    /// shed-when-saturated path with a small (or zero) cap.
    #[allow(clippy::too_many_arguments)]
    fn with_tunnel_send_cap(
        mgr: Arc<TunnelManager>,
        crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
        signing_key: Arc<ed25519_dalek::SigningKey>,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        inviter_identity_pub: [u8; 64],
        cas: Arc<dyn ContentStore>,
        tunnel_send_cap: usize,
    ) -> Self {
        Self {
            mgr,
            crdt_state,
            signing_key,
            self_owner,
            our_signing_device_hash,
            inviter_identity_pub,
            cas,
            tunnel_send_sem: Arc::new(tokio::sync::Semaphore::new(tunnel_send_cap)),
        }
    }

    /// Snapshot the recipient's reachable tunnel contacts AND (ZEB-504) the
    /// bootstrap `DmInvite` to ride ahead of the CidNotify, under ONE
    /// `crdt_state` lock so no lock is held across the tunnel `send_dm` calls.
    ///
    /// ZEB-504: rebuild the bootstrap `DmInvite` for `entry`'s Space from the
    /// durable Space record, to re-drive it over the (eventually-warm) tunnel.
    ///
    /// The first `add_space` to a new friend emits the invite fire-and-forget
    /// over a guaranteed-COLD tunnel, so it is dropped and never recovered
    /// (re-`add_space` dedupes and won't resend). To fix it, every message send
    /// re-drives the invite until the recipient acks this entry — an ack implies
    /// they admitted a CidNotify, which requires the Space to exist, so the
    /// invite is no longer needed. `apply_invite` on the receiver is an
    /// idempotent CRDT merge, so re-sending until the ack lands is safe.
    /// Returns `None` once acked, or for a non-DM Space / rebuild error (the
    /// CidNotify still attempts; the deposit rung carries the invite for the
    /// offline case).
    ///
    /// CodeAnt: called only AFTER the tunnel-send permit is acquired, so the
    /// signing/encoding cost (under the owner-state lock) is never paid for a
    /// tunnel attempt that is immediately shed by the concurrency cap.
    async fn build_bootstrap_invite(
        &self,
        entry: &OutboxEntry,
        recipient: OwnerAddr,
    ) -> Option<Vec<u8>> {
        // Acked → the Space exists on the recipient → no invite needed.
        if entry.delivered_to.contains(&recipient) {
            return None;
        }
        let state = self.crdt_state.lock().await;
        match crate::dm_outbox::build_invite_packet_from_space(
            &state,
            &entry.space_id,
            &self.signing_key,
            self.self_owner,
            self.our_signing_device_hash,
            self.inviter_identity_pub,
            // ZEB-580 S1: the live tunnel does NOT attach the #2 cert inline — it
            // holds no `enrollment_cert`. Its #2 DM material (`signing_key` /
            // `our_signing_device_hash` / `inviter_identity_pub` = the cert's #2
            // combined pub) is wired at construction in lib.rs (Task 6), so this
            // re-driven invite ships the inline #2 pub and a transitional receiver
            // TOFU-verifies it; the deposit rung and the original `add_space`
            // invite carry the master-attested cert for full verification.
            None,
        ) {
            Ok(invite) => invite,
            Err(e) => {
                tracing::warn!(
                    recipient = ?recipient,
                    space_id = ?entry.space_id,
                    error = %e,
                    "ZEB-504: bootstrap invite rebuild failed; sending CidNotify alone \
                     (deposit rung still carries the invite for the offline case)"
                );
                None
            }
        }
    }
}

/// ZEB-484: build the tunnel DM packet for one DM — `CidNotifyWithBlob` (inline
/// blob) when the blob is in CAS and the assembled packet fits `INLINE_BLOB_MAX`,
/// else the bare `CidNotify` (durability is the deposit rung's job either way).
async fn build_tunnel_dm_packet(
    cas: &Arc<dyn ContentStore>,
    signed: &crate::dm_envelope::DmCidNotifySigned,
    signing_key: &ed25519_dalek::SigningKey,
    message_cid: crate::owner_state_types::ContentId,
) -> Result<Vec<u8>, String> {
    // ZEB-484 (Qodo): LOCAL-only read — the sender's own DM blob is always in
    // local CAS, and a miss must fall back to a bare CidNotify rather than turn
    // the send path into a blocking network fetch that leaks an encrypted DM CID
    // onto the wire (`get` is `GetOrFetch` on the production store).
    if let Ok(Some(blob)) = cas.get_local(&message_cid).await {
        match build_dm_packet_with_blob(signed.clone(), signing_key, blob) {
            Ok(packet) if packet.len() <= INLINE_BLOB_MAX => return Ok(packet),
            Ok(_) => {
                // Oversize for the frame budget — fall through to bare CidNotify.
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "ZEB-484: with-blob packet build failed; sending bare CidNotify"
                );
            }
        }
    }
    build_dm_packet(signed.clone(), signing_key)
}

#[async_trait]
impl DmTransport for IrohTunnelDmTransport {
    /// Attempt the PQ tunnel to every reachable device of `recipient`, then
    /// return `Err` so the always-deposit rung fires for EVERY DM — `Transient`
    /// when an attempt was spawned, `TransientNoLiveAttempt` when not (ZEB-525;
    /// see module docs). `destinations` (the 16-byte Reticulum hashes) is
    /// ignored — the tunnel routes by iroh NodeId derived from the recipient's
    /// PQ keys.
    async fn send(
        &self,
        entry: &OutboxEntry,
        recipient: OwnerAddr,
        _destinations: Vec<[u8; 16]>,
    ) -> Result<(), TransportError> {
        // Resolve reachable tunnel targets first (cheap: a single owner-state
        // lock, no signing). The expensive invite rebuild is deferred until a
        // send permit is actually acquired (CodeAnt) — see below.
        let targets = {
            let state = self.crdt_state.lock().await;
            resolve_owner_tunnel_targets(&state, recipient)
        };

        // ZEB-485 (CodeAnt): cap concurrent in-flight tunnel sends. The spawned
        // build+send below is detached (required — see the deadlock note), so an
        // outbox-backlog drain would otherwise be one unbounded task per DM.
        // Acquire a permit up front; if the cap is saturated, SHED this
        // best-effort tunnel attempt — the deposit rung below still carries
        // durability — rather than pile more work onto CAS / the event loop.
        let mut live_attempt_spawned = false;
        if !targets.is_empty() {
            match Arc::clone(&self.tunnel_send_sem).try_acquire_owned() {
                Ok(permit) => {
                    // Permit held → this attempt WILL run, so it's now worth
                    // rebuilding the bootstrap invite (ZEB-504). Doing it here
                    // rather than before the semaphore avoids signing/encoding
                    // work under the owner-state lock for shed attempts (CodeAnt).
                    let invite_wire = self.build_bootstrap_invite(entry, recipient).await;
                    let mgr = std::sync::Arc::clone(&self.mgr);
                    // ZEB-485: SPAWN the tunnel `send_dm`s, do NOT await inline.
                    // `build_tunnel_dm_packet` reads the local CAS via
                    // `CasOp::GetLocal`, serviced by the SAME event loop that drives
                    // this drain inline — awaiting it here would deadlock. Spawning
                    // frees the loop; we return `Transient` immediately and the
                    // deposit rung carries durability regardless.
                    //
                    // ZEB-525 (Qodo, PR #500 R1): `live_attempt_spawned` is set
                    // per ARM, not on permit acquisition — the flag must track
                    // "a send is actually in flight", and the invite-only arm
                    // sends nothing when the invite rebuild failed.
                    match entry.message_cid {
                        // ZEB-505: invite-only entry — send the rebuilt bootstrap
                        // invite ALONE (no CidNotify; there is no message). The
                        // deposit rung carries durability exactly as for messages.
                        // No rebuilt invite (rebuild failure, or recipient already
                        // acked) → NOTHING to send → no spawn, permit dropped,
                        // and the flag stays false so the deposit fires on the
                        // first drain pass instead of after the grace window.
                        None => {
                            if invite_wire.is_some() {
                                live_attempt_spawned = true;
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if let Some(inv) = &invite_wire {
                                        for (node_id, contact) in &targets {
                                            mgr.send_dm(*node_id, contact, inv.clone());
                                        }
                                    }
                                });
                            } else {
                                tracing::debug!(
                                    recipient = ?recipient,
                                    "ZEB-525: invite-only entry with no rebuilt invite; \
                                     no tunnel attempt — deposit rung carries it immediately"
                                );
                            }
                        }
                        // Message entry — send the invite (if still needed) ahead of
                        // the CidNotify so the recipient creates the Space first.
                        // The CidNotify send is attempted regardless of the invite,
                        // so this arm is always a live attempt.
                        Some(message_cid) => {
                            live_attempt_spawned = true;
                            // ZEB-506: carry the sender's FULL cached device set (a
                            // cheap cache lookup under a brief owner-state lock — no
                            // CasOp here, so no drain deadlock) instead of a bare
                            // singleton. The recipient's ingestion refreshes its
                            // OwnerDeviceCache from `sender_devices` via the
                            // LWW-REPLACE `apply_owner_device_update`, so a singleton
                            // would shrink a multi-device sender's set and drop later
                            // messages signed by its other devices. Mirrors the
                            // deposit path + the PR #302 invite fix.
                            let sender_devices = {
                                let state = self.crdt_state.lock().await;
                                crate::dm_outbox::resolve_sender_devices(
                                    &state,
                                    self.self_owner,
                                    self.our_signing_device_hash,
                                )
                            };
                            let signed = crate::dm_envelope::DmCidNotifySigned {
                                space_id: entry.space_id,
                                message_cid,
                                sender_owner_addr: self.self_owner,
                                sender_devices,
                                signing_device_hash: self.our_signing_device_hash,
                            };
                            let cas = std::sync::Arc::clone(&self.cas);
                            let signing_key = std::sync::Arc::clone(&self.signing_key);
                            tokio::spawn(async move {
                                // Hold the permit for the task's lifetime; releasing
                                // it lets the next backlogged DM attempt the tunnel.
                                let _permit = permit;
                                // ZEB-484: inline the encrypted blob when it fits; else bare CidNotify.
                                match build_tunnel_dm_packet(
                                    &cas,
                                    &signed,
                                    &signing_key,
                                    message_cid,
                                )
                                .await
                                {
                                    Ok(packet) => {
                                        for (node_id, contact) in &targets {
                                            // ZEB-504: send the bootstrap invite FIRST
                                            // so the recipient creates the Space before
                                            // the CidNotify is admitted (else
                                            // `SpaceNotFound`). The tunnel's in-order
                                            // FIFO preserves this per peer; `invite_wire`
                                            // is `None` once the recipient has acked.
                                            if let Some(inv) = &invite_wire {
                                                mgr.send_dm(*node_id, contact, inv.clone());
                                            }
                                            mgr.send_dm(*node_id, contact, packet.clone());
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            recipient = ?recipient,
                                            error = %e,
                                            "ZEB-484: tunnel DM packet build failed; deposit rung still covers this DM"
                                        );
                                    }
                                }
                            });
                        }
                    }
                }
                Err(_) => {
                    tracing::debug!(
                        recipient = ?recipient,
                        cap = MAX_CONCURRENT_TUNNEL_SENDS,
                        "ZEB-485: tunnel send shed (concurrency cap reached); deposit rung carries this DM"
                    );
                }
            }
        }

        // ALWAYS return Err (never Ok): the tunnel is a parallel liveness
        // attempt; the outbox's deposit rung is the durability guarantee and
        // must fire for every DM (spec §5.7). The flavor tells the drain
        // whether an attempt is actually in flight (ZEB-525): `Transient`
        // keeps the one-window deposit grace so a live ack can win;
        // `TransientNoLiveAttempt` (no targets / capacity shed) deposits on
        // the first failure — there is nothing for the grace to wait on. The
        // spawned attempt above runs concurrently and never blocks the drain.
        if live_attempt_spawned {
            Err(TransportError::Transient(
                "ZEB-473: tunnel attempted (best-effort liveness); deposit rung carries durability"
                    .to_string(),
            ))
        } else {
            Err(TransportError::TransientNoLiveAttempt(
                "ZEB-525: no tunnel attempt (no reachable targets or capacity shed); \
                 deposit rung carries durability"
                    .to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{
        ContentId, DeliveryStatus, DeviceTunnelContact, Hlc, OutboxEntryId, OwnerDeviceEntry,
        SpaceId,
    };
    use harmony_identity::PqPrivateIdentity;
    use std::collections::BTreeSet;

    /// Build a `TunnelManager` over a real loopback iroh endpoint (cheap; these
    /// tests never complete a dial). Mirrors `tunnel_manager::tests`.
    async fn test_manager() -> Arc<TunnelManager> {
        let endpoint = {
            let sk = iroh::SecretKey::generate();
            crate::iroh_endpoint::new_with_secret_and_relays_hermetic_dns(sk, None)
                .await
                .expect("bind loopback iroh endpoint")
        };
        let local_pq = Arc::new(PqPrivateIdentity::generate(&mut rand::rngs::OsRng));
        let (ingest_tx, _ingest_rx) = tokio::sync::mpsc::channel(16);
        Arc::new(TunnelManager::new(
            Arc::new(endpoint),
            local_pq,
            ingest_tx,
            Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default())
                as Arc<dyn crate::tunnel_manager::CompatSink>,
        ))
    }

    /// ZEB-485: `IrohTunnelDmTransport::send` SPAWNS the build + tunnel `send_dm`
    /// (to avoid the `get_local` `CasOp::GetLocal` re-entrancy deadlock against
    /// the event loop), so the routed packet is observable only after the spawned
    /// task runs. The recipient NodeId is pre-registered as an Active handle (via
    /// `register_inbound`) so `send_dm` routes over `cmd_tx` — NOT a lazy dial
    /// whose fast failure to the synthetic contact would evict the pending before
    /// we observe it. Await the next routed `SendDm` payload under a generous
    /// timeout — parking on `recv()` yields to the spawned task, which is more
    /// robust than a fixed busy-poll budget that could trip under CI scheduling
    /// load. Panics on timeout.
    async fn wait_for_routed(
        cmd_rx: &mut tokio::sync::mpsc::Receiver<crate::tunnel_manager::TunnelCommand>,
    ) -> Vec<u8> {
        let routed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match cmd_rx.recv().await {
                    Some(crate::tunnel_manager::TunnelCommand::SendDm(p)) => return p,
                    Some(_) => continue,
                    None => panic!("tunnel command channel closed before routing a packet"),
                }
            }
        })
        .await;
        routed.expect("spawned tunnel send_dm never routed a packet within timeout")
    }

    fn synthetic_outbox_entry(space: SpaceId, cid: ContentId, recipient: OwnerAddr) -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([0xab; 16]),
            space_id: space,
            recipient_owners: vec![recipient],
            message_cid: Some(cid),
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    fn make_transport(
        mgr: Arc<TunnelManager>,
        state: Arc<tokio::sync::Mutex<OwnerState>>,
        cas: Arc<dyn crate::content_store::ContentStore>,
    ) -> IrohTunnelDmTransport {
        let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]));
        IrohTunnelDmTransport::new(
            mgr,
            state,
            signing_key,
            OwnerAddr([0xff; 16]),
            DeviceIdentityHash([0xaa; 16]),
            [0x55u8; 64],
            cas,
        )
    }

    /// ZEB-580 S1 Task 6: an `IrohTunnelDmTransport` wired with #2 (enrolled)
    /// material — exactly what `lib.rs` passes at node construction — signs its
    /// emitted CidNotify with the community key and stamps the #2 DM hash,
    /// verifiable against the cert's #2 combined pub (NOT a #3 transport key).
    /// Mirrors Task 5's `dm_outbox::tests::dm_outbox_signs_cidnotify_with_device2`
    /// at the tunnel seam, closing the "tunnel untested for #2" gap.
    #[tokio::test]
    async fn tunnel_signs_cidnotify_with_device2() {
        // Mint a real owner → its enrolled device cert + #2 (community) signing
        // key. Mirrors `dm_outbox::tests::outbox_from_mint`.
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let cert = minted.state.enrollments.values().next().unwrap().clone();
        let community_signing_key = Arc::new(minted.device_signing_key);
        let self_owner = OwnerAddr(cert.owner_id);
        let device2_hash =
            crate::dm_signing::device2_signing_hash(&cert).expect("real cert yields a #2 hash");
        let device2_pub = crate::dm_signing::device2_combined_pub(&cert);

        let mgr = test_manager().await;
        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 1184],
        };
        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x33; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "peer".into(),
                },
                device_tunnel_contacts: vec![Some(contact)],
            },
        );
        let state = Arc::new(tokio::sync::Mutex::new(owner_state));

        // Wire the transport with #2 material — exactly what lib.rs Task 6 passes:
        // community key + the cert's #2 DM hash + the cert's #2 combined pub.
        let transport = IrohTunnelDmTransport::new(
            Arc::clone(&mgr),
            state,
            Arc::clone(&community_signing_key),
            self_owner,
            device2_hash,
            device2_pub,
            Arc::new(crate::content_store::InMemoryStub::default()),
        );

        let space = SpaceId([0xcc; 16]);
        let cid = ContentId::from_bytes([0xee; 32]);
        let mut entry = synthetic_outbox_entry(space, cid, recipient);
        // ACKed → only the CidNotify routes (no bootstrap invite ahead of it), so
        // the first routed packet is the CidNotify under test.
        entry.delivered_to.insert(recipient);

        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);
        let _ = transport.send(&entry, recipient, Vec::new()).await;

        let routed = wait_for_routed(&mut cmd_rx).await;
        match crate::dm_envelope::decode_packet(&routed).expect("decode routed packet") {
            crate::dm_envelope::DmPacket::CidNotify {
                signed,
                signature,
                signed_bytes,
            } => {
                assert_eq!(
                    signed.signing_device_hash, device2_hash,
                    "tunnel CidNotify must stamp the #2 DM hash"
                );
                crate::dm_signing::verify_dm_packet_signature(
                    &signed_bytes,
                    &signature,
                    &device2_pub,
                    signed.signing_device_hash,
                )
                .expect("tunnel CidNotify must verify against the cert's #2 combined pub");
            }
            other => panic!("expected CidNotify, got {other:?}"),
        }
    }

    /// ZEB-504: insert a minimal DM `Space` (carrying a `content_key`) so the
    /// transport can rebuild a bootstrap invite for it. Members are sorted to
    /// honor the `Space::members` strictly-ascending invariant (production's
    /// `add_space` sorts; a direct insert here must too, else the rebuilt invite
    /// fails decode's `members` check).
    fn install_dm_space(state: &mut OwnerState, space_id: SpaceId, mut members: Vec<OwnerAddr>) {
        members.sort();
        let space = crate::owner_state_types::Space {
            id: space_id,
            kind: crate::owner_state_types::SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "dm".into(),
            transport: None,
            members,
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "dev".into(),
            },
            updated_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "dev".into(),
            },
            content_key: Some(crate::owner_state_types::DmContentKey::new([0x42u8; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };
        state.spaces.insert(space_id, space);
    }

    /// Build owner-state with `recipient` reachable (one tunnel contact) and a DM
    /// Space `space` present — the fixture both new ZEB-504 tests share.
    fn state_with_recipient_and_dm_space(
        recipient: OwnerAddr,
        space: SpaceId,
        dsa_pubkey: &[u8],
    ) -> OwnerState {
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.to_vec(),
            pq_kem_pubkey: vec![0x08u8; 1184],
        };
        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x33; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "peer".into(),
                },
                device_tunnel_contacts: vec![Some(contact)],
            },
        );
        install_dm_space(
            &mut owner_state,
            space,
            vec![OwnerAddr([0xff; 16]), recipient],
        );
        owner_state
    }

    /// ZEB-504: a recipient who hasn't acked the entry → `send` routes the
    /// bootstrap `DmInvite` FIRST, then the CidNotify, so the receiver creates
    /// the Space before the message is admitted (else `SpaceNotFound`).
    #[tokio::test]
    async fn send_routes_bootstrap_invite_before_cidnotify_when_unacked() {
        let mgr = test_manager().await;
        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);
        let space = SpaceId([0xcc; 16]);

        let state = Arc::new(tokio::sync::Mutex::new(state_with_recipient_and_dm_space(
            recipient,
            space,
            &dsa_pubkey,
        )));
        let transport = make_transport(
            Arc::clone(&mgr),
            state,
            Arc::new(crate::content_store::InMemoryStub::default()),
        );

        let cid = ContentId::from_bytes([0xee; 32]);
        // delivered_to empty → recipient un-acked → invite must ride along.
        let entry = synthetic_outbox_entry(space, cid, recipient);

        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);

        let err = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("tunnel transport must return Transient (always-deposit)");
        assert!(matches!(err, TransportError::Transient(_)));

        // First routed packet = the bootstrap invite; second = the CidNotify.
        let first = wait_for_routed(&mut cmd_rx).await;
        let second = wait_for_routed(&mut cmd_rx).await;
        fn tag(b: &[u8]) -> String {
            match crate::dm_envelope::decode_packet(b) {
                Ok(crate::dm_envelope::DmPacket::Invite { .. }) => "Invite".into(),
                Ok(crate::dm_envelope::DmPacket::CidNotify { .. }) => "CidNotify".into(),
                Ok(crate::dm_envelope::DmPacket::CidNotifyWithBlob { .. }) => "WithBlob".into(),
                Ok(crate::dm_envelope::DmPacket::Ack { .. }) => "Ack".into(),
                Ok(crate::dm_envelope::DmPacket::RevocationPush { .. }) => "RevocationPush".into(),
                Err(e) => format!("DecodeErr({e})"),
            }
        }
        assert!(
            matches!(
                crate::dm_envelope::decode_packet(&first),
                Ok(crate::dm_envelope::DmPacket::Invite { .. })
            ),
            "first routed packet must be the bootstrap DmInvite (ZEB-504); got first={}, second={}",
            tag(&first),
            tag(&second)
        );
        assert!(
            matches!(
                crate::dm_envelope::decode_packet(&second),
                Ok(crate::dm_envelope::DmPacket::CidNotify { .. })
                    | Ok(crate::dm_envelope::DmPacket::CidNotifyWithBlob { .. })
            ),
            "second routed packet must be the CidNotify"
        );
    }

    /// ZEB-505 (CodeRabbit): an invite-only outbox entry (`message_cid: None`)
    /// has no message, so `send` routes the bootstrap invite ALONE over the
    /// tunnel (no CidNotify can be built without a message_cid), and still
    /// returns `Transient` so the deposit rung carries durability.
    #[tokio::test]
    async fn send_routes_invite_only_entry_as_bare_invite() {
        let mgr = test_manager().await;
        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);
        let space = SpaceId([0xcc; 16]);

        let state = Arc::new(tokio::sync::Mutex::new(state_with_recipient_and_dm_space(
            recipient,
            space,
            &dsa_pubkey,
        )));
        let transport = make_transport(
            Arc::clone(&mgr),
            state,
            Arc::new(crate::content_store::InMemoryStub::default()),
        );

        // Invite-only entry: message_cid: None.
        let cid = ContentId::from_bytes([0xee; 32]);
        let mut entry = synthetic_outbox_entry(space, cid, recipient);
        entry.message_cid = None;

        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);

        let err = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("tunnel transport must return Transient (always-deposit)");
        assert!(matches!(err, TransportError::Transient(_)));

        // Exactly one routed packet, and it is the bootstrap invite.
        let first = wait_for_routed(&mut cmd_rx).await;
        assert!(
            matches!(
                crate::dm_envelope::decode_packet(&first),
                Ok(crate::dm_envelope::DmPacket::Invite { .. })
            ),
            "an invite-only entry must route the bootstrap DmInvite alone"
        );
        let second = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            wait_for_routed(&mut cmd_rx),
        )
        .await;
        assert!(
            second.is_err(),
            "no CidNotify must follow an invite-only send (there is no message)"
        );
    }

    /// ZEB-525 (Qodo, PR #500 R1): an invite-only entry whose bootstrap-invite
    /// rebuild FAILS (no DM Space in owner-state) sends nothing at all — the
    /// spawn is skipped, so `send` must return `TransientNoLiveAttempt` (first-
    /// tick deposit), not `Transient` (grace window). Pre-fix, the flag was set
    /// on permit acquisition and this case was misclassified as a live attempt.
    #[tokio::test]
    async fn send_invite_only_with_failed_invite_rebuild_returns_no_live_attempt() {
        let mgr = test_manager().await;
        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);

        // Reachable tunnel contact, but NO DM Space installed → the ZEB-504
        // invite rebuild errors → `build_bootstrap_invite` returns `None`.
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 1184],
        };
        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x33; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "peer".into(),
                },
                device_tunnel_contacts: vec![Some(contact)],
            },
        );
        let state = Arc::new(tokio::sync::Mutex::new(owner_state));
        let transport = make_transport(
            Arc::clone(&mgr),
            state,
            Arc::new(crate::content_store::InMemoryStub::default()),
        );

        // Invite-only entry: message_cid: None, recipient un-acked.
        let cid = ContentId::from_bytes([0xee; 32]);
        let mut entry = synthetic_outbox_entry(SpaceId([0xcc; 16]), cid, recipient);
        entry.message_cid = None;

        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);

        let err = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("must return an error (always-deposit)");
        assert!(
            matches!(err, TransportError::TransientNoLiveAttempt(_)),
            "failed invite rebuild sends nothing → must classify as \
             TransientNoLiveAttempt for first-tick deposit, got {err:?}"
        );

        // Nothing was spawned, so nothing routes to the manager. Negative wait
        // mirrors the shed test: it can only false-pass, never false-fail.
        let routed =
            tokio::time::timeout(std::time::Duration::from_millis(300), cmd_rx.recv()).await;
        assert!(
            routed.is_err(),
            "no packet may route when the invite rebuild failed (nothing to send)"
        );
    }

    /// ZEB-504: once the recipient has ACKed (an ack implies they admitted a
    /// CidNotify, so the Space exists on their side), `send` omits the invite and
    /// routes only the CidNotify.
    #[tokio::test]
    async fn send_omits_invite_once_recipient_acked() {
        let mgr = test_manager().await;
        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);
        let space = SpaceId([0xcc; 16]);

        let state = Arc::new(tokio::sync::Mutex::new(state_with_recipient_and_dm_space(
            recipient,
            space,
            &dsa_pubkey,
        )));
        let transport = make_transport(
            Arc::clone(&mgr),
            state,
            Arc::new(crate::content_store::InMemoryStub::default()),
        );

        let cid = ContentId::from_bytes([0xee; 32]);
        let mut entry = synthetic_outbox_entry(space, cid, recipient);
        entry.delivered_to.insert(recipient); // ACKed → Space exists; no invite.

        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);

        let err = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("tunnel transport must return Transient (always-deposit)");
        assert!(matches!(err, TransportError::Transient(_)));

        // Exactly one routed packet, and it is the CidNotify (no invite).
        let first = wait_for_routed(&mut cmd_rx).await;
        assert!(
            matches!(
                crate::dm_envelope::decode_packet(&first),
                Ok(crate::dm_envelope::DmPacket::CidNotify { .. })
                    | Ok(crate::dm_envelope::DmPacket::CidNotifyWithBlob { .. })
            ),
            "acked recipient must receive only the CidNotify"
        );
        let second = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            wait_for_routed(&mut cmd_rx),
        )
        .await;
        assert!(
            second.is_err(),
            "no bootstrap invite must be routed for an already-acked recipient"
        );
    }

    /// ZEB-504 regression (Qodo): the rebuilt bootstrap invite must carry the
    /// inviter's FULL cached device set, not a bare singleton. The live-tunnel
    /// resend is applied receiver-side with `refresh_owner_device_cache=true` at
    /// a fresh local `learned_at` HLC, and `apply_owner_device_update` is
    /// LWW-REPLACE (not union); a singleton resend would therefore shrink the
    /// receiver's cached inviter device set and later drop messages signed by the
    /// inviter's other devices as `UnknownSigningKey`.
    #[tokio::test]
    async fn send_invite_carries_full_inviter_device_set() {
        let mgr = test_manager().await;
        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);
        let space = SpaceId([0xcc; 16]);

        let mut owner_state = state_with_recipient_and_dm_space(recipient, space, &dsa_pubkey);
        // The inviter (self, OwnerAddr([0xff;16]) per `make_transport`) has THREE
        // enrolled devices; the signing device ([0xaa;16]) is one of them. Stored
        // sorted ascending, mirroring `apply_owner_device_update`.
        let self_owner = OwnerAddr([0xff; 16]);
        let full_devices = vec![
            DeviceIdentityHash([0xaa; 16]),
            DeviceIdentityHash([0xbb; 16]),
            DeviceIdentityHash([0xdd; 16]),
        ];
        owner_state.owner_device_cache.devices.insert(
            self_owner,
            OwnerDeviceEntry {
                devices: full_devices.clone(),
                device_identity_pubs: vec![None; 3],
                learned_at: Hlc {
                    wall_ms: 5,
                    logical: 0,
                    device_id: "self".into(),
                },
                device_tunnel_contacts: vec![None; 3],
            },
        );

        let state = Arc::new(tokio::sync::Mutex::new(owner_state));
        let transport = make_transport(
            Arc::clone(&mgr),
            state,
            Arc::new(crate::content_store::InMemoryStub::default()),
        );

        let cid = ContentId::from_bytes([0xee; 32]);
        // delivered_to empty → recipient un-acked → invite rides along first.
        let entry = synthetic_outbox_entry(space, cid, recipient);

        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);
        let _ = transport.send(&entry, recipient, Vec::new()).await;

        let first = wait_for_routed(&mut cmd_rx).await;
        let Ok(crate::dm_envelope::DmPacket::Invite { signed, .. }) =
            crate::dm_envelope::decode_packet(&first)
        else {
            panic!("first routed packet must be the bootstrap DmInvite");
        };
        assert_eq!(
            signed.sender_devices, full_devices,
            "rebuilt invite must carry the inviter's full cached device set, \
             not a singleton (ZEB-504 device-list-regression)"
        );
        // ZEB-580 S1: the live-tunnel rebuild is a deliberate no-inline-cert
        // trust boundary — it ships the #2 pub for transitional TOFU and relies
        // on the deposit rung / original add_space invite for the master-attested
        // cert. Pin that so a regression that accidentally attaches (or drops when
        // it shouldn't) the inline cert here is caught.
        assert!(
            signed.inviter_enrollment.is_none(),
            "ZEB-580 S1: the live-tunnel rebuilt invite must NOT carry the inline cert"
        );
    }

    /// A recipient whose cached entry advertises a `DeviceTunnelContact` →
    /// `send` derives the tunnel NodeId (`blake3(pq_dsa)`), builds the packet,
    /// and routes it through `TunnelManager::send_dm` (observable as a buffered
    /// pending packet on a freshly-dialing handle keyed by that NodeId).
    #[tokio::test]
    async fn send_resolves_contact_and_routes_to_manager() {
        let mgr = test_manager().await;

        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952]; // any non-empty DSA pub
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 1184],
        };
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);

        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x33; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "peer".into(),
                },
                device_tunnel_contacts: vec![Some(contact)],
            },
        );
        let state = Arc::new(tokio::sync::Mutex::new(owner_state));

        let transport = make_transport(
            Arc::clone(&mgr),
            state,
            Arc::new(crate::content_store::InMemoryStub::default()),
        );

        let space = SpaceId([0xcc; 16]);
        let cid = ContentId::from_bytes([0xee; 32]);
        let entry = synthetic_outbox_entry(space, cid, recipient);

        // ZEB-485: pre-register an Active handle for the recipient NodeId so the
        // SPAWNED send_dm routes over cmd_tx; a lazy dial to the synthetic contact
        // would fail fast and evict the pending before we could observe it.
        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);

        // Always-deposit invariant: send returns Transient (drives deposit).
        let err = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("tunnel transport must return Transient (always-deposit)");
        assert!(
            matches!(err, TransportError::Transient(_)),
            "must be Transient so the deposit rung fires for every DM, got {err:?}"
        );

        // The manager received exactly one send_dm for the derived NodeId, with
        // the byte-identical built packet buffered on the dialing handle.
        let routed = wait_for_routed(&mut cmd_rx).await;

        // The routed bytes must be the live build_dm_packet output for this DM.
        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id: space,
            message_cid: cid,
            sender_owner_addr: OwnerAddr([0xff; 16]),
            sender_devices: vec![DeviceIdentityHash([0xaa; 16])],
            signing_device_hash: DeviceIdentityHash([0xaa; 16]),
        };
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]);
        let expected = build_dm_packet(signed, &signing_key).expect("build expected packet");
        assert_eq!(routed, expected, "routed packet must match build_dm_packet");

        // Exactly one packet routes for this single (un-invited, acked-Space-less)
        // DM — no spurious extra send. (ZEB-739: the manager's session map is no
        // longer client-inspectable; observing the registered handle's cmd channel
        // is the behavior-equivalent check.)
        let extra =
            tokio::time::timeout(std::time::Duration::from_millis(200), cmd_rx.recv()).await;
        assert!(
            extra.is_err(),
            "exactly one packet must route for a single DM (no spurious send)"
        );
    }

    /// ZEB-482: the shared `send_packet_to_owner_tunnels` helper routes
    /// arbitrary pre-built packet bytes (here a synthetic DmInvite payload) to
    /// the recipient's reachable tunnel device — `blake3(pq_dsa)` NodeId carries
    /// exactly those bytes. This is the send half of the invite carrier; it
    /// reuses the SAME resolver `IrohTunnelDmTransport::send` uses.
    #[tokio::test]
    async fn send_packet_to_owner_tunnels_routes_arbitrary_bytes_to_resolved_device() {
        let mgr = test_manager().await;

        let recipient = OwnerAddr([0x55; 16]);
        // Realistic valid key sizes (ML-DSA-65 pub 1952B / ML-KEM-768 pub
        // 1184B), the same sizes `DeviceTunnelContact::has_valid_key_sizes`
        // accepts at the friend handshake.
        let dsa_pubkey = vec![0x07u8; 1952];
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 1184],
        };
        assert!(
            contact.has_valid_key_sizes(),
            "fixture contact must use valid PQ key sizes"
        );
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);

        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x33; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "peer".into(),
                },
                device_tunnel_contacts: vec![Some(contact)],
            },
        );

        // The helper takes a short crdt_state lock internally to resolve, then
        // releases it before send_dm.
        let crdt_state = Arc::new(tokio::sync::Mutex::new(owner_state));
        let payload = b"arbitrary DmInvite wire bytes".to_vec();

        // ZEB-739: the manager's session map is no longer client-inspectable, so
        // pre-register an Active handle for the derived NodeId and observe the
        // routed packet over its cmd channel — behavior-equivalent to the old
        // `test_pending_packets` read of the dialing handle's pending buffer.
        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);
        send_packet_to_owner_tunnels(&crdt_state, &mgr, recipient, &payload).await;

        let routed = wait_for_routed(&mut cmd_rx).await;
        assert_eq!(routed, payload, "routed bytes must be the passed packet");

        // An unknown recipient (no cached device entry) resolves to zero targets,
        // so nothing new routes — the existing target's channel receives no further
        // packet (catches a regression that routes unknown recipients to an
        // existing target).
        send_packet_to_owner_tunnels(&crdt_state, &mgr, OwnerAddr([0xEE; 16]), &payload).await;
        let extra =
            tokio::time::timeout(std::time::Duration::from_millis(200), cmd_rx.recv()).await;
        assert!(
            extra.is_err(),
            "an unknown recipient must NOT route anything to the existing target"
        );
    }

    /// A recipient with NO tunnel contact (device known-by-hash only) → `send`
    /// routes NOTHING to the tunnel and returns `TransientNoLiveAttempt`
    /// (ZEB-525) so the deposit rung fires on the FIRST drain pass — no live
    /// attempt exists for the one-window deposit grace to wait on.
    #[tokio::test]
    async fn send_without_contact_returns_no_live_attempt_for_immediate_deposit() {
        let mgr = test_manager().await;

        let recipient = OwnerAddr([0x22; 16]);
        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x44; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "peer".into(),
                },
                // No contact advertised yet.
                device_tunnel_contacts: vec![None],
            },
        );
        let state = Arc::new(tokio::sync::Mutex::new(owner_state));
        let transport = make_transport(
            Arc::clone(&mgr),
            state,
            Arc::new(crate::content_store::InMemoryStub::default()),
        );

        let entry = synthetic_outbox_entry(
            SpaceId([0xcc; 16]),
            ContentId::from_bytes([0xee; 32]),
            recipient,
        );

        let err = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("must return an error even with no tunnel target");
        assert!(
            matches!(err, TransportError::TransientNoLiveAttempt(_)),
            "no-contact recipient must deposit immediately (TransientNoLiveAttempt), got {err:?}"
        );
    }

    /// An UNKNOWN recipient (no cached entry at all) → no tunnel routing →
    /// `TransientNoLiveAttempt` (ZEB-525: deposit fires on the first drain
    /// pass; Flow A propagating the entry later re-enables tunnel attempts).
    #[tokio::test]
    async fn send_unknown_recipient_returns_no_live_attempt() {
        let mgr = test_manager().await;
        let state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
        let transport = make_transport(
            mgr,
            state,
            Arc::new(crate::content_store::InMemoryStub::default()),
        );

        let recipient = OwnerAddr([0x77; 16]);
        let entry = synthetic_outbox_entry(
            SpaceId([0xcc; 16]),
            ContentId::from_bytes([0xee; 32]),
            recipient,
        );

        let err = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("unknown recipient must return an error");
        assert!(matches!(err, TransportError::TransientNoLiveAttempt(_)));
    }

    /// ZEB-484: a recipient with a tunnel contact AND the blob in CAS → `send`
    /// routes a `CidNotifyWithBlob` carrying that exact blob.
    #[tokio::test]
    async fn send_with_blob_in_cas_routes_cidnotify_with_blob() {
        let mgr = test_manager().await;
        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 1184],
        };
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);
        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x33; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "peer".into(),
                },
                device_tunnel_contacts: vec![Some(contact)],
            },
        );
        let state = Arc::new(tokio::sync::Mutex::new(owner_state));

        let cid = ContentId::from_bytes([0xee; 32]);
        let blob = vec![0xCDu8; 2048];
        let cas: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        cas.put(cid, blob.clone()).await.expect("seed blob in CAS");

        let transport = make_transport(Arc::clone(&mgr), state, Arc::clone(&cas));
        let entry = synthetic_outbox_entry(SpaceId([0xcc; 16]), cid, recipient);
        // ZEB-485: pre-register an Active handle so the spawned send_dm routes over
        // cmd_tx (see send_resolves_contact_and_routes_to_manager for why).
        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);
        let _ = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("always-deposit: send returns Transient");

        let routed = wait_for_routed(&mut cmd_rx).await;
        match crate::dm_envelope::decode_packet(&routed).expect("decode routed packet") {
            crate::dm_envelope::DmPacket::CidNotifyWithBlob {
                signed,
                storage_blob,
                ..
            } => {
                assert_eq!(signed.message_cid, cid, "carries the DM's message_cid");
                assert_eq!(storage_blob, blob, "inlines the exact CAS blob");
            }
            other => panic!("expected CidNotifyWithBlob, got {other:?}"),
        }
    }

    /// ZEB-484: a blob larger than the frame budget → `send` falls back to a bare
    /// `CidNotify` (deposit rung carries durability).
    #[tokio::test]
    async fn send_oversize_blob_falls_back_to_bare_cidnotify() {
        let mgr = test_manager().await;
        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 1184],
        };
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);
        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x33; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "peer".into(),
                },
                device_tunnel_contacts: vec![Some(contact)],
            },
        );
        let state = Arc::new(tokio::sync::Mutex::new(owner_state));

        let cid = ContentId::from_bytes([0xee; 32]);
        let blob = vec![0x00u8; INLINE_BLOB_MAX]; // assembled packet exceeds the ceiling
        let cas: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        cas.put(cid, blob).await.expect("seed oversize blob");

        let transport = make_transport(Arc::clone(&mgr), state, Arc::clone(&cas));
        let entry = synthetic_outbox_entry(SpaceId([0xcc; 16]), cid, recipient);
        // ZEB-485: pre-register an Active handle so the spawned send_dm routes over
        // cmd_tx (see send_resolves_contact_and_routes_to_manager for why).
        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);
        let _ = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("Transient");

        let routed = wait_for_routed(&mut cmd_rx).await;
        assert!(
            matches!(
                crate::dm_envelope::decode_packet(&routed).unwrap(),
                crate::dm_envelope::DmPacket::CidNotify { .. }
            ),
            "an oversize blob must fall back to a bare CidNotify"
        );
    }

    /// ZEB-485 (CodeAnt): when the concurrent-tunnel-send cap is saturated, `send`
    /// SHEDS the best-effort tunnel attempt (no spawn, nothing routed to the
    /// manager) and returns `TransientNoLiveAttempt` (ZEB-525) so the deposit
    /// rung carries the DM on the first drain pass — a shed attempt leaves
    /// nothing in flight for the deposit grace window to wait on.
    #[tokio::test]
    async fn send_sheds_tunnel_attempt_when_concurrency_cap_saturated() {
        let mgr = test_manager().await;

        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 1184],
        };
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);

        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x33; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "peer".into(),
                },
                device_tunnel_contacts: vec![Some(contact)],
            },
        );
        let state = Arc::new(tokio::sync::Mutex::new(owner_state));

        // Cap = 0 => the very first tunnel attempt is shed onto the deposit rung.
        let transport = IrohTunnelDmTransport::with_tunnel_send_cap(
            Arc::clone(&mgr),
            state,
            Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32])),
            OwnerAddr([0xff; 16]),
            DeviceIdentityHash([0xaa; 16]),
            [0x55u8; 64],
            Arc::new(crate::content_store::InMemoryStub::default()),
            0,
        );

        let space = SpaceId([0xcc; 16]);
        let cid = ContentId::from_bytes([0xee; 32]);
        let entry = synthetic_outbox_entry(space, cid, recipient);

        let (mut cmd_rx, _ep) = mgr.register_inbound(expected_node_id);

        // Always-deposit invariant holds even when the tunnel attempt is shed.
        let err = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("must return an error (always-deposit) even when shed");
        assert!(
            matches!(err, TransportError::TransientNoLiveAttempt(_)),
            "shed attempt must deposit immediately (TransientNoLiveAttempt), got {err:?}"
        );

        // Nothing was spawned, so nothing routes to the manager. Negative wait:
        // a shed attempt never routes, so a short timeout that observes no packet
        // is the assertion — it can only false-pass, never false-fail under load.
        let routed =
            tokio::time::timeout(std::time::Duration::from_millis(50), cmd_rx.recv()).await;
        assert!(
            routed.is_err(),
            "a saturated cap must shed the tunnel attempt (no packet routed)"
        );
    }
}
