//! Per-community state CRDT — Phase 2 of ZEB-217 Sub-C.
//!
//! `CommunityState` holds the append-only signed event log for one
//! community, keyed by EventId. Mirrors the SHAPE of
//! `crate::owner_state_crdt::OwnerState` but at per-community
//! granularity — one `CommunityState` per joined community.
//!
//! Events arrive partial-ordered from DAG-sync; ordering for replay
//! is `event_sort_key` ascending. The materialized view (members +
//! power_levels) is computed on demand and cached with a version
//! counter that bumps on every successful insert.
//!
//! Wire format: canonical CBOR with the same-length-keys invariant
//! at this nesting level — both field codes (`ci` for community_id,
//! `ev` for events) are 2 chars.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::community_membership::{
    materialize, prior_state_at_event, verify_event, EventId, MaterializedMembership,
    SignedMembershipEvent, VerifyContext, VerifyError,
};
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{OwnerAddr, SpaceId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityState {
    /// The community this state belongs to. Persisted in the wire form
    /// so that a misrouted blob (wrong file, wrong ContentStore key) is
    /// rejected at decode-time rather than silently materialized into
    /// the wrong community's view.
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    /// Append-only signed event log, keyed by EventId. BTreeMap (not
    /// HashMap) so iteration order is deterministic across replicas —
    /// canonical CBOR encoding requires a stable order.
    #[serde(rename = "ev")]
    pub events: BTreeMap<EventId, SignedMembershipEvent>,
}

impl CommunityState {
    pub fn new(community_id: SpaceId) -> Self {
        Self {
            community_id,
            events: BTreeMap::new(),
        }
    }
}

impl CanonicalPayloadSealed for CommunityState {}
impl CanonicalPayload for CommunityState {}

/// Outcome of inserting one event into `CommunityState`.
///
/// Distinguishes the three meaningful states so callers (sync layer,
/// IPC layer, tests) can react appropriately:
/// - Inserted: event was new, verified, and now lives in the log
/// - AlreadyKnown: an event with this id was already in the log; the
///   sync layer should treat this as a no-op (NOT an error — DAG-sync
///   delivers duplicates by design)
/// - Rejected: verification failed; the wrapped VerifyError says why
#[derive(Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    AlreadyKnown,
    Rejected(VerifyError),
}

impl CommunityState {
    /// Insert a `SignedMembershipEvent` after running `verify_event`
    /// against the current materialized state. The state used for
    /// authorization is computed via `prior_state_at_event` so the
    /// `event_sort_key` comparator is shared with `materialize` and
    /// no caller can drift.
    ///
    /// Idempotent on duplicate EventIds — DAG-sync delivers the same
    /// event multiple times by design (e.g., when a peer re-publishes
    /// a state-root that includes events we already have). Returning
    /// `AlreadyKnown` rather than `Inserted` lets callers skip the
    /// cache-invalidation work.
    pub fn insert_event(
        &mut self,
        event: SignedMembershipEvent,
        ctx: &VerifyContext,
    ) -> InsertOutcome {
        if self.events.contains_key(&event.id) {
            return InsertOutcome::AlreadyKnown;
        }

        // Build prior_state from the current event log. Note that we
        // pass the candidate event so prior_state_at_event filters
        // strictly less-than, not less-than-or-equal — without this
        // the candidate would self-authorize against its own future
        // state if it had already been inserted.
        let log: Vec<SignedMembershipEvent> = self.events.values().cloned().collect();
        let prior = prior_state_at_event(&log, &event, ctx.admin_addr);

        if let Err(e) = verify_event(&event, &prior, ctx) {
            return InsertOutcome::Rejected(e);
        }

        self.events.insert(event.id, event);
        InsertOutcome::Inserted
    }

    /// Materialize the current event log. Pure; no caching at this
    /// layer — callers that want a cached view should hold the
    /// `materialized()` result and invalidate on every successful
    /// `insert_event`. Task 3 adds the cache.
    pub fn materialize_now(&self, admin_addr: OwnerAddr) -> MaterializedMembership {
        let log: Vec<SignedMembershipEvent> = self.events.values().cloned().collect();
        materialize(&log, admin_addr)
    }
}
