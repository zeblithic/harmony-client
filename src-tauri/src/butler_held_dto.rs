//! ZEB-489: read-only DTOs + mapper for the headless butler observability RPCs
//! (`get_butler_held`, `get_butler_pin`). The butler is the recipient's OWN
//! fleet device, so — unlike the third-party relay — the inbox key exposes the
//! DM `space_id` + `message_cid`, and `ingested_by` is the built-in
//! "recovered/cleared" signal. The sealed/bulky payload (`cidnotify_packet`,
//! `storage_blob`, `invite_packet`) is NEVER exposed.

use crate::dm_inbox_crdt::DmInboxDoc;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ButlerHeldEntryDto {
    pub sender_owner_hex: String,
    pub space_id_hex: String,
    pub message_cid_hex: String,
    pub deposited_at_ms: u64,
    pub deposited_by_device: String,
    pub ingested_by_devices: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ButlerHeldResponse {
    pub held: Vec<ButlerHeldEntryDto>,
}

/// ZEB-489: status of this fleet's pinned butler device. `pinned_at_ms` is
/// `Some` only when a butler is actually pinned — a never-set/cleared pin has a
/// sentinel HLC (`wall_ms == 0`) that would otherwise read as a bogus 1970
/// timestamp, so it is reported as `None` rather than `0`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ButlerPinStatus {
    pub pinned_device_id: Option<String>,
    pub pinned_at_ms: Option<u64>,
}

/// Map the butler dm-inbox doc into DTOs. Pure (no NodeState / no I/O) so it is
/// unit-testable in isolation.
pub fn map_butler_held(doc: &DmInboxDoc) -> Vec<ButlerHeldEntryDto> {
    doc.entries
        .iter()
        .map(|(key, e)| {
            // key = "{space_id_hex}:{message_cid_hex}" (DmInboxDoc::key). Both
            // halves are pure hex, so the FIRST ':' is the unambiguous separator.
            // A key with no ':' can't arise from DmInboxDoc::key, but a corrupted
            // replica could merge one in — surface the entry with EMPTY ids
            // rather than mis-presenting the raw key as a space id (Qodo).
            let (space_id_hex, message_cid_hex) = key
                .split_once(':')
                .map(|(s, c)| (s.to_string(), c.to_string()))
                .unwrap_or_default();
            ButlerHeldEntryDto {
                sender_owner_hex: hex::encode(e.sender_owner),
                space_id_hex,
                message_cid_hex,
                deposited_at_ms: e.deposited_at.wall_ms,
                deposited_by_device: e.deposited_by.clone(),
                ingested_by_devices: e.ingested_by.iter().cloned().collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm_inbox_crdt::{DmInboxDoc, DmInboxEntry};
    use crate::owner_state_types::Hlc;
    use std::collections::BTreeSet;

    fn entry(sender_owner: u8, dev: &str, ingested: &[&str]) -> DmInboxEntry {
        DmInboxEntry {
            sender_owner: [sender_owner; 16],
            cidnotify_packet: Some(vec![1, 2, 3]),
            storage_blob: vec![4, 5, 6],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            deposited_at: Hlc {
                wall_ms: 4242,
                logical: 0,
                device_id: dev.into(),
            },
            deposited_by: dev.into(),
            ingested_by: ingested
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<String>>(),
        }
    }

    #[test]
    fn maps_inbox_entries_with_key_split_and_ingested_set() {
        let space = [0xAB; 16];
        let cid = [0xCD; 20];
        let space2 = [0x22; 16];
        let cid2 = [0x33; 20];

        let mut doc = DmInboxDoc::default();
        doc.entries.insert(
            DmInboxDoc::key(&space, &cid),
            entry(0x11, "butlerdev", &["primarydev"]),
        );
        doc.entries.insert(
            DmInboxDoc::key(&space2, &cid2),
            entry(0x44, "butlerdev", &[]),
        );

        let held = map_butler_held(&doc);
        assert_eq!(held.len(), 2);

        let d = held
            .iter()
            .find(|d| d.space_id_hex == hex::encode(space))
            .unwrap();
        assert_eq!(d.sender_owner_hex, hex::encode([0x11u8; 16]));
        assert_eq!(d.space_id_hex, hex::encode(space));
        assert_eq!(d.message_cid_hex, hex::encode(cid));
        assert_eq!(d.deposited_at_ms, 4242);
        assert_eq!(d.deposited_by_device, "butlerdev");
        assert_eq!(d.ingested_by_devices, vec!["primarydev".to_string()]);

        let d2 = held
            .iter()
            .find(|d| d.space_id_hex == hex::encode(space2))
            .unwrap();
        assert!(d2.ingested_by_devices.is_empty());

        assert!(map_butler_held(&DmInboxDoc::default()).is_empty());
    }

    #[test]
    fn malformed_key_without_colon_yields_empty_ids() {
        let mut doc = DmInboxDoc::default();
        doc.entries
            .insert("no-colon-here".to_string(), entry(0x55, "butlerdev", &[]));
        let held = map_butler_held(&doc);
        assert_eq!(held.len(), 1);
        // Surfaced (a held blob exists) but with clearly-empty ids rather than
        // the raw key mis-presented as a space id.
        assert_eq!(held[0].space_id_hex, "");
        assert_eq!(held[0].message_cid_hex, "");
        assert_eq!(held[0].sender_owner_hex, hex::encode([0x55u8; 16]));
    }
}
