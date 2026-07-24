//! ZEB-473 (DM-over-iroh, Move 1a): inbound PQ tunnel acceptor.
//!
//! Implements [`IrohHandshakeDispatcher`] for the `harmony/tunnel/v1` ALPN. The
//! accept loop in `zenoh_iroh_transport` routes a negotiated tunnel connection
//! here; we spawn the responder driver, which completes the PQ handshake,
//! registers the live session into the [`TunnelManager`] (so our outbound DMs to
//! this peer reuse the bidirectional tunnel), and feeds inbound `FrameTag::Dm`
//! payloads onto the ingest seam.
//!
//! ZEB-757: the inbound path is the client's live, remote-triggered tunnel-
//! population surface — any peer that completes a valid PQ handshake gets a
//! session registered ([`TunnelManager::register_inbound`], inside the
//! `harmony_tunnel_iroh` crate), with **no contact allowlist**. The ZEB-739
//! un-fork left this acceptor without the lifetime-held admission semaphore the
//! node's accept loop has, so inbound population was unbounded (fd/memory/CPU
//! exhaustion under a flood). [`IrohTunnelAcceptor`] now claims one
//! [`InboundAdmission`] permit BEFORE any PQ crypto work and holds it for the
//! tunnel's whole lifetime, strictly bounding the live inbound population. A
//! rejected connection does not drop the DM: transport is hybrid (always-deposit
//! plus best-effort tunnel), so the sender's copy still lands via the deposit
//! rung and a later attempt succeeds once a slot frees.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use harmony_identity::PqPrivateIdentity;
use iroh::endpoint::Connection;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use crate::iroh_invite_acceptor::IrohHandshakeDispatcher;
use crate::tunnel_manager::{InboundDm, TunnelManager};

/// ZEB-757: strict cap on the number of concurrently-live INBOUND PQ tunnel
/// sessions this node will accept. Each admitted inbound connection holds one
/// [`InboundAdmission`] permit for the tunnel's whole lifetime, so this bounds the
/// session-map / spawned-responder population an inbound peer set can grow.
/// Distinct from `MAX_CONCURRENT_TUNNEL_SENDS` (outbound send-task concurrency,
/// permit released per-send) — do NOT conflate them. Sized to match the node's
/// `MAX_TUNNEL_CONNECTIONS` (harmony#295); idle tunnels are reaped, so 64
/// concurrent live inbound tunnels is generous, and overflow degrades to the
/// deposit rung rather than dropping DMs.
const MAX_INBOUND_TUNNEL_SESSIONS: usize = 64;

/// ZEB-757: emit at most one inbound-saturation warning per this many ms.
/// Rejecting is the CHEAP path an inbound flood drives, so a warning per
/// rejected connection would hand that flood an unbounded log-write amplifier —
/// exactly the cost the cap exists to shed. Throttling keeps the operator signal
/// (saturation started, and how heavily) while bounding log volume.
const REJECT_WARN_INTERVAL_MS: u64 = 30_000;

/// Sentinel for [`InboundAdmission::last_warn_ms`] meaning "no saturation
/// warning emitted yet", so the FIRST rejection always logs immediately rather
/// than waiting out a window.
const WARN_NEVER: u64 = u64::MAX;

/// ZEB-757: admission control for the live inbound tunnel population. Wraps a
/// [`Semaphore`] sized to the population cap; each admitted inbound tunnel keeps
/// one owned permit for its whole lifetime, so a `None` from [`try_admit`] means
/// the population is saturated and the connection must be rejected. Split out as
/// its own type so the bound is unit-testable without a real iroh endpoint.
///
/// Also owns the saturation-log throttle (see [`REJECT_WARN_INTERVAL_MS`]).
///
/// [`try_admit`]: InboundAdmission::try_admit
struct InboundAdmission {
    sem: Arc<Semaphore>,
    /// Monotonic base for [`now_ms`](InboundAdmission::now_ms). The throttle
    /// runs on this limiter's OWN monotonic clock, never wall time — a wall-clock
    /// step would distort the window (same convention as the ZEB-711 shields).
    started: Instant,
    /// Monotonic ms at which the last saturation warning was emitted, or
    /// [`WARN_NEVER`] if none has been.
    last_warn_ms: AtomicU64,
    /// Rejections accumulated since the last emitted warning.
    rejected_since_warn: AtomicU64,
}

impl InboundAdmission {
    fn new(cap: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(cap)),
            started: Instant::now(),
            last_warn_ms: AtomicU64::new(WARN_NEVER),
            rejected_since_warn: AtomicU64::new(0),
        }
    }

    /// Claim one inbound-population slot. `Some(permit)` admits the connection —
    /// the caller MUST hold the permit for the tunnel's whole lifetime (move it
    /// into the responder task) so the slot frees only when the tunnel dies.
    /// `None` means the live inbound population is at the cap.
    fn try_admit(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.sem).try_acquire_owned().ok()
    }

    /// Milliseconds since this admission control was built (monotonic).
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Record one rejected inbound connection and decide whether it should log.
    ///
    /// Returns `Some(n)` when the caller should emit ONE warning covering `n`
    /// rejections — the first rejection always logs (so operators see saturation
    /// begin), and afterwards at most one per [`REJECT_WARN_INTERVAL_MS`].
    /// Returns `None` when this rejection is instead folded into the count the
    /// next warning will carry. `now_ms` is a parameter so the window is testable
    /// without sleeping.
    fn note_rejection(&self, now_ms: u64) -> Option<u64> {
        self.rejected_since_warn.fetch_add(1, Ordering::Relaxed);
        let last = self.last_warn_ms.load(Ordering::Acquire);
        let due = last == WARN_NEVER || now_ms.saturating_sub(last) >= REJECT_WARN_INTERVAL_MS;
        if !due {
            return None;
        }
        // Concurrent rejections can all see the window as due; only the one that
        // installs its timestamp logs, the rest fold into the next window.
        if self
            .last_warn_ms
            .compare_exchange(last, now_ms, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        Some(self.rejected_since_warn.swap(0, Ordering::Relaxed))
    }
}

/// Inbound `harmony/tunnel/v1` acceptor. Holds the handles the responder driver
/// needs: our own PQ private identity (the responder self-input), the manager to
/// register the live session into, and the ingest channel for received DMs — plus
/// the ZEB-757 admission control that bounds the live inbound population.
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
            admission: InboundAdmission::new(MAX_INBOUND_TUNNEL_SESSIONS),
        }
    }
}

#[async_trait]
impl IrohHandshakeDispatcher for IrohTunnelAcceptor {
    async fn handle_connection(&self, conn: Connection) {
        // ZEB-757: claim an inbound-population slot BEFORE spawning the responder.
        // The QUIC connection is already established by the shared accept loop, but
        // rejecting here still sheds the expensive per-tunnel work — the PQ
        // handshake, the session registration, and the long-lived responder task —
        // so a flood costs only a QUIC accept + close. The owned permit is moved
        // into the responder task and held for the tunnel's whole lifetime, so the
        // live inbound population never exceeds MAX_INBOUND_TUNNEL_SESSIONS.
        let permit = match self.admission.try_admit() {
            Some(permit) => permit,
            None => {
                // Throttled: a flood drives this path, so it must not become a
                // log-write amplifier (the warning carries how many rejections
                // it covers, so nothing is silently lost).
                if let Some(rejected) = self.admission.note_rejection(self.admission.now_ms()) {
                    tracing::warn!(
                        cap = MAX_INBOUND_TUNNEL_SESSIONS,
                        rejected,
                        window_ms = REJECT_WARN_INTERVAL_MS,
                        "inbound tunnel population cap reached — rejecting connections \
                         (DMs still deliver via the deposit rung)"
                    );
                }
                conn.close(0u32.into(), b"tunnel-population-cap");
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
            // `run_tunnel_responder` returns frees the inbound-population slot.
            let _permit = permit;
            crate::tunnel_task::run_tunnel_responder(conn, local_pq, mgr, ingest_tx).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ZEB-757: the admission control strictly bounds the live inbound population —
    // it admits up to `cap` concurrent permits, rejects at the cap, and frees a
    // slot only when a held permit is dropped (i.e. when a tunnel dies). This is
    // the load-bearing guarantee that a `handle_connection` flood cannot grow the
    // session-map / responder-task population past the cap.
    #[test]
    fn inbound_admission_is_strictly_bounded_at_cap() {
        let admission = InboundAdmission::new(2);

        let p1 = admission.try_admit().expect("1st inbound admitted");
        let p2 = admission.try_admit().expect("2nd inbound admitted");
        assert!(
            admission.try_admit().is_none(),
            "3rd inbound rejected while 2 permits are held"
        );

        // A permit drop models a tunnel dying — its slot must free.
        drop(p1);
        let p3 = admission
            .try_admit()
            .expect("slot frees after a held permit is dropped");

        // Still strictly capped: with p2 + p3 live, the next admission is rejected.
        assert!(
            admission.try_admit().is_none(),
            "still capped at 2 concurrently-live permits"
        );

        drop(p2);
        drop(p3);
    }

    // ZEB-757 (Qodo): the saturation warning is throttled so an inbound flood
    // cannot turn the cheap reject path into an unbounded log-write amplifier.
    // The first rejection logs immediately (operators see saturation begin);
    // afterwards at most one warning per window, carrying the count it covers so
    // suppressed rejections are reported, never silently dropped.
    #[test]
    fn reject_warning_is_throttled_and_reports_suppressed_count() {
        let admission = InboundAdmission::new(1);

        assert_eq!(
            admission.note_rejection(0),
            Some(1),
            "first rejection logs immediately"
        );
        assert_eq!(
            admission.note_rejection(1_000),
            None,
            "inside the window — folded into the next warning"
        );
        assert_eq!(
            admission.note_rejection(REJECT_WARN_INTERVAL_MS - 1),
            None,
            "still inside the window"
        );
        assert_eq!(
            admission.note_rejection(REJECT_WARN_INTERVAL_MS),
            Some(3),
            "window elapsed — one warning covering the 3 rejections since the last"
        );
        assert_eq!(
            admission.note_rejection(REJECT_WARN_INTERVAL_MS + 1),
            None,
            "the new window starts closed again"
        );
    }
}
