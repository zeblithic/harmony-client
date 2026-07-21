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
    /// Signed CidNotify packet bytes (discriminant+body+sig). ZEB-505: `None`
    /// for a standalone durable DM *invite* deposit (no message) — then
    /// `invite_packet` is the sole payload and `storage_blob` is empty.
    /// Symmetric to `invite_packet`/`iv`; backward-compatible since legacy
    /// deposits always carry `cn` (decoding to `Some`).
    #[serde(
        rename = "cn",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub cidnotify_packet: Option<Vec<u8>>,
    /// The CAS storage blob ([ver][nonce][ct][tag]). Empty for an invite-only
    /// deposit (`cidnotify_packet` is `None`).
    #[serde(rename = "pl", with = "serde_bytes")]
    pub storage_blob: Vec<u8>,
    /// ZEB-483: optional signed DmInvite packet bytes, carried through from the
    /// sealed `DepositPayload` by the butler acceptor. Applied on recover to
    /// bootstrap the DM Space before CidNotify admission. `None` for non-DM /
    /// legacy deposits.
    #[serde(
        rename = "iv",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub invite_packet: Option<Vec<u8>>,
    /// ZEB-691: signed `RevocationPush` frame bytes (a `DmPacket::RevocationPush`),
    /// carried through from the sealed `DepositPayload` by the butler acceptor.
    /// Applied on recover via `handle_revocation_push`. `None` for message /
    /// invite / legacy deposits.
    #[serde(
        rename = "rp",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub revocation_push: Option<Vec<u8>>,
    /// ZEB-674 (C4): opaque `grant_push` wire value (canonical CBOR of
    /// `Vec<serde_bytes Vec<u8>>` — the per-device sealed
    /// [`crate::file_sharing::FileGrantInner`] blobs), carried through from the
    /// sealed `DepositPayload` by the butler acceptor. Applied on recover via
    /// `file_sharing::ingest_grant_push`. `None` for message / invite /
    /// revocation / legacy deposits. Symmetric to the other optional
    /// sub-payloads (`cn`/`iv`/`rp`); backward-compatible since absent → `None`.
    #[serde(
        rename = "gp",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub grant_push: Option<Vec<u8>>,
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

    /// ZEB-505: deposit key for a standalone invite-only entry (no message).
    /// The `:invite` suffix can't collide with a message key, whose second
    /// half is always 64 hex chars — so one standalone invite per space.
    pub fn invite_key(space_id: &[u8; 16]) -> String {
        format!("{}:invite", hex::encode(space_id))
    }

    /// ZEB-691: deposit key for a standalone device-revocation entry (no message,
    /// no space). Keyed by the revoking friend's owner + the revoked device id, so
    /// re-depositing the same revocation is idempotent (one entry per revoked
    /// device). The literal `revoke` first segment can never be 32 hex chars, so it
    /// cannot collide with a message key (`{space_hex}:{cid_hex}`) or an invite key
    /// (`{space_hex}:invite`).
    pub fn revoke_key(revoked_owner: &[u8; 16], revoked_target: &[u8; 16]) -> String {
        format!(
            "revoke:{}:{}",
            hex::encode(revoked_owner),
            hex::encode(revoked_target)
        )
    }

    /// ZEB-674 (C4): deposit key for a standalone file-share grant entry (no
    /// message, no invite, no revocation). Keyed by the depositing granter's
    /// owner + a blake3 hash of the opaque `grant_push` bytes, so re-depositing
    /// the SAME grant is idempotent (one entry per distinct grant payload) while
    /// two grants from the same granter (different files, or a re-seal to a
    /// changed device set) get distinct keys. The literal `grant` first segment
    /// can never be 32 hex chars, so it cannot collide with a message key
    /// (`{space_hex}:{cid_hex}`), an invite key (`{space_hex}:invite`), or a
    /// revoke key (`revoke:...`).
    pub fn grant_key(granter_owner: &[u8; 16], grant_push: &[u8]) -> String {
        format!(
            "grant:{}:{}",
            hex::encode(granter_owner),
            hex::encode(blake3::hash(grant_push).as_bytes())
        )
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
                    // ZEB-483 (CodeRabbit): reconcile the optional bootstrap
                    // invite. Same-key replicas can legitimately differ
                    // (`None` vs `Some`) — a pre-ZEB-483 entry merged against a
                    // sibling that re-deposited carrying the invite, or
                    // retry-timing skew. Promote `None → Some` so bootstrap
                    // bytes are never lost, and flag `changed` so the promotion
                    // nudges ingestion (an entry that previously rejected with
                    // `SpaceNotFound` can now bootstrap its Space). A `Some ≠
                    // Some` divergence is not expected in the common case (the
                    // invite is a deterministic rebuild of a stable Space
                    // record), but it CAN arise — e.g. a re-deposit after a
                    // content-key rotation or member change, or a multi-device
                    // sender signing the rebuilt invite with different keys. Both
                    // copies bootstrap the same Space, so keep the local one
                    // (first-writer-wins, consistent with the deposit-metadata
                    // rule below) and warn rather than churn.
                    let mut invite_promoted = false;
                    match (&l.invite_packet, &r.invite_packet) {
                        (None, Some(inv)) => {
                            l.invite_packet = Some(inv.clone());
                            invite_promoted = true;
                        }
                        (Some(a), Some(b)) if a != b => {
                            tracing::warn!(
                                key = %k,
                                "dm_inbox merge: conflicting invite_packet for same entry key; keeping local"
                            );
                        }
                        _ => {}
                    }
                    // Keep earliest deposit metadata (first-writer-wins):
                    // only when the local entry is strictly newer does the
                    // remote's earlier deposit replace it.
                    if l.deposited_at.is_strictly_newer_than(&r.deposited_at) {
                        l.deposited_at = r.deposited_at;
                        l.deposited_by = r.deposited_by;
                    }
                    changed |= l.ingested_by.len() != before || invite_promoted;
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
            cidnotify_packet: Some(vec![1, 2, 3]),
            storage_blob: vec![4, 5, 6],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
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

    /// ZEB-483 (CodeRabbit): a same-key merge must promote a missing bootstrap
    /// invite (`None → Some`) and flag `changed` so ingestion is re-nudged — an
    /// entry that previously rejected with `SpaceNotFound` can now bootstrap its
    /// Space once a sibling supplies the invite.
    #[test]
    fn merge_promotes_invite_none_to_some_and_flags_changed() {
        let mut local = DmInboxDoc::default();
        local
            .entries
            .insert(key(), entry(hlc(1, "A"), "dev-a", &[]));
        assert!(local.entries[&key()].invite_packet.is_none());

        // Remote replica of the SAME entry that carries the invite.
        let mut remote = DmInboxDoc::default();
        let mut with_invite = entry(hlc(1, "A"), "dev-a", &[]);
        with_invite.invite_packet = Some(vec![0xAA, 0xBB, 0xCC]);
        remote.entries.insert(key(), with_invite);

        let out = local.merge_from(remote);
        assert!(
            out.changed,
            "invite promotion must flag changed (nudges ingest)"
        );
        assert_eq!(
            local.entries[&key()].invite_packet.as_deref(),
            Some(&[0xAA, 0xBB, 0xCC][..]),
            "missing invite promoted from the sibling"
        );

        // Idempotent: a re-merge of the now-equal docs is a no-op, and an
        // already-present invite is never overwritten / re-flagged.
        let out = local.merge_from(local.clone());
        assert!(
            !out.changed,
            "re-merge with the invite already present is a no-op"
        );
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

    #[test]
    fn dm_inbox_entry_invite_only_cidnotify_none_round_trips() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        // ZEB-505: a fleet-replicated invite-only deposit carries the bootstrap
        // invite ALONE — `cidnotify_packet: None`, no storage blob — keyed by
        // `invite_key`. None must survive the canonical-CBOR round-trip
        // (skip_serializing_if omits `cn`; absent → None via default).
        let mut e = entry(hlc(1, "A"), "dev-a", &["dev-1"]);
        e.cidnotify_packet = None;
        e.storage_blob = Vec::new();
        e.invite_packet = Some(vec![0xAA, 0xBB, 0xCC]);
        let mut d = DmInboxDoc::default();
        let k = DmInboxDoc::invite_key(&[1u8; 16]);
        d.entries.insert(k.clone(), e);
        let bytes = canonical_cbor_encode(&d).expect("encode");
        let back: DmInboxDoc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, d);
        assert_eq!(back.entries[&k].cidnotify_packet, None);
    }

    #[test]
    fn revoke_key_de_collides_with_message_and_invite_keys() {
        let space = [0xAB; 16];
        let cid = [0xCD; 32];
        let owner = [0x11; 16];
        let device = [0x22; 16];
        let msg = DmInboxDoc::key(&space, &cid);
        let inv = DmInboxDoc::invite_key(&space);
        let rev = DmInboxDoc::revoke_key(&owner, &device);
        assert!(rev.starts_with("revoke:"));
        assert_ne!(rev, msg);
        assert_ne!(rev, inv);
        // A revoke key's first segment is the literal "revoke", never 32 hex chars,
        // so it can never alias a space-scoped key.
        assert!(!msg.starts_with("revoke:"));
        assert!(!inv.starts_with("revoke:"));
    }

    #[test]
    fn dm_inbox_entry_round_trips_revocation_push() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        let e = DmInboxEntry {
            sender_owner: [1u8; 16],
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: Some(vec![0x05, 0xAA, 0xBB]),
            grant_push: None,
            deposited_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            deposited_by: "d".into(),
            ingested_by: Default::default(),
        };
        let bytes = canonical_cbor_encode(&e).unwrap();
        let back: DmInboxEntry = canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(back.revocation_push, Some(vec![0x05, 0xAA, 0xBB]));
    }

    /// ZEB-674 (C4): a standalone file-share grant entry carries `grant_push`
    /// ALONE (no message / invite / revocation). The field must survive the
    /// canonical-CBOR round-trip (skip_serializing_if omits `gp` when `None`;
    /// absent → `None` via default) and merge into a sibling doc insert-once,
    /// exactly like its optional-sub-payload siblings.
    #[test]
    fn dm_inbox_entry_grant_push_merge_persist() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        let mut e = entry(hlc(1, "A"), "dev-a", &["dev-1"]);
        e.cidnotify_packet = None;
        e.storage_blob = Vec::new();
        e.grant_push = Some(vec![0xDE, 0xAD, 0xBE, 0xEF]);

        // Persist round-trip: `grant_push` survives canonical CBOR.
        let mut d = DmInboxDoc::default();
        let k = DmInboxDoc::grant_key(&[7u8; 16], &[0xDE, 0xAD, 0xBE, 0xEF]);
        d.entries.insert(k.clone(), e.clone());
        let bytes = canonical_cbor_encode(&d).expect("encode");
        let back: DmInboxDoc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, d, "grant entry round-trips through canonical CBOR");
        assert_eq!(
            back.entries[&k].grant_push,
            Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            "grant_push preserved verbatim"
        );

        // Merge into an empty sibling: insert-once carries `grant_push` through
        // (first-writer-wins, like `revocation_push`), and re-merge is a no-op.
        let mut sibling = DmInboxDoc::default();
        let out = sibling.merge_from(d.clone());
        assert!(out.changed, "new grant entry flags changed");
        assert_eq!(
            sibling.entries[&k].grant_push,
            Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            "merged sibling holds the grant payload"
        );
        let out = sibling.merge_from(d.clone());
        assert!(
            !out.changed,
            "re-merge of the identical grant entry is a no-op"
        );
    }

    /// A grant key can't alias a message / invite / revoke key.
    #[test]
    fn grant_key_de_collides_with_other_keys() {
        let owner = [0x33; 16];
        let gp = [0xAB, 0xCD, 0xEF];
        let grant = DmInboxDoc::grant_key(&owner, &gp);
        assert!(grant.starts_with("grant:"));
        assert_ne!(grant, DmInboxDoc::key(&[0x33; 16], &[0xCD; 32]));
        assert_ne!(grant, DmInboxDoc::invite_key(&[0x33; 16]));
        assert_ne!(grant, DmInboxDoc::revoke_key(&owner, &[0x22; 16]));
        // Distinct grant payloads from the same granter get distinct keys.
        assert_ne!(grant, DmInboxDoc::grant_key(&owner, &[0x01, 0x02]));
    }
}
