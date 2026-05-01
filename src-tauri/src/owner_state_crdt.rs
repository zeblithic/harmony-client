//! Owner-state CRDT merge semantics (ZEB-215 Sub-A Phase 2).
//!
//! See `docs/specs/2026-04-30-zeb-206-nav-tree-design.md`
//! §"CRDT convergence semantics".

use std::collections::{BTreeMap, BTreeSet};

use crate::owner_state_types::{
    DedupeKey, DeliveryStatus, InboxEntry, InboxKey, OutboxEntry, OutboxEntryId, OwnerAddr,
    ReadMarker, Space, SpaceId,
};

/// In-memory owner-state CRDT store. Phase 3 wraps this in persistence +
/// transport; Phase 2 owns purely the typed merge semantics.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OwnerState {
    pub spaces: BTreeMap<SpaceId, Space>,
    pub outbox: BTreeMap<OutboxEntryId, OutboxEntry>,
    pub inbox: BTreeMap<InboxKey, InboxEntry>,
    pub markers: BTreeMap<SpaceId, ReadMarker>,
    /// Permanent tombstones — explicit `remove_space` writes a SpaceId here;
    /// re-add via the normal apply path is rejected. Distinct from
    /// `Space.left_at` which is reversible.
    pub tombstones: BTreeSet<SpaceId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// New record — no existing entry matched.
    Inserted,
    /// Existing record updated. `old_id` is `Some` only when a Space dedupe
    /// merge collapsed two SpaceIds into one — caller must run dependent-
    /// record canonicalization (Task 14).
    Merged { old_id: Option<SpaceId> },
    /// Apply rejected — caller is observing a spec-mandated invariant.
    Rejected(RejectionReason),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RejectionReason {
    #[error("dedupe key collided with permanent tombstone for space {0:?}")]
    Tombstoned(SpaceId),
    #[error("space invariant violated: {0}")]
    InvariantFail(String),
    #[error("HLC not strictly newer than current {kind} (publisher {device_id:?})")]
    StaleHlc {
        kind: &'static str,
        device_id: String,
    },
}

impl OwnerState {
    /// Apply an incoming Space to the CRDT, handling per-kind dedupe,
    /// LWW field merge, ULID tie-break, and tombstone rejection.
    pub fn apply_space(&mut self, incoming: Space) -> ApplyOutcome {
        // 1. Invariant check — reject malformed Spaces before touching state.
        if let Err(e) = incoming.validate_invariants() {
            return ApplyOutcome::Rejected(RejectionReason::InvariantFail(e.0));
        }

        // 2. Tombstone check — if this Space's id is tombstoned, reject.
        //
        // Note: tombstones are stored by SpaceId. The ZEB-206 spec
        // §"Tombstones vs leaves" says re-creating a Space with the same
        // *dedupe key* should be blocked, which would also block re-adds
        // via a fresh SpaceId for non-folder kinds (e.g., a tombstoned DM
        // re-created via a different ULID with the same sorted-members).
        // That's a Phase-3 concern: it requires durable tombstone storage
        // keyed by dedupe key, which is the natural shape once the store
        // is persisted alongside the rest of owner-state. Phase 2 is
        // in-memory only, so the gap is bounded.
        if self.tombstones.contains(&incoming.id) {
            return ApplyOutcome::Rejected(RejectionReason::Tombstoned(incoming.id));
        }

        // 3. Check for same-SpaceId update first (always valid for
        //    folders/communities/channels/group-dms — their dedupe_key
        //    is derived from `id` or is `None`, so it can't change).
        //    For DMs (dedupe_key = sorted members) and PublicChannels
        //    (dedupe_key = Zenoh topic), the dedupe-key fields ARE
        //    mutable on the same SpaceId, so we must reject any merge
        //    that would change the dedupe_key — otherwise two live
        //    SpaceIds could end up sharing a dedupe_key without ever
        //    going through the cross-id collision branch (step 4) and
        //    the canonicalization rewrite would be skipped entirely.
        //    We also reject `kind` mutation outright. The dedupe_key
        //    check catches most kind changes (e.g., Folder→DM has a
        //    different DedupeKey shape) but not Channel↔GroupDm (both
        //    use DedupeKey::Id(self.id) and would slip past).
        if self.spaces.contains_key(&incoming.id) {
            let existing = self.spaces.get(&incoming.id).unwrap();
            if existing.kind != incoming.kind {
                return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                    "same-SpaceId update changes kind ({:?} → {:?}) for {:?} \
                     (kind is immutable; logical identity changes need a fresh ULID)",
                    existing.kind, incoming.kind, incoming.id
                )));
            }
            let existing_dk = existing.dedupe_key();
            let merged = lww_merge_space(existing, &incoming);
            if merged.dedupe_key() != existing_dk {
                return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                    "same-SpaceId update would change dedupe_key for {:?} \
                     (DM members or PublicChannel topic are immutable on \
                     the same SpaceId; logical identity changes need a fresh ULID)",
                    incoming.id
                )));
            }
            self.spaces.insert(incoming.id, merged);
            return ApplyOutcome::Merged { old_id: None };
        }

        // 4. Find any existing Space sharing the same dedupe key (cross-device
        //    collision). Folders use DedupeKey::None and never cross-dedupe —
        //    a folder write with a fresh SpaceId is always a new Space.
        let dk = incoming.dedupe_key();
        let existing_id = if matches!(dk, DedupeKey::None) {
            None // folders never cross-dedupe — every fresh SpaceId is a new space
        } else {
            self.spaces
                .iter()
                .find(|(_, s)| s.dedupe_key() == dk)
                .map(|(id, _)| *id)
        };

        match existing_id {
            None => {
                // No collision — insert as new.
                self.spaces.insert(incoming.id, incoming);
                ApplyOutcome::Inserted
            }
            Some(existing_id) => {
                // Different SpaceId, same dedupe key — ULID tie-break:
                // lexicographically-smaller ULID wins. Caller must run
                // dependent-record canonicalization for the loser.
                let winner_id = std::cmp::min(existing_id, incoming.id);
                let loser_id = std::cmp::max(existing_id, incoming.id);
                let existing = self.spaces.get(&existing_id).unwrap().clone();
                let mut merged = lww_merge_space(&existing, &incoming);
                merged.id = winner_id;
                // Drop the loser, install the merged winner.
                self.spaces.remove(&loser_id);
                self.spaces.insert(winner_id, merged);
                ApplyOutcome::Merged {
                    old_id: Some(loser_id),
                }
            }
        }
    }

    /// Mark a Space as permanently tombstoned. Subsequent `apply_space`
    /// calls with the same SpaceId are rejected. Distinct from
    /// `Space.left_at` (which is reversible).
    pub fn tombstone_space(&mut self, space_id: SpaceId) {
        self.spaces.remove(&space_id);
        self.tombstones.insert(space_id);
    }

    /// Apply an incoming OutboxEntry to the CRDT. Upsert by
    /// `OutboxEntryId`. On merge: `delivered_to` becomes the union of
    /// both sets and `delivery_status` recomputes from the union.
    /// OutboxEntries are NEVER GC'd in v1 — chat history.
    pub fn apply_outbox(&mut self, incoming: OutboxEntry) -> ApplyOutcome {
        // Validate that every ack in delivered_to is for an actual
        // recipient. A non-recipient ack inflates the set, has no
        // semantic meaning, and would persist on the wire — reject
        // rather than silently filter so the divergence is surfaced.
        let recipient_set: BTreeSet<&OwnerAddr> = incoming.recipient_owners.iter().collect();
        if !incoming
            .delivered_to
            .iter()
            .all(|o| recipient_set.contains(o))
        {
            return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                "OutboxEntry {:?} has delivered_to entry not in recipient_owners",
                incoming.id
            )));
        }

        match self.outbox.get(&incoming.id) {
            None => {
                let mut entry = incoming;
                // Re-derive status from delivered_to via ack-driven
                // transitions, but preserve Expired if the incoming
                // entry is already marked Expired (Phase 3 stamps the
                // 30-day wall-clock expiry and that decision must
                // survive replication — re-deriving from acks alone
                // would never produce Expired and would silently
                // downgrade it to Pending/Partial/Complete).
                let is_expired = matches!(entry.delivery_status, DeliveryStatus::Expired);
                entry.delivery_status = entry.compute_status(is_expired);
                self.outbox.insert(entry.id, entry);
                ApplyOutcome::Inserted
            }
            Some(existing) => {
                // Envelope immutability: same OutboxEntryId means same
                // logical message, so space_id/message_cid/recipient_owners/
                // created_at MUST match. A divergence implies ULID
                // collision, replay attack, or buggy peer — reject
                // rather than silently overwriting (existing wins) so
                // the operator sees the divergence. The incoming
                // delivered_to set could legitimately differ; that's
                // what we union below.
                if existing.space_id != incoming.space_id
                    || existing.message_cid != incoming.message_cid
                    || existing.recipient_owners != incoming.recipient_owners
                    || existing.created_at != incoming.created_at
                {
                    return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                        "OutboxEntry {:?} envelope mismatch (space_id/message_cid/\
                         recipient_owners/created_at must be immutable across \
                         merges; same OutboxEntryId implies same logical message)",
                        incoming.id
                    )));
                }
                let mut merged = existing.clone();
                merged
                    .delivered_to
                    .extend(incoming.delivered_to.iter().copied());
                // Expired is sticky across both sides of a merge —
                // either replica observing expiry seals the entry, so
                // a stale ack arriving later cannot un-expire it.
                let is_expired = matches!(existing.delivery_status, DeliveryStatus::Expired)
                    || matches!(incoming.delivery_status, DeliveryStatus::Expired);
                merged.delivery_status = merged.compute_status(is_expired);
                self.outbox.insert(incoming.id, merged);
                ApplyOutcome::Merged { old_id: None }
            }
        }
    }

    /// Apply an incoming InboxEntry to the CRDT. Upsert by composite key
    /// `(space_id, message_cid)` per ZEB-206 §Idempotency. On collision,
    /// keep the earliest `received_at` (matches the spec: "first device's
    /// receive time").
    pub fn apply_inbox(&mut self, incoming: InboxEntry) -> ApplyOutcome {
        let key = incoming.key();
        match self.inbox.get(&key) {
            None => {
                self.inbox.insert(key, incoming);
                ApplyOutcome::Inserted
            }
            Some(existing) => {
                let earlier = if existing
                    .received_at
                    .is_strictly_newer_than(&incoming.received_at)
                {
                    incoming
                } else {
                    existing.clone()
                };
                self.inbox.insert(key, earlier);
                ApplyOutcome::Merged { old_id: None }
            }
        }
    }

    /// `apply_space` followed by atomic rewrite of all dependent records
    /// (OutboxEntry/InboxEntry/ReadMarker `space_id`) when the merge
    /// collapses two SpaceIds into one. This is the public entry point
    /// for incoming Spaces — Phase 3 (sync) calls this; Phase 2 internal
    /// tests can call `apply_space` directly when they don't have
    /// dependent records to worry about.
    ///
    /// Implementation note: we capture the would-be winner id BEFORE
    /// `apply_space` mutates state (which removes the loser entry).
    /// Without this snapshot, we couldn't recover the winner from the
    /// post-merge map alone.
    pub fn apply_space_with_canonicalization(&mut self, incoming: Space) -> ApplyOutcome {
        let dk = incoming.dedupe_key();
        let predicted_winner = if matches!(dk, DedupeKey::None) {
            None
        } else {
            self.spaces
                .iter()
                .find(|(_, s)| s.dedupe_key() == dk)
                .map(|(id, _)| std::cmp::min(*id, incoming.id))
        };
        let outcome = self.apply_space(incoming);
        if let ApplyOutcome::Merged {
            old_id: Some(loser),
        } = &outcome
        {
            if let Some(winner) = predicted_winner {
                self.canonicalize_dependent_space_ids(*loser, winner);
            }
        }
        outcome
    }

    /// Atomic rewrite of every dependent record's `space_id` from `loser`
    /// to `winner`. Touches outbox (mutate space_id field in place),
    /// inbox (rebuild composite map key), markers (rebuild map key).
    fn canonicalize_dependent_space_ids(&mut self, loser: SpaceId, winner: SpaceId) {
        // OutboxEntry — mutate in place; the map key (OutboxEntryId) is
        // independent of space_id.
        for entry in self.outbox.values_mut() {
            if entry.space_id == loser {
                entry.space_id = winner;
            }
        }

        // InboxEntry — composite key (space_id, message_cid) is the BTreeMap
        // key, so rewriting space_id requires rebuilding the entry under
        // the new key. If the rewrite collides with an existing
        // (winner, message_cid) entry, delegate to `apply_inbox` so the
        // earliest-received_at merge rule applies (rather than blindly
        // overwriting).
        let mut rewritten: Vec<InboxEntry> = Vec::new();
        let mut keys_to_remove: Vec<InboxKey> = Vec::new();
        for (k, v) in &self.inbox {
            if k.space_id == loser {
                let mut new_entry = v.clone();
                new_entry.space_id = winner;
                rewritten.push(new_entry);
                keys_to_remove.push(*k);
            }
        }
        for k in keys_to_remove {
            self.inbox.remove(&k);
        }
        for entry in rewritten {
            // apply_inbox handles the (winner, message_cid) collision case
            // by keeping the earliest received_at; outcome is discarded
            // because canonicalization is a state rewrite, not a new apply.
            let _ = self.apply_inbox(entry);
        }

        // ReadMarker — keyed by space_id; rewrite map key. If the winner
        // already has a marker, delegate to `apply_marker` so the
        // monotone-advance rule applies (older HLC rejected, never
        // regresses read progress).
        if let Some(mut marker) = self.markers.remove(&loser) {
            marker.space_id = winner;
            let _ = self.apply_marker(marker);
        }
    }

    /// Apply an incoming ReadMarker. `last_read_at` advances monotonically —
    /// strictly-older HLCs are rejected so reading state never regresses.
    /// An identical HLC is treated as an idempotent replay (succeed
    /// without mutation) — sync flows replay the same marker freely
    /// and `Rejected` should be reserved for genuine conflicts.
    pub fn apply_marker(&mut self, incoming: ReadMarker) -> ApplyOutcome {
        match self.markers.get(&incoming.space_id) {
            None => {
                self.markers.insert(incoming.space_id, incoming);
                ApplyOutcome::Inserted
            }
            Some(existing) => {
                if incoming
                    .last_read_at
                    .is_strictly_newer_than(&existing.last_read_at)
                {
                    self.markers.insert(incoming.space_id, incoming);
                    ApplyOutcome::Merged { old_id: None }
                } else if existing
                    .last_read_at
                    .is_strictly_newer_than(&incoming.last_read_at)
                {
                    ApplyOutcome::Rejected(RejectionReason::StaleHlc {
                        kind: "ReadMarker",
                        device_id: incoming.last_read_at.device_id.clone(),
                    })
                } else {
                    // Equal HLCs — same logical write, idempotent replay.
                    ApplyOutcome::Merged { old_id: None }
                }
            }
        }
    }
}

/// Merge two Space values using last-writer-wins per-field on
/// `updated_at` HLC. `created_at` always takes the earlier HLC.
/// Caller is responsible for setting the merged Space's `id` correctly
/// (the dedupe-key-based caller already chose the winning ULID).
///
/// Equal-timestamp tie-break: when `a.updated_at == b.updated_at`,
/// `is_strictly_newer_than` returns false and we keep `a` (the existing
/// record). This is a "keep local" bias which is stable and safe — the
/// HLC's logical+device_id components mean exact-equality is rare in
/// practice (two devices would need identical wall_ms AND identical
/// logical AND identical device_id, which collapses to "the same write").
fn lww_merge_space(a: &Space, b: &Space) -> Space {
    let newer = if b.updated_at.is_strictly_newer_than(&a.updated_at) {
        b
    } else {
        a
    };
    Space {
        id: newer.id,
        kind: newer.kind, // kind shouldn't change in practice; LWW for safety
        parent: newer.parent,
        community_id: newer.community_id,
        name: newer.name.clone(),
        transport: newer.transport.clone(),
        members: newer.members.clone(),
        custom_name: newer.custom_name.clone(),
        notification_pref: newer.notification_pref,
        // left_at is also LWW — newer overrides (re-invitation clears to None).
        left_at: newer.left_at.clone(),
        // created_at is monotonically the earliest.
        created_at: if a.created_at.is_strictly_newer_than(&b.created_at) {
            b.created_at.clone()
        } else {
            a.created_at.clone()
        },
        updated_at: newer.updated_at.clone(),
    }
}

#[cfg(test)]
mod apply_space_tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceKind, TransportBinding};

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn folder(id: u8, ts: u64) -> Space {
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "F".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(ts),
            updated_at: hlc(ts),
        }
    }

    fn dm(id: u8, members: Vec<u8>, ts: u64) -> Space {
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: members.into_iter().map(|i| OwnerAddr([i; 16])).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(ts),
            updated_at: hlc(ts),
        }
    }

    #[test]
    fn folders_never_dedupe() {
        let mut s = OwnerState::default();
        assert_eq!(s.apply_space(folder(1, 100)), ApplyOutcome::Inserted);
        assert_eq!(s.apply_space(folder(2, 200)), ApplyOutcome::Inserted);
        // Two distinct folders despite identical name.
        assert_eq!(s.spaces.len(), 2);
    }

    #[test]
    fn dm_dedupes_by_sorted_members_regardless_of_id() {
        let mut s = OwnerState::default();
        // Device A creates DM with id=1, members=[alice, bob].
        let outcome_a = s.apply_space(dm(1, vec![1, 2], 100));
        assert_eq!(outcome_a, ApplyOutcome::Inserted);
        // Device B independently creates the same DM with id=2 — same
        // sorted members (the validate_invariants sorted-ascending
        // rule means both devices necessarily construct identical
        // member orderings, so dedupe always converges via the
        // SortedMembers key + ULID tie-break, never via member-order
        // reconciliation).
        let outcome_b = s.apply_space(dm(2, vec![1, 2], 100));
        match outcome_b {
            ApplyOutcome::Merged {
                old_id: Some(loser),
            } => {
                assert_eq!(loser, SpaceId([2; 16]), "loser should be the larger ULID");
            }
            other => panic!("expected Merged with loser id=2, got {:?}", other),
        }
        assert_eq!(s.spaces.len(), 1);
        assert!(s.spaces.contains_key(&SpaceId([1; 16])));
    }

    #[test]
    fn lww_merge_takes_newer_field() {
        let mut s = OwnerState::default();
        let mut f1 = folder(5, 100);
        f1.custom_name = Some("first".into());
        s.apply_space(f1);

        let mut f2 = folder(5, 200);
        f2.custom_name = Some("second".into());
        let outcome = s.apply_space(f2);
        assert_eq!(outcome, ApplyOutcome::Merged { old_id: None });
        assert_eq!(
            s.spaces.get(&SpaceId([5; 16])).unwrap().custom_name,
            Some("second".into())
        );
    }

    #[test]
    fn created_at_is_monotonically_earliest() {
        let mut s = OwnerState::default();
        s.apply_space(folder(7, 200));
        s.apply_space(folder(7, 100));
        assert_eq!(
            s.spaces.get(&SpaceId([7; 16])).unwrap().created_at.wall_ms,
            100
        );
    }

    #[test]
    fn tombstone_blocks_re_add() {
        let mut s = OwnerState::default();
        s.apply_space(folder(9, 100));
        s.tombstone_space(SpaceId([9; 16]));
        let outcome = s.apply_space(folder(9, 200));
        assert!(matches!(
            outcome,
            ApplyOutcome::Rejected(RejectionReason::Tombstoned(_))
        ));
        assert!(!s.spaces.contains_key(&SpaceId([9; 16])));
    }

    #[test]
    fn invariant_failure_rejected() {
        let mut s = OwnerState::default();
        let mut bad_dm = dm(1, vec![1], 100); // 1 member — invalid for dm
        bad_dm.kind = SpaceKind::Dm;
        let outcome = s.apply_space(bad_dm);
        assert!(matches!(
            outcome,
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(_))
        ));
    }

    #[test]
    fn left_at_is_lww_reversible() {
        let mut s = OwnerState::default();
        // First write: left_at = None.
        let mut d1 = dm(1, vec![1, 2], 100);
        s.apply_space(d1.clone());
        // Newer write sets left_at — Space marked as left.
        d1.left_at = Some(hlc(200));
        d1.updated_at = hlc(200);
        s.apply_space(d1.clone());
        assert!(s.spaces.get(&SpaceId([1; 16])).unwrap().left_at.is_some());
        // Even-newer write clears left_at — space "rejoined" via re-invite.
        d1.left_at = None;
        d1.updated_at = hlc(300);
        s.apply_space(d1);
        assert!(s.spaces.get(&SpaceId([1; 16])).unwrap().left_at.is_none());
    }

    /// Regression for PR #73 round 2 review: a same-SpaceId DM update
    /// that swaps members would change `dedupe_key()` and could create
    /// two live SpaceIds sharing a sorted-members key without ever
    /// going through the cross-id collision branch. Reject up front.
    #[test]
    fn same_id_dm_member_swap_rejects() {
        let mut s = OwnerState::default();
        s.apply_space(dm(1, vec![1, 2], 100));
        let outcome = s.apply_space(dm(1, vec![3, 4], 200));
        assert!(
            matches!(
                outcome,
                ApplyOutcome::Rejected(RejectionReason::InvariantFail(_))
            ),
            "expected InvariantFail, got {:?}",
            outcome
        );
        let stored = s.spaces.get(&SpaceId([1; 16])).unwrap();
        assert_eq!(stored.members.len(), 2);
        assert_eq!(stored.members[0], OwnerAddr([1; 16]));
        assert_eq!(stored.members[1], OwnerAddr([2; 16]));
    }

    /// Regression for PR #73 Greptile P2: kind mutation on the same
    /// SpaceId must be rejected. The dedupe_key check catches most
    /// kind changes, but Channel↔GroupDm both use `DedupeKey::Id(id)`
    /// and would slip through dedupe_key equality alone.
    #[test]
    fn same_id_kind_change_rejects() {
        let mut s = OwnerState::default();
        // Seed a Channel.
        let channel = Space {
            id: SpaceId([7; 16]),
            kind: SpaceKind::Channel,
            parent: None,
            community_id: Some(SpaceId([8; 16])),
            name: "general".into(),
            transport: Some(TransportBinding::Zenoh {
                topic: "harmony/community/general".into(),
            }),
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(100),
            updated_at: hlc(100),
        };
        assert_eq!(s.apply_space(channel), ApplyOutcome::Inserted);
        // Same SpaceId, kind swapped to GroupDm — dedupe_key still
        // Id([7;16]) on both sides, so the dedupe_key check would
        // not catch this. Explicit kind check must reject it.
        let group_dm = Space {
            id: SpaceId([7; 16]),
            kind: SpaceKind::GroupDm,
            parent: None,
            community_id: None,
            name: "Hijacked".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16]), OwnerAddr([3; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(200),
            updated_at: hlc(200),
        };
        let outcome = s.apply_space(group_dm);
        assert!(
            matches!(
                outcome,
                ApplyOutcome::Rejected(RejectionReason::InvariantFail(_))
            ),
            "expected InvariantFail on kind change, got {:?}",
            outcome
        );
        // Stored Space must remain a Channel.
        assert_eq!(
            s.spaces.get(&SpaceId([7; 16])).unwrap().kind,
            SpaceKind::Channel
        );
    }

    /// Same-SpaceId update that does NOT touch dedupe-key fields
    /// (e.g., custom_name on a DM) must still pass.
    #[test]
    fn same_id_dm_non_dedupe_field_update_succeeds() {
        let mut s = OwnerState::default();
        s.apply_space(dm(1, vec![1, 2], 100));
        let mut updated = dm(1, vec![1, 2], 200);
        updated.custom_name = Some("renamed".into());
        let outcome = s.apply_space(updated);
        assert_eq!(outcome, ApplyOutcome::Merged { old_id: None });
        assert_eq!(
            s.spaces.get(&SpaceId([1; 16])).unwrap().custom_name,
            Some("renamed".into())
        );
    }
}

#[cfg(test)]
mod apply_outbox_tests {
    use super::*;
    use crate::owner_state_types::{
        ContentId, DeliveryStatus, Hlc, OutboxEntry, OutboxEntryId, OwnerAddr,
    };

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn entry(id: u8, recipients: Vec<u8>, delivered: Vec<u8>) -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([id; 16]),
            space_id: SpaceId([1; 16]),
            recipient_owners: recipients.into_iter().map(|i| OwnerAddr([i; 16])).collect(),
            message_cid: ContentId([2; 32]),
            created_at: hlc(100),
            delivered_to: delivered.into_iter().map(|i| OwnerAddr([i; 16])).collect(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    #[test]
    fn first_write_inserts() {
        let mut s = OwnerState::default();
        let outcome = s.apply_outbox(entry(1, vec![10, 20], vec![]));
        assert_eq!(outcome, ApplyOutcome::Inserted);
        assert_eq!(s.outbox.len(), 1);
    }

    #[test]
    fn merge_unions_delivered_to() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20, 30], vec![10]));
        let outcome = s.apply_outbox(entry(1, vec![10, 20, 30], vec![20]));
        assert_eq!(outcome, ApplyOutcome::Merged { old_id: None });
        let merged = s.outbox.get(&OutboxEntryId([1; 16])).unwrap();
        assert_eq!(merged.delivered_to.len(), 2);
        assert!(merged.delivered_to.contains(&OwnerAddr([10; 16])));
        assert!(merged.delivered_to.contains(&OwnerAddr([20; 16])));
    }

    #[test]
    fn delivery_status_recomputes_to_complete_on_full_ack() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20], vec![10]));
        s.apply_outbox(entry(1, vec![10, 20], vec![20]));
        assert_eq!(
            s.outbox
                .get(&OutboxEntryId([1; 16]))
                .unwrap()
                .delivery_status,
            DeliveryStatus::Complete
        );
    }

    #[test]
    fn delivery_status_recomputes_to_partial_when_some_acked() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20, 30], vec![10]));
        assert_eq!(
            s.outbox
                .get(&OutboxEntryId([1; 16]))
                .unwrap()
                .delivery_status,
            DeliveryStatus::Partial
        );
    }

    /// Regression for PR #73 review: an Expired entry replicating in
    /// from another device must NOT be downgraded back to Pending/
    /// Partial/Complete just because we re-derive status from acks
    /// alone. Phase 3 stamps Expired via the wall-clock 30-day timer
    /// and that decision must survive replication.
    #[test]
    fn insert_preserves_expired_status() {
        let mut s = OwnerState::default();
        let mut e = entry(1, vec![10, 20, 30], vec![10]);
        e.delivery_status = DeliveryStatus::Expired;
        s.apply_outbox(e);
        assert_eq!(
            s.outbox
                .get(&OutboxEntryId([1; 16]))
                .unwrap()
                .delivery_status,
            DeliveryStatus::Expired
        );
    }

    /// Expired propagates across merges: if either side observed expiry
    /// and the merged delivered_to still doesn't cover all recipients,
    /// the entry stays Expired (would otherwise downgrade to Partial
    /// because compute_status(false) can't see the wall-clock decision).
    /// Note: if a merge happens to fill in ALL acks, compute_status
    /// short-circuits to Complete regardless of is_expired — that's
    /// intentional spec behavior (delivery did complete, just late).
    #[test]
    fn merge_preserves_expired_when_existing_expired_and_not_all_acked() {
        let mut s = OwnerState::default();
        // 3 recipients, only 1 acked — Expired and incomplete.
        let mut existing = entry(1, vec![10, 20, 30], vec![10]);
        existing.delivery_status = DeliveryStatus::Expired;
        s.apply_outbox(existing);
        // Late ack arrives for one more recipient (still not all 3).
        s.apply_outbox(entry(1, vec![10, 20, 30], vec![20]));
        assert_eq!(
            s.outbox
                .get(&OutboxEntryId([1; 16]))
                .unwrap()
                .delivery_status,
            DeliveryStatus::Expired
        );
    }

    #[test]
    fn merge_preserves_expired_when_incoming_expired_and_not_all_acked() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20, 30], vec![10]));
        let mut incoming = entry(1, vec![10, 20, 30], vec![20]);
        incoming.delivery_status = DeliveryStatus::Expired;
        s.apply_outbox(incoming);
        assert_eq!(
            s.outbox
                .get(&OutboxEntryId([1; 16]))
                .unwrap()
                .delivery_status,
            DeliveryStatus::Expired
        );
    }

    /// Spec-defined: a merge that fills every recipient ack DOES upgrade
    /// to Complete even if one side was Expired. Late delivery > expiry.
    #[test]
    fn merge_full_acks_overrides_expired() {
        let mut s = OwnerState::default();
        let mut existing = entry(1, vec![10, 20], vec![10]);
        existing.delivery_status = DeliveryStatus::Expired;
        s.apply_outbox(existing);
        s.apply_outbox(entry(1, vec![10, 20], vec![20]));
        assert_eq!(
            s.outbox
                .get(&OutboxEntryId([1; 16]))
                .unwrap()
                .delivery_status,
            DeliveryStatus::Complete
        );
    }

    #[test]
    fn distinct_outbox_ids_dont_collide() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10], vec![]));
        s.apply_outbox(entry(2, vec![20], vec![]));
        assert_eq!(s.outbox.len(), 2);
    }

    /// Regression for PR #73 round 4 review: an OutboxEntry whose
    /// `delivered_to` set contains an owner not in `recipient_owners`
    /// is malformed — a non-recipient ack inflates state with
    /// meaningless data. Reject at insert time.
    #[test]
    fn insert_rejects_delivered_to_with_non_recipient() {
        let mut s = OwnerState::default();
        // Recipients: [10, 20]; delivered_to includes 99 — not a recipient.
        let outcome = s.apply_outbox(entry(1, vec![10, 20], vec![10, 99]));
        assert!(
            matches!(
                outcome,
                ApplyOutcome::Rejected(RejectionReason::InvariantFail(_))
            ),
            "expected InvariantFail, got {:?}",
            outcome
        );
        assert!(s.outbox.is_empty());
    }

    /// Same rule applies on the merge path — incoming.delivered_to
    /// must also be a subset of recipient_owners.
    #[test]
    fn merge_rejects_delivered_to_with_non_recipient() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20], vec![10]));
        // Incoming has the same envelope but a stray 99 in delivered_to.
        let outcome = s.apply_outbox(entry(1, vec![10, 20], vec![99]));
        assert!(
            matches!(
                outcome,
                ApplyOutcome::Rejected(RejectionReason::InvariantFail(_))
            ),
            "expected InvariantFail, got {:?}",
            outcome
        );
        // Existing entry must be unchanged (no stray ack added).
        let stored = s.outbox.get(&OutboxEntryId([1; 16])).unwrap();
        assert_eq!(stored.delivered_to.len(), 1);
        assert!(stored.delivered_to.contains(&OwnerAddr([10; 16])));
    }

    /// Envelope immutability: same OutboxEntryId must mean same
    /// logical message. A merge with diverging space_id implies
    /// ULID collision, replay attack, or a buggy peer — reject.
    #[test]
    fn merge_rejects_space_id_divergence() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20], vec![10]));
        let mut diverged = entry(1, vec![10, 20], vec![20]);
        diverged.space_id = SpaceId([99; 16]); // different space
        let outcome = s.apply_outbox(diverged);
        assert!(
            matches!(
                outcome,
                ApplyOutcome::Rejected(RejectionReason::InvariantFail(_))
            ),
            "expected InvariantFail, got {:?}",
            outcome
        );
        // Existing entry must remain on its original space_id.
        assert_eq!(
            s.outbox.get(&OutboxEntryId([1; 16])).unwrap().space_id,
            SpaceId([1; 16])
        );
    }

    #[test]
    fn merge_rejects_message_cid_divergence() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20], vec![10]));
        let mut diverged = entry(1, vec![10, 20], vec![20]);
        diverged.message_cid = ContentId([99; 32]);
        let outcome = s.apply_outbox(diverged);
        assert!(matches!(
            outcome,
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(_))
        ));
    }

    #[test]
    fn merge_rejects_recipient_owners_divergence() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20], vec![10]));
        // Same id but a different recipient set — implies ULID collision.
        let outcome = s.apply_outbox(entry(1, vec![10, 30], vec![10]));
        assert!(matches!(
            outcome,
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(_))
        ));
    }

    #[test]
    fn merge_rejects_created_at_divergence() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20], vec![10]));
        let mut diverged = entry(1, vec![10, 20], vec![20]);
        diverged.created_at = hlc(999);
        let outcome = s.apply_outbox(diverged);
        assert!(matches!(
            outcome,
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(_))
        ));
    }
}

#[cfg(test)]
mod apply_inbox_tests {
    use super::*;
    use crate::owner_state_types::{ContentId, Hlc, InboxEntry, InboxKey, OwnerAddr};

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn entry(space: u8, msg: u8, from: u8, ts: u64) -> InboxEntry {
        InboxEntry {
            space_id: SpaceId([space; 16]),
            message_cid: ContentId([msg; 32]),
            from: OwnerAddr([from; 16]),
            received_at: hlc(ts),
        }
    }

    #[test]
    fn first_write_inserts() {
        let mut s = OwnerState::default();
        let outcome = s.apply_inbox(entry(1, 2, 3, 100));
        assert_eq!(outcome, ApplyOutcome::Inserted);
        assert_eq!(s.inbox.len(), 1);
    }

    #[test]
    fn duplicate_upserts_to_earliest_received_at() {
        let mut s = OwnerState::default();
        s.apply_inbox(entry(1, 2, 3, 200)); // device A
        let outcome = s.apply_inbox(entry(1, 2, 3, 100)); // device B (earlier)
        assert_eq!(outcome, ApplyOutcome::Merged { old_id: None });
        // Earliest wins.
        let key = InboxKey {
            space_id: SpaceId([1; 16]),
            message_cid: ContentId([2; 32]),
        };
        assert_eq!(s.inbox.get(&key).unwrap().received_at.wall_ms, 100);
    }

    #[test]
    fn different_messages_in_same_space_dont_collide() {
        let mut s = OwnerState::default();
        s.apply_inbox(entry(1, 2, 3, 100));
        s.apply_inbox(entry(1, 99, 3, 100));
        assert_eq!(s.inbox.len(), 2);
    }

    #[test]
    fn same_message_in_different_spaces_dont_collide() {
        // Pathological edge case: same message_cid would only happen if the
        // same encrypted blob was sent in two spaces — but we treat them as
        // distinct InboxEntries because the composite key differs.
        let mut s = OwnerState::default();
        s.apply_inbox(entry(1, 2, 3, 100));
        s.apply_inbox(entry(99, 2, 3, 100));
        assert_eq!(s.inbox.len(), 2);
    }
}

#[cfg(test)]
mod apply_marker_tests {
    use super::*;
    use crate::owner_state_types::{Hlc, ReadMarker};

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn marker(space: u8, ts: u64) -> ReadMarker {
        ReadMarker {
            space_id: SpaceId([space; 16]),
            last_read_at: hlc(ts),
        }
    }

    #[test]
    fn first_write_inserts() {
        let mut s = OwnerState::default();
        assert_eq!(s.apply_marker(marker(1, 100)), ApplyOutcome::Inserted);
    }

    #[test]
    fn newer_marker_advances() {
        let mut s = OwnerState::default();
        s.apply_marker(marker(1, 100));
        s.apply_marker(marker(1, 200));
        assert_eq!(
            s.markers
                .get(&SpaceId([1; 16]))
                .unwrap()
                .last_read_at
                .wall_ms,
            200
        );
    }

    #[test]
    fn older_marker_does_not_regress() {
        let mut s = OwnerState::default();
        s.apply_marker(marker(1, 200));
        let outcome = s.apply_marker(marker(1, 100));
        assert!(matches!(
            outcome,
            ApplyOutcome::Rejected(RejectionReason::StaleHlc { .. })
        ));
        assert_eq!(
            s.markers
                .get(&SpaceId([1; 16]))
                .unwrap()
                .last_read_at
                .wall_ms,
            200
        );
    }

    #[test]
    fn distinct_spaces_dont_interfere() {
        let mut s = OwnerState::default();
        s.apply_marker(marker(1, 100));
        s.apply_marker(marker(2, 50));
        assert_eq!(s.markers.len(), 2);
    }

    /// Regression for PR #73 round 2 review: an exact-duplicate marker
    /// (same HLC) replays as a successful idempotent op, not as a
    /// stale-write rejection. Sync flows replay the same marker freely;
    /// `Rejected` is reserved for genuine conflict (strictly older).
    #[test]
    fn equal_hlc_marker_is_idempotent_replay() {
        let mut s = OwnerState::default();
        s.apply_marker(marker(1, 100));
        let outcome = s.apply_marker(marker(1, 100));
        assert_eq!(outcome, ApplyOutcome::Merged { old_id: None });
        // Stored marker must be unchanged (same HLC, no mutation).
        assert_eq!(
            s.markers
                .get(&SpaceId([1; 16]))
                .unwrap()
                .last_read_at
                .wall_ms,
            100
        );
    }
}

#[cfg(test)]
mod canonicalization_tests {
    use super::*;
    use crate::owner_state_types::{
        ContentId, DeliveryStatus, Hlc, InboxEntry, OutboxEntry, OutboxEntryId, OwnerAddr,
        ReadMarker, SpaceKind, TransportBinding,
    };

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn dm(id: u8, members: Vec<u8>, ts: u64) -> Space {
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: members.into_iter().map(|i| OwnerAddr([i; 16])).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(ts),
            updated_at: hlc(ts),
        }
    }

    #[test]
    fn dedupe_rewrites_outbox_inbox_marker_space_ids() {
        let mut s = OwnerState::default();
        // Device A creates DM with id=5 (will be the loser — larger ULID).
        s.apply_space_with_canonicalization(dm(5, vec![1, 2], 100));
        // Insert an OutboxEntry, InboxEntry, ReadMarker pointing at id=5.
        s.apply_outbox(OutboxEntry {
            id: OutboxEntryId([100; 16]),
            space_id: SpaceId([5; 16]),
            recipient_owners: vec![OwnerAddr([2; 16])],
            message_cid: ContentId([1; 32]),
            created_at: hlc(100),
            delivered_to: Default::default(),
            delivery_status: DeliveryStatus::Pending,
        });
        s.apply_inbox(InboxEntry {
            space_id: SpaceId([5; 16]),
            message_cid: ContentId([2; 32]),
            from: OwnerAddr([2; 16]),
            received_at: hlc(100),
        });
        s.apply_marker(ReadMarker {
            space_id: SpaceId([5; 16]),
            last_read_at: hlc(100),
        });
        // Now device B's write with id=1 (smaller ULID — winner).
        let outcome = s.apply_space_with_canonicalization(dm(1, vec![1, 2], 100));
        assert_eq!(
            outcome,
            ApplyOutcome::Merged {
                old_id: Some(SpaceId([5; 16]))
            }
        );

        // All three dependent records should now reference id=1, not id=5.
        let outbox_entry = s.outbox.get(&OutboxEntryId([100; 16])).unwrap();
        assert_eq!(outbox_entry.space_id, SpaceId([1; 16]));

        // InboxEntry's composite key includes space_id, so the BTreeMap key
        // itself rewrites — old key is gone, new key present.
        let new_inbox_key = InboxKey {
            space_id: SpaceId([1; 16]),
            message_cid: ContentId([2; 32]),
        };
        let old_inbox_key = InboxKey {
            space_id: SpaceId([5; 16]),
            message_cid: ContentId([2; 32]),
        };
        assert!(s.inbox.contains_key(&new_inbox_key));
        assert!(!s.inbox.contains_key(&old_inbox_key));

        // ReadMarker keyed by space_id — same rewrite.
        assert!(s.markers.contains_key(&SpaceId([1; 16])));
        assert!(!s.markers.contains_key(&SpaceId([5; 16])));
    }

    #[test]
    fn no_dedupe_no_rewrite() {
        let mut s = OwnerState::default();
        s.apply_space_with_canonicalization(dm(1, vec![1, 2], 100));
        // Fresh outbox/inbox/marker untouched by a non-dedupe-merge apply.
        s.apply_outbox(OutboxEntry {
            id: OutboxEntryId([99; 16]),
            space_id: SpaceId([1; 16]),
            recipient_owners: vec![OwnerAddr([2; 16])],
            message_cid: ContentId([1; 32]),
            created_at: hlc(100),
            delivered_to: Default::default(),
            delivery_status: DeliveryStatus::Pending,
        });
        // Same dm, same id — pure LWW, no canonicalization triggered.
        s.apply_space_with_canonicalization(dm(1, vec![1, 2], 200));
        let entry = s.outbox.get(&OutboxEntryId([99; 16])).unwrap();
        assert_eq!(entry.space_id, SpaceId([1; 16]));
    }

    /// Regression for PR #73 review: when both loser and winner have an
    /// inbox entry for the same message_cid, the rewrite must NOT
    /// overwrite the winner's entry. Earliest received_at must win
    /// (matching apply_inbox's collision rule).
    #[test]
    fn dedupe_inbox_collision_keeps_earliest_received_at() {
        let mut s = OwnerState::default();
        // Both devices create the same DM independently and each
        // receives the same message; the loser's entry has received_at
        // = 200 (later), the winner's = 100 (earlier).
        s.apply_space_with_canonicalization(dm(5, vec![1, 2], 100));
        s.apply_space_with_canonicalization(dm(1, vec![1, 2], 100));
        // Re-create the loser space so its inbox slot exists; tombstone-
        // path doesn't matter here, we're testing the rewrite directly.
        // To set up a true collision we manually populate both keys,
        // then call canonicalization through a no-op apply that triggers
        // the merge path. Instead, directly seed both inbox keys and
        // exercise canonicalize_dependent_space_ids.
        s.inbox.insert(
            InboxKey {
                space_id: SpaceId([5; 16]),
                message_cid: ContentId([7; 32]),
            },
            InboxEntry {
                space_id: SpaceId([5; 16]),
                message_cid: ContentId([7; 32]),
                from: OwnerAddr([2; 16]),
                received_at: hlc(200), // later
            },
        );
        s.inbox.insert(
            InboxKey {
                space_id: SpaceId([1; 16]),
                message_cid: ContentId([7; 32]),
            },
            InboxEntry {
                space_id: SpaceId([1; 16]),
                message_cid: ContentId([7; 32]),
                from: OwnerAddr([2; 16]),
                received_at: hlc(100), // earlier — should win
            },
        );

        s.canonicalize_dependent_space_ids(SpaceId([5; 16]), SpaceId([1; 16]));

        // Old loser key gone; only the winner key remains.
        assert!(!s.inbox.contains_key(&InboxKey {
            space_id: SpaceId([5; 16]),
            message_cid: ContentId([7; 32]),
        }));
        let winner_entry = s
            .inbox
            .get(&InboxKey {
                space_id: SpaceId([1; 16]),
                message_cid: ContentId([7; 32]),
            })
            .unwrap();
        // Earlier (winner-side) received_at wins, NOT loser's later 200.
        assert_eq!(winner_entry.received_at.wall_ms, 100);
    }

    /// Regression for PR #73 review: when both loser and winner have a
    /// ReadMarker, the rewrite must NOT regress the winner's read
    /// progress. The newer last_read_at must win (matching
    /// apply_marker's monotone-advance rule).
    #[test]
    fn dedupe_marker_collision_keeps_newer_last_read_at() {
        let mut s = OwnerState::default();
        // Set up: winner already has a newer marker (300); loser has
        // an older one (100). Rewrite must NOT regress to 100.
        s.markers.insert(
            SpaceId([5; 16]),
            ReadMarker {
                space_id: SpaceId([5; 16]),
                last_read_at: hlc(100),
            },
        );
        s.markers.insert(
            SpaceId([1; 16]),
            ReadMarker {
                space_id: SpaceId([1; 16]),
                last_read_at: hlc(300), // newer — must win
            },
        );

        s.canonicalize_dependent_space_ids(SpaceId([5; 16]), SpaceId([1; 16]));

        assert!(!s.markers.contains_key(&SpaceId([5; 16])));
        let winner_marker = s.markers.get(&SpaceId([1; 16])).unwrap();
        assert_eq!(winner_marker.last_read_at.wall_ms, 300);
    }

    /// Inverse case: when the loser's marker is newer, it should
    /// advance the winner's marker (still monotone, just promoted).
    #[test]
    fn dedupe_marker_collision_loser_newer_advances_winner() {
        let mut s = OwnerState::default();
        s.markers.insert(
            SpaceId([5; 16]),
            ReadMarker {
                space_id: SpaceId([5; 16]),
                last_read_at: hlc(500), // newer
            },
        );
        s.markers.insert(
            SpaceId([1; 16]),
            ReadMarker {
                space_id: SpaceId([1; 16]),
                last_read_at: hlc(100),
            },
        );

        s.canonicalize_dependent_space_ids(SpaceId([5; 16]), SpaceId([1; 16]));

        let winner_marker = s.markers.get(&SpaceId([1; 16])).unwrap();
        assert_eq!(winner_marker.last_read_at.wall_ms, 500);
        assert_eq!(winner_marker.space_id, SpaceId([1; 16]));
    }
}

#[cfg(test)]
mod crypto_integration_tests {
    use super::*;
    use crate::owner_state_crypto::{
        canonical_cbor_decode, canonical_cbor_encode, decrypt_entry, encrypt_entry,
        space_lookup_key, KeyTree,
    };
    use crate::owner_state_types::{
        ContentId, DeliveryStatus, Hlc, NotificationPref, OutboxEntry, OutboxEntryId, OwnerAddr,
        SpaceKind, TransportBinding,
    };

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "alice".into(),
        }
    }

    fn sample_space() -> Space {
        Space {
            id: SpaceId([42; 16]),
            kind: SpaceKind::Channel,
            parent: Some(SpaceId([1; 16])),
            community_id: Some(SpaceId([2; 16])),
            name: "general".into(),
            transport: Some(TransportBinding::Zenoh {
                topic: "harmony/community/2/general".into(),
            }),
            members: vec![],
            custom_name: Some("My #general".into()),
            notification_pref: Some(NotificationPref::Mentions),
            left_at: None,
            created_at: hlc(100),
            updated_at: hlc(100),
        }
    }

    #[test]
    fn space_round_trip_through_phase1_crypto() {
        // 1. Canonical-CBOR encode the Space.
        let space = sample_space();
        let cleartext = canonical_cbor_encode(&space).expect("encode");

        // 2. Derive lookup key + encrypt with Phase 1 crypto.
        let kt = KeyTree::derive(&[0u8; 32]).expect("derive");
        let lookup = space_lookup_key(&kt, b"some-space-id");
        let blob = encrypt_entry(&kt, &lookup, &cleartext).expect("encrypt");

        // 3. Compute cipher_cid (BLAKE3 of the storage blob — what
        //    harmony-content would index by).
        let _cipher_cid = blake3::hash(&blob);

        // 4. Decrypt with the same lookup key → recover cleartext.
        let recovered_cleartext = decrypt_entry(&kt, &lookup, &blob).expect("decrypt");
        assert_eq!(recovered_cleartext, cleartext);

        // 5. Canonical-CBOR decode → recover the Space.
        let recovered: Space = canonical_cbor_decode(&recovered_cleartext).expect("decode");
        assert_eq!(recovered, space);
    }

    #[test]
    fn cross_encoder_determinism_gate() {
        // ZEB-211 spec §Verification gates: encode the same Space 100
        // times; assert byte-identical output. Catches non-determinism
        // in serde_derive / ciborium output for the actual Phase 2
        // type universe.
        let space = sample_space();
        let baseline = canonical_cbor_encode(&space).expect("baseline");
        for _ in 0..100 {
            let bytes = canonical_cbor_encode(&space).expect("repeat");
            assert_eq!(bytes, baseline, "non-deterministic CBOR for Space");
        }
    }

    #[test]
    fn outbox_entry_round_trip_through_phase1_crypto() {
        // Same as space_round_trip but for OutboxEntry — exercises
        // BTreeSet<OwnerAddr> serialization through canonical CBOR.
        let entry = OutboxEntry {
            id: OutboxEntryId([7; 16]),
            space_id: SpaceId([8; 16]),
            recipient_owners: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            message_cid: ContentId([3; 32]),
            created_at: hlc(100),
            delivered_to: [OwnerAddr([1; 16])].into_iter().collect(),
            delivery_status: DeliveryStatus::Partial,
        };
        let cleartext = canonical_cbor_encode(&entry).expect("encode");
        let kt = KeyTree::derive(&[1u8; 32]).expect("derive");
        let lookup = space_lookup_key(&kt, b"outbox-entry-test");
        let blob = encrypt_entry(&kt, &lookup, &cleartext).expect("encrypt");
        let recovered_cleartext = decrypt_entry(&kt, &lookup, &blob).expect("decrypt");
        let recovered: OutboxEntry = canonical_cbor_decode(&recovered_cleartext).expect("decode");
        assert_eq!(recovered, entry);
    }

    /// Regression for PR #73 Greptile P2: DM Spaces carry non-empty
    /// `members`. The sorted-ascending invariant ensures two devices
    /// constructing the same DM produce byte-identical canonical CBOR
    /// (and thus identical cipher_cids) without waiting on CRDT dedup
    /// to converge them.
    #[test]
    fn dm_with_members_yields_identical_cipher_cid_across_devices() {
        let dm = Space {
            id: SpaceId([42; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            // Sorted ascending — required by validate_invariants.
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(100),
            updated_at: hlc(100),
        };
        // Sanity: invariant check must pass for a well-formed DM.
        dm.validate_invariants().expect("DM invariants");

        let cleartext = canonical_cbor_encode(&dm).expect("encode");
        let master = [55u8; 32];
        let kt_a = KeyTree::derive(&master).expect("derive a");
        let kt_b = KeyTree::derive(&master).expect("derive b");
        let lookup_a = space_lookup_key(&kt_a, b"dm-space-id");
        let lookup_b = space_lookup_key(&kt_b, b"dm-space-id");
        let blob_a = encrypt_entry(&kt_a, &lookup_a, &cleartext).expect("encrypt a");
        let blob_b = encrypt_entry(&kt_b, &lookup_b, &cleartext).expect("encrypt b");
        assert_eq!(blob_a, blob_b);
        let cid_a = blake3::hash(&blob_a);
        let cid_b = blake3::hash(&blob_b);
        assert_eq!(cid_a.as_bytes(), cid_b.as_bytes());
    }

    #[test]
    fn two_bound_devices_produce_identical_cipher_cid_for_same_space() {
        // The CRDT convergence property the whole ZEB-211 spec hangs on:
        // two devices encrypting the same Space (same master seed) MUST
        // produce identical cipher_cids, otherwise the CRDT treats them
        // as conflicting writes.
        let space = sample_space();
        let cleartext = canonical_cbor_encode(&space).expect("encode");
        let master = [99u8; 32];
        let kt_a = KeyTree::derive(&master).expect("derive a");
        let kt_b = KeyTree::derive(&master).expect("derive b");
        let lookup_a = space_lookup_key(&kt_a, b"the-space-id");
        let lookup_b = space_lookup_key(&kt_b, b"the-space-id");
        assert_eq!(lookup_a, lookup_b);
        let blob_a = encrypt_entry(&kt_a, &lookup_a, &cleartext).expect("encrypt a");
        let blob_b = encrypt_entry(&kt_b, &lookup_b, &cleartext).expect("encrypt b");
        assert_eq!(
            blob_a, blob_b,
            "deterministic encryption across bound devices"
        );
        let cid_a = blake3::hash(&blob_a);
        let cid_b = blake3::hash(&blob_b);
        assert_eq!(cid_a.as_bytes(), cid_b.as_bytes());
    }
}
