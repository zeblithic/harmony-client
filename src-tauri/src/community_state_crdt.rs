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

#[derive(Debug, Deserialize, Serialize)]
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

    /// Materialized-view cache. Skipped from CBOR — derivable from
    /// `events` so persisting it would just inflate the wire form.
    /// Wrapped in a `Mutex` so reads (which may need to populate the
    /// cache on miss) don't take a mutable borrow on `&self`.
    #[serde(skip)]
    cache: std::sync::Mutex<MaterializedCache>,
}

#[derive(Default, Debug)]
struct MaterializedCache {
    /// Bumps every time `events` mutates. Reads hand out a clone of
    /// the cached value if `cached_version == version`; otherwise
    /// re-materialize and update.
    version: u64,
    cached_version: Option<u64>,
    cached: Option<MaterializedMembership>,
}

// `Mutex<MaterializedCache>` is not `Clone` / `PartialEq`, so we can't
// auto-derive these on `CommunityState`. The cache is purely a derived
// view of `events`, so a clone fresh-initializes the cache (the clone
// will re-materialize on first read) and equality is well-defined as
// `community_id` + `events`.
impl Clone for CommunityState {
    fn clone(&self) -> Self {
        Self {
            community_id: self.community_id,
            events: self.events.clone(),
            cache: std::sync::Mutex::new(MaterializedCache::default()),
        }
    }
}

impl PartialEq for CommunityState {
    fn eq(&self, other: &Self) -> bool {
        self.community_id == other.community_id && self.events == other.events
    }
}
impl Eq for CommunityState {}

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
    pub fn new(community_id: SpaceId) -> Self {
        Self {
            community_id,
            events: BTreeMap::new(),
            cache: std::sync::Mutex::new(MaterializedCache::default()),
        }
    }

    /// Cache version counter. Bumps on every successful insert.
    /// Useful for IPC layers that want to short-circuit "did anything
    /// change?" checks across calls. Mirrors the version-counter
    /// pattern from `inbox_entries_for_space` in DM transport.
    pub fn materialized_version(&self) -> u64 {
        self.cache.lock().expect("cache mutex poisoned").version
    }

    /// Return the materialized view. Recomputes from `events` if the
    /// cache is stale (version mismatch); otherwise returns a clone
    /// of the cached value. The clone is intentional — handing out
    /// references would block `insert_event` callers behind reader
    /// holds, and `MaterializedMembership` is small (BTreeMaps of
    /// 16-byte addrs + small structs).
    ///
    /// `Mutex<MaterializedCache>` (not `Cell` / `RefCell`) because
    /// `CommunityState` will be held in `Arc<Mutex<_>>` shared across
    /// the engine's tokio task and IPC callers. The inner cache lock
    /// is short-held — recompute work happens while the lock is held
    /// so a concurrent reader gets the freshly-materialized view, but
    /// the lock is released before the cloned value is returned.
    pub fn materialized(&self, admin_addr: OwnerAddr) -> MaterializedMembership {
        let mut cache = self.cache.lock().expect("cache mutex poisoned");
        if cache.cached_version != Some(cache.version) {
            let log: Vec<SignedMembershipEvent> = self.events.values().cloned().collect();
            let m = materialize(&log, admin_addr);
            cache.cached = Some(m.clone());
            cache.cached_version = Some(cache.version);
            return m;
        }
        cache
            .cached
            .clone()
            .expect("cached_version Some implies cached Some")
    }

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
        // Invalidate cache by bumping version. Lazy re-mat happens on
        // the next `materialized` call.
        self.cache.lock().expect("cache mutex poisoned").version += 1;
        InsertOutcome::Inserted
    }

    /// Materialize the current event log without consulting the cache.
    /// Pure; callers that want a cached view should use `materialized`.
    /// Kept as a separate helper for tests and one-shot reads where
    /// cache pollution would be undesirable.
    pub fn materialize_now(&self, admin_addr: OwnerAddr) -> MaterializedMembership {
        let log: Vec<SignedMembershipEvent> = self.events.values().cloned().collect();
        materialize(&log, admin_addr)
    }
}
