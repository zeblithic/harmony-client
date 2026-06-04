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

/// One recorded inbound friend request awaiting the user's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInbound {
    /// The requester's advertised display name (UX hint), if any.
    pub display: Option<String>,
    /// Wall-clock epoch-ms the request was first recorded (idempotent: a
    /// re-dial before acceptance does NOT bump this).
    pub received_at_ms: u64,
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
        });
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

    /// Decline `addr`: drop any recorded inbound request AND any approval.
    pub fn decline(&self, addr: &OwnerAddr) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.inbound.remove(addr);
        inner.approved.remove(addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> OwnerAddr {
        OwnerAddr([b; 16])
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
}
