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
//! `send` returns `Err(TransportError::Transient(_))` — byte-for-byte the same
//! contract `DepositOnlyDmTransport` had. This is deliberate and load-bearing:
//! the outbox drain's deposit rung fires on a *transient* failure once the pair
//! has aged ≥ 1 backoff window (`drain_phase_c` Err-arm, `pre_failure_count >=
//! 1`), so returning `Transient` keeps the butler/community-relay durability
//! rung firing for EVERY DM exactly as before — no regression for offline
//! peers. Returning `Ok` would instead steer the pair into the
//! "sent-but-never-acked" arm, which only deposits from `DEPOSIT_NOACK_WINDOWS`
//! (= 2) windows onward — strictly *later*, weakening durability. So the tunnel
//! is a pure parallel liveness path layered on top of the unchanged deposit
//! path; the recipient dedups (CRDT inbox) the tunnel copy against the deposit
//! copy.
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
/// `(NodeId = blake3(pq_dsa_pubkey), contact)` for each bound device that
/// advertised a [`DeviceTunnelContact`](crate::owner_state_types::DeviceTunnelContact).
/// Devices with no contact yet (`None`) are skipped — the tunnel can't reach
/// them, and the deposit rung covers them.
///
/// Pure over the passed `&OwnerState` (no lock taken here); the caller holds
/// (or has snapshotted out of) the owner-state lock and must NOT hold it across
/// any subsequent `send_dm` `.await` — `send_dm` itself does not await, so a
/// short read lock spanning the resolve+send pair is fine.
pub(crate) fn resolve_owner_tunnel_targets(
    state: &OwnerState,
    recipient: OwnerAddr,
) -> Vec<([u8; 32], crate::owner_state_types::DeviceTunnelContact)> {
    let Some(entry) = state.owner_device_cache.devices.get(&recipient) else {
        return Vec::new();
    };
    entry
        .device_tunnel_contacts
        .iter()
        .filter_map(|maybe| maybe.as_ref())
        .map(|contact| {
            let node_id = node_id_from_dsa_pubkey(&contact.pq_dsa_pubkey);
            (node_id, contact.clone())
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
    pub fn new(
        mgr: Arc<TunnelManager>,
        crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
        signing_key: Arc<ed25519_dalek::SigningKey>,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        cas: Arc<dyn ContentStore>,
    ) -> Self {
        Self::with_tunnel_send_cap(
            mgr,
            crdt_state,
            signing_key,
            self_owner,
            our_signing_device_hash,
            cas,
            MAX_CONCURRENT_TUNNEL_SENDS,
        )
    }

    /// Construct with an explicit concurrent-tunnel-send cap. `new` uses
    /// [`MAX_CONCURRENT_TUNNEL_SENDS`]; tests use this to exercise the
    /// shed-when-saturated path with a small (or zero) cap.
    fn with_tunnel_send_cap(
        mgr: Arc<TunnelManager>,
        crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
        signing_key: Arc<ed25519_dalek::SigningKey>,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        cas: Arc<dyn ContentStore>,
        tunnel_send_cap: usize,
    ) -> Self {
        Self {
            mgr,
            crdt_state,
            signing_key,
            self_owner,
            our_signing_device_hash,
            cas,
            tunnel_send_sem: Arc::new(tokio::sync::Semaphore::new(tunnel_send_cap)),
        }
    }

    /// Snapshot the recipient's reachable tunnel contacts: for each bound
    /// device of `recipient` that advertised a `DeviceTunnelContact`, derive
    /// its tunnel NodeId (`blake3(pq_dsa_pubkey)`, matching harmony-tunnel) and
    /// pair it with the contact. Devices with no contact yet (`None`) are
    /// skipped — the tunnel can't reach them, and the deposit rung covers them.
    ///
    /// Reads under a short `crdt_state` lock and clones out so no lock is held
    /// across the tunnel `send_dm` calls.
    async fn resolve_tunnel_targets(
        &self,
        recipient: OwnerAddr,
    ) -> Vec<([u8; 32], crate::owner_state_types::DeviceTunnelContact)> {
        let state = self.crdt_state.lock().await;
        resolve_owner_tunnel_targets(&state, recipient)
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
    /// return `Transient` so the always-deposit rung fires for EVERY DM (see
    /// module docs). `destinations` (the 16-byte Reticulum hashes) is ignored —
    /// the tunnel routes by iroh NodeId derived from the recipient's PQ keys.
    async fn send(
        &self,
        entry: &OutboxEntry,
        recipient: OwnerAddr,
        _destinations: Vec<[u8; 16]>,
    ) -> Result<(), TransportError> {
        let targets = self.resolve_tunnel_targets(recipient).await;

        // ZEB-485 (CodeAnt): cap concurrent in-flight tunnel sends. The spawned
        // build+send below is detached (required — see the deadlock note), so an
        // outbox-backlog drain would otherwise be one unbounded task per DM.
        // Acquire a permit up front; if the cap is saturated, SHED this
        // best-effort tunnel attempt — the deposit rung below still carries
        // durability — rather than pile more work onto CAS / the event loop.
        if !targets.is_empty() {
            match Arc::clone(&self.tunnel_send_sem).try_acquire_owned() {
                Ok(permit) => {
                    let signed = crate::dm_envelope::DmCidNotifySigned {
                        space_id: entry.space_id,
                        message_cid: entry.message_cid,
                        sender_owner_addr: self.self_owner,
                        sender_devices: vec![self.our_signing_device_hash],
                        signing_device_hash: self.our_signing_device_hash,
                    };
                    // ZEB-485: SPAWN the blob build + tunnel `send_dm`, do NOT
                    // await it inline. `build_tunnel_dm_packet` reads the local
                    // CAS via `CasOp::GetLocal`, which is serviced by the SAME
                    // event loop that drives this outbox drain inline (its timer
                    // tick does `drain_lifted(...).await`). Awaiting `get_local`
                    // here deadlocks: the loop is blocked on the drain and can
                    // never reply to its own `GetLocal`. Spawning frees the loop
                    // to service `GetLocal`; we return `Transient` immediately and
                    // the deposit rung carries durability regardless. (Resolving
                    // targets above only locks `crdt_state` — not the CasOp bridge
                    // — so it stays inline.)
                    let mgr = std::sync::Arc::clone(&self.mgr);
                    let cas = std::sync::Arc::clone(&self.cas);
                    let signing_key = std::sync::Arc::clone(&self.signing_key);
                    let message_cid = entry.message_cid;
                    tokio::spawn(async move {
                        // Hold the permit for the task's lifetime; releasing it on
                        // completion lets the next backlogged DM attempt the tunnel.
                        let _permit = permit;
                        // ZEB-484: inline the encrypted blob when it fits; else bare CidNotify.
                        match build_tunnel_dm_packet(&cas, &signed, &signing_key, message_cid).await
                        {
                            Ok(packet) => {
                                for (node_id, contact) in &targets {
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
                Err(_) => {
                    tracing::debug!(
                        recipient = ?recipient,
                        cap = MAX_CONCURRENT_TUNNEL_SENDS,
                        "ZEB-485: tunnel send shed (concurrency cap reached); deposit rung carries this DM"
                    );
                }
            }
        }

        // ALWAYS return Transient (never Ok): the tunnel is a parallel
        // liveness attempt; the outbox's deposit rung is the durability
        // guarantee and must fire for every DM (spec §5.7). See module docs for
        // why Transient (not Ok) is the correct contract. The spawned attempt
        // above runs concurrently and never blocks the drain.
        Err(TransportError::Transient(
            "ZEB-473: tunnel attempted (best-effort liveness); deposit rung carries durability"
                .to_string(),
        ))
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
            crate::iroh_endpoint::IrohEndpoint::new_with_secret(sk)
                .await
                .expect("bind loopback iroh endpoint")
        };
        let local_pq = Arc::new(PqPrivateIdentity::generate(&mut rand::rngs::OsRng));
        let (ingest_tx, _ingest_rx) = tokio::sync::mpsc::channel(16);
        Arc::new(TunnelManager::new(endpoint, local_pq, ingest_tx))
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
            message_cid: cid,
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
            cas,
        )
    }

    /// A recipient whose cached entry advertises a `DeviceTunnelContact` →
    /// `send` derives the tunnel NodeId (`blake3(pq_dsa)`), builds the packet,
    /// and routes it through `TunnelManager::send_dm` (observable as a buffered
    /// pending packet on a freshly-dialing handle keyed by that NodeId).
    #[tokio::test]
    async fn send_resolves_contact_and_routes_to_manager() {
        let mgr = test_manager().await;

        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 32]; // any non-empty DSA pub
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 32],
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

        // And no spurious session for any other NodeId.
        assert!(
            mgr.test_pending_packets(&[0x00; 32]).is_none(),
            "no session for an unrelated NodeId"
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
        send_packet_to_owner_tunnels(&crdt_state, &mgr, recipient, &payload).await;

        let pending = mgr
            .test_pending_packets(&expected_node_id)
            .expect("helper must register a session for the derived NodeId");
        assert_eq!(pending.len(), 1, "exactly one packet routed to the tunnel");
        assert_eq!(
            pending[0], payload,
            "routed bytes must be the passed packet"
        );

        // An unknown recipient routes nothing — neither a spurious new session
        // NOR a misrouted packet to the EXISTING target. Capture the resolved
        // target's pending count first, fire at an unknown recipient, then assert
        // both invariants: no `[0;32]` session was created AND the real target's
        // pending count is unchanged (catches a regression that routes unknown
        // recipients to an existing target).
        let target_pending_before = mgr
            .test_pending_packets(&expected_node_id)
            .map(|p| p.len())
            .expect("the resolved target session exists from the first send");
        send_packet_to_owner_tunnels(&crdt_state, &mgr, OwnerAddr([0xEE; 16]), &payload).await;
        assert!(
            mgr.test_pending_packets(&[0x00; 32]).is_none(),
            "no session for an unrelated NodeId"
        );
        let target_pending_after = mgr
            .test_pending_packets(&expected_node_id)
            .map(|p| p.len())
            .expect("the resolved target session must still exist");
        assert_eq!(
            target_pending_after, target_pending_before,
            "an unknown recipient must NOT route anything to the existing target"
        );
    }

    /// A recipient with NO tunnel contact (device known-by-hash only) → `send`
    /// routes NOTHING to the tunnel but STILL returns Transient so the deposit
    /// rung covers the offline/unreachable peer (no durability regression).
    #[tokio::test]
    async fn send_without_contact_still_returns_transient_for_deposit() {
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
            .expect_err("must return Transient even with no tunnel target");
        assert!(
            matches!(err, TransportError::Transient(_)),
            "no-contact recipient must still deposit (Transient), got {err:?}"
        );
    }

    /// An UNKNOWN recipient (no cached entry at all) → no tunnel routing, still
    /// Transient (deposit covers it once Flow A propagates the entry).
    #[tokio::test]
    async fn send_unknown_recipient_returns_transient() {
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
            .expect_err("unknown recipient must return Transient");
        assert!(matches!(err, TransportError::Transient(_)));
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
    /// manager) and still returns Transient so the deposit rung carries the DM.
    #[tokio::test]
    async fn send_sheds_tunnel_attempt_when_concurrency_cap_saturated() {
        let mgr = test_manager().await;

        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 32];
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 32],
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
            .expect_err("must return Transient (always-deposit) even when shed");
        assert!(
            matches!(err, TransportError::Transient(_)),
            "must be Transient so the deposit rung fires, got {err:?}"
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
