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

use crate::dm_outbox::{build_dm_packet, DmTransport, TransportError};
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_types::{DeviceIdentityHash, OutboxEntry, OwnerAddr};
use crate::tunnel_manager::{node_id_from_dsa_pubkey, TunnelManager};

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
}

impl IrohTunnelDmTransport {
    pub fn new(
        mgr: Arc<TunnelManager>,
        crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
        signing_key: Arc<ed25519_dalek::SigningKey>,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
    ) -> Self {
        Self {
            mgr,
            crdt_state,
            signing_key,
            self_owner,
            our_signing_device_hash,
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

        if !targets.is_empty() {
            let signed = crate::dm_envelope::DmCidNotifySigned {
                space_id: entry.space_id,
                message_cid: entry.message_cid,
                sender_owner_addr: self.self_owner,
                sender_devices: vec![self.our_signing_device_hash],
                signing_device_hash: self.our_signing_device_hash,
            };
            // Build ONCE; the same wire bytes go to every device (the recipient
            // dedups across devices by CID anyway).
            match build_dm_packet(signed, &self.signing_key) {
                Ok(packet) => {
                    for (node_id, contact) in &targets {
                        // Fire-and-forget liveness attempt: lazily dials/reuses
                        // the per-device PQ tunnel. Never blocks on connect.
                        self.mgr.send_dm(*node_id, contact, packet.clone());
                    }
                }
                Err(e) => {
                    // Packet build failure is local + permanent-ish, but we
                    // still fall through to the deposit rung below (the deposit
                    // path rebuilds its own packet), so just log.
                    tracing::warn!(
                        recipient = ?recipient,
                        error = %e,
                        "ZEB-473: tunnel DM packet build failed; deposit rung still covers this DM"
                    );
                }
            }
        }

        // ALWAYS return Transient (never Ok): the tunnel is a parallel
        // liveness attempt; the outbox's deposit rung is the durability
        // guarantee and must fire for every DM (spec §5.7). See module docs for
        // why Transient (not Ok) is the correct contract.
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
    ) -> IrohTunnelDmTransport {
        let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]));
        IrohTunnelDmTransport::new(
            mgr,
            state,
            signing_key,
            OwnerAddr([0xff; 16]),
            DeviceIdentityHash([0xaa; 16]),
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

        let transport = make_transport(Arc::clone(&mgr), state);

        let space = SpaceId([0xcc; 16]);
        let cid = ContentId::from_bytes([0xee; 32]);
        let entry = synthetic_outbox_entry(space, cid, recipient);

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
        let pending = mgr
            .test_pending_packets(&expected_node_id)
            .expect("send_dm must have registered a session for the derived NodeId");
        assert_eq!(pending.len(), 1, "exactly one DM routed to the tunnel");

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
        assert_eq!(
            pending[0], expected,
            "routed packet must match build_dm_packet"
        );

        // And no spurious session for any other NodeId.
        assert!(
            mgr.test_pending_packets(&[0x00; 32]).is_none(),
            "no session for an unrelated NodeId"
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
        let transport = make_transport(Arc::clone(&mgr), state);

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
        let transport = make_transport(mgr, state);

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
}
