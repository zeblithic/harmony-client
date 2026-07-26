//! ZEB-612 S3: per-CID observed-holder tracking.
//!
//! Counts distinct Zenoh sessions (zids) seen announcing each CID on
//! `harmony/announce/{cid_hex}`. This is an OBSERVED LOWER BOUND on
//! replicas: announcements carry no owner identity, encrypted content
//! never announces (existence-leak policy — see `should_announce` in
//! harmony-content's storage_tier), and observation starts at loop
//! boot. UI copy must say "copies seen across your peers".
//!
//! Freshness: this node re-announces its own announceable content every
//! `REANNOUNCE_INTERVAL_MS` (driver in event_loop.rs); entries older
//! than `HOLDER_STALE_MS` are dropped by `sweep`. Timestamps are the
//! event loop's monotonic ms (`start.elapsed()`), never wall-clock.

use std::collections::HashMap;

/// How often the event loop re-announces own announceable content (ms).
pub const REANNOUNCE_INTERVAL_MS: u64 = 60_000;
/// Holder entries older than this are pruned — 3 missed re-announces,
/// the `community_presence.rs` interval/TTL discipline.
pub const HOLDER_STALE_MS: u64 = 3 * REANNOUNCE_INTERVAL_MS;
/// Abuse bounds (PR #441 round 1): announcements are unauthenticated, so a
/// hostile peer could flood unique CID/zid pairs to grow this map without
/// limit inside a TTL window. Both caps saturate-skip: new entries beyond
/// the cap are dropped (the count is an observed LOWER bound, so skipping
/// is honest), and the TTL sweep frees capacity as entries age out.
pub const MAX_TRACKED_CIDS: usize = 4096;
pub const MAX_HOLDERS_PER_CID: usize = 32;

/// cid_hex → (announcer zid → last_seen_ms).
#[derive(Debug, Default)]
pub struct ObservedHolders {
    inner: HashMap<String, HashMap<String, u64>>,
}

impl ObservedHolders {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an announcement of `cid_hex` by `zid` at `now_ms`. Callers
    /// must exclude the own session's zid — self is counted separately
    /// (deterministically) at read time. Saturate-skips beyond
    /// `MAX_TRACKED_CIDS` / `MAX_HOLDERS_PER_CID` (refreshes of already
    /// tracked entries always land).
    pub fn note(&mut self, cid_hex: &str, zid: &str, now_ms: u64) {
        match self.inner.get_mut(cid_hex) {
            Some(holders) => {
                if holders.len() >= MAX_HOLDERS_PER_CID && !holders.contains_key(zid) {
                    return;
                }
                holders.insert(zid.to_string(), now_ms);
            }
            None => {
                if self.inner.len() >= MAX_TRACKED_CIDS {
                    return;
                }
                self.inner
                    .entry(cid_hex.to_string())
                    .or_default()
                    .insert(zid.to_string(), now_ms);
            }
        }
    }

    /// Distinct peer sessions seen announcing `cid_hex` (unswept entries).
    pub fn peer_count(&self, cid_hex: &str) -> u32 {
        self.inner
            .get(cid_hex)
            .map_or(0, |m| u32::try_from(m.len()).unwrap_or(u32::MAX))
    }

    /// Drop entries not refreshed within `ttl_ms` of `now_ms`, then drop
    /// CIDs with no remaining holders (mirrors `CommunityPresenceMap::sweep`).
    pub fn sweep(&mut self, now_ms: u64, ttl_ms: u64) {
        for holders in self.inner.values_mut() {
            // ZEB-791: forward bound as well — a future stamp would otherwise
            // pin the holder permanently. Fail closed; `note()` re-adds any
            // holder still announcing.
            holders.retain(|_, last| now_ms >= *last && now_ms - *last <= ttl_ms);
        }
        self.inner.retain(|_, holders| !holders.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_counts_distinct_zids() {
        let mut h = ObservedHolders::new();
        h.note("aa", "zid-1", 100);
        h.note("aa", "zid-2", 110);
        h.note("bb", "zid-1", 120);
        assert_eq!(h.peer_count("aa"), 2);
        assert_eq!(h.peer_count("bb"), 1);
    }

    #[test]
    fn note_same_zid_refreshes_without_double_count() {
        let mut h = ObservedHolders::new();
        h.note("aa", "zid-1", 100);
        h.note("aa", "zid-1", 500);
        assert_eq!(h.peer_count("aa"), 1);
        // The refresh took: a sweep that would have evicted the 100-stamp
        // entry keeps the holder alive.
        h.sweep(600, 200);
        assert_eq!(h.peer_count("aa"), 1);
    }

    #[test]
    fn peer_count_unknown_cid_is_zero() {
        assert_eq!(ObservedHolders::new().peer_count("nope"), 0);
    }

    #[test]
    fn sweep_evicts_stale_keeps_fresh() {
        let mut h = ObservedHolders::new();
        h.note("aa", "zid-old", 0);
        h.note("aa", "zid-new", 900);
        h.sweep(1000, 200); // cutoff: last_seen >= 800
        assert_eq!(h.peer_count("aa"), 1);
    }

    /// ZEB-791: a holder stamped in the FUTURE must still be evictable —
    /// `saturating_sub` previously read it as age 0 and pinned it permanently.
    #[test]
    fn sweep_evicts_future_stamped_holder() {
        let mut h = ObservedHolders::new();
        h.note("aa", "zid-future", 100_000);
        h.sweep(1_000, 200);
        assert_eq!(
            h.peer_count("aa"),
            0,
            "a future-stamped holder must not be immortal"
        );
    }

    #[test]
    fn sweep_drops_cids_with_no_holders() {
        let mut h = ObservedHolders::new();
        h.note("aa", "zid-1", 0);
        h.sweep(10_000, 100);
        assert_eq!(h.peer_count("aa"), 0);
        assert!(h.inner.is_empty(), "empty cid entries must be dropped");
    }

    #[test]
    fn per_cid_holder_cap_saturate_skips_new_zids_but_refreshes_existing() {
        let mut h = ObservedHolders::new();
        for i in 0..MAX_HOLDERS_PER_CID {
            h.note("aa", &format!("zid-{i}"), 0);
        }
        assert_eq!(h.peer_count("aa"), MAX_HOLDERS_PER_CID as u32);
        // A flood zid beyond the cap is dropped…
        h.note("aa", "zid-flood", 10);
        assert_eq!(h.peer_count("aa"), MAX_HOLDERS_PER_CID as u32);
        // …but refreshing an already-tracked zid still lands (survives sweep).
        h.note("aa", "zid-0", 1_000);
        h.sweep(1_100, 200);
        assert_eq!(h.peer_count("aa"), 1, "only the refreshed zid survives");
    }

    #[test]
    fn global_cid_cap_saturate_skips_new_cids_but_tracked_cids_keep_noting() {
        let mut h = ObservedHolders::new();
        for i in 0..MAX_TRACKED_CIDS {
            h.note(&format!("cid-{i}"), "zid-1", 0);
        }
        // A flood CID beyond the cap is dropped…
        h.note("cid-flood", "zid-1", 10);
        assert_eq!(h.peer_count("cid-flood"), 0);
        // …but already-tracked CIDs still accept new holders.
        h.note("cid-0", "zid-2", 10);
        assert_eq!(h.peer_count("cid-0"), 2);
        // Sweeping frees capacity for new CIDs again.
        h.sweep(1_000_000, 200);
        h.note("cid-flood", "zid-1", 1_000_001);
        assert_eq!(h.peer_count("cid-flood"), 1);
    }

    #[test]
    fn stale_ttl_is_three_reannounce_intervals() {
        // The presence-map discipline (community_presence.rs): TTL = 3× the
        // announce interval so two lost announcements don't evict a live
        // holder.
        assert_eq!(HOLDER_STALE_MS, 3 * REANNOUNCE_INTERVAL_MS);
    }
}
