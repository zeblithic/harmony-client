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

use core::cmp::Ordering;
use harmony_crdt_sync::verified_log::{LogPolicy, VerifiedLog};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::community_membership::{
    event_sort_key, materialize, materialize_with_now, verify_event, EventId,
    MaterializedMembership, SignedMembershipEvent, VerifyContext, VerifyError,
};
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{OwnerAddr, SpaceId};

/// Serde shim (ZEB-748 phase 6a Task 7) that keeps
/// [`CommunityState::log`] — a [`VerifiedLog<MembershipPolicy>`] — encoding
/// BYTE-IDENTICALLY to the legacy `events: BTreeMap<EventId,
/// SignedMembershipEvent>` field it replaced (CBOR field "ev").
///
/// **Byte-transparency is the hard requirement**: every persisted
/// `CommunityState` blob and every wire fixture (`zeb285`, `zeb250`, the
/// disk round-trip, the in-module `community_state_forked_from_*` tests) must
/// stay valid with zero fixture edits. To guarantee that by construction,
/// `serialize` rebuilds the OWNED `BTreeMap<EventId, SignedMembershipEvent>`
/// and delegates to its `Serialize` impl — the *exact same* code path
/// `#[derive(Serialize)]` invoked for the old field (`serialize_map(Some(len))`
/// then one `serialize_entry` per BTreeMap pair in EventId order). The
/// zero-copy borrowed-entry path would only match if it reproduced that
/// sequence precisely; the owned rebuild is identical by definition, so we
/// take it. Byte-identity beats zero-copy here.
mod membership_log_serde {
    use super::*;
    use serde::{Deserializer, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        log: &VerifiedLog<MembershipPolicy>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        // Rebuild the exact owned BTreeMap<EventId, SignedMembershipEvent> the
        // derive used to emit for `events` (keyed by e.id → EventId-ascending
        // iteration) and delegate to BTreeMap's own Serialize impl. This is
        // byte-for-byte what `#[derive(Serialize)]` did for the old field.
        let map: BTreeMap<EventId, SignedMembershipEvent> =
            log.events().map(|e| (e.id, e.clone())).collect();
        map.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<VerifiedLog<MembershipPolicy>, D::Error> {
        // Decode the legacy map shape, then restore into the engine WITHOUT
        // re-verifying (these events were verified when they first arrived and
        // were persisted). `into_values()` yields EventId order; the engine
        // re-keys by `e.id`, so the internal ordering is preserved.
        let map = BTreeMap::<EventId, SignedMembershipEvent>::deserialize(d)?;
        Ok(VerifiedLog::from_verified_events(map.into_values()))
    }
}

#[derive(Serialize, Deserialize)]
pub struct CommunityState {
    /// The community this state belongs to. Persisted in the wire form
    /// so that a misrouted blob (wrong file, wrong ContentStore key) is
    /// rejected at decode-time rather than silently materialized into
    /// the wrong community's view.
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    /// ZEB-285: SpaceId of the community this one was forked from, or
    /// None for a top-level (non-fork) community. Persisted in wire form
    /// so a fork's lineage survives round-trips and is visible to anyone
    /// who decodes the state. Set once at fork creation, never mutated.
    /// Byte-compatible with pre-ZEB-285 blobs (omitted when None).
    #[serde(rename = "ff", skip_serializing_if = "Option::is_none", default)]
    pub forked_from: Option<SpaceId>,

    /// ZEB-287 Phase 2: wall_ms component of the Fork event that
    /// created THIS community from its parent. Set at redeem-time from
    /// `PreForkSnapshot.forked_at.wall_ms`. `None` for top-level
    /// (non-fork) communities. Byte-compatible with pre-ZEB-287 blobs
    /// (omitted when None).
    #[serde(rename = "fa", skip_serializing_if = "Option::is_none", default)]
    pub forked_at_wall_ms: Option<u64>,

    /// ZEB-287 Phase 2: ordered list of ancestors (root → immediate parent)
    /// frozen at fork-time. For a Phase 2 fork built via
    /// `community_invite::build_parent_lineage`, the tail entry is the
    /// fork's immediate parent (also reflected in `forked_from`). For a
    /// Phase 1 fork (legacy invite, no `pl` carried), this stays empty
    /// and `forked_from` alone identifies the immediate parent.
    ///
    /// Populated at redeem-time from `PreForkSnapshot.parent_lineage` and
    /// at local-fork-create time by `community_fork.rs::fork_community`.
    /// Empty for top-level (non-fork) communities. Byte-compatible.
    ///
    /// IPC-DTO note: the `get_community_lineage` IPC synthesizes a single
    /// immediate-parent entry into the DTO when this stored chain is
    /// empty but `forked_from` is set (Phase 1 single-hop forks) so the
    /// frontend tree can render the parent row uniformly. Storage is
    /// unaffected — the synthesized entry exists only on the IPC boundary.
    #[serde(rename = "fl", skip_serializing_if = "Vec::is_empty", default)]
    pub parent_lineage: Vec<crate::community_invite::ParentLineageEntry>,

    /// ZEB-649: the forker's stated reason for creating THIS community
    /// (why it split from `forked_from`). Set once at fork creation
    /// (local-fork path) or mirrored from `PreForkSnapshot.fork_reason`
    /// at redeem-time (joiner path). `None` for top-level communities and
    /// for forks minted before ZEB-649. Byte-compatible (omitted when None).
    #[serde(rename = "fr", skip_serializing_if = "Option::is_none", default)]
    pub fork_reason: Option<String>,

    /// ZEB-250: M-of-N admin quorum. Number of admin-tier signatures
    /// required for admin-affecting actions (SetPower to/from 100,
    /// Kick of an admin, change of admin_quorum itself).
    ///
    /// Default 1 (single-admin governance — the proposer's signature
    /// alone suffices). When raised >= 2, admin-affecting actions
    /// must arrive as AdminProposal (with >= N-1 AdminCountersigns)
    /// instead of direct SetPower/Kick events. Backwards-compatible:
    /// pre-ZEB-250 blobs lack this field and decode as default 1.
    ///
    /// Cache of materialize-derived state — `materialize` walks
    /// ChangeQuorum proposals to compute the current value, and
    /// `insert_event` writes the result back here so fast-load
    /// (deserialize from disk) has the right value without
    /// re-materializing.
    #[serde(
        rename = "aq",
        default = "crate::community_membership::default_admin_quorum",
        skip_serializing_if = "crate::community_membership::is_default_admin_quorum"
    )]
    pub admin_quorum: u8,

    /// Append-only signed event log, verified on insert (ZEB-748 phase 6a).
    /// Backed by the core `VerifiedLog<MembershipPolicy>` engine. Serialized
    /// byte-identically to the legacy
    /// `events: BTreeMap<EventId, SignedMembershipEvent>` field it replaced —
    /// CBOR field "ev", keyed by EventId in ascending order — via the
    /// `membership_log_serde` shim below. Private: ALL access goes through the
    /// accessors (`events`, `get_event`, `insert_event`, …) so a future
    /// backing swap only touches this module.
    #[serde(rename = "ev", with = "membership_log_serde")]
    log: VerifiedLog<MembershipPolicy>,

    /// Materialized-view cache. Skipped from CBOR — derivable from
    /// `events` so persisting it would just inflate the wire form.
    /// Wrapped in a `Mutex` so reads (which may need to populate the
    /// cache on miss) don't take a mutable borrow on `&self`.
    #[serde(skip)]
    cache: std::sync::Mutex<MaterializedCache>,

    /// ZEB-249 Task 6 spec §5.2: UI bootstrap hint injected from
    /// `CommunityInvitePayload.epoch_snapshot.state_snapshot` after
    /// `mint_redemption`. Returned by `materialized()` ONLY when the
    /// event log is empty (i.e., CRDT events haven't arrived yet from
    /// Zenoh sync). On first real event insert, `version` increments
    /// past the cache, causing re-materialization from events which
    /// supersedes this hint. Skipped from CBOR (not source of truth —
    /// CRDT events are). Never written to disk.
    #[serde(skip)]
    bootstrap_hint: std::sync::Mutex<Option<MaterializedMembership>>,
}

#[derive(Default, Debug)]
struct MaterializedCache {
    /// Bumps every time `events` mutates. Reads hand out a clone of
    /// the cached value if `cached_version == version` AND
    /// `cached_admin_addr == Some(requested_admin_addr)`; otherwise
    /// re-materialize and update.
    version: u64,
    cached_version: Option<u64>,
    /// Admin address used to materialize the cached view. The
    /// `MaterializedMembership` shape depends on `admin_addr`
    /// (admin-power bootstrap), so the same `events` log can produce
    /// different materialized views under different admin_addr.
    /// Caching by version alone would let a caller passing a
    /// different `admin_addr` receive the wrong view at the same
    /// version. Phase 2 only ever has one admin_addr per community,
    /// but pinning this is cheap defense in depth.
    cached_admin_addr: Option<OwnerAddr>,
    cached: Option<MaterializedMembership>,
}

// `VerifiedLog<MembershipPolicy>` (the core engine) does not derive `Debug`,
// so `CommunityState` can't `#[derive(Debug)]`. Format it by hand, rendering
// the event log as the list of events it holds (EventId order).
impl std::fmt::Debug for CommunityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommunityState")
            .field("community_id", &self.community_id)
            .field("forked_from", &self.forked_from)
            .field("forked_at_wall_ms", &self.forked_at_wall_ms)
            .field("parent_lineage", &self.parent_lineage)
            .field("fork_reason", &self.fork_reason)
            .field("admin_quorum", &self.admin_quorum)
            .field("events", &self.log.events().collect::<Vec<_>>())
            .field("cache", &self.cache)
            .field("bootstrap_hint", &self.bootstrap_hint)
            .finish()
    }
}

// `Mutex<MaterializedCache>` is not `Clone` / `PartialEq`, so we can't
// auto-derive these on `CommunityState`. The cache is purely a derived
// view of the event log, so a clone fresh-initializes the cache (the clone
// will re-materialize on first read) and equality is well-defined as
// `community_id` + the event set.
impl Clone for CommunityState {
    fn clone(&self) -> Self {
        Self {
            community_id: self.community_id,
            forked_from: self.forked_from,
            forked_at_wall_ms: self.forked_at_wall_ms,
            parent_lineage: self.parent_lineage.clone(),
            fork_reason: self.fork_reason.clone(),
            admin_quorum: self.admin_quorum,
            // The events are already-verified; rebuild the engine from them
            // without re-running `verify` (a clone must never reject events
            // the original accepted).
            log: VerifiedLog::from_verified_events(self.log.events().cloned()),
            cache: std::sync::Mutex::new(MaterializedCache::default()),
            bootstrap_hint: std::sync::Mutex::new(
                self.bootstrap_hint.lock().ok().and_then(|g| g.clone()),
            ),
        }
    }
}

impl PartialEq for CommunityState {
    fn eq(&self, other: &Self) -> bool {
        self.community_id == other.community_id
            && self.forked_from == other.forked_from
            && self.forked_at_wall_ms == other.forked_at_wall_ms
            && self.parent_lineage == other.parent_lineage
            && self.fork_reason == other.fork_reason
            && self.admin_quorum == other.admin_quorum
            // Both logs iterate in EventId order, so `Iterator::eq` is exact
            // event-set equality (SignedMembershipEvent: PartialEq).
            && self.log.len() == other.log.len()
            && self.log.events().eq(other.log.events())
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

/// Per-insert policy context for [`MembershipPolicy`] (ZEB-748 phase 6a).
///
/// The core [`VerifiedLog`](harmony_crdt_sync::verified_log::VerifiedLog)
/// threads one `Context` through both the prior-state `materialize` and the
/// `verify` of a single insert. `now_floor_ms` is the candidate event's own
/// `at.wall_ms`, carried here so the prior-state materialization ages out
/// TIME-DRIVEN state (PendingJoin's 30-day expiry; the admin-recovery
/// lifecycle) exactly as `prior_state_at_event` does today — the R4-6
/// idle-community now-floor. Threading `None` instead would be a behavioral
/// regression.
///
/// Produced now for Task 7's `CommunityState` adoption, which threads this
/// per-insert context through [`CommunityState::insert_event`].
pub(crate) struct MembershipInsertCtx {
    pub verify: VerifyContext,
    pub now_floor_ms: u64,
}

/// The [`LogPolicy`] that adopts community-membership into the core
/// verified-event-log engine (ZEB-748 phase 6a).
///
/// Pure glue: every method delegates to the unchanged `community_membership`
/// free functions (`event_sort_key`, `verify_event`, `materialize_with_now`),
/// so a `VerifiedLog<MembershipPolicy>` and the legacy
/// [`CommunityState::insert_event`] path stay bit-for-bit equivalent. The type
/// is zero-sized — all state lives in the log's events and the per-insert
/// [`MembershipInsertCtx`]. Task 7 makes [`CommunityState`] hold a
/// `VerifiedLog<MembershipPolicy>`, so this type now has a real consumer.
pub(crate) struct MembershipPolicy;

impl LogPolicy for MembershipPolicy {
    type Event = SignedMembershipEvent;
    type EventId = EventId;
    type State = MaterializedMembership;
    type Context = MembershipInsertCtx;
    type Error = VerifyError;

    fn event_id(e: &SignedMembershipEvent) -> EventId {
        e.id
    }

    fn cmp(a: &SignedMembershipEvent, b: &SignedMembershipEvent) -> Ordering {
        // The single canonical total order, shared with `materialize`.
        event_sort_key(a).cmp(&event_sort_key(b))
    }

    fn verify(
        e: &SignedMembershipEvent,
        prior: &MaterializedMembership,
        ctx: &MembershipInsertCtx,
    ) -> Result<(), VerifyError> {
        verify_event(e, prior, &ctx.verify)
    }

    fn materialize(
        events: &[&SignedMembershipEvent],
        ctx: &MembershipInsertCtx,
    ) -> MaterializedMembership {
        // The core hands events in unspecified order; `materialize_with_now`
        // sorts internally by `event_sort_key`, so input order is irrelevant.
        // Passing `Some(now_floor_ms)` — the candidate's own wall_ms —
        // reproduces `prior_state_at_event`'s R4-6 idle-community aging floor.
        // Threading `None` here would be a behavioral regression.
        let owned: Vec<SignedMembershipEvent> = events.iter().map(|e| (*e).clone()).collect();
        materialize_with_now(&owned, ctx.verify.admin_addr, Some(ctx.now_floor_ms))
    }
}

impl CommunityState {
    pub fn new(community_id: SpaceId) -> Self {
        Self {
            community_id,
            forked_from: None,
            forked_at_wall_ms: None,
            parent_lineage: Vec::new(),
            fork_reason: None,
            admin_quorum: 1,
            log: VerifiedLog::new(),
            cache: std::sync::Mutex::new(MaterializedCache::default()),
            bootstrap_hint: std::sync::Mutex::new(None),
        }
    }

    /// ZEB-249 Task 6 spec §5.2: seed the materialized-view cache with
    /// the snapshot from `CommunityInvitePayload.epoch_snapshot.state_snapshot`.
    ///
    /// This hint is returned by `materialized()` ONLY when the event log
    /// is still empty (i.e., before Zenoh-synced CRDT events arrive). The
    /// first real event insert invalidates the cache and causes re-
    /// materialization from the authoritative event log, which supersedes
    /// any inviter-supplied snapshot per spec §5.2 + §10.3.
    ///
    /// Concurrency: takes the `bootstrap_hint` mutex briefly. Safe to call
    /// from an async context (held only across the clone + assign — no
    /// `.await` inside).
    pub fn seed_bootstrap_hint(&self, hint: MaterializedMembership) {
        if let Ok(mut g) = self.bootstrap_hint.lock() {
            *g = Some(hint);
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
        // ZEB-249 Task 6 spec §5.2: when the event log is still empty
        // (just after redemption, before Zenoh-sync delivers CRDT events),
        // return the bootstrap hint if one was seeded by `seed_bootstrap_hint`.
        // Once a real event is inserted (bumping `version` to 1+), the cache
        // miss path re-materializes from events and the hint is superseded.
        //
        // M5: ALSO guard on events.is_empty(). After deserialization from
        // disk, version stays 0 (insert_event is never called on load; the
        // CRDT is rebuilt from the persisted event list). Without the
        // is_empty() check, the bootstrap hint would shadow real CRDT
        // data for any deserialized state where version was never advanced
        // (e.g., a replica that only received remote events, never inserted
        // a local one). The correct behavior: hint is only the authoritative
        // view when there are truly NO events yet.
        if cache.version == 0 && self.log.is_empty() {
            if let Ok(hint_g) = self.bootstrap_hint.lock() {
                if let Some(hint) = hint_g.clone() {
                    return hint;
                }
            }
        }
        let cache_hit = cache.cached_version == Some(cache.version)
            && cache.cached_admin_addr == Some(admin_addr);
        if !cache_hit {
            let log: Vec<SignedMembershipEvent> = self.log.events().cloned().collect();
            let m = materialize(&log, admin_addr);
            cache.cached = Some(m.clone());
            cache.cached_version = Some(cache.version);
            cache.cached_admin_addr = Some(admin_addr);
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
        use harmony_crdt_sync::verified_log::InsertOutcome as CoreOutcome;

        // Build a FRESH per-insert policy context. `now_floor_ms` is the
        // candidate's own `at.wall_ms` — the R4-6 idle-community aging floor
        // that `prior_state_at_event` applied — read into a local BEFORE
        // `event` moves into `self.log.insert`. The engine dedups by id,
        // materializes the strictly-prior set (matching the old
        // `prior_state_at_event` filter), then runs `verify_event`.
        let policy_ctx = MembershipInsertCtx {
            verify: *ctx,
            now_floor_ms: event.at.wall_ms,
        };
        match self.log.insert(event, &policy_ctx) {
            CoreOutcome::AlreadyKnown => InsertOutcome::AlreadyKnown,
            CoreOutcome::Rejected(e) => InsertOutcome::Rejected(e),
            CoreOutcome::Inserted => {
                // Invalidate cache by bumping version. Lazy re-mat happens on
                // the next `materialized` call.
                self.cache.lock().expect("cache mutex poisoned").version += 1;

                // ZEB-250: synchronize CommunityState.admin_quorum with the
                // freshly-recomputed materialized view. `materialize` is the
                // source of truth (walks ChangeQuorum proposals in HLC order);
                // we write the result back to the persistent field so
                // fast-load doesn't need to re-materialize.
                let derived = self.materialize_now(ctx.admin_addr).admin_quorum;
                self.admin_quorum = derived;

                InsertOutcome::Inserted
            }
        }
    }

    /// Materialize the current event log without consulting the cache.
    /// Pure; callers that want a cached view should use `materialized`.
    /// Kept as a separate helper for tests and one-shot reads where
    /// cache pollution would be undesirable.
    pub fn materialize_now(&self, admin_addr: OwnerAddr) -> MaterializedMembership {
        let log: Vec<SignedMembershipEvent> = self.log.events().cloned().collect();
        materialize(&log, admin_addr)
    }

    /// ZEB-713: time-aware uncached materialization — the accessor every
    /// consumer of TIME-DRIVEN derived state (admin-recovery lifecycle:
    /// `recovery_proposals` phases, recovery execution, its
    /// `pending_rotation_for` marker) MUST use, passing real wall-clock
    /// ms. The cached `materialized()` view is event-driven: in an idle
    /// community it can never advance a Time-locked recovery to
    /// Executed (same staleness class as PendingJoin's 30-day expiry in
    /// the cached view — the R4-6 precedent). Not cached: the result
    /// depends on `now_ms`, so the version-keyed cache cannot hold it.
    ///
    /// D2 wiring (ZEB-714): `get_recovery_state`, the recovery banner
    /// projection, and the ZEB-249 self-healing rotation observer route
    /// through this.
    pub fn materialized_with_now(
        &self,
        admin_addr: OwnerAddr,
        now_ms: u64,
    ) -> MaterializedMembership {
        let log: Vec<SignedMembershipEvent> = self.log.events().cloned().collect();
        materialize_with_now(&log, admin_addr, Some(now_ms))
    }

    // ── ZEB-748 phase 6a: event-log accessors ──────────────────────────
    //
    // Read accessors + trusted-write seams that mediate ALL access to the
    // event log. Today they delegate to the `events` `BTreeMap` field; when
    // Task 7 flips that field to a `VerifiedLog`, only these method BODIES
    // change and the ~138 migrated call sites (Tasks 4–6) stay untouched.
    // Their SIGNATURES are the migration contract — do not alter them.

    /// Iterate the event log in canonical (EventId-ascending) order.
    pub fn events(&self) -> impl Iterator<Item = &SignedMembershipEvent> {
        // Core `VerifiedLog::events()` iterates its internal
        // `BTreeMap<EventId, Event>` values → EventId-ascending, preserving
        // the old `BTreeMap::values()` contract.
        self.log.events()
    }

    /// Look up a single event by id.
    pub fn get_event(&self, id: &EventId) -> Option<&SignedMembershipEvent> {
        self.log.get(id)
    }

    /// Whether an event with this id is already in the log.
    pub fn contains_event(&self, id: &EventId) -> bool {
        self.log.contains(id)
    }

    /// Number of events in the log.
    pub fn event_count(&self) -> usize {
        self.log.len()
    }

    /// Whether the log holds no events yet.
    pub fn events_is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// Consume the state, yielding its events (canonical order).
    pub fn into_events(self) -> Vec<SignedMembershipEvent> {
        self.log.events().cloned().collect()
    }

    /// Trusted-write seam: insert a pre-verified event WITHOUT re-running
    /// `verify_event`. Test/bootstrap only — never a production merge path.
    /// Bumps the cache version so the next `materialized()` re-materializes
    /// (mirrors what direct `events.insert` callers relied on; harmless for
    /// the pre-flip field, required after Task 7 flips to a `VerifiedLog`).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn insert_verified_for_test(&mut self, e: SignedMembershipEvent) {
        // `VerifiedLog` has no unverified single-insert seam, so rebuild it
        // from the existing events plus the new one via the trusted
        // `from_verified_events` restore path (dedups by id).
        let mut evs: Vec<SignedMembershipEvent> = self.log.events().cloned().collect();
        evs.push(e);
        self.log = VerifiedLog::from_verified_events(evs);
        self.cache.lock().expect("cache mutex poisoned").version += 1;
    }

    /// Trusted-write seam: replace the entire event log in one shot.
    /// Test/bootstrap only. Bumps the cache version (see
    /// `insert_verified_for_test`).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn set_event_log_for_test(&mut self, events: BTreeMap<EventId, SignedMembershipEvent>) {
        self.log = VerifiedLog::from_verified_events(events.into_values());
        self.cache.lock().expect("cache mutex poisoned").version += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crypto::canonical_cbor_encode;

    #[test]
    fn community_state_forked_from_cbor_skip() {
        // ZEB-285: a CommunityState with forked_from = None must encode
        // byte-identical to pre-ZEB-285 wire form (no "ff" key emitted).
        let cid = SpaceId([0xc0; 16]);
        let state = CommunityState::new(cid);

        let bytes = canonical_cbor_encode(&state).expect("encode");
        let value: ciborium::Value =
            ciborium::de::from_reader(&bytes[..]).expect("decode as value");
        let map = value.as_map().expect("outer is map");

        // Top-level keys should NOT include "ff" when forked_from is None.
        assert!(
            !map.iter()
                .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text() == Some("ff")),
            "forked_from=None should be omitted from CBOR encoding"
        );
    }

    #[test]
    fn community_state_forked_from_some_roundtrip() {
        // ZEB-285: with forked_from = Some(_), the "ff" key appears and
        // round-trips correctly.
        let cid = SpaceId([0xc0; 16]);
        let original_id = SpaceId([0xa0; 16]);

        let mut state = CommunityState::new(cid);
        state.forked_from = Some(original_id);

        let bytes = canonical_cbor_encode(&state).expect("encode");
        let decoded: CommunityState = ciborium::de::from_reader(&bytes[..]).expect("decode");

        assert_eq!(decoded.community_id, cid);
        assert_eq!(decoded.forked_from, Some(original_id));

        let value: ciborium::Value =
            ciborium::de::from_reader(&bytes[..]).expect("re-decode as value");
        let map = value.as_map().expect("outer is map");
        assert!(
            map.iter()
                .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text() == Some("ff")),
            "forked_from=Some should appear in CBOR encoding"
        );
    }
}

#[cfg(test)]
mod policy_tests {
    //! ZEB-748 phase 6a: `MembershipPolicy` is the `LogPolicy` adopter that
    //! lets a `VerifiedLog<MembershipPolicy>` reuse the unchanged
    //! `community_membership` verify/materialize/sort functions. This proves
    //! the adopter's insert/dedup/reject wiring against the core engine
    //! WITHOUT touching `CommunityState` (that is a later task).
    use super::*;
    use crate::community_membership::{
        mint_test_owner, sign_event, EventPayload, MembershipEventKind, TestOwner,
    };
    use crate::owner_state_types::Hlc;
    // The core engine's `InsertOutcome<E>` is a distinct type from this
    // crate's own `InsertOutcome`, so it is aliased to avoid the name clash.
    use harmony_crdt_sync::verified_log::{InsertOutcome as CoreOutcome, VerifiedLog};

    fn hlc(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: "d".into(),
        }
    }

    /// Sign a membership event with `owner`'s enrolled device key, attaching
    /// the Master cert on identity-introducing Join events so materialize
    /// populates `enrolled_device_keys` and `verify_event` can resolve the
    /// signer. Mirrors `community_state_crdt_unit.rs::sign_event_with_identity`.
    fn sign_join(payload: &EventPayload, owner: &TestOwner) -> SignedMembershipEvent {
        let ev = sign_event(payload, &owner.device_key).expect("sign");
        match ev.kind {
            MembershipEventKind::Join | MembershipEventKind::PendingJoin { .. } => {
                SignedMembershipEvent {
                    enrollment: Some(owner.cert.clone()),
                    ..ev
                }
            }
            _ => ev,
        }
    }

    #[test]
    fn membership_policy_insert_dedup_reject() {
        let owner = mint_test_owner(0xa1);
        let addr = owner.owner;
        let community_id = SpaceId([1u8; 16]);

        // A valid admin self-Join in an open (not invite-only) community.
        let bootstrap = sign_join(
            &EventPayload {
                id: [3u8; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: addr,
                at: hlc(100),
            },
            &owner,
        );

        // now_floor_ms = the candidate's own wall_ms (the R4-6 floor), exactly
        // as Task 7's per-insert ctx will thread it.
        let ctx = MembershipInsertCtx {
            verify: VerifyContext {
                expected_community_id: community_id,
                admin_addr: addr,
                is_invite_only: false,
            },
            now_floor_ms: bootstrap.at.wall_ms,
        };

        let mut log: VerifiedLog<MembershipPolicy> = VerifiedLog::new();

        // New, verified event -> Inserted.
        assert_eq!(log.insert(bootstrap.clone(), &ctx), CoreOutcome::Inserted);
        assert_eq!(log.len(), 1);

        // Same id again -> AlreadyKnown; verify is NOT re-run (dedup short-circuit).
        assert_eq!(
            log.insert(bootstrap.clone(), &ctx),
            CoreOutcome::AlreadyKnown
        );
        assert_eq!(log.len(), 1);

        // A NEW-id event for the WRONG community: verify_event rejects at its
        // community-binding step 0, so the policy surfaces Rejected and the
        // event does not land.
        let wrong_community = sign_join(
            &EventPayload {
                id: [4u8; 16],
                community_id: SpaceId([2u8; 16]),
                kind: MembershipEventKind::Join,
                actor: addr,
                at: hlc(200),
            },
            &owner,
        );
        assert!(matches!(
            log.insert(wrong_community, &ctx),
            CoreOutcome::Rejected(VerifyError::WrongCommunity)
        ));
        assert_eq!(log.len(), 1);
    }
}
