//! ZEB-487: read-only DTOs + mapper for the headless `get_relay_held`
//! observability RPC. The relay holds blobs SEALED to the recipient's device
//! key — it cannot see the DM `space_id` or plaintext. Only routing metadata
//! (sender/recipient owner, community, the sealed-blob content id, timestamps)
//! is exposed. The content id is the recipient's CAS id for the held blob and
//! uniquely identifies the entry (the hold-doc map key is
//! `"{recipientOwnerHex}:{contentIdHex}"`).

use crate::community_relay_hold_crdt::RelayHoldDoc;
use crate::owner_state_types::SpaceId;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayHeldEntryDto {
    pub sender_owner_hex: String,
    pub recipient_owner_hex: String,
    pub community_id_hex: String,
    pub content_id_hex: String,
    pub held_at_ms: u64,
    pub held_by_device: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RelayHeldResponse {
    pub held: Vec<RelayHeldEntryDto>,
}

/// Map the relay-hold doc into DTOs, optionally filtered to one community.
/// Pure (no NodeState / no I/O) so it is unit-testable in isolation.
pub fn map_relay_held(
    doc: &RelayHoldDoc,
    community_filter: Option<&SpaceId>,
) -> Vec<RelayHeldEntryDto> {
    doc.entries
        .iter()
        // match (not `is_none_or`/`map_or`) sidesteps the MSRV-vs-clippy tension:
        // `is_none_or` needs Rust 1.82 (the `msrv` CI job may pin older), while
        // `map_or(true, …)` trips clippy::unnecessary_map_or on a recent toolchain.
        .filter(|(_, e)| match community_filter {
            Some(c) => &e.community_id == c,
            None => true,
        })
        .map(|(key, e)| {
            // key = "{recipientOwnerHex}:{contentIdHex}"
            let content_id_hex = key
                .rsplit_once(':')
                .map(|(_, c)| c.to_string())
                .unwrap_or_else(|| key.clone());
            RelayHeldEntryDto {
                sender_owner_hex: hex::encode(e.sender_owner),
                recipient_owner_hex: hex::encode(e.recipient_owner),
                community_id_hex: hex::encode(e.community_id.0),
                content_id_hex,
                held_at_ms: e.held_at.wall_ms,
                held_by_device: e.held_by.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_relay_hold_crdt::{RelayHoldDoc, RelayHoldEntry};
    use crate::owner_state_types::{Hlc, SpaceId};

    fn entry(so: u8, ro: u8, c: SpaceId, dev: &str) -> RelayHoldEntry {
        RelayHoldEntry {
            recipient_owner: [ro; 16],
            sender_owner: [so; 16],
            community_id: c,
            sealed_blob: vec![1, 2, 3],
            held_at: Hlc {
                wall_ms: 1234,
                logical: 0,
                device_id: dev.into(),
            },
            held_by: dev.into(),
            pulled_by: Default::default(),
        }
    }

    #[test]
    fn maps_entries_with_optional_community_filter() {
        let c1 = SpaceId([0x11; 16]);
        let c2 = SpaceId([0x22; 16]);
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            format!("{}:{}", hex::encode([0xBB; 16]), hex::encode([0xCC; 32])),
            entry(0xAA, 0xBB, c1, "relaydev1"),
        );
        doc.entries.insert(
            format!("{}:{}", hex::encode([0xFF; 16]), hex::encode([0xEE; 32])),
            entry(0xDD, 0xFF, c2, "relaydev1"),
        );

        let all = map_relay_held(&doc, None);
        assert_eq!(all.len(), 2);

        let filtered = map_relay_held(&doc, Some(&c1));
        assert_eq!(filtered.len(), 1);
        let dto = &filtered[0];
        assert_eq!(dto.sender_owner_hex, hex::encode([0xAA; 16]));
        assert_eq!(dto.recipient_owner_hex, hex::encode([0xBB; 16]));
        assert_eq!(dto.community_id_hex, hex::encode([0x11; 16]));
        assert_eq!(dto.content_id_hex, hex::encode([0xCC; 32]));
        assert_eq!(dto.held_at_ms, 1234);
        assert_eq!(dto.held_by_device, "relaydev1");

        assert!(map_relay_held(&RelayHoldDoc::default(), None).is_empty());
    }
}
