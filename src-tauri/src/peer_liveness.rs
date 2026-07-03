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
/// can hang off it without perturbing the wire shape.
#[derive(Debug, Clone)]
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
    /// Wall-clock ms of the last `Connected` transition. Read by later tasks
    /// (last-seen telemetry); write-only in Task 1.
    #[allow(dead_code)]
    last_connected_ms: Option<u64>,
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
                            last_connected_ms: None,
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
                            last_connected_ms: None,
                        },
                    );
                    (true, false, true)
                }
            }
        };
        self.commit(changed, was_up, is_now_up);
    }

    /// Path event for `conn_id`'s connection. Ignored unless the slot's current
    /// conn matches (a superseded conn's watcher is silenced). A selected path
    /// promotes to `Connected` (preserving `since_ms` across a Connected→Connected
    /// path change); a lost path drops to `Degraded`. `min_relay_rtt_ms` is
    /// always refreshed from the report.
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
                    match selected {
                        Some((mode, rtt)) => {
                            let since_ms = match &slot.state {
                                SlotState::Connected { since_ms, .. } => *since_ms,
                                _ => now_ms(),
                            };
                            slot.state = SlotState::Connected {
                                mode,
                                rtt_ms: Some(rtt),
                                since_ms,
                            };
                            slot.ever_connected = true;
                            slot.last_connected_ms = Some(now_ms());
                        }
                        None => {
                            slot.state = SlotState::Degraded { since_ms: now_ms() };
                        }
                    }
                    slot.min_relay_rtt_ms = min_relay_rtt_ms;
                    (true, was_up, slot.state.is_up())
                }
                _ => (false, false, false),
            }
        };
        self.commit(changed, was_up, is_now_up);
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
            _ = tick.tick() => report(&handle),
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
