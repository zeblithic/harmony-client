//! ZEB-495 (ZEB-340 Part 2) Unit 5a: the `community-device-intro` fleet CRDT —
//! deposited-but-not-yet-relayed `DeviceAnnounce` membership events, replicated
//! across the owner's fleet via `FleetSyncEngine` so an already-enrolled
//! sibling device can relay a second device's self-introduction into the
//! community engine (Option A, design spec §"Unit 5"/§"Unit 6").
//!
//! A second device that paired into an existing owner (ZEB-494) holds the
//! community KeyTree but its enrolled device key #2 is not yet in any member's
//! `enrolled_device_keys`, so it cannot self-publish its own introduction
//! (`verify_publisher_sig` would reject — chicken-and-egg). It therefore signs a
//! `DeviceAnnounce` (carrying its own Master `EnrollmentCert`) and DEPOSITS it
//! here; an already-enrolled device of the same owner drains each pending intro
//! and `insert_local_event`s it into the matching community engine, whose next
//! state-root publish carries the announce and passes the publish gate for every
//! peer.
//!
//! Mirrors `dm_inbox_crdt`: string-keyed `BTreeMap` entries, insert-once for new
//! keys, and a grow-only `BTreeSet` (`relayed_by`) unioned on merge. `changed`
//! flags only a new entry or `relayed_by` growth — the immutable
//! `signed_event` / `deposited_at` are first-writer-wins and never re-flag (they
//! drive the relay sweeper's wakeups, so deposit-metadata churn must wake
//! nothing).

use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{Hlc, SpaceId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One deposited `DeviceAnnounce`, awaiting relay into its community engine by
/// an already-enrolled sibling. Keyed by `"{community_hex}:{device_hex}"` (see
/// [`CommunityDeviceIntroDoc::key`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityDeviceIntroEntry {
    /// Canonical-CBOR bytes of the signed `SignedMembershipEvent`
    /// (`MembershipEventKind::DeviceAnnounce`, with the introduced device's
    /// Master cert on `.enrollment`). Immutable first-writer-wins.
    #[serde(rename = "se", with = "serde_bytes")]
    pub signed_event: Vec<u8>,
    /// The community this announce targets — the relay drains only entries
    /// whose community this device is enrolled in.
    #[serde(rename = "ci")]
    pub community_id: SpaceId,
    /// When the second device deposited this intro (first-writer-wins, like
    /// `dm_inbox`'s `deposited_at`). Drives the relay-sweeper's TTL GC.
    #[serde(rename = "da")]
    pub deposited_at: Hlc,
    /// Grow-only set of SP1 device ids (64-hex) that have successfully relayed
    /// this announce into the community engine. GC fires once every
    /// currently-enrolled device has relayed (coverage) — mirrors
    /// `dm_inbox`'s `ingested_by`.
    #[serde(rename = "rb", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub relayed_by: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityDeviceIntroDoc {
    #[serde(rename = "en")]
    pub entries: BTreeMap<String, CommunityDeviceIntroEntry>,
}

// Manual CanonicalPayload registration (the `impl_canonical!` macro in
// owner_state_types.rs is module-private) — mirrors `dm_inbox_crdt` /
// `notes_crdt`.
impl CanonicalPayloadSealed for CommunityDeviceIntroEntry {}
impl CanonicalPayload for CommunityDeviceIntroEntry {}
impl CanonicalPayloadSealed for CommunityDeviceIntroDoc {}
impl CanonicalPayload for CommunityDeviceIntroDoc {}

impl CommunityDeviceIntroDoc {
    /// Deterministic key for one `(community, device)` intro. `device_id` is the
    /// introduced (second) device's SP1 64-hex id; `community_id` is the
    /// 32-hex community SpaceId. Two devices of the same owner introducing into
    /// the same community get distinct keys; the same device re-depositing is
    /// insert-once.
    pub fn key(community_id: &SpaceId, device_id: &str) -> String {
        format!("{}:{}", hex::encode(community_id.0), device_id)
    }

    /// ZEB-668 S3: map key for a RETIRE entry — the intro key plus a ":r"
    /// suffix. Distinct from `key()` so a retire never collides with the
    /// same device's still-pending intro entry (insert-once would otherwise
    /// silently drop whichever deposits second). Same `device_id` = 64-hex
    /// ed25519 verify key — here of the RETIRED device.
    pub fn retire_key(community_id: &SpaceId, device_id: &str) -> String {
        format!("{}:{}:r", hex::encode(community_id.0), device_id)
    }

    /// Insert-once + `relayed_by`-union merge. A new key is inserted whole; an
    /// existing key unions its grow-only `relayed_by` set (concurrent relays by
    /// siblings can never race). The `signed_event` / `community_id` /
    /// `deposited_at` are immutable first-writer-wins (the same key always
    /// carries the same signed announce by construction — the second device
    /// signs it once), so they are NEVER overwritten and never re-flag
    /// `changed`. `changed=true` only on a new entry or `relayed_by` growth so
    /// `on_applied` wakes the relay sweeper exactly when there is new work.
    pub fn merge_from(&mut self, remote: CommunityDeviceIntroDoc) -> MergeOutcome {
        let mut changed = false;
        for (k, r) in remote.entries {
            match self.entries.get_mut(&k) {
                None => {
                    changed = true;
                    self.entries.insert(k, r);
                }
                Some(l) => {
                    let before = l.relayed_by.len();
                    l.relayed_by.extend(r.relayed_by);
                    // signed_event / community_id / deposited_at are immutable
                    // first-writer-wins — same key, same announce — so they are
                    // intentionally left untouched here (no LWW churn to flag).
                    changed |= l.relayed_by.len() != before;
                }
            }
        }
        MergeOutcome { changed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hlc(w: u64, d: &str) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: d.into(),
        }
    }

    fn community(b: u8) -> SpaceId {
        SpaceId([b; 16])
    }

    fn entry(at: Hlc, ci: SpaceId, signed: &[u8], rb: &[&str]) -> CommunityDeviceIntroEntry {
        CommunityDeviceIntroEntry {
            signed_event: signed.to_vec(),
            community_id: ci,
            deposited_at: at,
            relayed_by: rb.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn key() -> String {
        CommunityDeviceIntroDoc::key(&community(1), "device2-64hex")
    }

    #[test]
    fn key_is_community_hex_colon_device() {
        let k = CommunityDeviceIntroDoc::key(&SpaceId([0xAB; 16]), "dev-64hex");
        assert_eq!(k, format!("{}:dev-64hex", "ab".repeat(16)));
    }

    /// ZEB-668 S3: retire keys are the intro key plus a ":r" suffix —
    /// never colliding with the same device's intro entry.
    #[test]
    fn retire_key_is_intro_key_plus_r_suffix() {
        let c = SpaceId([0xAB; 16]);
        let k = CommunityDeviceIntroDoc::retire_key(&c, "dev-64hex");
        assert_eq!(
            k,
            format!("{}:r", CommunityDeviceIntroDoc::key(&c, "dev-64hex"))
        );
        assert_ne!(k, CommunityDeviceIntroDoc::key(&c, "dev-64hex"));
    }

    #[test]
    fn merge_inserts_new_entry_and_is_idempotent() {
        let mut a = CommunityDeviceIntroDoc::default();
        let mut b = CommunityDeviceIntroDoc::default();
        b.entries
            .insert(key(), entry(hlc(1, "B"), community(1), &[1, 2, 3], &[]));

        let out = a.merge_from(b.clone());
        assert!(out.changed, "new entry must flag changed");
        assert_eq!(a.entries.len(), 1);
        assert_eq!(a.entries[&key()], b.entries[&key()]);

        let out = a.merge_from(b.clone());
        assert!(!out.changed, "re-merge of identical doc is a no-op");
        assert_eq!(a, b);
    }

    #[test]
    fn relayed_by_merges_by_union_no_lww_race() {
        let mut base = CommunityDeviceIntroDoc::default();
        base.entries
            .insert(key(), entry(hlc(1, "X"), community(1), &[9], &[]));

        // Concurrent relays on two replicas of the same entry.
        let mut a = base.clone();
        a.entries
            .get_mut(&key())
            .unwrap()
            .relayed_by
            .insert("dev-1".into());
        let mut b = base.clone();
        b.entries
            .get_mut(&key())
            .unwrap()
            .relayed_by
            .insert("dev-2".into());

        let out = a.merge_from(b.clone());
        assert!(out.changed, "relayed_by growth must flag changed");
        let both: BTreeSet<String> = ["dev-1".to_string(), "dev-2".to_string()].into();
        assert_eq!(a.entries[&key()].relayed_by, both);

        // Union merge converges from both sides.
        let mut b2 = b.clone();
        b2.merge_from(a.clone());
        assert_eq!(b2, a, "union merge converges from both sides");
    }

    #[test]
    fn changed_flag_only_on_new_entry_or_relayed_by_growth() {
        let mut a = CommunityDeviceIntroDoc::default();
        a.entries
            .insert(key(), entry(hlc(5, "A"), community(1), &[7], &["dev-1"]));

        // Identical remote: no change.
        let out = a.merge_from(a.clone());
        assert!(!out.changed, "identical merge is not a change");

        // relayed_by growth: flagged.
        let mut grown = CommunityDeviceIntroDoc::default();
        grown.entries.insert(
            key(),
            entry(hlc(5, "A"), community(1), &[7], &["dev-1", "dev-2"]),
        );
        let out = a.merge_from(grown);
        assert!(out.changed, "relayed_by growth must flag changed");

        // relayed_by subset (no growth): not flagged.
        let mut subset = CommunityDeviceIntroDoc::default();
        subset
            .entries
            .insert(key(), entry(hlc(5, "A"), community(1), &[7], &["dev-2"]));
        let out = a.merge_from(subset);
        assert!(
            !out.changed,
            "relayed_by subset adds nothing — not a change"
        );

        // New entry under a different key: flagged.
        let mut fresh = CommunityDeviceIntroDoc::default();
        fresh.entries.insert(
            CommunityDeviceIntroDoc::key(&community(9), "device9-64hex"),
            entry(hlc(7, "C"), community(9), &[3], &[]),
        );
        let out = a.merge_from(fresh);
        assert!(out.changed, "new entry must flag changed");
    }

    #[test]
    fn signed_event_is_immutable_first_writer_wins() {
        // Same key, divergent signed_event bytes (should never happen by
        // construction, but the merge must keep local — no LWW churn).
        let mut a = CommunityDeviceIntroDoc::default();
        a.entries
            .insert(key(), entry(hlc(5, "A"), community(1), &[0xAA], &[]));
        let mut b = CommunityDeviceIntroDoc::default();
        b.entries
            .insert(key(), entry(hlc(5, "A"), community(1), &[0xBB], &["dev-2"]));

        let out = a.merge_from(b);
        // relayed_by grew, so changed — but signed_event stays local.
        assert!(out.changed, "relayed_by growth flags changed");
        assert_eq!(
            a.entries[&key()].signed_event,
            vec![0xAA],
            "signed_event is immutable first-writer-wins (local kept)"
        );
    }

    #[test]
    fn cbor_round_trips_canonically() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        let mut d = CommunityDeviceIntroDoc::default();
        d.entries.insert(
            key(),
            entry(hlc(1, "A"), community(1), &[1, 2, 3], &["dev-1", "dev-2"]),
        );
        // Pin the empty-relayed_by skip path round-trips too.
        d.entries.insert(
            CommunityDeviceIntroDoc::key(&community(9), "device9-64hex"),
            entry(hlc(2, "B"), community(9), &[4, 5], &[]),
        );
        let bytes = canonical_cbor_encode(&d).expect("encode");
        let back: CommunityDeviceIntroDoc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, d);
    }
}
