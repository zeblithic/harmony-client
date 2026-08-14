//! ZEB-321 Phase 1: side-projection of ReachabilityAnnounce CRDT events.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.4
//! (LWW projection) and §7.4 (resolver consumed by zenoh-over-iroh transport).
//!
//! ## Multi-device keying (PR #157 round 5)
//!
//! Each harmony owner identity (ZEB-173) may be bound to multiple devices,
//! each with its own iroh `EndpointId`. The resolver is therefore keyed
//! by the pair `(OwnerAddr, iroh_node_id)`, not by `OwnerAddr` alone —
//! otherwise device B's announce would overwrite device A's, and only
//! the most-recently-announced device would be dialable. The LWW
//! comparator runs per-key, so each (owner, device) pair maintains its
//! own latest record independently.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use harmony_reachability::{lww_newer as core_lww_newer, MultiDeviceMap};

use crate::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr};
use crate::peer_liveness::LivenessHandle;
use crate::reachability_record::ReachabilityAnnouncePayload;
use crate::reconnect_supervisor::{ReconnectTrigger, SupervisorHandle};

/// ZEB-621: a freshest dial view whose `announced_at_ms` is older than this
/// (24h) is considered stale enough that the dial route may be pinned to a
/// long-dead address — [`ReachabilityResolver::maybe_refresh_stale`] then kicks
/// off an async pkarr re-resolve to heal it.
pub(crate) const STALE_RECORD_REFRESH_MS: u64 = 24 * 60 * 60 * 1000;

/// ZEB-621: minimum spacing between async pkarr re-resolves for the same owner.
/// A stale record that the supervisor re-observes on every dispatch must not
/// hammer pkarr — after one refresh fires, further `maybe_refresh_stale` calls
/// for that owner are suppressed until this cooldown lapses (measured on the
/// paused-time-testable `tokio::time::Instant` clock).
pub(crate) const PKARR_REFRESH_COOLDOWN: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);

/// ZEB-621: forward clock-skew allowance for freshness timestamps. A record
/// claiming to be authored more than this far in the future (benign clock skew
/// or a bogus/hostile record) is clamped to `now + FUTURE_SKEW_TOLERANCE_MS` at
/// insert time so it cannot permanently dominate the freshest dial view or
/// same-source LWW and starve out later, correct records. For realistic records
/// (`announced_at_ms <= now`) the clamp is a no-op.
pub(crate) const FUTURE_SKEW_TOLERANCE_MS: u64 = crate::clock_trust::MAX_FORWARD_SKEW_MS;

/// ZEB-621: maximum number of concurrent background pkarr refresh network calls
/// across the whole resolver (see [`ReachabilityResolver::maybe_refresh_stale`]).
/// Bounds instantaneous fan-out when a reconnect sweep finds many stale peers due
/// at once; per-owner cooldowns separately bound the per-owner rate.
pub(crate) const PKARR_REFRESH_MAX_CONCURRENT: usize = 4;

/// Default wall clock for [`ReachabilityResolver`]: milliseconds since the Unix
/// epoch. Injectable via `set_clock` in tests so the future-skew clamp
/// (ZEB-621) can be driven against a controllable "now".
fn default_clock() -> Arc<dyn Fn() -> u64 + Send + Sync> {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    })
}

/// Async fallback called by `resolve_async` when the in-memory CRDT cache has
/// no entry for a given owner. Implemented by `PkarrResolverAdapter` (case C)
/// and by test stubs. The concrete impl is injected at boot via
/// `set_fallback_source`.
///
/// ZEB-744: sourced from the core `harmony-reachability` kernel (generic over
/// the owner key); the client instantiates it at `Owner = OwnerAddr`. Re-exported
/// so existing `crate::reachability_resolver::ReachabilityFallback` paths resolve.
pub use harmony_reachability::ReachabilityFallback;

/// Composite key: harmony owner + iroh endpoint. Same-owner-different-
/// device entries coexist; same-owner-same-device updates are LWW.
type ResolverKey = (OwnerAddr, [u8; 32]);

/// Provenance of a resolved reachability payload, used to apply a per-source
/// butler-set freshness policy (Decision 3): durable replicated CRDT records
/// carry seal-targets that stay valid even when the recipient's primary is
/// long-offline, so they are exempt from the live pkarr freshness window;
/// pkarr-sourced records keep the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachabilitySource {
    /// Projected from a durable community-membership ReachabilityAnnounce CRDT
    /// event (persisted, replicated, boot-replayed).
    DurableCrdt,
    /// Fetched live from the recipient's pkarr routing blob.
    PkarrLive,
    /// ZEB-510: a same-owner fleet sibling's iroh endpoint. Fed from two
    /// verification-exempt sources, each carrying a zero-filled
    /// `identity_signature` (the ingest boundary is the trust boundary, not a
    /// per-record signature): (1) step 1 — the owner's durable `FleetNetDoc`
    /// (fleet_net.cbor), whose boundary is fleet-net's symmetric-key decrypt;
    /// (2) step 2 — the `fleet_peer_seed` store, whose boundary is the
    /// SAS-authenticated pairing channel the endpoint was observed on. The step-1
    /// FleetNetDoc row supersedes a step-2 seed for the same node via LWW once it
    /// exists (same stable node_id either way). Never present in `self_owner`'s
    /// pkarr blob (that is the deferred ZEB-513 cross-WAN path).
    FleetSibling,
}

impl ReachabilitySource {
    /// Stable camelCase tag for the `connectivity_list_peer_reachability` DTO so
    /// consumers (e.g. the e2e durable-replication barrier) can tell a durable
    /// CRDT-replicated record apart from a live pkarr cache-back.
    pub fn as_dto_str(self) -> &'static str {
        match self {
            ReachabilitySource::DurableCrdt => "durableCrdt",
            ReachabilitySource::PkarrLive => "pkarrLive",
            ReachabilitySource::FleetSibling => "fleetSibling",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverEntry {
    pub payload: ReachabilityAnnouncePayload,
    pub hlc: Hlc,
    pub source: ReachabilitySource,
    /// ZEB-621: `payload.announced_at_ms` clamped to at most
    /// `now + FUTURE_SKEW_TOLERANCE_MS`, computed once at insert time in
    /// [`ReachabilityResolver::update_with_source`]. The freshest dial view
    /// ([`ResolverSlots::freshest`]) and the
    /// [`maybe_refresh_stale`](ReachabilityResolver::maybe_refresh_stale)
    /// staleness gate compare THIS rather than the raw announced time, so a
    /// future-dated record cannot permanently pin the dial route. For realistic
    /// records (`announced_at_ms <= now`) it equals `payload.announced_at_ms`.
    pub effective_announced_at_ms: u64,
}

/// Dual-slot storage for one `(OwnerAddr, iroh_node_id)` key (ZEB-621).
///
/// The durable-CRDT record and the live-pkarr record for the same peer are held
/// in SEPARATE cells rather than competing for a single slot: each source writes
/// only its own slot (same-source replacement is ordinary [`lww_newer`] LWW), so
/// a pkarr cache-back can never evict a durable record and vice versa. Two views
/// derive from the pair:
/// - [`durable_preferred`](Self::durable_preferred) — the butler / diagnostics
///   authority. The durable slot's seal-targets stay valid even when the
///   recipient's primary is long-offline (window-EXEMPT, ZEB-488), so butler /
///   diag / e2e consumers keep seeing it whenever it exists.
/// - [`freshest`](Self::freshest) — the DIAL authority. A dialer wants the
///   record with the most recent addressing, whichever source carries it, so a
///   fresher pkarr blob can win the route while the durable slot still backs the
///   butler view.
#[derive(Debug, Clone, Default)]
struct ResolverSlots {
    durable: Option<ResolverEntry>,
    pkarr: Option<ResolverEntry>,
    /// ZEB-510: same-owner fleet-sibling endpoint, seeded from `FleetNetDoc`.
    /// A DISTINCT slot (not `durable`) because a sibling that is also a shared-
    /// community co-member lands its community `DurableCrdt` record under the
    /// SAME resolver key `(self_owner, sibling_node_id)`; sharing a slot would
    /// let the two sources clobber each other, breaking the per-source-slot
    /// invariant this dual/tri-slot storage exists to protect.
    fleet: Option<ResolverEntry>,
}

impl ResolverSlots {
    /// Dial authority: the entry whose payload was announced most recently
    /// (greater `effective_announced_at_ms` — the future-skew-clamped announce
    /// time, ZEB-621). Ties break by source authority (durable > pkarr > fleet)
    /// so a verified community record still wins a tie against an unsigned
    /// fleet-sibling one, and the result is deterministic. `None` only for an
    /// empty triple (never stored — `update_with_source` writes at least one
    /// slot before any entry lands in the map).
    fn freshest(&self) -> Option<&ResolverEntry> {
        [
            self.durable.as_ref(),
            self.pkarr.as_ref(),
            self.fleet.as_ref(),
        ]
        .into_iter()
        .flatten()
        .max_by(|a, b| {
            a.effective_announced_at_ms
                .cmp(&b.effective_announced_at_ms)
                .then_with(|| source_rank(a.source).cmp(&source_rank(b.source)))
        })
    }

    /// Butler / diagnostics authority: the durable slot if present, else pkarr.
    /// ZEB-510: the `fleet` slot is deliberately EXCLUDED — fleet entries carry
    /// an empty `butler_set` and are dial-only, so they must not shape the
    /// butler-set / diagnostics view. (Excluding them here also keeps a stale
    /// fleet entry out of the `maybe_refresh_stale` pkarr path when a key has no
    /// durable/pkarr slot at all.)
    fn durable_preferred(&self) -> Option<&ResolverEntry> {
        self.durable.as_ref().or(self.pkarr.as_ref())
    }
}

/// Source authority for freshness ties: durable > pkarr > fleet — a verified
/// community record beats an unsigned fleet-sibling one at equal announce
/// time. Shared by the within-entry slot pick ([`ResolverSlots::freshest`])
/// and the ZEB-704 cross-owner reverse lookups ([`freshest_across_owners`]) so
/// both views break ties the same way.
fn source_rank(s: ReachabilitySource) -> u8 {
    match s {
        ReachabilitySource::DurableCrdt => 2,
        ReachabilitySource::PkarrLive => 1,
        ReachabilitySource::FleetSibling => 0,
    }
}

/// ZEB-704: the globally freshest entry matching `node_id_bytes` ACROSS
/// owners. The reverse lookups previously stopped at the FIRST
/// `(owner, node_id)` key in owner-address order and only ran the slot pick
/// within that entry — so when the same iroh node-id existed under multiple
/// owners, the dial route could diverge from the boot-seed recency ranking
/// (`boot_seed_node_ids_by_recency`, keep-freshest-across-owners).
/// Ordering: `effective_announced_at_ms`, ties → [`source_rank`] (mirrors the
/// slot pick), remaining ties → the first owner in BTreeMap order (replace
/// only on strictly-greater), keeping the degenerate all-tie case
/// deterministic and identical to the old behavior.
fn freshest_across_owners<'a>(
    map: &'a MultiDeviceMap<OwnerAddr, ResolverSlots>,
    node_id_bytes: &'a [u8; 32],
) -> Option<(OwnerAddr, &'a ResolverEntry)> {
    map.find_by_node_id(node_id_bytes)
        .filter_map(|((owner, _), v)| v.freshest().map(|e| (*owner, e)))
        .reduce(|best, cand| {
            let ord = cand
                .1
                .effective_announced_at_ms
                .cmp(&best.1.effective_announced_at_ms)
                .then_with(|| source_rank(cand.1.source).cmp(&source_rank(best.1.source)));
            if ord == std::cmp::Ordering::Greater {
                cand
            } else {
                best
            }
        })
}

pub struct ReachabilityResolver {
    inner: Arc<RwLock<MultiDeviceMap<OwnerAddr, ResolverSlots>>>,
    /// Wrapped in an outer `Arc` so that all clones share the same
    /// `RwLock` — wiring the fallback via `set_fallback_source` on any
    /// clone is immediately visible to all others (CodeRabbit PR #158 round 2).
    fallback_source: Arc<RwLock<Option<Arc<dyn ReachabilityFallback<OwnerAddr>>>>>,
    // ZEB-620: optional reconnect-supervisor handle. None until boot installs it
    // via `set_supervisor`. Kicked `NewPeer` on first-learn and `RecordChanged`
    // on a material LWW record change — the sole successor to ZEB-373's retired
    // `DialHint` mpsc (the dial-once dial driver).
    supervisor: Arc<RwLock<Option<SupervisorHandle>>>,
    // ZEB-622: optional peer-liveness handle. None until boot installs it via
    // `set_liveness`. Mirrors `supervisor` exactly (shared `Arc<RwLock>`, latest
    // install wins, read-only clone getter); the Network Health snapshot reads
    // its `states_snapshot` for per-peer liveness telemetry.
    liveness: Arc<RwLock<Option<LivenessHandle>>>,
    // ZEB-621: per-owner cooldown clock for `maybe_refresh_stale`'s async pkarr
    // re-resolve. Shared across clones (`Arc`) like the other fields, so a
    // refresh fired through one clone rate-limits every clone. Keyed by owner
    // (not `(owner, node_id)`) so one owner's whole address set re-resolves at
    // most once per `PKARR_REFRESH_COOLDOWN`. `tokio::time::Instant` → paused-
    // time testable.
    refresh_cooldowns:
        Arc<std::sync::Mutex<std::collections::HashMap<OwnerAddr, tokio::time::Instant>>>,
    // ZEB-621: wall clock (Unix-epoch ms) consulted by `update_with_source` to
    // compute each entry's `effective_announced_at_ms` (future-skew clamp).
    // Wrapped `Arc<RwLock<Arc<..>>>` so it is (a) shared across clones like the
    // other fields — every clone reads the same clock — and (b) swappable via
    // `set_clock` in tests without a `&mut self`. Production keeps `default_clock`
    // (`SystemTime`); tests inject a controllable "now".
    clock: Arc<RwLock<Arc<dyn Fn() -> u64 + Send + Sync>>>,
    // ZEB-621: bounds the instantaneous fan-out of background pkarr refresh I/O.
    // A post-address-change `kick_sweep` can make the whole roster due in one
    // dispatch pass, so many `maybe_refresh_stale` calls could each spawn a pkarr
    // `resolve` at once. The per-owner cooldown bounds the RATE per owner; this
    // semaphore bounds the total number of refresh network calls in flight across
    // all owners. Shared across clones (`Arc`) so the bound is global.
    refresh_permits: Arc<tokio::sync::Semaphore>,
    // ZEB-627: monotonic change counter for the peer-record map — bumped on
    // every MATERIALIZED update (LWW-accepted slot write) and on any
    // non-empty `remove_owner`. Generation-keyed caches over derived views
    // (event_loop's zid→node map) compare it to decide when to rebuild; a
    // bump covers both stale directions (evicted/reassigned AND newly added).
    // Shared across clones (`Arc`) like every other field.
    generation: Arc<std::sync::atomic::AtomicU64>,
    // ZEB-928: optional R4 admission oracle. None until boot installs it via
    // `set_admission_oracle`. When present, the resolver forwards verified
    // `node_id -> enrolled_device_key` bindings (`note_enrolled_binding`) into it and
    // evicts them on `remove_owner`, so the supervisor's kick-time filter can classify
    // a node_id by its enrolled device key. Mirrors `supervisor` (shared `Arc<RwLock>`,
    // latest install wins).
    admission_oracle: Arc<RwLock<Option<Arc<crate::admission_oracle::AdmissionOracle>>>>,
}

impl std::fmt::Debug for ReachabilityResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReachabilityResolver")
            .field("inner", &self.inner)
            .field("fallback_source", &"<dyn ReachabilityFallback>")
            .finish()
    }
}

impl Default for ReachabilityResolver {
    fn default() -> Self {
        Self {
            inner: Arc::new(RwLock::new(MultiDeviceMap::new())),
            fallback_source: Arc::new(RwLock::new(None)),
            supervisor: Arc::new(RwLock::new(None)),
            liveness: Arc::new(RwLock::new(None)),
            refresh_cooldowns: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            clock: Arc::new(RwLock::new(default_clock())),
            refresh_permits: Arc::new(tokio::sync::Semaphore::new(PKARR_REFRESH_MAX_CONCURRENT)),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            admission_oracle: Arc::new(RwLock::new(None)),
        }
    }
}

impl Clone for ReachabilityResolver {
    fn clone(&self) -> Self {
        ReachabilityResolver {
            inner: Arc::clone(&self.inner),
            // Clone the Arc so all instances share the same RwLock.
            // Wiring the fallback on any clone is visible to all others
            // (CodeRabbit PR #158 round 2 correctness fix).
            fallback_source: Arc::clone(&self.fallback_source),
            supervisor: Arc::clone(&self.supervisor),
            liveness: Arc::clone(&self.liveness),
            refresh_cooldowns: Arc::clone(&self.refresh_cooldowns),
            clock: Arc::clone(&self.clock),
            refresh_permits: Arc::clone(&self.refresh_permits),
            generation: Arc::clone(&self.generation),
            admission_oracle: Arc::clone(&self.admission_oracle),
        }
    }
}

impl ReachabilityResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// ZEB-621: current wall-clock time (Unix-epoch ms) as seen by the future-skew
    /// clamp. Reads the injectable [`clock`](Self::clock); production uses
    /// [`default_clock`] (`SystemTime`).
    fn now_ms(&self) -> u64 {
        let clock = Arc::clone(&*self.clock.read().expect("resolver clock lock"));
        clock()
    }

    /// ZEB-621 (test-only): swap the wall clock consulted by the future-skew
    /// clamp so paused-time tests can drive a controllable "now". Shared across
    /// clones via the same `Arc<RwLock<..>>`, so an injection on any clone is
    /// visible to all.
    #[cfg(test)]
    pub(crate) fn set_clock(&self, f: Arc<dyn Fn() -> u64 + Send + Sync>) {
        *self.clock.write().expect("resolver clock lock") = f;
    }

    /// ZEB-620: install the reconnect-supervisor handle. Called once at boot
    /// (event_loop, right after the supervisor is spawned). The sole first-learn /
    /// record-change notify seam since ZEB-373's `DialHint` mpsc was retired.
    /// Thread-safe; latest install wins.
    pub fn set_supervisor(&self, handle: SupervisorHandle) {
        *self.supervisor.write().expect("supervisor lock") = Some(handle);
    }

    /// ZEB-620 Task 6: read the installed reconnect-supervisor handle, if any.
    /// `None` until boot installs it via [`set_supervisor`](Self::set_supervisor).
    /// Read-only clone of the shared handle — the Network Health snapshot uses it
    /// to read `states_snapshot` for dial-state telemetry (`SupervisorHandle` is
    /// a cheap `Arc`-backed clone).
    pub fn supervisor(&self) -> Option<SupervisorHandle> {
        self.supervisor.read().expect("supervisor lock").clone()
    }

    /// ZEB-627: current map generation (see the field doc). `Acquire` pairs
    /// with the mutators' `Release` bumps.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// ZEB-622: install the peer-liveness handle. Called once at boot
    /// (event_loop, alongside the reconnect-supervisor install). Mirrors
    /// [`set_supervisor`](Self::set_supervisor): thread-safe, latest install wins.
    pub fn set_liveness(&self, handle: LivenessHandle) {
        *self.liveness.write().expect("liveness lock") = Some(handle);
    }

    /// ZEB-622: read the installed peer-liveness handle, if any. `None` until
    /// boot installs it via [`set_liveness`](Self::set_liveness). Read-only clone
    /// of the shared handle — the Network Health snapshot uses it to read
    /// `states_snapshot` for per-peer liveness telemetry (`LivenessHandle` is a
    /// cheap `Arc`-backed clone).
    pub fn liveness(&self) -> Option<LivenessHandle> {
        self.liveness.read().expect("liveness lock").clone()
    }

    /// ZEB-928: install the R4 admission oracle. Called once at boot (event_loop),
    /// alongside the supervisor install. Mirrors [`set_supervisor`](Self::set_supervisor):
    /// thread-safe, latest install wins. `None` until then, in which case
    /// [`note_enrolled_binding`](Self::note_enrolled_binding) and the `remove_owner`
    /// eviction are no-ops.
    pub fn set_admission_oracle(&self, oracle: Arc<crate::admission_oracle::AdmissionOracle>) {
        *self
            .admission_oracle
            .write()
            .expect("admission_oracle lock") = Some(oracle);
    }

    /// ZEB-928: forward a verified `(owner, node_id) -> enrolled_device_key` binding into the
    /// admission oracle (no-op if none installed). Call this at each record-ingest seam that
    /// holds the enrolled key, BEFORE the `update`/`update_with_source` that fires the supervisor
    /// kick — so the just-resolved peer is classifiable the instant it is kicked (race-free
    /// admission). The owner scopes eviction so `remove_owner` never drops a co-resident owner's
    /// binding for a shared node_id.
    pub fn note_enrolled_binding(&self, owner: [u8; 16], node_id: [u8; 32], enrolled_vk: [u8; 32]) {
        if let Some(o) = self
            .admission_oracle
            .read()
            .expect("admission_oracle lock")
            .as_ref()
        {
            o.bind(owner, node_id, enrolled_vk);
        }
    }

    /// LWW update — higher HLC wins; ties broken by announced_at_ms then
    /// lexicographic iroh_node_id. See spec §5.4.
    ///
    /// Per PR #157 round 5: keyed by `(actor, payload.iroh_node_id)` so
    /// same-owner-different-device announces don't overwrite each other.
    ///
    /// CRDT-projection update (the default, durable source). All
    /// membership-apply call sites use this unchanged.
    pub fn update(&self, actor: OwnerAddr, payload: ReachabilityAnnouncePayload, hlc: Hlc) {
        self.update_with_source(actor, payload, hlc, ReachabilitySource::DurableCrdt);
    }

    /// Source-tagged update (Task 3, Decision 3). Only the pkarr-cache-back
    /// path in [`resolve_async`](Self::resolve_async) /
    /// [`resolve_async_with_source`](Self::resolve_async_with_source) passes
    /// `PkarrLive`; every membership-apply projection goes through
    /// [`update`](Self::update) and is therefore `DurableCrdt`.
    pub fn update_with_source(
        &self,
        actor: OwnerAddr,
        payload: ReachabilityAnnouncePayload,
        hlc: Hlc,
        source: ReachabilitySource,
    ) {
        let key: ResolverKey = (actor, payload.iroh_node_id);
        let node_id = payload.iroh_node_id;
        // ZEB-621 / ZEB-852 future-skew clamp: bound the announce time AND the HLC
        // wall time to `now + skew` so a future-dated record cannot permanently
        // dominate the freshest dial view (`effective_announced_at_ms`) or same-
        // source LWW (`hlc.wall_ms`) and block later correct records from
        // installing. Post-ZEB-815 the `DurableCrdt`/`FleetSibling` HLC is no longer
        // owner-authored-and-trusted: it arrives peer-signed via the address book
        // (`ingest_verified_row` feeds `DurableCrdt` a peer-signed `row.at`), so a
        // hostile future `wall_ms` would win LWW and pin the durable slot for
        // process life. All three sources are therefore clamped to the receiver's
        // control-tier skew ceiling (`FUTURE_SKEW_TOLERANCE_MS ==
        // clock_trust::MAX_FORWARD_SKEW_MS`). This resolver is a terminal, in-memory,
        // non-replicated projection, so clamping is receiver-independent in effect —
        // every peer re-clamps at its own ingest — which is why we clamp rather than
        // reject. Clock-unreadable handling is inherited from `self.now_ms()`
        // (fail-open, symmetric across all arms).
        let skew_ceiling = self.now_ms().saturating_add(FUTURE_SKEW_TOLERANCE_MS);
        let effective_announced_at_ms = payload.announced_at_ms.min(skew_ceiling);
        let hlc = match source {
            ReachabilitySource::PkarrLive => Hlc {
                wall_ms: hlc.wall_ms.min(skew_ceiling),
                ..hlc
            },
            ReachabilitySource::DurableCrdt | ReachabilitySource::FleetSibling => Hlc {
                wall_ms: hlc.wall_ms.min(skew_ceiling),
                ..hlc
            },
        };
        let next = ResolverEntry {
            payload,
            hlc,
            source,
            effective_announced_at_ms,
        };
        let mut map = self.inner.write().expect("resolver write lock");
        let slots = map.entry(key).or_default();
        // The reconnect-supervisor kick gate (ZEB-620/621) is evaluated against
        // the FRESHEST dial view, snapshotted before AND after the slot write:
        // `was_present` = any slot existed beforehand. Deriving `RecordChanged`
        // from the pre/post freshest addressing means a fresher pkarr record that
        // shifts the effective route fires it — the stale-route-unpin payoff —
        // while an LWW-rejected or byte-identical write leaves the view unchanged
        // and silent (no ladder thrash). The snapshot is the addressing-only
        // `addr_key` (ZEB-621 M1), not a full `payload.clone()`: that is all the
        // gate compares, and it keeps the butler_set / signature copies off every
        // CRDT apply and pkarr cache-back.
        let was_present = slots.durable.is_some() || slots.pkarr.is_some() || slots.fleet.is_some();
        let before_view = slots.freshest().map(|e| addr_key(&e.payload));
        // Each source writes ONLY its own slot; same-source replacement is LWW.
        let target = match source {
            ReachabilitySource::DurableCrdt => &mut slots.durable,
            ReachabilitySource::PkarrLive => &mut slots.pkarr,
            ReachabilitySource::FleetSibling => &mut slots.fleet,
        };
        let do_replace = match target.as_ref() {
            Some(prev) => lww_newer(prev, &next),
            None => true,
        };
        if do_replace {
            *target = Some(next);
            // ZEB-627: the derived zid→node view may have changed.
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        // When `do_replace` is false the freshest view is unchanged, so the after
        // snapshot equals the before one — skip re-reading it.
        let after_view = if do_replace {
            slots.freshest().map(|e| addr_key(&e.payload))
        } else {
            before_view.clone()
        };
        drop(map);
        // ZEB-620/621: reconnect-supervisor triggers (the successor to ZEB-373's
        // retired `DialHint` mpsc). A first-learn kicks `NewPeer`; a change in the
        // effective dial addressing on an already-known peer kicks
        // `RecordChanged`. Mutually exclusive by construction (`was_present`), so
        // at most one fires per update; `kick` is lossless and non-blocking.
        if let Some(sup) = self.supervisor.read().expect("supervisor lock").as_ref() {
            match (was_present, before_view, after_view) {
                (false, _, Some(_)) => sup.kick(node_id, ReconnectTrigger::NewPeer),
                (true, Some(before), Some(after)) if before != after => {
                    sup.kick(node_id, ReconnectTrigger::RecordChanged)
                }
                _ => {}
            }
        }
    }

    /// Returns ALL device records for `actor`. Multi-device owners
    /// (per ZEB-173) may have multiple entries; callers that need to
    /// dial the owner should try each in turn (Phase 2 will add ranking
    /// based on heartbeat/liveness).
    pub fn resolve(&self, actor: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
        let map = self.inner.read().expect("resolver read lock");
        map.range_owner(actor)
            .filter_map(|(_, v)| v.durable_preferred().map(|e| e.payload.clone()))
            .collect()
    }

    pub fn list_active_peers(&self) -> Vec<(OwnerAddr, ReachabilityAnnouncePayload)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter()
            .filter_map(|((owner, _node_id), v)| {
                v.durable_preferred().map(|e| (*owner, e.payload.clone()))
            })
            .collect()
    }

    /// ZEB-621 / D1: like [`list_active_peers`](Self::list_active_peers), but
    /// pairs each peer with its future-skew-clamped `effective_announced_at_ms`
    /// (`announced_at_ms` capped at `now + FUTURE_SKEW_TOLERANCE_MS`).
    /// Diagnostics (network-health "last seen") read THIS so a future-dated
    /// record cannot report a peer as "seen 0 s ago". Backed by
    /// `durable_preferred`, matching [`list_active_peers`](Self::list_active_peers).
    pub fn list_active_peers_effective(
        &self,
    ) -> Vec<(OwnerAddr, ReachabilityAnnouncePayload, u64)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter()
            .filter_map(|((owner, _node_id), v)| {
                v.durable_preferred()
                    .map(|e| (*owner, e.payload.clone(), e.effective_announced_at_ms))
            })
            .collect()
    }

    /// Like [`list_active_peers`](Self::list_active_peers), but carries each
    /// entry's [`ReachabilitySource`] so diagnostics / the e2e durable-sync
    /// barrier can distinguish a durable CRDT-replicated record from a live
    /// pkarr cache-back.
    pub fn list_active_peers_with_source(
        &self,
    ) -> Vec<(OwnerAddr, ReachabilityAnnouncePayload, ReachabilitySource)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter()
            .filter_map(|((owner, _node_id), v)| {
                v.durable_preferred()
                    .map(|e| (*owner, e.payload.clone(), e.source))
            })
            .collect()
    }

    /// ZEB-702: the DIAL view — the freshest [`ResolverEntry`] per
    /// `(owner, node_id)` across ALL three slots (durable / pkarr / fleet), one
    /// row per (owner, device). Unlike [`list_active_peers`](Self::list_active_peers)
    /// / [`list_active_peers_with_source`](Self::list_active_peers_with_source)
    /// (both backed by [`durable_preferred`](ResolverSlots::durable_preferred),
    /// which EXCLUDES the fleet slot), this uses
    /// [`freshest`](ResolverSlots::freshest), so a fleet-slot-only sibling — e.g.
    /// a SAS-paired cert-only butler known only from pairing — appears here.
    ///
    /// The boot-seed reconnect enumeration
    /// ([`crate::iroh_zenoh_registration::boot_seed_node_ids_by_recency`]) reads
    /// THIS view so such siblings are re-dialed at every boot. `durable_preferred`
    /// is deliberately NOT widened: it backs [`resolve`](Self::resolve) /
    /// `list_active_peers` whose callers (dial-by-owner, diagnostics, e2e
    /// barriers) depend on durable/pkarr semantics. This view is additive.
    pub fn list_dialable_peers(&self) -> Vec<(OwnerAddr, ResolverEntry)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter()
            .filter_map(|((owner, _node_id), v)| v.freshest().map(|e| (*owner, e.clone())))
            .collect()
    }

    /// Reverse lookup: given an iroh `EndpointId` byte representation,
    /// find the matching `(OwnerAddr, payload)` entry.
    ///
    /// Used by [`crate::zenoh_iroh_transport::IrohZenohLinkManager`] (Task 6),
    /// where Zenoh hands us a locator carrying the iroh `EndpointId`
    /// (not the harmony `OwnerAddr`) — see spec §7.3.
    ///
    /// Per PR #157 round 5: the composite-key map could enable an O(log N)
    /// secondary-index lookup, but we keep the linear scan for now since
    /// `N` (joined community member count × device count) is expected to
    /// stay under ~10⁴ in Phase 1. Phase 2 should profile and add a
    /// `BTreeMap<[u8; 32], OwnerAddr>` secondary index if hot.
    /// ZEB-704: when the same node-id exists under multiple owners, the
    /// globally freshest matching entry wins (see [`freshest_across_owners`]) —
    /// keeping this dial view consistent with the boot-seed recency ranking.
    pub fn resolve_by_node_id(
        &self,
        node_id_bytes: &[u8; 32],
    ) -> Option<(OwnerAddr, ReachabilityAnnouncePayload)> {
        let map = self.inner.read().expect("resolver read lock");
        freshest_across_owners(&map, node_id_bytes).map(|(owner, e)| (owner, e.payload.clone()))
    }

    /// Freshest-view reverse lookup for the DIAL path (ZEB-621; consumed by
    /// Task 2). Like [`resolve_by_node_id`](Self::resolve_by_node_id) but returns
    /// the full [`ResolverEntry`] — source + HLC + timestamps — so the dialer can
    /// inspect the winning record's provenance and freshness, not just its
    /// payload. Returns the entry with the most recent `effective_announced_at_ms`
    /// (future-skew-clamped announce time, ZEB-621) across the peer's durable,
    /// pkarr, and fleet slots (ties → durable), matching `resolve_by_node_id`'s
    /// freshest-wins dial semantics. (ZEB-510 added the fleet slot, which
    /// `freshest()` includes.)
    pub fn resolve_entry_by_node_id(
        &self,
        node_id_bytes: &[u8; 32],
    ) -> Option<(OwnerAddr, ResolverEntry)> {
        let map = self.inner.read().expect("resolver read lock");
        freshest_across_owners(&map, node_id_bytes).map(|(owner, e)| (owner, e.clone()))
    }

    /// ZEB-621: heal a stale dial route by kicking off an async pkarr
    /// re-resolve. Sync + non-blocking: the staleness + cooldown checks run
    /// inline, and the actual pkarr `resolve` + cache write happen in a spawned
    /// task, so the caller (the supervisor dispatch pass) is never gated or
    /// delayed by network I/O.
    ///
    /// The staleness gate is the FRESHEST dial view's age, source-agnostic
    /// (Decision, ZEB-621): a >24h pkarr record is as worth re-resolving as a
    /// durable one. The refresh feeds [`update_with_source`](Self::update_with_source)
    /// tagged [`ReachabilitySource::PkarrLive`], so a fresher record changes the
    /// dial view and auto-fires the `RecordChanged` supervisor kick through Task
    /// 1's gate — this seam adds no kick logic of its own.
    pub fn maybe_refresh_stale(&self, owner: OwnerAddr, node_id: [u8; 32], now_ms: u64) {
        self.refresh_if_older_than(owner, node_id, now_ms, STALE_RECORD_REFRESH_MS)
    }

    /// PR #659 review (CodeAnt): the composite-key freshness read for
    /// [`refresh_if_older_than`](Self::refresh_if_older_than) — the exact
    /// (owner, node) row the caller names, NEVER the cross-owner freshest row
    /// for the node. Under the ZEB-704 seed-owner split one node id can
    /// legitimately live under two owners (beacon-seeded composite owner +
    /// gossip master owner), and a fleet-sibling row for the same endpoint
    /// must not veto a community owner's pkarr refresh.
    fn resolve_entry_for(&self, owner: &OwnerAddr, node_id: &[u8; 32]) -> Option<ResolverEntry> {
        let map = self.inner.read().expect("resolver read lock");
        let entry = map
            .range_owner(owner)
            .find(|(key, _)| key.1 == *node_id)
            .and_then(|(_, v)| v.freshest().cloned());
        entry
    }

    /// ZEB-910: [`maybe_refresh_stale`](Self::maybe_refresh_stale) with the
    /// staleness bar as a parameter. Repair callers (the gateway dial driver's
    /// Degraded/Starved passes) lower ONLY the staleness gate; the fleet-sibling
    /// skip, no-fallback bail, per-owner [`PKARR_REFRESH_COOLDOWN`], and
    /// [`PKARR_REFRESH_MAX_CONCURRENT`] semaphore are deliberately kept — a
    /// "forced" refresh is still rate-bounded, because repair passes repeat on a
    /// ladder and an ungated refresh would hammer pkarr for members that are
    /// simply asleep.
    pub(crate) fn refresh_if_older_than(
        &self,
        owner: OwnerAddr,
        node_id: [u8; 32],
        now_ms: u64,
        older_than_ms: u64,
    ) {
        // (1) Staleness gate against the freshest view's clamped age. No record ⇒
        //     nothing to heal. The comparison uses `effective_announced_at_ms`
        //     (`announced_at_ms` clamped to `now + FUTURE_SKEW_TOLERANCE_MS` at
        //     insert time, ZEB-621 future-skew fix): a bogus future-dated record's
        //     effective time is at most `skew` ahead of `now_ms`, which still
        //     yields `None` from `checked_sub` and so is treated as stale — worth
        //     a re-resolve. For realistic data (`effective <= now_ms`) this is
        //     exactly `now_ms - effective_announced_at_ms`. The read is
        //     composite-keyed (PR #659 review): the caller's (owner, node)
        //     row, not the cross-owner freshest row for the node.
        let Some(entry) = self.resolve_entry_for(&owner, &node_id) else {
            return;
        };
        // ZEB-510: a fleet-sibling entry is a same-owner LAN/fleet-net record,
        // never present in `self_owner`'s pkarr blob. A pkarr re-resolve for
        // `self_owner` would fetch P's OWN record (or no-op), never the
        // sibling's endpoint — so never refresh a fleet entry here. (Cross-WAN
        // sibling rendezvous over pkarr is the deferred ZEB-513 path.)
        if entry.source == ReachabilitySource::FleetSibling {
            return;
        }
        match now_ms.checked_sub(entry.effective_announced_at_ms) {
            Some(age) if age <= older_than_ms => return, // fresh under this caller's bar
            _ => {} // stale (older than the window) or implausibly future-dated
        }

        // (2) Clone the fallback Arc (as `resolve_async` does). No fallback
        //     wired ⇒ nothing to re-resolve with, so bail BEFORE the cooldown
        //     check-and-set (ZEB-621 M2): a no-fallback path must not burn the
        //     owner's cooldown or accumulate a map entry it can never act on.
        let fb = {
            let guard = self
                .fallback_source
                .read()
                .expect("fallback_source poisoned");
            guard.clone()
        };
        let Some(fb) = fb else {
            return;
        };

        // (3) Per-owner cooldown check-and-set. Insert BEFORE spawning so two
        //     concurrent callers (e.g. two due dials in one dispatch pass) can't
        //     both fire a refresh for the same owner inside one window.
        {
            let mut cooldowns = self
                .refresh_cooldowns
                .lock()
                .expect("refresh_cooldowns lock");
            let now = tokio::time::Instant::now();
            if let Some(&last) = cooldowns.get(&owner) {
                if now.saturating_duration_since(last) < PKARR_REFRESH_COOLDOWN {
                    return; // a refresh for this owner fired within the cooldown
                }
            }
            cooldowns.insert(owner, now);
        }

        // (4) Fire-and-forget async re-resolve. Each returned payload is fed
        //     through `update_with_source(.., PkarrLive)` with the same HLC
        //     synthesis as `resolve_async` (wall_ms = announced_at_ms, logical
        //     0, device_id "").
        //
        //     The global refresh-concurrency permit is acquired INSIDE the task,
        //     just before the network call (ZEB-621): task spawn stays cheap, but
        //     the number of pkarr `resolve` calls actually in flight is bounded by
        //     `PKARR_REFRESH_MAX_CONCURRENT` even when a reconnect sweep makes the
        //     whole roster due at once. A closed semaphore (never closed in
        //     practice) just skips the refresh.
        let resolver = self.clone();
        let permits = Arc::clone(&self.refresh_permits);
        tokio::spawn(async move {
            let _permit = match permits.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let payloads = fb.resolve(&owner).await;
            for payload in payloads {
                let hlc = Hlc {
                    wall_ms: payload.announced_at_ms,
                    logical: 0,
                    device_id: String::new(),
                };
                resolver.update_with_source(owner, payload, hlc, ReachabilitySource::PkarrLive);
            }
        });
    }

    /// Remove ALL device records for `actor`, returning the iroh node-ids
    /// of the removed records. Called from the membership-delta consumer on
    /// Leave / Kick events (PR #157 round 5 Cursor finding) so departed
    /// members don't linger in `connectivity_list_peer_reachability` or
    /// `resolve_by_node_id` until restart bootstrap re-filters them.
    ///
    /// ZEB-643: the decide-and-remove pair executes
    /// under a single write-lock hold, so the returned set is the
    /// authoritative deleted set — a concurrent `update` for the same owner
    /// cannot slip a record between a caller's separate capture read and
    /// this removal (the old resolve→remove_owner two-step raced exactly
    /// that way, leaking a record-less supervisor slot via the pending
    /// NewPeer kick of the uncaptured device). Callers evict supervisor
    /// slots from the returned set.
    pub fn remove_owner(&self, actor: &OwnerAddr) -> Vec<[u8; 32]> {
        let mut map = self.inner.write().expect("resolver write lock");
        let to_remove: Vec<ResolverKey> = map.range_owner(actor).map(|(k, _)| *k).collect();
        for k in &to_remove {
            map.remove(k);
        }
        if !to_remove.is_empty() {
            // ZEB-627: departed devices left the derived views.
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        drop(map);
        // Full per-owner eviction (ZEB-621): drop the stale-refresh cooldown
        // too, else the map grows monotonically with every stale-dispatched
        // owner over process lifetime.
        self.refresh_cooldowns
            .lock()
            .expect("refresh_cooldowns lock")
            .remove(actor);
        let node_ids: Vec<[u8; 32]> = to_remove.into_iter().map(|(_, node_id)| node_id).collect();
        // ZEB-928: drop only THIS owner's oracle bindings for the departed node_ids — a
        // node_id co-asserted under another live owner keeps that owner's binding (CR-2).
        if let Some(o) = self
            .admission_oracle
            .read()
            .expect("admission_oracle lock")
            .as_ref()
        {
            o.unbind_owner(actor.0, &node_ids);
        }
        node_ids
    }

    /// Register a pkarr-backed fallback source. Called once at boot by
    /// lib.rs after wiring `PkarrResolverAdapter`. Thread-safe; may be
    /// called any number of times (latest wins).
    pub fn set_fallback_source(&self, fb: Arc<dyn ReachabilityFallback<OwnerAddr>>) {
        *self
            .fallback_source
            .write()
            .expect("fallback_source poisoned") = Some(fb);
    }

    /// ZEB-325 Phase 2c (Task 2): seed the in-memory routing map with a
    /// pkarr-resolved record so the synchronous `resolve_by_node_id`
    /// path (used by Phase 1's `IrohZenohLinkManager.new_link`) can
    /// find the inviter's iroh routing on demand.
    ///
    /// Called by `connectivity_redeem_invite_iroh` immediately after a
    /// pkarr record has been verified, so the subsequent
    /// `redeem_invite_inner` call's CRDT-sync `PendingJoin` publish has
    /// a route through `IrohZenohLinkManager`. Distinct from the Phase 2b
    /// async fallback hook (`set_fallback_source`): that hook fires on
    /// `resolve_async` cache misses for ongoing routing resolution; this
    /// is a deterministic one-shot pre-seed before invoking the redemption
    /// flow.
    ///
    /// The `device_hash` parameter records the inviter's bound-device
    /// identity for API parity with the caller in
    /// `connectivity_redeem_invite_iroh` (Task 3). The `enrolled_vk` parameter
    /// (ZEB-930 Part 2) is the enrolled Ed25519 signing key to bind
    /// `node_id → enrolled_vk` into the admission oracle before the auto-kick —
    /// a DIFFERENT notion from `device_hash` (device address). `Some` only from a
    /// caller holding a membership-verified key (the beacon path); `None`
    /// otherwise (invite-redeem), preserving fail-open. The resolver's
    /// composite key uses `payload.iroh_node_id` (matching `update()`),
    /// because the Phase 1 transport reaches peers exclusively via
    /// `EndpointId` — see spec §7.3 and the docstring on
    /// `resolve_by_node_id` above.
    ///
    /// Tagged [`ReachabilitySource::PkarrLive`] — this is a pkarr-resolved
    /// record, so its butler-set must stay subject to the live 15-min freshness
    /// window, not the durable-CRDT exemption (Qodo "Api mismatch": routing the
    /// seed through `update()` would mis-tag it `DurableCrdt`). Uses the same HLC
    /// construction pattern as `resolve_async`'s cache population
    /// (`wall_ms = payload.announced_at_ms, logical = 0, device_id = ""`); a
    /// later durable-CRDT record for the same `(owner, node_id)` installs into
    /// its own durable slot (ZEB-621 dual-slot storage), so the butler view
    /// (durable-preferred) returns it while the dial view (freshest) keeps
    /// whichever record was announced most recently.
    ///
    /// `async` for API alignment with the Task 3 IPC handler (which
    /// `.await`s this call between two sync-locked sections). The
    /// implementation itself is non-blocking — the underlying `RwLock`
    /// is `std::sync`, not `tokio::sync`.
    pub async fn seed_from_pkarr(
        &self,
        owner_addr: OwnerAddr,
        _device_hash: DeviceIdentityHash,
        enrolled_vk: Option<[u8; 32]>,
        payload: ReachabilityAnnouncePayload,
    ) {
        // ZEB-930 Part 2: forward the vouch-verified enrolled key into the
        // admission oracle BEFORE the update fires the supervisor auto-kick, so
        // the seeded peer is bounded-degree-classifiable the instant it is kicked
        // (race-free admission). Only callers holding a membership-verified key
        // pass `Some`; the invite-redeem callers pass `None` and stay fail-open.
        // `_device_hash` is the device-ADDRESS notion (`DeviceIdentityHash`) —
        // NOT this enrolled Ed25519 vk; the two must never converge.
        if let Some(vk) = enrolled_vk {
            self.note_enrolled_binding(owner_addr.0, payload.iroh_node_id, vk);
        }
        let hlc = Hlc {
            wall_ms: payload.announced_at_ms,
            logical: 0,
            device_id: String::new(),
        };
        self.update_with_source(owner_addr, payload, hlc, ReachabilitySource::PkarrLive);
    }

    /// Async resolve: checks the in-memory CRDT cache first (via the
    /// existing sync `resolve()`), then falls back to the registered
    /// `ReachabilityFallback` (e.g. pkarr) on a cache miss. Fallback
    /// payloads are inserted into the cache so subsequent sync `resolve()`
    /// calls return them without hitting pkarr again.
    ///
    /// The cache-population HLC is constructed with `wall_ms` equal to
    /// the payload's `announced_at_ms`; logical=0, device_id="". This
    /// makes pkarr-sourced entries subordinate to any CRDT-sourced entry
    /// with a higher HLC — the existing LWW logic in `update()` handles
    /// the ordering correctly.
    pub async fn resolve_async(&self, addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
        // 1. Sync cache check.
        let cached = self.resolve(addr);
        if !cached.is_empty() {
            return cached;
        }
        // 2. Fallback to pkarr if configured.
        let fb = {
            let guard = self
                .fallback_source
                .read()
                .expect("fallback_source poisoned");
            guard.clone()
        };
        let Some(fb) = fb else {
            return Vec::new();
        };
        let payloads = fb.resolve(addr).await;
        // 3. Populate cache so subsequent sync resolves hit warm. Tagged
        //    `PkarrLive` (Task 3, Decision 3) so the per-source freshness
        //    policy keeps the live window on these — unlike CRDT-projected
        //    entries, which are window-exempt.
        for payload in &payloads {
            let hlc = Hlc {
                wall_ms: payload.announced_at_ms,
                logical: 0,
                device_id: String::new(),
            };
            self.update_with_source(*addr, payload.clone(), hlc, ReachabilitySource::PkarrLive);
        }
        payloads
    }

    /// ZEB-910 (PR #659 review): bounded cache-miss discovery for a member with
    /// NO resolver rows — the record-less arm of the gateway repair. Sync +
    /// fire-and-forget like [`maybe_refresh_stale`](Self::maybe_refresh_stale):
    /// the cache check and per-owner cooldown run inline; the pkarr fetch runs
    /// in a spawned task behind the SAME [`PKARR_REFRESH_MAX_CONCURRENT`]
    /// semaphore, so a mostly-record-less community cannot fan out unbounded
    /// concurrent fetches ([`resolve_async`](Self::resolve_async) takes no
    /// permit — it exists for callers that need the payloads inline). The
    /// cooldown map is shared with the refresh path deliberately: both are
    /// "one pkarr fetch per owner per window" decisions.
    pub(crate) fn discover_record_less(&self, owner: OwnerAddr) {
        {
            let map = self.inner.read().expect("resolver read lock");
            if map.range_owner(&owner).next().is_some() {
                return; // any row at all ⇒ not record-less; the refresh path owns it
            }
        }
        let fb = {
            let guard = self
                .fallback_source
                .read()
                .expect("fallback_source poisoned");
            guard.clone()
        };
        let Some(fb) = fb else {
            return; // no fallback ⇒ nothing to discover with; don't burn the cooldown
        };
        {
            let mut cooldowns = self
                .refresh_cooldowns
                .lock()
                .expect("refresh_cooldowns lock");
            let now = tokio::time::Instant::now();
            if let Some(&last) = cooldowns.get(&owner) {
                if now.saturating_duration_since(last) < PKARR_REFRESH_COOLDOWN {
                    return;
                }
            }
            cooldowns.insert(owner, now);
        }
        let resolver = self.clone();
        let permits = Arc::clone(&self.refresh_permits);
        tokio::spawn(async move {
            let _permit = match permits.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let payloads = fb.resolve(&owner).await;
            for payload in payloads {
                let hlc = Hlc {
                    wall_ms: payload.announced_at_ms,
                    logical: 0,
                    device_id: String::new(),
                };
                resolver.update_with_source(owner, payload, hlc, ReachabilitySource::PkarrLive);
            }
        });
    }

    /// Like [`resolve`](Self::resolve), but carries each entry's source for the
    /// per-source butler-set freshness policy (Task 3, Decision 3).
    pub fn resolve_with_source(
        &self,
        actor: &OwnerAddr,
    ) -> Vec<(ReachabilityAnnouncePayload, ReachabilitySource)> {
        let map = self.inner.read().expect("resolver read lock");
        map.range_owner(actor)
            .filter_map(|(_, v)| v.durable_preferred().map(|e| (e.payload.clone(), e.source)))
            .collect()
    }

    /// Like [`resolve_async`](Self::resolve_async), but carries source. Cache
    /// hits keep their stored source; on a cache miss the pkarr fallback results
    /// are stored `PkarrLive` and the AUTHORITATIVE post-store resolver view is
    /// re-read and returned — a concurrent durable-CRDT `update()` may have
    /// landed in the durable slot during the fallback await, and the durable-
    /// preferred read then surfaces it, so returning the pre-await pkarr vector
    /// would mis-tag a now-durable entry as `PkarrLive` (Qodo
    /// "resolve_async_with_source TOCTOU").
    pub async fn resolve_async_with_source(
        &self,
        addr: &OwnerAddr,
    ) -> Vec<(ReachabilityAnnouncePayload, ReachabilitySource)> {
        // 1. Sync cache check — preserves each cached entry's source.
        let cached = self.resolve_with_source(addr);
        if !cached.is_empty() {
            return cached;
        }
        // 2. Fallback to pkarr if configured.
        let fb = {
            let guard = self
                .fallback_source
                .read()
                .expect("fallback_source poisoned");
            guard.clone()
        };
        let Some(fb) = fb else {
            return Vec::new();
        };
        let payloads = fb.resolve(addr).await;
        // 3. Populate cache (PkarrLive) and return all results tagged PkarrLive.
        for payload in &payloads {
            let hlc = Hlc {
                wall_ms: payload.announced_at_ms,
                logical: 0,
                device_id: String::new(),
            };
            self.update_with_source(*addr, payload.clone(), hlc, ReachabilitySource::PkarrLive);
        }
        // Re-read the authoritative resolver view rather than returning the
        // pre-await pkarr vector: a concurrently-arrived durable entry for this
        // `(owner, node_id)` sits in its own durable slot, and the durable-
        // preferred read surfaces it, so it must be returned tagged `DurableCrdt`,
        // not `PkarrLive` (CA3 TOCTOU).
        self.resolve_with_source(addr)
    }
}

/// The addressing fingerprint the reconnect supervisor's `RecordChanged`
/// trigger compares (ZEB-620): the iroh home-relay URL plus the direct-address
/// *set*. Direct addresses are collected into a `BTreeSet` so they compare
/// order- and duplicate-insensitively — a reordered republish of the same
/// endpoints is not a change, and a byte-identical beacon refresh yields an
/// equal key, so the supervisor does not re-arm the peer's backoff ladder for no
/// reason. ZEB-621 M1: the kick gate snapshots this key (not the whole payload)
/// before/after the slot write, keeping the two snapshots off the butler_set /
/// signature copy path that every CRDT apply and pkarr cache-back would pay.
fn addr_key(payload: &ReachabilityAnnouncePayload) -> (String, BTreeSet<std::net::SocketAddr>) {
    (
        payload.home_relay_url.clone(),
        payload.direct_addresses.iter().copied().collect(),
    )
}

/// Same-source LWW: is `next` strictly newer than `prev`? ZEB-744 delegates the
/// comparison to the core `harmony-reachability` kernel comparator
/// ([`core_lww_newer`]), passing the HLC as its `(wall_ms, logical, device_id)`
/// `Ord` tuple — harmony's `Hlc` deliberately does not derive `Ord`
/// (canonical-CBOR keying constraint — see `owner_state_types.rs::Hlc`), so the
/// tuple is the clock the kernel orders. The payload supplies the
/// `announced_at_ms` then lex `iroh_node_id` tie-breaks via the core
/// `ReachabilityRecord` impl (spec §5.4). Full equality → `false` (byte-identical
/// replay is a no-op).
///
/// ZEB-621 dual-slot split: durable-CRDT and live-pkarr records for the same
/// `(owner, iroh_node_id)` live in SEPARATE cells ([`ResolverSlots`]), and each
/// source writes only its own slot — so this comparator only ever sees
/// `prev`/`next` of the SAME source. The old cross-source source-priority guard
/// (ZEB-488) is gone: its intent is now structural rather than a comparator
/// special-case. The durable slot is the butler authority and stays
/// window-EXEMPT (a pkarr write can never touch it); the freshest view across
/// both slots is the dial authority.
fn lww_newer(prev: &ResolverEntry, next: &ResolverEntry) -> bool {
    let prev_clock = (
        prev.hlc.wall_ms,
        prev.hlc.logical,
        prev.hlc.device_id.as_str(),
    );
    let next_clock = (
        next.hlc.wall_ms,
        next.hlc.logical,
        next.hlc.device_id.as_str(),
    );
    core_lww_newer(&prev_clock, &prev.payload, &next_clock, &next.payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_payload(node_id_byte: u8, announced_at_ms: u64) -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [node_id_byte; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms,
            identity_signature: [0; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        }
    }

    fn make_hlc(wall_ms: u64, logical: u32, device: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical,
            device_id: device.into(),
        }
    }

    /// The composite key's node-id half for a payload minted by
    /// [`make_payload`] with byte `b` — must match `make_payload`'s
    /// `iroh_node_id: [b; 32]` convention so ids agree across helpers.
    fn node_id_bytes(b: u8) -> [u8; 32] {
        [b; 32]
    }

    /// ZEB-622: the liveness seam mirrors the supervisor seam — `liveness()` is
    /// `None` until `set_liveness`, then returns a handle sharing the installed
    /// state, so an up-edge raised through the returned clone is visible via a
    /// fresh getter's `states_snapshot()`.
    #[test]
    fn set_liveness_roundtrip_snapshot() {
        let r = ReachabilityResolver::new();
        assert!(r.liveness().is_none());
        r.set_liveness(crate::peer_liveness::LivenessHandle::new());
        let got = r.liveness().expect("liveness installed");
        assert!(got.states_snapshot().is_empty());
        // Raise an external up-edge through the returned clone…
        got.on_transport_up_external([7u8; 32]);
        // …and observe it via an independently-fetched clone (shared state).
        let snap = r.liveness().expect("still installed").states_snapshot();
        assert!(snap.iter().any(|(p, _)| *p == [7u8; 32]));
    }

    /// ZEB-928: the resolver forwards a verified enrolled-key binding into the admission
    /// oracle and evicts it on `remove_owner`. The denied→admitted flip across eviction
    /// proves the binding was actually dropped, not merely covered by fail-open.
    #[test]
    fn admission_oracle_binding_and_eviction() {
        use crate::admission_oracle::AdmissionOracle;
        let oracle = std::sync::Arc::new(AdmissionOracle::new(true));
        let r = ReachabilityResolver::new();
        r.set_admission_oracle(std::sync::Arc::clone(&oracle));

        let actor = OwnerAddr([0x11; 16]);
        let node_id = node_id_bytes(0x42);
        let enrolled_vk = [0xBB; 32];

        // Ingest-seam order: bind first, then the update that would fire the kick.
        r.note_enrolled_binding(actor.0, node_id, enrolled_vk);
        r.update(actor, make_payload(0x42, 1_000), make_hlc(1_000, 0, "d"));

        // Bound but nothing admitted → denied.
        oracle.publish_admitted(std::collections::BTreeSet::new());
        assert!(
            !oracle.admit(&node_id),
            "bound to a non-admitted key -> denied"
        );

        // Admitting the enrolled key flips it.
        oracle.publish_admitted(std::collections::BTreeSet::from([enrolled_vk]));
        assert!(oracle.admit(&node_id), "admitted key -> node_id dialable");

        // Eviction: with nothing admitted the node_id is denied while bound; after
        // remove_owner it becomes unknown → fail-open → admitted.
        oracle.publish_admitted(std::collections::BTreeSet::new());
        assert!(!oracle.admit(&node_id));
        let removed = r.remove_owner(&actor);
        assert!(removed.contains(&node_id));
        assert!(
            oracle.admit(&node_id),
            "unbound after remove -> fail-open (unknown)"
        );
    }

    /// ZEB-930 Part 2: `seed_from_pkarr` binds the enrolled key BEFORE its
    /// internal update fires the kick when given `Some(vk)`, so the seeded node
    /// is classifiable the instant it is kicked. `None` leaves it fail-open
    /// (the invite-redeem callers' unchanged posture).
    #[tokio::test]
    async fn seed_from_pkarr_some_binds_none_fails_open() {
        use crate::admission_oracle::AdmissionOracle;
        let oracle = std::sync::Arc::new(AdmissionOracle::new(true));
        let r = ReachabilityResolver::new();
        r.set_admission_oracle(std::sync::Arc::clone(&oracle));
        oracle.publish_admitted(std::collections::BTreeSet::new()); // nothing admitted

        let owner = OwnerAddr([0x11; 16]);
        let dh = crate::owner_state_types::DeviceIdentityHash([0u8; 16]);
        let vk = [0xBB; 32];

        // Some(vk): binds -> node_id classifiable -> denied (bound to a non-admitted key).
        let node_some = node_id_bytes(0x42);
        r.seed_from_pkarr(owner, dh, Some(vk), make_payload(0x42, 1_000))
            .await;
        assert!(
            !oracle.admit(&node_some),
            "Some(vk) binds -> non-admitted key denied"
        );

        // None: no bind -> node_id unknown -> fail-open.
        let node_none = node_id_bytes(0x43);
        r.seed_from_pkarr(owner, dh, None, make_payload(0x43, 1_000))
            .await;
        assert!(
            oracle.admit(&node_none),
            "None leaves node_id unbound -> fail-open"
        );
    }

    /// Same owner, SAME device (same iroh_node_id) — later HLC wins per LWW.
    #[test]
    fn lww_higher_hlc_wins_per_device() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        // Both announces from the SAME device (node_id_byte = 1) — second
        // has higher HLC and should overwrite.
        r.update(actor, make_payload(1, 1000), make_hlc(1000, 0, "a"));
        r.update(actor, make_payload(1, 2000), make_hlc(2000, 0, "a"));
        let records = r.resolve(&actor);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].announced_at_ms, 2000);
    }

    /// ZEB-627: the generation counter bumps only when the map materially
    /// changes — accepted (LWW-winning) writes and non-empty removals — so
    /// generation-keyed caches (event_loop's zid→node map) rebuild exactly
    /// when the derived view could have changed.
    #[test]
    fn generation_bumps_only_on_materialized_change() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0xAA; 16]);
        let g0 = r.generation();
        // First learn: materialized → bump.
        r.update(actor, make_payload(1, 1000), make_hlc(1000, 0, "a"));
        let g1 = r.generation();
        assert!(g1 > g0, "accepted write bumps");
        // LWW-rejected (older HLC, same device slot): no bump.
        r.update(actor, make_payload(1, 500), make_hlc(500, 0, "a"));
        assert_eq!(r.generation(), g1, "rejected write does not bump");
        // Eviction: bump.
        assert_eq!(r.remove_owner(&actor).len(), 1);
        assert!(r.generation() > g1, "remove_owner bumps");
        // Removing an absent owner: no bump.
        let g2 = r.generation();
        assert_eq!(r.remove_owner(&OwnerAddr([0xBB; 16])).len(), 0);
        assert_eq!(r.generation(), g2, "empty removal does not bump");
    }

    #[test]
    fn lww_lower_hlc_ignored_per_device() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        r.update(actor, make_payload(1, 2000), make_hlc(2000, 0, "a"));
        r.update(actor, make_payload(1, 1000), make_hlc(1000, 0, "a"));
        let records = r.resolve(&actor);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].announced_at_ms, 2000);
    }

    /// ZEB-621: durable and pkarr now occupy SEPARATE slots. A durable announce
    /// that replicates LATER (with an OLDER `announced_at` than a
    /// frequently-refreshed pkarr blob) installs into its OWN slot: the butler
    /// view (durable-preferred) flips to the durable record, while the dial view
    /// (freshest) stays with the fresher pkarr record. (Supersedes the old
    /// single-slot `durable_not_evicted_by_fresher_pkarr` /
    /// `pkarr_upgraded_by_older_durable` source-guard pair; the butler-view half
    /// is now pinned by `butler_view_stays_durable_despite_fresher_pkarr`.)
    #[test]
    fn pkarr_upgraded_by_older_durable() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        r.update_with_source(
            actor,
            make_payload(1, 9000),
            make_hlc(9000, 0, "a"),
            ReachabilitySource::PkarrLive,
        );
        r.update_with_source(
            actor,
            make_payload(1, 1000),
            make_hlc(1000, 0, "a"),
            ReachabilitySource::DurableCrdt,
        );
        // Butler view: durable-preferred → the durable slot, tagged DurableCrdt.
        let tagged = r.resolve_with_source(&actor);
        assert_eq!(tagged.len(), 1);
        assert_eq!(
            tagged[0].1,
            ReachabilitySource::DurableCrdt,
            "butler view flips to durable once its own slot is installed"
        );
        assert_eq!(tagged[0].0.announced_at_ms, 1000);
        // Dial view: freshest → the fresher pkarr slot survives.
        let (_, dial) = r
            .resolve_by_node_id(&node_id_bytes(1))
            .expect("dial record");
        assert_eq!(
            dial.announced_at_ms, 9000,
            "dial view keeps the fresher pkarr record"
        );
    }

    #[test]
    fn list_active_peers_effective_reports_the_future_skew_clamp() {
        const T: u64 = 1_000_000_000_000; // fixed "now" (ms)
        let r = ReachabilityResolver::new();
        r.set_clock(std::sync::Arc::new(|| T));

        // A future-dated durable record: announced 1 h ahead of now.
        let owner = OwnerAddr([7; 16]);
        r.update(owner, make_payload(1, T + 3_600_000), make_hlc(T, 0, "a"));

        let got = r.list_active_peers_effective();
        assert_eq!(got.len(), 1);
        // Raw announce is still the future value on the payload...
        assert_eq!(got[0].1.announced_at_ms, T + 3_600_000);
        // ...but the effective (clamped) value is capped at now + 5 min.
        assert_eq!(got[0].2, T + crate::clock_trust::MAX_FORWARD_SKEW_MS);
    }

    /// ZEB-852 (RB): a future-dated `DurableCrdt` HLC must not permanently pin the
    /// durable routing slot. Post-ZEB-815 the durable/sibling HLC arrives
    /// peer-signed via the address book (`ingest_verified_row` feeds `DurableCrdt`
    /// a peer-signed `row.at`), so a hostile peer can stamp `wall_ms = now + 1yr`
    /// and — absent the insert-time clamp — win same-source LWW for process life,
    /// locking out every honest later record. The clamp to the control-tier skew
    /// ceiling (`now + MAX_FORWARD_SKEW_MS`) bounds that stamp so an honest
    /// in-range record minted after the clock advances still overtakes it.
    ///
    /// Discrimination (both halves): the future HLC must NOT pin the slot over an
    /// honest in-range record, AND an honest newer in-range record must win it.
    #[test]
    fn durable_future_hlc_is_clamped_so_later_honest_record_wins() {
        const T0: u64 = 1_000_000_000_000; // fixed "now" at ingest of the hostile record
        const ONE_YEAR_MS: u64 = 365 * 24 * 60 * 60 * 1000;
        let r = ReachabilityResolver::new();
        // Advance-able clock: the hostile record is clamped against T0, the honest
        // record against the later T1 (mirrors real ingest — each peer re-clamps at
        // its own "now").
        let clock = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(T0));
        {
            let c = std::sync::Arc::clone(&clock);
            r.set_clock(std::sync::Arc::new(move || {
                c.load(std::sync::atomic::Ordering::SeqCst)
            }));
        }

        let owner = OwnerAddr([7; 16]);
        // Hostile peer-signed durable record stamped a year into the future. Without
        // the clamp this raw HLC (T0 + 1yr) wins LWW and pins the slot forever.
        r.update_with_source(
            owner,
            make_payload(1, 111_000),
            make_hlc(T0 + ONE_YEAR_MS, 0, "attacker"),
            ReachabilitySource::DurableCrdt,
        );

        // Clock advances an hour; the honest device re-announces in-range.
        let t1 = T0 + 3_600_000;
        clock.store(t1, std::sync::atomic::Ordering::SeqCst);
        r.update_with_source(
            owner,
            make_payload(1, 222_000),
            make_hlc(t1, 0, "honest"),
            ReachabilitySource::DurableCrdt,
        );

        // Durable slot (butler / durable-preferred view) must now be the honest
        // record: the hostile HLC was clamped to T0 + skew, which the honest T1
        // overtakes. Without the clamp the durable slot stays pinned to the future
        // record (announced 111_000) and this assertion fails.
        let tagged = r.resolve_with_source(&owner);
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].1, ReachabilitySource::DurableCrdt);
        assert_eq!(
            tagged[0].0.announced_at_ms, 222_000,
            "honest in-range durable record must win the slot; the future HLC must not pin it"
        );
    }

    /// The dual-slot separation governs only cross-source collisions; same-source
    /// updates keep ordinary HLC LWW.
    #[test]
    fn same_source_keeps_hlc_lww() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        r.update_with_source(
            actor,
            make_payload(1, 1000),
            make_hlc(1000, 0, "a"),
            ReachabilitySource::PkarrLive,
        );
        r.update_with_source(
            actor,
            make_payload(1, 2000),
            make_hlc(2000, 0, "a"),
            ReachabilitySource::PkarrLive,
        );
        let tagged = r.resolve_with_source(&actor);
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].0.announced_at_ms, 2000);
        assert_eq!(tagged[0].1, ReachabilitySource::PkarrLive);
    }

    /// Same owner, DIFFERENT devices — both records coexist. This is the
    /// multi-device case from PR #157 round 5 (Cursor): per ZEB-173 a
    /// harmony identity binds to multiple devices, each with its own
    /// iroh `EndpointId`. The resolver must keep all of them so any
    /// of the owner's devices is dialable.
    #[test]
    fn multi_device_same_owner_coexist() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let device_a_node: u8 = 0x01;
        let device_b_node: u8 = 0x02;
        r.update(
            actor,
            make_payload(device_a_node, 1000),
            make_hlc(1000, 0, "a"),
        );
        r.update(
            actor,
            make_payload(device_b_node, 1500),
            make_hlc(1500, 0, "b"),
        );

        let records = r.resolve(&actor);
        assert_eq!(records.len(), 2, "both devices' records must survive");

        let node_ids: Vec<[u8; 32]> = records.iter().map(|p| p.iroh_node_id).collect();
        assert!(node_ids.contains(&[device_a_node; 32]));
        assert!(node_ids.contains(&[device_b_node; 32]));

        // Each device's payload preserved with its own HLC slot.
        for r_p in &records {
            if r_p.iroh_node_id == [device_a_node; 32] {
                assert_eq!(r_p.announced_at_ms, 1000);
            } else {
                assert_eq!(r_p.announced_at_ms, 1500);
            }
        }

        // list_active_peers reports one row per (owner, device).
        let active = r.list_active_peers();
        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|(o, _)| *o == actor));
    }

    #[test]
    fn determinism_across_orders() {
        // Same intent as the original: applying the same set of
        // ReachabilityAnnounce events in arbitrary order converges to
        // the same map. Under composite keying, each (owner, device)
        // pair is its own slot, so the convergence guarantee is even
        // stronger — distinct events never overwrite each other.
        let events: Vec<(OwnerAddr, ReachabilityAnnouncePayload, Hlc)> = vec![
            (
                OwnerAddr([0x11; 16]),
                make_payload(1, 1000),
                make_hlc(1000, 0, "a"),
            ),
            (
                OwnerAddr([0x22; 16]),
                make_payload(3, 1500),
                make_hlc(1500, 0, "b"),
            ),
            (
                OwnerAddr([0x11; 16]),
                make_payload(1, 2000),
                make_hlc(2000, 0, "a"),
            ),
            (
                OwnerAddr([0x22; 16]),
                make_payload(3, 2500),
                make_hlc(2500, 0, "b"),
            ),
        ];

        let mut orders = vec![
            vec![0, 1, 2, 3],
            vec![3, 2, 1, 0],
            vec![1, 3, 0, 2],
            vec![2, 0, 3, 1],
        ];

        let mut final_states: Vec<Vec<(OwnerAddr, [u8; 32], u64)>> = Vec::new();
        for order in orders.drain(..) {
            let r = ReachabilityResolver::new();
            for i in order {
                let (a, p, h) = &events[i];
                r.update(*a, p.clone(), h.clone());
            }
            let mut s: Vec<(OwnerAddr, [u8; 32], u64)> = r
                .list_active_peers()
                .into_iter()
                .map(|(a, p)| (a, p.iroh_node_id, p.announced_at_ms))
                .collect();
            s.sort();
            final_states.push(s);
        }

        for w in final_states.windows(2) {
            assert_eq!(w[0], w[1], "ReachabilityResolver is not order-independent");
        }
    }

    #[test]
    fn lww_announced_at_ms_breaks_hlc_tie() {
        // Spec §5.4 tie-break #1: equal HLC, higher announced_at_ms wins.
        // Same-device case (same iroh_node_id) so both updates target the
        // same composite-key slot.
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let hlc = make_hlc(1000, 0, "a");
        r.update(actor, make_payload(1, 1500), hlc.clone());
        r.update(actor, make_payload(1, 2500), hlc.clone());
        let records = r.resolve(&actor);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].announced_at_ms, 2500);

        // Reverse order: lower announced_at_ms must NOT overwrite higher.
        let r2 = ReachabilityResolver::new();
        r2.update(actor, make_payload(1, 2500), hlc.clone());
        r2.update(actor, make_payload(1, 1500), hlc);
        let records2 = r2.resolve(&actor);
        assert_eq!(records2.len(), 1);
        assert_eq!(records2[0].announced_at_ms, 2500);
    }

    #[test]
    fn lww_iroh_node_id_tie_broken_in_payload_collision() {
        // Spec §5.4 tie-break #2 used to mean "same HLC + same
        // announced_at_ms, lex-greater iroh_node_id wins". Under composite
        // keying, two payloads with DIFFERENT iroh_node_ids land in
        // different slots and both survive. This tie-break only fires
        // when the SAME composite key sees two competing updates whose
        // HLC and announced_at_ms both collide AND their iroh_node_id
        // matches — which only happens for same-device replays. We
        // assert that the resolver remains deterministic in that case
        // (no panic, last-write semantics by HLC tie).
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let hlc = make_hlc(1000, 0, "a");
        // Same device, same announced_at_ms, same HLC — second `update`
        // is a no-op (lww_newer returns false on full equality).
        r.update(actor, make_payload(0x01, 2000), hlc.clone());
        r.update(actor, make_payload(0x01, 2000), hlc);
        let records = r.resolve(&actor);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].iroh_node_id, [0x01; 32]);
    }

    #[test]
    fn resolve_by_node_id_finds_inserted_entry() {
        // CodeRabbit PR #157 round 1: positive lookup. The Phase 1
        // transport reaches peers exclusively via node_id (the locator
        // form `iroh/<hex-32>`), so resolve_by_node_id is on the hot
        // path of every outbound `new_link` call.
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x55; 16]);
        let payload = make_payload(0xAB, 1500);
        r.update(actor, payload.clone(), make_hlc(1500, 0, "a"));
        let hit = r.resolve_by_node_id(&[0xAB; 32]);
        assert!(hit.is_some());
        let (got_addr, got_payload) = hit.unwrap();
        assert_eq!(got_addr, actor);
        assert_eq!(got_payload.iroh_node_id, [0xAB; 32]);
        assert_eq!(got_payload.announced_at_ms, payload.announced_at_ms);
    }

    #[test]
    fn resolve_by_node_id_returns_none_for_unknown() {
        // CodeRabbit PR #157 round 1: negative lookup.
        let r = ReachabilityResolver::new();
        r.update(
            OwnerAddr([0x11; 16]),
            make_payload(0xAB, 1500),
            make_hlc(1500, 0, "a"),
        );
        assert!(r.resolve_by_node_id(&[0xCD; 32]).is_none());
    }

    /// PR #157 round 5 (Cursor): Leave/Kick events should drop the
    /// departed member's records from the resolver. `remove_owner`
    /// removes ALL device entries for the owner in a single call.
    #[test]
    fn remove_owner_evicts_all_devices() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let other = OwnerAddr([0x22; 16]);
        // Three devices for `actor` + one for `other` (the latter must
        // NOT be evicted).
        r.update(actor, make_payload(0x01, 1000), make_hlc(1000, 0, "a"));
        r.update(actor, make_payload(0x02, 1000), make_hlc(1000, 0, "b"));
        r.update(actor, make_payload(0x03, 1000), make_hlc(1000, 0, "c"));
        r.update(other, make_payload(0xCC, 1000), make_hlc(1000, 0, "d"));

        let removed = r.remove_owner(&actor);
        assert_eq!(removed.len(), 3, "all three of actor's devices evicted");

        assert!(r.resolve(&actor).is_empty());
        assert_eq!(r.resolve(&other).len(), 1, "other owner untouched");
        // Reverse lookups for the evicted device-ids must now miss.
        assert!(r.resolve_by_node_id(&[0x01; 32]).is_none());
        assert!(r.resolve_by_node_id(&[0xCC; 32]).is_some());
    }

    #[test]
    fn remove_owner_is_idempotent() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        // No entries → returns 0, doesn't panic.
        assert_eq!(r.remove_owner(&actor).len(), 0);

        r.update(actor, make_payload(0x01, 1000), make_hlc(1000, 0, "a"));
        assert_eq!(r.remove_owner(&actor).len(), 1);
        // Second remove is a no-op — also 0.
        assert_eq!(r.remove_owner(&actor).len(), 0);
    }

    /// ZEB-643: `remove_owner` returns the node-ids of the records it
    /// deleted — the authoritative set, captured under the same write-lock
    /// hold as the removal (callers evict supervisor slots from it).
    #[test]
    fn remove_owner_returns_removed_node_ids() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let other = OwnerAddr([0x22; 16]);
        r.update(actor, make_payload(0x01, 1000), make_hlc(1000, 0, "a"));
        r.update(actor, make_payload(0x02, 1000), make_hlc(1000, 0, "b"));
        r.update(other, make_payload(0xCC, 1000), make_hlc(1000, 0, "c"));

        let mut removed = r.remove_owner(&actor);
        removed.sort();
        assert_eq!(
            removed,
            vec![[0x01; 32], [0x02; 32]],
            "returns exactly the deleted node-ids"
        );
        assert_eq!(r.resolve(&other).len(), 1, "other owner untouched");
        assert!(
            r.remove_owner(&actor).is_empty(),
            "second remove returns the empty set"
        );
    }

    // ---- ZEB-620 Task 4: reconnect-supervisor triggers -------------------

    /// First-learn of a `(owner, node_id)` kicks `NewPeer` on the installed
    /// supervisor handle (the supervisor-side successor to the legacy dial-hint).
    #[test]
    fn first_learn_kicks_new_peer() {
        let r = ReachabilityResolver::new();
        let sup = SupervisorHandle::new();
        r.set_supervisor(sup.clone());
        let owner = OwnerAddr([0xAA; 16]);
        let node_id = [0x11; 32];
        r.update(owner, make_payload(0x11, 1000), make_hlc(1, 0, "a"));
        assert_eq!(
            sup.pending_trigger(node_id),
            Some(ReconnectTrigger::NewPeer),
            "first learn must kick NewPeer"
        );
    }

    /// A newer-HLC LWW replace whose iroh relay URL differs kicks `RecordChanged`.
    /// The supervisor is installed AFTER the first-learn so the assertion observes
    /// only the changed-record kick (NewPeer and RecordChanged coalesce to equal
    /// strength, so an un-drained NewPeer would otherwise mask it).
    #[test]
    fn changed_relay_kicks_record_changed() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0xAA; 16]);
        let node_id = [0x11; 32];
        // First learn with no supervisor installed — no kick recorded.
        r.update(owner, make_payload(0x11, 1000), make_hlc(1000, 0, "a"));
        let sup = SupervisorHandle::new();
        r.set_supervisor(sup.clone());
        // Same key, newer HLC, DIFFERENT relay URL.
        let mut changed = make_payload(0x11, 1000);
        changed.home_relay_url = "https://other-relay.example/".into();
        r.update(owner, changed, make_hlc(2000, 0, "a"));
        assert_eq!(
            sup.pending_trigger(node_id),
            Some(ReconnectTrigger::RecordChanged),
            "changed relay URL must kick RecordChanged"
        );
    }

    /// A newer-HLC LWW replace whose direct-address set differs kicks
    /// `RecordChanged`.
    #[test]
    fn changed_direct_addrs_kicks_record_changed() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0xAA; 16]);
        let node_id = [0x11; 32];
        // First learn (empty direct-address set) with no supervisor installed.
        r.update(owner, make_payload(0x11, 1000), make_hlc(1000, 0, "a"));
        let sup = SupervisorHandle::new();
        r.set_supervisor(sup.clone());
        // Same key, newer HLC, DIFFERENT (now non-empty) direct-address set.
        let mut changed = make_payload(0x11, 1000);
        changed.direct_addresses = vec!["203.0.113.7:9000".parse().expect("v4 addr")];
        r.update(owner, changed, make_hlc(2000, 0, "a"));
        assert_eq!(
            sup.pending_trigger(node_id),
            Some(ReconnectTrigger::RecordChanged),
            "changed direct-address set must kick RecordChanged"
        );
    }

    /// A byte-identical addressing replay (newer HLC, same relay + same
    /// direct-address set — e.g. a beacon republish) must NOT kick: re-arming
    /// the ladder on an unchanged record is thrash the supervisor must avoid.
    #[test]
    fn identical_payload_replay_does_not_kick() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0xAA; 16]);
        let node_id = [0x11; 32];
        r.update(owner, make_payload(0x11, 1000), make_hlc(1000, 0, "a"));
        let sup = SupervisorHandle::new();
        r.set_supervisor(sup.clone());
        // Same key, newer HLC (so lww_newer succeeds), IDENTICAL addressing.
        r.update(owner, make_payload(0x11, 1000), make_hlc(2000, 0, "a"));
        assert_eq!(
            sup.pending_trigger(node_id),
            None,
            "byte-identical addressing replay must not kick"
        );
    }

    /// ZEB-510/ZEB-690: a FLEET-source heartbeat re-feed (a `FleetSibling`
    /// republish with a newer HLC but identical addressing) must NOT re-fire
    /// `NewPeer` — `was_present` (the fleet slot already exists) suppresses the
    /// first-learn kick, and the unchanged `addr_key` keeps `RecordChanged`
    /// silent too. Mirrors `identical_payload_replay_does_not_kick` on the fleet
    /// slot (the ZEB-510 step-1 addition that the durable-path tests don't cover).
    #[test]
    fn fleet_heartbeat_refeed_does_not_refire_new_peer() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0xAA; 16]);
        let node_id = [0x11; 32];
        // First fleet-sibling learn with NO supervisor installed — no kick recorded.
        r.update_with_source(
            owner,
            make_payload(0x11, 1000),
            make_hlc(1000, 0, "a"),
            ReachabilitySource::FleetSibling,
        );
        let sup = SupervisorHandle::new();
        r.set_supervisor(sup.clone());
        // Fleet heartbeat: same key + source, newer HLC, IDENTICAL addressing.
        r.update_with_source(
            owner,
            make_payload(0x11, 1000),
            make_hlc(2000, 0, "a"),
            ReachabilitySource::FleetSibling,
        );
        assert_eq!(
            sup.pending_trigger(node_id),
            None,
            "fleet heartbeat re-feed must not re-fire NewPeer",
        );
    }

    /// An LWW-rejected stale record (`lww_newer == false`) must NOT kick,
    /// even though its addressing differs — the slot is unchanged, so there is
    /// nothing to re-arm on.
    #[test]
    fn lww_rejected_stale_record_does_not_kick() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0xAA; 16]);
        let node_id = [0x11; 32];
        // Establish the winning record at HLC 2000, no supervisor installed.
        r.update(owner, make_payload(0x11, 1000), make_hlc(2000, 0, "a"));
        let sup = SupervisorHandle::new();
        r.set_supervisor(sup.clone());
        // Older HLC with a DIFFERENT relay — rejected by LWW, so no replace.
        let mut stale = make_payload(0x11, 1000);
        stale.home_relay_url = "https://stale-relay.example/".into();
        r.update(owner, stale, make_hlc(1000, 0, "a"));
        assert_eq!(
            sup.pending_trigger(node_id),
            None,
            "LWW-rejected stale record must not kick"
        );
    }

    // ---- ZEB-621: dual-slot storage + freshest-wins dial view ------------

    /// ZEB-621: dial view (resolve_by_node_id) prefers the FRESHER record across
    /// sources — a fresher pkarr record beats a stale durable one at dial time.
    #[test]
    fn dial_view_prefers_fresher_pkarr_over_stale_durable() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([1u8; 16]);
        let durable = make_payload(7, 1_000); // announced_at_ms = 1_000 (stale)
        r.update(owner, durable, make_hlc(1_000, 0, "dev-a"));
        let mut pkarr = make_payload(7, 50_000); // same node, fresher
        pkarr.home_relay_url = "https://fresh.relay/".to_string();
        let hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: String::new(),
        };
        r.update_with_source(owner, pkarr, hlc, ReachabilitySource::PkarrLive);

        let (o, p) = r.resolve_by_node_id(&node_id_bytes(7)).expect("record");
        assert_eq!(o, owner);
        assert_eq!(p.home_relay_url, "https://fresh.relay/");
        assert_eq!(p.announced_at_ms, 50_000);
    }

    /// ZEB-621 + ZEB-488 pin: the durable slot is NEVER evicted by pkarr — the
    /// butler/diag view (resolve_with_source) still returns the durable entry,
    /// tagged DurableCrdt, even when a fresher pkarr record wins the dial view.
    #[test]
    fn butler_view_stays_durable_despite_fresher_pkarr() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([1u8; 16]);
        r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
        let hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: String::new(),
        };
        r.update_with_source(
            owner,
            make_payload(7, 50_000),
            hlc,
            ReachabilitySource::PkarrLive,
        );

        let v = r.resolve_with_source(&owner);
        assert_eq!(v.len(), 1, "one view per (owner,node), durable-preferred");
        assert_eq!(v[0].1, ReachabilitySource::DurableCrdt);
        assert_eq!(v[0].0.announced_at_ms, 1_000);
    }

    /// ZEB-621: an OLDER pkarr record loses the dial view to a fresher durable.
    #[test]
    fn dial_view_keeps_fresher_durable_over_stale_pkarr() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([1u8; 16]);
        let hlc = Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: String::new(),
        };
        r.update_with_source(
            owner,
            make_payload(7, 1_000),
            hlc,
            ReachabilitySource::PkarrLive,
        );
        r.update(owner, make_payload(7, 50_000), make_hlc(50_000, 0, "dev-a"));

        let (_, p) = r.resolve_by_node_id(&node_id_bytes(7)).expect("record");
        assert_eq!(p.announced_at_ms, 50_000);
    }

    /// ZEB-621: a fresher pkarr record that changes effective addressing kicks
    /// RecordChanged (the stale-route-unpin payoff). The supervisor is installed
    /// AFTER the first learn (as in the existing kick tests) so the NewPeer kick
    /// is never recorded and cannot mask the RecordChanged assertion —
    /// `pending_trigger` is a read-only accessor and does not drain.
    #[test]
    fn fresher_pkarr_with_new_addressing_kicks_record_changed() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([1u8; 16]);
        r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
        let sup = SupervisorHandle::new();
        r.set_supervisor(sup.clone());
        let mut pkarr = make_payload(7, 50_000);
        pkarr.home_relay_url = "https://fresh.relay/".to_string();
        let hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: String::new(),
        };
        r.update_with_source(owner, pkarr, hlc, ReachabilitySource::PkarrLive);
        assert_eq!(
            sup.pending_trigger(node_id_bytes(7)),
            Some(ReconnectTrigger::RecordChanged)
        );
    }

    /// ZEB-621: a fresher pkarr record with IDENTICAL addressing does NOT kick
    /// (no ladder thrash on a byte-identical refresh). Supervisor installed after
    /// the first learn so no NewPeer kick is pending to confound the assertion.
    #[test]
    fn fresher_pkarr_same_addressing_does_not_kick() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([1u8; 16]);
        r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
        let sup = SupervisorHandle::new();
        r.set_supervisor(sup.clone());
        let hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: String::new(),
        };
        r.update_with_source(
            owner,
            make_payload(7, 50_000),
            hlc,
            ReachabilitySource::PkarrLive,
        );
        assert_eq!(sup.pending_trigger(node_id_bytes(7)), None);
    }

    /// ZEB-621: remove_owner clears BOTH slots.
    #[test]
    fn remove_owner_clears_both_slots() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([1u8; 16]);
        r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
        let hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: String::new(),
        };
        r.update_with_source(
            owner,
            make_payload(7, 50_000),
            hlc,
            ReachabilitySource::PkarrLive,
        );
        // ZEB-621: seed a stale-refresh cooldown so we can assert it is evicted.
        r.refresh_cooldowns
            .lock()
            .unwrap()
            .insert(owner, tokio::time::Instant::now());
        assert_eq!(r.remove_owner(&owner).len(), 1); // one composite entry (both slots)
        assert!(r.resolve_by_node_id(&node_id_bytes(7)).is_none());
        assert!(r.resolve(&owner).is_empty());
        assert!(
            r.refresh_cooldowns.lock().unwrap().is_empty(),
            "remove_owner evicts the owner's refresh cooldown"
        );
    }

    // ---- ZEB-704: reverse lookup is globally-freshest ACROSS owners ------

    /// ZEB-704: when the SAME node-id exists under two owners, the reverse
    /// lookups must return the owner whose record is globally freshest — not
    /// the first `(owner, node_id)` key in owner-address order. Here the
    /// LATER owner holds the fresher record, so a first-key scan gets it wrong.
    #[test]
    fn reverse_lookup_prefers_fresher_record_across_owners_zeb704() {
        let r = ReachabilityResolver::new();
        let owner_a = OwnerAddr([1u8; 16]); // first in BTreeMap order, stale
        let owner_b = OwnerAddr([2u8; 16]); // later in order, FRESHER
        r.update(owner_a, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
        r.update(
            owner_b,
            make_payload(7, 50_000),
            make_hlc(50_000, 0, "dev-b"),
        );

        let (o, p) = r.resolve_by_node_id(&node_id_bytes(7)).expect("record");
        assert_eq!(o, owner_b, "dial route must follow the freshest record");
        assert_eq!(p.announced_at_ms, 50_000);

        let (o2, e2) = r
            .resolve_entry_by_node_id(&node_id_bytes(7))
            .expect("entry");
        assert_eq!(o2, owner_b);
        assert_eq!(e2.effective_announced_at_ms, 50_000);
    }

    /// ZEB-704 mirror: the EARLIER owner holds the fresher record — guards an
    /// accidental "last matching key wins" implementation.
    #[test]
    fn reverse_lookup_keeps_fresher_first_owner_zeb704() {
        let r = ReachabilityResolver::new();
        let owner_a = OwnerAddr([1u8; 16]); // first in order AND fresher
        let owner_b = OwnerAddr([2u8; 16]);
        r.update(
            owner_a,
            make_payload(7, 50_000),
            make_hlc(50_000, 0, "dev-a"),
        );
        r.update(owner_b, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-b"));

        let (o, p) = r.resolve_by_node_id(&node_id_bytes(7)).expect("record");
        assert_eq!(o, owner_a);
        assert_eq!(p.announced_at_ms, 50_000);
    }

    /// ZEB-704 determinism pin: a FULL tie (equal effective time, equal source
    /// rank) keeps the first owner in BTreeMap order — today's behavior for the
    /// degenerate case, now pinned so the selection stays deterministic.
    #[test]
    fn reverse_lookup_full_tie_keeps_first_owner_zeb704() {
        let r = ReachabilityResolver::new();
        let owner_a = OwnerAddr([1u8; 16]);
        let owner_b = OwnerAddr([2u8; 16]);
        r.update(owner_a, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
        r.update(owner_b, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-b"));

        let (o, _) = r.resolve_by_node_id(&node_id_bytes(7)).expect("record");
        assert_eq!(o, owner_a, "full tie is deterministic: first owner wins");
    }

    /// ZEB-704 (CodeRabbit R1): at EQUAL effective announce time, the
    /// cross-owner pick falls to the source rank — a later owner's durable
    /// (verified) record beats an earlier owner's unsigned fleet one, mirroring
    /// the within-entry slot semantics.
    #[test]
    fn reverse_lookup_equal_time_prefers_higher_source_rank_zeb704() {
        let r = ReachabilityResolver::new();
        let owner_a = OwnerAddr([1u8; 16]); // first in order, FLEET (rank 0)
        let owner_b = OwnerAddr([2u8; 16]); // later, DURABLE (rank 2)
        r.update_with_source(
            owner_a,
            make_payload(7, 50_000),
            make_hlc(50_000, 0, "dev-a"),
            ReachabilitySource::FleetSibling,
        );
        r.update(
            owner_b,
            make_payload(7, 50_000),
            make_hlc(50_000, 0, "dev-b"),
        );

        let (owner, entry) = r
            .resolve_entry_by_node_id(&node_id_bytes(7))
            .expect("entry");
        assert_eq!(owner, owner_b, "equal time: higher source rank wins");
        assert_eq!(entry.source, ReachabilitySource::DurableCrdt);
    }

    /// ZEB-704: the boot-seed recency ranking
    /// (`boot_seed_node_ids_by_recency`, keep-freshest-across-owners) and the
    /// dial-route resolution must AGREE — the seed ranks node 7 by owner B's
    /// fresh record, so resolving node 7 must yield that same record, not
    /// owner A's stale one.
    #[test]
    fn boot_seed_ranking_and_dial_route_agree_zeb704() {
        let r = ReachabilityResolver::new();
        let owner_a = OwnerAddr([1u8; 16]);
        let owner_b = OwnerAddr([2u8; 16]);
        // node 7: stale under A (1_000), fresh under B (50_000).
        r.update(owner_a, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
        r.update(
            owner_b,
            make_payload(7, 50_000),
            make_hlc(50_000, 0, "dev-b"),
        );
        // node 9: single-owner, freshness between the two (10_000).
        r.update(
            owner_a,
            make_payload(9, 10_000),
            make_hlc(10_000, 0, "dev-a"),
        );

        let seed = crate::iroh_zenoh_registration::boot_seed_node_ids_by_recency(&r, &[0xFF; 32]);
        assert_eq!(
            seed,
            vec![node_id_bytes(7), node_id_bytes(9)],
            "seed ranks node 7 first via owner B's 50_000 record"
        );
        let (_, e) = r
            .resolve_entry_by_node_id(&node_id_bytes(7))
            .expect("entry");
        assert_eq!(
            e.effective_announced_at_ms, 50_000,
            "dial route resolves the same record the seed ranked by"
        );
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;

    fn make_payload(node_id_byte: u8, announced_at_ms: u64) -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [node_id_byte; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms,
            identity_signature: [0; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        }
    }

    struct StubFallback {
        responses: std::sync::Mutex<Vec<ReachabilityAnnouncePayload>>,
    }

    #[async_trait]
    impl ReachabilityFallback<OwnerAddr> for StubFallback {
        async fn resolve(&self, _addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
            self.responses.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn resolve_async_returns_empty_when_no_fallback_and_no_cached() {
        let r = ReachabilityResolver::new();
        let addr = OwnerAddr([0u8; 16]);
        let out = r.resolve_async(&addr).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn resolve_async_falls_back_to_pkarr_on_cache_miss() {
        let r = ReachabilityResolver::new();
        let addr = OwnerAddr([0u8; 16]);
        let stub_payload = make_payload(0x42, 9000);
        let stub = Arc::new(StubFallback {
            responses: std::sync::Mutex::new(vec![stub_payload.clone()]),
        });
        r.set_fallback_source(stub);

        let out = r.resolve_async(&addr).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].iroh_node_id, stub_payload.iroh_node_id);

        // Subsequent sync resolve hits warm cache.
        let warm = r.resolve(&addr);
        assert_eq!(warm.len(), 1);
        assert_eq!(warm[0].iroh_node_id, stub_payload.iroh_node_id);
    }

    /// ZEB-325 Phase 2c (Task 2): `seed_from_pkarr` writes a pkarr-resolved
    /// `ReachabilityAnnouncePayload` directly into the in-memory map so that
    /// Phase 1's synchronous `resolve_by_node_id` path (used by
    /// `IrohZenohLinkManager.new_link`) can find the inviter's iroh routing
    /// immediately after `connectivity_redeem_invite_iroh` resolves the
    /// pkarr record — without waiting for the async fallback hook to fire.
    #[tokio::test]
    async fn seed_from_pkarr_makes_record_resolvable_by_node_id() {
        use crate::owner_state_types::DeviceIdentityHash;

        let resolver = ReachabilityResolver::new();
        let owner_addr = OwnerAddr([0x77; 16]);
        let device_hash = DeviceIdentityHash([0x77; 16]);
        let payload = make_payload(0xBE, 4200);

        // Before seeding: resolver has no record for this addr.
        assert!(resolver.resolve(&owner_addr).is_empty());
        assert!(resolver.resolve_by_node_id(&payload.iroh_node_id).is_none());

        // Seed from pkarr (Phase 2c entry point).
        resolver
            .seed_from_pkarr(owner_addr, device_hash, None, payload.clone())
            .await;

        // After seeding: `resolve()` returns the record and
        // `resolve_by_node_id()` (the hot path used by
        // IrohZenohLinkManager.new_link) finds it.
        let resolved = resolver.resolve(&owner_addr);
        assert_eq!(resolved.len(), 1, "seeded record must be retrievable");
        assert_eq!(resolved[0].iroh_node_id, payload.iroh_node_id);
        assert_eq!(resolved[0].announced_at_ms, payload.announced_at_ms);

        let by_node = resolver.resolve_by_node_id(&payload.iroh_node_id);
        assert!(
            by_node.is_some(),
            "resolve_by_node_id must find seeded entry"
        );
        let (got_owner, got_payload) = by_node.unwrap();
        assert_eq!(got_owner, owner_addr);
        assert_eq!(got_payload.iroh_node_id, payload.iroh_node_id);
    }

    /// CA2 ("Api mismatch"): `seed_from_pkarr` must tag the record `PkarrLive`,
    /// not `DurableCrdt`. It is a pkarr-resolved record, so its butler-set stays
    /// subject to the live freshness window; mis-tagging it durable would make a
    /// stale pkarr set window-exempt.
    #[tokio::test]
    async fn seed_from_pkarr_tags_pkarr_live() {
        use crate::owner_state_types::DeviceIdentityHash;
        let resolver = ReachabilityResolver::new();
        let owner_addr = OwnerAddr([0x77; 16]);
        let device_hash = DeviceIdentityHash([0x77; 16]);
        let payload = make_payload(0xBE, 4200);
        resolver
            .seed_from_pkarr(owner_addr, device_hash, None, payload)
            .await;
        let tagged = resolver.resolve_with_source(&owner_addr);
        assert_eq!(tagged.len(), 1);
        assert_eq!(
            tagged[0].1,
            ReachabilitySource::PkarrLive,
            "pkarr-seeded record must be tagged PkarrLive, not DurableCrdt"
        );
    }

    /// Fallback stub that injects a concurrent DURABLE update for the same
    /// `(owner, node_id)` the pkarr blob will report — deterministically
    /// reproducing the CA3 TOCTOU: a durable announce landing DURING the
    /// `resolve_async_with_source` fallback await.
    struct DurableInjectingFallback {
        resolver: ReachabilityResolver,
        owner: OwnerAddr,
        durable: ReachabilityAnnouncePayload,
        pkarr: ReachabilityAnnouncePayload,
    }

    #[async_trait]
    impl ReachabilityFallback<OwnerAddr> for DurableInjectingFallback {
        async fn resolve(&self, _addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
            self.resolver.update_with_source(
                self.owner,
                self.durable.clone(),
                Hlc {
                    wall_ms: self.durable.announced_at_ms,
                    logical: 0,
                    device_id: String::new(),
                },
                ReachabilitySource::DurableCrdt,
            );
            vec![self.pkarr.clone()]
        }
    }

    /// CA3: `resolve_async_with_source` re-reads the authoritative resolver view
    /// after pkarr population, so a durable entry that arrived concurrently
    /// (during the fallback await) for the SAME `(owner, node_id)` is returned
    /// tagged `DurableCrdt` — even though the pkarr blob is fresher — rather than
    /// the pre-await pkarr-tagged vector.
    #[tokio::test]
    async fn resolve_async_with_source_reread_keeps_concurrent_durable() {
        let r = ReachabilityResolver::new();
        let addr = OwnerAddr([0x33; 16]);
        let durable = make_payload(0xAA, 1000); // older
        let pkarr = make_payload(0xAA, 9000); // fresher, SAME node_id
        let fb = Arc::new(DurableInjectingFallback {
            resolver: r.clone(),
            owner: addr,
            durable,
            pkarr,
        });
        r.set_fallback_source(fb);

        let out = r.resolve_async_with_source(&addr).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].1,
            ReachabilitySource::DurableCrdt,
            "concurrent durable must survive + be returned, not the pre-await pkarr tag"
        );
        assert_eq!(out[0].0.announced_at_ms, 1000);
    }

    /// CodeRabbit PR #158 round 2: Clone must share fallback_source, not
    /// snapshot it. If the caller clones before wiring the fallback and then
    /// wires via the original, the clone must see the fallback too — otherwise
    /// boot wiring on the original silently leaves every clone dark.
    ///
    /// Uses DISTINCT addresses per resolver so neither can warm-cache from
    /// the other's resolve call, isolating the fallback-source sharing.
    #[tokio::test]
    async fn clone_shares_fallback_source() {
        let r = ReachabilityResolver::new();
        // Clone BEFORE wiring the fallback — this is the boot-wiring ordering
        // that triggered the bug.
        let clone = r.clone();

        let stub_payload = make_payload(0x07, 5500);
        let stub = Arc::new(StubFallback {
            responses: std::sync::Mutex::new(vec![stub_payload.clone()]),
        });
        // Wire fallback on the original only.
        r.set_fallback_source(stub);

        // Use a distinct address for the clone so neither warm-caches the
        // other's resolve result. The clone hasn't seen addr_clone before,
        // so it MUST go through fallback_source to produce results.
        let addr_orig = OwnerAddr([0x07u8; 16]);
        let addr_clone = OwnerAddr([0x08u8; 16]);

        // Resolve on original first (populates its cache for addr_orig only).
        let from_orig = r.resolve_async(&addr_orig).await;
        // Resolve on clone using a fresh address — forces the fallback path.
        let from_clone = clone.resolve_async(&addr_clone).await;

        assert_eq!(from_orig.len(), 1, "original must resolve via fallback");
        assert_eq!(from_clone.len(), 1, "clone must share fallback_source Arc");
        assert_eq!(from_orig[0].iroh_node_id, stub_payload.iroh_node_id);
        assert_eq!(from_clone[0].iroh_node_id, stub_payload.iroh_node_id);
    }

    // ---- ZEB-621 Task 2: stale-record async pkarr refresh ----------------

    fn make_hlc(wall_ms: u64, logical: u32, device: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical,
            device_id: device.into(),
        }
    }

    fn node_id_bytes(b: u8) -> [u8; 32] {
        [b; 32]
    }

    /// A `ReachabilityFallback` that counts every `resolve` call and returns a
    /// fixed payload set — the counting analogue of [`StubFallback`], used to
    /// assert `maybe_refresh_stale` fires the fallback exactly the expected
    /// number of times under the staleness + cooldown gates.
    struct CountingFallback {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        payloads: Vec<ReachabilityAnnouncePayload>,
    }

    #[async_trait]
    impl ReachabilityFallback<OwnerAddr> for CountingFallback {
        async fn resolve(&self, _addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.payloads.clone()
        }
    }

    /// Drive the current-thread scheduler until `counter` reaches `want` (or a
    /// bounded number of polls elapses; the assertion after the call reports the
    /// failure). Robust against the spawned refresh gaining new suspension
    /// points (semaphore acquire, fallback awaits), unlike a single
    /// `yield_now()` — same idiom as `future_record_clamped_and_healed_by_refresh`.
    async fn wait_for_count(counter: &std::sync::atomic::AtomicUsize, want: usize) {
        for _ in 0..50 {
            if counter.load(std::sync::atomic::Ordering::SeqCst) >= want {
                break;
            }
            tokio::task::yield_now().await;
        }
    }

    /// Give any (erroneously) spawned refresh task ample chance to run before a
    /// NEGATIVE assertion — a handful of yields, since "poll until it appears"
    /// can't prove absence.
    async fn yield_a_few() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    /// ZEB-621: a stale freshest view (>24h) triggers exactly one async pkarr
    /// refresh; a second call inside the cooldown window is suppressed.
    #[tokio::test(start_paused = true)]
    async fn stale_record_triggers_refresh_once_per_cooldown() {
        let r = ReachabilityResolver::new();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&counter),
            payloads: vec![make_payload(7, 90_000_000_000)],
        }));
        let owner = OwnerAddr([1u8; 16]);
        r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));

        let now = 1_000 + STALE_RECORD_REFRESH_MS + 1;
        r.maybe_refresh_stale(owner, node_id_bytes(7), now);
        wait_for_count(&counter, 1).await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        // refresh result installed into the pkarr slot → dial view updated
        let (_, p) = r.resolve_by_node_id(&node_id_bytes(7)).expect("record");
        assert_eq!(p.announced_at_ms, 90_000_000_000);

        // second call within the cooldown: suppressed even though still "stale"
        r.maybe_refresh_stale(owner, node_id_bytes(7), now + 1);
        yield_a_few().await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        // advance past the cooldown: fires again
        tokio::time::advance(PKARR_REFRESH_COOLDOWN + std::time::Duration::from_secs(1)).await;
        r.maybe_refresh_stale(owner, node_id_bytes(7), now + 2);
        wait_for_count(&counter, 2).await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// A fresh record (<24h) never fires the fallback.
    #[tokio::test(start_paused = true)]
    async fn fresh_record_never_triggers_refresh() {
        let r = ReachabilityResolver::new();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&counter),
            payloads: vec![],
        }));
        let owner = OwnerAddr([1u8; 16]);
        r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
        r.maybe_refresh_stale(owner, node_id_bytes(7), 1_000 + STALE_RECORD_REFRESH_MS);
        yield_a_few().await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// ZEB-910: `refresh_if_older_than` honors a caller-supplied staleness bar —
    /// an entry fresh under the 24h default bar refreshes under a 10-minute bar.
    #[tokio::test(start_paused = true)]
    async fn refresh_if_older_than_honors_custom_threshold() {
        let r = ReachabilityResolver::new();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&counter),
            payloads: vec![],
        }));
        let owner = OwnerAddr([1u8; 16]);
        r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));

        let now = 1_000 + 20 * 60 * 1000; // entry is 20 minutes old
        r.maybe_refresh_stale(owner, node_id_bytes(7), now);
        yield_a_few().await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "under the 24h bar a 20-minute entry is fresh"
        );

        r.refresh_if_older_than(owner, node_id_bytes(7), now, 10 * 60 * 1000);
        wait_for_count(&counter, 1).await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// ZEB-910: the per-owner cooldown survives the parameterized bar — repair
    /// callers lower only the staleness gate, never the rate bound.
    #[tokio::test(start_paused = true)]
    async fn refresh_if_older_than_keeps_owner_cooldown() {
        let r = ReachabilityResolver::new();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&counter),
            payloads: vec![],
        }));
        let owner = OwnerAddr([1u8; 16]);
        r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));

        let now = 1_000 + 20 * 60 * 1000;
        r.refresh_if_older_than(owner, node_id_bytes(7), now, 10 * 60 * 1000);
        wait_for_count(&counter, 1).await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        r.refresh_if_older_than(owner, node_id_bytes(7), now + 1, 10 * 60 * 1000);
        yield_a_few().await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "second call inside PKARR_REFRESH_COOLDOWN must not fire"
        );
    }

    /// ZEB-910: the ZEB-510 fleet-sibling skip survives the parameterized bar —
    /// a pkarr re-resolve can never heal a fleet-net record (wrong key space).
    #[tokio::test(start_paused = true)]
    async fn refresh_if_older_than_still_skips_fleet_sibling() {
        let r = ReachabilityResolver::new();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&counter),
            payloads: vec![],
        }));
        let owner = OwnerAddr([1u8; 16]);
        r.update_with_source(
            owner,
            make_payload(7, 1_000),
            make_hlc(1_000, 0, "dev-a"),
            ReachabilitySource::FleetSibling,
        );

        // Ancient under ANY bar — the source skip must fire before staleness.
        let now = 1_000 + 48 * 60 * 60 * 1000;
        r.refresh_if_older_than(owner, node_id_bytes(7), now, 10 * 60 * 1000);
        yield_a_few().await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// PR #659 review (CodeAnt): the staleness/source gates must read the
    /// (owner, node) row the caller names. A same-node row under ANOTHER
    /// owner — here a fresh fleet-sibling row (ZEB-704 seed-owner-split
    /// shape) — must not veto the named owner's refresh.
    #[tokio::test(start_paused = true)]
    async fn refresh_gates_read_named_owner_row_not_cross_owner_freshest() {
        let r = ReachabilityResolver::new();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&counter),
            payloads: vec![],
        }));
        let community_owner = OwnerAddr([1u8; 16]);
        let fleet_owner = OwnerAddr([2u8; 16]);
        let now = 100 * 60 * 60 * 1000u64;
        // Ancient pkarr-live row for the community owner…
        r.update_with_source(
            community_owner,
            make_payload(7, 1_000),
            make_hlc(1_000, 0, "dev-a"),
            ReachabilitySource::PkarrLive,
        );
        // …and a FRESH fleet-sibling row for the SAME node under another
        // owner. The cross-owner freshest view picks the fleet row, whose
        // source-skip would (wrongly) suppress the refresh.
        r.update_with_source(
            fleet_owner,
            make_payload(7, now - 1_000),
            make_hlc(now - 1_000, 0, "dev-b"),
            ReachabilitySource::FleetSibling,
        );

        r.refresh_if_older_than(community_owner, node_id_bytes(7), now, 10 * 60 * 1000);
        wait_for_count(&counter, 1).await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the named owner's ancient row must drive the refresh — a fresh \
             cross-owner fleet row must not veto it"
        );
    }

    /// ZEB-910 (PR #659 review): `discover_record_less` fires the fallback for
    /// a truly record-less owner, installs the result, and is cooldown-bounded.
    #[tokio::test(start_paused = true)]
    async fn discover_record_less_fires_once_and_respects_cooldown() {
        let r = ReachabilityResolver::new();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&counter),
            payloads: vec![make_payload(9, 90_000_000_000)],
        }));
        let owner = OwnerAddr([3u8; 16]);

        r.discover_record_less(owner);
        wait_for_count(&counter, 1).await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (_, p) = r
            .resolve_by_node_id(&node_id_bytes(9))
            .expect("discovered record must land in the cache");
        assert_eq!(p.announced_at_ms, 90_000_000_000);

        // Rows now exist ⇒ the record-less gate no-ops (refresh path owns it),
        // regardless of cooldown.
        r.discover_record_less(owner);
        yield_a_few().await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// ZEB-910 (PR #659 review): repeated discovery for an owner pkarr cannot
    /// find stays inside the shared per-owner cooldown window.
    #[tokio::test(start_paused = true)]
    async fn discover_record_less_cooldown_bounds_repeat_misses() {
        let r = ReachabilityResolver::new();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&counter),
            payloads: vec![], // pkarr has nothing either — stays record-less
        }));
        let owner = OwnerAddr([4u8; 16]);

        r.discover_record_less(owner);
        wait_for_count(&counter, 1).await;
        r.discover_record_less(owner);
        yield_a_few().await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a second discovery inside PKARR_REFRESH_COOLDOWN must not fire"
        );

        tokio::time::advance(PKARR_REFRESH_COOLDOWN + std::time::Duration::from_secs(1)).await;
        r.discover_record_less(owner);
        wait_for_count(&counter, 2).await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// ZEB-621 (Qodo #1) regression: a far-future pkarr record initially wins the
    /// dial view, but its effective freshness is CLAMPED at insert time, so a
    /// later refresh returning a sane payload can replace it in the pkarr slot and
    /// the dial view moves off the future record. Without the clamp the future
    /// HLC would stay LWW-newest forever and the route would pin to the bogus
    /// address. The timeline is written as named consts so the arithmetic is
    /// auditable.
    #[tokio::test(start_paused = true)]
    async fn future_record_clamped_and_healed_by_refresh() {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        // Timeline (Unix-epoch ms).
        const T: u64 = 1_000_000; // injected "now" at insert time
        const FUTURE_ANNOUNCED: u64 = T + 10_000_000_000; // ~116 days in the future
        const CLAMP_CEIL: u64 = T + FUTURE_SKEW_TOLERANCE_MS; // effective at insert
                                                              // "now" at refresh: past the 24h window measured from the CLAMPED effective
                                                              // time, so the future record reads as stale and a refresh fires.
        const NOW_REFRESH: u64 = T + FUTURE_SKEW_TOLERANCE_MS + STALE_RECORD_REFRESH_MS + 1;
        // Sane refresh payload: authored just before NOW_REFRESH (not future → not
        // clamped) and above the old clamped wall_ms, so it wins same-source LWW.
        const SANE_ANNOUNCED: u64 = NOW_REFRESH - 1;

        let r = ReachabilityResolver::new();
        let clock = Arc::new(AtomicU64::new(T));
        let c2 = Arc::clone(&clock);
        r.set_clock(Arc::new(move || c2.load(Ordering::SeqCst)));

        // (a) Insert the far-future pkarr record at clock = T. The dial view shows
        //     it (raw announce preserved), but effective freshness + synthetic HLC
        //     wall are both clamped to now + skew.
        let owner = OwnerAddr([1u8; 16]);
        r.update_with_source(
            owner,
            make_payload(7, FUTURE_ANNOUNCED),
            make_hlc(FUTURE_ANNOUNCED, 0, ""),
            ReachabilitySource::PkarrLive,
        );
        let (_, entry) = r
            .resolve_entry_by_node_id(&node_id_bytes(7))
            .expect("future record wins the dial view");
        assert_eq!(
            entry.payload.announced_at_ms, FUTURE_ANNOUNCED,
            "raw announce preserved"
        );
        assert_eq!(
            entry.effective_announced_at_ms, CLAMP_CEIL,
            "effective clamped to now + skew"
        );
        assert_eq!(
            entry.hlc.wall_ms, CLAMP_CEIL,
            "synthetic pkarr HLC wall_ms clamped too"
        );

        // (b) Register a sane-payload fallback, advance the clock past the window,
        //     and fire the refresh.
        let counter = Arc::new(AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&counter),
            payloads: vec![make_payload(7, SANE_ANNOUNCED)],
        }));
        clock.store(NOW_REFRESH, Ordering::SeqCst);
        r.maybe_refresh_stale(owner, node_id_bytes(7), NOW_REFRESH);
        for _ in 0..50 {
            if counter.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "stale future record triggers exactly one refresh"
        );

        // The sane payload replaced the future record; the dial view has moved.
        let (_, healed) = r
            .resolve_entry_by_node_id(&node_id_bytes(7))
            .expect("healed record");
        assert_eq!(
            healed.payload.announced_at_ms, SANE_ANNOUNCED,
            "dial view moved off the future record onto the sane payload"
        );
        assert_eq!(
            healed.effective_announced_at_ms, SANE_ANNOUNCED,
            "effective is the sane (unclamped) time"
        );
    }

    /// ZEB-621 (Qodo #3): a burst of stale-refresh spawns is bounded by
    /// [`PKARR_REFRESH_MAX_CONCURRENT`]. Ten owners due at once never put more than
    /// four pkarr `resolve` calls in flight simultaneously, yet all ten resolve.
    #[tokio::test(start_paused = true)]
    async fn refresh_fan_out_bounded_by_semaphore() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct InFlightFallback {
            in_flight: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
            done: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl ReachabilityFallback<OwnerAddr> for InFlightFallback {
            async fn resolve(&self, _addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
                let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(cur, Ordering::SeqCst);
                // Hold the permit across a brief (paused) network delay so the
                // concurrent-in-flight count is observable.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                self.done.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            }
        }

        let r = ReachabilityResolver::new();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(InFlightFallback {
            in_flight: Arc::clone(&in_flight),
            peak: Arc::clone(&peak),
            done: Arc::clone(&done),
        }));

        // Ten distinct owners, each with a >24h-stale record (default real-epoch
        // clock ⇒ announced_at 1_000 is far past the 24h window).
        const N: u8 = 10;
        for i in 0..N {
            r.update(
                OwnerAddr([i; 16]),
                make_payload(i, 1_000),
                make_hlc(1_000, 0, "d"),
            );
        }
        let now = 1_000 + STALE_RECORD_REFRESH_MS + 1;
        for i in 0..N {
            r.maybe_refresh_stale(OwnerAddr([i; 16]), node_id_bytes(i), now);
        }

        // Drive paused time until every refresh has drained.
        for _ in 0..200 {
            if done.load(Ordering::SeqCst) == N as usize {
                break;
            }
            tokio::time::advance(std::time::Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(
            done.load(Ordering::SeqCst),
            N as usize,
            "all ten refreshes eventually resolve"
        );
        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(observed_peak >= 1, "refreshes actually ran");
        assert!(
            observed_peak <= PKARR_REFRESH_MAX_CONCURRENT,
            "fan-out bounded to {} (observed peak {observed_peak})",
            PKARR_REFRESH_MAX_CONCURRENT
        );
    }

    #[test]
    fn fleet_sibling_dto_tag() {
        assert_eq!(
            ReachabilitySource::FleetSibling.as_dto_str(),
            "fleetSibling"
        );
    }

    #[test]
    fn fleet_sibling_entry_is_dialable_via_node_id_and_freshest() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0x51; 16]);
        let payload = make_payload(0xB2, 5000);
        r.update_with_source(
            owner,
            payload.clone(),
            Hlc {
                wall_ms: 5000,
                logical: 0,
                device_id: String::new(),
            },
            ReachabilitySource::FleetSibling,
        );
        // Dial authority: resolvable by node id (uses freshest()).
        let (got_owner, got) = r
            .resolve_by_node_id(&payload.iroh_node_id)
            .expect("fleet entry resolvable by node id");
        assert_eq!(got_owner, owner);
        assert_eq!(got.iroh_node_id, payload.iroh_node_id);
        // Butler/diagnostics authority (durable_preferred) excludes fleet:
        // fleet rows carry an empty butler_set and must not shape butler views.
        assert!(
            r.resolve(&owner).is_empty(),
            "fleet-only key must not surface via durable_preferred resolve()"
        );
    }

    #[test]
    fn fleet_and_durable_slots_coexist_without_clobber_on_same_key() {
        // A sibling that is ALSO a shared-community co-member: its community
        // DurableCrdt record and its FleetSibling record share the SAME key
        // (self_owner, node_id). Distinct slots must keep both.
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0x77; 16]);
        let node = 0xAB;
        let durable = make_payload(node, 100);
        let fleet = make_payload(node, 200); // fresher announce
        assert_eq!(durable.iroh_node_id, fleet.iroh_node_id, "same key");

        r.update_with_source(
            owner,
            durable.clone(),
            Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            ReachabilitySource::DurableCrdt,
        );
        r.update_with_source(
            owner,
            fleet.clone(),
            Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: String::new(),
            },
            ReachabilitySource::FleetSibling,
        );

        // freshest() (dial) = the fresher fleet record.
        let (_, freshest) = r
            .resolve_by_node_id(&fleet.iroh_node_id)
            .expect("resolvable");
        assert_eq!(freshest.announced_at_ms, 200);
        // durable_preferred() (butler/diag) still returns the durable record —
        // proof the fleet write did NOT clobber the durable slot.
        let diag = r.resolve(&owner);
        assert_eq!(diag.len(), 1);
        assert_eq!(diag[0].announced_at_ms, 100);
    }

    // ZEB-702: the dial view (`list_dialable_peers`) INCLUDES the fleet slot,
    // unlike the butler/diagnostics view (`list_active_peers`, backed by
    // `durable_preferred`). This asymmetry is the keystone: a SAS-paired
    // cert-only butler is known only via its fleet slot, so it must be dialable
    // (re-dialed at boot) even though it never surfaces in butler/diag views.
    #[test]
    fn list_dialable_peers_includes_fleet_only_that_list_active_excludes() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0x51; 16]);
        let payload = make_payload(0xB2, 5000);
        r.update_with_source(
            owner,
            payload.clone(),
            make_hlc(5000, 0, ""),
            ReachabilitySource::FleetSibling,
        );

        // Dial view returns the fleet-slot-only sibling, full entry + source.
        let dialable = r.list_dialable_peers();
        assert_eq!(dialable.len(), 1, "one row per (owner, device)");
        assert_eq!(dialable[0].0, owner);
        assert_eq!(dialable[0].1.payload.iroh_node_id, payload.iroh_node_id);
        assert_eq!(dialable[0].1.source, ReachabilitySource::FleetSibling);

        // Butler/diagnostics view still excludes it (deliberate asymmetry).
        assert!(
            r.list_active_peers().is_empty(),
            "fleet-only entry must NOT surface via list_active_peers()"
        );
    }

    // ZEB-702: when a peer has entries in more than one slot, the dial view
    // returns the FRESHEST (by `effective_announced_at_ms`, ties → durable),
    // matching the freshest-wins dial semantics the runtime kick gate uses.
    #[test]
    fn list_dialable_peers_returns_freshest_across_slots() {
        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0x77; 16]);
        let node = 0xAB;
        let durable = make_payload(node, 100);
        let fleet = make_payload(node, 200); // fresher announce
        assert_eq!(durable.iroh_node_id, fleet.iroh_node_id, "same key");

        r.update_with_source(
            owner,
            durable.clone(),
            make_hlc(100, 0, "d"),
            ReachabilitySource::DurableCrdt,
        );
        r.update_with_source(
            owner,
            fleet.clone(),
            make_hlc(200, 0, ""),
            ReachabilitySource::FleetSibling,
        );

        let dialable = r.list_dialable_peers();
        assert_eq!(dialable.len(), 1, "distinct slots collapse to one dial row");
        assert_eq!(
            dialable[0].1.payload.announced_at_ms, 200,
            "the fresher fleet record wins the dial view"
        );
        assert_eq!(dialable[0].1.source, ReachabilitySource::FleetSibling);
    }

    // Reuses the `CountingFallback` defined above (`payloads: Vec::new()` — this
    // test only cares about call count, not the returned records).
    #[tokio::test]
    async fn maybe_refresh_stale_skips_fleet_sibling_but_fires_for_durable() {
        let r = ReachabilityResolver::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&calls),
            payloads: Vec::new(),
        }));

        // A deliberately STALE fleet entry (announced far in the past).
        let owner_f = OwnerAddr([0xF1; 16]);
        let fleet = make_payload(0xF1, 1_000);
        r.update_with_source(
            owner_f,
            fleet.clone(),
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: String::new(),
            },
            ReachabilitySource::FleetSibling,
        );
        // now_ms far past the staleness window; every OTHER early-return
        // condition (record present, fallback installed, cooldown fresh) is
        // satisfied, so a non-zero count could ONLY come from a missing guard.
        r.maybe_refresh_stale(
            owner_f,
            fleet.iroh_node_id,
            1_000 + STALE_RECORD_REFRESH_MS + 1,
        );
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "fleet-sibling stale entry must not trigger a pkarr re-resolve"
        );

        // Positive control: a stale DURABLE entry DOES trigger the refresh.
        let owner_d = OwnerAddr([0xD1; 16]);
        let durable = make_payload(0xD1, 1_000);
        r.update_with_source(
            owner_d,
            durable.clone(),
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "d".into(),
            },
            ReachabilitySource::DurableCrdt,
        );
        r.maybe_refresh_stale(
            owner_d,
            durable.iroh_node_id,
            1_000 + STALE_RECORD_REFRESH_MS + 1,
        );
        let mut fired = false;
        for _ in 0..200 {
            if calls.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                fired = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(fired, "durable stale entry must trigger a pkarr re-resolve");
    }
}
