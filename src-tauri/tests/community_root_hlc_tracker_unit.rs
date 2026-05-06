//! Unit tests for CommunityRootHlcTracker — replay protection +
//! dedupe-merge monotonicity gates.
//!
//! Bug-class context: PR #81 round 3 fixed an HLC-tracker monotonicity
//! regression where dedupe-merging two SpaceIds with the same dedupe
//! key would clobber the per-device latest-seen HLC backward. Community
//! state-root tracking has the same shape (per-device latest-accepted
//! HLC) and would fail the same way without explicit testing.

use harmony_app::community_state_sync::CommunityRootHlcTracker;
use harmony_app::owner_state_types::Hlc;

fn h(wall: u64, log: u32, dev: &str) -> Hlc {
    Hlc {
        wall_ms: wall,
        logical: log,
        device_id: dev.into(),
    }
}

#[test]
fn would_accept_returns_true_for_unseen_device() {
    let t = CommunityRootHlcTracker::default();
    assert!(t.would_accept(&h(100, 0, "a")));
}

#[test]
fn would_accept_rejects_equal_or_older() {
    let mut t = CommunityRootHlcTracker::default();
    t.record(h(100, 0, "a"));
    assert!(!t.would_accept(&h(100, 0, "a")), "exact replay rejected");
    assert!(!t.would_accept(&h(99, 5, "a")), "older wall_ms rejected");
    assert!(t.would_accept(&h(100, 1, "a")), "later logical accepted");
    assert!(t.would_accept(&h(101, 0, "a")), "later wall_ms accepted");
}

#[test]
fn would_accept_blocks_regression_at_caller() {
    // The bug-class from PR #81 round 3: if two paths ever feed the
    // tracker out of order and the caller skips `would_accept`, the
    // next legitimate publish from that device could be rejected (it's
    // "older than" the regressed value but we already saw a newer
    // one). The new API surfaces that bug at the caller — record()
    // debug_asserts the precondition — so this test pins that the
    // caller-facing check correctly rejects the older input.
    let mut t = CommunityRootHlcTracker::default();
    t.record(h(200, 0, "a"));
    assert!(
        !t.would_accept(&h(100, 0, "a")),
        "older HLC must be caller-rejected, never reach record()"
    );
    // The state remains pinned at 200 because record(100) was skipped.
    assert!(!t.would_accept(&h(150, 0, "a")), "still bounded by 200");
    assert!(t.would_accept(&h(201, 0, "a")), "201 > 200");
}

#[test]
fn record_per_device_isolates_clocks() {
    let mut t = CommunityRootHlcTracker::default();
    t.record(h(500, 0, "a"));
    // device b is unseen; new HLC accepted regardless of a's clock
    assert!(t.would_accept(&h(100, 0, "b")));
    t.record(h(100, 0, "b"));
    assert!(!t.would_accept(&h(99, 0, "b")));
}

#[test]
fn record_is_strictly_newer_per_lex_order() {
    // Hlc::is_strictly_newer_than uses lex order over (wall_ms, logical):
    // pin that this composes with the tracker's per-device map.
    let mut t = CommunityRootHlcTracker::default();
    t.record(h(100, 5, "a"));
    assert!(!t.would_accept(&h(100, 5, "a")), "exact equal rejected");
    assert!(!t.would_accept(&h(100, 4, "a")), "lower logical rejected");
    assert!(t.would_accept(&h(100, 6, "a")), "higher logical accepted");
    assert!(t.would_accept(&h(101, 0, "a")), "higher wall accepted");
    assert!(!t.would_accept(&h(99, 999, "a")), "lower wall rejected");
}
