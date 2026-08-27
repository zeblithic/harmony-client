//! peer_liveness.rs — ZEB-622: passive per-peer transport liveness.
//!
//! One state machine fuses the ZEB-616 registry's connect/drop edges (both
//! directions), iroh 1.0 path events (Direct vs Relay + per-path RTT), and
//! zenoh transport events into per-peer `Connected/Degraded/Disconnected`.
//! Consumers: the transport-epoch backfill re-arm (up-edges REPLACE the
//! accumulating seen-zid gate — a same-zid flap now re-arms), the rate-limited
//! network-health-changed pipeline (via `changed_rx`), and
//! `NetworkHealthService::snapshot`, which joins these transport states with
//! the reconnect supervisor's Retrying/Dormant for the fused PeerHealth view.
//! `Dormant` deliberately stays supervisor-owned (single source of truth).
//!
//! Every producer feeds one of the non-async `on_*` / `report_path` seams; the
//! handle is cheap to clone (shared `Arc`) and safe to call from sync contexts.
//! The one endpoint-touching item is the iroh path watcher
//! (`run_conn_path_watcher`, ZEB-622 Task 2): it owns a `Connection` clone,
//! translates iroh 1.0 path snapshots/events into `report_path` calls, and is
//! the sole reason this module imports iroh.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// Selected-path transport class for a live peer link. `Direct` is a
/// hole-punched/LAN path; `Relay` is relay-mediated. Serialized camelCase for
/// the Network Health panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LivenessMode {
    Direct,
    Relay,
}

/// Serializable telemetry projection of a peer's transport liveness for the
/// Network Health panel. Same `tag = "kind"` camelCase encoding as
/// [`crate::reconnect_supervisor::PeerStateWire`], so the two fuse into one
/// wire shape downstream.
///
/// - `Connected` — a selected path is up (with its mode and, when known, RTT).
/// - `Degraded` — the link is up but no selected path is known yet (e.g. an
///   up-edge before the first path report, or a lost-path report on the live
///   conn).
/// - `Disconnected` — the transport for this peer is gone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// `rename_all` camelCases the variant tags; `rename_all_fields` camelCases the
// struct-variant fields (`rttMs`, `sinceMs`) — the enum-level `rename_all` alone
// does not reach struct-variant fields (matches the repo idiom in pairing/types.rs).
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum LivenessStateWire {
    Connected {
        mode: LivenessMode,
        rtt_ms: Option<u32>,
        since_ms: u64,
    },
    Degraded {
        since_ms: u64,
    },
    Disconnected {
        since_ms: u64,
    },
}

/// Internal per-peer liveness state. Structurally mirrors [`LivenessStateWire`]
/// today but is kept distinct so future non-serializable bookkeeping (Task 2+)
/// can hang off it without perturbing the wire shape. `PartialEq` backs
/// `report_path`'s observable-change detection (ZEB-1002).
#[derive(Debug, Clone, PartialEq)]
enum SlotState {
    Connected {
        mode: LivenessMode,
        rtt_ms: Option<u32>,
        since_ms: u64,
    },
    Degraded {
        since_ms: u64,
    },
    Disconnected {
        since_ms: u64,
    },
}

impl SlotState {
    /// A state is "up" when the transport link exists — `Connected` (selected
    /// path) or `Degraded` (link up, path unknown). Only up states raise the
    /// transport epoch on entry and contribute to [`LivenessHandle::min_relay_rtt_ms`].
    fn is_up(&self) -> bool {
        matches!(
            self,
            SlotState::Connected { .. } | SlotState::Degraded { .. }
        )
    }

    fn to_wire(&self) -> LivenessStateWire {
        match self {
            SlotState::Connected {
                mode,
                rtt_ms,
                since_ms,
            } => LivenessStateWire::Connected {
                mode: *mode,
                rtt_ms: *rtt_ms,
                since_ms: *since_ms,
            },
            SlotState::Degraded { since_ms } => LivenessStateWire::Degraded {
                since_ms: *since_ms,
            },
            SlotState::Disconnected { since_ms } => LivenessStateWire::Disconnected {
                since_ms: *since_ms,
            },
        }
    }
}

/// Per-peer bookkeeping behind the handle's lock.
#[derive(Debug)]
struct PeerSlot {
    state: SlotState,
    /// Identity guard: only reports carrying this exact conn id apply, so a
    /// superseded connection's watcher can't clobber a fresher link.
    conn_id: Option<usize>,
    /// Min relay RTT over the CURRENT conn's relay paths (reset on any conn
    /// swap / down). Feeds [`LivenessHandle::min_relay_rtt_ms`].
    min_relay_rtt_ms: Option<u32>,
    /// Set once this peer has ever reached `Connected`. Read by later tasks
    /// (never-connected vs dropped telemetry); write-only in Task 1.
    #[allow(dead_code)]
    ever_connected: bool,
    /// Wall-clock ms when an rx application-frame delta was last observed for
    /// this peer (ZEB-804). Survives conn swaps and disconnects — it is
    /// evidence of past exchange, not connection state.
    last_traffic_ms: Option<u64>,
    /// Cumulative rx app-frame count at the last `report_traffic` sample for
    /// the CURRENT conn. Reset to `None` on any conn swap/down so a new
    /// connection's counters are never diffed against the old one's.
    app_frames_baseline: Option<u64>,
}

struct Inner {
    slots: Mutex<HashMap<[u8; 32], PeerSlot>>,
    /// Transport-epoch sink for the backfill re-arm. Installed once via
    /// [`LivenessHandle::set_transport_epoch_tx`]; bumped on every up-edge.
    epoch_tx: OnceLock<watch::Sender<u64>>,
    /// Monotone counter bumped on any slot change; drives the rate-limited
    /// network-health-changed pipeline.
    changed_tx: watch::Sender<u64>,
}

/// Producer-facing handle. Cheap to clone (shared `Arc`); every mutating method
/// is non-async and safe to call from sync contexts.
#[derive(Clone)]
pub struct LivenessHandle {
    inner: Arc<Inner>,
}

impl Default for LivenessHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// ZEB-804: per-peer liveness projection carrying the state AND the
/// state-independent traffic-evidence stamp.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerLivenessView {
    pub state: LivenessStateWire,
    pub last_traffic_ms: Option<u64>,
}

impl LivenessHandle {
    pub fn new() -> Self {
        let (changed_tx, _rx) = watch::channel(0u64);
        Self {
            inner: Arc::new(Inner {
                slots: Mutex::new(HashMap::new()),
                epoch_tx: OnceLock::new(),
                changed_tx,
            }),
        }
    }

    /// Install the transport-epoch sink. Install-once: a second call is ignored
    /// (the first sink wins), so wiring order between producers is irrelevant.
    pub fn set_transport_epoch_tx(&self, tx: watch::Sender<u64>) {
        let _ = self.inner.epoch_tx.set(tx);
    }

    /// Watch receiver bumped on any slot change (rate-limited downstream).
    pub fn changed_rx(&self) -> watch::Receiver<u64> {
        self.inner.changed_tx.subscribe()
    }

    /// Registry up-edge with a concrete `conn_id`. A re-report of the SAME conn
    /// is a no-op (not a fresh up-edge). Any other case (absent peer, a
    /// different conn id, or a previously-`Disconnected` slot) installs the new
    /// conn and drops to `Degraded` until the new conn's first path report — a
    /// superseding swap deliberately discards the old conn's `Connected`-ness.
    pub fn on_transport_up(&self, peer: [u8; 32], conn_id: usize) {
        let (changed, was_up, is_now_up) = {
            let mut slots = self.inner.slots.lock().expect("slots lock");
            match slots.get_mut(&peer) {
                Some(slot) if slot.conn_id == Some(conn_id) => (false, false, false),
                Some(slot) => {
                    let was_up = slot.state.is_up();
                    slot.conn_id = Some(conn_id);
                    slot.min_relay_rtt_ms = None;
                    slot.app_frames_baseline = None;
                    slot.state = SlotState::Degraded { since_ms: now_ms() };
                    (true, was_up, true)
                }
                None => {
                    slots.insert(
                        peer,
                        PeerSlot {
                            state: SlotState::Degraded { since_ms: now_ms() },
                            conn_id: Some(conn_id),
                            min_relay_rtt_ms: None,
                            ever_connected: false,
                            last_traffic_ms: None,
                            app_frames_baseline: None,
                        },
                    );
                    (true, false, true)
                }
            }
        };
        self.commit(changed, was_up, is_now_up);
    }

    /// Zenoh-view up-edge with no `Connection`. Acts only if the peer is absent
    /// or `Disconnected` — it installs a conn-less `Degraded` (a later registry
    /// up installs the real conn id) and never clobbers a conn-backed state.
    pub fn on_transport_up_external(&self, peer: [u8; 32]) {
        let (changed, was_up, is_now_up) = {
            let mut slots = self.inner.slots.lock().expect("slots lock");
            match slots.get_mut(&peer) {
                Some(slot) if matches!(slot.state, SlotState::Disconnected { .. }) => {
                    slot.state = SlotState::Degraded { since_ms: now_ms() };
                    slot.conn_id = None;
                    (true, false, true)
                }
                Some(_) => (false, false, false),
                None => {
                    slots.insert(
                        peer,
                        PeerSlot {
                            state: SlotState::Degraded { since_ms: now_ms() },
                            conn_id: None,
                            min_relay_rtt_ms: None,
                            ever_connected: false,
                            last_traffic_ms: None,
                            app_frames_baseline: None,
                        },
                    );
                    (true, false, true)
                }
            }
        };
        self.commit(changed, was_up, is_now_up);
    }

    /// Zenoh-view down-edge with no `Connection`. The mirror of
    /// [`Self::on_transport_up_external`]: a zenoh transport `Delete` is the only
    /// down-edge a conn-less (zenoh-only) slot will ever get, so honor it by
    /// dropping to `Disconnected`. Acts ONLY on a currently-up slot whose
    /// `conn_id.is_none()` — a registry-backed slot (`conn_id.is_some()`) is left
    /// untouched, since its `Disconnected` edge is owned by that conn's drop
    /// watcher. Clears `min_relay_rtt_ms` and fires the changed signal; it is NOT
    /// an up-edge, so no transport epoch bump.
    pub fn on_transport_down_external(&self, peer: [u8; 32]) {
        let (changed, was_up, is_now_up) = {
            let mut slots = self.inner.slots.lock().expect("slots lock");
            match slots.get_mut(&peer) {
                Some(slot) if slot.conn_id.is_none() && slot.state.is_up() => {
                    slot.state = SlotState::Disconnected { since_ms: now_ms() };
                    slot.min_relay_rtt_ms = None;
                    (true, true, false)
                }
                _ => (false, false, false),
            }
        };
        self.commit(changed, was_up, is_now_up);
    }

    /// Path event for `conn_id`'s connection. Ignored unless the slot's current
    /// conn matches (a superseded conn's watcher is silenced). A selected path
    /// promotes to `Connected` (preserving `since_ms` across a Connected→Connected
    /// path change); a lost path drops to `Degraded` (preserving `since_ms`
    /// across a Degraded→Degraded refresh — ZEB-1002: the 30s RTT tick must not
    /// make a continuously-degraded link look newly degraded). `min_relay_rtt_ms`
    /// is always refreshed from the report.
    ///
    /// The changed watch fires only on an OBSERVABLE change — the slot's wire
    /// projection or its `min_relay_rtt_ms` — so an identical refresh doesn't
    /// drip `network-health-changed` recomputes downstream (ZEB-1002; the 2s
    /// rate limiter bounds burst rate, not steady-state waste).
    pub fn report_path(
        &self,
        peer: [u8; 32],
        conn_id: usize,
        selected: Option<(LivenessMode, u32)>,
        min_relay_rtt_ms: Option<u32>,
    ) {
        let (changed, was_up, is_now_up) = {
            let mut slots = self.inner.slots.lock().expect("slots lock");
            match slots.get_mut(&peer) {
                Some(slot) if slot.conn_id == Some(conn_id) => {
                    let was_up = slot.state.is_up();
                    let new_state = match selected {
                        Some((mode, rtt)) => {
                            let since_ms = match &slot.state {
                                SlotState::Connected { since_ms, .. } => *since_ms,
                                _ => now_ms(),
                            };
                            SlotState::Connected {
                                mode,
                                rtt_ms: Some(rtt),
                                since_ms,
                            }
                        }
                        None => {
                            let since_ms = match &slot.state {
                                SlotState::Degraded { since_ms } => *since_ms,
                                _ => now_ms(),
                            };
                            SlotState::Degraded { since_ms }
                        }
                    };
                    let observable =
                        new_state != slot.state || slot.min_relay_rtt_ms != min_relay_rtt_ms;
                    if matches!(new_state, SlotState::Connected { .. }) {
                        slot.ever_connected = true;
                    }
                    slot.state = new_state;
                    slot.min_relay_rtt_ms = min_relay_rtt_ms;
                    (observable, was_up, slot.state.is_up())
                }
                _ => (false, false, false),
            }
        };
        self.commit(changed, was_up, is_now_up);
    }

    /// ZEB-804: cumulative rx app-frame sample for `conn_id`'s connection.
    /// Conn-guarded like `report_path`. First sample for a conn baselines without
    /// stamping (handshake-era frames must not masquerade as exchange evidence);
    /// a later sample with a positive delta stamps `last_traffic_ms` and bumps the
    /// changed watch.
    pub fn report_traffic(&self, peer: [u8; 32], conn_id: usize, cumulative_app_frames: u64) {
        let changed = {
            let mut slots = self.inner.slots.lock().expect("slots lock");
            match slots.get_mut(&peer) {
                Some(slot) if slot.conn_id == Some(conn_id) => match slot.app_frames_baseline {
                    None => {
                        slot.app_frames_baseline = Some(cumulative_app_frames);
                        false
                    }
                    Some(base) if cumulative_app_frames > base => {
                        slot.app_frames_baseline = Some(cumulative_app_frames);
                        slot.last_traffic_ms = Some(now_ms());
                        true
                    }
                    Some(_) => false,
                },
                _ => false,
            }
        };
        if changed {
            self.inner.changed_tx.send_modify(|e| *e += 1);
        }
    }

    /// Registry down-edge for `conn_id`. Applied only if the slot's conn still
    /// matches — a stale down from a superseded conn is ignored so it can't kill
    /// a freshly re-established link.
    pub fn on_transport_down(&self, peer: [u8; 32], conn_id: usize) {
        let (changed, was_up, is_now_up) = {
            let mut slots = self.inner.slots.lock().expect("slots lock");
            match slots.get_mut(&peer) {
                Some(slot) if slot.conn_id == Some(conn_id) => {
                    let was_up = slot.state.is_up();
                    slot.state = SlotState::Disconnected { since_ms: now_ms() };
                    slot.conn_id = None;
                    slot.min_relay_rtt_ms = None;
                    slot.app_frames_baseline = None;
                    (true, was_up, false)
                }
                _ => (false, false, false),
            }
        };
        self.commit(changed, was_up, is_now_up);
    }

    /// Telemetry snapshot of every known peer's transport-liveness state.
    pub fn states_snapshot(&self) -> Vec<([u8; 32], LivenessStateWire)> {
        let slots = self.inner.slots.lock().expect("slots lock");
        slots
            .iter()
            .map(|(p, slot)| (*p, slot.state.to_wire()))
            .collect()
    }

    /// ZEB-910: targeted single-peer read of the traffic-evidence stamp. The
    /// gateway coverage numerator queries per node-id on every driver pass;
    /// a full [`views_snapshot`](Self::views_snapshot) per lookup would
    /// allocate the whole view for one entry.
    pub fn last_traffic_ms(&self, peer: &[u8; 32]) -> Option<u64> {
        let slots = self.inner.slots.lock().expect("slots lock");
        slots.get(peer).and_then(|slot| slot.last_traffic_ms)
    }

    /// Telemetry snapshot of every known peer's liveness view: transport state
    /// plus the state-independent traffic-evidence stamp.
    pub fn views_snapshot(&self) -> Vec<([u8; 32], PeerLivenessView)> {
        let slots = self.inner.slots.lock().expect("slots lock");
        slots
            .iter()
            .map(|(p, slot)| {
                (
                    *p,
                    PeerLivenessView {
                        state: slot.state.to_wire(),
                        last_traffic_ms: slot.last_traffic_ms,
                    },
                )
            })
            .collect()
    }

    /// Minimum relay RTT across all currently-up peers (`Connected`/`Degraded`),
    /// or `None` if none report one. A disconnected peer's relay RTT drops out.
    pub fn min_relay_rtt_ms(&self) -> Option<u32> {
        let slots = self.inner.slots.lock().expect("slots lock");
        slots
            .values()
            .filter(|s| s.state.is_up())
            .filter_map(|s| s.min_relay_rtt_ms)
            .min()
    }

    /// Post-mutation fan-out: bump the transport epoch on an up-edge
    /// (`!was_up && is_now_up`) and the changed watch on any slot change.
    /// Called after the slots lock is released.
    fn commit(&self, changed: bool, was_up: bool, is_now_up: bool) {
        if !changed {
            return;
        }
        if !was_up && is_now_up {
            if let Some(tx) = self.inner.epoch_tx.get() {
                tx.send_modify(|e| *e += 1);
            }
        }
        self.inner.changed_tx.send_modify(|e| *e += 1);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Refresh cadence for RTT while a connection is quiet (path events fire on
/// open/close/selection change, not on RTT drift).
const RTT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Received application frames (STREAM + DATAGRAM) on a connection — the
/// ZEB-804 traffic-evidence basis. RX-only and frames-only, both load-bearing:
/// byte counters advance on QUIC keepalives/ACKs, and tx frame counters advance
/// on retransmissions into a blackholed path — either would let a dead-or-mute
/// peer read fresh forever, the exact lie this exists to fix.
pub(crate) fn app_frame_count(stats: &iroh::endpoint::ConnectionStats) -> u64 {
    stats.frame_rx.stream + stats.frame_rx.datagram
}

/// Watch one connection's paths and feed `handle`. Owns a `Connection` clone:
/// `Path<'_>` borrows the connection and cannot cross tasks, so each event
/// re-reads `conn.paths()`. Exits when the event stream ends (conn closed) —
/// the registry drop watcher owns the Disconnected edge.
///
/// (`n0_future::StreamExt` — iroh's own stream-ext — is a re-export of
/// `futures_lite::StreamExt`; iroh's `PathEventStream` is a plain
/// `futures_core::Stream`, so the crate-local `futures::StreamExt` drives
/// `.next()` identically without pulling `n0_future` in as a direct dep.)
pub async fn run_conn_path_watcher(
    handle: LivenessHandle,
    peer: [u8; 32],
    conn: iroh::endpoint::Connection,
) {
    use futures::StreamExt;
    let conn_id = conn.stable_id();
    let report = |h: &LivenessHandle| {
        let paths = conn.paths();
        let selected = paths.iter().find(|p| p.is_selected()).map(|p| {
            let mode = if p.is_relay() {
                LivenessMode::Relay
            } else {
                LivenessMode::Direct
            };
            (mode, p.rtt().as_millis().min(u32::MAX as u128) as u32)
        });
        let min_relay = paths
            .iter()
            .filter(|p| p.is_relay())
            .map(|p| p.rtt().as_millis().min(u32::MAX as u128) as u32)
            .min();
        h.report_path(peer, conn_id, selected, min_relay);
    };
    report(&handle);
    let mut events = conn.path_events();
    let mut tick = tokio::time::interval(RTT_REFRESH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = events.next() => match ev {
                Some(_) => report(&handle),   // any event → re-read snapshot (incl. Lagged)
                None => break,
            },
            _ = tick.tick() => {
                report(&handle);
                handle.report_traffic(peer, conn_id, app_frame_count(&conn.stats()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> [u8; 32] {
        [n; 32]
    }

    #[tokio::test(start_paused = true)]
    async fn up_edge_bumps_epoch_and_changed() {
        let h = LivenessHandle::new();
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        h.set_transport_epoch_tx(tx);
        let mut changed = h.changed_rx();
        let before_change = *changed.borrow_and_update();
        h.on_transport_up(peer(1), 11);
        assert_eq!(*rx.borrow(), 1, "up-edge bumps transport epoch");
        assert!(
            *changed.borrow_and_update() > before_change,
            "changed watch bumped"
        );
        let snap = h.states_snapshot();
        assert!(
            matches!(snap.as_slice(), [(p, LivenessStateWire::Degraded { .. })] if *p == peer(1)),
            "up-edge without a path report is Degraded (link up, path unknown)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn duplicate_up_same_conn_does_not_double_bump() {
        let h = LivenessHandle::new();
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        h.set_transport_epoch_tx(tx);
        h.on_transport_up(peer(1), 11);
        h.on_transport_up(peer(1), 11);
        assert_eq!(*rx.borrow(), 1, "same conn re-report is not a new up-edge");
    }

    #[tokio::test(start_paused = true)]
    async fn path_report_promotes_to_connected_and_stale_conn_ignored() {
        let h = LivenessHandle::new();
        h.on_transport_up(peer(1), 11);
        h.report_path(peer(1), 11, Some((LivenessMode::Direct, 12)), None);
        assert!(matches!(
            h.states_snapshot().as_slice(),
            [(
                _,
                LivenessStateWire::Connected {
                    mode: LivenessMode::Direct,
                    rtt_ms: Some(12),
                    ..
                }
            )]
        ));
        // A superseded connection's watcher must not clobber the fresh state.
        h.report_path(peer(1), 10, Some((LivenessMode::Relay, 99)), None);
        assert!(
            matches!(
                h.states_snapshot().as_slice(),
                [(
                    _,
                    LivenessStateWire::Connected {
                        mode: LivenessMode::Direct,
                        ..
                    }
                )]
            ),
            "stale conn_id report ignored"
        );
        // Selected path lost on the CURRENT conn → Degraded.
        h.report_path(peer(1), 11, None, None);
        assert!(matches!(
            h.states_snapshot().as_slice(),
            [(_, LivenessStateWire::Degraded { .. })]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn down_edge_and_same_zid_flap_re_bumps_epoch() {
        let h = LivenessHandle::new();
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        h.set_transport_epoch_tx(tx);
        h.on_transport_up(peer(1), 11);
        h.on_transport_down(peer(1), 11);
        assert!(matches!(
            h.states_snapshot().as_slice(),
            [(_, LivenessStateWire::Disconnected { .. })]
        ));
        assert_eq!(*rx.borrow(), 1, "down is not an up-edge");
        // SAME peer reconnects (new conn id) — the exact case the seen-zid gate missed.
        h.on_transport_up(peer(1), 12);
        assert_eq!(*rx.borrow(), 2, "same-peer flap re-bumps the epoch");
        // Stale down from the OLD conn must not kill the fresh link.
        h.on_transport_down(peer(1), 11);
        assert!(
            matches!(
                h.states_snapshot().as_slice(),
                [(_, LivenessStateWire::Degraded { .. })]
            ),
            "superseded conn's down-edge ignored"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn external_up_only_acts_when_disconnected_or_absent() {
        let h = LivenessHandle::new();
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        h.set_transport_epoch_tx(tx);
        h.on_transport_up_external(peer(1)); // absent → Degraded + bump
        assert_eq!(*rx.borrow(), 1);
        h.on_transport_up(peer(2), 22);
        h.report_path(peer(2), 22, Some((LivenessMode::Relay, 40)), Some(40));
        h.on_transport_up_external(peer(2)); // conn-backed Connected → no-op
        assert_eq!(*rx.borrow(), 2, "external up on a live peer is a no-op");
        assert!(h.states_snapshot().iter().any(|(p, s)| *p == peer(2)
            && matches!(
                s,
                LivenessStateWire::Connected {
                    mode: LivenessMode::Relay,
                    ..
                }
            )));
    }

    #[tokio::test(start_paused = true)]
    async fn external_down_edge_disconnects_and_flap_re_arms() {
        let h = LivenessHandle::new();
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        h.set_transport_epoch_tx(tx);
        // External up (zenoh-only, conn-less) → Degraded + epoch 1.
        h.on_transport_up_external(peer(1));
        assert_eq!(*rx.borrow(), 1, "external up bumps epoch");
        // External down → Disconnected, but NOT an up-edge (epoch unchanged).
        h.on_transport_down_external(peer(1));
        assert!(
            matches!(
                h.states_snapshot().as_slice(),
                [(_, LivenessStateWire::Disconnected { .. })]
            ),
            "external down drops the conn-less slot to Disconnected"
        );
        assert_eq!(*rx.borrow(), 1, "external down is not an up-edge");
        // External up AGAIN — the zenoh-only flap re-arms (epoch 2). Before the
        // down-edge existed this stuck Degraded forever and future Puts were
        // no-ops.
        h.on_transport_up_external(peer(1));
        assert_eq!(*rx.borrow(), 2, "post-down external up re-bumps the epoch");
        assert!(matches!(
            h.states_snapshot().as_slice(),
            [(_, LivenessStateWire::Degraded { .. })]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn external_down_does_not_touch_registry_backed_slot() {
        let h = LivenessHandle::new();
        // Registry-backed up-edge (carries a conn_id) → Degraded, conn-backed.
        h.on_transport_up(peer(1), 11);
        h.report_path(peer(1), 11, Some((LivenessMode::Direct, 12)), None);
        // A stray external down MUST NOT clobber a conn-backed slot — its
        // Disconnected edge is owned by the registry conn's drop watcher.
        h.on_transport_down_external(peer(1));
        assert!(
            matches!(
                h.states_snapshot().as_slice(),
                [(
                    _,
                    LivenessStateWire::Connected {
                        mode: LivenessMode::Direct,
                        ..
                    }
                )]
            ),
            "external down leaves a registry-backed slot unchanged"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn degraded_since_ms_stable_across_repeated_lost_path_reports() {
        let h = LivenessHandle::new();
        h.on_transport_up(peer(1), 11);
        h.report_path(peer(1), 11, Some((LivenessMode::Direct, 12)), None);
        // Lost path → Degraded, stamped now.
        h.report_path(peer(1), 11, None, None);
        let since1 = match h.states_snapshot().as_slice() {
            [(_, LivenessStateWire::Degraded { since_ms })] => *since_ms,
            other => panic!("expected Degraded, got {other:?}"),
        };
        let mut changed = h.changed_rx();
        let before = *changed.borrow_and_update();
        // `now_ms()` is SystemTime-based (tokio's paused clock doesn't apply),
        // so a real-clock sleep is what makes a ZEB-1002 reset observable.
        std::thread::sleep(std::time::Duration::from_millis(15));
        // The 30s RTT-refresh tick re-reports the same lost-path state.
        h.report_path(peer(1), 11, None, None);
        let since2 = match h.states_snapshot().as_slice() {
            [(_, LivenessStateWire::Degraded { since_ms })] => *since_ms,
            other => panic!("expected Degraded, got {other:?}"),
        };
        assert_eq!(
            since1, since2,
            "a refresh of an already-Degraded slot must not reset since_ms \
             (degraded-duration telemetry would be meaningless)"
        );
        assert_eq!(
            *changed.borrow_and_update(),
            before,
            "an unchanged Degraded refresh is not an observable change"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unchanged_path_refresh_does_not_bump_changed() {
        let h = LivenessHandle::new();
        h.on_transport_up(peer(1), 11);
        h.report_path(peer(1), 11, Some((LivenessMode::Direct, 12)), Some(40));
        let mut changed = h.changed_rx();
        let before = *changed.borrow_and_update();
        // Identical 30s-tick refresh: same selected path, same rtt, same
        // min relay → nothing observable changed → no changed bump (the
        // downstream network-health notify pipeline stays quiet).
        h.report_path(peer(1), 11, Some((LivenessMode::Direct, 12)), Some(40));
        assert_eq!(
            *changed.borrow_and_update(),
            before,
            "identical path refresh must not bump the changed watch"
        );
        // A real RTT drift is observable (wire rttMs) → signals.
        h.report_path(peer(1), 11, Some((LivenessMode::Direct, 13)), Some(40));
        let after_rtt = *changed.borrow_and_update();
        assert!(after_rtt > before, "rtt change is observable → signals");
        // A min-relay-only change is observable via min_relay_rtt_ms() → signals.
        h.report_path(peer(1), 11, Some((LivenessMode::Direct, 13)), Some(35));
        assert!(
            *changed.borrow_and_update() > after_rtt,
            "min-relay-rtt change is observable → signals"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn min_relay_rtt_across_peers() {
        let h = LivenessHandle::new();
        h.on_transport_up(peer(1), 11);
        h.report_path(peer(1), 11, Some((LivenessMode::Direct, 5)), Some(80));
        h.on_transport_up(peer(2), 22);
        h.report_path(peer(2), 22, Some((LivenessMode::Relay, 60)), Some(60));
        assert_eq!(h.min_relay_rtt_ms(), Some(60));
        h.on_transport_down(peer(2), 22);
        assert_eq!(
            h.min_relay_rtt_ms(),
            Some(80),
            "disconnected peer's relay rtt drops out"
        );
    }

    #[test]
    fn app_frame_count_ignores_keepalives_and_tx() {
        use iroh::endpoint::ConnectionStats;
        let mut stats = ConnectionStats::default();
        stats.frame_rx.ping = 500;
        stats.frame_rx.acks = 900;
        stats.udp_rx.bytes = 1_000_000;
        stats.frame_tx.stream = 700; // retransmission-into-blackhole counterfeit
        assert_eq!(
            app_frame_count(&stats),
            0,
            "keepalives/acks/bytes/tx must not count"
        );
        stats.frame_rx.stream = 3;
        stats.frame_rx.datagram = 2;
        assert_eq!(
            app_frame_count(&stats),
            5,
            "rx stream+datagram frames count"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn traffic_first_sample_baselines_without_stamp() {
        let h = LivenessHandle::new();
        h.on_transport_up(peer(1), 11);
        h.report_traffic(peer(1), 11, 40); // handshake-era frames: baseline only
        let views = h.views_snapshot();
        assert!(
            matches!(views.as_slice(), [(p, v)] if *p == peer(1) && v.last_traffic_ms.is_none()),
            "first sample must baseline, never stamp"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn traffic_delta_stamps_and_zero_delta_does_not() {
        let h = LivenessHandle::new();
        h.on_transport_up(peer(1), 11);
        // The changed watch is the deterministic observer: `now_ms()` is
        // wall-clock, so an equal-timestamp assertion alone could pass against
        // a buggy `>=` re-stamp within one millisecond.
        let mut rx = h.changed_rx();
        rx.borrow_and_update();
        h.report_traffic(peer(1), 11, 40); // first sample: baseline only
        assert!(
            !rx.has_changed().expect("changed watch alive"),
            "a baselining first sample must not bump the changed watch"
        );
        h.report_traffic(peer(1), 11, 41); // delta > 0 → stamp
        assert!(
            rx.has_changed().expect("changed watch alive"),
            "a positive rx app-frame delta bumps the changed watch"
        );
        rx.borrow_and_update();
        let stamped = h.views_snapshot()[0].1.last_traffic_ms;
        assert!(
            stamped.is_some(),
            "rx app-frame delta stamps last_traffic_ms"
        );
        h.report_traffic(peer(1), 11, 41); // delta == 0 → no change
        assert!(
            !rx.has_changed().expect("changed watch alive"),
            "a zero-delta re-report must not bump the changed watch"
        );
        assert_eq!(h.views_snapshot()[0].1.last_traffic_ms, stamped);
    }

    #[tokio::test(start_paused = true)]
    async fn traffic_stale_conn_ignored_and_baseline_resets_on_swap() {
        let h = LivenessHandle::new();
        h.on_transport_up(peer(1), 11);
        h.report_traffic(peer(1), 11, 40);
        h.report_traffic(peer(1), 10, 90); // superseded conn → ignored entirely
        assert!(h.views_snapshot()[0].1.last_traffic_ms.is_none());
        h.on_transport_up(peer(1), 12); // conn swap resets the baseline
        h.report_traffic(peer(1), 12, 7); // NEW conn's first sample: baseline, no stamp
        assert!(
            h.views_snapshot()[0].1.last_traffic_ms.is_none(),
            "a fresh conn's first cumulative sample must baseline, not diff against the old conn"
        );
        h.report_traffic(peer(1), 12, 8);
        assert!(h.views_snapshot()[0].1.last_traffic_ms.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn connected_transition_does_not_stamp_traffic() {
        // Regression pin for the ZEB-804 defect: the deleted
        // `last_connected_ms = Some(now_ms())` establishment stamp lived in
        // `report_path`'s Connected arm. Re-adding a stamp beside
        // `ever_connected = true` must fail this test.
        let h = LivenessHandle::new();
        h.on_transport_up(peer(1), 11);
        h.report_path(peer(1), 11, Some((LivenessMode::Direct, 10)), None);
        assert!(
            h.views_snapshot()[0].1.last_traffic_ms.is_none(),
            "reaching Connected is not traffic evidence"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn traffic_stamp_survives_degraded_and_disconnect() {
        let h = LivenessHandle::new();
        h.on_transport_up(peer(1), 11);
        h.report_traffic(peer(1), 11, 1);
        h.report_traffic(peer(1), 11, 2);
        let stamped = h.views_snapshot()[0].1.last_traffic_ms;
        assert!(stamped.is_some());
        h.report_path(peer(1), 11, None, None); // Connected→Degraded
        assert_eq!(h.views_snapshot()[0].1.last_traffic_ms, stamped);
        h.on_transport_down(peer(1), 11); // evidence of past exchange persists
        assert_eq!(h.views_snapshot()[0].1.last_traffic_ms, stamped);
    }

    #[test]
    fn liveness_state_wire_serde_pin() {
        let v = serde_json::to_value(LivenessStateWire::Connected {
            mode: LivenessMode::Direct,
            rtt_ms: Some(12),
            since_ms: 5,
        })
        .expect("serialize");
        assert_eq!(
            v,
            serde_json::json!({"kind":"connected","mode":"direct","rttMs":12,"sinceMs":5})
        );
    }
}
