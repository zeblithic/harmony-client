//! Butler dm-inbox CRDT (ZEB-418 P1): deposited-but-not-yet-ingested DM
//! deliveries, replicated across the owner's fleet via FleetSyncEngine.
//! NOT a migration of DM history (spec D6).

use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Key = "{space_id_hex}:{message_cid_hex}" — mirrors InboxKey, string-keyed
/// for canonical CBOR map encoding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmInboxEntry {
    #[serde(rename = "so")]
    pub sender_owner: [u8; 16],
    /// Full signed CidNotify packet bytes (discriminant+body+sig).
    #[serde(rename = "cn", with = "serde_bytes")]
    pub cidnotify_packet: Vec<u8>,
    /// The CAS storage blob ([ver][nonce][ct][tag]).
    #[serde(rename = "pl", with = "serde_bytes")]
    pub storage_blob: Vec<u8>,
    #[serde(rename = "da")]
    pub deposited_at: Hlc,
    /// SP1 device_id (64-hex).
    #[serde(rename = "db")]
    pub deposited_by: String,
    #[serde(rename = "ig", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub ingested_by: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmInboxDoc {
    #[serde(rename = "en")]
    pub entries: BTreeMap<String, DmInboxEntry>,
}

// Manual CanonicalPayload registration: the `impl_canonical!` macro in
// owner_state_types.rs is module-private, so we register these types with the
// two impls the macro expands to (mirroring `notes_crdt`).
impl CanonicalPayloadSealed for DmInboxEntry {}
impl CanonicalPayload for DmInboxEntry {}
impl CanonicalPayloadSealed for DmInboxDoc {}
impl CanonicalPayload for DmInboxDoc {}

impl DmInboxDoc {
    pub fn key(space_id: &[u8; 16], message_cid: &[u8]) -> String {
        format!("{}:{}", hex::encode(space_id), hex::encode(message_cid))
    }

    /// Insert-once + ig-union merge. Same key redeposited carries identical
    /// payload (same CidNotify + blob), so first-writer-wins by `da` is safe;
    /// `ingested_by` always merges by union (grow-only set — concurrent
    /// ingestion by siblings can never race). `changed` flags only new
    /// entries or ig growth (it drives `on_applied` → ingestion wakeups;
    /// deposit-metadata churn must not wake anything).
    pub fn merge_from(&mut self, remote: DmInboxDoc) -> MergeOutcome {
        let mut changed = false;
        for (k, r) in remote.entries {
            match self.entries.get_mut(&k) {
                None => {
                    changed = true;
                    self.entries.insert(k, r);
                }
                Some(l) => {
                    let before = l.ingested_by.len();
                    l.ingested_by.extend(r.ingested_by);
                    // Keep earliest deposit metadata (first-writer-wins):
                    // only when the local entry is strictly newer does the
                    // remote's earlier deposit replace it.
                    if l.deposited_at.is_strictly_newer_than(&r.deposited_at) {
                        l.deposited_at = r.deposited_at;
                        l.deposited_by = r.deposited_by;
                    }
                    changed |= l.ingested_by.len() != before;
                }
            }
        }
        MergeOutcome { changed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::Hlc;
    use std::collections::BTreeSet;

    fn hlc(w: u64, d: &str) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: d.into(),
        }
    }

    fn entry(at: Hlc, by: &str, ig: &[&str]) -> DmInboxEntry {
        DmInboxEntry {
            sender_owner: [7u8; 16],
            cidnotify_packet: vec![1, 2, 3],
            storage_blob: vec![4, 5, 6],
            deposited_at: at,
            deposited_by: by.into(),
            ingested_by: ig.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn key() -> String {
        DmInboxDoc::key(&[1u8; 16], &[2u8; 32])
    }

    #[test]
    fn merge_inserts_new_entry_and_is_idempotent() {
        let mut a = DmInboxDoc::default();
        let mut b = DmInboxDoc::default();
        b.entries.insert(key(), entry(hlc(1, "B"), "dev-b", &[]));

        let out = a.merge_from(b.clone());
        assert!(out.changed, "new entry must flag changed");
        assert_eq!(a.entries.len(), 1);
        assert_eq!(a.entries[&key()], b.entries[&key()]);

        let out = a.merge_from(b.clone());
        assert!(!out.changed, "re-merge of identical doc is a no-op");
        assert_eq!(a, b);
    }

    #[test]
    fn ingested_by_merges_by_union_no_lww_race() {
        let mut base = DmInboxDoc::default();
        base.entries.insert(key(), entry(hlc(1, "X"), "dev-x", &[]));

        // Concurrent ingestion acks on two replicas of the same entry.
        let mut a = base.clone();
        a.entries
            .get_mut(&key())
            .unwrap()
            .ingested_by
            .insert("dev-1".into());
        let mut b = base.clone();
        b.entries
            .get_mut(&key())
            .unwrap()
            .ingested_by
            .insert("dev-2".into());

        let out = a.merge_from(b.clone());
        assert!(out.changed, "ig growth must flag changed");
        let both: BTreeSet<String> = ["dev-1".to_string(), "dev-2".to_string()].into();
        assert_eq!(a.entries[&key()].ingested_by, both);

        let mut b2 = b.clone();
        b2.merge_from(a.clone());
        assert_eq!(b2, a, "union merge converges from both sides");
    }

    #[test]
    fn concurrent_insert_same_key_converges() {
        // Same key deposited on two butlers: payload identical by invariant,
        // deposit metadata differs. First-writer-wins on deposited_at.
        let mut a = DmInboxDoc::default();
        a.entries.insert(key(), entry(hlc(5, "A"), "dev-a", &[]));
        let mut b = DmInboxDoc::default();
        b.entries.insert(key(), entry(hlc(3, "B"), "dev-b", &[]));

        let mut a2 = a.clone();
        a2.merge_from(b.clone());
        let mut b2 = b.clone();
        b2.merge_from(a.clone());

        assert_eq!(a2, b2, "both merge orders converge to one entry");
        assert_eq!(
            a2.entries[&key()].deposited_at,
            hlc(3, "B"),
            "earliest deposit wins"
        );
        assert_eq!(a2.entries[&key()].deposited_by, "dev-b");
    }

    #[test]
    fn visible_change_flag_only_on_new_entries_or_ig_growth() {
        let mut a = DmInboxDoc::default();
        a.entries
            .insert(key(), entry(hlc(5, "A"), "dev-a", &["dev-1"]));

        // Identical remote: no change.
        let out = a.merge_from(a.clone());
        assert!(!out.changed, "identical merge is not a change");

        // Earlier deposit metadata, same ig: metadata swaps but is NOT
        // flagged (changed drives ingestion wakeups, not metadata churn).
        let mut earlier = DmInboxDoc::default();
        earlier
            .entries
            .insert(key(), entry(hlc(3, "B"), "dev-b", &["dev-1"]));
        let out = a.merge_from(earlier);
        assert!(!out.changed, "metadata-only swap must not flag changed");
        assert_eq!(a.entries[&key()].deposited_by, "dev-b");

        // ig growth: flagged.
        let mut grown = DmInboxDoc::default();
        grown
            .entries
            .insert(key(), entry(hlc(3, "B"), "dev-b", &["dev-1", "dev-2"]));
        let out = a.merge_from(grown);
        assert!(out.changed, "ig growth must flag changed");

        // ig subset of local (no growth): not flagged.
        let mut subset = DmInboxDoc::default();
        subset
            .entries
            .insert(key(), entry(hlc(3, "B"), "dev-b", &["dev-2"]));
        let out = a.merge_from(subset);
        assert!(!out.changed, "ig subset adds nothing — not a change");

        // New entry under a different key: flagged.
        let mut fresh = DmInboxDoc::default();
        fresh.entries.insert(
            DmInboxDoc::key(&[9u8; 16], &[9u8; 32]),
            entry(hlc(7, "C"), "dev-c", &[]),
        );
        let out = a.merge_from(fresh);
        assert!(out.changed, "new entry must flag changed");
    }

    #[test]
    fn cbor_round_trips_canonically() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        let mut d = DmInboxDoc::default();
        d.entries
            .insert(key(), entry(hlc(1, "A"), "dev-a", &["dev-1", "dev-2"]));
        // Also pin the empty-ig skip path round-trips.
        d.entries.insert(
            DmInboxDoc::key(&[9u8; 16], &[9u8; 32]),
            entry(hlc(2, "B"), "dev-b", &[]),
        );
        let bytes = canonical_cbor_encode(&d).expect("encode");
        let back: DmInboxDoc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, d);
    }
}
