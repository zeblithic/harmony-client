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
}

/// Merge two Space values using last-writer-wins per-field on
/// `updated_at` HLC. `created_at` always takes the earlier HLC.
/// Caller is responsible for setting the merged Space's `id` correctly
/// (the dedupe-key-based caller already chose the winning ULID).
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
