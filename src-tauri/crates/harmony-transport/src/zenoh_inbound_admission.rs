//! ZEB-996: bounded admission for resolver-unknown ("stranger") inbound
//! `harmony/zenoh/v1` faces.
//!
//! ## Why this bounds strangers and not identity
//!
//! The zenoh accept path (`zenoh_iroh_transport::spawn_accept_loop`) admits any
//! peer whose QUIC handshake negotiates the zenoh ALPN. That is deliberate:
//! zenoh transport faces carry no authority — every sensitive payload layer
//! does its own crypto and ingest verification (owner-state envelopes, voting
//! epoch encryption, DM E2E, membership-at-HLC checks), profile cards are
//! public by intent, and the zenoh session keeps an unauthenticated LAN TCP
//! listener anyway. Gating faces on identity would also island new community
//! members during the join window, when *they* have already learned our
//! reachability record but our CRDT ingest has not yet produced theirs
//! (the same anti-islanding argument that makes `AdmissionOracle` fail open
//! on unknown node ids — which is exactly why that oracle cannot serve as an
//! inbound gate: an attacker's fresh endpoint is always unknown).
//!
//! What an open face DOES cost is availability: iroh endpoint ids are free to
//! mint, so a Sybil can otherwise create unbounded faces — unbounded
//! `zenoh_conns` registry growth plus linkstate routing amplification, against
//! ZEB-912's measured super-linear router flood (collapse ≈ N50). So the gate
//! bounds the axis the attacker actually needs (many *distinct* stranger
//! endpoint ids) while leaving resolver-known peers — bounded naturally by the
//! verified reachability projection — un-gated, preserving today's behavior
//! for every legitimate mesh member.
//!
//! ## Two axes, mirroring `iroh_tunnel_acceptor::InboundAdmission`
//!
//! 1. **Occupancy** — at most [`MAX_INBOUND_ZENOH_STRANGER_FACES`] live
//!    stranger faces. Tracked as a set of endpoint ids maintained at the same
//!    two points as the `zenoh_conns` registry (idempotent insert on admit,
//!    removal in the identity-guarded drop-watcher eviction), NOT as semaphore
//!    permits — a same-zid reconnect replaces its own face, and permit
//!    accounting would drift across that replacement window.
//! 2. **Rate** — at most [`ZENOH_STRANGER_ADMITS_PER_WINDOW`] *new* stranger
//!    admissions per [`ZENOH_STRANGER_RATE_WINDOW_MS`], via the audited
//!    bounded-eviction [`KeyedSlidingWindow`] under a single global key
//!    (per-source keying is meaningless when the key is free to mint — the
//!    global window is the backstop). Bounds churn/linkstate amplification
//!    from admit/close cycling. A peer already occupying a stranger slot
//!    re-admits without a rate token: its reconnect replaces its own face, so
//!    the occupancy axis already bounds it.
//!
//! ## Boot window
//!
//! The accept loop starts before the CRDT replay populates the resolver, so
//! early inbound connections from genuinely-known peers can classify as
//! strangers. The caps are sized so a realistic simultaneous-reconnect burst
//! fits the pool; anything shed retries via the peer's reconnect ladder
//! (ZEB-620) within seconds, by which point the resolver is loaded. Transient
//! and self-healing by design.
//!
//! Like the ZEB-711/757 shields, the rate window runs on this limiter's OWN
//! monotonic clock (never wall time), and unit tests drive `now_ms` logically.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::friend_intro::KeyedSlidingWindow;

/// Maximum live resolver-unknown zenoh faces. Sized comfortably above any
/// realistic join-window / boot-window burst of not-yet-known legitimate
/// peers, and far below the ZEB-912 router flood collapse region.
pub(crate) const MAX_INBOUND_ZENOH_STRANGER_FACES: usize = 16;

/// New stranger admissions allowed per rate window — bounds face churn (and
/// with it linkstate update amplification), not just standing occupancy.
/// Matches the tunnel path's 30-per-60s posture.
pub(crate) const ZENOH_STRANGER_ADMITS_PER_WINDOW: usize = 30;

/// Width of the stranger-admission rate window.
pub(crate) const ZENOH_STRANGER_RATE_WINDOW_MS: u64 = 60_000;

/// Shed-warning throttle: at most one `warn!` per this interval; sheds in
/// between are counted and reported in the next warning (per-connection warns
/// under an active flood would be their own log DoS).
const SHED_WARN_INTERVAL_MS: u64 = 30_000;

/// Admission verdict for one inbound stranger connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrangerVerdict {
    /// Proceed to the registry swap.
    Admit,
    /// Shed: the stranger pool is at capacity.
    ShedOccupancy,
    /// Shed: too many new stranger admissions in the current window.
    ShedRate,
}

struct ShedCounters {
    last_warn_ms: Option<u64>,
    occupancy_since_warn: u64,
    rate_since_warn: u64,
}

/// Stranger-face admission state. One per [`IrohZenohLinkManager`]; all
/// methods are cheap synchronous map/window ops (locks never held across
/// awaits).
///
/// [`IrohZenohLinkManager`]: crate::zenoh_iroh_transport::IrohZenohLinkManager
pub(crate) struct ZenohInboundAdmission {
    cap: usize,
    /// Live stranger-admitted faces: endpoint id → `stable_id` of the
    /// connection that currently OWNS the slot. Conn-id ownership (PR #752
    /// review, CodeRabbit + CodeAnt) closes two same-peer supersession holes
    /// a bare peer-set had: a same-zid re-admission racing the old conn's
    /// drop-watcher could have its slot freed out from under the live face
    /// (repeatable → cap overshoot), and a stranger superseded by an
    /// outbound/known connection leaked its slot for the replacement's
    /// lifetime. Re-admission transfers ownership to the new conn under the
    /// lock; `release` frees the slot only for the conn that owns it, so
    /// every drop-watcher releases unconditionally and stale watchers no-op.
    strangers: Mutex<HashMap<[u8; 32], usize>>,
    /// Global new-admission rate window (single unit key; see module docs).
    rate: Mutex<KeyedSlidingWindow<()>>,
    /// Monotonic base for the production `now_ms`.
    started: Instant,
    warn: Mutex<ShedCounters>,
}

impl ZenohInboundAdmission {
    pub(crate) fn new() -> Self {
        Self::with_caps(
            MAX_INBOUND_ZENOH_STRANGER_FACES,
            ZENOH_STRANGER_ADMITS_PER_WINDOW,
            ZENOH_STRANGER_RATE_WINDOW_MS,
        )
    }

    /// Deterministic tiny caps for tests (unit tests here; integration tests
    /// via `IrohZenohLinkManager`'s gated constructor).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub(crate) fn with_caps_for_test(cap: usize, rate_max: usize, rate_window_ms: u64) -> Self {
        Self::with_caps(cap, rate_max, rate_window_ms)
    }

    fn with_caps(cap: usize, rate_max: usize, rate_window_ms: u64) -> Self {
        Self {
            cap,
            strangers: Mutex::new(HashMap::new()),
            rate: Mutex::new(KeyedSlidingWindow::new(rate_max, rate_window_ms)),
            started: Instant::now(),
            warn: Mutex::new(ShedCounters {
                last_warn_ms: None,
                occupancy_since_warn: 0,
                rate_since_warn: 0,
            }),
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Admission check for a resolver-unknown inbound zenoh connection.
    /// Callers classify first: resolver-known peers must NOT be routed here.
    /// `conn_id` is the connection's `stable_id`; on admit it becomes (or
    /// takes over) the slot's owner.
    pub(crate) fn try_admit_stranger(&self, peer: &[u8; 32], conn_id: usize) -> StrangerVerdict {
        self.try_admit_stranger_at(peer, conn_id, self.now_ms())
    }

    /// Logical-time core (unit-testable without wall-clock waits).
    fn try_admit_stranger_at(
        &self,
        peer: &[u8; 32],
        conn_id: usize,
        now_ms: u64,
    ) -> StrangerVerdict {
        let mut strangers = self.strangers.lock().expect("strangers poisoned");
        // A peer already holding a slot is replacing its own face (same-zid
        // reconnect): admit without a rate token, occupancy unchanged — and
        // TRANSFER slot ownership to the new connection under this lock, so
        // the superseded conn's drop-watcher release no-ops instead of
        // freeing the slot out from under the live face.
        if let Some(owner) = strangers.get_mut(peer) {
            *owner = conn_id;
            return StrangerVerdict::Admit;
        }
        if strangers.len() >= self.cap {
            drop(strangers);
            self.note_shed(StrangerVerdict::ShedOccupancy, now_ms);
            return StrangerVerdict::ShedOccupancy;
        }
        // Rate-check only genuinely new admissions, and only after the
        // occupancy gate passed — a connection the pool would shed anyway
        // must not drain the rate budget honest retriers need (the ZEB-758
        // peek-then-commit lesson). `admit` records the token.
        if !self.rate.lock().expect("rate poisoned").admit((), now_ms) {
            drop(strangers);
            self.note_shed(StrangerVerdict::ShedRate, now_ms);
            return StrangerVerdict::ShedRate;
        }
        strangers.insert(*peer, conn_id);
        StrangerVerdict::Admit
    }

    /// Release `peer`'s stranger slot, but only if `conn_id` still OWNS it.
    /// Called unconditionally from every zenoh drop-watcher (regardless of
    /// the registry-eviction verdict): a no-op for resolver-known peers
    /// (never tracked), for stale watchers whose conn was superseded by a
    /// same-peer re-admission (ownership transferred), and it correctly
    /// frees the slot when a stranger's face is replaced by an
    /// outbound/known connection that never took admission ownership.
    pub(crate) fn release(&self, peer: &[u8; 32], conn_id: usize) {
        let mut strangers = self.strangers.lock().expect("strangers poisoned");
        if strangers.get(peer) == Some(&conn_id) {
            strangers.remove(peer);
        }
    }

    /// Throttled shed accounting: at most one warn per
    /// [`SHED_WARN_INTERVAL_MS`], carrying the counts accumulated since the
    /// previous warning.
    fn note_shed(&self, verdict: StrangerVerdict, now_ms: u64) {
        let mut warn = self.warn.lock().expect("warn poisoned");
        match verdict {
            StrangerVerdict::ShedOccupancy => warn.occupancy_since_warn += 1,
            StrangerVerdict::ShedRate => warn.rate_since_warn += 1,
            StrangerVerdict::Admit => {}
        }
        let due = match warn.last_warn_ms {
            None => true,
            Some(last) => now_ms.saturating_sub(last) >= SHED_WARN_INTERVAL_MS,
        };
        if due {
            tracing::warn!(
                occupancy_sheds = warn.occupancy_since_warn,
                rate_sheds = warn.rate_since_warn,
                cap = self.cap,
                "ZEB-996: shedding resolver-unknown inbound zenoh faces \
                 (counts since previous warning)"
            );
            warn.last_warn_ms = Some(now_ms);
            warn.occupancy_since_warn = 0;
            warn.rate_since_warn = 0;
        } else {
            tracing::debug!(?verdict, "ZEB-996: stranger zenoh face shed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn admits_up_to_cap_then_sheds_occupancy() {
        let a = ZenohInboundAdmission::with_caps(2, 100, 60_000);
        assert_eq!(
            a.try_admit_stranger_at(&peer(1), 10, 0),
            StrangerVerdict::Admit
        );
        assert_eq!(
            a.try_admit_stranger_at(&peer(2), 20, 1),
            StrangerVerdict::Admit
        );
        assert_eq!(
            a.try_admit_stranger_at(&peer(3), 30, 2),
            StrangerVerdict::ShedOccupancy
        );
    }

    #[test]
    fn release_frees_the_slot() {
        let a = ZenohInboundAdmission::with_caps(1, 100, 60_000);
        assert_eq!(
            a.try_admit_stranger_at(&peer(1), 10, 0),
            StrangerVerdict::Admit
        );
        assert_eq!(
            a.try_admit_stranger_at(&peer(2), 20, 1),
            StrangerVerdict::ShedOccupancy
        );
        a.release(&peer(1), 10);
        assert_eq!(
            a.try_admit_stranger_at(&peer(2), 20, 2),
            StrangerVerdict::Admit
        );
    }

    #[test]
    fn tracked_peer_readmits_at_full_pool_without_rate_token() {
        // cap 1, rate 1: peer 1 takes both the slot and the sole rate token.
        let a = ZenohInboundAdmission::with_caps(1, 1, 60_000);
        assert_eq!(
            a.try_admit_stranger_at(&peer(1), 10, 0),
            StrangerVerdict::Admit
        );
        // Same-zid reconnect: pool full with ITS OWN entry and the rate window
        // exhausted — still admitted, and no second token is recorded.
        assert_eq!(
            a.try_admit_stranger_at(&peer(1), 11, 1),
            StrangerVerdict::Admit
        );
        // A different stranger at the same instant sheds on occupancy.
        assert_eq!(
            a.try_admit_stranger_at(&peer(2), 20, 2),
            StrangerVerdict::ShedOccupancy
        );
        // Even after peer 1 leaves, the exhausted rate window (1/60s, token
        // recorded at t=0) sheds newcomers until it slides past.
        a.release(&peer(1), 11);
        assert_eq!(
            a.try_admit_stranger_at(&peer(2), 20, 3),
            StrangerVerdict::ShedRate
        );
        assert_eq!(
            a.try_admit_stranger_at(&peer(2), 20, 60_001),
            StrangerVerdict::Admit
        );
    }

    #[test]
    fn occupancy_shed_does_not_drain_rate_budget() {
        // cap 1, rate 1. Peer 1 occupies; peers 2..5 shed on OCCUPANCY, which
        // must not consume rate tokens (peek-then-commit ordering): after
        // peer 1 releases, advance past the single t=0 token's window and
        // verify a newcomer admits immediately (occupancy sheds recorded no
        // tokens; otherwise it would take 4 more windows).
        let a = ZenohInboundAdmission::with_caps(1, 1, 60_000);
        assert_eq!(
            a.try_admit_stranger_at(&peer(1), 10, 0),
            StrangerVerdict::Admit
        );
        for (i, b) in (2u8..=5).enumerate() {
            assert_eq!(
                a.try_admit_stranger_at(&peer(b), u64::from(b) as usize, 1 + i as u64),
                StrangerVerdict::ShedOccupancy
            );
        }
        a.release(&peer(1), 10);
        assert_eq!(
            a.try_admit_stranger_at(&peer(6), 60, 60_001),
            StrangerVerdict::Admit
        );
    }

    #[test]
    fn rate_window_sheds_fast_churn_and_recovers() {
        // Roomy pool, tight rate: 2 admissions per window.
        let a = ZenohInboundAdmission::with_caps(100, 2, 1_000);
        assert_eq!(
            a.try_admit_stranger_at(&peer(1), 10, 0),
            StrangerVerdict::Admit
        );
        assert_eq!(
            a.try_admit_stranger_at(&peer(2), 20, 1),
            StrangerVerdict::Admit
        );
        assert_eq!(
            a.try_admit_stranger_at(&peer(3), 30, 2),
            StrangerVerdict::ShedRate
        );
        assert_eq!(
            a.try_admit_stranger_at(&peer(3), 30, 1_001),
            StrangerVerdict::Admit
        );
    }

    #[test]
    fn release_of_untracked_peer_is_noop() {
        let a = ZenohInboundAdmission::with_caps(1, 100, 60_000);
        a.release(&peer(9), 90);
        assert_eq!(
            a.try_admit_stranger_at(&peer(1), 10, 0),
            StrangerVerdict::Admit
        );
    }

    /// PR #752 review regression (CodeRabbit Major / CodeAnt Major): a
    /// same-peer re-admission transfers slot ownership to the new conn, so
    /// the superseded conn's (possibly delayed) drop-watcher release must
    /// NOT free the live face's slot — while the owning conn's release must.
    #[test]
    fn readmission_transfers_ownership_and_stale_release_is_noop() {
        let a = ZenohInboundAdmission::with_caps(1, 100, 60_000);
        // conn 10 admits; conn 11 (same peer) re-admits and takes ownership.
        assert_eq!(
            a.try_admit_stranger_at(&peer(1), 10, 0),
            StrangerVerdict::Admit
        );
        assert_eq!(
            a.try_admit_stranger_at(&peer(1), 11, 1),
            StrangerVerdict::Admit
        );
        // The old conn's watcher fires late: must be a no-op.
        a.release(&peer(1), 10);
        assert_eq!(
            a.try_admit_stranger_at(&peer(2), 20, 2),
            StrangerVerdict::ShedOccupancy,
            "stale release must not free the live face's slot"
        );
        // The owning conn's release frees it.
        a.release(&peer(1), 11);
        assert_eq!(
            a.try_admit_stranger_at(&peer(2), 20, 3),
            StrangerVerdict::Admit
        );
    }
}
