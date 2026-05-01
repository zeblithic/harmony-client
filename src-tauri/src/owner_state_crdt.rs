//! Owner-state CRDT merge semantics (ZEB-215 Sub-A Phase 2).
//!
//! See `docs/specs/2026-04-30-zeb-206-nav-tree-design.md`
//! §"CRDT convergence semantics".

use std::collections::{BTreeMap, BTreeSet};

use crate::owner_state_types::{
    DedupeKey, InboxEntry, InboxKey, OutboxEntry, OutboxEntryId, ReadMarker, Space, SpaceId,
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

        // 3. Check for same-SpaceId update first (always valid, even for folders).
        if self.spaces.contains_key(&incoming.id) {
            let merged = lww_merge_space(self.spaces.get(&incoming.id).unwrap(), &incoming);
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
        match self.outbox.get(&incoming.id) {
            None => {
                let mut entry = incoming;
                // Re-derive status from delivered_to. is_expired is false here —
                // Phase 3 owns the wall-clock 30-day timer and will set
                // delivery_status=Expired explicitly when it fires.
                entry.delivery_status = entry.compute_status(false);
                self.outbox.insert(entry.id, entry);
                ApplyOutcome::Inserted
            }
            Some(existing) => {
                let mut merged = existing.clone();
                merged
                    .delivered_to
                    .extend(incoming.delivered_to.iter().copied());
                // Re-derive status from merged delivered_to. is_expired is
                // false here — Phase 3 owns the wall-clock 30-day timer
                // and will set delivery_status=Expired explicitly when
                // it fires; we only handle the ack-driven transitions.
                merged.delivery_status = merged.compute_status(false);
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

    /// Apply an incoming ReadMarker. `last_read_at` advances monotonically —
    /// older HLCs are rejected so reading state never regresses.
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
                } else {
                    ApplyOutcome::Rejected(RejectionReason::StaleHlc {
                        kind: "ReadMarker",
                        device_id: incoming.last_read_at.device_id.clone(),
                    })
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
        // Device A creates DM with id=1, members=[a, b].
        let outcome_a = s.apply_space(dm(1, vec![1, 2], 100));
        assert_eq!(outcome_a, ApplyOutcome::Inserted);
        // Device B creates DM with id=2, members=[b, a] — same sorted set.
        // Should dedupe; ULID tie-break picks lexicographically smaller (id=1).
        let outcome_b = s.apply_space(dm(2, vec![2, 1], 100));
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

    #[test]
    fn distinct_outbox_ids_dont_collide() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10], vec![]));
        s.apply_outbox(entry(2, vec![20], vec![]));
        assert_eq!(s.outbox.len(), 2);
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
}
