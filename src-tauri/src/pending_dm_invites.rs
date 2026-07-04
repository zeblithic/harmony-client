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
use crate::owner_state_types::SpaceId;
use std::collections::HashMap;
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
}

/// Process-local store of staged DM invites, keyed by `SpaceId`. Single
/// `Mutex` held only for the duration of one map op — never across `.await`.
#[derive(Default)]
pub struct PendingDmInvites {
    inner: Mutex<HashMap<SpaceId, StagedDmInvite>>,
}

impl PendingDmInvites {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage an invite. Returns `true` if newly staged; `false` when an
    /// invite for the same `space_id` is already pending (keep-first — a
    /// ZEB-483 co-deposit redelivery must NOT bump `received_at_ms`, and the
    /// caller must NOT re-emit `dm-invite-received` for it).
    pub fn stage(&self, staged: StagedDmInvite) -> bool {
        let mut inner = self.inner.lock().expect("pending dm invites poisoned");
        match inner.entry(staged.signed.space_id) {
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
            .values()
            .cloned()
            .collect()
    }

    /// Remove + return the staged invite for `space_id` (accept and decline
    /// both consume through here; decline simply drops the return).
    pub fn take(&self, space_id: &SpaceId) -> Option<StagedDmInvite> {
        self.inner
            .lock()
            .expect("pending dm invites poisoned")
            .remove(space_id)
    }
}

/// ZEB-236 (T3): stage a verified non-friend invite into the process-local
/// store and fire the UI events, shared by every live ingest route so all
/// paths emit identically. CANONICAL semantics: emit `dm-invite-received` ONLY
/// when `stage()` reports a NEWLY-staged invite (a keep-first redelivery of an
/// already-pending invite must NOT re-prompt); ALWAYS emit
/// `dm-invite-list-changed` after a successful stage so the list view refreshes.
/// Both payloads are `&()` — the frontend re-fetches via `list_pending_dm_invites`
/// (mirrors `friend-list-changed`).
///
/// `pending == None` means the store was not wired on this path (defensive; the
/// live routes always pass `Some`) — the invite is warn-logged and dropped
/// rather than silently swallowed.
///
/// CALLER CONTRACT: the `crdt_state` async lock guard MUST already be dropped
/// before calling this — `stage()` takes the store `Mutex` and this fires the
/// event sink, neither of which may nest inside the held owner-state lock.
pub(crate) fn stage_and_emit_staged_invite(
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
    }
    crate::node_event_sink::emit_ser(sink, "dm-invite-list-changed", &());
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
        }
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
    fn decline_then_redeliver_restages() {
        let store = PendingDmInvites::new();
        assert!(store.stage(staged(3, 100)));
        let sid = store.list()[0].signed.space_id;
        store.take(&sid); // decline consumes
                          // The next ZEB-483 redelivery re-stages (spec: repeat invites re-prompt).
        assert!(store.stage(staged(3, 200)));
        assert_eq!(store.list()[0].received_at_ms, 200);
    }
}
