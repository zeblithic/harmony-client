//! Unit tests for the community replay tracker — replay protection +
//! per-(addr, device) monotonicity gates.
//!
//! ZEB-256: tracker key changed from `device_id: String` to
//! `(OwnerAddr, String)`. Cross-addr collisions are now structurally
//! impossible — Alice cannot squat Bob's HLC slot even with the
//! EpochKey, because tracker entries are namespaced by addr.
//!
//! ZEB-750: the tracker is now core `harmony_crdt_sync::ReplayTracker`
//! (aliased `CommunityReplayTracker`). The behaviour these tests pin is
//! unchanged — `admit` is the old `would_accept` and `commit` is the old
//! `record` — but the apply-before-advance ordering is now enforced by
//! the type system instead of a release-stripped `debug_assert!`:
//! `commit` consumes a `CommitTicket` that only `admit` can mint.
//!
//! Every test therefore states its expectation through `Admission`
//! rather than a bool, which also pins the `Echo` classification the old
//! API could not express.

use harmony_app::community_state_sync::CommunityReplayTracker;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_crdt_sync::Admission;

const ALICE: OwnerAddr = OwnerAddr([0xA1; 16]);
const BOB: OwnerAddr = OwnerAddr([0xB1; 16]);
/// The receiver running these trackers. Distinct from ALICE and BOB so
/// no test accidentally exercises the `Echo` path except the one that
/// means to.
const SELF: OwnerAddr = OwnerAddr([0x5E; 16]);

fn h(wall: u64, log: u32, dev: &str) -> Hlc {
    Hlc {
        wall_ms: wall,
        logical: log,
        device_id: dev.into(),
    }
}

fn key(addr: OwnerAddr, dev: &str) -> (OwnerAddr, String) {
    (addr, dev.to_string())
}

fn tracker() -> CommunityReplayTracker {
    CommunityReplayTracker::new(key(SELF, "self-dev"))
}

/// `admit` says yes; `commit` consumes the ticket and advances.
fn admit_and_commit(t: &mut CommunityReplayTracker, addr: OwnerAddr, clock: Hlc) {
    let k = key(addr, &clock.device_id);
    match t.admit(&k, &clock) {
        Admission::Accept(ticket) => assert!(t.commit(ticket), "watermark must advance"),
        other => panic!("expected Accept for {addr:?} @ {clock:?}, got {other:?}"),
    }
}

/// Whether `admit` would accept, without advancing anything. The direct
/// replacement for the old `would_accept`.
fn would_accept(t: &CommunityReplayTracker, addr: OwnerAddr, clock: &Hlc) -> bool {
    matches!(
        t.admit(&key(addr, &clock.device_id), clock),
        Admission::Accept(_)
    )
}

#[test]
fn admit_accepts_an_unseen_addr_device() {
    let t = tracker();
    assert!(would_accept(&t, ALICE, &h(100, 0, "a")));
}

#[test]
fn admit_rejects_equal_or_older_per_addr_device() {
    let mut t = tracker();
    admit_and_commit(&mut t, ALICE, h(100, 0, "a"));
    assert!(
        !would_accept(&t, ALICE, &h(100, 0, "a")),
        "exact replay rejected"
    );
    assert!(
        !would_accept(&t, ALICE, &h(99, 5, "a")),
        "older wall_ms rejected"
    );
    assert!(
        would_accept(&t, ALICE, &h(100, 1, "a")),
        "later logical accepted"
    );
    assert!(
        would_accept(&t, ALICE, &h(101, 0, "a")),
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
    let mut t = tracker();
    admit_and_commit(&mut t, ALICE, h(200, 0, "alice-dev"));
    assert!(
        would_accept(&t, BOB, &h(100, 0, "alice-dev")),
        "Bob's slot must be independent of Alice's"
    );
    admit_and_commit(&mut t, BOB, h(100, 0, "alice-dev"));
    // Pinning both still leaves them isolated.
    assert!(!would_accept(&t, ALICE, &h(199, 0, "alice-dev")));
    assert!(!would_accept(&t, BOB, &h(99, 0, "alice-dev")));
}

#[test]
fn admit_blocks_regression_per_addr() {
    let mut t = tracker();
    admit_and_commit(&mut t, ALICE, h(200, 0, "a"));
    assert!(
        !would_accept(&t, ALICE, &h(100, 0, "a")),
        "older HLC must be rejected"
    );
    assert!(
        !would_accept(&t, ALICE, &h(150, 0, "a")),
        "still bounded by 200"
    );
    assert!(would_accept(&t, ALICE, &h(201, 0, "a")), "201 > 200");
}

#[test]
fn commit_per_addr_device_isolates_clocks() {
    let mut t = tracker();
    admit_and_commit(&mut t, ALICE, h(500, 0, "a"));
    assert!(
        would_accept(&t, ALICE, &h(100, 0, "b")),
        "different device under same addr"
    );
    admit_and_commit(&mut t, ALICE, h(100, 0, "b"));
    assert!(!would_accept(&t, ALICE, &h(99, 0, "b")));
}

#[test]
fn admit_is_strictly_newer_per_lex_order() {
    let mut t = tracker();
    admit_and_commit(&mut t, ALICE, h(100, 5, "a"));
    assert!(!would_accept(&t, ALICE, &h(100, 5, "a")));
    assert!(!would_accept(&t, ALICE, &h(100, 4, "a")));
    assert!(would_accept(&t, ALICE, &h(100, 6, "a")));
    assert!(would_accept(&t, ALICE, &h(101, 0, "a")));
    assert!(!would_accept(&t, ALICE, &h(99, 999, "a")));
}

/// ZEB-750: dropping a ticket instead of committing it leaves the
/// watermark where it was — the retry-safe outcome for a publish that
/// failed to apply, and the invariant `handle_incoming_publish` relies on
/// when the membership gate rejects mid-pipeline.
#[test]
fn a_dropped_ticket_leaves_the_watermark_un_advanced() {
    let mut t = tracker();
    admit_and_commit(&mut t, ALICE, h(100, 0, "a"));

    let candidate = h(200, 0, "a");
    let k = key(ALICE, &candidate.device_id);
    match t.admit(&k, &candidate) {
        Admission::Accept(ticket) => drop(ticket),
        other => panic!("expected Accept, got {other:?}"),
    }

    // The watermark never moved, so the same frame is admissible again.
    assert!(
        would_accept(&t, ALICE, &candidate),
        "a dropped ticket must not advance the watermark"
    );
    assert_eq!(t.accepted_from(&k), Some(&h(100, 0, "a")));
}

/// ZEB-750: the receiver's own publish, reflected back by the transport,
/// is `Echo` — not a replay. The old `would_accept` had no way to say
/// this; it fell out as a plain rejection.
#[test]
fn a_self_publish_reflected_back_is_an_echo() {
    let t = tracker();
    let own = h(100, 0, "self-dev");
    assert!(matches!(
        t.admit(&key(SELF, "self-dev"), &own),
        Admission::Echo
    ));
}
