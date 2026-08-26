//! ZEB-236: process-local pending inbound DM-invite store.
//!
//! A verified `DmInvite` from a NON-active-friend inviter is staged here (not
//! applied) until the user explicitly accepts or declines. PROCESS-LOCAL and
//! deliberately ephemeral (mirrors `friend_requests::PendingFriendRequests`):
//! ZEB-483 co-deposits the rebuilt invite alongside every message CidNotify,
//! so an entry lost to a restart re-stages on the next inbound message, and
//! keeping nothing on disk is what makes decline write no persistent state
//! (spec §"DmInvite rejection / decline semantics (v1)").

use crate::dm_envelope::DmInviteSigned;
use crate::owner_state_types::{ContentId, SpaceId};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// One verified, staged DM invite awaiting the user's decision. Carries
/// everything the deferred accept needs to run the exact tail auto-accept
/// runs today.
#[derive(Debug, Clone)]
pub struct StagedDmInvite {
    /// The signature-verified invite (verified at staging time by
    /// `apply_invite`'s gates — accept does NOT re-verify).
    pub signed: DmInviteSigned,
    /// Wall-clock epoch-ms first staged (idempotent: redelivery keeps this).
    pub received_at_ms: u64,
    /// The ingest route's cache-refresh entitlement (tunnel=true,
    /// deposit-recover=false). Accept must honor the same trust distinction
    /// the auto-accept path applies (ZEB-483).
    pub refresh_owner_device_cache: bool,
    /// ZEB-236 (final review): the CAS `message_cid` of the notifying message
    /// this invite was CO-DEPOSITED alongside, when it arrived on a
    /// sweep-redelivered deposit path (deposit-recover / direct-relay) — `None`
    /// on the tunnel and invite-only paths. Used ONLY as the decline-ledger key
    /// (see [`PendingDmInvites`]): the co-deposit sweepers re-run every ~7.5 min
    /// / on startup and re-deliver the SAME notifying message (same
    /// `message_cid`) while the invite stays pending-and-unacked, so a declined
    /// invite must remember which message's redeliveries to suppress.
    pub source_cid: Option<ContentId>,
}

/// The single-`Mutex` interior of [`PendingDmInvites`]: the live pending map
/// plus the session-local declined ledger.
#[derive(Default)]
struct Inner {
    /// Currently-staged invites awaiting a user decision, keyed by `SpaceId`.
    pending: HashMap<SpaceId, StagedDmInvite>,
    /// ZEB-236 (final review): session-local declined ledger, keyed by
    /// `(SpaceId, source message cid)`.
    ///
    /// WHY cid-keyed: the co-deposit sweepers re-run every ~7.5 min / on
    /// startup and RE-DELIVER the SAME notifying message (same `message_cid`)
    /// for an invite that deliberately stays unacked while pending. Without
    /// this ledger, a user who DECLINED an invite would be re-prompted on every
    /// subsequent sweep of that identical message. Keying on the message's
    /// `message_cid` makes "same message" precise: a mechanical redelivery of
    /// the declined message is suppressed, while a genuinely NEW message from
    /// the same inviter carries a NEW cid and still re-prompts (spec: "repeat
    /// invites re-prompt").
    ///
    /// WHY session-local (never persisted): the decline contract (spec
    /// §"DmInvite rejection / decline semantics (v1)") requires decline to
    /// write NOTHING durable — a decline must stay indistinguishable from the
    /// receiver being offline. A restart therefore clears this ledger; the
    /// worst case after a restart is a single re-prompt of a previously
    /// declined message, which keeps the no-persistent-state invariant intact.
    declined: HashSet<(SpaceId, ContentId)>,
}

/// Process-local store of staged DM invites, keyed by `SpaceId`, plus a
/// session-local declined ledger (see [`Inner::declined`]). Single `Mutex` held
/// only for the duration of one map op — never across `.await`.
#[derive(Default)]
pub struct PendingDmInvites {
    inner: Mutex<Inner>,
}

impl PendingDmInvites {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage an invite. Returns `true` if newly staged; `false` when either an
    /// invite for the same `space_id` is already pending (keep-first — a
    /// ZEB-483 co-deposit redelivery must NOT bump `received_at_ms`, and the
    /// caller must NOT re-emit `dm-invite-received` for it) OR the incoming
    /// invite's `(space_id, source_cid=Some(cid))` was already declined this
    /// session (a sweep re-delivery of the SAME declined message must not
    /// re-prompt).
    pub fn stage(&self, staged: StagedDmInvite) -> bool {
        let mut inner = self.inner.lock().expect("pending dm invites poisoned");
        // Declined-ledger gate: a co-deposit redelivery carries the notifying
        // message's `source_cid`; if THAT (space, cid) was already declined this
        // session, suppress the re-stage (and, via the caller, the re-prompt).
        if let Some(cid) = staged.source_cid {
            if inner.declined.contains(&(staged.signed.space_id, cid)) {
                return false;
            }
        }
        match inner.pending.entry(staged.signed.space_id) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(staged);
                true
            }
        }
    }

    /// Snapshot the currently-pending invites (for the list IPC).
    pub fn list(&self) -> Vec<StagedDmInvite> {
        self.inner
            .lock()
            .expect("pending dm invites poisoned")
            .pending
            .values()
            .cloned()
            .collect()
    }

    /// Remove + return the staged invite for `space_id` WITHOUT recording a
    /// decline. ACCEPT consumes through here: an accepted invite writes the DM
    /// Space, so a later redelivery is admitted by the normal receive path (the
    /// Space now exists) and never re-stages — no ledger entry is needed.
    pub fn take(&self, space_id: &SpaceId) -> Option<StagedDmInvite> {
        self.inner
            .lock()
            .expect("pending dm invites poisoned")
            .pending
            .remove(space_id)
    }

    /// ZEB-236 (final review): DECLINE consumes through here. Removes + returns
    /// the staged invite for `space_id` AND, when it carried a co-deposit
    /// `source_cid`, records `(space_id, cid)` in the session-local declined
    /// ledger so a sweep re-delivery of the SAME message does not re-prompt. A
    /// tunnel/invite-only invite (`source_cid == None`) records NOTHING — those
    /// paths never sweep-redeliver, so a genuine re-send is a new sender action
    /// that SHOULD re-prompt. Decline must never use `take()` (which skips the
    /// ledger).
    pub fn decline(&self, space_id: &SpaceId) -> Option<StagedDmInvite> {
        let mut inner = self.inner.lock().expect("pending dm invites poisoned");
        let removed = inner.pending.remove(space_id);
        if let Some(cid) = removed.as_ref().and_then(|s| s.source_cid) {
            inner.declined.insert((*space_id, cid));
        }
        removed
    }
}

/// ZEB-236 (T3): stage a verified non-friend invite into the process-local
/// store and fire the UI events, shared by every live ingest route so all
/// paths emit identically. CANONICAL semantics: emit BOTH `dm-invite-received`
/// and `dm-invite-list-changed` ONLY when `stage()` reports a NEWLY-staged
/// invite. A non-new stage — a keep-first redelivery of an already-pending
/// invite, OR a sweep re-delivery of a declined message (ZEB-236 final review) —
/// emits NEITHER event: the pending list did not change, so re-emitting
/// `dm-invite-list-changed` on every ~7.5-min sweep would churn the list view
/// for no reason (and re-prompting via `dm-invite-received` is already
/// forbidden). Both payloads are `&()` — the frontend re-fetches via
/// `list_pending_dm_invites` (mirrors `friend-list-changed`).
///
/// `pending == None` means the store was not wired on this path (defensive; the
/// live routes always pass `Some`) — the invite is warn-logged and dropped
/// rather than silently swallowed.
///
/// CALLER CONTRACT: the `crdt_state` async lock guard MUST already be dropped
/// before calling this — `stage()` takes the store `Mutex` and this fires the
/// event sink, neither of which may nest inside the held owner-state lock.
pub fn stage_and_emit_staged_invite(
    pending: Option<&std::sync::Arc<PendingDmInvites>>,
    sink: &dyn crate::node_event_sink::NodeEventSink,
    staged: StagedDmInvite,
) {
    let Some(pending) = pending else {
        tracing::warn!("ZEB-236: staged DM invite dropped (pending store not wired on this path)");
        return;
    };
    let newly = pending.stage(staged);
    if newly {
        crate::node_event_sink::emit_ser(sink, "dm-invite-received", &());
        crate::node_event_sink::emit_ser(sink, "dm-invite-list-changed", &());
    }
}

/// ZEB-640 (1): purge a stale staged invite after a friend-tier auto-accept
/// for the same `space_id`, shared by every live arm that observes an accept
/// and holds store+sink. Befriend-then-redeliver: a non-friend invite is
/// staged, the user then befriends the inviter, and a later redelivery
/// auto-accepts (friend tier) — without this purge the staged entry survives
/// as a pending toast/row for a DM Space that already exists in nav.
///
/// Emits `dm-invite-list-changed` ONLY when an entry was actually removed —
/// every friend-tier DM redelivery hits the accept arms, so an unconditional
/// emit would churn the list view on the hot path. NEVER emits
/// `dm-invite-received` (nothing to prompt: the Space exists). Consumes via
/// `take()`, not `decline()`: the Space now exists, so a later redelivery is
/// admitted by the normal receive path and never re-stages — no declined-ledger
/// entry is needed (mirrors the user-accept contract on [`PendingDmInvites::take`]).
///
/// `pending == None` means the store was not wired on this path (defensive; the
/// live routes always pass `Some`) — returns silently: purge is best-effort
/// cleanup, and a path with no store has nothing staged to go stale (unlike a
/// dropped staging, which loses a user prompt and warrants the stage helper's
/// warn).
///
/// CALLER CONTRACT: the `crdt_state` async lock guard MUST already be dropped
/// before calling this — `take()` takes the store `Mutex` and this fires the
/// event sink, neither of which may nest inside the held owner-state lock.
pub fn purge_stale_staged_on_accept(
    pending: Option<&std::sync::Arc<PendingDmInvites>>,
    sink: &dyn crate::node_event_sink::NodeEventSink,
    space_id: &crate::owner_state_types::SpaceId,
) {
    let Some(pending) = pending else {
        return;
    };
    if pending.take(space_id).is_some() {
        crate::node_event_sink::emit_ser(sink, "dm-invite-list-changed", &());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a minimal StagedDmInvite around the shared test fixture
    // (dm_envelope::test_fixtures::minimal_invite_for_space — store logic
    // is gate-agnostic; the fixture's field consistency is its own concern).
    fn staged(space: u8, ms: u64) -> StagedDmInvite {
        StagedDmInvite {
            signed: crate::dm_envelope::test_fixtures::minimal_invite_for_space(space),
            received_at_ms: ms,
            refresh_owner_device_cache: true,
            // Tunnel-shaped by default (no co-deposit cid); the ledger tests
            // below set `source_cid` explicitly where the deposit path matters.
            source_cid: None,
        }
    }

    fn cid(byte: u8) -> ContentId {
        ContentId::from_bytes([byte; 32])
    }

    #[test]
    fn stage_then_list_then_take() {
        let store = PendingDmInvites::new();
        assert!(store.stage(staged(1, 100)));
        assert_eq!(store.list().len(), 1);
        let took = store.take(&store.list()[0].signed.space_id);
        assert!(took.is_some());
        assert!(store.list().is_empty());
        assert!(store.take(&took.unwrap().signed.space_id).is_none());
    }

    #[test]
    fn stage_is_idempotent_keep_first() {
        let store = PendingDmInvites::new();
        assert!(store.stage(staged(2, 100)));
        // Redelivery: same space_id, later timestamp — must be rejected and
        // must NOT bump received_at_ms.
        assert!(!store.stage(staged(2, 999)));
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].received_at_ms, 100);
    }

    #[test]
    fn decline_then_redeliver_restages_when_no_source_cid() {
        // A tunnel-origin invite (source_cid = None) records NO ledger entry on
        // decline, so a later re-send re-stages (spec: repeat invites re-prompt).
        let store = PendingDmInvites::new();
        assert!(store.stage(staged(3, 100)));
        let sid = store.list()[0].signed.space_id;
        assert!(store.decline(&sid).is_some()); // decline consumes; no cid → no ledger
        assert!(store.stage(staged(3, 200)));
        assert_eq!(store.list()[0].received_at_ms, 200);
    }

    #[test]
    fn purge_on_accept_removes_and_emits_once() {
        // ZEB-640 (1): a staged entry made stale by a friend-tier auto-accept
        // is purged, and the UI is told the list changed EXACTLY once — with
        // no re-prompt (`dm-invite-received` is for NEW pending invites only;
        // the Space now exists in nav, so there is nothing to ask).
        let store = std::sync::Arc::new(PendingDmInvites::new());
        let rec = crate::node_event_sink::RecordingSink::new();
        assert!(store.stage(staged(5, 100)));
        let sid = store.list()[0].signed.space_id;

        purge_stale_staged_on_accept(Some(&store), &rec, &sid);

        assert!(store.list().is_empty(), "purge consumes the staged entry");
        let frames = rec.frames();
        assert_eq!(
            frames
                .iter()
                .filter(|(n, _)| n == "dm-invite-list-changed")
                .count(),
            1,
            "purge emits list-changed exactly once"
        );
        assert!(
            frames.iter().all(|(n, _)| n != "dm-invite-received"),
            "purge must never re-prompt"
        );
        assert_eq!(frames.len(), 1, "no other events from the purge");
    }

    #[test]
    fn purge_on_accept_is_silent_when_nothing_staged() {
        // Hot-path noise guard: EVERY friend-tier DM redelivery hits the
        // Accepted arms, so a purge that finds nothing staged must emit
        // NOTHING (an unconditional list-changed would churn the list view
        // on every ~7.5-min sweep).
        let store = std::sync::Arc::new(PendingDmInvites::new());
        let rec = crate::node_event_sink::RecordingSink::new();
        let sid = staged(6, 100).signed.space_id;

        purge_stale_staged_on_accept(Some(&store), &rec, &sid);
        assert!(rec.frames().is_empty(), "empty store → no events");

        // The defensive None-store arm is silent too (best-effort cleanup —
        // a path with no store has nothing staged to go stale).
        purge_stale_staged_on_accept(None, &rec, &sid);
        assert!(rec.frames().is_empty(), "no store → no events");
    }

    #[test]
    fn decline_records_ledger_only_when_source_cid_present() {
        // (a) source_cid = Some → decline records the ledger → a re-stage of the
        //     SAME (space, cid) is suppressed (the sweep re-delivery kill).
        let store = PendingDmInvites::new();
        let mut with_cid = staged(7, 100);
        with_cid.source_cid = Some(cid(0x77));
        let sid = with_cid.signed.space_id;
        assert!(store.stage(with_cid.clone()));
        assert!(store.decline(&sid).is_some());
        assert!(
            !store.stage(with_cid),
            "same (space, cid) redelivery after decline must be suppressed"
        );
        assert!(store.list().is_empty(), "suppressed stage inserts nothing");

        // A DIFFERENT cid for the SAME space is a new message → re-stages.
        let mut other_cid = staged(7, 200);
        other_cid.source_cid = Some(cid(0x88));
        assert!(store.stage(other_cid));
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].received_at_ms, 200);

        // (b) source_cid = None → decline records NOTHING → re-stage allowed.
        let store2 = PendingDmInvites::new();
        let none_cid = staged(9, 100);
        let sid2 = none_cid.signed.space_id;
        assert!(store2.stage(none_cid.clone()));
        assert!(store2.decline(&sid2).is_some());
        assert!(store2.stage(none_cid), "no ledger entry → re-stage allowed");
        assert_eq!(store2.list().len(), 1);
    }
}
