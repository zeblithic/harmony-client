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
    /// (deterministically) at read time.
    pub fn note(&mut self, cid_hex: &str, zid: &str, now_ms: u64) {
        self.inner
            .entry(cid_hex.to_string())
            .or_default()
            .insert(zid.to_string(), now_ms);
    }

    /// Distinct peer sessions seen announcing `cid_hex` (unswept entries).
    pub fn peer_count(&self, cid_hex: &str) -> u32 {
        self.inner.get(cid_hex).map_or(0, |m| m.len() as u32)
    }

    /// Drop entries not refreshed within `ttl_ms` of `now_ms`, then drop
    /// CIDs with no remaining holders (mirrors `CommunityPresenceMap::sweep`).
    pub fn sweep(&mut self, now_ms: u64, ttl_ms: u64) {
        for holders in self.inner.values_mut() {
            holders.retain(|_, last| now_ms.saturating_sub(*last) <= ttl_ms);
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

    #[test]
    fn sweep_drops_cids_with_no_holders() {
        let mut h = ObservedHolders::new();
        h.note("aa", "zid-1", 0);
        h.sweep(10_000, 100);
        assert_eq!(h.peer_count("aa"), 0);
        assert!(h.inner.is_empty(), "empty cid entries must be dropped");
    }

    #[test]
    fn stale_ttl_is_three_reannounce_intervals() {
        // The presence-map discipline (community_presence.rs): TTL = 3× the
        // announce interval so two lost announcements don't evict a live
        // holder.
        assert_eq!(HOLDER_STALE_MS, 3 * REANNOUNCE_INTERVAL_MS);
    }
}
