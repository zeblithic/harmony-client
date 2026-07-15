//! ZEB-371 Task 12: process-local pending inbound friend-request store.
//!
//! Path A (mutual-key, no token) records an inbound `FriendLinkRequest` from a
//! NEW owner here and replies `FriendLinkResponse::Pending` (writing NO friend)
//! until the user explicitly accepts. The user-facing accept/decline/add IPCs
//! (the NEXT task) reach this store via the `Arc<PendingFriendRequests>` parked
//! on `NodeState`:
//!   * accept → [`PendingFriendRequests::approve`] (atomically removes the
//!     inbound entry from the inbox AND marks the requester approved so their
//!     NEXT dial passes the consent gate via `prior_accept`),
//!   * decline → [`PendingFriendRequests::decline`] (drop the inbound + any
//!     approval),
//!   * the acceptor, on a re-dial it accepts inline, consumes the approval via
//!     [`PendingFriendRequests::take_approved`].
//!
//! This is PROCESS-LOCAL (not owner-state CRDT) and intentionally ephemeral: a
//! pending request that hasn't been accepted by shutdown is simply re-sent by
//! the requester's next dial. No persistence, no replication.

use crate::owner_state_types::OwnerAddr;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// ZEB-376 Task 11: what KIND of pending inbox entry this is. A plain Path-A
/// `LinkRequest` (mutual-key dial) is accepted by marking the requester approved
/// so their next dial links inline; an `IntroductionOffer` (AskMe-staged F→X
/// vouch) is accepted by X actively dialing the introducee via
/// `complete_introduction` — the SAME action an auto-`Proceed` runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingKind {
    /// Path-A mutual-key inbound request (the historical default).
    LinkRequest,
    /// AskMe-staged introduction offer carrying the already-verified vouch +
    /// relayed reachability the accept path needs to dial the introducee. Boxed
    /// so the enum (and `PendingInbound`) stays small for the common
    /// `LinkRequest` case.
    IntroductionOffer(Box<StoredIntroductionOffer>),
}

/// ZEB-376 Task 11: the introduction data staged for an AskMe accept. Recorded
/// by the friend-PEX `Introduction` arm AFTER it has already verified F's vouch,
/// the relayed reachability's inner signature, and its freshness — so a staged
/// offer is trustworthy. On the user's explicit accept, `take_offer` hands this
/// back and X runs `complete_introduction(subject, reachability, …)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredIntroductionOffer {
    /// The voucher (F) who relayed this introduction — surfaced to the UI as
    /// `introducedBy` and used only for display / provenance.
    pub voucher: OwnerAddr,
    /// The introducee (X's prospective friend) X dials on accept.
    pub subject: OwnerAddr,
    /// The subject's relayed, already-verified reachability X synthesizes the
    /// dial target from.
    pub reachability: crate::reachability_record::ReachabilityAnnouncePayload,
}

/// One recorded inbound friend request awaiting the user's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInbound {
    /// The requester's advertised display name (UX hint), if any.
    pub display: Option<String>,
    /// Wall-clock epoch-ms the request was first recorded (idempotent: a
    /// re-dial before acceptance does NOT bump this).
    pub received_at_ms: u64,
    /// ZEB-376 Task 11: whether this is a plain Path-A `LinkRequest` or an
    /// AskMe-staged `IntroductionOffer` (which the accept path runs as a
    /// self-dial link rather than a `prior_accept` approval).
    pub kind: PendingKind,
}

#[derive(Default)]
struct Inner {
    inbound: HashMap<OwnerAddr, PendingInbound>,
    approved: HashSet<OwnerAddr>,
}

/// Process-local store of pending inbound friend requests and the set of
/// requesters the user has approved (but whose link hasn't completed yet).
///
/// A single `Mutex<Inner>` makes `approve` (remove inbound + insert approved)
/// atomic with respect to concurrent `record_inbound` calls, preventing a
/// re-dial from resurrecting a just-approved request. The lock is held only
/// for the duration of a single map op — never across an `.await`.
#[derive(Default)]
pub struct PendingFriendRequests {
    inner: Mutex<Inner>,
}

impl PendingFriendRequests {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an inbound request from `addr`. IDEMPOTENT: a request already
    /// recorded (and not yet declined/accepted) keeps its original
    /// `received_at_ms` and display — a re-dial does not reset the entry.
    /// If `addr` has already been approved, the call is a no-op (prevents
    /// a concurrent re-dial from resurrecting the inbox entry).
    pub fn record_inbound(&self, addr: OwnerAddr, display: Option<String>, now_ms: u64) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        if inner.approved.contains(&addr) {
            return;
        }
        inner.inbound.entry(addr).or_insert(PendingInbound {
            display,
            received_at_ms: now_ms,
            kind: PendingKind::LinkRequest,
        });
    }

    /// ZEB-376 Task 11: stage an AskMe introduction OFFER for `subject` in the
    /// pending inbox. IDEMPOTENT (like [`record_inbound`](Self::record_inbound)):
    /// a re-delivered introduction keeps the original entry rather than resetting
    /// its timestamp. The offer carries the already-verified vouch + relayed
    /// reachability the accept path consumes via [`take_offer`](Self::take_offer).
    pub fn record_introduction_offer(
        &self,
        subject: OwnerAddr,
        display: Option<String>,
        now_ms: u64,
        offer: StoredIntroductionOffer,
    ) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.inbound.entry(subject).or_insert(PendingInbound {
            display,
            received_at_ms: now_ms,
            kind: PendingKind::IntroductionOffer(Box::new(offer)),
        });
    }

    /// ZEB-376 Task 11: remove + return the staged introduction offer for
    /// `subject` (the user tapped Accept on an introduction). Returns `None` when
    /// `subject` has no entry OR its entry is a plain `LinkRequest` — leaving a
    /// `LinkRequest` entry INTACT so the accept-IPC falls through to the Path-A
    /// [`approve`](Self::approve) branch. One-shot: a second call returns `None`.
    pub fn take_offer(&self, subject: &OwnerAddr) -> Option<StoredIntroductionOffer> {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        // Only consume the entry when it is actually an IntroductionOffer.
        match inner.inbound.get(subject) {
            Some(p) if matches!(p.kind, PendingKind::IntroductionOffer(_)) => {
                match inner.inbound.remove(subject).map(|p| p.kind) {
                    Some(PendingKind::IntroductionOffer(offer)) => Some(*offer),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// ZEB-376 Task 11: non-consuming peek — true iff `subject` has a staged
    /// `IntroductionOffer` (NOT a plain `LinkRequest`). Lets the accept path
    /// validate its self-dial handles BEFORE the one-shot [`take_offer`](Self::take_offer)
    /// irreversibly consumes the offer (HARD-RULE: verify before an irreversible
    /// write — a missing handle must leave the offer row intact + recoverable).
    /// Read-only; a subsequent `take_offer` is still the single consuming op.
    pub fn has_offer(&self, subject: &OwnerAddr) -> bool {
        let inner = self.inner.lock().expect("pending inner mutex poisoned");
        matches!(
            inner.inbound.get(subject),
            Some(p) if matches!(p.kind, PendingKind::IntroductionOffer(_))
        )
    }

    /// Snapshot the currently-pending inbound requests (for the list IPC).
    pub fn list(&self) -> Vec<(OwnerAddr, PendingInbound)> {
        let inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner
            .inbound
            .iter()
            .map(|(addr, p)| (*addr, p.clone()))
            .collect()
    }

    /// True if the user has approved `addr` (so the requester's next dial can be
    /// accepted inline via the consent decision's `prior_accept`).
    pub fn is_approved(&self, addr: &OwnerAddr) -> bool {
        self.inner
            .lock()
            .expect("pending inner mutex poisoned")
            .approved
            .contains(addr)
    }

    /// Mark `addr` as approved (the user tapped Accept). The approval persists
    /// until the link completes ([`take_approved`](Self::take_approved)) or the
    /// user declines ([`decline`](Self::decline)).
    ///
    /// Prefer [`approve`](Self::approve) for the accept-IPC path — it also
    /// removes the entry from the pending inbox atomically.
    pub fn mark_approved(&self, addr: OwnerAddr) {
        self.inner
            .lock()
            .expect("pending inner mutex poisoned")
            .approved
            .insert(addr);
    }

    /// The user accepted `addr`: drop it from the pending inbox AND record the
    /// approval atomically (so the requester's next dial completes via
    /// `prior_accept`). The inbox no longer shows it; the friend appears in the
    /// friends list once the link completes.
    pub fn approve(&self, addr: OwnerAddr) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.inbound.remove(&addr);
        inner.approved.insert(addr);
    }

    /// Remove + return whether `addr` was approved. Called once the inbound
    /// handshake actually completes the inline accept, so a single Accept tap
    /// authorises exactly one completed link (re-arming requires a fresh tap).
    pub fn take_approved(&self, addr: &OwnerAddr) -> bool {
        self.inner
            .lock()
            .expect("pending inner mutex poisoned")
            .approved
            .remove(addr)
    }

    /// A link with `addr` completed (via token redeem OR inline accept): drop
    /// any lingering pending-inbox entry AND consume any approval, atomically.
    ///
    /// This is the completion-cleanup the acceptor calls on EVERY path that
    /// writes an Active friend. Without it, a requester who earlier received
    /// `Pending` (recorded in the inbox) and then became an active friend
    /// through a *different* path — e.g. redeeming a friend token (`TokenPath`)
    /// or being added by key — would linger as a "ghost" request in
    /// `list_pending_friend_requests` even though they are already a friend.
    /// Idempotent: a no-op when neither set holds `addr`.
    pub fn clear_completed(&self, addr: &OwnerAddr) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.inbound.remove(addr);
        inner.approved.remove(addr);
    }

    /// Decline `addr`: drop any recorded inbound request AND any approval.
    pub fn decline(&self, addr: &OwnerAddr) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.inbound.remove(addr);
        inner.approved.remove(addr);
    }
}

/// ZEB-376: process-local pre-authorization for introductions the user
/// initiated. When you send an `IntroduceRequest` for target X you `record(X)`;
/// X's inbound introduction-driven `FriendLinkRequest` then auto-accepts because
/// its authenticated sender is `take`-able here. One-shot + TTL-bounded so a
/// stale pre-auth can't silently accept an unrelated later dial. Not persisted
/// (ephemeral, like `PendingFriendRequests`).
#[derive(Default)]
pub struct PendingOutboundIntroductions {
    inner: Mutex<HashMap<OwnerAddr, u64>>,
}

/// TTL bounding a recorded pre-authorization: a `record`ed target older than
/// this (in wall-clock epoch-ms) never authorizes an inline-introduced accept.
pub const OUTBOUND_INTRO_TTL_MS: u64 = 10 * 60 * 1000; // 10 min

impl PendingOutboundIntroductions {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (idempotent refresh of the deadline for) a pre-authorization for
    /// `target` at `now_ms`. A later `record` bumps the recorded instant.
    pub fn record(&self, target: OwnerAddr, now_ms: u64) {
        self.inner
            .lock()
            .expect("outbound-intro mutex poisoned")
            .insert(target, now_ms);
    }

    /// Remove + return true iff `target` was recorded AND still within the TTL.
    /// A present-but-expired entry is removed and returns false. One-shot: even
    /// a fresh hit is consumed, so a single pre-auth accepts exactly one dial.
    pub fn take(&self, target: &OwnerAddr, now_ms: u64) -> bool {
        let mut m = self.inner.lock().expect("outbound-intro mutex poisoned");
        match m.remove(target) {
            Some(rec) => now_ms.saturating_sub(rec) < OUTBOUND_INTRO_TTL_MS,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> OwnerAddr {
        OwnerAddr([b; 16])
    }

    /// Minimal (unsigned) reachability payload — `take_offer`/`record_*` never
    /// inspect its contents, so a zeroed record suffices for the store tests.
    fn fixture_reach() -> crate::reachability_record::ReachabilityAnnouncePayload {
        crate::reachability_record::ReachabilityAnnouncePayload {
            iroh_node_id: [7u8; 32],
            home_relay_url: String::new(),
            direct_addresses: Vec::new(),
            announced_at_ms: 0,
            identity_signature: [0u8; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        }
    }

    #[test]
    fn introduction_offer_stages_and_take_consumes() {
        let store = PendingFriendRequests::new();
        let offer = StoredIntroductionOffer {
            voucher: addr(2),
            subject: addr(1),
            reachability: fixture_reach(),
        };
        store.record_introduction_offer(addr(1), Some("alice".into()), 1_000, offer);
        // Surfaces in the inbox with its introduced_by voucher.
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert!(
            matches!(&list[0].1.kind, PendingKind::IntroductionOffer(o) if o.voucher == addr(2))
        );
        // take_offer consumes it once.
        assert!(store.take_offer(&addr(1)).is_some());
        assert!(store.take_offer(&addr(1)).is_none());
        assert!(store.list().is_empty());
    }

    #[test]
    fn take_offer_leaves_link_request_intact() {
        // A plain Path-A LinkRequest must NOT be consumed by the introduction
        // accept path — take_offer returns None and leaves the row so the accept
        // IPC falls through to the `approve` branch.
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(3), Some("bob".into()), 2_000);
        assert!(store.take_offer(&addr(3)).is_none());
        assert_eq!(store.list().len(), 1, "LinkRequest row must remain");
        assert!(matches!(&store.list()[0].1.kind, PendingKind::LinkRequest));
    }

    #[test]
    fn record_then_list_returns_inbound() {
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(1), Some("alice".into()), 1_000);
        let list = store.list();
        assert_eq!(list.len(), 1);
        let (a, p) = &list[0];
        assert_eq!(*a, addr(1));
        assert_eq!(p.display.as_deref(), Some("alice"));
        assert_eq!(p.received_at_ms, 1_000);
    }

    #[test]
    fn record_inbound_is_idempotent() {
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(2), Some("first".into()), 1_000);
        // A re-dial must NOT overwrite the original entry (display + timestamp).
        store.record_inbound(addr(2), Some("second".into()), 9_999);
        let list = store.list();
        assert_eq!(
            list.len(),
            1,
            "duplicate record must not create a 2nd entry"
        );
        let (_, p) = &list[0];
        assert_eq!(p.display.as_deref(), Some("first"));
        assert_eq!(p.received_at_ms, 1_000);
    }

    #[test]
    fn approve_then_take_consumes_once() {
        let store = PendingFriendRequests::new();
        assert!(!store.is_approved(&addr(3)), "unknown addr is not approved");
        store.approve(addr(3));
        assert!(store.is_approved(&addr(3)));
        // take_approved removes + returns true the first time…
        assert!(store.take_approved(&addr(3)));
        // …and false thereafter (consumed).
        assert!(!store.take_approved(&addr(3)));
        assert!(!store.is_approved(&addr(3)));
    }

    #[test]
    fn approve_clears_inbox_and_records_approval() {
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(8), Some("bob".into()), 1_000);
        store.approve(addr(8));
        assert!(
            store.list().is_empty(),
            "approved request leaves the pending inbox"
        );
        assert!(
            store.is_approved(&addr(8)),
            "approval recorded for the next dial"
        );
        // one-shot completion still consumes the approval
        assert!(store.take_approved(&addr(8)));
        assert!(!store.take_approved(&addr(8)));
    }

    #[test]
    fn decline_drops_inbound_and_approval() {
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(4), None, 5_000);
        store.mark_approved(addr(4));
        store.decline(&addr(4));
        assert!(store.list().is_empty(), "decline drops the inbound request");
        assert!(!store.is_approved(&addr(4)), "decline drops the approval");
        // take_approved is now a no-op (returns false).
        assert!(!store.take_approved(&addr(4)));
    }

    #[test]
    fn take_approved_on_unknown_is_false() {
        let store = PendingFriendRequests::new();
        assert!(!store.take_approved(&addr(5)));
    }

    #[test]
    fn clear_completed_drops_stale_inbox_entry() {
        // Regression (Cursor "Stale pending after link completes"): a requester
        // recorded as Pending who later becomes an active friend via another
        // path (e.g. token redeem) must NOT linger in the pending inbox.
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(7), Some("dora".into()), 1_000);
        assert_eq!(store.list().len(), 1, "precondition: recorded as pending");
        store.clear_completed(&addr(7));
        assert!(
            store.list().is_empty(),
            "completed link must clear the stale inbox entry (no ghost request)"
        );
    }

    #[test]
    fn clear_completed_consumes_approval_and_is_idempotent() {
        let store = PendingFriendRequests::new();
        store.approve(addr(9)); // user tapped Accept → approved (inbox already cleared)
        store.clear_completed(&addr(9));
        assert!(
            !store.is_approved(&addr(9)),
            "completion consumes the one-shot approval"
        );
        // Idempotent: a second call on an unknown addr is a harmless no-op.
        store.clear_completed(&addr(9));
        assert!(store.list().is_empty());
    }

    #[test]
    fn record_inbound_skips_already_approved() {
        // A re-dial from an already-approved peer must NOT resurrect the inbox
        // entry — approve+record_inbound must be atomic via the single mutex.
        let store = PendingFriendRequests::new();
        store.approve(addr(6));
        store.record_inbound(addr(6), Some("charlie".into()), 5_000);
        assert!(
            store.list().is_empty(),
            "record_inbound must not add to inbox when addr is already approved"
        );
    }

    #[test]
    fn outbound_intro_take_is_one_shot_and_ttl_bounded() {
        let s = PendingOutboundIntroductions::new();
        s.record(addr(1), 1_000);
        // Fresh + present → true, and consumed (one-shot).
        assert!(s.take(&addr(1), 1_500));
        assert!(!s.take(&addr(1), 1_600));
        // Expired records never authorize.
        s.record(addr(2), 1_000);
        assert!(!s.take(&addr(2), 1_000 + OUTBOUND_INTRO_TTL_MS + 1));
        // Unknown target → false.
        assert!(!s.take(&addr(3), 2_000));
    }
}
