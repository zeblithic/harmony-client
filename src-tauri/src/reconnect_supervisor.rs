//! ZEB-620: reconnect supervisor core — the pure-logic engine that decides
//! *when* and *whether* to (re)dial each peer.
//!
//! This is the successor to the dial-once `iroh_dial_driver` (ZEB-373): where
//! that driver dials a newly-learned peer a fixed number of times and then gives
//! up for the session, the supervisor maintains a **per-peer state machine**
//! (`Connected` / `Retrying` / `Dormant`) driven by a jittered exponential
//! backoff ladder, so a peer whose dial fails or whose transport later drops is
//! reconnected indefinitely (with bounded, jittered cadence) until it either
//! connects or falls dormant for lack of fresh interest.
//!
//! Design (binding, per the Task 2 brief):
//! - **Single supervisor loop, no per-peer tasks.** One [`run_reconnect_supervisor`]
//!   future owns all scheduling; the only spawned tasks are the individual
//!   in-flight dials, bounded by a `max_concurrent_dials` semaphore.
//! - **Coalescing dirty set.** Producers (resolver, drop-watchers, registry)
//!   call the lossless, non-async [`SupervisorHandle::kick`], which inserts into
//!   a `HashMap<[u8;32], ReconnectTrigger>` (strongest trigger wins on merge)
//!   and notifies. 1000 kicks for one peer before the loop drains collapse to a
//!   single scheduled dial.
//! - **Lower-NodeId dial-role gate (ZEB-485, generalized).** The peer with the
//!   lexicographically lower NodeId dials immediately; the higher one waits
//!   `higher_id_fallback_delay` first, so a mutually-learned pair does not dial
//!   into each other simultaneously. See [`dial_role`].
//! - **Record-gated dialing.** At dial time the supervisor consults the
//!   [`ReachabilityResolver`] (record *freshness*): a peer with no live routing
//!   record is not dialed — the attempt is a soft failure that still advances
//!   the ladder, so an evicted peer eventually falls dormant rather than
//!   dialing into the void.
//!
//! This module intentionally wires *no* producers (resolver notify, drop
//! watchers, registry eviction) — later ZEB-620 tasks connect those to the
//! `kick*` / `mark_connected` seams exposed here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::time::Instant;

use crate::iroh_dial_driver::PeerDialer;
use crate::network_health::DialTelemetry;
use crate::reachability_resolver::ReachabilityResolver;

/// Fractional jitter added to every scheduled delay: the realized delay is
/// `nominal × (1.0 + rand[0, JITTER_FRACTION])`, i.e. jitter only ever *extends*
/// a delay (never shortens it below the nominal rung), spreading a herd of
/// simultaneous re-arms without dialing more eagerly than the ladder intends.
const JITTER_FRACTION: f64 = 0.1;

/// Why a peer needs attention. Kicks are idempotent — duplicates are harmless
/// (the dirty set coalesces them). On merge the *strongest* trigger wins:
/// `Dropped` > `NewPeer` = `RecordChanged` > `PresenceSweep`. The ordering only
/// changes behavior for an already-`Connected` peer: `Dropped` re-arms it,
/// `NewPeer`/`RecordChanged`/`PresenceSweep` merely refresh its interest stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectTrigger {
    /// Resolver first-learn of a peer (today's `DialHint`).
    NewPeer,
    /// Resolver LWW-replaced an existing record with new relay/addresses.
    RecordChanged,
    /// Registry eviction or a zenoh transport `Delete` — the transport is gone.
    Dropped,
    /// Identity-free roster edge: re-arm ALL known non-connected peers.
    PresenceSweep,
}

impl ReconnectTrigger {
    /// Merge precedence for the coalescing dirty set. `Dropped` is strictly
    /// highest so that a `Connected` peer which receives both a `Dropped` and a
    /// weaker trigger in the same drain window is re-armed (the refinement the
    /// brief's `Dropped/NewPeer/RecordChanged > PresenceSweep` grouping leaves
    /// to the implementer — a lost `Dropped` would strand a genuinely-gone
    /// transport).
    fn strength(self) -> u8 {
        match self {
            ReconnectTrigger::Dropped => 3,
            ReconnectTrigger::NewPeer | ReconnectTrigger::RecordChanged => 2,
            ReconnectTrigger::PresenceSweep => 1,
        }
    }
}

/// Per-peer state, loop-owned. Snapshotted to [`PeerStateWire`] for telemetry.
#[derive(Debug, Clone)]
pub enum PeerState {
    /// Transport is up (inbound accept or a successful dial).
    Connected { since_ms: u64 },
    /// Between dial attempts; `next_at` is the paused-clock instant of the next
    /// dial and `attempt` indexes the ladder rung.
    Retrying { attempt: u32, next_at: Instant },
    /// Given up scheduling after `dormant_after` of no fresh interest; stays in
    /// the map so a later kick can revive it at the base rung.
    Dormant { since_ms: u64 },
}

impl PeerState {
    fn to_wire(&self, now: Instant) -> PeerStateWire {
        match self {
            PeerState::Connected { since_ms } => PeerStateWire::Connected {
                since_ms: *since_ms,
            },
            PeerState::Dormant { since_ms } => PeerStateWire::Dormant {
                since_ms: *since_ms,
            },
            PeerState::Retrying { attempt, next_at } => PeerStateWire::Retrying {
                attempt: *attempt,
                retry_in_ms: next_at.saturating_duration_since(now).as_millis() as u64,
            },
        }
    }
}

/// Serializable telemetry projection of [`PeerState`] for the Network Health
/// panel. `retry_in_ms` is snapshot-relative (millis until the next dial), so it
/// carries no `Instant`/`SystemTime` coupling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PeerStateWire {
    Connected { since_ms: u64 },
    Retrying { attempt: u32, retry_in_ms: u64 },
    Dormant { since_ms: u64 },
}

/// Result of the lower-NodeId dial-role gate (ZEB-485, generalized).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialRole {
    /// This node has the lower NodeId: dial immediately (at the base rung).
    Dialer,
    /// This node has the higher NodeId: wait `higher_id_fallback_delay` before
    /// the first attempt, giving the lower-id peer a chance to connect first.
    DelayedDialer,
}

/// Tuning for [`run_reconnect_supervisor`]. Tests pass small durations and
/// `jitter_seed: Some(_)` for determinism; production uses [`Default`].
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// First ladder rung (attempt 0) delay before jitter.
    pub retry_base: Duration,
    /// Ceiling for the exponential rung (`min(base·2^attempt, cap)`).
    pub retry_cap: Duration,
    /// Elapsed-since-last-fresh-trigger after which a failing peer goes dormant.
    pub dormant_after: Duration,
    /// Minimum spacing between presence sweeps; a sweep within the window is
    /// deferred to fire once the window lapses.
    pub presence_sweep_cooldown: Duration,
    /// Upper bound on concurrently in-flight dials.
    pub max_concurrent_dials: usize,
    /// First-attempt delay for a [`DialRole::DelayedDialer`] (higher NodeId).
    pub higher_id_fallback_delay: Duration,
    /// `Some(seed)` makes jitter deterministic (tests); `None` seeds from entropy.
    pub jitter_seed: Option<u64>,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            retry_base: Duration::from_secs(2),
            retry_cap: Duration::from_secs(60),
            dormant_after: Duration::from_secs(15 * 60),
            presence_sweep_cooldown: Duration::from_secs(30),
            max_concurrent_dials: 8,
            higher_id_fallback_delay: Duration::from_secs(3),
            jitter_seed: None,
        }
    }
}

/// Loop-owned per-peer bookkeeping. `epoch` invalidates the result of an
/// in-flight dial whose peer was re-armed or marked connected while the dial was
/// outstanding (a stale result must not clobber the fresher schedule/state).
struct PeerSlot {
    state: PeerState,
    last_fresh_trigger: Instant,
    dial_in_flight: bool,
    epoch: u64,
}

/// Message from a spawned dial task back to the supervisor loop.
struct DialResult {
    peer: [u8; 32],
    epoch: u64,
    ok: bool,
}

/// Shared inner state behind [`SupervisorHandle`] — cloned (via `Arc`) to every
/// producer and to the loop.
struct SupervisorInner {
    /// Coalescing dirty set: strongest trigger per peer wins on merge.
    dirty: Mutex<HashMap<[u8; 32], ReconnectTrigger>>,
    /// Authoritative per-peer state (loop-mutated; read for telemetry snapshots
    /// and written by `mark_connected`).
    states: Mutex<HashMap<[u8; 32], PeerSlot>>,
    /// Set by `kick_sweep`; the loop drains it and applies cooldown gating.
    sweep_requested: AtomicBool,
    /// Wakes the loop on any kick / sweep / connect.
    notify: Notify,
}

/// Producer-facing handle. Cheap to clone (shared `Arc`); every method is
/// non-async and safe to call from sync contexts (drop-watchers, the resolver).
#[derive(Clone)]
pub struct SupervisorHandle {
    inner: Arc<SupervisorInner>,
}

impl Default for SupervisorHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl SupervisorHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                dirty: Mutex::new(HashMap::new()),
                states: Mutex::new(HashMap::new()),
                sweep_requested: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    /// Lossless, non-async: insert `trigger` for `peer` into the dirty set
    /// (strongest wins) and wake the loop. Never blocks, never drops.
    pub fn kick(&self, peer: [u8; 32], trigger: ReconnectTrigger) {
        {
            let mut dirty = self.inner.dirty.lock().expect("dirty lock");
            dirty
                .entry(peer)
                .and_modify(|cur| {
                    if trigger.strength() > cur.strength() {
                        *cur = trigger;
                    }
                })
                .or_insert(trigger);
        }
        self.inner.notify.notify_one();
    }

    /// Request a presence sweep (re-arm all known non-connected peers). Subject
    /// to `presence_sweep_cooldown` gating inside the loop.
    pub fn kick_sweep(&self) {
        self.inner.sweep_requested.store(true, Ordering::Release);
        self.inner.notify.notify_one();
    }

    /// Mark `peer` connected (inbound accept or dial success). Cancels further
    /// dialing until the peer is `Dropped`; bumps the slot epoch so any
    /// in-flight dial's result is discarded rather than overriding `Connected`.
    pub fn mark_connected(&self, peer: [u8; 32]) {
        {
            let mut states = self.inner.states.lock().expect("states lock");
            let slot = states.entry(peer).or_insert_with(|| PeerSlot {
                state: PeerState::Connected { since_ms: 0 },
                last_fresh_trigger: Instant::now(),
                dial_in_flight: false,
                epoch: 0,
            });
            slot.epoch = slot.epoch.wrapping_add(1);
            slot.state = PeerState::Connected { since_ms: now_ms() };
        }
        self.inner.notify.notify_one();
    }

    /// Telemetry snapshot of every known peer's current state.
    pub fn states_snapshot(&self) -> Vec<([u8; 32], PeerStateWire)> {
        let now = Instant::now();
        let states = self.inner.states.lock().expect("states lock");
        states
            .iter()
            .map(|(peer, slot)| (*peer, slot.state.to_wire(now)))
            .collect()
    }
}

/// ZEB-485 gate, generalized: the lexicographically lower NodeId dials
/// immediately ([`DialRole::Dialer`]); the higher one waits
/// `higher_id_fallback_delay` first ([`DialRole::DelayedDialer`]). A self-vs-self
/// comparison (guarded elsewhere) yields `DelayedDialer`. Pure — unit-tested
/// directly.
pub fn dial_role(self_id: &[u8; 32], peer_id: &[u8; 32]) -> DialRole {
    if self_id < peer_id {
        DialRole::Dialer
    } else {
        DialRole::DelayedDialer
    }
}

/// Nominal (pre-jitter) delay for the given ladder rung and role. Attempt 0 for a
/// [`DialRole::DelayedDialer`] uses `higher_id_fallback_delay` instead of the
/// base rung; every other rung is the capped exponential `min(base·2^n, cap)`.
fn nominal_delay(attempt: u32, role: DialRole, config: &SupervisorConfig) -> Duration {
    if attempt == 0 && role == DialRole::DelayedDialer {
        return config.higher_id_fallback_delay;
    }
    let mult = 2u32.saturating_pow(attempt.min(20));
    config.retry_base.saturating_mul(mult).min(config.retry_cap)
}

/// Realized delay = nominal extended by up to [`JITTER_FRACTION`].
fn schedule_delay(
    attempt: u32,
    role: DialRole,
    config: &SupervisorConfig,
    rng: &mut ChaCha8Rng,
) -> Duration {
    let nominal = nominal_delay(attempt, role, config);
    let frac: f64 = rng.gen_range(0.0..=JITTER_FRACTION);
    nominal + nominal.mul_f64(frac)
}

/// Deterministic-locator form used for every dial (`iroh/<hex(node_id)>`); the
/// iroh transport resolves relay/addrs from the NodeId via its own discovery,
/// so the locator does not embed the record's relay — the record's role here is
/// purely freshness gating. Mirrors `iroh_dial_driver::iroh_locator` (kept
/// local so this task touches no other source file).
fn iroh_locator(node_id: &[u8; 32]) -> String {
    format!("iroh/{}", hex::encode(node_id))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run the reconnect supervisor until dropped. Shares `handle` with producers;
/// dials through `dialer`, gates freshness through `resolver`, records outcomes
/// to `telemetry`, and never dials `self_node_id`.
pub async fn run_reconnect_supervisor(
    handle: SupervisorHandle,
    dialer: Arc<dyn PeerDialer>,
    resolver: Arc<ReachabilityResolver>,
    telemetry: Arc<DialTelemetry>,
    self_node_id: [u8; 32],
    config: SupervisorConfig,
) {
    let inner = handle.inner.clone();
    let mut rng = ChaCha8Rng::seed_from_u64(config.jitter_seed.unwrap_or_else(rand::random));
    let sem = Arc::new(Semaphore::new(config.max_concurrent_dials.max(1)));
    let (res_tx, mut res_rx) = mpsc::unbounded_channel::<DialResult>();

    // Sweep bookkeeping (loop-local): the instant of the last performed sweep and
    // the deadline of a sweep deferred because it arrived within the cooldown.
    let mut last_sweep: Option<Instant> = None;
    let mut pending_sweep_at: Option<Instant> = None;

    loop {
        let now = Instant::now();

        // --- presence-sweep gating -----------------------------------------
        if inner.sweep_requested.swap(false, Ordering::AcqRel) {
            match last_sweep {
                Some(ts) if now.duration_since(ts) < config.presence_sweep_cooldown => {
                    // Within cooldown: defer to the moment it lapses (mirrors the
                    // backfill kick's deferred re-arm).
                    pending_sweep_at = Some(ts + config.presence_sweep_cooldown);
                }
                _ => {
                    do_sweep(&inner, now, &self_node_id, &config, &mut rng);
                    last_sweep = Some(now);
                    pending_sweep_at = None;
                }
            }
        }
        if let Some(due) = pending_sweep_at {
            if now >= due {
                do_sweep(&inner, now, &self_node_id, &config, &mut rng);
                last_sweep = Some(now);
                pending_sweep_at = None;
            }
        }

        // --- drain the coalescing dirty set --------------------------------
        let drained: Vec<([u8; 32], ReconnectTrigger)> = {
            let mut dirty = inner.dirty.lock().expect("dirty lock");
            dirty.drain().collect()
        };

        // --- apply triggers + dispatch due dials (single states-locked pass) -
        let next_deadline = {
            let mut states = inner.states.lock().expect("states lock");

            for (peer, trigger) in drained {
                if peer == self_node_id {
                    continue; // self-dial guard
                }
                apply_trigger(
                    &mut states,
                    peer,
                    trigger,
                    now,
                    &self_node_id,
                    &config,
                    &mut rng,
                );
            }

            for (peer, slot) in states.iter_mut() {
                if slot.dial_in_flight {
                    continue;
                }
                let due =
                    matches!(slot.state, PeerState::Retrying { next_at, .. } if next_at <= now);
                if !due {
                    continue;
                }
                match resolver.resolve_by_node_id(peer) {
                    Some((owner, _payload)) => {
                        slot.epoch = slot.epoch.wrapping_add(1);
                        slot.dial_in_flight = true;
                        let epoch = slot.epoch;
                        let peer = *peer;
                        let owner = owner.0;
                        let sem = sem.clone();
                        let dialer = dialer.clone();
                        let telemetry = telemetry.clone();
                        let res_tx = res_tx.clone();
                        tokio::spawn(async move {
                            let _permit = match sem.acquire_owned().await {
                                Ok(p) => p,
                                Err(_) => return,
                            };
                            telemetry.record_attempt();
                            let ok = dialer.dial(peer, iroh_locator(&peer)).await;
                            if ok {
                                telemetry.record_succeeded(peer, owner);
                            } else {
                                telemetry.record_failed(peer, owner);
                            }
                            let _ = res_tx.send(DialResult { peer, epoch, ok });
                        });
                    }
                    None => {
                        // No live routing record: soft-fail (no dial, no telemetry)
                        // and advance the ladder, so an evicted peer eventually
                        // falls dormant instead of dialing into the void.
                        ladder_after_failure(slot, now, &self_node_id, peer, &config, &mut rng);
                    }
                }
            }

            earliest_deadline(&states, now)
        };

        let deadline = min_opt(next_deadline, pending_sweep_at);

        tokio::select! {
            biased;
            Some(result) = res_rx.recv() => {
                apply_result(&inner, result, &self_node_id, &config, &mut rng);
            }
            _ = inner.notify.notified() => {}
            _ = sleep_until_opt(deadline) => {}
        }
    }
}

/// Apply a per-peer trigger to the state map. An unknown peer is armed at the
/// base rung. For a known peer: `Dropped` (from any state) and any trigger on a
/// non-`Connected` peer re-arm at base; `NewPeer`/`RecordChanged`/`PresenceSweep`
/// on a `Connected` peer only refresh the interest stamp (no dial).
fn apply_trigger(
    states: &mut HashMap<[u8; 32], PeerSlot>,
    peer: [u8; 32],
    trigger: ReconnectTrigger,
    now: Instant,
    self_node_id: &[u8; 32],
    config: &SupervisorConfig,
    rng: &mut ChaCha8Rng,
) {
    let role = dial_role(self_node_id, &peer);
    match states.get_mut(&peer) {
        None => {
            let delay = schedule_delay(0, role, config, rng);
            states.insert(
                peer,
                PeerSlot {
                    state: PeerState::Retrying {
                        attempt: 0,
                        next_at: now + delay,
                    },
                    last_fresh_trigger: now,
                    dial_in_flight: false,
                    epoch: 0,
                },
            );
        }
        Some(slot) => {
            let connected = matches!(slot.state, PeerState::Connected { .. });
            let record_only = connected && !matches!(trigger, ReconnectTrigger::Dropped);
            if record_only {
                slot.last_fresh_trigger = now;
            } else {
                // Re-arm at base. Bump the epoch so any in-flight dial's result is
                // discarded (it belongs to the pre-re-arm schedule).
                slot.epoch = slot.epoch.wrapping_add(1);
                slot.last_fresh_trigger = now;
                let delay = schedule_delay(0, role, config, rng);
                slot.state = PeerState::Retrying {
                    attempt: 0,
                    next_at: now + delay,
                };
            }
        }
    }
}

/// Re-arm every known non-connected peer at the base rung (identity-free roster
/// edge). Connected peers are left untouched.
fn do_sweep(
    inner: &SupervisorInner,
    now: Instant,
    self_node_id: &[u8; 32],
    config: &SupervisorConfig,
    rng: &mut ChaCha8Rng,
) {
    let mut states = inner.states.lock().expect("states lock");
    for (peer, slot) in states.iter_mut() {
        if peer == self_node_id || matches!(slot.state, PeerState::Connected { .. }) {
            continue;
        }
        let role = dial_role(self_node_id, peer);
        slot.epoch = slot.epoch.wrapping_add(1);
        slot.last_fresh_trigger = now;
        let delay = schedule_delay(0, role, config, rng);
        slot.state = PeerState::Retrying {
            attempt: 0,
            next_at: now + delay,
        };
    }
}

/// Advance a peer one ladder rung after a failed (or record-less) attempt, or
/// transition it to `Dormant` once `dormant_after` has elapsed since the last
/// fresh trigger.
fn ladder_after_failure(
    slot: &mut PeerSlot,
    now: Instant,
    self_node_id: &[u8; 32],
    peer: &[u8; 32],
    config: &SupervisorConfig,
    rng: &mut ChaCha8Rng,
) {
    slot.dial_in_flight = false;
    if now.duration_since(slot.last_fresh_trigger) > config.dormant_after {
        slot.state = PeerState::Dormant { since_ms: now_ms() };
        return;
    }
    let attempt = match slot.state {
        PeerState::Retrying { attempt, .. } => attempt + 1,
        _ => 1,
    };
    let role = dial_role(self_node_id, peer);
    let delay = schedule_delay(attempt, role, config, rng);
    slot.state = PeerState::Retrying {
        attempt,
        next_at: now + delay,
    };
}

/// Handle a completed dial. A stale result (peer re-armed or marked connected
/// while the dial was outstanding — epoch mismatch) only clears the in-flight
/// flag; a current success connects the peer, a current failure ladders it.
fn apply_result(
    inner: &SupervisorInner,
    result: DialResult,
    self_node_id: &[u8; 32],
    config: &SupervisorConfig,
    rng: &mut ChaCha8Rng,
) {
    let mut states = inner.states.lock().expect("states lock");
    let slot = match states.get_mut(&result.peer) {
        Some(s) => s,
        None => return,
    };
    slot.dial_in_flight = false;
    if slot.epoch != result.epoch {
        return; // superseded — the fresher schedule/state already stands
    }
    if result.ok {
        slot.state = PeerState::Connected { since_ms: now_ms() };
    } else {
        ladder_after_failure(
            slot,
            Instant::now(),
            self_node_id,
            &result.peer,
            config,
            rng,
        );
    }
}

/// Earliest `next_at` among peers eligible to dial (Retrying, not in flight).
fn earliest_deadline(states: &HashMap<[u8; 32], PeerSlot>, _now: Instant) -> Option<Instant> {
    states
        .values()
        .filter_map(|slot| match slot.state {
            PeerState::Retrying { next_at, .. } if !slot.dial_in_flight => Some(next_at),
            _ => None,
        })
        .min()
}

fn min_opt(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) => Some(x),
        (None, y) => y,
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use crate::reachability_record::ReachabilityAnnouncePayload;
    use std::sync::atomic::AtomicUsize;

    // ---- helpers ---------------------------------------------------------

    fn peer(n: u8) -> [u8; 32] {
        [n; 32]
    }

    /// Seed a live routing record so the record-gated supervisor will dial `p`.
    fn seed(resolver: &ReachabilityResolver, p: [u8; 32]) {
        resolver.update(
            OwnerAddr([0xAA; 16]),
            ReachabilityAnnouncePayload {
                iroh_node_id: p,
                home_relay_url: String::new(),
                direct_addresses: vec![],
                announced_at_ms: 1,
                identity_signature: [0u8; 64],
                butler_set: vec![],
                bs_at: 0,
            },
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: String::new(),
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn cfg(
        base_ms: u64,
        cap_ms: u64,
        dormant_ms: u64,
        cooldown_ms: u64,
        max_dials: usize,
        fallback_ms: u64,
    ) -> SupervisorConfig {
        SupervisorConfig {
            retry_base: Duration::from_millis(base_ms),
            retry_cap: Duration::from_millis(cap_ms),
            dormant_after: Duration::from_millis(dormant_ms),
            presence_sweep_cooldown: Duration::from_millis(cooldown_ms),
            max_concurrent_dials: max_dials,
            higher_id_fallback_delay: Duration::from_millis(fallback_ms),
            jitter_seed: Some(0xC0FFEE),
        }
    }

    enum DialBehavior {
        Fail,
        Succeed,
        Park,
    }

    /// Flexible test dialer: records the (time, node_id) of every `dial()` entry
    /// and tracks concurrent/peak in-flight count. `Park` mode blocks each dial
    /// on a gate until `release()`, to observe the concurrency bound.
    struct RecordingDialer {
        behavior: DialBehavior,
        calls: Mutex<Vec<(Instant, [u8; 32])>>,
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        gate: Semaphore,
    }

    impl RecordingDialer {
        fn new(behavior: DialBehavior) -> Arc<Self> {
            Arc::new(Self {
                behavior,
                calls: Mutex::new(Vec::new()),
                in_flight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                gate: Semaphore::new(0),
            })
        }
        fn failing() -> Arc<Self> {
            Self::new(DialBehavior::Fail)
        }
        fn succeeding() -> Arc<Self> {
            Self::new(DialBehavior::Succeed)
        }
        fn parking() -> Arc<Self> {
            Self::new(DialBehavior::Park)
        }
        fn release(&self) {
            self.gate.add_permits(10_000);
        }
        fn times_for(&self, p: [u8; 32]) -> Vec<Instant> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, n)| *n == p)
                .map(|(t, _)| *t)
                .collect()
        }
        fn count_for(&self, p: [u8; 32]) -> usize {
            self.times_for(p).len()
        }
    }

    #[async_trait::async_trait]
    impl PeerDialer for RecordingDialer {
        async fn dial(&self, node_id: [u8; 32], _locator: String) -> bool {
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            self.calls.lock().unwrap().push((Instant::now(), node_id));
            let ok = match self.behavior {
                DialBehavior::Fail => false,
                DialBehavior::Succeed => true,
                DialBehavior::Park => {
                    let p = self.gate.acquire().await.expect("gate");
                    p.forget();
                    false
                }
            };
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            ok
        }
    }

    fn assert_between(actual: Duration, lo: Duration, hi: Duration, label: &str) {
        assert!(
            actual >= lo && actual <= hi,
            "{label}: {actual:?} not in [{lo:?}, {hi:?}]"
        );
    }

    /// Jitter only ever *extends* a delay by up to `JITTER_FRACTION`, so the
    /// observed gap for a nominal rung lies in `[nominal, nominal·1.1]`. We add a
    /// small slack for scheduler dispatch latency (≈0 under a paused clock).
    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }
    fn rung_lo(nominal_ms: u64) -> Duration {
        ms(nominal_ms)
    }
    fn rung_hi(nominal_ms: u64) -> Duration {
        ms((nominal_ms as f64 * (1.0 + JITTER_FRACTION)) as u64 + 60)
    }

    // ---- tests -----------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn ladder_escalates_and_caps() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        // self = 0 < p = 1 -> Dialer, so the first dial is at the base rung.
        let self_id = peer(0);
        let config = cfg(1_000, 16_000, 3_600_000, 30_000, 8, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            self_id,
            config,
        ));

        let start = Instant::now();
        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(200_000)).await;

        let times = dialer.times_for(p);
        assert!(times.len() >= 6, "expected >=6 dials, got {}", times.len());
        // Gap before dial0 measured from the kick; subsequent gaps between dials.
        assert_between(
            times[0] - start,
            rung_lo(1_000),
            rung_hi(1_000),
            "rung0 (base)",
        );
        assert_between(times[1] - times[0], rung_lo(2_000), rung_hi(2_000), "rung1");
        assert_between(times[2] - times[1], rung_lo(4_000), rung_hi(4_000), "rung2");
        assert_between(times[3] - times[2], rung_lo(8_000), rung_hi(8_000), "rung3");
        assert_between(
            times[4] - times[3],
            rung_lo(16_000),
            rung_hi(16_000),
            "rung4 (cap)",
        );
        assert_between(
            times[5] - times[4],
            rung_lo(16_000),
            rung_hi(16_000),
            "rung5 (capped)",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn trigger_rearms_from_base() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        let config = cfg(1_000, 256_000, 3_600_000, 30_000, 8, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p, ReconnectTrigger::NewPeer);
        // Let the peer climb deep into the ladder (next dial ~255s out).
        tokio::time::sleep(ms(150_000)).await;
        let before = dialer.count_for(p);
        // Nothing should dial in the quiescent gap just before the kick.
        tokio::time::sleep(ms(5_000)).await;
        assert_eq!(dialer.count_for(p), before, "quiescent before Dropped kick");

        let kick_at = Instant::now();
        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(ms(2_000)).await;

        let times = dialer.times_for(p);
        let after = *times.last().unwrap();
        assert!(times.len() > before, "Dropped kick should produce a dial");
        assert_between(
            after - kick_at,
            rung_lo(1_000),
            rung_hi(1_000),
            "re-armed dial at base",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dormancy_after_15min_and_revival() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        // dormant_after = 10s: ladder dials at ~1,3,7,15s then goes dormant.
        let config = cfg(1_000, 64_000, 10_000, 30_000, 8, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(100_000)).await;
        let dormant_count = dialer.count_for(p);
        assert!(
            (3..=4).contains(&dormant_count),
            "expected ~4 pre-dormant dials, got {dormant_count}"
        );
        // Long quiescent window: no dials once dormant.
        tokio::time::sleep(ms(100_000)).await;
        assert_eq!(dialer.count_for(p), dormant_count, "no dials while dormant");

        // A fresh kick revives at the base rung.
        let revive_at = Instant::now();
        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(2_000)).await;
        let times = dialer.times_for(p);
        assert_eq!(times.len(), dormant_count + 1, "revival produces one dial");
        assert_between(
            *times.last().unwrap() - revive_at,
            rung_lo(1_000),
            rung_hi(1_000),
            "revived dial at base",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn presence_sweep_cooldown_gates_and_defers() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        // dormant_after (500ms) < base (1s): each re-arm produces exactly one
        // dial, then the peer falls dormant — no ladder churn between sweeps.
        let config = cfg(1_000, 64_000, 500, 10_000, 8, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));
        let start = Instant::now();

        // Arm the peer so it becomes known, dial once (~1s), then dormant.
        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(5_000)).await;
        assert_eq!(dialer.count_for(p), 1, "one dial then dormant");

        // First sweep (no prior) fires immediately -> dial at ~6s.
        handle.kick_sweep();
        tokio::time::sleep(ms(3_000)).await; // now ~8s
        assert_eq!(dialer.count_for(p), 2, "immediate sweep re-arm");

        // Second sweep within the 10s cooldown of the first (@~5s): deferred.
        handle.kick_sweep();
        tokio::time::sleep(ms(2_000)).await; // now ~10s
        assert_eq!(dialer.count_for(p), 2, "second sweep deferred, no dial yet");

        // Cooldown lapses (~15s from first sweep) -> deferred sweep fires -> dial ~16s.
        tokio::time::sleep(ms(10_000)).await; // now ~20s
        assert_eq!(
            dialer.count_for(p),
            3,
            "deferred sweep fired at cooldown lapse"
        );

        let times = dialer.times_for(p);
        assert_between(
            times[0] - start,
            rung_lo(1_000),
            rung_hi(1_000),
            "initial dial",
        );
        // Deferred sweep dial lands well after the immediate one (cooldown gap).
        assert!(
            times[2] - times[1] >= ms(9_000),
            "deferred sweep dial spaced by cooldown: {:?}",
            times[2] - times[1]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dial_role_gate() {
        // Pure-fn cases: lower id dials, higher id delays.
        assert_eq!(dial_role(&peer(0), &peer(1)), DialRole::Dialer);
        assert_eq!(dial_role(&peer(9), &peer(1)), DialRole::DelayedDialer);

        // Integration: with self in the middle, a lower peer is a DelayedDialer
        // (first dial after the fallback delay) and a higher peer is a Dialer
        // (first dial at base).
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let self_id = peer(0x80);
        let low = peer(0x01); // self > low  -> DelayedDialer
        let high = peer(0xFF); // self < high -> Dialer
        seed(&resolver, low);
        seed(&resolver, high);
        let handle = SupervisorHandle::new();
        // base 1s, fallback 5s.
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 8, 5_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            self_id,
            config,
        ));
        let start = Instant::now();
        handle.kick(low, ReconnectTrigger::NewPeer);
        handle.kick(high, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(8_000)).await;

        let high_first = *dialer.times_for(high).first().unwrap();
        let low_first = *dialer.times_for(low).first().unwrap();
        assert_between(
            high_first - start,
            rung_lo(1_000),
            rung_hi(1_000),
            "Dialer (high peer) at base",
        );
        assert_between(
            low_first - start,
            rung_lo(5_000),
            rung_hi(5_000),
            "DelayedDialer (low peer) after fallback delay",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_dials_bounded() {
        let dialer = RecordingDialer::parking();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let handle = SupervisorHandle::new();
        let max = 3;
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, max, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        for n in 1..=10u8 {
            let p = peer(n);
            seed(&resolver, p);
            handle.kick(p, ReconnectTrigger::NewPeer);
        }
        // Let all first dials dispatch; parked dials hold their slot.
        tokio::time::sleep(ms(5_000)).await;
        assert_eq!(
            dialer.peak.load(Ordering::SeqCst),
            max,
            "peak in-flight dials should equal the concurrency cap"
        );
        assert_eq!(
            dialer.in_flight.load(Ordering::SeqCst),
            max,
            "exactly `max` dials parked in flight"
        );
        dialer.release();
    }

    #[tokio::test(start_paused = true)]
    async fn kick_is_lossless_and_coalescing() {
        let dialer = RecordingDialer::succeeding();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let a = peer(1);
        let b = peer(2);
        let c = peer(3);
        for p in [a, b, c] {
            seed(&resolver, p);
        }
        let handle = SupervisorHandle::new();
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 8, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        // 1000 kicks for `a` before the loop drains -> exactly one dial.
        for _ in 0..1000 {
            handle.kick(a, ReconnectTrigger::NewPeer);
        }
        // b, c each kicked once -> no event lost.
        handle.kick(b, ReconnectTrigger::NewPeer);
        handle.kick(c, ReconnectTrigger::NewPeer);

        tokio::time::sleep(ms(2_000)).await;
        assert_eq!(dialer.count_for(a), 1, "1000 kicks coalesce to one dial");
        assert_eq!(dialer.count_for(b), 1, "b not lost");
        assert_eq!(dialer.count_for(c), 1, "c not lost");
    }

    #[tokio::test(start_paused = true)]
    async fn connected_peer_not_dialed_until_dropped() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 8, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.mark_connected(p);
        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(5_000)).await;
        assert_eq!(dialer.count_for(p), 0, "Connected peer is not dialed");

        let dropped_at = Instant::now();
        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(ms(2_000)).await;
        let times = dialer.times_for(p);
        assert!(!times.is_empty(), "Dropped re-arms the connected peer");
        assert_between(
            times[0] - dropped_at,
            rung_lo(1_000),
            rung_hi(1_000),
            "dial at base after Dropped",
        );
    }
}
