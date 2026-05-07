//! Unit tests for CommunityRootHlcTracker — replay protection +
//! per-(addr, device) monotonicity gates.
//!
//! ZEB-256: tracker key changed from `device_id: String` to
//! `(OwnerAddr, String)`. Cross-addr collisions are now structurally
//! impossible — Alice cannot squat Bob's HLC slot even with the
//! MembershipKey, because tracker entries are namespaced by addr.

use harmony_app::community_state_sync::CommunityRootHlcTracker;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};

const ALICE: OwnerAddr = OwnerAddr([0xA1; 16]);
const BOB: OwnerAddr = OwnerAddr([0xB1; 16]);

fn h(wall: u64, log: u32, dev: &str) -> Hlc {
    Hlc {
        wall_ms: wall,
        logical: log,
        device_id: dev.into(),
    }
}

#[test]
fn would_accept_returns_true_for_unseen_addr_device() {
    let t = CommunityRootHlcTracker::default();
    assert!(t.would_accept(&ALICE, &h(100, 0, "a")));
}

#[test]
fn would_accept_rejects_equal_or_older_per_addr_device() {
    let mut t = CommunityRootHlcTracker::default();
    t.record(ALICE, h(100, 0, "a"));
    assert!(
        !t.would_accept(&ALICE, &h(100, 0, "a")),
        "exact replay rejected"
    );
    assert!(
        !t.would_accept(&ALICE, &h(99, 5, "a")),
        "older wall_ms rejected"
    );
    assert!(
        t.would_accept(&ALICE, &h(100, 1, "a")),
        "later logical accepted"
    );
    assert!(
        t.would_accept(&ALICE, &h(101, 0, "a")),
        "later wall_ms accepted"
    );
}

#[test]
fn cross_addr_same_device_id_is_isolated() {
    // ZEB-256 core defense: Alice publishes at (alice-dev, 200); Bob
    // submits at (alice-dev, 100). The tracker must accept Bob's
    // because his (BOB, "alice-dev") slot is unseen — the (ALICE,
    // "alice-dev") slot is irrelevant to Bob's namespace. Phase 2's
    // BTreeMap<String, Hlc> would reject Bob's because device_id
    // collisions clobbered each other; this test pins the fix.
    let mut t = CommunityRootHlcTracker::default();
    t.record(ALICE, h(200, 0, "alice-dev"));
    assert!(
        t.would_accept(&BOB, &h(100, 0, "alice-dev")),
        "Bob's slot must be independent of Alice's"
    );
    t.record(BOB, h(100, 0, "alice-dev"));
    // Pinning both still leaves them isolated.
    assert!(!t.would_accept(&ALICE, &h(199, 0, "alice-dev")));
    assert!(!t.would_accept(&BOB, &h(99, 0, "alice-dev")));
}

#[test]
fn would_accept_blocks_regression_at_caller_per_addr() {
    let mut t = CommunityRootHlcTracker::default();
    t.record(ALICE, h(200, 0, "a"));
    assert!(
        !t.would_accept(&ALICE, &h(100, 0, "a")),
        "older HLC must be caller-rejected"
    );
    assert!(
        !t.would_accept(&ALICE, &h(150, 0, "a")),
        "still bounded by 200"
    );
    assert!(t.would_accept(&ALICE, &h(201, 0, "a")), "201 > 200");
}

#[test]
fn record_per_addr_device_isolates_clocks() {
    let mut t = CommunityRootHlcTracker::default();
    t.record(ALICE, h(500, 0, "a"));
    assert!(
        t.would_accept(&ALICE, &h(100, 0, "b")),
        "different device under same addr"
    );
    t.record(ALICE, h(100, 0, "b"));
    assert!(!t.would_accept(&ALICE, &h(99, 0, "b")));
}

#[test]
fn record_is_strictly_newer_per_lex_order() {
    let mut t = CommunityRootHlcTracker::default();
    t.record(ALICE, h(100, 5, "a"));
    assert!(!t.would_accept(&ALICE, &h(100, 5, "a")));
    assert!(!t.would_accept(&ALICE, &h(100, 4, "a")));
    assert!(t.would_accept(&ALICE, &h(100, 6, "a")));
    assert!(t.would_accept(&ALICE, &h(101, 0, "a")));
    assert!(!t.would_accept(&ALICE, &h(99, 999, "a")));
}
