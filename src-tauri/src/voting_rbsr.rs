//! ZEB-932: range-based set reconciliation (RBSR) for the voting log.
//!
//! Approach A (see `docs/superpowers/specs/2026-08-15-zeb932-voting-log-rbsr-design.md`):
//! reuse the generic [`crate::channel_rbsr`] protocol core over the voting log,
//! replacing the O(peers × events) 300 s full-dump with a range-reconcile whose
//! wire cost tracks the diff between two peers.
//!
//! Unlike the disk-segmented channel log, the voting log is fully in-memory
//! (`VotingLog.events: Vec<SignedVotingEvent>`), so the reconcile source is
//! built **transiently** from `events` at reconcile time rather than stored on
//! the log. This avoids maintaining an index across `events`' many append sites,
//! keeps `VotingLog` cheap to `Clone`, and makes the archive boundary correct by
//! construction: a source derived from the already-pruned `events` can never
//! advertise an archived (pruned-away) key. Because the log is in-memory, the
//! channel log's `ChunkIndex` disk-bounding is unnecessary — a direct fold over
//! the sorted slice (à la [`crate::channel_rbsr::SliceSource`]) is exact and
//! cheap, and the modular-sum fingerprint is order-independent by construction.

use crate::channel_rbsr::{RangeFingerprint, RangeReconcileSource, ReconcileKey};
use crate::community_voting_core::SignedVotingEvent;
use crate::community_voting_tier3::canonical_key;

/// Canonical RBSR set-element key for a voting event —
/// `(wall_ms, logical, device_id, sha256(signing_bytes))`. Identical shape to
/// the channel log's `reconcile_key`; already computed by [`canonical_key`], so
/// the element hash matches what `apply_event` derives internally.
pub(crate) fn voting_reconcile_key(ev: &SignedVotingEvent) -> ReconcileKey {
    canonical_key(ev)
}

/// A transient in-memory [`RangeReconcileSource`] over one community's voting
/// events, sorted ascending by [`ReconcileKey`]. Built fresh per reconcile from
/// `VotingLog.events`; never stored (the log is `Clone` and mutated by archival).
pub(crate) struct VotingReconcileSource {
    entries: Vec<(ReconcileKey, [u8; 32])>,
}

impl VotingReconcileSource {
    /// Build the sorted `(key, element_hash)` index from a community's events.
    /// Deduplicates by key so a re-applied event can't double-count in a
    /// fingerprint.
    pub(crate) fn from_events(events: &[SignedVotingEvent]) -> Self {
        let mut entries: Vec<(ReconcileKey, [u8; 32])> = events
            .iter()
            .map(|ev| {
                let k = voting_reconcile_key(ev);
                let h = k.3;
                (k, h)
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        Self { entries }
    }

    /// Half-open `[lo, hi)` slice of the sorted entries. `e.max(s)` keeps an
    /// inverted range a harmless empty slice rather than a panic (defense in
    /// depth; `validate_message` rejects such ranges at the trust boundary).
    fn slice(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> &[(ReconcileKey, [u8; 32])] {
        let s = self.entries.partition_point(|x| &x.0 < lo);
        let e = self.entries.partition_point(|x| &x.0 < hi);
        &self.entries[s..e.max(s)]
    }
}

impl RangeReconcileSource for VotingReconcileSource {
    fn range_fingerprint(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> RangeFingerprint {
        let mut f = RangeFingerprint::zero();
        for (_, h) in self.slice(lo, hi) {
            f.fold(h);
        }
        f
    }

    fn range_count(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> u64 {
        self.slice(lo, hi).len() as u64
    }

    fn keys_in_range(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> Vec<ReconcileKey> {
        self.slice(lo, hi).iter().map(|x| x.0.clone()).collect()
    }

    fn split_key(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> Option<ReconcileKey> {
        let s = self.slice(lo, hi);
        if s.len() < 2 {
            None
        } else {
            Some(s[s.len() / 2].0.clone())
        }
    }
}

/// Resolve a set of RBSR keys back to their full event bodies for inline `Have`
/// transfer. Distinct-key deduped so a duplicate body (same key seen twice)
/// can't mask another advertised-but-missing key when the responder count-checks
/// `bodies.len() == have_keys.len()`. A requested key absent from `events` is
/// simply not returned, so the caller detects the shortfall by length.
pub(crate) fn resolve_bodies(
    events: &[SignedVotingEvent],
    keys: &[ReconcileKey],
) -> Vec<SignedVotingEvent> {
    if keys.is_empty() {
        return Vec::new();
    }
    let wanted: std::collections::HashSet<ReconcileKey> = keys.iter().cloned().collect();
    // Track DISTINCT resolved keys so a duplicate body can't mask another
    // advertised-but-missing key when the caller count-checks the result length.
    let mut found: std::collections::HashSet<ReconcileKey> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for ev in events {
        let key = voting_reconcile_key(ev);
        if wanted.contains(&key) && found.insert(key) {
            out.push(ev.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_rbsr::{max_key, min_key, SliceSource};
    use crate::community_voting_core::{PollEventKindCode, Tier};
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn ev(wall_ms: u64, logical: u32, dev: &str, marker: u8) -> SignedVotingEvent {
        SignedVotingEvent {
            tag: 'v',
            version: 1,
            tier: Tier::Approval,
            kind: PollEventKindCode::BallotCast,
            hlc: Hlc {
                wall_ms,
                logical,
                device_id: dev.to_string(),
            },
            actor: OwnerAddr([marker; 16]),
            payload: vec![marker, marker.wrapping_add(1)],
            sig: Vec::new(),
        }
    }

    fn entries_of(events: &[SignedVotingEvent]) -> Vec<(ReconcileKey, [u8; 32])> {
        events
            .iter()
            .map(|e| {
                let k = voting_reconcile_key(e);
                (k.clone(), k.3)
            })
            .collect()
    }

    /// Deliberately unsorted, all distinct keys.
    fn sample() -> Vec<SignedVotingEvent> {
        vec![
            ev(300, 0, "d2", 7),
            ev(100, 1, "d1", 3),
            ev(100, 0, "d1", 2),
            ev(500, 2, "d3", 9),
            ev(300, 0, "d1", 5),
            ev(200, 0, "d2", 4),
        ]
    }

    #[test]
    fn matches_slicesource_oracle() {
        let events = sample();
        let oracle = SliceSource::from_unsorted(entries_of(&events));
        let src = VotingReconcileSource::from_events(&events);

        let mut keys: Vec<ReconcileKey> = entries_of(&events).into_iter().map(|x| x.0).collect();
        keys.sort();
        let bounds = vec![
            (min_key(), max_key()),
            (keys[1].clone(), keys[4].clone()),
            (min_key(), keys[3].clone()),
            (keys[2].clone(), max_key()),
        ];
        for (lo, hi) in bounds {
            assert_eq!(
                src.range_count(&lo, &hi),
                oracle.range_count(&lo, &hi),
                "range_count mismatch vs oracle"
            );
            assert_eq!(src.keys_in_range(&lo, &hi), oracle.keys_in_range(&lo, &hi));
            assert_eq!(
                src.range_fingerprint(&lo, &hi).finalize(),
                oracle.range_fingerprint(&lo, &hi).finalize(),
                "fingerprint mismatch vs oracle"
            );
            assert_eq!(src.split_key(&lo, &hi), oracle.split_key(&lo, &hi));
        }
        assert_eq!(src.range_count(&min_key(), &max_key()), 6);
    }

    #[test]
    fn resolve_bodies_distinct_and_complete() {
        let events = sample();
        let all: Vec<ReconcileKey> = entries_of(&events).into_iter().map(|x| x.0).collect();

        assert_eq!(resolve_bodies(&events, &all).len(), events.len());

        let subset: Vec<ReconcileKey> = all.iter().take(3).cloned().collect();
        let got = resolve_bodies(&events, &subset);
        assert_eq!(got.len(), 3);
        for e in &got {
            assert!(subset.contains(&voting_reconcile_key(e)));
        }

        // A bogus key must not resolve to any body (caller detects by length).
        let mut with_bogus = subset.clone();
        with_bogus.push((999, 9, "zzz".to_string(), [0xFF; 32]));
        assert_eq!(
            resolve_bodies(&events, &with_bogus).len(),
            3,
            "bogus key must not resolve"
        );

        // Duplicate requested key → exactly one body (distinct dedup).
        let dup = vec![all[0].clone(), all[0].clone()];
        assert_eq!(resolve_bodies(&events, &dup).len(), 1);
    }

    #[test]
    fn fingerprint_is_insertion_order_independent() {
        let events = sample();
        let mut rev = events.clone();
        rev.reverse();
        let a = VotingReconcileSource::from_events(&events);
        let b = VotingReconcileSource::from_events(&rev);
        assert_eq!(a.range_count(&min_key(), &max_key()), 6);
        assert_eq!(
            a.range_fingerprint(&min_key(), &max_key()).finalize(),
            b.range_fingerprint(&min_key(), &max_key()).finalize()
        );
    }

    #[test]
    fn duplicate_events_dedup_in_source() {
        let mut events = sample();
        events.push(events[0].clone()); // exact dup → identical canonical_key
        let src = VotingReconcileSource::from_events(&events);
        assert_eq!(
            src.range_count(&min_key(), &max_key()),
            6,
            "duplicate event must collapse to one entry"
        );
    }
}
