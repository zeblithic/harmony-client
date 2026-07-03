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

use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use crate::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr};
use crate::peer_liveness::LivenessHandle;
use crate::reachability_record::ReachabilityAnnouncePayload;
use crate::reconnect_supervisor::{ReconnectTrigger, SupervisorHandle};

/// Async fallback called by `resolve_async` when the in-memory CRDT cache has
/// no entry for a given owner. Implemented by `PkarrResolverAdapter` (case C)
/// and by test stubs. The concrete impl is injected at boot via
/// `set_fallback_source`.
#[async_trait]
pub trait ReachabilityFallback: Send + Sync {
    async fn resolve(&self, addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload>;
}

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
}

impl ReachabilitySource {
    /// Stable camelCase tag for the `connectivity_list_peer_reachability` DTO so
    /// consumers (e.g. the e2e durable-replication barrier) can tell a durable
    /// CRDT-replicated record apart from a live pkarr cache-back.
    pub fn as_dto_str(self) -> &'static str {
        match self {
            ReachabilitySource::DurableCrdt => "durableCrdt",
            ReachabilitySource::PkarrLive => "pkarrLive",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverEntry {
    pub payload: ReachabilityAnnouncePayload,
    pub hlc: Hlc,
    pub source: ReachabilitySource,
}

pub struct ReachabilityResolver {
    inner: Arc<RwLock<BTreeMap<ResolverKey, ResolverEntry>>>,
    /// Wrapped in an outer `Arc` so that all clones share the same
    /// `RwLock` — wiring the fallback via `set_fallback_source` on any
    /// clone is immediately visible to all others (CodeRabbit PR #158 round 2).
    fallback_source: Arc<RwLock<Option<Arc<dyn ReachabilityFallback>>>>,
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
            inner: Arc::new(RwLock::new(BTreeMap::new())),
            fallback_source: Arc::new(RwLock::new(None)),
            supervisor: Arc::new(RwLock::new(None)),
            liveness: Arc::new(RwLock::new(None)),
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
        }
    }
}

impl ReachabilityResolver {
    pub fn new() -> Self {
        Self::default()
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
        let mut map = self.inner.write().expect("resolver write lock");
        let was_present = map.contains_key(&key);
        let next = ResolverEntry {
            payload,
            hlc,
            source,
        };
        // Track whether an existing record is LWW-replaced by one whose iroh
        // relay URL or direct-address SET materially differs — the signal the
        // reconnect supervisor re-dials on (ZEB-620). Computed BEFORE the
        // overwrite (the prior payload is gone afterwards) and only inside the
        // `should_replace` branch, so a byte-identical beacon republish and an
        // LWW-rejected stale record both leave it false (no ladder thrash).
        let mut record_changed = false;
        let do_replace = match map.get(&key) {
            Some(prev) => {
                let replace = should_replace(prev, &next);
                if replace {
                    record_changed = addressing_differs(&prev.payload, &next.payload);
                }
                replace
            }
            None => true,
        };
        if do_replace {
            map.insert(key, next);
        }
        drop(map);
        // ZEB-620: reconnect-supervisor triggers (the successor to ZEB-373's
        // retired `DialHint` mpsc). A first-learn kicks `NewPeer`; a material
        // record change on an already-known peer kicks `RecordChanged`. Mutually
        // exclusive by construction (`was_present`), so at most one fires per
        // update; `kick` is lossless and non-blocking.
        if let Some(sup) = self.supervisor.read().expect("supervisor lock").as_ref() {
            if !was_present {
                sup.kick(node_id, ReconnectTrigger::NewPeer);
            } else if record_changed {
                sup.kick(node_id, ReconnectTrigger::RecordChanged);
            }
        }
    }

    /// Returns ALL device records for `actor`. Multi-device owners
    /// (per ZEB-173) may have multiple entries; callers that need to
    /// dial the owner should try each in turn (Phase 2 will add ranking
    /// based on heartbeat/liveness).
    pub fn resolve(&self, actor: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
        let map = self.inner.read().expect("resolver read lock");
        map.range((*actor, [0u8; 32])..=(*actor, [0xFFu8; 32]))
            .map(|(_, v)| v.payload.clone())
            .collect()
    }

    pub fn list_active_peers(&self) -> Vec<(OwnerAddr, ReachabilityAnnouncePayload)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter()
            .map(|((owner, _node_id), v)| (*owner, v.payload.clone()))
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
            .map(|((owner, _node_id), v)| (*owner, v.payload.clone(), v.source))
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
    pub fn resolve_by_node_id(
        &self,
        node_id_bytes: &[u8; 32],
    ) -> Option<(OwnerAddr, ReachabilityAnnouncePayload)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter()
            .find(|((_, key_node_id), _)| key_node_id == node_id_bytes)
            .map(|((owner, _), v)| (*owner, v.payload.clone()))
    }

    /// Evict every device record for `actor`. Called from the membership-
    /// delta consumer on Leave / Kick events (PR #157 round 5 Cursor
    /// finding) so departed members don't linger in
    /// `connectivity_list_peer_reachability` or `resolve_by_node_id`
    /// until restart bootstrap re-filters them.
    ///
    /// Returns the number of entries removed (across all the owner's
    /// devices) so callers can log / decide whether to emit a UI hint.
    pub fn remove_owner(&self, actor: &OwnerAddr) -> usize {
        let mut map = self.inner.write().expect("resolver write lock");
        let to_remove: Vec<ResolverKey> = map
            .range((*actor, [0u8; 32])..=(*actor, [0xFFu8; 32]))
            .map(|(k, _)| *k)
            .collect();
        let n = to_remove.len();
        for k in to_remove {
            map.remove(&k);
        }
        n
    }

    /// Register a pkarr-backed fallback source. Called once at boot by
    /// lib.rs after wiring `PkarrResolverAdapter`. Thread-safe; may be
    /// called any number of times (latest wins).
    pub fn set_fallback_source(&self, fb: Arc<dyn ReachabilityFallback>) {
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
    /// `connectivity_redeem_invite_iroh` (Task 3). The resolver's
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
    /// later durable-CRDT record for the same `(owner, node_id)` then takes over
    /// the slot via the source-priority guard in `should_replace`.
    ///
    /// `async` for API alignment with the Task 3 IPC handler (which
    /// `.await`s this call between two sync-locked sections). The
    /// implementation itself is non-blocking — the underlying `RwLock`
    /// is `std::sync`, not `tokio::sync`.
    pub async fn seed_from_pkarr(
        &self,
        owner_addr: OwnerAddr,
        _device_hash: DeviceIdentityHash,
        payload: ReachabilityAnnouncePayload,
    ) {
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

    /// Like [`resolve`](Self::resolve), but carries each entry's source for the
    /// per-source butler-set freshness policy (Task 3, Decision 3).
    pub fn resolve_with_source(
        &self,
        actor: &OwnerAddr,
    ) -> Vec<(ReachabilityAnnouncePayload, ReachabilitySource)> {
        let map = self.inner.read().expect("resolver read lock");
        map.range((*actor, [0u8; 32])..=(*actor, [0xFFu8; 32]))
            .map(|(_, v)| (v.payload.clone(), v.source))
            .collect()
    }

    /// Like [`resolve_async`](Self::resolve_async), but carries source. Cache
    /// hits keep their stored source; on a cache miss the pkarr fallback results
    /// are stored `PkarrLive` and the AUTHORITATIVE post-store resolver view is
    /// re-read and returned — a concurrent durable-CRDT `update()` may have
    /// landed during the fallback await and won the slot via source-priority, so
    /// returning the pre-await pkarr vector would mis-tag a now-durable entry as
    /// `PkarrLive` (Qodo "resolve_async_with_source TOCTOU").
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
        // pre-await pkarr vector: source-priority LWW may have kept/installed a
        // concurrently-arrived durable entry for this `(owner, node_id)`, which
        // must be returned tagged `DurableCrdt`, not `PkarrLive` (CA3 TOCTOU).
        self.resolve_with_source(addr)
    }
}

/// Whether two reachability payloads carry materially different addressing, the
/// gate for the reconnect supervisor's `RecordChanged` trigger (ZEB-620): the
/// iroh relay URL differs, or the direct-address *set* differs. The direct
/// addresses are compared order- and duplicate-insensitively (via `BTreeSet`),
/// so a reordered republish of the same endpoints is not a change; a
/// byte-identical beacon refresh therefore returns `false` and the supervisor
/// does not re-arm the peer's backoff ladder for no reason.
fn addressing_differs(
    prev: &ReachabilityAnnouncePayload,
    next: &ReachabilityAnnouncePayload,
) -> bool {
    if prev.home_relay_url != next.home_relay_url {
        return true;
    }
    let prev_addrs: BTreeSet<_> = prev.direct_addresses.iter().collect();
    let next_addrs: BTreeSet<_> = next.direct_addresses.iter().collect();
    prev_addrs != next_addrs
}

/// LWW comparator. `Hlc` does not derive `Ord` (canonical-CBOR keying
/// constraints — see `owner_state_types.rs::Hlc`), so we compare by the
/// same lexicographic tuple `(wall_ms, logical, device_id)` used by
/// `Hlc::is_strictly_newer_than`. Ties on HLC fall through to
/// `announced_at_ms` then lex `iroh_node_id`, per spec §5.4.
fn should_replace(prev: &ResolverEntry, next: &ResolverEntry) -> bool {
    // Source-priority guard (ZEB-488; Qodo "Pkarr evicts durable reachability").
    // Durable-CRDT seal-targets are authoritative and window-EXEMPT, so the
    // single LWW slot per `(owner, iroh_node_id)` must never let a pkarr
    // cache-back evict a durable record — otherwise the entry becomes
    // `PkarrLive`, the 15-min freshness window re-applies, and a fully-offline
    // recipient's seal-targets vanish after the window (the exact gap this PR
    // closes). Conversely a durable announce that replicates LATER (with an
    // older `announced_at_ms` than a frequently-refreshed pkarr blob) must still
    // take over the slot, so an HLC-blind upgrade is required. Same-source
    // updates fall through to the HLC/announced_at/node_id LWW below.
    match (prev.source, next.source) {
        (ReachabilitySource::DurableCrdt, ReachabilitySource::PkarrLive) => return false,
        (ReachabilitySource::PkarrLive, ReachabilitySource::DurableCrdt) => return true,
        _ => {}
    }
    let prev_key = (
        prev.hlc.wall_ms,
        prev.hlc.logical,
        prev.hlc.device_id.as_str(),
    );
    let next_key = (
        next.hlc.wall_ms,
        next.hlc.logical,
        next.hlc.device_id.as_str(),
    );
    match next_key.cmp(&prev_key) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => match next
            .payload
            .announced_at_ms
            .cmp(&prev.payload.announced_at_ms)
        {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => next.payload.iroh_node_id > prev.payload.iroh_node_id,
        },
    }
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

    /// Q1 / ZEB-488: a fresher pkarr cache-back must NOT evict a durable record.
    /// The durable entry's butler-set is window-exempt; letting a 15-min-windowed
    /// pkarr entry overwrite the slot reopens "empty seal-targets after 15 min".
    #[test]
    fn durable_not_evicted_by_fresher_pkarr() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        r.update_with_source(
            actor,
            make_payload(1, 1000),
            make_hlc(1000, 0, "a"),
            ReachabilitySource::DurableCrdt,
        );
        // Fresher pkarr cache-back for the SAME (owner, node_id).
        r.update_with_source(
            actor,
            make_payload(1, 9000),
            make_hlc(9000, 0, "a"),
            ReachabilitySource::PkarrLive,
        );
        let tagged = r.resolve_with_source(&actor);
        assert_eq!(tagged.len(), 1);
        assert_eq!(
            tagged[0].1,
            ReachabilitySource::DurableCrdt,
            "durable record must survive a fresher pkarr cache-back"
        );
        assert_eq!(
            tagged[0].0.announced_at_ms, 1000,
            "durable payload retained, not the pkarr one"
        );
    }

    /// Q1 / ZEB-488: a durable announce that replicates LATER — with an OLDER
    /// announced_at than a frequently-refreshed pkarr blob — must still take over
    /// the slot. The pkarr→durable upgrade is HLC-blind by design.
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
        let tagged = r.resolve_with_source(&actor);
        assert_eq!(tagged.len(), 1);
        assert_eq!(
            tagged[0].1,
            ReachabilitySource::DurableCrdt,
            "durable must upgrade the slot regardless of HLC ordering"
        );
        assert_eq!(tagged[0].0.announced_at_ms, 1000);
    }

    /// The source guard governs only cross-source collisions; same-source updates
    /// keep ordinary HLC LWW.
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
        // is a no-op (should_replace returns false on full equality).
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
        assert_eq!(removed, 3, "all three of actor's devices evicted");

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
        assert_eq!(r.remove_owner(&actor), 0);

        r.update(actor, make_payload(0x01, 1000), make_hlc(1000, 0, "a"));
        assert_eq!(r.remove_owner(&actor), 1);
        // Second remove is a no-op — also 0.
        assert_eq!(r.remove_owner(&actor), 0);
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
        // Same key, newer HLC (so should_replace succeeds), IDENTICAL addressing.
        r.update(owner, make_payload(0x11, 1000), make_hlc(2000, 0, "a"));
        assert_eq!(
            sup.pending_trigger(node_id),
            None,
            "byte-identical addressing replay must not kick"
        );
    }

    /// An LWW-rejected stale record (`should_replace == false`) must NOT kick,
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
    impl ReachabilityFallback for StubFallback {
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
            .seed_from_pkarr(owner_addr, device_hash, payload.clone())
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
            .seed_from_pkarr(owner_addr, device_hash, payload)
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
    impl ReachabilityFallback for DurableInjectingFallback {
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
}
