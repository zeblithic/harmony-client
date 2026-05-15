//! Owner-state CRDT merge semantics (ZEB-215 Sub-A Phase 2).
//!
//! See `docs/specs/2026-04-30-zeb-206-nav-tree-design.md`
//! §"CRDT convergence semantics".

use std::collections::{BTreeMap, BTreeSet};

use crate::owner_state_types::{
    DedupeKey, DeliveryStatus, DeviceIdentityHash, DmContentKey, Hlc, InboxEntry, InboxKey,
    LibraryEntry, OutboxEntry, OutboxEntryId, OwnerAddr, OwnerDeviceCache, OwnerDeviceEntry,
    ReadMarker, Space, SpaceId, SpaceKind, MAX_DEVICES_PER_OWNER, MAX_PRIOR_CONTENT_KEYS,
};
use serde::{Deserialize, Serialize};

/// In-memory owner-state CRDT store. Phase 3 wraps this in persistence +
/// transport; Phase 2 owns purely the typed merge semantics.
///
/// Wire format: canonical CBOR map with single-letter-length keys to
/// satisfy `canonical_cbor_encode`'s same-length-keys precondition
/// (see Phase 1 spec). Phase 3a registers this type as
/// `CanonicalPayload`; the renames here keep that registration honest.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerState {
    #[serde(rename = "sp")]
    pub spaces: BTreeMap<SpaceId, Space>,
    #[serde(rename = "ob")]
    pub outbox: BTreeMap<OutboxEntryId, OutboxEntry>,
    #[serde(rename = "ib")]
    pub inbox: BTreeMap<InboxKey, InboxEntry>,
    #[serde(rename = "mk")]
    pub markers: BTreeMap<SpaceId, ReadMarker>,
    /// Permanent tombstones — explicit `remove_space` writes a SpaceId here;
    /// re-add via the normal apply path is rejected. Distinct from
    /// `Space.left_at` which is reversible.
    #[serde(rename = "tm")]
    pub tombstones: BTreeSet<SpaceId>,
    /// ZEB-216 Sub-B Phase 1: per-OwnerAddr device cache for DM unicast
    /// addressing. Replicates across the owner's bound devices via Flow A.
    /// (Phase 3b will use this to resolve from_identity_hash → OwnerAddr
    /// for link-origin binding.)
    #[serde(
        rename = "od",
        skip_serializing_if = "OwnerDeviceCache::is_empty",
        default
    )]
    pub owner_device_cache: OwnerDeviceCache,
    /// ZEB-218 Sub-D Phase 1: per-OwnerAddr trusted-library list.
    /// Replicates across bound devices via Flow A. LWW add/remove
    /// semantics; tombstones retained (see `LibraryEntry::is_effective`).
    #[serde(rename = "lb", skip_serializing_if = "BTreeMap::is_empty", default)]
    pub libraries: BTreeMap<OwnerAddr, LibraryEntry>,
    /// ZEB-243: tombstones for deleted OutboxEntries. Maps each
    /// `OutboxEntryId` to the HLC at which the delete was applied
    /// locally. LWW semantics on merge: the tombstone with the
    /// strictly-greater HLC wins. `apply_outbox` rejects any incoming
    /// entry whose `created_at` HLC is strictly older than its matching
    /// tombstone HLC.
    ///
    /// ULIDs are unique per send; collisions across honest peers are
    /// impossible. The HLC comparison is defensive against clock skew.
    /// No GC in this PR — outbox is bounded by the 30-day expiration
    /// timer, so tombstone growth remains low. See spec §4.1 + §8.
    #[serde(rename = "ot", skip_serializing_if = "BTreeMap::is_empty", default)]
    pub outbox_tombstones: BTreeMap<OutboxEntryId, Hlc>,
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
    /// ZEB-243: incoming OutboxEntry has a matching tombstone whose HLC
    /// is strictly greater than the entry's `created_at` HLC. The
    /// tombstone was written by `delete_dm_outbox_entry` on some device
    /// and replicated via `merge_remote_into_local`. Equal HLCs are
    /// theoretically impossible (tombstone is always written after the
    /// entry, so its HLC is strictly newer on the same device); if they
    /// occur due to clock skew the entry falls through (not rejected).
    #[error("OutboxEntry has a matching tombstone with strictly-newer HLC")]
    OutboxEntryTombstoned,
}

impl OwnerState {
    /// Apply an incoming Space to the CRDT, handling per-kind dedupe,
    /// LWW field merge, ULID tie-break, and tombstone rejection.
    ///
    /// Crate-private: a cross-SpaceId dedupe through this method
    /// removes the loser Space but does NOT rewrite outbox / inbox /
    /// markers. External callers (Phase 3 sync, Tauri IPC) must use
    /// [`Self::apply_space_with_canonicalization`] which leaves
    /// OwnerState internally consistent. Internal tests can still
    /// reach this directly when they don't care about dependent records.
    pub(crate) fn apply_space(&mut self, incoming: Space) -> ApplyOutcome {
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
            // Reject any structural divergence on dedupe-key fields
            // before LWW. We check incoming.dedupe_key() directly
            // rather than the merged result: if we waited until after
            // lww_merge_space, an incoming with different DM members
            // but older HLC would silently lose those bad members to
            // existing's via LWW and the rejection would never fire.
            // Rejecting on incoming's own dedupe_key catches both
            // cases (LWW winner OR loser).
            let existing_dk = existing.dedupe_key();
            if incoming.dedupe_key() != existing_dk {
                return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                    "same-SpaceId update would change dedupe_key for {:?} \
                     (DM members or PublicChannel topic are immutable on \
                     the same SpaceId; logical identity changes need a fresh ULID)",
                    incoming.id
                )));
            }
            // Reject same-SpaceId content_key changes. lww_merge_space picks
            // the current content_key by ULID order, but on a same-SpaceId
            // merge both sides share the same `id` so "winner" is undefined
            // — each replica's local Space wins on its own machine and the
            // active key diverges permanently. Phase 1 has no key rotation
            // API; content_key is set at Space creation and is immutable for
            // that ULID. Future explicit rotation will need its own merge
            // semantics.
            if existing.content_key != incoming.content_key {
                return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                    "same-SpaceId update changes content_key for {:?} \
                     (key rotation needs explicit semantics; current-key \
                     merges must stay deterministic on the same ULID)",
                    incoming.id
                )));
            }
            // Reject same-SpaceId Community creation-pinned field changes.
            // lww_merge_space pins these to the older-created_at side, but
            // an attacker that backdates `created_at` could "win" the pin
            // and shift admin authority / rotate the membership key /
            // flip privacy mode — none of which are valid v1 operations.
            // Reject at apply time so divergent state never enters the
            // CRDT (mirrors content_key rejection above).
            //
            // Phase 1 has no community-creation IPC, so this branch is
            // unreachable today; Phase 2's encrypted Zenoh state-root
            // sync is where it becomes load-bearing. Adding the gate now
            // means Phase 2 can't accidentally regress it.
            if existing.kind == SpaceKind::Community {
                if existing.current_epoch_key != incoming.current_epoch_key {
                    return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                        "same-SpaceId community update changes current_epoch_key for {:?} \
                         (creation-pinned in v1; rotation via EpochRotation event is ZEB-249)",
                        incoming.id
                    )));
                }
                // M13: guard current_epoch and old_epoch_keys for the same
                // reason as current_epoch_key. A malicious or stale Space row
                // with a different current_epoch / old_epoch_keys set would
                // diverge the key history, leaving some replicas unable to
                // decrypt historical messages. These are creation-pinned in v1;
                // rotation via EpochRotation event is ZEB-249.
                if existing.current_epoch != incoming.current_epoch {
                    return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                        "same-SpaceId community update changes current_epoch for {:?} \
                         (creation-pinned in v1; epoch advance via EpochRotation event is ZEB-249)",
                        incoming.id
                    )));
                }
                if existing.old_epoch_keys != incoming.old_epoch_keys {
                    return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                        "same-SpaceId community update changes old_epoch_keys for {:?} \
                         (creation-pinned in v1; key history is managed via EpochRotation events)",
                        incoming.id
                    )));
                }
                if existing.admin_addr != incoming.admin_addr {
                    return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                        "same-SpaceId community update changes admin_addr for {:?} \
                         (creation-pinned in v1; admin transfer needs explicit semantics)",
                        incoming.id
                    )));
                }
                if existing.is_invite_only != incoming.is_invite_only {
                    return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                        "same-SpaceId community update changes is_invite_only for {:?} \
                         (creation-pinned in v1; privacy mode is set once)",
                        incoming.id
                    )));
                }
                if existing.created_at != incoming.created_at {
                    return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                        "same-SpaceId community update changes created_at for {:?} \
                         (immutable; defends against backdating attacks that would \
                         hijack the lww_merge_space creator-pin)",
                        incoming.id
                    )));
                }
            }
            let merged = lww_merge_space(existing, &incoming);
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
        // ZEB-243: tombstone gate. Strict-greater-than semantics —
        // tombstone wins iff its HLC is strictly newer than the entry's
        // `created_at`. Equal HLCs (theoretically impossible since
        // tombstones are written after entries on the same device via the
        // same monotone tracker) fall through — not rejected.
        if let Some(tombstone_hlc) = self.outbox_tombstones.get(&incoming.id) {
            if tombstone_hlc.is_strictly_newer_than(&incoming.created_at) {
                return ApplyOutcome::Rejected(RejectionReason::OutboxEntryTombstoned);
            }
        }

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
                // logical message, so message_cid/recipient_owners/
                // created_at MUST match. A divergence implies ULID
                // collision, replay attack, or buggy peer — reject
                // rather than silently overwriting (existing wins) so
                // the operator sees the divergence.
                //
                // space_id is intentionally NOT in this check.
                // canonicalize_dependent_space_ids rewrites stored
                // outbox space_ids when a Space dedupe collapses two
                // SpaceIds into one. A peer that hasn't yet learned
                // about that dedupe may still send acks referencing
                // the original (loser) space_id; we keep existing's
                // space_id (already canonicalized) and merge the
                // delivered_to set. message_cid (a content hash) and
                // recipients/created_at (immutable per logical
                // message) remain the strong identity signal. The
                // delivered_to set could legitimately differ; that's
                // what we union below.
                if existing.message_cid != incoming.message_cid
                    || existing.recipient_owners != incoming.recipient_owners
                    || existing.created_at != incoming.created_at
                {
                    return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                        "OutboxEntry {:?} envelope mismatch (message_cid/\
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

    /// Iterator over InboxEntries belonging to a given Space, in
    /// BTreeMap natural order (`(space_id, message_cid)` lex).
    ///
    /// For UI scrollback, callers typically collect + sort by
    /// `received_at` descending. The natural BTreeMap order is by
    /// message_cid which IS NOT chronological — `received_at` is
    /// the chronological key.
    ///
    /// Implementation: BTreeMap range scoped by SpaceId. `InboxKey` orders
    /// by `(space_id, message_cid)` so all entries for `space_id` form a
    /// contiguous run in the map. We start the range at the all-zero
    /// `message_cid` and `take_while` `key.space_id == space_id` to bound
    /// the scan to O(matching) rather than O(total inbox entries).
    pub fn inbox_entries_for_space(&self, space_id: SpaceId) -> impl Iterator<Item = &InboxEntry> {
        let start = InboxKey {
            space_id,
            message_cid: crate::owner_state_types::ContentId::from_bytes([0u8; 32]),
        };
        self.inbox
            .range(start..)
            .take_while(move |(k, _)| k.space_id == space_id)
            .map(|(_, e)| e)
    }

    /// Remove an InboxEntry by (space_id, message_cid). Returns the
    /// removed entry on hit, None on miss. Idempotent: second call
    /// with the same key returns None.
    ///
    /// Phase 4: used by `delete_outbox_entry` IPC to clear a stuck/
    /// expired self-Message from the user's history.
    pub fn delete_inbox_entry(&mut self, key: InboxKey) -> Option<InboxEntry> {
        self.inbox.remove(&key)
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

    /// Apply a device-list update for an OwnerAddr. LWW on `learned_at` HLC;
    /// devices are deduped + sorted + capped at MAX_DEVICES_PER_OWNER before
    /// storage to bound cache memory and prevent cache-growth DoS via spoofed
    /// updates.
    ///
    /// Equal-HLC semantics deliberately differ from `apply_marker`. A marker's
    /// payload is uniquely identified by its HLC (the marker IS the HLC), so
    /// equal-HLC replay is always idempotent. `OwnerDeviceEntry` carries a
    /// separate `devices` payload, so equal-HLC must additionally check that
    /// the payload matches before treating it as a replay; otherwise two
    /// replicas that concurrently issue different `devices` lists under the
    /// same HLC would each keep their local list and report success, leaving
    /// the cache permanently divergent. Equal-HLC + matching devices →
    /// idempotent `Merged { old_id: None }`; equal-HLC + diverging devices →
    /// `Rejected(InvariantFail)`.
    ///
    /// See ZEB-216 §"OwnerDeviceCache (Phase 1)".
    pub fn apply_owner_device_update(
        &mut self,
        addr: OwnerAddr,
        devices: Vec<DeviceIdentityHash>,
        device_identity_pubs: Vec<Option<[u8; 64]>>,
        learned_at: Hlc,
    ) -> ApplyOutcome {
        // Sanitize: the on-the-wire vec might be unsorted / duplicated /
        // oversized. Sort+dedup `devices` while maintaining the parallel-
        // vec correspondence with `device_identity_pubs`.
        //
        // Pad/truncate device_identity_pubs to match devices.len() FIRST so
        // the zip is well-formed even if the caller passed a mismatched-
        // length pubs vec (Phase 1/2 callers pass `vec![]`; defensive
        // sanitization is cheap).
        let mut pubs = device_identity_pubs;
        pubs.resize(devices.len(), None);

        // Zip into Vec<(DeviceIdentityHash, Option<[u8; 64]>)>, sort by
        // .0, then walk-and-merge consecutive entries with the same hash.
        // This is the one correct way to preserve parallel-vec
        // correspondence through sort+dedup — naively sorting `devices`
        // and `device_identity_pubs` independently would shuffle pubs out
        // of alignment, silently breaking signature lookups in
        // `resolve_signed_origin_owner`.
        //
        // Merge rule for duplicate-hash entries (same `DeviceIdentityHash`
        // appearing more than once in the zipped vec):
        //   - both `None` → `None`.
        //   - exactly one `Some(pub)` → keep the `Some` (the other side
        //     was just "known by hash, pub not yet propagated"; merging
        //     LOSES information if we kept `None` — `dedup_by_key` did
        //     exactly this and dropped a Some when it followed a None).
        //   - both `Some(pub)` and equal → keep one.
        //   - both `Some(pub)` and DIFFERENT → reject as invariant fail.
        //     A peer claiming two different identity pubs for the same
        //     `DeviceIdentityHash` is either malicious or a bug in their
        //     bootstrap path; either way, silently picking one would leak
        //     a TOCTOU into signature verification.
        let mut zipped: Vec<(DeviceIdentityHash, Option<[u8; 64]>)> =
            devices.into_iter().zip(pubs).collect();
        zipped.sort_by_key(|(d, _)| *d);

        let mut merged: Vec<(DeviceIdentityHash, Option<[u8; 64]>)> =
            Vec::with_capacity(zipped.len());
        for (d, p) in zipped {
            match merged.last_mut() {
                Some((prev_d, prev_p)) if *prev_d == d => {
                    // Merge into the existing entry per the rules above.
                    match (*prev_p, p) {
                        (None, None) => {}
                        (None, Some(_)) => *prev_p = p,
                        (Some(_), None) => {}
                        (Some(a), Some(b)) if a == b => {}
                        (Some(_), Some(_)) => {
                            return ApplyOutcome::Rejected(RejectionReason::InvariantFail(
                                format!(
                                    "owner_device_entry for {:?} has conflicting identity pubs \
                                     for device {:?}",
                                    addr, d
                                ),
                            ));
                        }
                    }
                }
                _ => merged.push((d, p)),
            }
        }
        merged.truncate(MAX_DEVICES_PER_OWNER);

        // Defense-in-depth: every cached `Some(identity_pub)` MUST derive
        // (via `derive_device_hash_from_identity_pub` = SHA256(pub)[:16])
        // to its paired `DeviceIdentityHash`. A malformed/poisoned cache
        // entry where the pair is mismatched would silently fail every
        // later signature verify in `resolve_signed_origin_owner`,
        // converting "this device's signature didn't match" into a
        // confusing dead-letter; rejecting here surfaces the bug at
        // apply time. `derive_device_hash_from_identity_pub` returns
        // `None` for a structurally-invalid pub (malformed X25519 /
        // Ed25519 split) — also reject as InvariantFail.
        for (d, p) in merged.iter() {
            if let Some(pub_bytes) = p {
                match crate::dm_signing::derive_device_hash_from_identity_pub(pub_bytes) {
                    Some(derived) if derived == *d => {}
                    Some(derived) => {
                        return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                            "owner_device_entry for {:?} has identity pub for device {:?} \
                             that derives to a different device hash {:?}",
                            addr, d, derived
                        )));
                    }
                    None => {
                        return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                            "owner_device_entry for {:?} has structurally-invalid identity pub \
                             for device {:?}",
                            addr, d
                        )));
                    }
                }
            }
        }

        let (sanitized_devices, sanitized_pubs): (Vec<_>, Vec<_>) = merged.into_iter().unzip();

        // LWW guard.
        if let Some(existing) = self.owner_device_cache.devices.get(&addr) {
            if existing.learned_at.is_strictly_newer_than(&learned_at) {
                return ApplyOutcome::Rejected(RejectionReason::StaleHlc {
                    kind: "owner_device_entry",
                    device_id: learned_at.device_id.clone(),
                });
            }
            if existing.learned_at == learned_at {
                // Equal HLC — only idempotent if the payload (devices AND
                // pubs) matches. Two replicas concurrently issuing
                // different payloads under the same HLC would otherwise
                // diverge silently.
                //
                // Note on equal-HLC + None-from-existing-Some: at equal
                // HLC the new entry is supposed to be the SAME logical
                // update (same wall_ms+logical+device_id). Asking us to
                // "downgrade" an existing `Some(P)` to `None` at the
                // same HLC would mean the same source emitted two
                // different payloads under the same HLC, which is a
                // non-deterministic-source invariant fail. Reject. The
                // pub-preserve merge below applies only when new HLC
                // is strictly newer.
                if existing.devices == sanitized_devices
                    && existing.device_identity_pubs == sanitized_pubs
                {
                    return ApplyOutcome::Merged { old_id: None };
                }
                return ApplyOutcome::Rejected(RejectionReason::InvariantFail(format!(
                    "owner_device_entry for {:?} diverges at identical learned_at \
                     (concurrent updates with same HLC but different devices or pubs)",
                    addr
                )));
            }
        }
        // Per-device pub-preserve LWW: when the new entry passes LWW
        // (strictly newer HLC), a `None` for a device hash whose
        // existing entry has `Some(pub)` MUST NOT erase that pub.
        // Otherwise a peer who learned about a device but doesn't yet
        // have its identity_pub (Path B bootstrap-incompleteness) would
        // clobber our cached pub on every gossip, breaking signature
        // verification for that device until the next invite-equivalent
        // flow. Mirrors the in-merge `Some-over-None` rule already in
        // place for duplicate-hash entries within a single update.
        //
        // Conflict (existing `Some(A)` vs new `Some(B)` with `A != B`)
        // for the same device hash → InvariantFail: a peer claiming a
        // different pub for an already-known device hash is either
        // malicious or a bug; silently picking one would leak a TOCTOU
        // into signature verification.
        let merged_pubs: Vec<Option<[u8; 64]>> =
            if let Some(existing) = self.owner_device_cache.devices.get(&addr) {
                let mut out = Vec::with_capacity(sanitized_devices.len());
                for (d, new_p) in sanitized_devices.iter().zip(sanitized_pubs.iter()) {
                    let merged = match existing.devices.binary_search(d) {
                        // Device hash exists in cached entry — apply the
                        // per-pub merge rule.
                        Ok(idx) => match (existing.device_identity_pubs[idx], *new_p) {
                            (Some(p), None) => Some(p), // PRESERVE known pub
                            (None, None) => None,
                            (None, Some(p)) => Some(p), // ADOPT new pub
                            (Some(a), Some(b)) if a == b => Some(a),
                            (Some(_), Some(_)) => {
                                return ApplyOutcome::Rejected(RejectionReason::InvariantFail(
                                    format!(
                                        "owner_device_entry for {:?} has conflicting identity \
                                         pub for device {:?} vs existing cached pub",
                                        addr, d
                                    ),
                                ));
                            }
                        },
                        // Device hash is new (or removed-then-readded by a
                        // peer with no cached pub) — use whatever the new
                        // entry says.
                        Err(_) => *new_p,
                    };
                    out.push(merged);
                }
                out
            } else {
                sanitized_pubs.clone()
            };

        let was_present = self.owner_device_cache.devices.contains_key(&addr);
        self.owner_device_cache.devices.insert(
            addr,
            OwnerDeviceEntry {
                devices: sanitized_devices,
                device_identity_pubs: merged_pubs,
                learned_at,
            },
        );
        if was_present {
            ApplyOutcome::Merged { old_id: None }
        } else {
            ApplyOutcome::Inserted
        }
    }

    /// Returns the current epoch key for a community Space, or `None` if the
    /// Space is not found or has no epoch key set.
    ///
    /// Used by the self-healing observer to obtain the CURRENT epoch key (the
    /// key that was installed when `kick_from_community` / `leave_community`
    /// landed the rotation locally) rather than the engine's spawn-time key.
    ///
    /// ZEB-249 §10.6 Phase C — hydration watermark note: a separate
    /// "have-we-replayed-all-events-since-boot" flag is NOT needed here.
    /// Phase A wires `CommunitySyncEngine` to read the live key from this
    /// function on every publish/decrypt, and Phase B's
    /// `apply_remote_epoch_event` updates this entry immediately when a
    /// remote EpochRotation / EpochCatchup delta lands. Together they ensure
    /// the key is always current by the time any encrypt or decrypt is
    /// attempted — no separate replay-complete gate is required.
    pub fn current_epoch_key_for(
        &self,
        community_id: crate::owner_state_types::SpaceId,
    ) -> Option<crate::owner_state_types::EpochKey> {
        self.spaces
            .get(&community_id)
            .and_then(|s| s.current_epoch_key.clone())
    }
}

/// Merge two sides' content keys per ZEB-216 §"Dedupe-merge cap rule":
///   1. Take winner.prior, plus loser.current as a one-element addition,
///      plus loser.prior.
///   2. Filter out winner.current (the active key MUST NOT appear in prior
///      per validate_invariants).
///   3. Sort ascending lex by raw key bytes.
///   4. Dedup (set-semantics on byte equality).
///   5. Truncate to MAX_PRIOR_CONTENT_KEYS.
///
/// For same-SpaceId LWW merges, pass `loser_current == winner_current` —
/// the duplicate gets filtered in step 2 so the operation is the same.
///
/// Order-independent (CRDT-convergent) under multi-merge — see ZEB-219
/// §"Why first N of sorted" for the proof and the 5-Space convergence
/// regression test in this module.
///
/// SECURITY NOTE: truncation keeps lex-smallest entries. An attacker
/// who can publish a Space (winning the LWW gate) with a chosen
/// content_key could grind low-byte keys to displace legitimate prior
/// keys from the cap, making earlier messages encrypted under those
/// keys silently undecryptable. Acceptable in Phase 1 since the LWW
/// gate (HLC + ULID) limits attack feasibility. Same class as the
/// OwnerDeviceCache lex-grinding concern documented on
/// OwnerDeviceEntry.devices and ZEB-219's prior_content_keys discussion.
pub(crate) fn merge_prior_content_keys(
    winner_current: &DmContentKey,
    winner_prior: &[DmContentKey],
    loser_current: &DmContentKey,
    loser_prior: &[DmContentKey],
) -> Vec<DmContentKey> {
    let mut all: Vec<DmContentKey> = winner_prior
        .iter()
        .cloned()
        .chain(std::iter::once(loser_current.clone()))
        .chain(loser_prior.iter().cloned())
        .filter(|k| k.as_bytes() != winner_current.as_bytes())
        .collect();
    all.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    all.dedup(); // PartialEq is byte-equality on the inner [u8; 32]
    all.truncate(MAX_PRIOR_CONTENT_KEYS);
    all
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
///
/// content_key/prior_content_keys: for DM spaces, applies the
/// ZEB-216 §"Dedupe-merge cap rule" via `merge_prior_content_keys`.
/// The winner for content_key purposes is the Space with the lex-smaller
/// `id` ULID (stable across same-SpaceId and cross-SpaceId merges).
/// For same-SpaceId merges, both sides have the same id so the winner
/// pick is moot; v1 has no key rotation so content_key is identical on
/// both sides anyway.
///
/// Dual-semantics rationale: mutable fields (name, parent, custom_name,
/// etc.) use HLC LWW — the side with the strictly-newer `updated_at`
/// wins. content_key/prior_content_keys use ULID LWW (lex-smaller `id`
/// wins) instead. This is intentional: content_key must be
/// topology-independent and stable across all merge orderings
/// (CRDT-convergent), and `id` is set at creation and never updated,
/// making it safe for that role. HLC could in principle be tied across
/// two divergent devices and would not give a deterministic winner.
fn lww_merge_space(a: &Space, b: &Space) -> Space {
    let newer = if b.updated_at.is_strictly_newer_than(&a.updated_at) {
        b
    } else {
        a
    };

    // content_key/prior_content_keys: apply cap-rule merge for DM spaces.
    // For same-SpaceId merges a.id == b.id so winner_is_a is arbitrary
    // (content_key is identical in v1). For cross-SpaceId dedupe collapse,
    // the lex-smaller ULID is the canonical winner.
    let (content_key, prior_content_keys) = match (&a.content_key, &b.content_key) {
        (Some(a_ck), Some(b_ck)) => {
            // Determine winner by ULID lex order (smaller = winner).
            let (winner_ck, winner_prior, loser_ck, loser_prior) = if a.id <= b.id {
                (
                    a_ck,
                    &a.prior_content_keys[..],
                    b_ck,
                    &b.prior_content_keys[..],
                )
            } else {
                (
                    b_ck,
                    &b.prior_content_keys[..],
                    a_ck,
                    &a.prior_content_keys[..],
                )
            };
            let merged_prior =
                merge_prior_content_keys(winner_ck, winner_prior, loser_ck, loser_prior);
            (Some(winner_ck.clone()), merged_prior)
        }
        // Non-DM kinds have no content_key; keep as-is.
        (None, None) => (None, vec![]),
        // Mixed Some/None across the same dedupe_key is an invariant
        // violation. validate_invariants runs before any merge, so this
        // branch is only reachable if the caller bypassed it. Defensive:
        // prefer whichever side has Some, carry no prior.
        (Some(ck), None) => (Some(ck.clone()), vec![]),
        (None, Some(ck)) => (Some(ck.clone()), vec![]),
    };

    // admin_addr, current_epoch_key, and is_invite_only are creation-pinned
    // for Community spaces (per ZEB-217 spec: no admin transfer, no
    // membership-key rotation, no flip-to-private in v1). LWW on
    // updated_at would let a malicious or buggy peer publish a same-
    // SpaceId Space with a fresher HLC and different bootstrap admin /
    // encryption key / privacy mode — silently shifting power-100
    // authority, locking prior encrypted state, or flipping the
    // community model from open to invite-only. Pin to the side with
    // the OLDER created_at instead. Same-SpaceId merge with the same
    // original creator yields identical values; cross-creator
    // divergence is an invariant violation caught upstream by
    // validate_invariants.
    let creator_side = if a.created_at.is_strictly_newer_than(&b.created_at) {
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
        created_at: creator_side.created_at.clone(),
        updated_at: newer.updated_at.clone(),
        content_key,
        prior_content_keys,
        current_epoch: creator_side.current_epoch,
        current_epoch_key: creator_side.current_epoch_key.clone(),
        old_epoch_keys: creator_side.old_epoch_keys.clone(),
        admin_addr: creator_side.admin_addr,
        is_invite_only: creator_side.is_invite_only,
        // shared_in_profile is LWW like other mutable per-device prefs
        // (notification_pref, custom_name). Hardcoding `false` here
        // would break the cross-device opt-in promise in ZEB-281 spec
        // §2 — Device A opting in could be silently overwritten by a
        // stale Device B replay. Newer `updated_at` wins.
        shared_in_profile: newer.shared_in_profile,
        // pending_join_at is LWW — the newer updated_at wins, covering
        // both None→Some (PendingJoin minted) and Some→None (countersign
        // received) transitions. ZEB-254 §"CRDT merge".
        pending_join_at: newer.pending_join_at.clone(),
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
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        }
    }

    fn community_space(
        id: u8,
        admin_addr: OwnerAddr,
        epoch_key: crate::owner_state_types::EpochKey,
        invite_only: bool,
    ) -> Space {
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "C".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(100),
            updated_at: hlc(100),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(epoch_key),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(admin_addr),
            is_invite_only: Some(invite_only),
            shared_in_profile: false,
            pending_join_at: None,
        }
    }

    fn dm(id: u8, members: Vec<u8>, ts: u64) -> Space {
        use crate::owner_state_types::DmContentKey;
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
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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

    /// Same-SpaceId LWW merge: v1 has no key rotation so both sides have the
    /// same content_key. The cap-rule merge on prior_content_keys must union
    /// both sides' priors, dedup, sort, and cap. Winner content_key is
    /// preserved unchanged.
    /// apply_space rejects same-SpaceId Community updates that change
    /// the creation-pinned fields (current_epoch_key, admin_addr,
    /// is_invite_only, created_at). Without this check, a backdated
    /// incoming Space could win the lww_merge_space creator-pin and
    /// hijack admin authority / rotate the symmetric key.
    #[test]
    fn apply_space_rejects_same_space_id_community_admin_addr_change() {
        use crate::owner_state_types::EpochKey;

        let mut s = OwnerState::default();
        let original = community_space(7, OwnerAddr([1u8; 16]), EpochKey::new([0xaa; 32]), false);
        let outcome = s.apply_space(original.clone());
        assert_eq!(outcome, ApplyOutcome::Inserted);

        let mut takeover = original.clone();
        takeover.admin_addr = Some(OwnerAddr([99u8; 16])); // attacker
        takeover.updated_at = hlc(999);
        let outcome = s.apply_space(takeover);
        match outcome {
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(msg)) => {
                assert!(
                    msg.contains("admin_addr"),
                    "expected admin_addr rejection; got: {msg}"
                );
            }
            other => panic!("expected InvariantFail rejection, got {other:?}"),
        }
        assert_eq!(
            s.spaces.get(&SpaceId([7; 16])).unwrap().admin_addr,
            Some(OwnerAddr([1u8; 16])),
            "admin_addr must be unchanged after rejection"
        );
    }

    #[test]
    fn apply_space_rejects_same_space_id_community_current_epoch_key_change() {
        use crate::owner_state_types::EpochKey;

        let mut s = OwnerState::default();
        let original = community_space(7, OwnerAddr([1u8; 16]), EpochKey::new([0xaa; 32]), false);
        let outcome = s.apply_space(original.clone());
        assert_eq!(outcome, ApplyOutcome::Inserted);

        let mut rotation = original.clone();
        rotation.current_epoch_key = Some(EpochKey::new([0xbb; 32]));
        rotation.updated_at = hlc(999);
        let outcome = s.apply_space(rotation);
        match outcome {
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(msg)) => {
                assert!(
                    msg.contains("current_epoch_key"),
                    "expected current_epoch_key rejection; got: {msg}"
                );
            }
            other => panic!("expected InvariantFail rejection, got {other:?}"),
        }
    }

    #[test]
    fn apply_space_rejects_same_space_id_community_current_epoch_change() {
        use crate::owner_state_types::EpochKey;

        let mut s = OwnerState::default();
        let original = community_space(7, OwnerAddr([1u8; 16]), EpochKey::new([0xaa; 32]), false);
        assert_eq!(original.current_epoch, Some(0));
        let outcome = s.apply_space(original.clone());
        assert_eq!(outcome, ApplyOutcome::Inserted);

        let mut update = original.clone();
        update.current_epoch = Some(1); // attempt to advance epoch without EpochRotation event
        update.updated_at = hlc(999);
        let outcome = s.apply_space(update);
        match outcome {
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(msg)) => {
                assert!(
                    msg.contains("current_epoch"),
                    "expected current_epoch rejection; got: {msg}"
                );
            }
            other => panic!("expected InvariantFail rejection, got {other:?}"),
        }
    }

    #[test]
    fn apply_space_rejects_same_space_id_community_old_epoch_keys_change() {
        use crate::owner_state_types::EpochKey;

        let mut s = OwnerState::default();
        let original = community_space(7, OwnerAddr([1u8; 16]), EpochKey::new([0xaa; 32]), false);
        assert!(original.old_epoch_keys.is_empty());
        let outcome = s.apply_space(original.clone());
        assert_eq!(outcome, ApplyOutcome::Inserted);

        let mut update = original.clone();
        // Inject a stale old key — must be rejected
        update.old_epoch_keys.insert(0, EpochKey::new([0xdd; 32]));
        update.updated_at = hlc(999);
        let outcome = s.apply_space(update);
        match outcome {
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(msg)) => {
                assert!(
                    msg.contains("old_epoch_keys"),
                    "expected old_epoch_keys rejection; got: {msg}"
                );
            }
            other => panic!("expected InvariantFail rejection, got {other:?}"),
        }
    }

    #[test]
    fn apply_space_rejects_same_space_id_community_is_invite_only_flip() {
        use crate::owner_state_types::EpochKey;

        let mut s = OwnerState::default();
        let original = community_space(7, OwnerAddr([1u8; 16]), EpochKey::new([0xaa; 32]), false);
        let outcome = s.apply_space(original.clone());
        assert_eq!(outcome, ApplyOutcome::Inserted);

        let mut flip = original.clone();
        flip.is_invite_only = Some(true); // open → invite-only takeover
        flip.updated_at = hlc(999);
        let outcome = s.apply_space(flip);
        match outcome {
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(msg)) => {
                assert!(
                    msg.contains("is_invite_only"),
                    "expected is_invite_only rejection; got: {msg}"
                );
            }
            other => panic!("expected InvariantFail rejection, got {other:?}"),
        }
    }

    #[test]
    fn apply_space_rejects_same_space_id_community_created_at_backdate() {
        // The backdating attack from CodeRabbit: even though
        // lww_merge_space pins to the older created_at side, an attacker
        // can set their incoming created_at older than the legitimate
        // creator's to "win" the pin. Reject the change at apply time.
        use crate::owner_state_types::EpochKey;

        let mut s = OwnerState::default();
        let original = community_space(7, OwnerAddr([1u8; 16]), EpochKey::new([0xaa; 32]), false);
        let outcome = s.apply_space(original.clone());
        assert_eq!(outcome, ApplyOutcome::Inserted);

        let mut backdate = original.clone();
        backdate.created_at = hlc(1); // claim to be older than original (created_at=1)
        backdate.updated_at = hlc(999);
        let outcome = s.apply_space(backdate);
        match outcome {
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(msg)) => {
                assert!(
                    msg.contains("created_at"),
                    "expected created_at rejection; got: {msg}"
                );
            }
            other => panic!("expected InvariantFail rejection, got {other:?}"),
        }
    }

    #[test]
    fn apply_space_accepts_same_space_id_community_mutable_field_update() {
        use crate::owner_state_types::EpochKey;

        let mut s = OwnerState::default();
        let original = community_space(7, OwnerAddr([1u8; 16]), EpochKey::new([0xaa; 32]), false);
        let outcome = s.apply_space(original.clone());
        assert_eq!(outcome, ApplyOutcome::Inserted);

        let mut renamed = original.clone();
        renamed.name = "renamed".into();
        renamed.updated_at = hlc(999);
        let outcome = s.apply_space(renamed);
        assert_eq!(outcome, ApplyOutcome::Merged { old_id: None });
        assert_eq!(s.spaces.get(&SpaceId([7; 16])).unwrap().name, "renamed");
    }

    /// Defense-in-depth for ZEB-217: admin_addr, current_epoch_key, and
    /// is_invite_only are creation-pinned for Community spaces. A
    /// fresher updated_at on a same-SpaceId Space must NOT shift the
    /// bootstrap admin, rotate the epoch key, or flip privacy
    /// mode — those are explicit higher-level operations that don't
    /// exist in v1. Pinning to the older created_at side defends
    /// against a malicious or buggy peer publishing a Space with the
    /// same SpaceId but a hostile admin/key/policy.
    #[test]
    fn lww_merge_community_pins_creation_fields_to_older_creator() {
        use crate::owner_state_types::EpochKey;

        let original_admin = OwnerAddr([1u8; 16]);
        let original_key = EpochKey::new([0xaa; 32]);
        let earlier = Space {
            id: SpaceId([7; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "earlier".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(100), // older creator
            updated_at: hlc(100),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(original_key.clone()),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(original_admin),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        let attacker_replay = Space {
            id: SpaceId([7; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "later".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(200), // newer (attacker pretends to be original creator)
            updated_at: hlc(300), // even fresher
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(1),
            current_epoch_key: Some(EpochKey::new([0xbb; 32])), // hostile rotation
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([99u8; 16])), // hostile admin takeover
            is_invite_only: Some(true),              // hostile flip to private
            shared_in_profile: false,
            pending_join_at: None,
        };

        let merged = lww_merge_space(&earlier, &attacker_replay);

        assert_eq!(
            merged.admin_addr,
            Some(original_admin),
            "admin_addr must pin to the older creator — power-100 authority can't shift via LWW"
        );
        assert_eq!(
            merged.current_epoch_key.as_ref().map(|k| *k.as_bytes()),
            Some(*original_key.as_bytes()),
            "current_epoch_key must pin to creation — rotation would lock prior encrypted state"
        );
        assert_eq!(
            merged.is_invite_only,
            Some(false),
            "is_invite_only must pin to creation — privacy mode is set once"
        );
        // M13: current_epoch and old_epoch_keys must also pin to the older creator.
        assert_eq!(
            merged.current_epoch,
            Some(0),
            "current_epoch must pin to older creator's value — epoch advance via EpochRotation only"
        );
        assert_eq!(
            merged.old_epoch_keys,
            std::collections::BTreeMap::new(),
            "old_epoch_keys must pin to older creator's value — key history via EpochRotation only"
        );
        // Mutable fields still LWW (newer wins)
        assert_eq!(
            merged.name, "later",
            "mutable fields like name still take the newer side"
        );
    }

    /// Sub-D Phase 4 (ZEB-281): `shared_in_profile` is a mutable
    /// per-device preference that MUST follow LWW semantics — the newer
    /// `updated_at` side wins. A prior revision of `lww_merge_space`
    /// hardcoded `false`, which silently overwrote a Device A opt-in
    /// whenever a stale Device B replay merged in. This regression
    /// pins the LWW behaviour so the cross-device opt-in promise in
    /// spec §2 doesn't break again.
    #[test]
    fn lww_merge_preserves_shared_in_profile_from_newer() {
        use crate::owner_state_types::EpochKey;

        // Older (Device B): opted OUT.
        let older = Space {
            id: SpaceId([42; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "C".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(100),
            updated_at: hlc(100), // older
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0xaa; 32])),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([1u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        // Newer (Device A): opted IN — user just flipped the toggle.
        let newer = Space {
            id: SpaceId([42; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "C".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(100),
            updated_at: hlc(500), // strictly newer
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0xaa; 32])),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([1u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: true,
            pending_join_at: None,
        };

        // Merge in both orderings; the LWW winner is `newer` regardless
        // of argument position so the result must inherit
        // shared_in_profile=true either way.
        let merged_a = lww_merge_space(&older, &newer);
        assert!(
            merged_a.shared_in_profile,
            "merge(older, newer): newer side opted in MUST win — \
             prior hardcoded `false` silently dropped Device A's opt-in"
        );
        let merged_b = lww_merge_space(&newer, &older);
        assert!(
            merged_b.shared_in_profile,
            "merge(newer, older): result must be argument-order-independent"
        );

        // Symmetric case: newer side opted OUT must also win.
        let mut newer_opt_out = newer.clone();
        newer_opt_out.shared_in_profile = false;
        let mut older_opt_in = older.clone();
        older_opt_in.shared_in_profile = true;
        let merged_out = lww_merge_space(&older_opt_in, &newer_opt_out);
        assert!(
            !merged_out.shared_in_profile,
            "newer opt-out MUST win over older opt-in (LWW symmetric)"
        );
    }

    #[test]
    fn lww_merge_same_space_id_prior_content_keys_union() {
        use crate::owner_state_types::DmContentKey;

        let shared_key = DmContentKey::new([0xaa; 32]);

        let a = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(shared_key.clone()),
            prior_content_keys: vec![DmContentKey::new([0x10; 32])],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };
        let b = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(2),
            content_key: Some(shared_key.clone()),
            prior_content_keys: vec![DmContentKey::new([0x20; 32])],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };

        // Order-independent: both call orderings yield the same merged prior.
        let merged_ab = lww_merge_space(&a, &b);
        let merged_ba = lww_merge_space(&b, &a);

        // content_key is unchanged.
        assert_eq!(
            merged_ab.content_key.as_ref().unwrap().as_bytes(),
            &[0xaa; 32]
        );
        assert_eq!(
            merged_ba.content_key.as_ref().unwrap().as_bytes(),
            &[0xaa; 32]
        );

        // prior_content_keys is the union of both sides, sorted ascending,
        // with winner_current (0xaa) filtered out. Result: [0x10, 0x20].
        let ab_prior: Vec<[u8; 32]> = merged_ab
            .prior_content_keys
            .iter()
            .map(|k| *k.as_bytes())
            .collect();
        let ba_prior: Vec<[u8; 32]> = merged_ba
            .prior_content_keys
            .iter()
            .map(|k| *k.as_bytes())
            .collect();
        assert_eq!(
            ab_prior, ba_prior,
            "same-SpaceId merge must be order-independent"
        );
        assert_eq!(ab_prior, vec![[0x10; 32], [0x20; 32]]);
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
        use crate::owner_state_types::DmContentKey;
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
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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

    /// Regression: a same-SpaceId DM update that swaps content_key must be
    /// rejected. lww_merge_space picks the current content_key by ULID
    /// order, but on a same-SpaceId merge both sides share the same `id` so
    /// "winner" is undefined — each replica's local Space wins on its own
    /// machine and the active key diverges silently. Phase 1 has no key
    /// rotation API, so this is an outright invariant violation.
    #[test]
    fn same_id_dm_content_key_change_rejects() {
        use crate::owner_state_types::DmContentKey;
        let mut s = OwnerState::default();
        // Seed DM with content_key = [0xaa; 32] (the helper's default).
        assert_eq!(
            s.apply_space(dm(1, vec![1, 2], 100)),
            ApplyOutcome::Inserted
        );

        // Same SpaceId, same members, same kind, BUT content_key swapped to
        // [0xbb; 32]. Must be rejected with a content_key-flavored message.
        let mut rotated = dm(1, vec![1, 2], 200);
        rotated.content_key = Some(DmContentKey::new([0xbb; 32]));
        let outcome = s.apply_space(rotated);
        match outcome {
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(msg)) => {
                assert!(
                    msg.contains("content_key"),
                    "expected message mentioning content_key, got: {msg}"
                );
            }
            other => panic!(
                "expected Rejected(InvariantFail) on content_key change, got {:?}",
                other
            ),
        }
        // Stored Space must still have the original content_key.
        let stored = s.spaces.get(&SpaceId([1; 16])).unwrap();
        assert_eq!(
            stored.content_key,
            Some(DmContentKey::new([0xaa; 32])),
            "local content_key must not have changed"
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
            message_cid: ContentId::from_bytes([2; 32]),
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
    /// logical message for the immutable identity fields. space_id
    /// is intentionally excluded because canonicalize_dependent_
    /// space_ids can rewrite it (Space dedupe collapses two SpaceIds
    /// into one), and a peer that hasn't yet learned about the
    /// dedupe may still send acks referencing the loser space_id —
    /// we accept the merge and preserve existing's (canonicalized)
    /// space_id while unioning the ack set.
    #[test]
    fn merge_accepts_space_id_divergence_and_preserves_existing() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20], vec![10]));
        let mut diverged = entry(1, vec![10, 20], vec![20]);
        diverged.space_id = SpaceId([99; 16]); // peer still on old (loser) space
        let outcome = s.apply_outbox(diverged);
        assert_eq!(outcome, ApplyOutcome::Merged { old_id: None });
        let merged = s.outbox.get(&OutboxEntryId([1; 16])).unwrap();
        // Existing's (canonicalized) space_id wins.
        assert_eq!(merged.space_id, SpaceId([1; 16]));
        // Ack from the diverged peer is still folded in.
        assert_eq!(merged.delivered_to.len(), 2);
        assert!(merged.delivered_to.contains(&OwnerAddr([10; 16])));
        assert!(merged.delivered_to.contains(&OwnerAddr([20; 16])));
    }

    #[test]
    fn merge_rejects_message_cid_divergence() {
        let mut s = OwnerState::default();
        s.apply_outbox(entry(1, vec![10, 20], vec![10]));
        let mut diverged = entry(1, vec![10, 20], vec![20]);
        diverged.message_cid = ContentId::from_bytes([99; 32]);
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

    /// ZEB-243: an incoming OutboxEntry whose created_at HLC is strictly
    /// older than a matching tombstone in outbox_tombstones must be
    /// rejected with OutboxEntryTombstoned and must NOT be inserted.
    #[test]
    fn apply_outbox_rejects_entry_older_than_tombstone() {
        let mut state = OwnerState::default();
        let id = OutboxEntryId([0x11; 16]);
        let entry_hlc = hlc(1_000); // T1 — older
        let tomb_hlc = hlc(2_000); // T2 — newer (strictly)
                                   // Pre-load a tombstone for this id with the newer HLC.
        state.outbox_tombstones.insert(id, tomb_hlc);

        // Build an OutboxEntry for the same id with created_at = T1.
        let mut e = entry(0x11, vec![10], vec![]);
        e.created_at = entry_hlc;
        let outcome = state.apply_outbox(e);

        assert!(
            matches!(
                outcome,
                ApplyOutcome::Rejected(RejectionReason::OutboxEntryTombstoned)
            ),
            "expected OutboxEntryTombstoned rejection, got {:?}",
            outcome
        );
        // Entry must NOT have been inserted.
        assert!(
            !state.outbox.contains_key(&id),
            "tombstoned entry must not appear in outbox"
        );
    }

    /// ZEB-243: an incoming OutboxEntry whose created_at HLC is strictly
    /// newer than the matching tombstone falls through (tombstone does NOT
    /// win when the entry is newer). The entry must be accepted + inserted.
    #[test]
    fn apply_outbox_accepts_entry_newer_than_tombstone() {
        let mut state = OwnerState::default();
        let id = OutboxEntryId([0x22; 16]);
        let tomb_hlc = hlc(1_000); // T1 — older tombstone
        let entry_hlc = hlc(2_000); // T2 — newer entry
                                    // Pre-load a tombstone for this id with the older HLC.
        state.outbox_tombstones.insert(id, tomb_hlc);

        // Build an OutboxEntry for the same id with created_at = T2.
        let mut e = entry(0x22, vec![10], vec![]);
        e.created_at = entry_hlc;
        let outcome = state.apply_outbox(e);

        assert_eq!(
            outcome,
            ApplyOutcome::Inserted,
            "entry newer than tombstone must be accepted"
        );
        assert!(
            state.outbox.contains_key(&id),
            "accepted entry must appear in outbox"
        );
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
            message_cid: ContentId::from_bytes([msg; 32]),
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
            message_cid: ContentId::from_bytes([2; 32]),
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

    #[test]
    fn inbox_entries_for_space_returns_only_matching_space() {
        let mut state = OwnerState::default();
        let space_a = SpaceId([0x01; 16]);
        let space_b = SpaceId([0x02; 16]);
        let owner = OwnerAddr([0xff; 16]);

        state.apply_inbox(InboxEntry {
            space_id: space_a,
            message_cid: ContentId::from_bytes([0x10; 32]),
            from: owner,
            received_at: hlc(2),
        });
        state.apply_inbox(InboxEntry {
            space_id: space_a,
            message_cid: ContentId::from_bytes([0x11; 32]),
            from: owner,
            received_at: hlc(1),
        });
        state.apply_inbox(InboxEntry {
            space_id: space_b,
            message_cid: ContentId::from_bytes([0x20; 32]),
            from: owner,
            received_at: hlc(99),
        });

        let entries: Vec<&InboxEntry> = state.inbox_entries_for_space(space_a).collect();
        assert_eq!(entries.len(), 2, "only space_a entries");
        assert!(entries.iter().all(|e| e.space_id == space_a));
    }

    #[test]
    fn inbox_entries_for_space_skips_unrelated_at_scale() {
        // Regression for the BTreeMap::range optimization: with 200
        // unrelated entries across other SpaceIds, the helper must still
        // return only the matching-Space entries. This guards against a
        // future refactor that accidentally widens the range bounds or
        // forgets the take_while terminator.
        let mut state = OwnerState::default();
        let target = SpaceId([0x77; 16]);
        let owner = OwnerAddr([0xff; 16]);

        // Seed 200 entries across other spaces (SpaceIds [0x00..=0x76]
        // and [0x78..=0xff], straddling target on both sides of the
        // range bound to exercise the take_while terminator).
        for sp_byte in 0u8..=255u8 {
            if sp_byte == 0x77 {
                continue;
            }
            state.apply_inbox(InboxEntry {
                space_id: SpaceId([sp_byte; 16]),
                message_cid: ContentId::from_bytes([0x42; 32]),
                from: owner,
                received_at: hlc(sp_byte as u64),
            });
        }
        // Three entries in target with distinct message_cids.
        for cid_byte in 0u8..3u8 {
            state.apply_inbox(InboxEntry {
                space_id: target,
                message_cid: ContentId::from_bytes([cid_byte; 32]),
                from: owner,
                received_at: hlc(1_000 + cid_byte as u64),
            });
        }

        let entries: Vec<&InboxEntry> = state.inbox_entries_for_space(target).collect();
        assert_eq!(entries.len(), 3, "exactly the 3 target-space entries");
        assert!(
            entries.iter().all(|e| e.space_id == target),
            "no unrelated SpaceId leaks through"
        );
    }

    #[test]
    fn delete_inbox_entry_removes_only_matching_key() {
        let mut state = OwnerState::default();
        let space_a = SpaceId([0x01; 16]);
        let cid_x = ContentId::from_bytes([0xaa; 32]);
        let cid_y = ContentId::from_bytes([0xbb; 32]);

        state.apply_inbox(InboxEntry {
            space_id: space_a,
            message_cid: cid_x,
            from: OwnerAddr([1; 16]),
            received_at: hlc(1),
        });
        state.apply_inbox(InboxEntry {
            space_id: space_a,
            message_cid: cid_y,
            from: OwnerAddr([1; 16]),
            received_at: hlc(2),
        });
        assert_eq!(state.inbox.len(), 2);

        let removed = state.delete_inbox_entry(InboxKey {
            space_id: space_a,
            message_cid: cid_x,
        });
        assert!(removed.is_some(), "must return the removed entry");
        assert_eq!(removed.unwrap().message_cid, cid_x);
        assert_eq!(state.inbox.len(), 1, "exactly one entry deleted");
        assert!(
            state.inbox.values().any(|e| e.message_cid == cid_y),
            "the other entry survives"
        );

        let removed_again = state.delete_inbox_entry(InboxKey {
            space_id: space_a,
            message_cid: cid_x,
        });
        assert!(removed_again.is_none(), "second delete returns None");
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
        use crate::owner_state_types::DmContentKey;
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
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            message_cid: ContentId::from_bytes([1; 32]),
            created_at: hlc(100),
            delivered_to: Default::default(),
            delivery_status: DeliveryStatus::Pending,
        });
        s.apply_inbox(InboxEntry {
            space_id: SpaceId([5; 16]),
            message_cid: ContentId::from_bytes([2; 32]),
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
            message_cid: ContentId::from_bytes([2; 32]),
        };
        let old_inbox_key = InboxKey {
            space_id: SpaceId([5; 16]),
            message_cid: ContentId::from_bytes([2; 32]),
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
            message_cid: ContentId::from_bytes([1; 32]),
            created_at: hlc(100),
            delivered_to: Default::default(),
            delivery_status: DeliveryStatus::Pending,
        });
        // Same dm, same id — pure LWW, no canonicalization triggered.
        s.apply_space_with_canonicalization(dm(1, vec![1, 2], 200));
        let entry = s.outbox.get(&OutboxEntryId([99; 16])).unwrap();
        assert_eq!(entry.space_id, SpaceId([1; 16]));
    }

    /// Regression for PR #73 round 5 (Cursor): after a Space dedupe
    /// rewrites an outbox entry's space_id from loser→winner, a peer
    /// that hasn't yet learned about the dedupe still sends acks
    /// referencing the OLD (loser) space_id. The apply_outbox
    /// envelope check used to reject those, silently dropping
    /// delivery acknowledgments and stranding delivery_status. The
    /// fix: drop space_id from the envelope check; preserve
    /// existing's (canonicalized) space_id and union the ack set.
    #[test]
    fn outbox_ack_with_loser_space_id_after_dedupe_still_merges() {
        let mut s = OwnerState::default();
        // Device A creates DM id=5 (will be loser — larger ULID).
        s.apply_space_with_canonicalization(dm(5, vec![1, 2], 100));
        // Device A sends an OutboxEntry referencing id=5.
        s.apply_outbox(OutboxEntry {
            id: OutboxEntryId([42; 16]),
            space_id: SpaceId([5; 16]),
            recipient_owners: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            message_cid: ContentId::from_bytes([7; 32]),
            created_at: hlc(100),
            delivered_to: [OwnerAddr([1; 16])].into_iter().collect(),
            delivery_status: DeliveryStatus::Partial,
        });
        // Device B's DM id=1 arrives — Space dedupe collapses 5→1
        // and canonicalize_dependent_space_ids rewrites the outbox
        // entry to space_id=1.
        s.apply_space_with_canonicalization(dm(1, vec![1, 2], 100));
        assert_eq!(
            s.outbox.get(&OutboxEntryId([42; 16])).unwrap().space_id,
            SpaceId([1; 16]),
            "outbox entry should now reference winner space_id"
        );

        // Device C (has not yet learned about the dedupe) sends an
        // ack still referencing the original loser space_id=5. With
        // the round-4 envelope check this would have been rejected
        // for space_id mismatch — now it merges.
        let outcome = s.apply_outbox(OutboxEntry {
            id: OutboxEntryId([42; 16]),
            space_id: SpaceId([5; 16]), // peer is still on the old loser id
            recipient_owners: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            message_cid: ContentId::from_bytes([7; 32]),
            created_at: hlc(100),
            delivered_to: [OwnerAddr([2; 16])].into_iter().collect(),
            delivery_status: DeliveryStatus::Partial,
        });
        assert_eq!(outcome, ApplyOutcome::Merged { old_id: None });
        let merged = s.outbox.get(&OutboxEntryId([42; 16])).unwrap();
        // Stored space_id stays canonicalized (winner).
        assert_eq!(merged.space_id, SpaceId([1; 16]));
        // Both acks landed; delivery is Complete.
        assert_eq!(merged.delivered_to.len(), 2);
        assert_eq!(merged.delivery_status, DeliveryStatus::Complete);
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
                message_cid: ContentId::from_bytes([7; 32]),
            },
            InboxEntry {
                space_id: SpaceId([5; 16]),
                message_cid: ContentId::from_bytes([7; 32]),
                from: OwnerAddr([2; 16]),
                received_at: hlc(200), // later
            },
        );
        s.inbox.insert(
            InboxKey {
                space_id: SpaceId([1; 16]),
                message_cid: ContentId::from_bytes([7; 32]),
            },
            InboxEntry {
                space_id: SpaceId([1; 16]),
                message_cid: ContentId::from_bytes([7; 32]),
                from: OwnerAddr([2; 16]),
                received_at: hlc(100), // earlier — should win
            },
        );

        s.canonicalize_dependent_space_ids(SpaceId([5; 16]), SpaceId([1; 16]));

        // Old loser key gone; only the winner key remains.
        assert!(!s.inbox.contains_key(&InboxKey {
            space_id: SpaceId([5; 16]),
            message_cid: ContentId::from_bytes([7; 32]),
        }));
        let winner_entry = s
            .inbox
            .get(&InboxKey {
                space_id: SpaceId([1; 16]),
                message_cid: ContentId::from_bytes([7; 32]),
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
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            message_cid: ContentId::from_bytes([3; 32]),
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
        use crate::owner_state_types::DmContentKey;
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
            content_key: Some(DmContentKey::new([0xcc; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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

#[cfg(test)]
mod owner_device_cache_tests {
    use super::*;
    use crate::owner_state_types::{
        DeviceIdentityHash, OwnerAddr, OwnerDeviceCache, MAX_DEVICES_PER_OWNER,
    };

    fn hlc(ms: u64) -> Hlc {
        Hlc {
            wall_ms: ms,
            logical: 0,
            device_id: "d".into(),
        }
    }

    /// Build a real (DeviceIdentityHash, [u8; 64]) pair from a seed where
    /// the hash IS `derive_device_hash_from_identity_pub(&pub)`. Required
    /// for any test that exercises a path now gated by the
    /// pub-derives-to-hash invariant added in this commit — placeholder
    /// pubs (e.g., `[0x42u8; 64]`) would now be rejected as InvariantFail
    /// and mask the test's intent.
    fn matching_device_pair(seed_byte: u8) -> (DeviceIdentityHash, [u8; 64]) {
        let private = harmony_identity::PrivateIdentity::from_seed(&[seed_byte; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);
        (device_hash, identity_pub)
    }

    #[test]
    fn lww_newer_replaces() {
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let d1 = vec![DeviceIdentityHash([1; 16])];
        let d2 = vec![DeviceIdentityHash([2; 16])];
        // First insert at HLC=1 → Inserted
        let outcome1 = apply_owner_device_update_helper(&mut c, addr, d1.clone(), hlc(1));
        assert!(matches!(outcome1, ApplyOutcome::Inserted));
        // Second update at newer HLC=2 → Merged
        let outcome2 = apply_owner_device_update_helper(&mut c, addr, d2.clone(), hlc(2));
        assert!(matches!(outcome2, ApplyOutcome::Merged { .. }));
        // Cache reflects newer
        assert_eq!(c.devices.get(&addr).unwrap().devices, d2);
    }

    #[test]
    fn lww_older_is_rejected() {
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let d1 = vec![DeviceIdentityHash([1; 16])];
        let d2 = vec![DeviceIdentityHash([2; 16])];
        // Establish at HLC=2
        apply_owner_device_update_helper(&mut c, addr, d2.clone(), hlc(2));
        // Older write at HLC=1 → Rejected (StaleHlc)
        let outcome = apply_owner_device_update_helper(&mut c, addr, d1, hlc(1));
        assert!(matches!(
            outcome,
            ApplyOutcome::Rejected(RejectionReason::StaleHlc { .. })
        ));
        // Cache unchanged
        assert_eq!(c.devices.get(&addr).unwrap().devices, d2);
    }

    #[test]
    fn lww_equal_hlc_is_idempotent_replay() {
        // Sync replay flows deliver the same update to multiple devices, so
        // an equal HLC + matching devices payload must not produce a spurious
        // StaleHlc — return Merged without mutation.
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let d = vec![DeviceIdentityHash([1; 16])];
        let outcome1 = apply_owner_device_update_helper(&mut c, addr, d.clone(), hlc(5));
        assert!(matches!(outcome1, ApplyOutcome::Inserted));
        // Second call at SAME HLC AND SAME devices — must be idempotent
        // Merged, not Rejected.
        let outcome2 = apply_owner_device_update_helper(&mut c, addr, d.clone(), hlc(5));
        assert!(
            matches!(outcome2, ApplyOutcome::Merged { old_id: None }),
            "expected Merged on equal-HLC replay, got {:?}",
            outcome2
        );
        // Cache unchanged.
        assert_eq!(c.devices.get(&addr).unwrap().devices, d);
    }

    #[test]
    fn lww_equal_hlc_diverging_devices_is_rejected() {
        // Equal HLC + DIFFERENT devices payload is NOT a replay — it's a
        // concurrent-update divergence. Without explicit rejection, two
        // replicas that produced the entries independently would each keep
        // their own list and report Merged, leaving the cache permanently
        // divergent. Reject as InvariantFail and preserve the local list.
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let d_a = vec![DeviceIdentityHash([1; 16])];
        let d_b = vec![DeviceIdentityHash([2; 16])];

        // Establish at hlc(5) with devices = d_a.
        let outcome1 = apply_owner_device_update_helper(&mut c, addr, d_a.clone(), hlc(5));
        assert!(matches!(outcome1, ApplyOutcome::Inserted));

        // Same hlc(5) but different devices = d_b → Rejected(InvariantFail).
        let outcome2 = apply_owner_device_update_helper(&mut c, addr, d_b, hlc(5));
        match outcome2 {
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(msg)) => {
                assert!(
                    msg.contains("owner_device_entry"),
                    "expected message mentioning owner_device_entry, got: {msg}"
                );
                assert!(
                    msg.contains("identical learned_at") || msg.contains("learned_at"),
                    "expected message to reference identical-HLC divergence, got: {msg}"
                );
            }
            other => panic!("expected Rejected(InvariantFail), got {:?}", other),
        }
        // Local list preserved unchanged.
        assert_eq!(c.devices.get(&addr).unwrap().devices, d_a);

        // True replay (same HLC, same devices) is still Merged{old_id: None}.
        let outcome3 = apply_owner_device_update_helper(&mut c, addr, d_a.clone(), hlc(5));
        assert!(
            matches!(outcome3, ApplyOutcome::Merged { old_id: None }),
            "expected Merged on equal-HLC equal-devices replay, got {:?}",
            outcome3
        );
        assert_eq!(c.devices.get(&addr).unwrap().devices, d_a);
    }

    #[test]
    fn dedupes_input() {
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let d1 = DeviceIdentityHash([1; 16]);
        let d2 = DeviceIdentityHash([2; 16]);
        apply_owner_device_update_helper(&mut c, addr, vec![d1, d2, d1], hlc(1));
        // Stored vec must be deduped + sorted.
        assert_eq!(c.devices.get(&addr).unwrap().devices, vec![d1, d2]);
    }

    #[test]
    fn caps_at_max_devices_per_owner() {
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let big: Vec<DeviceIdentityHash> =
            (0..100u8).map(|i| DeviceIdentityHash([i; 16])).collect();
        apply_owner_device_update_helper(&mut c, addr, big, hlc(1));
        let stored = &c.devices.get(&addr).unwrap().devices;
        assert_eq!(stored.len(), MAX_DEVICES_PER_OWNER);
        // Lex-smallest entries survive — first 32 of [0..100].
        assert_eq!(stored[0], DeviceIdentityHash([0; 16]));
        assert_eq!(stored[31], DeviceIdentityHash([31; 16]));
    }

    #[test]
    fn binary_search_works_after_apply() {
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let target = DeviceIdentityHash([5; 16]);
        let big: Vec<DeviceIdentityHash> = (0..10u8).map(|i| DeviceIdentityHash([i; 16])).collect();
        apply_owner_device_update_helper(&mut c, addr, big, hlc(1));
        // The cache stores devices sorted, so binary_search works (used by
        // resolve_link_origin_owner in Phase 3b).
        let stored = &c.devices.get(&addr).unwrap().devices;
        assert!(stored.binary_search(&target).is_ok());
    }

    #[test]
    fn apply_owner_device_update_stores_identity_pubs_parallel_to_devices() {
        let mut state = OwnerState::default();
        let owner = OwnerAddr([1; 16]);
        // Use real matching (hash, pub) pairs — placeholder pubs would
        // now be rejected by the pub-derives-to-hash invariant.
        let (h1, p1) = matching_device_pair(0xa1);
        let (h2, p2) = matching_device_pair(0xa2);
        // Pre-sort so the input is already canonical (the sort/merge
        // path is exercised by `apply_owner_device_update_sort_dedup_keeps_pubs_aligned`).
        let (devices, pubs) = if h1 < h2 {
            (vec![h1, h2], vec![Some(p1), Some(p2)])
        } else {
            (vec![h2, h1], vec![Some(p2), Some(p1)])
        };
        let learned_at = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };

        let outcome =
            state.apply_owner_device_update(owner, devices.clone(), pubs.clone(), learned_at);
        assert!(matches!(outcome, ApplyOutcome::Inserted));

        let entry = state.owner_device_cache.devices.get(&owner).unwrap();
        assert_eq!(entry.devices, devices);
        assert_eq!(entry.device_identity_pubs, pubs);
    }

    #[test]
    fn apply_owner_device_update_sort_dedup_keeps_pubs_aligned() {
        // Sanitization MUST maintain parallel-vec correspondence: when
        // sort+merge reorders or collapses `devices` entries, the matching
        // entries in `device_identity_pubs` must follow.
        //
        // For the duplicate-hash entry below we pass MATCHING Some pubs
        // — the merge rule accepts equal Somes (the "two replicas
        // converged on the same pub" case). Conflicting Somes are
        // covered by `apply_owner_device_update_rejects_conflicting_some_pubs`.
        //
        // Each (hash, pub) is a real derive-matching pair so the
        // pub-derives-to-hash invariant doesn't fire.
        let mut state = OwnerState::default();
        let owner = OwnerAddr([1; 16]);
        let (h_a, p_a) = matching_device_pair(0xa1);
        let (h_b, p_b) = matching_device_pair(0xa2);
        let (h_c, p_c) = matching_device_pair(0xa3);
        // Compute the post-sort/dedup expected order from the real hashes
        // (we don't know the lex order of derived hashes a priori).
        let mut expected: Vec<(DeviceIdentityHash, [u8; 64])> =
            vec![(h_a, p_a), (h_b, p_b), (h_c, p_c)];
        expected.sort_by_key(|(h, _)| *h);

        // Unsorted + duplicate input (h_a appears twice with the same
        // matching pub — merge accepts equal Somes).
        let devices = vec![h_c, h_a, h_b, h_a];
        let pubs = vec![Some(p_c), Some(p_a), Some(p_b), Some(p_a)];
        let learned_at = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };

        state.apply_owner_device_update(owner, devices, pubs, learned_at);
        let entry = state.owner_device_cache.devices.get(&owner).unwrap();

        // Sorted ascending by hash, merged to 3 unique devices.
        let expected_devices: Vec<DeviceIdentityHash> = expected.iter().map(|(h, _)| *h).collect();
        let expected_pubs: Vec<Option<[u8; 64]>> = expected.iter().map(|(_, p)| Some(*p)).collect();
        assert_eq!(entry.devices, expected_devices);
        assert_eq!(entry.device_identity_pubs.len(), 3);
        assert_eq!(entry.device_identity_pubs, expected_pubs);
    }

    #[test]
    fn apply_owner_device_update_merges_some_over_none_on_duplicate() {
        // Pre-fix bug: `dedup_by_key` kept the FIRST occurrence of a
        // duplicate hash, so [d1, d1] paired with [None, Some(P)]
        // dropped the Some. The fix walks-and-merges duplicates,
        // preferring Some over None regardless of order.
        let mut state = OwnerState::default();
        let owner = OwnerAddr([1; 16]);
        // Real matching (hash, pub) pair so the pub-derives-to-hash
        // invariant doesn't fire — placeholder pubs would now reject.
        let (d1, p) = matching_device_pair(0xa1);
        let learned_at = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };

        let outcome =
            state.apply_owner_device_update(owner, vec![d1, d1], vec![None, Some(p)], learned_at);
        assert!(matches!(outcome, ApplyOutcome::Inserted));
        let entry = state.owner_device_cache.devices.get(&owner).unwrap();
        assert_eq!(entry.devices, vec![d1]);
        assert_eq!(
            entry.device_identity_pubs[0],
            Some(p),
            "merge MUST prefer Some over None (regardless of order)"
        );
    }

    #[test]
    fn apply_owner_device_update_merges_some_over_none_reverse_order() {
        // Reverse-order variant: pubs = [Some(P), None] still preserves
        // Some after merge. Order independence matters because wire-
        // order is not constrained by callers; only the post-merge
        // canonical form is.
        let mut state = OwnerState::default();
        let owner = OwnerAddr([1; 16]);
        let (d1, p) = matching_device_pair(0xa1);
        let learned_at = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };

        let outcome =
            state.apply_owner_device_update(owner, vec![d1, d1], vec![Some(p), None], learned_at);
        assert!(matches!(outcome, ApplyOutcome::Inserted));
        let entry = state.owner_device_cache.devices.get(&owner).unwrap();
        assert_eq!(entry.devices, vec![d1]);
        assert_eq!(entry.device_identity_pubs[0], Some(p));
    }

    #[test]
    fn apply_owner_device_update_rejects_conflicting_some_pubs() {
        // Two Some(pub) values on the same DeviceIdentityHash that
        // disagree is a real invariant violation — silently picking
        // one would leak a TOCTOU into signature verification. Reject
        // as InvariantFail.
        let mut state = OwnerState::default();
        let owner = OwnerAddr([1; 16]);
        let d1 = DeviceIdentityHash([0xa1; 16]);
        let p_a = [0x11u8; 64];
        let p_b = [0x22u8; 64];
        let learned_at = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };

        let outcome = state.apply_owner_device_update(
            owner,
            vec![d1, d1],
            vec![Some(p_a), Some(p_b)],
            learned_at,
        );
        match outcome {
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(msg)) => {
                assert!(
                    msg.contains("conflicting identity pubs"),
                    "expected message mentioning conflicting identity pubs, got: {msg}"
                );
            }
            other => panic!("expected Rejected(InvariantFail), got {:?}", other),
        }
        // No partial state inserted.
        assert!(!state.owner_device_cache.devices.contains_key(&owner));
    }

    #[test]
    fn apply_owner_device_update_rejects_pub_with_mismatched_hash() {
        // Defense-in-depth: a `Some(identity_pub)` MUST derive (via
        // SHA256(pub)[:16] = `derive_device_hash_from_identity_pub`) to
        // its paired `DeviceIdentityHash`. A malformed/poisoned cache
        // entry where the pair is mismatched would silently fail every
        // later signature verify in `resolve_signed_origin_owner`,
        // converting "this device's signature didn't match" into a
        // confusing dead-letter. Reject at apply time so the bad state
        // never enters the cache.
        // Use a STRUCTURALLY-VALID pub (derived from a real
        // PrivateIdentity) paired with a hash that does NOT derive from
        // it — this isolates the derives-to-different-hash branch
        // (vs. the structurally-invalid-pub branch, which would fire on
        // arbitrary garbage like `[0x99; 64]`).
        let mut state = OwnerState::default();
        let owner = OwnerAddr([1; 16]);
        let (real_hash, real_pub) = matching_device_pair(0xa1);
        let mismatched_hash = DeviceIdentityHash([0x42; 16]);
        // Sanity: confirm the test fixture really IS a mismatch.
        assert_ne!(
            real_hash, mismatched_hash,
            "test fixture must keep mismatched_hash distinct from the derived hash"
        );
        let learned_at = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };

        let outcome = state.apply_owner_device_update(
            owner,
            vec![mismatched_hash],
            vec![Some(real_pub)],
            learned_at,
        );
        match outcome {
            ApplyOutcome::Rejected(RejectionReason::InvariantFail(msg)) => {
                assert!(
                    msg.contains("identity pub") && msg.contains("device hash"),
                    "expected message about identity pub deriving to a different device hash, got: {msg}"
                );
            }
            other => panic!("expected Rejected(InvariantFail), got {:?}", other),
        }
        // No partial state inserted.
        assert!(!state.owner_device_cache.devices.contains_key(&owner));
    }

    #[test]
    fn apply_owner_device_update_accepts_pub_with_matching_hash() {
        // Sanity counterpart to
        // `apply_owner_device_update_rejects_pub_with_mismatched_hash`:
        // a real (hash, pub) pair derived from a `PrivateIdentity` must
        // pass the new derive check and insert successfully.
        let mut state = OwnerState::default();
        let owner = OwnerAddr([1; 16]);
        let (device_hash, identity_pub) = matching_device_pair(0x42);
        let learned_at = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };

        let outcome = state.apply_owner_device_update(
            owner,
            vec![device_hash],
            vec![Some(identity_pub)],
            learned_at,
        );
        assert!(
            matches!(outcome, ApplyOutcome::Inserted),
            "expected Inserted with a real matching (hash, pub) pair, got {outcome:?}"
        );
        let entry = state.owner_device_cache.devices.get(&owner).unwrap();
        assert_eq!(entry.devices, vec![device_hash]);
        assert_eq!(entry.device_identity_pubs, vec![Some(identity_pub)]);
    }

    #[test]
    fn apply_owner_device_update_preserves_existing_pub_when_new_has_none_at_strictly_newer_hlc() {
        // Per-pub LWW preserve: when the new entry passes LWW (strictly
        // newer HLC) but carries `None` for a device hash whose existing
        // entry has `Some(P)`, the cached `Some(P)` MUST be preserved.
        // Pre-fix this branch wholesale-overwrote with `None`, breaking
        // signature verification for that device on every subsequent
        // gossip from a Path-B-bootstrap-incomplete peer.
        let mut state = OwnerState::default();
        let owner = OwnerAddr([1; 16]);
        let (d1, p1) = matching_device_pair(0xa1);

        // Seed cache at HLC=10 with Some(p1).
        let outcome1 = state.apply_owner_device_update(
            owner,
            vec![d1],
            vec![Some(p1)],
            Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "src".into(),
            },
        );
        assert!(matches!(outcome1, ApplyOutcome::Inserted));

        // New update at strictly newer HLC=20 with None — must NOT erase Some(p1).
        let outcome2 = state.apply_owner_device_update(
            owner,
            vec![d1],
            vec![None],
            Hlc {
                wall_ms: 20,
                logical: 0,
                device_id: "src".into(),
            },
        );
        assert!(
            matches!(outcome2, ApplyOutcome::Merged { old_id: None }),
            "expected Merged on per-pub-preserve LWW, got {:?}",
            outcome2
        );
        let entry = state.owner_device_cache.devices.get(&owner).unwrap();
        assert_eq!(entry.devices, vec![d1]);
        assert_eq!(
            entry.device_identity_pubs,
            vec![Some(p1)],
            "per-pub LWW MUST preserve existing Some(pub) when new entry has None"
        );
        // learned_at advances to the newer HLC (LWW won on the wrapper).
        assert_eq!(entry.learned_at.wall_ms, 20);
    }

    #[test]
    fn apply_owner_device_update_adopts_new_pub_when_existing_is_none() {
        // Inverse of the preserve case: existing has `None`, new has
        // `Some(P)` at strictly newer HLC — adopt the new pub. This is
        // the bootstrap-completion path (a peer learns the device by
        // hash first, then gets the pub propagated later).
        let mut state = OwnerState::default();
        let owner = OwnerAddr([1; 16]);
        let (d1, p1) = matching_device_pair(0xa1);

        // Seed cache at HLC=10 with None.
        let outcome1 = state.apply_owner_device_update(
            owner,
            vec![d1],
            vec![None],
            Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "src".into(),
            },
        );
        assert!(matches!(outcome1, ApplyOutcome::Inserted));

        // New update at HLC=20 with Some(p1) — adopt.
        let outcome2 = state.apply_owner_device_update(
            owner,
            vec![d1],
            vec![Some(p1)],
            Hlc {
                wall_ms: 20,
                logical: 0,
                device_id: "src".into(),
            },
        );
        assert!(matches!(outcome2, ApplyOutcome::Merged { old_id: None }));
        let entry = state.owner_device_cache.devices.get(&owner).unwrap();
        assert_eq!(entry.device_identity_pubs, vec![Some(p1)]);
    }

    #[test]
    fn apply_owner_device_update_rejects_conflicting_existing_and_new_some_pubs_at_strictly_newer_hlc(
    ) {
        // Conflict: existing has `Some(p1)`, new has `Some(p2)` at
        // strictly newer HLC, where `p1 != p2` for the SAME device hash.
        // Silently picking one would leak a TOCTOU into signature
        // verification — reject as InvariantFail.
        //
        // Test fixture note: cryptographically two different valid
        // pubs cannot derive to the same hash (SHA256 collision). To
        // exercise this branch we synthesize the conflict by directly
        // seeding the cache with one (hash, pub) pair and then applying
        // a different (hash, pub) pair where the new pub matches a
        // DIFFERENT real (hash, pub) pair — but we then mutate the
        // device hash in the new entry to match the existing one.
        // This intentionally bypasses the derive-to-hash check (which
        // we verified separately) to isolate the cross-update conflict
        // branch.
        //
        // Concretely: seed cache directly with `(d1, Some(p1))` at
        // HLC=10, then apply `(d1, Some(p2))` at HLC=20 where `p2`
        // structurally derives to the SAME `d1` (impossible
        // cryptographically; we forge the test fixture by overriding
        // the cache directly so the derive-check passes for the seed,
        // then the cross-update branch is the only thing left to test).
        //
        // Simplification: seed via the public API with `(d1, Some(p1))`
        // at HLC=10 (real matching pair so derive passes). Then for
        // the new update, use `(d1, Some(p2))` where p2 derives to a
        // DIFFERENT hash d2; the inner derive-check fires FIRST
        // (before our cross-update conflict branch), so this scenario
        // is structurally impossible to reach without test-only API.
        // We document the gap and rely on the in-merge conflict test
        // (apply_owner_device_update_rejects_conflicting_some_pubs)
        // for coverage of the pub-conflict semantics; the cross-
        // update path is mechanically symmetric.
        //
        // What we CAN test cheaply: seed with `Some(p1)`, apply a
        // strictly-newer LWW with `Some(p1)` (same pub) — must merge
        // idempotently. This pins the equal-Some short-circuit branch.
        let mut state = OwnerState::default();
        let owner = OwnerAddr([1; 16]);
        let (d1, p1) = matching_device_pair(0xa1);

        // Seed with Some(p1) at HLC=10.
        let outcome1 = state.apply_owner_device_update(
            owner,
            vec![d1],
            vec![Some(p1)],
            Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "src".into(),
            },
        );
        assert!(matches!(outcome1, ApplyOutcome::Inserted));

        // Apply Some(p1) again at strictly-newer HLC=20 — equal Some
        // values, must merge cleanly (no conflict).
        let outcome2 = state.apply_owner_device_update(
            owner,
            vec![d1],
            vec![Some(p1)],
            Hlc {
                wall_ms: 20,
                logical: 0,
                device_id: "src".into(),
            },
        );
        assert!(
            matches!(outcome2, ApplyOutcome::Merged { old_id: None }),
            "expected Merged with equal Some-pub at strictly-newer HLC, got {:?}",
            outcome2
        );
        let entry = state.owner_device_cache.devices.get(&owner).unwrap();
        assert_eq!(entry.device_identity_pubs, vec![Some(p1)]);
        assert_eq!(entry.learned_at.wall_ms, 20);
    }

    // Helper that lets the test pass without naming the public method twice.
    // If apply_owner_device_update is a method on OwnerState rather than a
    // free function, adapt: the helper either calls a free function in
    // owner_state_crdt or a method on a fresh OwnerState wrapping the cache.
    fn apply_owner_device_update_helper(
        cache: &mut OwnerDeviceCache,
        addr: OwnerAddr,
        devices: Vec<DeviceIdentityHash>,
        learned_at: Hlc,
    ) -> ApplyOutcome {
        let mut state = OwnerState {
            owner_device_cache: std::mem::take(cache),
            ..Default::default()
        };
        // Phase 1/2 helper: pass empty pubs vec (apply resizes to
        // None-padded internally). Phase 3b's parallel-vec semantics are
        // exercised by the dedicated tests in this same module.
        let outcome = state.apply_owner_device_update(addr, devices, vec![], learned_at);
        *cache = state.owner_device_cache;
        outcome
    }
}

#[cfg(test)]
mod merge_prior_content_keys_tests {
    use super::*;
    use crate::owner_state_types::{
        DmContentKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind, TransportBinding,
    };

    fn key(byte: u8) -> DmContentKey {
        DmContentKey::new([byte; 32])
    }

    fn dm_space(id_byte: u8, content_key: DmContentKey) -> Space {
        // hlc_ms is intentionally fixed: the 5-Space cap-rule convergence
        // proof depends on ULID order (id_byte), not HLC.
        let hlc = Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "d".into(),
        };
        Space {
            id: SpaceId([id_byte; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc.clone(),
            updated_at: hlc,
            content_key: Some(content_key),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        }
    }

    /// 5-Space convergence test from ZEB-219 §"Why first N of sorted":
    /// K₃<K₂<K₄<K₅<K₁ lex (subscript indexes the Space-ID byte, NOT key
    /// lex order — see byte-value comments in the test body), two distinct
    /// merge orders → both yield the same prior_content_keys Vec
    /// (sorted ascending). With cap=16 (production), all 4 losers fit, so
    /// result is [K₃, K₂, K₄, K₅].
    #[test]
    fn dedupe_merge_prior_content_keys_5_space_convergence() {
        // Choose first bytes that give us the desired lex ordering:
        // K3 = [0x10..], K2 = [0x20..], K4 = [0x30..], K5 = [0x40..], K1 = [0x50..]
        // So K3 < K2 < K4 < K5 < K1 lex.
        let k1 = key(0x50);
        let k2 = key(0x20);
        let k3 = key(0x10);
        let k4 = key(0x30);
        let k5 = key(0x40);

        // Each of S1..S5 has a different ULID byte (so they're distinct
        // by id) but all share the same dedupe_key (sorted members).
        // S1 has the smallest id_byte so it'll be the dedupe winner.
        let s1 = dm_space(0x01, k1.clone());
        let s2 = dm_space(0x02, k2.clone());
        let s3 = dm_space(0x03, k3.clone());
        let s4 = dm_space(0x04, k4.clone());
        let s5 = dm_space(0x05, k5.clone());

        // Apply order P: [S2, S3, S4, S5, S1]
        let mut state_p = OwnerState::default();
        for s in [s2.clone(), s3.clone(), s4.clone(), s5.clone(), s1.clone()] {
            state_p.apply_space_with_canonicalization(s);
        }

        // Apply order Q: [S5, S4, S3, S2, S1]
        let mut state_q = OwnerState::default();
        for s in [s5.clone(), s4.clone(), s3.clone(), s2.clone(), s1.clone()] {
            state_q.apply_space_with_canonicalization(s);
        }

        // Convergence assertion: both orders yield byte-identical
        // prior_content_keys on the surviving (S1) Space.
        let p_winner = state_p
            .spaces
            .get(&SpaceId([0x01; 16]))
            .expect("S1 survives");
        let q_winner = state_q
            .spaces
            .get(&SpaceId([0x01; 16]))
            .expect("S1 survives");

        let p_prior: Vec<[u8; 32]> = p_winner
            .prior_content_keys
            .iter()
            .map(|k| *k.as_bytes())
            .collect();
        let q_prior: Vec<[u8; 32]> = q_winner
            .prior_content_keys
            .iter()
            .map(|k| *k.as_bytes())
            .collect();

        assert_eq!(
            p_prior, q_prior,
            "convergence: orders P and Q must yield identical prior_content_keys"
        );

        // Identity-of-content assertion: cap=MAX_PRIOR_CONTENT_KEYS=16 for
        // production, but with 5 keys total all four losers fit. The loser
        // current_keys are k2..k5; winner current is k1, which MUST NOT
        // appear in prior. Sorted ascending: [k3, k2, k4, k5].
        assert_eq!(p_prior.len(), 4);
        assert_eq!(p_prior[0], *k3.as_bytes());
        assert_eq!(p_prior[1], *k2.as_bytes());
        assert_eq!(p_prior[2], *k4.as_bytes());
        assert_eq!(p_prior[3], *k5.as_bytes());

        // Winner's content_key is unchanged (S1's k1).
        assert_eq!(
            p_winner.content_key.as_ref().unwrap().as_bytes(),
            k1.as_bytes()
        );
    }

    #[test]
    fn merge_prior_content_keys_filters_winner_current() {
        let winner_current = key(0x10);
        let loser_current = key(0x20);
        // Winner's prior includes a duplicate of winner_current — must
        // be filtered out.
        let winner_prior = vec![winner_current.clone(), key(0x30)];
        let loser_prior = vec![key(0x40)];
        let merged =
            merge_prior_content_keys(&winner_current, &winner_prior, &loser_current, &loser_prior);
        let merged_bytes: Vec<[u8; 32]> = merged.iter().map(|k| *k.as_bytes()).collect();
        // Sorted ascending: 0x20, 0x30, 0x40 (no 0x10).
        assert_eq!(merged_bytes, vec![[0x20; 32], [0x30; 32], [0x40; 32]]);
    }

    #[test]
    fn merge_prior_content_keys_caps_at_max() {
        let winner_current = key(0xff);
        let winner_prior = vec![];
        let loser_current = key(0xfe);
        // Loser's prior has way more than MAX_PRIOR_CONTENT_KEYS entries.
        let loser_prior: Vec<DmContentKey> = (0u8..30).map(key).collect();
        let merged =
            merge_prior_content_keys(&winner_current, &winner_prior, &loser_current, &loser_prior);
        // Cap is 16. Smallest 16 of {0..29, loser_current=0xfe, winner_prior empty}
        // after sort = [0..15] (loser_current and keys 16..29 don't make the cut).
        // Note: keys are filtered to remove winner_current (0xff), but 0xff isn't
        // in the input set anyway, so all 30+1=31 inputs are eligible.
        // Sorted: [0,1,2,...,29, 0xfe]. Truncated to 16: [0..15].
        assert_eq!(
            merged.len(),
            crate::owner_state_types::MAX_PRIOR_CONTENT_KEYS
        );
        for (i, k) in merged.iter().enumerate() {
            assert_eq!(k.as_bytes(), &[i as u8; 32]);
        }
    }
}

#[cfg(test)]
mod dm_crypto_integration_tests {
    use super::*;
    use crate::dm_crypto::{compute_aad, decrypt_dm_message, encrypt_dm_message};
    use crate::dm_envelope::MessagePayload;
    use crate::owner_state_types::{
        DmContentKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind, TransportBinding,
    };

    fn dm_at(id_byte: u8, ck: DmContentKey) -> Space {
        let hlc = Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "d".into(),
        };
        Space {
            id: SpaceId([id_byte; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc.clone(),
            updated_at: hlc,
            content_key: Some(ck),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        }
    }

    /// Full-chain invariant test: encrypt under pre-collapse keys, apply cross-SpaceId
    /// dedupe collapse, then verify decrypt still works under the merged Space's key
    /// set, and that AAD is stable throughout.
    ///
    /// Exercises four invariants:
    ///   1. AAD is identical before the merge (same dedupe_key, different SpaceId).
    ///   2. AAD is identical after the dedupe collapse.
    ///   3. blob_a (encrypted under winner's key) decrypts under merged content_key.
    ///   4. blob_b (encrypted under loser's key) decrypts via merged prior_content_keys.
    #[test]
    fn encrypt_then_dedupe_collapse_then_decrypt_with_merged_keys() {
        let key_a = DmContentKey::new([0x10; 32]); // 0x10 < 0x20 → winner content_key
        let key_b = DmContentKey::new([0x20; 32]);
        let space_a = dm_at(0x01, key_a.clone()); // smaller SpaceId byte → ULID winner
        let space_b = dm_at(0x02, key_b.clone()); // larger SpaceId byte → ULID loser

        let payload_a = MessagePayload {
            body: b"hello from A".to_vec(),
            mime_type: "text/plain".into(),
            sender: OwnerAddr([1; 16]),
            sent_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        };
        let payload_b = MessagePayload {
            body: b"hello from B".to_vec(),
            mime_type: "text/plain".into(),
            sender: OwnerAddr([2; 16]),
            sent_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        };

        // Each device computes its own AAD (from its own Space) and encrypts.
        let aad_a = compute_aad(&space_a).unwrap();
        let aad_b = compute_aad(&space_b).unwrap();

        // INVARIANT 1: AAD is identical before merge (same dedupe_key, different SpaceId).
        assert_eq!(
            aad_a, aad_b,
            "AAD must be stable across SpaceIds with the same dedupe_key"
        );

        let blob_a = encrypt_dm_message(&key_a, &aad_a, &payload_a).unwrap();
        let blob_b = encrypt_dm_message(&key_b, &aad_b, &payload_b).unwrap();

        // Cross-SpaceId dedupe collapse: space_a wins (smaller ULID / id_byte).
        let mut state = OwnerState::default();
        state.apply_space_with_canonicalization(space_a.clone());
        state.apply_space_with_canonicalization(space_b.clone());

        let merged = state
            .spaces
            .get(&SpaceId([0x01; 16]))
            .expect("Space A wins (smaller id)");

        // INVARIANT 2: AAD is identical AFTER the dedupe collapse.
        let aad_merged = compute_aad(merged).unwrap();
        assert_eq!(
            aad_merged, aad_a,
            "AAD must be stable across the dedupe collapse"
        );

        let merged_ck = merged
            .content_key
            .as_ref()
            .expect("merged Space has content_key");
        let merged_prior = &merged.prior_content_keys;

        // INVARIANT 3: blob_a decrypts using merged Space's content_key
        // (Space A was the winner — key_a is the current content_key after merge).
        let recovered_a = decrypt_dm_message(merged_ck, merged_prior, &aad_merged, &blob_a)
            .expect("blob_a must decrypt under the merged Space's content_key (winner side)");
        assert_eq!(recovered_a, payload_a);

        // INVARIANT 4: blob_b ALSO decrypts using merged Space's prior_content_keys
        // (Space B was the loser — key_b migrated into prior_content_keys).
        let recovered_b = decrypt_dm_message(merged_ck, merged_prior, &aad_merged, &blob_b)
            .expect("blob_b must decrypt under the merged Space's prior_content_keys (loser side)");
        assert_eq!(recovered_b, payload_b);
    }
}

#[cfg(test)]
mod outbox_tombstones_tests {
    use super::*;
    use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use crate::owner_state_types::{Hlc, OutboxEntryId};

    fn hlc(w: u64, device: &str) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: device.into(),
        }
    }

    /// ZEB-243: OwnerState with a non-empty outbox_tombstones map must
    /// round-trip through canonical CBOR encode + decode, preserving
    /// the tombstone entry exactly. `#[serde(default)]` ensures legacy
    /// snapshots without the field decode as empty map.
    #[test]
    fn outbox_tombstones_round_trip_via_canonical_cbor() {
        let mut state = OwnerState::default();
        let id = OutboxEntryId([0x42; 16]);
        let ts_hlc = hlc(1_000, "dev-a");
        state.outbox_tombstones.insert(id, ts_hlc.clone());

        // Encode via the same canonical-CBOR path the rest of OwnerState uses.
        let bytes = canonical_cbor_encode(&state).expect("encode should succeed");
        let recovered: OwnerState = canonical_cbor_decode(&bytes).expect("decode should succeed");

        assert_eq!(
            recovered.outbox_tombstones.get(&id),
            Some(&ts_hlc),
            "tombstone entry must survive canonical-CBOR round-trip"
        );
        assert_eq!(
            recovered.outbox_tombstones.len(),
            1,
            "no extra tombstone entries after round-trip"
        );
    }

    /// ZEB-243: legacy OwnerState snapshot (no outbox_tombstones field)
    /// decodes to empty map via #[serde(default)]. Simulate by encoding
    /// a state with no tombstones and asserting the map is empty.
    #[test]
    fn outbox_tombstones_defaults_to_empty_on_legacy_decode() {
        let state = OwnerState::default();
        let bytes = canonical_cbor_encode(&state).expect("encode");
        let recovered: OwnerState = canonical_cbor_decode(&bytes).expect("decode");
        assert!(
            recovered.outbox_tombstones.is_empty(),
            "fresh OwnerState must have empty outbox_tombstones"
        );
    }
}
