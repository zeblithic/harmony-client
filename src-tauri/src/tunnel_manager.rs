//! ZEB-473 (DM-over-iroh, Move 1a): the per-peer PQ tunnel session map.
//!
//! ZEB-739 (iroh-tier extraction): the `TunnelManager` (session map + lazy dial +
//! collision dedup) and the `TunnelPeer` descriptor now live in the reusable
//! [`harmony_tunnel_iroh`] crate and are **re-exported** here so every existing
//! `crate::tunnel_manager::…` path keeps compiling. Two app-coupling seams stay
//! client-side:
//!
//! * **Seam A** — the app's owner-state [`DeviceTunnelContact`] is mapped into the
//!   crate-owned [`TunnelPeer`] at the [`TunnelManager::send_dm`] boundary. The
//!   fallible reconstruction (PQ identity + lenient relay parse) lives in
//!   [`tunnel_peer_from_contact`] here.
//! * **Seam B** — the `CompatSink` trait replaces the concrete
//!   `ProtocolCompatRegistry`; the client impls it in `protocol_versioning.rs`.

use harmony_identity::PqIdentity;

pub use harmony_tunnel_iroh::{
    node_id_from_dsa_pubkey, CompatSink, HandshakeOutcome, InboundDm, TunnelCommand, TunnelManager,
    TunnelPeer,
};

use crate::owner_state_types::DeviceTunnelContact;

/// Map the app's owner-state [`DeviceTunnelContact`] into the crate-owned
/// [`TunnelPeer`] the tunnel driver dials + authenticates against (Seam A).
///
/// **Fallible** — the PQ identity is reconstructed here (it was rebuilt inside
/// the client's old `run_tunnel_initiator`/`peer_pq_identity`), so a contact with
/// malformed / wrong-length key bytes yields `Err`. The caller skips that device:
/// it's unreachable over the tunnel, and the always-deposit rung covers
/// durability — the same net effect as the old driver's `note_dial_failed` bail
/// on a bad contact.
///
/// * `iroh_node_id` → [`TunnelPeer::node_id`] verbatim (the peer's iroh
///   `EndpointId` bytes — the dial target, NOT the tunnel node id).
/// * `pq_kem_pubkey` ‖ `pq_dsa_pubkey` → [`TunnelPeer::pq_identity`] via
///   [`PqIdentity::from_public_bytes`] (KEM first, then DSA — the order
///   `from_public_bytes` expects).
/// * `home_relay_url: Option<String>` → [`TunnelPeer::home_relay`] via the same
///   LENIENT parse the old `dial_addr` used: empty / `None` → `None`; a malformed
///   URL is trace-logged and dropped to `None` (never a hard error).
pub fn tunnel_peer_from_contact(contact: &DeviceTunnelContact) -> Result<TunnelPeer, String> {
    // Reconstruct the peer's PQ identity from `[ML-KEM pub (1184)][ML-DSA pub
    // (1952)]`, exactly the order `PqIdentity::from_public_bytes` expects.
    let mut combined =
        Vec::with_capacity(contact.pq_kem_pubkey.len() + contact.pq_dsa_pubkey.len());
    combined.extend_from_slice(&contact.pq_kem_pubkey);
    combined.extend_from_slice(&contact.pq_dsa_pubkey);
    let pq_identity =
        PqIdentity::from_public_bytes(&combined).map_err(|e| format!("peer PQ identity: {e:?}"))?;

    // Lenient home-relay parse: empty / None → None; malformed → trace + None.
    let home_relay = contact.home_relay_url.as_deref().and_then(|relay| {
        if relay.is_empty() {
            return None;
        }
        match relay.parse::<iroh::RelayUrl>() {
            Ok(url) => Some(url),
            Err(e) => {
                tracing::trace!(
                    relay = %relay,
                    "ZEB-473/739: skip malformed tunnel home_relay_url: {e}"
                );
                None
            }
        }
    });

    Ok(TunnelPeer {
        node_id: contact.iroh_node_id,
        pq_identity,
        home_relay,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{ML_DSA_65_PUBKEY_LEN, ML_KEM_768_PUBKEY_LEN};
    use harmony_identity::PqPrivateIdentity;

    /// A contact built from a REAL PQ identity's public bytes maps `Ok` and the
    /// reconstructed identity round-trips (`address_hash` matches) — so the tunnel
    /// initiator authenticates the right peer.
    #[test]
    fn tunnel_peer_from_contact_roundtrips_real_identity() {
        let id = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let pubid = id.public_identity();
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: Some("https://relay.example/".to_string()),
            pq_dsa_pubkey: pubid.verifying_key.as_bytes().to_vec(),
            pq_kem_pubkey: pubid.encryption_key.as_bytes().to_vec(),
        };
        let peer = tunnel_peer_from_contact(&contact).expect("real-key contact must map Ok");
        assert_eq!(
            peer.node_id, [0x09; 32],
            "node_id is the iroh EndpointId verbatim"
        );
        assert_eq!(
            peer.pq_identity.address_hash, pubid.address_hash,
            "reconstructed PQ identity must match the source identity"
        );
        assert!(peer.home_relay.is_some(), "a valid relay url must parse");
    }

    /// TEETH (ZEB-739, guardrail): the synthetic fixtures the dm_transport /
    /// dm_outbox routing tests use — correctly-SIZED all-same-byte PQ keys — MUST
    /// reconstruct. `PqIdentity::from_public_bytes` validates length only (plus the
    /// ML-KEM modulus bound, which small byte values pass). If that ever tightens
    /// to a full structural/re-encode check, this fails LOUDLY here rather than
    /// letting those routing tests silently take `resolve_owner_tunnel_targets`'
    /// skip-device branch (which would make their negative assertions pass
    /// vacuously). Uses the exact byte patterns those fixtures use.
    #[test]
    fn tunnel_peer_from_contact_accepts_correctly_sized_synthetic_keys() {
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![0x07u8; ML_DSA_65_PUBKEY_LEN],
            pq_kem_pubkey: vec![0x08u8; ML_KEM_768_PUBKEY_LEN],
        };
        assert!(
            tunnel_peer_from_contact(&contact).is_ok(),
            "correctly-sized synthetic PQ keys must reconstruct a TunnelPeer"
        );
    }

    /// Wrong-length keys can't reconstruct → `Err` → the resolver skips the device
    /// (unreachable; the always-deposit rung covers durability).
    #[test]
    fn tunnel_peer_from_contact_rejects_wrong_length_keys() {
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![0x07u8; 32],
            pq_kem_pubkey: vec![0x08u8; 32],
        };
        assert!(tunnel_peer_from_contact(&contact).is_err());
    }

    /// A malformed home-relay url is leniently dropped to `None` (never a hard
    /// error) — matching the old `dial_addr` behavior.
    #[test]
    fn tunnel_peer_from_contact_tolerates_malformed_relay() {
        let id = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let pubid = id.public_identity();
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: Some("not a url".to_string()),
            pq_dsa_pubkey: pubid.verifying_key.as_bytes().to_vec(),
            pq_kem_pubkey: pubid.encryption_key.as_bytes().to_vec(),
        };
        let peer = tunnel_peer_from_contact(&contact).expect("map Ok despite bad relay");
        assert!(
            peer.home_relay.is_none(),
            "a malformed relay url must drop to None, not error"
        );
    }
}
