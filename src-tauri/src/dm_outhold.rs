//! dm-outhold-v1: content side-table for pending outbound DMs, fleet-replicated
//! via FleetSyncEngine (ZEB-418 P2). Holds the encrypted CAS blob of each
//! PENDING outbound DM so sibling devices can complete delivery.
//! NOT a DM-history migration (spec D6/D14).

use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Inbound zenoh size cap for dm-outhold-v1 full-doc CRDT sync frames
/// (ZEB-418 P2, PR #222 round 1). The dataset replicates the WHOLE doc per
/// frame, so the cap bounds a doc of ~64 max-size deposit blobs
/// ([`crate::butler_deposit::DEPOSIT_MAX_FRAME_BYTES`] each — 16 MiB
/// total). Outbox expiry GC bounds real growth well below this; anything
/// larger is a malformed or hostile peer frame and is dropped before
/// allocation.
pub const DM_OUTHOLD_DATASET_MAX_BYTES: usize = 64 * crate::butler_deposit::DEPOSIT_MAX_FRAME_BYTES;

/// Key = "{space_id_hex}:{message_cid_hex}" — same composite as DmInboxDoc::key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmOutholdEntry {
    /// The CAS storage blob ([ver][nonce][ct][tag]) — already encrypted.
    #[serde(rename = "pl", with = "serde_bytes")]
    pub storage_blob: Vec<u8>,
    #[serde(rename = "sp")]
    pub space_id: [u8; 16],
    #[serde(rename = "ca")]
    pub created_at: Hlc,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmOutholdDoc {
    #[serde(rename = "en")]
    pub entries: BTreeMap<String, DmOutholdEntry>,
}

// Manual CanonicalPayload registration: the `impl_canonical!` macro in
// owner_state_types.rs is module-private, so we register these types with the
// two impls the macro expands to (mirroring `notes_crdt` and `dm_inbox_crdt`).
impl CanonicalPayloadSealed for DmOutholdEntry {}
impl CanonicalPayload for DmOutholdEntry {}
impl CanonicalPayloadSealed for DmOutholdDoc {}
impl CanonicalPayload for DmOutholdDoc {}

impl DmOutholdDoc {
    pub fn key(space_id: &[u8; 16], message_cid: &[u8]) -> String {
        format!("{}:{}", hex::encode(space_id), hex::encode(message_cid))
    }

    /// Insert-once union merge. A key already present locally is NEVER
    /// overwritten — content-addressed: same key carries identical bytes by
    /// construction, so first-writer-wins is exact. `changed` is true only when
    /// a new key was inserted.
    ///
    /// # Resurrection invariant
    /// Removal does NOT replicate (state-CRDT). A locally-GC'd row can
    /// resurrect from a stale sibling's publish, which is harmless: the
    /// status-driven GC sweep (Task 5) re-deletes any row whose matching outbox
    /// entry is terminal. Converges because outbox status replicates via
    /// OwnerState.
    pub fn merge_from(&mut self, remote: DmOutholdDoc) -> MergeOutcome {
        let mut changed = false;
        for (k, r) in remote.entries {
            if let std::collections::btree_map::Entry::Vacant(slot) = self.entries.entry(k) {
                slot.insert(r);
                changed = true;
            }
            // Key already present: identical bytes (content-addressed) — no overwrite.
        }
        MergeOutcome { changed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::Hlc;

    fn hlc(w: u64, d: &str) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: d.into(),
        }
    }

    fn entry(blob: Vec<u8>, at: Hlc) -> DmOutholdEntry {
        DmOutholdEntry {
            storage_blob: blob,
            space_id: [0x11u8; 16],
            created_at: at,
        }
    }

    fn key() -> String {
        DmOutholdDoc::key(&[0x11u8; 16], &[0x22u8; 32])
    }

    #[test]
    fn merge_inserts_new_entry_and_is_idempotent() {
        let mut a = DmOutholdDoc::default();
        let mut b = DmOutholdDoc::default();
        b.entries
            .insert(key(), entry(vec![0xAA, 0xBB, 0xCC], hlc(1, "B")));

        let out = a.merge_from(b.clone());
        assert!(out.changed, "new entry must flag changed");
        assert_eq!(a.entries.len(), 1);
        assert_eq!(a.entries[&key()], b.entries[&key()]);

        // Re-merge of identical doc must be a no-op.
        let out = a.merge_from(b.clone());
        assert!(!out.changed, "re-merge of identical doc is a no-op");
        assert_eq!(a, b);
    }

    #[test]
    fn merge_never_overwrites_existing_entry() {
        // Local entry has blob A; remote same key has blob B.
        // Invariant: local keeps A; changed = false.
        let mut local = DmOutholdDoc::default();
        local
            .entries
            .insert(key(), entry(vec![0xAA], hlc(1, "local")));

        let mut remote = DmOutholdDoc::default();
        remote
            .entries
            .insert(key(), entry(vec![0xBB], hlc(2, "remote")));

        let out = local.merge_from(remote);
        assert!(!out.changed, "no change when existing key skipped");
        assert_eq!(
            local.entries[&key()].storage_blob,
            vec![0xAA],
            "local blob must not be overwritten"
        );
    }

    #[test]
    fn merge_unchanged_when_remote_subset() {
        // Local has two entries; remote has only one of them.
        let k1 = DmOutholdDoc::key(&[0x01u8; 16], &[0x02u8; 32]);
        let k2 = DmOutholdDoc::key(&[0x03u8; 16], &[0x04u8; 32]);

        let mut local = DmOutholdDoc::default();
        local
            .entries
            .insert(k1.clone(), entry(vec![1], hlc(1, "d1")));
        local
            .entries
            .insert(k2.clone(), entry(vec![2], hlc(2, "d2")));

        let mut remote = DmOutholdDoc::default();
        remote
            .entries
            .insert(k1.clone(), entry(vec![1], hlc(1, "d1")));

        let out = local.merge_from(remote);
        assert!(!out.changed, "remote is a strict subset — no change");
        assert_eq!(local.entries.len(), 2, "both entries still present");
    }

    /// Pins the dm-outhold-v1 wire format. NEVER regenerate — any change to
    /// this hex means the on-disk/over-the-wire encoding changed and old peers
    /// would break.
    #[test]
    fn outhold_doc_canonical_cbor_pinned() {
        use ciborium::into_writer;

        // Fixed deterministic fixture values.
        let space_id = [0x11u8; 16];
        let cid_bytes = [0x22u8; 32];
        let blob = vec![0xAAu8, 0xBB, 0xCC];
        let created_at = Hlc {
            wall_ms: 1234,
            logical: 0,
            device_id: "dev-fixture".into(),
        };

        let mut doc = DmOutholdDoc::default();
        doc.entries.insert(
            DmOutholdDoc::key(&space_id, &cid_bytes),
            DmOutholdEntry {
                storage_blob: blob,
                space_id,
                created_at,
            },
        );

        let mut buf = Vec::new();
        into_writer(&doc, &mut buf).expect("encode");
        let actual = hex::encode(&buf);

        // Pins the dm-outhold-v1 wire format; NEVER regenerate.
        const EXPECTED_OUTHOLD_DOC_HEX: &str = "a162656ea1786131313131313131313131313131313131313131313131313131313131313131313a32323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232a362706c43aabbcc6273709011111111111111111111111111111111626361a361771904d2616c0061646b6465762d66697874757265";
        assert_eq!(
            actual, EXPECTED_OUTHOLD_DOC_HEX,
            "DmOutholdDoc wire encoding drifted from pinned fixture.\nactual hex: {actual}"
        );
    }
}
