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
    /// # Key ↔ payload binding (ZEB-997)
    /// A remote row is admitted only if its key equals the key a well-behaved
    /// writer would mint for its payload: `"{hex(entry.space_id)}:{hex(cid)}"`
    /// with `cid = ContentId::for_book(storage_blob, {encrypted: true})` —
    /// the exact construction of the production insert site (`dm_outbox`
    /// step 5; the flags here must stay in lockstep with it). Without this,
    /// a corrupt or hostile sibling frame could persist a row the outhold
    /// sweeper admits into CAS under a false CID and repeatedly attempts to
    /// deliver — the binding is otherwise only checked much later at
    /// ingestion. Invalid rows are dropped with telemetry; the rest of the
    /// snapshot still applies (drop-don't-abort, mirroring the sync-ingest
    /// convention). This doc has no tombstones (removal does not replicate),
    /// so validation gates every remote row.
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
                // ZEB-997: recompute the writer's canonical key from the
                // payload itself; one string compare then binds the CID half,
                // the space prefix, and the key shape simultaneously.
                let expected = match harmony_content::cid::ContentId::for_book(
                    &r.storage_blob,
                    harmony_content::cid::ContentFlags {
                        encrypted: true,
                        ..Default::default()
                    },
                ) {
                    Ok(cid) => Self::key(&r.space_id, &cid.to_bytes()),
                    Err(e) => {
                        tracing::warn!(
                            key = %slot.key(),
                            error = %e,
                            "ZEB-997 outhold merge: CID uncomputable for remote row — dropped"
                        );
                        continue;
                    }
                };
                if slot.key() != &expected {
                    tracing::warn!(
                        key = %slot.key(),
                        "ZEB-997 outhold merge: key does not bind to payload — dropped"
                    );
                    continue;
                }
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

    /// The key a well-behaved writer mints for `e` — same construction as the
    /// production insert site (`dm_outbox` step 5): CID over the encrypted
    /// storage blob with `encrypted: true`.
    fn valid_key(e: &DmOutholdEntry) -> String {
        let cid = harmony_content::cid::ContentId::for_book(
            &e.storage_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("fixture blob under payload cap");
        DmOutholdDoc::key(&e.space_id, &cid.to_bytes())
    }

    #[test]
    fn merge_inserts_new_entry_and_is_idempotent() {
        let e = entry(vec![0xAA, 0xBB, 0xCC], hlc(1, "B"));
        let k = valid_key(&e);
        let mut a = DmOutholdDoc::default();
        let mut b = DmOutholdDoc::default();
        b.entries.insert(k.clone(), e);

        let out = a.merge_from(b.clone());
        assert!(out.changed, "new entry must flag changed");
        assert_eq!(a.entries.len(), 1);
        assert_eq!(a.entries[&k], b.entries[&k]);

        // Re-merge of identical doc must be a no-op.
        let out = a.merge_from(b.clone());
        assert!(!out.changed, "re-merge of identical doc is a no-op");
        assert_eq!(a, b);
    }

    #[test]
    fn merge_never_overwrites_existing_entry() {
        // Local entry has blob A; remote same key has blob B.
        // Invariant: local keeps A; changed = false. The present-key skip
        // happens BEFORE ZEB-997 validation — an existing local row is never
        // re-judged (it was validated or locally minted when it entered).
        let local_entry = entry(vec![0xAA], hlc(1, "local"));
        let k = valid_key(&local_entry);
        let mut local = DmOutholdDoc::default();
        local.entries.insert(k.clone(), local_entry);

        let mut remote = DmOutholdDoc::default();
        remote
            .entries
            .insert(k.clone(), entry(vec![0xBB], hlc(2, "remote")));

        let out = local.merge_from(remote);
        assert!(!out.changed, "no change when existing key skipped");
        assert_eq!(
            local.entries[&k].storage_blob,
            vec![0xAA],
            "local blob must not be overwritten"
        );
    }

    #[test]
    fn merge_unchanged_when_remote_subset() {
        // Local has two entries; remote has only one of them.
        let e1 = entry(vec![1], hlc(1, "d1"));
        let e2 = entry(vec![2], hlc(2, "d2"));
        let k1 = valid_key(&e1);
        let k2 = valid_key(&e2);

        let mut local = DmOutholdDoc::default();
        local.entries.insert(k1.clone(), e1.clone());
        local.entries.insert(k2.clone(), e2);

        let mut remote = DmOutholdDoc::default();
        remote.entries.insert(k1, e1);

        let out = local.merge_from(remote);
        assert!(!out.changed, "remote is a strict subset — no change");
        assert_eq!(local.entries.len(), 2, "both entries still present");
    }

    // ── ZEB-997: merge-time key ↔ payload binding validation ─────────────────

    #[test]
    fn merge_drops_row_whose_key_cid_does_not_match_blob() {
        // Key claims CID 0x22…22 but the blob hashes to something else — the
        // exact shape a corrupt/hostile sibling frame would carry. Pre-ZEB-997
        // this row persisted and the sweeper redelivered it under a false CID.
        let mut remote = DmOutholdDoc::default();
        remote.entries.insert(
            DmOutholdDoc::key(&[0x11u8; 16], &[0x22u8; 32]),
            entry(vec![0xAA, 0xBB, 0xCC], hlc(1, "evil")),
        );

        let mut local = DmOutholdDoc::default();
        let out = local.merge_from(remote);
        assert!(!out.changed, "invalid row must not flag changed");
        assert!(local.entries.is_empty(), "invalid row must be dropped");
    }

    #[test]
    fn merge_drops_row_whose_space_prefix_does_not_match_entry() {
        // CID half is honest (matches the blob) but the key's space prefix
        // disagrees with entry.space_id.
        let e = entry(vec![0xAA, 0xBB, 0xCC], hlc(1, "evil"));
        let honest = valid_key(&e);
        let cid_hex = honest.split(':').nth(1).unwrap().to_string();
        let forged = format!("{}:{}", hex::encode([0x99u8; 16]), cid_hex);

        let mut remote = DmOutholdDoc::default();
        remote.entries.insert(forged, e);

        let mut local = DmOutholdDoc::default();
        let out = local.merge_from(remote);
        assert!(!out.changed);
        assert!(local.entries.is_empty(), "space-mismatched row dropped");
    }

    #[test]
    fn merge_drops_invalid_rows_without_aborting_snapshot() {
        // One valid + one invalid row in the same remote doc: the valid row
        // must still apply (drop-don't-abort).
        let good = entry(vec![0x01, 0x02], hlc(1, "sib"));
        let good_key = valid_key(&good);

        let mut remote = DmOutholdDoc::default();
        remote.entries.insert(good_key.clone(), good.clone());
        remote.entries.insert(
            DmOutholdDoc::key(&[0x11u8; 16], &[0x22u8; 32]),
            entry(vec![0xAA], hlc(2, "evil")),
        );

        let mut local = DmOutholdDoc::default();
        let out = local.merge_from(remote);
        assert!(out.changed, "the valid row still applies");
        assert_eq!(local.entries.len(), 1);
        assert_eq!(local.entries[&good_key], good);
    }

    #[test]
    fn merge_drops_row_with_oversized_blob() {
        // Blob past ContentId's payload cap: for_book errors, so the binding
        // cannot even be computed — the row is dropped, not inserted blind.
        let big = entry(vec![0u8; 0x10_0000], hlc(1, "evil")); // 1 MiB + 1 > 0xF_FFFF
        let mut remote = DmOutholdDoc::default();
        remote
            .entries
            .insert(DmOutholdDoc::key(&big.space_id, &[0x22u8; 32]), big);

        let mut local = DmOutholdDoc::default();
        let out = local.merge_from(remote);
        assert!(!out.changed);
        assert!(local.entries.is_empty(), "uncomputable binding → dropped");
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
