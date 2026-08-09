//! ZEB-473 (DM-over-iroh, Move 1a): inbound PQ tunnel acceptor.
//!
//! Implements [`IrohHandshakeDispatcher`] for the `harmony/tunnel/v1` ALPN. The
//! accept loop in `zenoh_iroh_transport` routes a negotiated tunnel connection
//! here; we spawn the responder driver, which completes the PQ handshake,
//! registers the live session into the [`TunnelManager`] (so our outbound DMs to
//! this peer reuse the bidirectional tunnel), and feeds inbound `FrameTag::Dm`
//! payloads onto the ingest seam.
//!
//! ## Admission (ZEB-757 + ZEB-758)
//!
//! The inbound path is the client's live, remote-triggered tunnel-population
//! surface — any peer that completes a valid PQ handshake gets a session
//! registered ([`TunnelManager::register_inbound`], inside the
//! `harmony_tunnel_iroh` crate), with **no contact allowlist**. Admission runs in
//! [`InboundAdmission`] and gates on the **authenticated iroh `remote_id()`** (a
//! `[u8; 32]` endpoint key that is un-spoofable, known pre-PQ-handshake, and
//! stable across the ephemeral-PQ-identity rotation a Sybil would use — the same
//! seam the friend/invite/pex acceptors gate on), in three layers, cheapest-first:
//!
//! 1. **ZEB-758 rate shield** — a per-source [`KeyedSlidingWindow`] sheds a *fast*
//!    handshake flood from one source before any semaphore or PQ-crypto work.
//! 2. **ZEB-758 per-source concurrency sub-cap** — one source may hold at most
//!    [`PER_SOURCE_INBOUND_TUNNEL_MAX`] live tunnels, so a Sybil rotating PQ
//!    identities cannot monopolise the global population (it now needs that many
//!    *distinct iroh endpoint keys* to saturate the host).
//! 3. **ZEB-757 global population cap** — [`MAX_INBOUND_TUNNEL_SESSIONS`] total
//!    live inbound tunnels, one lifetime-held permit each, bounding host
//!    fd/memory/CPU.
//!
//! Layers 2+3 hand back one [`InboundPermit`] held for the tunnel's whole
//! lifetime; dropping it (when the tunnel dies) frees both the per-source slot and
//! the global-population slot. A rejected connection does not drop the DM:
//! transport is hybrid (always-deposit plus best-effort tunnel), so the sender's
//! copy still lands via the deposit rung and a later attempt succeeds once a slot
//! frees.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use harmony_identity::PqPrivateIdentity;
use iroh::endpoint::Connection;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use crate::friend_intro::KeyedSlidingWindow;
use crate::iroh_invite_acceptor::IrohHandshakeDispatcher;
use crate::tunnel_manager::{InboundDm, TunnelManager};

/// ZEB-757: strict cap on the number of concurrently-live INBOUND PQ tunnel
/// sessions this node will accept. Each admitted inbound connection holds one
/// [`InboundPermit`] for the tunnel's whole lifetime, so this bounds the
/// session-map / spawned-responder population an inbound peer set can grow.
/// Distinct from `MAX_CONCURRENT_TUNNEL_SENDS` (outbound send-task concurrency,
/// permit released per-send) — do NOT conflate them. Sized to match the node's
/// `MAX_TUNNEL_CONNECTIONS` (harmony#295); idle tunnels are reaped, so 64
/// concurrent live inbound tunnels is generous, and overflow degrades to the
/// deposit rung rather than dropping DMs.
const MAX_INBOUND_TUNNEL_SESSIONS: usize = 64;

/// ZEB-758: per-source concurrency sub-cap — the most live inbound tunnels a
/// single authenticated iroh `remote_id` may hold at once, out of the global
/// [`MAX_INBOUND_TUNNEL_SESSIONS`]. An honest endpoint opens ~1 concurrent
/// inbound tunnel (bidirectional, reused), so 8 is generous reconnect headroom
/// while forcing a Sybil to mint 8 *distinct iroh endpoint keys* — not just PQ
/// identities, which this key ignores — to monopolise the host.
const PER_SOURCE_INBOUND_TUNNEL_MAX: u32 = 8;

/// ZEB-758: per-source rate-shield cap — the most inbound tunnel handshakes one
/// source may open within [`INBOUND_TUNNEL_RATE_WINDOW_MS`]. Sheds a fast
/// connect/close hammering flood cheaply (before any semaphore/crypto work)
/// without ever tripping an honest peer's occasional reconnects.
const MAX_INBOUND_TUNNEL_HANDSHAKES: usize = 30;

/// ZEB-758: sliding window for the per-source rate shield (30 handshakes/min).
const INBOUND_TUNNEL_RATE_WINDOW_MS: u64 = 60_000;

/// ZEB-757: emit at most one inbound-shed warning per this many ms. Rejecting is
/// the CHEAP path an inbound flood drives, so a warning per rejected connection
/// would hand that flood an unbounded log-write amplifier — exactly the cost the
/// caps exist to shed. Throttling keeps the operator signal (shedding started,
/// how heavily, and by which reason) while bounding log volume.
const REJECT_WARN_INTERVAL_MS: u64 = 30_000;

/// Sentinel for [`InboundAdmission::last_warn_ms`] meaning "no shed warning
/// emitted yet", so the FIRST shed always logs immediately rather than waiting
/// out a window.
const WARN_NEVER: u64 = u64::MAX;

/// Why an inbound tunnel connection was shed. Each reason maps to a distinct
/// connection-close string and is counted separately in the throttled warning so
/// a targeted Sybil (rate / per-source dominating) is legible versus benign
/// capacity pressure (population dominating).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShedReason {
    /// ZEB-758: the source exceeded its handshake rate window.
    Rate,
    /// ZEB-758: the source already holds [`PER_SOURCE_INBOUND_TUNNEL_MAX`] live
    /// tunnels.
    PerSource,
    /// ZEB-757: the global live inbound population is at
    /// [`MAX_INBOUND_TUNNEL_SESSIONS`].
    Population,
}

impl ShedReason {
    /// The QUIC close reason sent to a shed peer (kept distinct per reason for
    /// remote-side diagnosability). `tunnel-population-cap` is unchanged from
    /// ZEB-757.
    fn close_reason(self) -> &'static [u8] {
        match self {
            ShedReason::Rate => b"tunnel-rate-cap",
            ShedReason::PerSource => b"per-source-tunnel-cap",
            ShedReason::Population => b"tunnel-population-cap",
        }
    }
}

/// RAII guard for one admitted inbound tunnel. Holds the global-population
/// [`Semaphore`] permit AND accounts for one per-source concurrency slot; dropping
/// it (when the tunnel dies) frees BOTH. Moved into the responder task so the
/// slots are held for the tunnel's whole lifetime.
pub(crate) struct InboundPermit {
    /// Freed on drop (after the per-source decrement), releasing the global slot.
    _global: OwnedSemaphorePermit,
    /// Shared handle to the per-source live-count map (see
    /// [`InboundAdmission::per_source`]); the guard's `Drop` decrements this
    /// source's count.
    per_source: Arc<Mutex<HashMap<[u8; 32], u32>>>,
    /// The authenticated iroh `remote_id` this tunnel was admitted for.
    source: [u8; 32],
}

impl Drop for InboundPermit {
    fn drop(&mut self) {
        let mut counts = self
            .per_source
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(n) = counts.get_mut(&self.source) {
            *n -= 1;
            if *n == 0 {
                // Keep the map self-bounded: a source is tracked only while it
                // holds >=1 live permit, so the map never exceeds the global cap
                // of entries and a rotating-source flood cannot grow it.
                counts.remove(&self.source);
            }
        }
        // `_global` drops after this scope, releasing the global-population slot.
    }
}

/// ZEB-757/758: admission control for the live inbound tunnel population. Owns
/// the global population semaphore, the per-source concurrency counter, and the
/// per-source rate shield — each admitted inbound tunnel keeps one
/// [`InboundPermit`] for its whole lifetime, so [`try_admit`] returning
/// `Err(reason)` means the connection must be rejected. Split out as its own type
/// so the bounds are unit-testable without a real iroh endpoint.
///
/// Also owns the shed-log throttle (see [`REJECT_WARN_INTERVAL_MS`]).
///
/// [`try_admit`]: InboundAdmission::try_admit
struct InboundAdmission {
    /// ZEB-757 global population cap: one owned permit per live inbound tunnel.
    sem: Arc<Semaphore>,
    /// ZEB-758 per-source concurrency: live permits currently held per
    /// authenticated iroh `remote_id`. Self-bounded at `<=` the global cap of
    /// entries (a source is present only while holding `>=1` permit), so no
    /// eviction is needed.
    per_source: Arc<Mutex<HashMap<[u8; 32], u32>>>,
    /// ZEB-758 per-source concurrency sub-cap (see
    /// [`PER_SOURCE_INBOUND_TUNNEL_MAX`]).
    per_source_max: u32,
    /// ZEB-758 per-source rate shield: sheds a fast per-source handshake flood
    /// before any semaphore/PQ-crypto work. The audited bounded-eviction
    /// primitive (`friend_intro::KeyedSlidingWindow`) so a rotating-key flood
    /// cannot turn the shield into a memory-DoS of its own.
    rate: Mutex<KeyedSlidingWindow<[u8; 32]>>,
    /// Monotonic base for [`now_ms`](InboundAdmission::now_ms). The rate window
    /// and the warn throttle run on this limiter's OWN monotonic clock, never
    /// wall time — a wall-clock step would distort both (same convention as the
    /// ZEB-711 shields).
    started: Instant,
    /// Monotonic ms at which the last shed warning was emitted, or [`WARN_NEVER`]
    /// if none has been.
    last_warn_ms: AtomicU64,
    /// Rate-shed count accumulated since the last emitted warning.
    rate_since_warn: AtomicU64,
    /// Per-source-cap shed count accumulated since the last emitted warning.
    per_source_since_warn: AtomicU64,
    /// Population-cap shed count accumulated since the last emitted warning.
    population_since_warn: AtomicU64,
}

impl InboundAdmission {
    fn new() -> Self {
        Self::with_caps(
            MAX_INBOUND_TUNNEL_SESSIONS,
            PER_SOURCE_INBOUND_TUNNEL_MAX,
            MAX_INBOUND_TUNNEL_HANDSHAKES,
            INBOUND_TUNNEL_RATE_WINDOW_MS,
        )
    }

    /// Test/tuning constructor — deterministic tiny caps in unit tests.
    fn with_caps(
        global_cap: usize,
        per_source_max: u32,
        rate_max: usize,
        rate_window_ms: u64,
    ) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(global_cap)),
            per_source: Arc::new(Mutex::new(HashMap::new())),
            per_source_max,
            rate: Mutex::new(KeyedSlidingWindow::new(rate_max, rate_window_ms)),
            started: Instant::now(),
            last_warn_ms: AtomicU64::new(WARN_NEVER),
            rate_since_warn: AtomicU64::new(0),
            per_source_since_warn: AtomicU64::new(0),
            population_since_warn: AtomicU64::new(0),
        }
    }

    /// Poison-tolerant lock on the per-source count map: a panic elsewhere must
    /// not wedge the acceptor — the guarded state is plain counters, safe to keep
    /// using.
    fn lock_per_source(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], u32>> {
        self.per_source
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Poison-tolerant lock on the rate shield.
    fn lock_rate(&self) -> std::sync::MutexGuard<'_, KeyedSlidingWindow<[u8; 32]>> {
        self.rate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Milliseconds since this admission control was built (monotonic).
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Admit one inbound tunnel connection from `source` (the authenticated iroh
    /// `remote_id`). `Ok(permit)` — the caller MUST hold the permit for the
    /// tunnel's whole lifetime (move it into the responder task) so both the
    /// per-source slot and the global-population slot free only when the tunnel
    /// dies. `Err(reason)` sheds the connection (see [`ShedReason`]).
    ///
    /// Layers, cheapest-first: (1) a per-source RATE shield sheds a fast flood
    /// before any semaphore/crypto work; (2) a per-source CONCURRENCY sub-cap +
    /// the global population cap, taken atomically under the per-source lock so N
    /// concurrent admits cannot overshoot either cap.
    fn try_admit(&self, source: [u8; 32], now_ms: u64) -> Result<InboundPermit, ShedReason> {
        // Layer 1 — rate shield. Records on admit; holds no permit. Recording
        // before the inner caps is deliberate: a source hammering at its
        // concurrency cap SHOULD burn rate budget, and an honest source hitting
        // rare global saturation loses a negligible slice of a generous budget.
        if !self.lock_rate().admit(source, now_ms) {
            return Err(ShedReason::Rate);
        }
        // Layer 2 — per-source concurrency + global population, atomic under the
        // per-source lock. `try_acquire_owned` never blocks, so holding the std
        // mutex across it is safe (no suspension point).
        let mut counts = self.lock_per_source();
        if counts.get(&source).copied().unwrap_or(0) >= self.per_source_max {
            return Err(ShedReason::PerSource);
        }
        let global = Arc::clone(&self.sem)
            .try_acquire_owned()
            .map_err(|_| ShedReason::Population)?;
        *counts.entry(source).or_insert(0) += 1;
        Ok(InboundPermit {
            _global: global,
            per_source: Arc::clone(&self.per_source),
            source,
        })
    }

    /// Record one shed (by reason) and decide whether it should log.
    ///
    /// Returns `Some((rate, per_source, population))` — the per-reason shed counts
    /// the ONE emitted warning covers — when a warning is due (the first shed
    /// always logs, then at most one per [`REJECT_WARN_INTERVAL_MS`]). Returns
    /// `None` when this shed is instead folded into the counts the next warning
    /// will carry. `now_ms` is a parameter so the window is testable without
    /// sleeping.
    fn note_rejection(&self, reason: ShedReason, now_ms: u64) -> Option<(u64, u64, u64)> {
        let counter = match reason {
            ShedReason::Rate => &self.rate_since_warn,
            ShedReason::PerSource => &self.per_source_since_warn,
            ShedReason::Population => &self.population_since_warn,
        };
        counter.fetch_add(1, Ordering::Relaxed);

        let last = self.last_warn_ms.load(Ordering::Acquire);
        let due = last == WARN_NEVER || now_ms.saturating_sub(last) >= REJECT_WARN_INTERVAL_MS;
        if !due {
            return None;
        }
        // Concurrent sheds can all see the window as due; only the one that
        // installs its timestamp logs, the rest fold into the next window.
        if self
            .last_warn_ms
            .compare_exchange(last, now_ms, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        Some((
            self.rate_since_warn.swap(0, Ordering::Relaxed),
            self.per_source_since_warn.swap(0, Ordering::Relaxed),
            self.population_since_warn.swap(0, Ordering::Relaxed),
        ))
    }
}

/// Inbound `harmony/tunnel/v1` acceptor. Holds the handles the responder driver
/// needs: our own PQ private identity (the responder self-input), the manager to
/// register the live session into, and the ingest channel for received DMs — plus
/// the ZEB-757/758 admission control that bounds the live inbound population
/// globally and per source.
pub struct IrohTunnelAcceptor {
    local_pq: Arc<PqPrivateIdentity>,
    mgr: Arc<TunnelManager>,
    ingest_tx: mpsc::Sender<InboundDm>,
    admission: InboundAdmission,
}

impl IrohTunnelAcceptor {
    pub fn new(
        local_pq: Arc<PqPrivateIdentity>,
        mgr: Arc<TunnelManager>,
        ingest_tx: mpsc::Sender<InboundDm>,
    ) -> Self {
        Self {
            local_pq,
            mgr,
            ingest_tx,
            admission: InboundAdmission::new(),
        }
    }
}

#[async_trait]
impl IrohHandshakeDispatcher for IrohTunnelAcceptor {
    async fn handle_connection(&self, conn: Connection) {
        // ZEB-757/758: gate on the authenticated iroh source id BEFORE spawning
        // the responder. The QUIC connection is already established by the shared
        // accept loop, but rejecting here still sheds the expensive per-tunnel
        // work — the PQ handshake, the session registration, and the long-lived
        // responder task — so a flood costs only a QUIC accept + close.
        // `remote_id()` is the un-spoofable iroh endpoint key, available
        // pre-handshake and stable across the PQ-identity rotation a Sybil uses.
        let source = *conn.remote_id().as_bytes();
        let now_ms = self.admission.now_ms();
        let permit = match self.admission.try_admit(source, now_ms) {
            Ok(permit) => permit,
            Err(reason) => {
                // Throttled: a flood drives this path, so it must not become a
                // log-write amplifier (the warning carries per-reason counts, so
                // nothing is silently lost). Rate/per-source dominating signals a
                // targeted source; population dominating is benign capacity.
                if let Some((rate_shed, per_source_shed, population_shed)) =
                    self.admission.note_rejection(reason, now_ms)
                {
                    tracing::warn!(
                        global_cap = MAX_INBOUND_TUNNEL_SESSIONS,
                        per_source_cap = PER_SOURCE_INBOUND_TUNNEL_MAX,
                        rate_shed,
                        per_source_shed,
                        population_shed,
                        window_ms = REJECT_WARN_INTERVAL_MS,
                        "inbound tunnel admission shedding connections \
                         (DMs still deliver via the deposit rung)"
                    );
                }
                conn.close(0u32.into(), reason.close_reason());
                return;
            }
        };

        // Spawn so a slow/hung peer can't block the accept loop (the responder
        // driver owns the connection for the tunnel's whole lifetime).
        let local_pq = Arc::clone(&self.local_pq);
        let mgr = Arc::clone(&self.mgr);
        let ingest_tx = self.ingest_tx.clone();
        tokio::spawn(async move {
            // Held for the tunnel's whole lifetime; dropping it when
            // `run_tunnel_responder` returns frees BOTH the per-source slot and
            // the global-population slot.
            let _permit = permit;
            crate::tunnel_task::run_tunnel_responder(conn, local_pq, mgr, ingest_tx).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A distinct 32-byte source key per index (an iroh `remote_id` stand-in).
    fn src(n: u8) -> [u8; 32] {
        [n; 32]
    }

    impl InboundAdmission {
        /// Number of distinct sources currently tracked (i.e. holding `>=1` live
        /// permit). Used to assert the per-source map is self-bounded / empties.
        fn live_source_count(&self) -> usize {
            self.lock_per_source().len()
        }
    }

    // ZEB-758: the per-source concurrency sub-cap bounds how many live inbound
    // tunnels ONE source may hold, independently of the global cap — the
    // Sybil-fairness guarantee (a single source rotating PQ identities cannot
    // monopolise the global population).
    #[test]
    fn per_source_concurrency_is_capped_below_the_global_cap() {
        // Global room for 10, but a per-source cap of 2; rate generous so only
        // the per-source cap bites.
        let admission = InboundAdmission::with_caps(10, 2, 1_000, 60_000);
        let s = src(1);

        let p1 = admission.try_admit(s, 0).expect("1st from source admitted");
        let _p2 = admission.try_admit(s, 0).expect("2nd from source admitted");
        assert!(
            matches!(admission.try_admit(s, 0), Err(ShedReason::PerSource)),
            "3rd from the same source shed on the per-source cap despite global room"
        );

        // Freeing one of this source's permits re-opens exactly one slot.
        drop(p1);
        let _p3 = admission
            .try_admit(s, 0)
            .expect("slot re-opens after a held permit from this source drops");
        assert!(
            matches!(admission.try_admit(s, 0), Err(ShedReason::PerSource)),
            "still capped at 2 concurrent from this source"
        );
    }

    // ZEB-757 (subsumed): the global cap still strictly bounds the TOTAL live
    // inbound population across DISTINCT sources, and the RAII guard frees the
    // global slot on drop (not just the per-source count).
    #[test]
    fn global_population_cap_bounds_distinct_sources_and_frees_on_drop() {
        // Global cap 2; generous per-source + rate so only the global cap bites.
        let admission = InboundAdmission::with_caps(2, 8, 1_000, 60_000);

        let p1 = admission.try_admit(src(1), 0).expect("1st source admitted");
        let _p2 = admission.try_admit(src(2), 0).expect("2nd source admitted");
        assert!(
            matches!(admission.try_admit(src(3), 0), Err(ShedReason::Population)),
            "3rd distinct source shed on the GLOBAL cap despite per-source room"
        );

        // Dropping a permit frees the global slot — proves the guard releases the
        // semaphore, not only the per-source count.
        drop(p1);
        let _p3 = admission
            .try_admit(src(3), 0)
            .expect("global slot re-opens after a held permit drops");
    }

    // ZEB-758: the per-source map is self-bounded — a source is tracked only while
    // it holds a live permit, so a rotating-source flood cannot grow it, and it
    // empties once every permit is dropped.
    #[test]
    fn per_source_map_is_self_bounded_and_empties_when_permits_drop() {
        let admission = InboundAdmission::with_caps(4, 8, 1_000, 60_000);
        {
            let _a1 = admission.try_admit(src(1), 0).expect("admit source 1");
            let _a2 = admission
                .try_admit(src(1), 0)
                .expect("admit source 1 again");
            let _b1 = admission.try_admit(src(2), 0).expect("admit source 2");
            assert_eq!(
                admission.live_source_count(),
                2,
                "exactly the two distinct live sources are tracked"
            );
        }
        // All guards dropped at scope end.
        assert_eq!(
            admission.live_source_count(),
            0,
            "per-source map empties once every permit is dropped"
        );
    }

    // ZEB-758: the rate shield sheds a FAST per-source flood (independent of
    // concurrent population — the permits here drop immediately) and recovers once
    // the window elapses, so it never permanently locks out an honest peer.
    #[test]
    fn rate_shield_sheds_a_fast_source_flood_and_recovers_after_the_window() {
        // Rate cap 3 per 100ms window; generous global + per-source so only the
        // rate shield bites. Permits drop immediately — the rate axis is about
        // handshake RATE, independent of concurrent population.
        let admission = InboundAdmission::with_caps(64, 8, 3, 100);
        let s = src(7);

        for i in 0..3 {
            admission
                .try_admit(s, 0)
                .unwrap_or_else(|_| panic!("attempt {i} within the rate window admitted"));
        }
        assert!(
            matches!(admission.try_admit(s, 0), Err(ShedReason::Rate)),
            "a 4th rapid attempt from the same source is rate-shed"
        );
        assert!(
            admission.try_admit(s, 101).is_ok(),
            "the source is re-admitted once its rate window has elapsed"
        );
    }

    // ZEB-757/758: the shed warning is throttled so a flood cannot turn the cheap
    // reject path into an unbounded log-write amplifier, and it carries a
    // PER-REASON breakdown so a targeted Sybil (rate/per-source) is distinguishable
    // from benign capacity pressure (population). The first shed logs immediately;
    // afterwards at most one warning per window, reporting every reason's count
    // since the last — nothing silently dropped.
    #[test]
    fn reject_warning_is_throttled_and_reports_per_reason_counts() {
        let admission = InboundAdmission::with_caps(1, 1, 1_000, 60_000);

        assert_eq!(
            admission.note_rejection(ShedReason::Rate, 0),
            Some((1, 0, 0)),
            "first shed logs immediately with its reason counted"
        );
        // Inside the window — folded into the next warning's counts.
        assert_eq!(admission.note_rejection(ShedReason::PerSource, 1_000), None);
        assert_eq!(
            admission.note_rejection(ShedReason::Population, 2_000),
            None
        );
        assert_eq!(admission.note_rejection(ShedReason::Rate, 3_000), None);
        // Window elapsed — one warning covering every reason shed since the last.
        assert_eq!(
            admission.note_rejection(ShedReason::Rate, REJECT_WARN_INTERVAL_MS),
            Some((2, 1, 1)),
            "one warning covers the 2 rate + 1 per-source + 1 population sheds folded in"
        );
        // The new window starts closed again.
        assert_eq!(
            admission.note_rejection(ShedReason::Population, REJECT_WARN_INTERVAL_MS + 1),
            None,
        );
    }
}
