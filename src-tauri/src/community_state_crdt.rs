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
// Sole non-scoped consumer is `set_event_log_for_test`, gated the same way.
#[cfg(any(test, feature = "test-fixtures"))]
use std::collections::BTreeMap;

use crate::community_membership::{
    event_sort_key, materialize_with_bounds, materialize_with_now, verify_event, EventId,
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
/// stay valid with zero fixture edits. `serialize` collects the log's events
/// into a `BTreeMap<EventId, &SignedMembershipEvent>` (borrowed values —
/// serde serializes `&T` byte-identically to `T`, so no per-event clone is
/// needed) and delegates to `BTreeMap`'s own `Serialize` impl: the *exact
/// same* code path `#[derive(Serialize)]` invoked for the old field
/// (`serialize_map(Some(len))` then one `serialize_entry` per pair in EventId
/// order). `deserialize` decodes the same legacy map shape and rejects any
/// blob whose stored key disagrees with the event's own id (see below).
mod membership_log_serde {
    use super::*;
    use serde::{Deserializer, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        log: &VerifiedLog<MembershipPolicy>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        // Collect the log's events into BTreeMap<EventId, &SignedMembershipEvent>
        // (borrowed values — serde serializes `&T` byte-identically to `T`),
        // keyed by e.id → EventId-ascending, and delegate to BTreeMap's own
        // Serialize impl. Byte-for-byte what `#[derive(Serialize)]` emitted for
        // the old owned field, with no per-event clone.
        let map: BTreeMap<EventId, &SignedMembershipEvent> =
            log.events().map(|e| (e.id, e)).collect();
        map.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<VerifiedLog<MembershipPolicy>, D::Error> {
        // Decode the legacy map shape, then restore into the engine WITHOUT
        // re-verifying (these events were verified when they first arrived and
        // were persisted). Reject any blob whose stored CBOR map key disagrees
        // with the event's own id rather than silently re-keying it — a
        // mismatch can only mean a corrupt or tampered blob, and silent
        // re-keying could collapse a distinct event. Every valid blob upholds
        // key == e.id (the invariant every writer maintains), so this is
        // byte-transparent for all real state. `from_verified_events` re-keys
        // by e.id, preserving EventId iteration order.
        let map = BTreeMap::<EventId, SignedMembershipEvent>::deserialize(d)?;
        let mut events = Vec::with_capacity(map.len());
        for (id, event) in map {
            if id != event.id {
                return Err(<D::Error as serde::de::Error>::custom(
                    "membership event map key does not match event id",
                ));
            }
            events.push(event);
        }
        Ok(VerifiedLog::from_verified_events(events))
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
    /// ZEB-846: wall time (ms) at which the soonest currently-deferred
    /// future-dated event enters the `MAX_FORWARD_SKEW_MS` window, i.e.
    /// `min(excluded event walls) - MAX_FORWARD_SKEW_MS`. `None` = the cached
    /// view excluded nothing on forward-skew grounds, so advancing wall time
    /// cannot change it (the version key alone governs validity). When `Some`,
    /// a cache hit is additionally gated on `receiver_now < recompute_after_ms`
    /// — past that instant the version-keyed view is stale w.r.t. the clock and
    /// must re-materialize even though the log is unchanged, so a legitimately
    /// skewed event becomes effective the moment it is plausible rather than
    /// waiting for the next log mutation to bust the cache.
    recompute_after_ms: Option<u64>,
    /// ZEB-846: whether the forward-skew ceiling was ENABLED (receiver clock
    /// available, `Some`) when this view was built. The bound is time-dependent
    /// *only while enabled*; the enabled/disabled MODE is a second axis the
    /// version key does not capture. A view built with the ceiling disabled
    /// (pre-epoch clock ⇒ apply-all) must not keep serving after the clock
    /// recovers — it would UNDER-exclude a now-boundable future event; a bounded
    /// view must not keep serving after the clock is lost — it would keep
    /// DEFERRING honest governance. So a cache hit additionally requires the
    /// current mode to equal this one. `None` = cache never populated.
    cached_ceiling_enabled: Option<bool>,
}

/// ZEB-846: whether a time-aware cached materialization is still valid at
/// `receiver_now_ms`. `recompute_after_ms` is the wall time at which the
/// soonest currently-deferred event enters the forward-skew window (see
/// [`MaterializedCache::recompute_after_ms`]); `None` means nothing was
/// deferred, so the cache is time-insensitive. A `None` `receiver_now_ms`
/// (pre-epoch clock) keeps the cache valid — the ceiling is disabled anyway, so
/// nothing was deferred to become includable. Pulled out as a pure fn so the
/// refresh boundary is exhaustively unit-testable without mocking the clock.
fn cache_time_still_valid(recompute_after_ms: Option<u64>, receiver_now_ms: Option<u64>) -> bool {
    match recompute_after_ms {
        None => true,
        Some(t) => receiver_now_ms.is_none_or(|rn| rn < t),
    }
}

/// ZEB-846: whether a cached materialization is still valid to serve at the
/// current clock, across BOTH axes the version key does not capture:
///
/// 1. **Ceiling mode** — the view is only reusable if the ceiling is in the
///    same enabled/disabled state it was built under. A `None`-clock (apply-all)
///    view surviving a clock recovery would UNDER-exclude a now-boundable future
///    event (the direction ZEB-846 exists to prevent); a bounded view surviving
///    a clock loss would keep DEFERRING honest governance. A never-populated
///    cache (`cached_ceiling_enabled == None`) is never a hit.
/// 2. **Time** — within a bounded (enabled) mode, the soonest-deferred event
///    must not yet have entered the window ([`cache_time_still_valid`]).
///
/// Pure so both mode-flip directions are exhaustively unit-testable without
/// mocking the system clock.
fn cache_policy_still_valid(
    cached_ceiling_enabled: Option<bool>,
    now_ceiling_enabled: bool,
    recompute_after_ms: Option<u64>,
    receiver_now_ms: Option<u64>,
) -> bool {
    cached_ceiling_enabled == Some(now_ceiling_enabled)
        && cache_time_still_valid(recompute_after_ms, receiver_now_ms)
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

    type SupersessionKey = AnnounceKey;

    fn supersession_key(e: &SignedMembershipEvent) -> Option<AnnounceKey> {
        use crate::community_membership::MembershipEventKind::{
            CommunityRelayAnnounce, ReachabilityAnnounce,
        };
        // ZEB-813: only the two LWW routing-announce kinds form supersession
        // lineages — both are materialize-neutral (their `materialize` arms
        // are no-ops), satisfying the core contract; every other kind is
        // durable community history and must never be compacted. Within a
        // lineage the core keeps the maximum under `cmp` (= event_sort_key),
        // i.e. the newest announce per (actor, device/node).
        match &e.kind {
            ReachabilityAnnounce { payload } => {
                Some(AnnounceKey::Reachability(e.actor, payload.iroh_node_id))
            }
            CommunityRelayAnnounce { payload } => {
                Some(AnnounceKey::Relay(e.actor, payload.relay.relay_device_id))
            }
            _ => None,
        }
    }
}

/// ZEB-813: supersession-lineage key — one lineage per (kind, actor,
/// device/node id). Only the newest event per lineage is retained by the
/// core `VerifiedLog`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AnnounceKey {
    Reachability(OwnerAddr, [u8; 32]),
    Relay(OwnerAddr, [u8; 16]),
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

    /// ZEB-776: the channels the bootstrap hint knows about (from the invite's
    /// epoch_snapshot), regardless of the `materialized()` log-empty guard.
    /// Empty when no hint was seeded. Clones out under the brief hint mutex —
    /// the read path merges these with the hint-blind materialize to label
    /// still-converging channels `syncing`.
    pub fn bootstrap_hint_channels(
        &self,
    ) -> Vec<(
        crate::community_membership::ChannelId,
        crate::community_membership::ChannelInfo,
    )> {
        self.bootstrap_hint
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|h| h.channels.clone()))
            .map(|channels| channels.into_iter().collect())
            .unwrap_or_default()
    }

    /// ZEB-776: whether the event log has any events yet. The channel read path
    /// uses this to prefer the cached `materialized()` (log non-empty — the
    /// post-redeem norm) over the uncached `materialize_now()`, while still
    /// treating a truly-empty log as "nothing confirmed" so hint channels stay
    /// `syncing`. Mirrors the `log.is_empty()` half of `materialized()`'s
    /// hint-substitution guard.
    pub fn log_is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// Test-only: build a state whose log is restored from already-trusted
    /// events, mirroring the deserialize path (`from_verified_events`) —
    /// including its ZEB-813 supersession compaction.
    #[cfg(test)]
    pub(crate) fn from_trusted_events_for_test(
        community_id: SpaceId,
        events: impl IntoIterator<Item = SignedMembershipEvent>,
    ) -> Self {
        let mut state = Self::new(community_id);
        state.log = VerifiedLog::from_verified_events(events);
        state
    }

    /// Test-only: number of events currently in the log.
    #[cfg(test)]
    pub(crate) fn event_count_for_test(&self) -> usize {
        self.log.len()
    }

    /// Test-only: the cache's current `recompute_after_ms` (ZEB-846 time-aware
    /// invalidation marker). `None` until `materialized()` has populated it.
    #[cfg(test)]
    pub(crate) fn cache_recompute_after_for_test(&self) -> Option<u64> {
        self.cache
            .lock()
            .expect("cache mutex poisoned")
            .recompute_after_ms
    }

    /// Test-only: the ceiling MODE (`Some(true)` = ceiling enabled / receiver
    /// clock available) recorded when the cache was last populated. `None` until
    /// `materialized()` has run. ZEB-846 clock-mode invalidation marker.
    #[cfg(test)]
    pub(crate) fn cache_ceiling_enabled_for_test(&self) -> Option<bool> {
        self.cache
            .lock()
            .expect("cache mutex poisoned")
            .cached_ceiling_enabled
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
        // ZEB-846: sample the receiver-`now` ONCE up front — it feeds both the
        // time-aware cache-invalidation check below and (on a miss) the forward
        // ceiling. `receiver_now` is THIS node's own trusted wall clock (never a
        // peer/HLC-adopt value — the attacker we bound can move those). `None`
        // (pre-epoch clock) ⇒ ceiling disabled (apply-all), so a bad local clock
        // can't drop honest governance.
        let receiver_now = crate::clock_trust::receiver_now_ms();
        let now_ceiling_enabled = receiver_now.is_some();
        let cache_hit = cache.cached_version == Some(cache.version)
            && cache.cached_admin_addr == Some(admin_addr)
            && cache_policy_still_valid(
                cache.cached_ceiling_enabled,
                now_ceiling_enabled,
                cache.recompute_after_ms,
                receiver_now,
            );
        if !cache_hit {
            let log: Vec<SignedMembershipEvent> = self.log.events().cloned().collect();
            // Apply the receiver-`now` forward-skew ceiling. The floor stays
            // `None` here (event-driven cached view). The excluded set is
            // view-only: nothing is deleted from the stored CRDT state, only
            // from this materialized snapshot. Reload-safe: the store is never
            // mutated, so a reloaded log re-derives the same bound on first read.
            let m = materialize_with_bounds(&log, admin_addr, None, receiver_now);
            // Compute when the soonest currently-deferred event will enter the
            // window so the version-keyed cache self-refreshes at that instant
            // instead of over-excluding until the next log mutation. `None`
            // receiver_now (pre-epoch) excludes nothing ⇒ no time sensitivity.
            let recompute_after_ms = receiver_now.and_then(|rn| {
                log.iter()
                    .map(|e| e.at.wall_ms)
                    .filter(|&w| {
                        crate::clock_trust::reject_future(
                            w,
                            rn,
                            crate::clock_trust::MAX_FORWARD_SKEW_MS,
                        )
                    })
                    .min()
                    .map(|min_excluded| {
                        min_excluded.saturating_sub(crate::clock_trust::MAX_FORWARD_SKEW_MS)
                    })
            });
            cache.cached = Some(m.clone());
            cache.cached_version = Some(cache.version);
            cache.cached_admin_addr = Some(admin_addr);
            cache.recompute_after_ms = recompute_after_ms;
            cache.cached_ceiling_enabled = Some(now_ceiling_enabled);
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
            CoreOutcome::Superseded => {
                // ZEB-813: a stale announce that a stored event supersedes.
                // Nothing changed — no cache bump, no dirty mark, no
                // persist. AlreadyKnown is exactly that contract, so
                // callers need no new arm.
                InsertOutcome::AlreadyKnown
            }
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
        // ZEB-846: forward-skew ceiling against this node's own wall clock; floor
        // stays `None` (this is the no-floor uncached read). `None` receiver_now
        // (pre-epoch clock) ⇒ apply-all.
        let receiver_now = crate::clock_trust::receiver_now_ms();
        materialize_with_bounds(&log, admin_addr, None, receiver_now)
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
        // ZEB-846: recovery reads pass `wall_now_ms` here — it IS the receiver's
        // own clock, so it serves as BOTH the aging floor and the forward-skew
        // ceiling. A future-dated event is excluded before materialize just as in
        // the cached read.
        materialize_with_bounds(&log, admin_addr, Some(now_ms), Some(now_ms))
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

    #[test]
    fn bootstrap_hint_channels_returns_seeded_channels_and_empty_without_hint() {
        use crate::community_membership::{ChannelId, ChannelInfo, ChannelKind};
        use crate::owner_state_types::Hlc;
        // ZEB-776: the accessor surfaces the seeded hint's channels regardless
        // of the materialized() log-empty guard, and is empty without a hint.
        let state = CommunityState::new(SpaceId([0x01; 16]));
        assert!(state.bootstrap_hint_channels().is_empty());

        let mut channels = std::collections::BTreeMap::new();
        channels.insert(
            ChannelId([0x11; 16]),
            ChannelInfo {
                name: "general".into(),
                write_power: 0,
                kind: ChannelKind::Text,
                created_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "seed".into(),
                },
                deleted_at: None,
            },
        );
        state.seed_bootstrap_hint(MaterializedMembership {
            channels: channels.clone(),
            ..Default::default()
        });

        let got = state.bootstrap_hint_channels();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, ChannelId([0x11; 16]));
        assert_eq!(got[0].1.name, "general");
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
                now_ms: None,
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

// ── ZEB-846 Task 2: forward-skew ceiling in the CommunityState accessors ──
//
// The three read accessors (`materialized`, `materialize_now`,
// `materialized_with_now`) apply a receiver-`now` forward ceiling: an event
// stamped more than `MAX_FORWARD_SKEW_MS` beyond THIS node's own wall clock is
// excluded from the view. The exclusion is a live view over the persisted log —
// non-destructive — so it must survive a persist→reload round-trip WITHOUT any
// admission layer having filtered the log first.
#[cfg(test)]
mod zeb846_accessor_ceiling {
    use super::*;
    use crate::community_membership::{MemberStatus, MembershipEventKind};
    use crate::community_state_persist;
    use crate::owner_state_types::Hlc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::tempdir;

    const COMMUNITY: SpaceId = SpaceId([0xc0; 16]);
    const YEAR_MS: u64 = 365 * 86_400_000;

    fn real_now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as u64
    }

    fn addr(seed: u8) -> OwnerAddr {
        OwnerAddr([seed; 16])
    }

    /// Build a signed-shaped event with a dummy signature. `materialize*` is a
    /// pure replay that never verifies signatures (see its doc comment), so a
    /// zero `sig` is sufficient to exercise the accessors' ceiling.
    fn join_event(id_byte: u8, actor: OwnerAddr, at_wall_ms: u64) -> SignedMembershipEvent {
        let mut id = [0xfd; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: COMMUNITY,
            kind: MembershipEventKind::Join,
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    fn kick_event(
        id_byte: u8,
        actor: OwnerAddr,
        target: OwnerAddr,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let mut id = [0xfa; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: COMMUNITY,
            kind: MembershipEventKind::Kick {
                target,
                reason: None,
            },
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    /// Local "is `a` an active member of the view" read — mirrors the
    /// `mat.members.get(&addr).map(|m| m.status)` inspection used throughout the
    /// membership tests; not a new accessor.
    fn membership_contains(m: &MaterializedMembership, a: &OwnerAddr) -> bool {
        matches!(
            m.members.get(a).map(|s| s.status),
            Some(MemberStatus::Joined)
        )
    }

    /// A `CommunityState` whose log holds an honest victim Join (stamped just
    /// before real now) plus a far-future admin Kick of that victim (now + 1yr).
    /// The admin holds power 100 implicitly via the materialize bootstrap rule,
    /// so WITHOUT the ceiling the Kick wins LWW (wall is the primary sort key)
    /// and bans the victim; WITH it the future Kick is excluded and the victim
    /// retains membership.
    fn state_with_join_and_future_kick() -> (CommunityState, OwnerAddr, OwnerAddr) {
        let admin = addr(1);
        let victim = addr(2);
        let now = real_now_ms();
        let join = join_event(10, victim, now - 10_000);
        let kick = kick_event(20, admin, victim, now + YEAR_MS);
        let state = CommunityState::from_trusted_events_for_test(COMMUNITY, [join, kick]);
        (state, admin, victim)
    }

    #[test]
    fn cached_materialized_excludes_future_dated_kick() {
        let (state, admin, victim) = state_with_join_and_future_kick();
        let m = state.materialized(admin);
        assert!(
            membership_contains(&m, &victim),
            "cached materialized() read must exclude the future-dated Kick (receiver-now ceiling)"
        );
    }

    #[test]
    fn materialize_now_excludes_future_dated_kick() {
        let (state, admin, victim) = state_with_join_and_future_kick();
        let m = state.materialize_now(admin);
        assert!(
            membership_contains(&m, &victim),
            "uncached materialize_now() read must also exclude the future-dated Kick"
        );
    }

    #[test]
    fn materialized_with_now_floor_and_ceiling_excludes_future_dated_kick() {
        // Task 1 review carry-forward: exercise the recovery read where BOTH the
        // aging floor Some(now) AND the forward ceiling Some(now) are active
        // (the accessor passes `now_ms` as both), not only the None-floor path.
        let (state, admin, victim) = state_with_join_and_future_kick();
        let m = state.materialized_with_now(admin, real_now_ms());
        assert!(
            membership_contains(&m, &victim),
            "materialized_with_now() (floor+ceiling both Some(now)) must exclude the future Kick"
        );
    }

    #[test]
    fn poison_survives_reload_but_is_still_deferred_on_materialize() {
        // Reload-safety proof: persist a state carrying a far-future Kick frame
        // directly (the poison is a normal trusted-log event here — Layer 3
        // admission isn't wired yet), reload it, and confirm the ceiling STILL
        // holds — proving the fix does NOT depend on admission having filtered
        // the log.
        let dir = tempdir().unwrap();
        let path = dir.path().join("crdt.cbor");
        let (state, admin, victim) = state_with_join_and_future_kick();
        assert_eq!(
            state.event_count(),
            2,
            "precondition: join + future kick both in the log"
        );

        community_state_persist::save_crdt(&path, &state).unwrap();
        let reloaded = community_state_persist::load_crdt(&path, COMMUNITY).unwrap();

        // (a) the poison event is RETAINED after reload (non-destructive):
        assert!(
            reloaded.event_count() >= 2,
            "reload must RETAIN the poison event (non-destructive); we only defer it from the view"
        );
        // (b) but the materialized view still defers it:
        let m = reloaded.materialized(admin);
        assert!(
            membership_contains(&m, &victim),
            "after reload, materialized() still defers the future-dated Kick"
        );
    }

    #[test]
    fn cache_time_still_valid_covers_the_refresh_boundary() {
        // No deferred event ⇒ time-insensitive: valid at any clock, including a
        // pre-epoch (None) one.
        assert!(cache_time_still_valid(None, Some(1_000)));
        assert!(cache_time_still_valid(None, None));
        // Some(t): the cache is valid strictly BEFORE the deferred event enters
        // the window, and stale AT or AFTER that instant (must re-materialize so
        // the now-plausible event takes effect).
        assert!(
            cache_time_still_valid(Some(1_000), Some(999)),
            "before the window: cache still valid"
        );
        assert!(
            !cache_time_still_valid(Some(1_000), Some(1_000)),
            "at the window edge: recompute (event just became plausible)"
        );
        assert!(
            !cache_time_still_valid(Some(1_000), Some(1_001)),
            "past the window: recompute"
        );
        // A pre-epoch receiver clock disables the ceiling entirely (nothing was
        // deferred), so the cache stays valid rather than thrashing.
        assert!(
            cache_time_still_valid(Some(1_000), None),
            "pre-epoch receiver clock keeps the cache valid (apply-all)"
        );
    }

    #[test]
    fn materialized_sets_recompute_after_to_when_the_deferred_kick_becomes_plausible() {
        // Wiring proof for the time-aware invalidation: after materializing a
        // view that defers a far-future Kick, the cache records EXACTLY when that
        // Kick enters the 5-min window (`kick_wall - MAX_FORWARD_SKEW_MS`), so a
        // silent community (no further log mutations) still self-refreshes at
        // that instant instead of over-excluding forever.
        let admin = addr(1);
        let victim = addr(2);
        let now = real_now_ms();
        let kick_wall = now + YEAR_MS;
        let join = join_event(10, victim, now - 10_000);
        let kick = kick_event(20, admin, victim, kick_wall);
        let state = CommunityState::from_trusted_events_for_test(COMMUNITY, [join, kick]);

        let _ = state.materialized(admin); // populate the cache
        assert_eq!(
            state.cache_recompute_after_for_test(),
            Some(kick_wall - crate::clock_trust::MAX_FORWARD_SKEW_MS),
            "recompute_after must mark exactly when the sole deferred Kick becomes plausible"
        );
    }

    #[test]
    fn cache_policy_invalidates_on_ceiling_mode_flip() {
        // Greptile P1: the cache must not reuse a view built under a different
        // ceiling MODE (receiver-clock availability).
        //
        // None→Some (clock recovers): an apply-all view (built ceiling-disabled)
        // must NOT survive — it would keep INCLUDING a future poison event the
        // recovered clock should now bound out (under-exclusion).
        assert!(
            !cache_policy_still_valid(Some(false), true, None, Some(1_000)),
            "apply-all view must be invalidated once the clock recovers (else under-exclusion)"
        );
        // Some→None (clock breaks): a bounded view (built ceiling-enabled) must
        // NOT survive — it would keep DEFERRING honest governance after the
        // clock is lost, violating the apply-all-on-bad-clock invariant.
        assert!(
            !cache_policy_still_valid(Some(true), false, Some(1_000), None),
            "bounded view must be invalidated once the clock is lost (else honest governance stays dropped)"
        );
        // Same mode + time still valid ⇒ hit.
        assert!(
            cache_policy_still_valid(Some(true), true, Some(1_000), Some(999)),
            "same enabled mode, deferred event not yet plausible ⇒ still valid"
        );
        assert!(
            cache_policy_still_valid(Some(false), false, None, None),
            "same disabled mode ⇒ still valid"
        );
        // Same enabled mode but the deferred event has entered the window ⇒
        // the time axis still forces a recompute.
        assert!(
            !cache_policy_still_valid(Some(true), true, Some(1_000), Some(1_000)),
            "same mode but deferred event now plausible ⇒ recompute"
        );
        // A never-populated cache is never a hit.
        assert!(
            !cache_policy_still_valid(None, true, None, Some(1_000)),
            "unpopulated cache (cached_ceiling_enabled None) is never a hit"
        );
    }

    #[test]
    fn materialized_records_enabled_ceiling_mode() {
        // Wiring proof: on any real host the clock is available, so a populated
        // cache records the ENABLED mode — the value the mode gate compares
        // against on the next read.
        let (state, admin, _victim) = state_with_join_and_future_kick();
        let _ = state.materialized(admin);
        assert_eq!(
            state.cache_ceiling_enabled_for_test(),
            Some(true),
            "a real-clock materialize must record the ceiling as enabled"
        );
    }

    #[test]
    fn materialized_leaves_recompute_after_none_when_nothing_is_deferred() {
        // No future-dated event ⇒ the cached view is time-insensitive and the
        // version key alone governs validity (no wall-clock invalidation cost on
        // the common path).
        let admin = addr(1);
        let member = addr(2);
        let now = real_now_ms();
        let join = join_event(10, member, now - 20_000);
        let state = CommunityState::from_trusted_events_for_test(COMMUNITY, [join]);

        let _ = state.materialized(admin);
        assert_eq!(
            state.cache_recompute_after_for_test(),
            None,
            "no future-dated event ⇒ recompute_after stays None"
        );
    }
}
