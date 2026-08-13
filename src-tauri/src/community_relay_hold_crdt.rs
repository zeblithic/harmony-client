//! ZEB-458 Phase A Task 2: RelayHoldDoc CRDT — opaque holding store.
//!
//! Mirrors [`crate::dm_inbox_crdt`] almost exactly: insert-once + grow-only
//! `pulled_by` set, first-writer-wins metadata, one-sweep coverage deferral
//! GC, and TTL eviction. The relay holds `sealed_blob` OPAQUE — it never
//! opens the blob.

use crate::community_relay::{
    RELAY_HOLD_TOMBSTONE_CAP, RELAY_HOLD_TOMBSTONE_RETENTION_MS, RELAY_HOLD_TTL_MS,
};
use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{Hlc, SpaceId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One held-blob entry in the relay's opaque holding store.
///
/// The `sealed_blob` is sealed to the recipient's device key; the relay
/// cannot open it. Key = `"{recipient_owner_hex}:{content_id_hex}"` where
/// `content_id = ContentId(sealed_blob)` — unique per seal (fresh ephemeral
/// key each time), so a single content id already identifies one (recipient,
/// device) copy with no device label needed in the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayHoldEntry {
    #[serde(rename = "ro")]
    pub recipient_owner: [u8; 16],
    #[serde(rename = "so")]
    pub sender_owner: [u8; 16],
    #[serde(rename = "ci")]
    pub community_id: SpaceId,
    /// Sealed blob — OPAQUE to the relay; never opened here.
    #[serde(rename = "sb", with = "serde_bytes")]
    pub sealed_blob: Vec<u8>,
    #[serde(rename = "ha")]
    pub held_at: Hlc,
    /// Relay device id that accepted the deposit (64-hex).
    #[serde(rename = "hb")]
    pub held_by: String,
    /// Grow-only: recipient device ids that pulled + acked this blob.
    #[serde(rename = "pb", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub pulled_by: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RelayHoldDoc {
    #[serde(rename = "en")]
    pub entries: BTreeMap<String, RelayHoldEntry>,
    /// ZEB-851: LOCAL per-replica first-observation clock (ms) keyed by
    /// entry key, driving TTL GC instead of the untrusted peer `held_at`.
    /// Never serialized (canonical wire bytes unchanged) and excluded from
    /// `PartialEq` below, so it never affects convergence or equality.
    #[serde(skip)]
    first_observed_ms: BTreeMap<String, u64>,
    /// ZEB-924: LOCAL expiry memory — keys this replica TTL-expired, mapped
    /// to the local wall-ms of expiry. Suppresses resurrection-by-merge (a
    /// still-holding peer's anti-entropy re-insert) so a never-acked hold's
    /// lifetime here is bounded by first-observation + TTL + one sweep.
    /// Never serialized (canonical wire bytes unchanged), excluded from
    /// `PartialEq`, restart-durable via a local sidecar
    /// (`relay_hold_persist::save_expired`). Bounded by
    /// `RELAY_HOLD_TOMBSTONE_RETENTION_MS` age-out and
    /// `RELAY_HOLD_TOMBSTONE_CAP` oldest-first eviction in [`Self::gc`].
    #[serde(skip)]
    expired_at_ms: BTreeMap<String, u64>,
}

impl PartialEq for RelayHoldDoc {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}
impl Eq for RelayHoldDoc {}

// Manual CanonicalPayload registrations (mirrors dm_inbox_crdt.rs).
impl CanonicalPayloadSealed for RelayHoldEntry {}
impl CanonicalPayload for RelayHoldEntry {}
impl CanonicalPayloadSealed for RelayHoldDoc {}
impl CanonicalPayload for RelayHoldDoc {}

impl RelayHoldDoc {
    /// Key = `"{recipient_owner_hex}:{content_id_hex}"`.
    ///
    /// The `content_id` (32 bytes) is computed by the caller as
    /// `ContentId::for_book(&sealed_blob, ContentFlags{encrypted:true,..})`.
    /// This module keeps `key()` independent of the CAS machinery.
    pub fn key(recipient_owner: &[u8; 16], content_id: &[u8; 32]) -> String {
        format!(
            "{}:{}",
            hex::encode(recipient_owner),
            hex::encode(content_id)
        )
    }

    /// Insert-once + `pulled_by`-union merge. Mirrors
    /// `DmInboxDoc::merge_from` exactly in shape.
    ///
    /// - New entries insert unconditionally → `Changed`.
    /// - Existing entries: `pulled_by` merges by union (grow-only); the
    ///   immutable metadata (`held_at`, `held_by`, `sender_owner`,
    ///   `community_id`, `sealed_blob`) is first-writer-wins by `held_at` and
    ///   replaced WHOLESALE (only a remote with an EARLIER `held_at` replaces
    ///   the local metadata, and when it does it brings ALL of its metadata so
    ///   the result is never a field-wise hybrid). Mirrors the `deposited_at`
    ///   rule in `DmInboxDoc`. `Changed` is set ONLY on new entry or `pulled_by`
    ///   growth — not on metadata-only updates (persistence is unconditional in
    ///   the fleet-sync engine; `changed` only drives the `on_applied` wakeup).
    pub fn merge_from(&mut self, remote: RelayHoldDoc) -> MergeOutcome {
        let mut changed = false;
        for (k, r) in remote.entries {
            match self.entries.get_mut(&k) {
                None => {
                    // ZEB-924: a key this replica already TTL-expired must not
                    // be resurrected by a still-holding peer's merge — suppress
                    // the insert entirely (no `changed`, no flush churn, and
                    // `held_for` never sees it). Deposits are unaffected: a
                    // fresh send mints a fresh content-id key and
                    // `persist_hold` inserts directly, not through here.
                    if self.expired_at_ms.contains_key(&k) {
                        continue;
                    }
                    changed = true;
                    self.entries.insert(k, r);
                }
                Some(l) => {
                    let before = l.pulled_by.len();
                    l.pulled_by.extend(r.pulled_by);
                    // First-writer-wins on held_at: when the remote deposit is
                    // strictly earlier, adopt its immutable metadata WHOLESALE
                    // (held_at, held_by, sender_owner, community_id, sealed_blob)
                    // rather than field-wise, so the merged entry always equals a
                    // state that existed on some replica — never a hybrid.
                    // `sealed_blob` is content-addressed by the key so it is
                    // already byte-identical; `sender_owner`/`community_id` are
                    // NOT part of the content id, so if the same sealed bytes were
                    // ever deposited via a different frame they must travel
                    // together with the winning `held_at` rather than being left
                    // behind from the losing writer.
                    if l.held_at.is_strictly_newer_than(&r.held_at) {
                        l.held_at = r.held_at;
                        l.held_by = r.held_by;
                        l.sender_owner = r.sender_owner;
                        l.community_id = r.community_id;
                        l.sealed_blob = r.sealed_blob;
                    }
                    // `changed` deliberately flags ONLY new entries / `pulled_by`
                    // growth, never this metadata churn — it drives the
                    // `on_applied` wakeup (mirroring `DmInboxDoc`). Persistence is
                    // unconditional in the fleet-sync engine (`persist_now` runs on
                    // every `Applied` outcome regardless of `changed`, see
                    // `fleet_sync.rs`), so the earliest `held_at` is durably kept
                    // across restarts without flagging `changed`.
                    changed |= l.pulled_by.len() != before;
                }
            }
        }
        MergeOutcome { changed }
    }

    /// Count entries for (community_id, sender_owner) — used to enforce
    /// `RELAY_HOLD_PER_SENDER_CAP` on admission.
    pub fn count_for_sender(&self, community_id: &SpaceId, sender_owner: &[u8; 16]) -> usize {
        self.entries
            .values()
            .filter(|e| &e.community_id == community_id && &e.sender_owner == sender_owner)
            .count()
    }

    /// Live entry count (= `entries.len()`).
    pub fn live_count(&self) -> usize {
        self.entries.len()
    }

    /// GC pass: remove entries that are COVERED or TTL-expired.
    ///
    /// **Coverage** — an entry with a non-empty `pulled_by` set is removed
    /// ONLY if it was already non-empty at sweep start (one-sweep deferral,
    /// exactly mirroring `dm_inbox_ingest`'s `covered_at_start` pattern). A
    /// deterministic function of (`pulled_by`, now_ms): every replica
    /// evaluating a resurrected-but-covered entry reaches the same verdict,
    /// so the fleet converges WITHOUT tombstones (see `dm_inbox_ingest.rs`).
    /// An entry whose ack landed DURING this gc call survives one call so
    /// the freshly-covered state can replicate before any replica destroys
    /// it. Deferral is implemented identically to `dm_inbox_ingest`: a
    /// `covered_at_start` set is snapshotted before the retain loop, and
    /// `retain` only removes entries whose key is IN that snapshot.
    ///
    /// **TTL** — `first_observed_ms[key] + RELAY_HOLD_TTL_MS < now_ms` (strict
    /// less-than, keyed off this replica's OWN local receipt time rather than
    /// the peer-supplied `held_at`, so a backdated stamp cannot pre-expire a
    /// live hold). `first_observed_ms` is lazily stamped on the first sweep
    /// that observes each entry. ZEB-862: it is LOCAL-only (never on the wire
    /// or in `PartialEq`) but restart-DURABLE — persisted to and restored from
    /// a local sidecar (`relay_hold_persist::save_first_observed`), so a
    /// restart no longer resets the TTL. A stamp read back GREATER than the
    /// boot `now_ms` (a backward local clock step across restart) is rebased to
    /// `now_ms` in [`Self::restore_first_observed`], so a future stamp cannot
    /// delay expiry. Unlike coverage, this is a per-replica SOFT
    /// deadline, not fleet-wide deterministic. ZEB-924: expired keys leave a
    /// bounded LOCAL tombstone (`expired_at_ms`) that [`Self::merge_from`]
    /// consults, so a resurrection-by-merge from a still-holding peer is
    /// suppressed and a never-acked hold's lifetime here is bounded by
    /// first-observation + TTL + one sweep interval.
    ///
    /// Returns `true` iff the doc changed (some entry was removed).
    pub fn gc(&mut self, now_ms: u64) -> bool {
        // Snapshot which entries are ALREADY covered (pulled_by non-empty)
        // before we mutate anything — entries that become covered during this
        // call survive until the next gc() call (one-sweep deferral).
        let covered_at_start: BTreeSet<String> = self
            .entries
            .iter()
            .filter(|(_, e)| !e.pulled_by.is_empty())
            .map(|(k, _)| k.clone())
            .collect();

        // ZEB-851: expire from this replica's OWN first observation, not the
        // peer-supplied `held_at` (a backdated stamp must not pre-expire a
        // live hold). Lazy-stamp on the first sweep that sees each entry —
        // consistent with the churn-tolerant / resurrection-by-merge model.
        for key in self.entries.keys().cloned().collect::<Vec<_>>() {
            self.first_observed_ms.entry(key).or_insert(now_ms);
        }

        let before = self.entries.len();
        let first_observed = &self.first_observed_ms;
        // ZEB-924: record WHICH keys the TTL rule removes — they become local
        // tombstones so a peer merge cannot resurrect them. Coverage-only
        // removals are fleet-deterministic (every replica re-removes a covered
        // resurrection) and need no tombstone.
        let mut ttl_removed: Vec<String> = Vec::new();
        self.entries.retain(|key, _e| {
            let observed = first_observed.get(key).copied().unwrap_or(now_ms);
            let ttl_expired = observed.saturating_add(RELAY_HOLD_TTL_MS) < now_ms;
            if ttl_expired {
                ttl_removed.push(key.clone());
            }
            !(ttl_expired || covered_at_start.contains(key))
        });
        for key in ttl_removed {
            self.expired_at_ms.insert(key, now_ms);
        }
        self.prune_tombstones(now_ms);
        // Prune the side-map for removed keys (bounded with `entries`).
        let live: BTreeSet<String> = self.entries.keys().cloned().collect();
        self.first_observed_ms.retain(|k, _| live.contains(k));
        self.entries.len() != before
    }

    /// ZEB-862: read the LOCAL first-observation clock for durable sidecar
    /// persistence. Never leaves this replica and never enters the wire.
    pub fn first_observed_ms(&self) -> &BTreeMap<String, u64> {
        &self.first_observed_ms
    }

    /// ZEB-862: restore the LOCAL first-observation clock on boot from the
    /// sidecar file, so TTL GC survives restart instead of re-stamping `now`.
    /// `now_ms` is the boot wall clock.
    ///
    /// - Q-2: orphan stamps for keys not in `entries` (e.g. a crash between the
    ///   doc and sidecar writes) are dropped, so the restored clock is
    ///   self-consistent and `persist` never re-writes dead keys.
    /// - Q-1: a stamp GREATER than `now_ms` (a backward local clock step left a
    ///   future stamp in the sidecar) is rebased down to `now_ms`, so it cannot
    ///   delay TTL expiry beyond `now_ms + TTL`. Self-heals on each boot.
    ///
    /// Callers MUST load `entries` before calling this (the boot path does).
    pub fn restore_first_observed(&mut self, mut map: BTreeMap<String, u64>, now_ms: u64) {
        map.retain(|k, _| self.entries.contains_key(k));
        for v in map.values_mut() {
            *v = (*v).min(now_ms);
        }
        self.first_observed_ms = map;
    }

    /// ZEB-924: read the LOCAL expiry tombstones for durable sidecar
    /// persistence. Never leaves this replica and never enters the wire.
    pub fn expired_at_ms(&self) -> &BTreeMap<String, u64> {
        &self.expired_at_ms
    }

    /// ZEB-924: restore the LOCAL expiry tombstones on boot from the sidecar.
    ///
    /// - A stamp GREATER than `now_ms` (a backward local clock step across
    ///   restart) is rebased to `now_ms` (mirrors
    ///   [`Self::restore_first_observed`] Q-1).
    /// - Retention + cap are enforced HERE too (PR #667 R1, CodeAnt), so an
    ///   aged-out or over-cap sidecar never suppresses merges differently at
    ///   boot than in steady state — pruning runs BEFORE tombstones are
    ///   applied to `entries`, so an expired tombstone neither suppresses nor
    ///   deletes anything.
    /// - Any surviving tombstone key still present in `entries` (a stale doc
    ///   file from a crash between atomic writes resurrected an entry this
    ///   replica already expired) is REMOVED from `entries` — expiry is
    ///   monotone; the tombstone wins.
    ///
    /// Callers MUST load `entries` first and MUST call this BEFORE
    /// [`Self::restore_first_observed`], whose orphan-pruning then drops the
    /// removed entries' stamps (the boot path does).
    pub fn restore_expired(&mut self, mut map: BTreeMap<String, u64>, now_ms: u64) {
        for v in map.values_mut() {
            *v = (*v).min(now_ms);
        }
        self.expired_at_ms = map;
        self.prune_tombstones(now_ms);
        let tombstones = &self.expired_at_ms;
        self.entries.retain(|k, _| !tombstones.contains_key(k));
    }

    /// ZEB-924 (PR #667 R2, Greptile): forget the expiry tombstone for `key`.
    ///
    /// Called by the DEPOSIT path (`persist_hold`) when it accepts a key —
    /// deposits are deliberately ungated, so a byte-identical redelivery can
    /// re-insert a key this replica once TTL-expired. Acceptance is a fresh
    /// local decision to hold, so expiry memory must not outlive it: leaving
    /// the stale tombstone beside the live entry would let
    /// [`Self::restore_expired`] delete the accepted (and already-acked) hold
    /// at the next boot.
    pub fn clear_tombstone(&mut self, key: &str) {
        self.expired_at_ms.remove(key);
    }

    /// ZEB-924: age-out + cap so expiry memory cannot itself become unbounded
    /// state. Shared by [`Self::gc`] and [`Self::restore_expired`] so the
    /// retention/cap invariants hold unconditionally, not only after a sweep.
    fn prune_tombstones(&mut self, now_ms: u64) {
        self.expired_at_ms
            .retain(|_, t| now_ms.saturating_sub(*t) < RELAY_HOLD_TOMBSTONE_RETENTION_MS);
        while self.expired_at_ms.len() > RELAY_HOLD_TOMBSTONE_CAP {
            let oldest = self
                .expired_at_ms
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.expired_at_ms.remove(&k);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_relay::RELAY_HOLD_PER_SENDER_CAP;
    use crate::owner_state_types::Hlc;

    fn hlc(w: u64, d: &str) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: d.into(),
        }
    }

    fn space(b: u8) -> SpaceId {
        SpaceId([b; 16])
    }

    fn entry(
        recipient_owner: [u8; 16],
        sender_owner: [u8; 16],
        community_id: SpaceId,
        at: Hlc,
        by: &str,
        pulled: &[&str],
    ) -> RelayHoldEntry {
        RelayHoldEntry {
            recipient_owner,
            sender_owner,
            community_id,
            sealed_blob: vec![0xDE, 0xAD, 0xBE, 0xEF],
            held_at: at,
            held_by: by.into(),
            pulled_by: pulled.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn key_rr(recipient: u8, content: u8) -> String {
        RelayHoldDoc::key(&[recipient; 16], &[content; 32])
    }

    // ----------------------------------------------------------------
    // ZEB-862: restart-durable first-observation clock
    // ----------------------------------------------------------------

    #[test]
    fn restored_old_first_observed_expires_across_restart() {
        // A durable OLD stamp (as if reloaded from the sidecar) ages the entry
        // out on the next sweep — the whole point of the fix.
        let mut doc = RelayHoldDoc::default();
        let k = key_rr(1, 1);
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [2; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let now = crate::community_relay::RELAY_HOLD_TTL_MS + 10_000;
        doc.restore_first_observed([(k.clone(), 1u64)].into_iter().collect(), now);
        assert!(doc.gc(now), "old restored stamp → entry ages out");
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn empty_first_observed_survives_first_sweep_after_restart() {
        // Negative: without the sidecar the empty clock re-stamps at `now`, so
        // the entry survives — this is today's bug, pinned to show the contrast.
        let mut doc = RelayHoldDoc::default();
        let k = key_rr(1, 1);
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [2; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let now = crate::community_relay::RELAY_HOLD_TTL_MS + 10_000;
        assert!(!doc.gc(now), "empty clock re-stamps at now → survives");
        assert_eq!(doc.entries.len(), 1);
    }

    #[test]
    fn restore_and_read_first_observed_round_trips() {
        let mut doc = RelayHoldDoc::default();
        let k = key_rr(1, 1);
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [2; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let m: BTreeMap<String, u64> = [(k.clone(), 12_345u64)].into_iter().collect();
        doc.restore_first_observed(m.clone(), u64::MAX);
        assert_eq!(doc.first_observed_ms(), &m);
    }

    #[test]
    fn gc_grows_first_observed_once_per_entry() {
        // The stamp-only detection in both sweepers keys off the side-map length
        // delta: gc grows it on the first sweep that sees a new entry, and not
        // thereafter. (ZEB-862 finding A — persist a stamp-only sweep.)
        let mut doc = RelayHoldDoc::default();
        let k = key_rr(1, 1);
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [2; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        assert_eq!(doc.first_observed_ms().len(), 0);
        assert!(!doc.gc(1_000), "fresh stamp at now → nothing removed");
        assert_eq!(doc.first_observed_ms().len(), 1, "gc stamped the new entry");
        assert!(!doc.gc(2_000));
        assert_eq!(
            doc.first_observed_ms().len(),
            1,
            "already-stamped entry does not grow the side-map"
        );
    }

    #[test]
    fn restore_clamps_future_stamp_to_now() {
        // ZEB-862 Q-1: a stamp restored GREATER than the boot `now` (a backward
        // clock step) is rebased to `now`, so it cannot delay expiry past now+TTL.
        let mut doc = RelayHoldDoc::default();
        let k = key_rr(1, 1);
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [2; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let now = 1_000_000u64;
        doc.restore_first_observed([(k.clone(), now + 5_000_000)].into_iter().collect(), now);
        assert_eq!(
            doc.first_observed_ms()[&k],
            now,
            "future stamp rebased to now"
        );
        assert!(
            doc.gc(now + RELAY_HOLD_TTL_MS + 1),
            "expires at now+TTL, not later"
        );
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn restore_prunes_orphan_stamps() {
        // ZEB-862 Q-2: a sidecar stamp whose key is absent from `entries` (a
        // crash between the doc and sidecar writes) is dropped on restore.
        let mut doc = RelayHoldDoc::default();
        let k = key_rr(1, 1);
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [2; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let mut m: BTreeMap<String, u64> = BTreeMap::new();
        m.insert(k.clone(), 5);
        m.insert(key_rr(9, 9), 5); // orphan: not in entries
        doc.restore_first_observed(m, u64::MAX);
        assert!(doc.first_observed_ms().contains_key(&k));
        assert!(
            !doc.first_observed_ms().contains_key(&key_rr(9, 9)),
            "orphan stamp pruned"
        );
    }

    // ----------------------------------------------------------------
    // 1. key() determinism + format
    // ----------------------------------------------------------------

    #[test]
    fn key_format_is_recipient_hex_colon_content_hex() {
        let recipient = [0xAAu8; 16];
        let content = [0xBBu8; 32];
        let k = RelayHoldDoc::key(&recipient, &content);

        let expected_recipient = "aa".repeat(16);
        let expected_content = "bb".repeat(32);
        assert_eq!(k, format!("{expected_recipient}:{expected_content}"));
    }

    #[test]
    fn key_is_deterministic() {
        let recipient = [0x01u8; 16];
        let content = [0x02u8; 32];
        assert_eq!(
            RelayHoldDoc::key(&recipient, &content),
            RelayHoldDoc::key(&recipient, &content),
        );
    }

    #[test]
    fn key_differs_by_recipient_or_content() {
        let r1 = [0x01u8; 16];
        let r2 = [0x02u8; 16];
        let c1 = [0x01u8; 32];
        let c2 = [0x02u8; 32];

        assert_ne!(RelayHoldDoc::key(&r1, &c1), RelayHoldDoc::key(&r2, &c1));
        assert_ne!(RelayHoldDoc::key(&r1, &c1), RelayHoldDoc::key(&r1, &c2));
    }

    // ----------------------------------------------------------------
    // 2. merge_from
    // ----------------------------------------------------------------

    #[test]
    fn merge_inserts_new_entry_and_is_idempotent() {
        let mut local = RelayHoldDoc::default();
        let mut remote = RelayHoldDoc::default();
        remote.entries.insert(
            key_rr(1, 2),
            entry([1; 16], [2; 16], space(0xCC), hlc(100, "R"), "relay-r", &[]),
        );

        let out = local.merge_from(remote.clone());
        assert!(out.changed, "new remote entry must flag Changed");
        assert_eq!(local.entries.len(), 1);

        let out = local.merge_from(remote.clone());
        assert!(!out.changed, "re-merge of identical doc is a no-op");
    }

    #[test]
    fn merge_pulled_by_grows_by_union_and_flags_changed() {
        let mut base = RelayHoldDoc::default();
        base.entries.insert(
            key_rr(1, 2),
            entry([1; 16], [2; 16], space(0xCC), hlc(100, "R"), "relay-r", &[]),
        );

        let mut a = base.clone();
        a.entries
            .get_mut(&key_rr(1, 2))
            .unwrap()
            .pulled_by
            .insert("dev-1".into());
        let mut b = base.clone();
        b.entries
            .get_mut(&key_rr(1, 2))
            .unwrap()
            .pulled_by
            .insert("dev-2".into());

        let out = a.merge_from(b);
        assert!(out.changed, "pulled_by growth must flag Changed");
        let merged: BTreeSet<String> = ["dev-1".into(), "dev-2".into()].into();
        assert_eq!(a.entries[&key_rr(1, 2)].pulled_by, merged);
    }

    #[test]
    fn merge_identical_no_pulled_by_growth_is_unchanged() {
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            key_rr(1, 2),
            entry(
                [1; 16],
                [2; 16],
                space(0xCC),
                hlc(100, "R"),
                "relay-r",
                &["dev-1"],
            ),
        );
        let out = doc.merge_from(doc.clone());
        assert!(!out.changed, "identical merge must not flag Changed");
    }

    #[test]
    fn merge_metadata_only_swap_not_flagged_as_changed() {
        // A remote with a different (earlier) held_at swaps the local metadata
        // but must NOT flag Changed (only new entries and pulled_by growth flag).
        let mut local = RelayHoldDoc::default();
        local.entries.insert(
            key_rr(1, 2),
            entry(
                [1; 16],
                [2; 16],
                space(0xCC),
                hlc(500, "A"),
                "relay-a",
                &["dev-1"],
            ),
        );
        let mut remote = RelayHoldDoc::default();
        remote.entries.insert(
            key_rr(1, 2),
            entry(
                [1; 16],
                [2; 16],
                space(0xCC),
                hlc(100, "B"),
                "relay-b",
                &["dev-1"],
            ),
        );
        let out = local.merge_from(remote);
        assert!(!out.changed, "metadata-only swap must not flag Changed");
        // But the metadata WAS updated (first-writer-wins → earlier wins).
        assert_eq!(local.entries[&key_rr(1, 2)].held_by, "relay-b");
        assert_eq!(local.entries[&key_rr(1, 2)].held_at, hlc(100, "B"));
    }

    #[test]
    fn merge_first_writer_wins_replaces_metadata_wholesale_no_hybrid() {
        // Same key (content-addressed), but the two writers disagree on the
        // non-key metadata sender_owner / community_id (reachable only via a
        // byte-replay of the same sealed blob through a different frame). When
        // the earlier writer wins, ALL of its metadata must travel together —
        // the merged entry must equal a state that existed on some replica, not
        // a hybrid of the earlier held_at with the later sender/community.
        let mut local = RelayHoldDoc::default();
        local.entries.insert(
            key_rr(1, 2),
            entry([1; 16], [2; 16], space(0xCC), hlc(500, "A"), "relay-a", &[]),
        );
        let mut remote = RelayHoldDoc::default();
        remote.entries.insert(
            key_rr(1, 2),
            // EARLIER held_at, DIFFERENT sender_owner + community_id.
            entry(
                [1; 16],
                [0x33; 16],
                space(0xEE),
                hlc(100, "B"),
                "relay-b",
                &[],
            ),
        );
        let out = local.merge_from(remote);
        assert!(!out.changed, "metadata-only churn must not flag Changed");
        let merged = &local.entries[&key_rr(1, 2)];
        // The earlier writer (remote) wins — its metadata is adopted WHOLESALE.
        assert_eq!(merged.held_at, hlc(100, "B"));
        assert_eq!(merged.held_by, "relay-b");
        assert_eq!(
            merged.sender_owner, [0x33; 16],
            "sender_owner must follow the winning held_at, not stay hybrid"
        );
        assert_eq!(
            merged.community_id,
            space(0xEE),
            "community_id must follow the winning held_at, not stay hybrid"
        );
    }

    #[test]
    fn merge_first_writer_wins_on_metadata() {
        // A remote with LATER held_at must NOT overwrite local immutable fields.
        let mut local = RelayHoldDoc::default();
        local.entries.insert(
            key_rr(1, 2),
            entry([1; 16], [2; 16], space(0xCC), hlc(100, "A"), "relay-a", &[]),
        );
        let mut remote = RelayHoldDoc::default();
        remote.entries.insert(
            key_rr(1, 2),
            entry([1; 16], [2; 16], space(0xCC), hlc(999, "B"), "relay-b", &[]),
        );
        let out = local.merge_from(remote);
        assert!(!out.changed);
        // Local metadata is preserved (local is the earlier writer).
        assert_eq!(local.entries[&key_rr(1, 2)].held_by, "relay-a");
        assert_eq!(local.entries[&key_rr(1, 2)].held_at, hlc(100, "A"));
    }

    #[test]
    fn merge_converges_from_both_sides() {
        // Two concurrent first-deposits with different pulled_by sets converge.
        let mut a = RelayHoldDoc::default();
        a.entries.insert(
            key_rr(1, 2),
            entry(
                [1; 16],
                [2; 16],
                space(0xCC),
                hlc(50, "A"),
                "relay-a",
                &["dev-x"],
            ),
        );
        let mut b = RelayHoldDoc::default();
        b.entries.insert(
            key_rr(1, 2),
            entry(
                [1; 16],
                [2; 16],
                space(0xCC),
                hlc(80, "B"),
                "relay-b",
                &["dev-y"],
            ),
        );

        let mut a2 = a.clone();
        a2.merge_from(b.clone());
        let mut b2 = b.clone();
        b2.merge_from(a.clone());

        assert_eq!(a2, b2, "union merge converges from both sides");
        // Earlier held_at wins.
        assert_eq!(a2.entries[&key_rr(1, 2)].held_by, "relay-a");
        // pulled_by is the union.
        let both: BTreeSet<String> = ["dev-x".into(), "dev-y".into()].into();
        assert_eq!(a2.entries[&key_rr(1, 2)].pulled_by, both);
    }

    // ----------------------------------------------------------------
    // 3. count_for_sender / live_count
    // ----------------------------------------------------------------

    #[test]
    fn count_for_sender_filters_by_community_and_sender() {
        let mut doc = RelayHoldDoc::default();

        // sender [0x01;16] in community space(0x01) — 2 entries.
        doc.entries.insert(
            key_rr(0xA0, 0x01),
            entry([0xA0; 16], [0x01; 16], space(0x01), hlc(1, "R"), "r", &[]),
        );
        doc.entries.insert(
            key_rr(0xA1, 0x01),
            entry([0xA1; 16], [0x01; 16], space(0x01), hlc(2, "R"), "r", &[]),
        );
        // same sender, DIFFERENT community — should not count.
        doc.entries.insert(
            key_rr(0xA2, 0x01),
            entry([0xA2; 16], [0x01; 16], space(0x02), hlc(3, "R"), "r", &[]),
        );
        // different sender in same community — should not count.
        doc.entries.insert(
            key_rr(0xA3, 0x02),
            entry([0xA3; 16], [0x02; 16], space(0x01), hlc(4, "R"), "r", &[]),
        );

        assert_eq!(doc.count_for_sender(&space(0x01), &[0x01; 16]), 2);
        assert_eq!(doc.count_for_sender(&space(0x02), &[0x01; 16]), 1);
        assert_eq!(doc.count_for_sender(&space(0x01), &[0x02; 16]), 1);
    }

    #[test]
    fn live_count_equals_entries_len() {
        let mut doc = RelayHoldDoc::default();
        assert_eq!(doc.live_count(), 0);
        doc.entries.insert(
            key_rr(1, 1),
            entry([1; 16], [2; 16], space(0xCC), hlc(1, "R"), "r", &[]),
        );
        assert_eq!(doc.live_count(), 1);
        doc.entries.insert(
            key_rr(2, 2),
            entry([2; 16], [3; 16], space(0xCC), hlc(2, "R"), "r", &[]),
        );
        assert_eq!(doc.live_count(), 2);
    }

    // ----------------------------------------------------------------
    // 4. gc
    // ----------------------------------------------------------------

    #[test]
    fn gc_uses_local_receipt_not_backdated_held_at() {
        let mut doc = RelayHoldDoc::default();
        let key = RelayHoldDoc::key(&[1u8; 16], &[2u8; 32]);
        // A BACKDATED held_at (far in the past): under the old
        // `held_at.wall_ms + TTL < now` rule this is already expired.
        doc.entries.insert(
            key.clone(),
            RelayHoldEntry {
                recipient_owner: [1u8; 16],
                sender_owner: [3u8; 16],
                community_id: space(7),
                sealed_blob: vec![9],
                held_at: hlc(1, "relay"),
                held_by: "relay".into(),
                pulled_by: BTreeSet::new(),
            },
        );
        let now = RELAY_HOLD_TTL_MS + 1_000_000;
        // First sweep OBSERVES the entry locally at `now` → must NOT expire it.
        assert!(!doc.gc(now), "no entry removed on first observation");
        assert!(
            doc.entries.contains_key(&key),
            "a backdated held_at must not pre-expire a freshly-observed hold"
        );
        // A later sweep past first_observed + TTL DOES expire it.
        assert!(doc.gc(now + RELAY_HOLD_TTL_MS + 1));
        assert!(
            !doc.entries.contains_key(&key),
            "expired from local receipt"
        );
    }

    #[test]
    fn gc_removes_ttl_expired_entry_after_local_receipt_ttl() {
        // ZEB-851: TTL now keys off local first-observation, not the
        // peer-supplied (and possibly backdated) `held_at` — so eviction
        // takes a first observing sweep plus a later sweep past the TTL,
        // rather than being immediate on the first sweep.
        let now_ms: u64 = RELAY_HOLD_TTL_MS + 1_000_000;
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            key_rr(1, 1),
            entry([1; 16], [2; 16], space(0xCC), hlc(500, "R"), "r", &[]),
        );
        let changed = doc.gc(now_ms);
        assert!(!changed, "first observation must not immediately expire");
        assert_eq!(doc.entries.len(), 1);

        let changed = doc.gc(now_ms + RELAY_HOLD_TTL_MS + 1);
        assert!(changed, "TTL eviction must return true past the deadline");
        assert!(doc.entries.is_empty(), "expired entry must be removed");
    }

    #[test]
    fn gc_ttl_boundary_is_strict() {
        // Exact boundary: first_observed_ms + RELAY_HOLD_TTL_MS == now_ms → NOT expired.
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            key_rr(1, 1),
            entry([1; 16], [2; 16], space(0xCC), hlc(1_000_000, "R"), "r", &[]),
        );
        let observed_at: u64 = 1_000_000;
        let pre_changed = doc.gc(observed_at); // stamps first_observed_ms[key] = observed_at
        assert!(
            !pre_changed,
            "pre-stamp sweep must not remove a fresh entry"
        );

        let changed = doc.gc(observed_at + RELAY_HOLD_TTL_MS);
        assert!(!changed, "boundary is NOT expired (strict <)");
        assert_eq!(doc.entries.len(), 1);

        let changed = doc.gc(observed_at + RELAY_HOLD_TTL_MS + 1);
        assert!(changed, "past-boundary IS expired");
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn gc_removes_covered_entry_that_was_covered_at_sweep_start() {
        // An entry with pulled_by non-empty AT gc() call time is "covered at
        // start" and must be removed.
        let now_ms: u64 = 1_000_000; // well within TTL
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            key_rr(1, 1),
            entry(
                [1; 16],
                [2; 16],
                space(0xCC),
                hlc(now_ms - 1_000, "R"),
                "r",
                &["dev-1"],
            ),
        );
        let changed = doc.gc(now_ms);
        assert!(changed, "covered-at-start entry must be removed");
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn gc_defers_freshly_covered_entry_for_one_sweep() {
        // Simulate an entry that becomes covered DURING a gc call: since
        // gc() snapshots covered_at_start BEFORE iterating, an entry that
        // had pulled_by = {} at snapshot time must survive this gc call.
        // (In our pure-CRDT GC, "becomes covered during gc" is modelled by
        // inserting an uncovered entry and calling gc — it is NOT in
        // covered_at_start, so it survives. On the NEXT call with the same
        // state it IS in covered_at_start and is removed.)

        let now_ms: u64 = 1_000_000;

        // Phase 1: entry starts uncovered → gc call → NOT removed (not in covered_at_start).
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            key_rr(1, 1),
            entry(
                [1; 16],
                [2; 16],
                space(0xCC),
                hlc(now_ms - 1_000, "R"),
                "r",
                &[],
            ),
        );
        let changed = doc.gc(now_ms);
        assert!(!changed, "uncovered entry must survive first gc call");
        assert_eq!(doc.entries.len(), 1);

        // Phase 2: ack arrives (pulled_by becomes non-empty — now covered at start).
        doc.entries
            .get_mut(&key_rr(1, 1))
            .unwrap()
            .pulled_by
            .insert("dev-1".into());

        // Second gc call: NOW covered at sweep start → removed.
        let changed = doc.gc(now_ms);
        assert!(
            changed,
            "entry covered at sweep start must be removed on next gc"
        );
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn gc_keeps_uncovered_unexpired_entries() {
        let now_ms: u64 = 1_000_000;
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            key_rr(1, 1),
            entry(
                [1; 16],
                [2; 16],
                space(0xCC),
                hlc(now_ms - 1_000, "R"),
                "r",
                &[],
            ),
        );
        let changed = doc.gc(now_ms);
        assert!(!changed, "uncovered + unexpired entry must not be removed");
        assert_eq!(doc.entries.len(), 1);
    }

    #[test]
    fn gc_returns_false_when_nothing_removed() {
        let now_ms: u64 = 1_000_000;
        let mut doc = RelayHoldDoc::default();
        let changed = doc.gc(now_ms);
        assert!(!changed, "empty doc returns false");
    }

    #[test]
    fn gc_removes_only_eligible_entries() {
        let now_ms: u64 = RELAY_HOLD_TTL_MS + 1_000_000;

        let mut doc = RelayHoldDoc::default();
        // TTL-target: first-observed on an EARLIER sweep (ZEB-851 keys TTL
        // off local receipt, so it must be stamped before `now_ms` to have
        // any chance of expiring by the final sweep below).
        doc.entries.insert(
            key_rr(1, 1),
            entry([1; 16], [2; 16], space(0xCC), hlc(500, "R"), "r", &[]),
        );
        let pre_changed = doc.gc(0); // stamps first_observed_ms[key_rr(1,1)] = 0
        assert!(
            !pre_changed,
            "pre-stamp sweep must not remove a fresh entry"
        );
        // Covered at start → removed.
        doc.entries.insert(
            key_rr(2, 2),
            entry(
                [2; 16],
                [3; 16],
                space(0xCC),
                hlc(now_ms - 1_000, "R"),
                "r",
                &["dev-x"],
            ),
        );
        // Fresh + uncovered → retained.
        doc.entries.insert(
            key_rr(3, 3),
            entry(
                [3; 16],
                [4; 16],
                space(0xCC),
                hlc(now_ms - 1_000, "R"),
                "r",
                &[],
            ),
        );
        // Freshly observed on this sweep, uncovered → retained. (`held_at` no
        // longer drives TTL post-ZEB-851, so this is a second fresh-uncovered
        // case rather than a `held_at` boundary case.)
        doc.entries.insert(
            key_rr(4, 4),
            entry(
                [4; 16],
                [5; 16],
                space(0xCC),
                hlc(now_ms - RELAY_HOLD_TTL_MS, "R"),
                "r",
                &[],
            ),
        );

        let changed = doc.gc(now_ms);
        assert!(changed);
        assert!(
            !doc.entries.contains_key(&key_rr(1, 1)),
            "TTL-from-earlier-observation → removed"
        );
        assert!(
            !doc.entries.contains_key(&key_rr(2, 2)),
            "covered-at-start → removed"
        );
        assert!(
            doc.entries.contains_key(&key_rr(3, 3)),
            "fresh uncovered → retained"
        );
        assert!(
            doc.entries.contains_key(&key_rr(4, 4)),
            "fresh uncovered → retained"
        );
    }

    // ----------------------------------------------------------------
    // 5. RELAY_HOLD_PER_SENDER_CAP is visible (count_for_sender usable
    //    with cap constant from community_relay)
    // ----------------------------------------------------------------

    #[test]
    fn per_sender_cap_constant_is_accessible() {
        // Confirm the cap constant imported from community_relay is the
        // expected value (64) and that count_for_sender feeds admission logic.
        assert_eq!(RELAY_HOLD_PER_SENDER_CAP, 64);
        let doc = RelayHoldDoc::default();
        assert_eq!(doc.count_for_sender(&space(0x01), &[0x01; 16]), 0);
    }

    // ----------------------------------------------------------------
    // 6. Dataset-cap CBOR overhead accounting (ZEB-458 P4B review fix)
    //
    // RELAY_HOLD_DATASET_MAX_BYTES must account for the per-entry CBOR
    // map key + metadata, NOT just the blob bytes. Build a SMALL
    // representative doc with realistic max-length keys + metadata + a
    // tiny blob, encode it with the SAME canonical-CBOR encoder the
    // fleet-sync dataset root uses, and assert the measured per-entry
    // overhead (encoded size minus the blob bytes, divided by the entry
    // count) stays within RELAY_HOLD_ENTRY_OVERHEAD_BYTES. We do NOT
    // build a full-capacity 256 MiB doc — we extrapolate from this
    // small sample, since the per-entry overhead is constant in the
    // number of entries.
    // ----------------------------------------------------------------

    #[test]
    fn dataset_cap_per_entry_overhead_within_allowance() {
        use crate::community_relay::RELAY_HOLD_ENTRY_OVERHEAD_BYTES;
        use crate::owner_state_crypto::canonical_cbor_encode;

        const ENTRY_COUNT: usize = 8;
        // Small per-entry blob so the measured overhead is dominated by
        // the key + metadata (the thing the const is meant to cover).
        const BLOB_LEN: usize = 16;

        // A realistic max-length 64-hex relay-device `held_by` string.
        let held_by_64hex = "ab".repeat(32);
        assert_eq!(held_by_64hex.len(), 64);
        // A realistic HLC with a non-trivial device_id string.
        let held_at = hlc(1_700_000_000_000, "relay-device-id");

        let mut doc = RelayHoldDoc::default();
        let mut total_blob_bytes = 0usize;
        for i in 0..ENTRY_COUNT {
            // Distinct recipient + content per entry → distinct realistic
            // max-length keys (`{recipient_hex}:{content_id_hex}` ≈ 81 chars).
            let recipient = [i as u8; 16];
            let content = [(0x80 + i) as u8; 32];
            let key = RelayHoldDoc::key(&recipient, &content);
            assert_eq!(
                key.len(),
                16 * 2 + 1 + 32 * 2,
                "key is recipient:content hex"
            );
            let e = RelayHoldEntry {
                recipient_owner: recipient,
                sender_owner: [(0x40 + i) as u8; 16],
                community_id: space(0x55),
                sealed_blob: vec![0xAB; BLOB_LEN],
                held_at: held_at.clone(),
                held_by: held_by_64hex.clone(),
                // A couple of pulled-by device strings (grow-only set).
                pulled_by: ["pull-dev-aaaa", "pull-dev-bbbb"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            };
            total_blob_bytes += e.sealed_blob.len();
            doc.entries.insert(key, e);
        }
        assert_eq!(doc.entries.len(), ENTRY_COUNT);

        let encoded = canonical_cbor_encode(&doc).expect("canonical-CBOR encode RelayHoldDoc");
        // Per-entry overhead = (whole-doc encoded size − Σ blob bytes) / entries.
        // This generously attributes the doc-level CBOR framing to the
        // per-entry budget too, so the assertion is conservative.
        let overhead_total = encoded
            .len()
            .checked_sub(total_blob_bytes)
            .expect("encoded doc must be at least its blob bytes");
        let per_entry_overhead = overhead_total / ENTRY_COUNT;

        assert!(
            per_entry_overhead <= RELAY_HOLD_ENTRY_OVERHEAD_BYTES,
            "measured per-entry CBOR overhead {per_entry_overhead} B exceeds the \
             RELAY_HOLD_ENTRY_OVERHEAD_BYTES allowance ({RELAY_HOLD_ENTRY_OVERHEAD_BYTES} B); \
             the dataset cap would under-count a full doc and the zenoh adapter \
             could DROP the sample"
        );
    }

    // ----------------------------------------------------------------
    // ZEB-924: local expiry tombstones vs resurrection-by-merge
    // ----------------------------------------------------------------

    #[test]
    fn gc_ttl_expiry_tombstones_the_key_but_coverage_removal_does_not() {
        let mut doc = RelayHoldDoc::default();
        let ttl_key = key_rr(1, 1);
        let cov_key = key_rr(2, 2);
        doc.entries.insert(
            ttl_key.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        doc.entries.insert(
            cov_key.clone(),
            entry([2; 16], [9; 16], space(3), hlc(1, "a"), "relay", &["dev-1"]),
        );
        assert!(doc.gc(1_000), "covered-at-start entry removed immediately");
        assert!(
            doc.expired_at_ms().is_empty(),
            "coverage removal must NOT tombstone (fleet-deterministic already)"
        );
        // TTL-expire the survivor: stamped at 1_000, sweep past TTL.
        let later = 1_000 + RELAY_HOLD_TTL_MS + 1;
        assert!(doc.gc(later), "TTL removal");
        assert_eq!(
            doc.expired_at_ms().get(&ttl_key),
            Some(&later),
            "TTL removal records a tombstone at the sweep time"
        );
    }

    #[test]
    fn merge_suppresses_resurrection_of_tombstoned_key() {
        let mut doc = RelayHoldDoc::default();
        let k = key_rr(1, 1);
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        doc.gc(1_000); // stamp
        doc.gc(1_000 + RELAY_HOLD_TTL_MS + 1); // expire + tombstone
        assert!(doc.entries.is_empty());

        // A still-holding peer's doc re-offers the entry.
        let mut remote = RelayHoldDoc::default();
        remote.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let out = doc.merge_from(remote);
        assert!(
            !out.changed,
            "suppressed resurrection is silent (no flush churn)"
        );
        assert!(
            doc.entries.is_empty(),
            "tombstoned key never re-enters entries"
        );
    }

    #[test]
    fn resurrection_lifetime_bound_across_merge_traffic() {
        // Acceptance pin: interleave merges from a still-holding peer with
        // sweeps — after the first TTL expiry the key never re-enters.
        let k = key_rr(1, 1);
        let mut peer = RelayHoldDoc::default();
        peer.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let mut doc = RelayHoldDoc::default();
        doc.merge_from(peer.clone());
        doc.gc(0); // first observation stamp at 0
        let expiry_sweep = RELAY_HOLD_TTL_MS + 1;
        assert!(doc.gc(expiry_sweep), "expires at TTL");
        for i in 1..=5u64 {
            let now = expiry_sweep + i * 600_000; // merge every sweep interval
            let out = doc.merge_from(peer.clone());
            assert!(!out.changed, "merge round {i} suppressed");
            assert!(!doc.gc(now), "nothing to remove in round {i}");
            assert!(doc.entries.is_empty(), "never re-enters (round {i})");
        }
    }

    #[test]
    fn covered_resurrection_still_converges_without_tombstone() {
        // Existing semantics pinned: a resurrected COVERED entry carries its
        // pulled_by and is deterministically re-removed on the next sweep.
        let k = key_rr(1, 1);
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &["dev-1"]),
        );
        assert!(doc.gc(1_000), "covered-at-start → removed, no tombstone");
        let mut peer = RelayHoldDoc::default();
        peer.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &["dev-1"]),
        );
        let out = doc.merge_from(peer);
        assert!(out.changed, "covered entry MAY resurrect (no tombstone)");
        assert!(
            doc.gc(2_000),
            "and is deterministically re-removed next sweep"
        );
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn tombstone_ages_out_after_retention_and_reopens() {
        use crate::community_relay::RELAY_HOLD_TOMBSTONE_RETENTION_MS;
        let k = key_rr(1, 1);
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        doc.gc(0);
        let expiry = RELAY_HOLD_TTL_MS + 1;
        assert!(doc.gc(expiry));
        assert!(doc.expired_at_ms().contains_key(&k));
        // Past retention the tombstone is dropped…
        assert!(!doc.gc(expiry + RELAY_HOLD_TOMBSTONE_RETENTION_MS + 1));
        assert!(doc.expired_at_ms().is_empty(), "tombstone aged out");
        // …and a pathological late holder re-arms ONE more TTL window
        // (documented bounded harm, spec §6).
        let mut peer = RelayHoldDoc::default();
        peer.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        assert!(
            doc.merge_from(peer).changed,
            "post-retention resurrection re-inserts"
        );
    }

    #[test]
    fn tombstone_cap_evicts_oldest_first() {
        use crate::community_relay::RELAY_HOLD_TOMBSTONE_CAP;
        let mut doc = RelayHoldDoc::default();
        // Overfill via restore (unit seam) — the shared prune runs at restore
        // AND after every gc, so cap enforcement is unconditional.
        let mut m: BTreeMap<String, u64> = BTreeMap::new();
        for i in 0..(RELAY_HOLD_TOMBSTONE_CAP + 2) {
            // Distinct keys; stamps strictly increasing so "oldest" is i=0,1.
            let content: [u8; 32] = {
                let mut c = [0u8; 32];
                c[0] = (i / 256) as u8;
                c[1] = (i % 256) as u8;
                c
            };
            m.insert(RelayHoldDoc::key(&[7; 16], &content), 1_000 + i as u64);
        }
        let newest_stamp = 1_000 + (RELAY_HOLD_TOMBSTONE_CAP + 1) as u64;
        doc.restore_expired(m, newest_stamp);
        assert_eq!(doc.expired_at_ms().len(), RELAY_HOLD_TOMBSTONE_CAP);
        assert!(
            !doc.expired_at_ms().values().any(|t| *t < 1_002),
            "the two OLDEST tombstones were evicted"
        );
    }

    #[test]
    fn redeposit_after_expiry_clears_tombstone_so_restart_keeps_the_hold() {
        // PR #667 R2 (Greptile): an ungated re-deposit of the same sealed
        // bytes re-inserts a tombstoned key; acceptance must also CLEAR the
        // tombstone, else the persisted pair (live entry + stale tombstone)
        // lets restore_expired delete the acked hold at the next boot.
        let k = key_rr(1, 1);
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        doc.gc(0);
        doc.gc(RELAY_HOLD_TTL_MS + 1);
        assert!(doc.expired_at_ms().contains_key(&k));
        // The deposit path re-accepts the same sealed bytes (same key).
        doc.clear_tombstone(&k);
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(2, "a"), "relay", &[]),
        );
        assert!(!doc.expired_at_ms().contains_key(&k), "tombstone cleared");
        // Simulated restart: what persist saves is what restore receives.
        let saved = doc.expired_at_ms().clone();
        let mut rebooted = RelayHoldDoc::default();
        rebooted.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(2, "a"), "relay", &[]),
        );
        rebooted.restore_expired(saved, RELAY_HOLD_TTL_MS + 2);
        assert!(
            rebooted.entries.contains_key(&k),
            "accepted redelivery survives restart"
        );
    }

    #[test]
    fn restore_prunes_aged_out_tombstones_and_lets_their_entries_live() {
        use crate::community_relay::RELAY_HOLD_TOMBSTONE_RETENTION_MS;
        // PR #667 R1 (CodeAnt): a sidecar tombstone older than the retention
        // window must not suppress (or delete) anything at boot — pruning runs
        // BEFORE tombstones are applied to entries, so boot behavior matches
        // steady-state GC behavior.
        let k = key_rr(1, 1);
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let now = RELAY_HOLD_TOMBSTONE_RETENTION_MS + 10_000;
        doc.restore_expired([(k.clone(), 5u64)].into_iter().collect(), now);
        assert!(
            doc.expired_at_ms().is_empty(),
            "aged-out tombstone pruned at restore"
        );
        assert!(
            doc.entries.contains_key(&k),
            "entry survives — expired suppression must not delete it"
        );
    }

    #[test]
    fn restore_expired_removes_tombstoned_entries_and_clamps_future_stamps() {
        let k = key_rr(1, 1);
        let live = key_rr(2, 2);
        let mut doc = RelayHoldDoc::default();
        // Stale doc file resurrected an entry this replica already expired…
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        doc.entries.insert(
            live.clone(),
            entry([2; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let now = 1_000_000u64;
        // …and the sidecar carries a FUTURE stamp (backward clock across restart).
        doc.restore_expired([(k.clone(), now + 5_000_000)].into_iter().collect(), now);
        assert!(
            !doc.entries.contains_key(&k),
            "tombstone wins over the stale doc file"
        );
        assert!(
            doc.entries.contains_key(&live),
            "non-tombstoned entry untouched"
        );
        assert_eq!(doc.expired_at_ms()[&k], now, "future stamp rebased to now");
    }
}
