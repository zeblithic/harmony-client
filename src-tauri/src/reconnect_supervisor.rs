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
//! The module itself stays pure logic; the producers feed the `kick*` /
//! `mark_connected` seams from outside — `event_loop.rs` wires the resolver
//! (`set_supervisor`: first-learn + changed-record kicks), the transport
//! (`set_reconnect_handle`: registry drop-watchers both directions +
//! `mark_connected`), the zenoh transport-events listener (`Delete` →
//! `Dropped`), boot seeding, and the presence roster-edge sweeps.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::time::Instant;

use crate::iroh_dial_driver::PeerDialer;
use crate::network_health::DialTelemetry;
use crate::reachability_resolver::ReachabilityResolver;

/// Multiplicative jitter applied to every scheduled delay: the realized delay is
/// `nominal × U[JITTER_LO, JITTER_HI)`, a uniform multiplier over a full-width
/// window centred on the nominal rung. Unlike a one-sided extension, a rung may
/// fire earlier *or* later than nominal, decorrelating a herd of simultaneous
/// re-arms in both directions. Deterministic under `SupervisorConfig.jitter_seed`.
const JITTER_LO: f64 = 0.5;
const JITTER_HI: f64 = 1.5;

/// Why a peer needs attention. Kicks are idempotent — duplicates are harmless
/// (the dirty set coalesces them). On merge the *strongest* trigger wins:
/// `Dropped` > `NewPeer` = `RecordChanged` > `PresenceSweep`. The ordering only
/// changes behavior for an already-`Connected` peer: `Dropped` re-arms it,
/// `NewPeer`/`RecordChanged`/`PresenceSweep` merely refresh its interest stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectTrigger {
    /// Resolver first-learn of a peer.
    NewPeer,
    /// Resolver LWW-replaced an existing record with new relay/addresses.
    RecordChanged,
    /// Registry eviction or a zenoh transport `Delete` — the transport is gone.
    Dropped,
    /// Identity-free roster edge: re-arm ALL known non-connected peers.
    ///
    /// Reserved for identity-AWARE per-peer sweeps: v1 roster edges carry no
    /// peer id, so every sweep goes through [`SupervisorHandle::kick_sweep`] →
    /// `do_sweep` and nothing `kick()`s this variant today. It participates in
    /// the merge ordering so per-peer sweep kicks slot in without a wire
    /// change when the liveness slice adds them.
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
    /// Wall-clock bound on a single dial attempt. A dial still pending after
    /// this long is treated as failed (the peer ladders) and its permit is
    /// released — otherwise `max_concurrent_dials` hung dials would starve
    /// every other peer's reconnect forever.
    pub dial_timeout: Duration,
    /// First-attempt delay for a [`DialRole::DelayedDialer`] (higher NodeId).
    pub higher_id_fallback_delay: Duration,
    /// ZEB-910: interval between Dormant-parole ticks — a low-frequency
    /// periodic sweep that re-arms a small batch of record-backed Dormant
    /// peers with a paired stale-refresh, so pair-level recovery is
    /// steady-state instead of edge-triggered (a quiet, stable community
    /// otherwise keeps cross-island pairs Dormant for process life). Default
    /// ≥ the resolver's per-owner refresh cooldown (15 min) so the paired
    /// refresh is never structurally cooldown-blocked. `Duration::ZERO`
    /// DISABLES parole (PR #659 review: a zero interval would otherwise fire
    /// on every loop pass and busy-spin the select).
    pub parole_interval: Duration,
    /// ZEB-910: max Dormant peers re-armed per parole tick. Batch × interval
    /// caps steady-state parole load regardless of the Dormant population;
    /// a failing parolee re-dormants after `dormant_after` on its own.
    pub parole_batch: usize,
    /// `Some(seed)` makes jitter deterministic (tests); `None` seeds from entropy.
    pub jitter_seed: Option<u64>,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            retry_base: Duration::from_secs(1),
            retry_cap: Duration::from_secs(300),
            dormant_after: Duration::from_secs(900),
            presence_sweep_cooldown: Duration::from_secs(60),
            max_concurrent_dials: 4,
            dial_timeout: Duration::from_secs(30),
            higher_id_fallback_delay: Duration::from_secs(5),
            parole_interval: Duration::from_secs(900),
            parole_batch: 2,
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
    /// ZEB-622: true once this peer has reached `Connected` at least once (set on
    /// every →Connected edge: `mark_connected` and the `apply_result` ok-arm).
    /// Gates the `reconnected` ring marker so a peer's FIRST-ever connect is not
    /// mis-counted as a recovery.
    ever_connected: bool,
    /// ZEB-622: set by `mark_connected` when an inbound connect lands on a peer
    /// that has connected before and is not currently `Connected` — a real
    /// (non-Connected)→Connected recovery edge. `mark_connected` has no
    /// resolver access (the `reconnected` ring marker needs the peer's owner
    /// addr), so it defers the marker; the supervisor loop drains this flag on
    /// its next pass (owner via the resolver, marker via telemetry) and clears
    /// it. (The ZEB-804 `connected_via_registry` counter needs no owner and is
    /// stamped by `mark_connected` directly.)
    pending_reconnected_marker: bool,
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
    /// ZEB-804 (spec §8): optional dial-telemetry sink for the
    /// `connected_via_registry` counter. Installed once by
    /// [`run_reconnect_supervisor`] (which already receives the shared
    /// `DialTelemetry` Arc — no new wiring); absent on handles whose loop
    /// never started (hermetic tests), where `mark_connected` simply doesn't
    /// stamp. NOTE: because the install rides loop startup, a registry swap in
    /// the narrow window between the transport's `set_reconnect_handle` and
    /// the loop task's first poll goes uncounted — acceptable for a
    /// legibility counter.
    dial_telemetry: OnceLock<Arc<DialTelemetry>>,
    /// ZEB-928 (R4): optional admission oracle. Installed once at boot via
    /// [`SupervisorHandle::set_admission_oracle`]. When present, `kick` and `do_sweep`
    /// drop dials to node_ids the oracle rejects (bounded-degree filter); absent — or the
    /// oracle disabled (peer mode) — every kick/sweep passes, exactly pre-R4 behavior.
    admission_oracle: OnceLock<Arc<crate::admission_oracle::AdmissionOracle>>,
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
                dial_telemetry: OnceLock::new(),
                admission_oracle: OnceLock::new(),
            }),
        }
    }

    /// Lossless, non-async: insert `trigger` for `peer` into the dirty set
    /// (strongest wins) and wake the loop. Never blocks, never drops.
    pub fn kick(&self, peer: [u8; 32], trigger: ReconnectTrigger) {
        // ZEB-928 (R4): bounded-degree filter. Drop dials to non-neighbors before they
        // enter the dirty set. Disabled oracle / peer mode / unknown node_id all admit
        // (see `AdmissionOracle::admit`), so pre-R4 behavior is unchanged.
        if let Some(o) = self.inner.admission_oracle.get() {
            if !o.admit(&peer) {
                return;
            }
        }
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

    /// ZEB-804 (spec §8): install the shared [`DialTelemetry`] so
    /// [`mark_connected`](Self::mark_connected) can stamp the
    /// `connected_via_registry` counter. Install-once; a later install is
    /// ignored (all production paths share one Arc, so first-wins is
    /// equivalent). Called by [`run_reconnect_supervisor`] at startup.
    pub fn set_dial_telemetry(&self, telemetry: Arc<DialTelemetry>) {
        let _ = self.inner.dial_telemetry.set(telemetry);
    }

    /// ZEB-928 (R4): install the bounded-degree admission oracle. Install-once (first
    /// wins, like [`set_dial_telemetry`](Self::set_dial_telemetry)); called at boot
    /// alongside the resolver install. Until installed, `kick`/`do_sweep` filter nothing.
    pub fn set_admission_oracle(&self, oracle: Arc<crate::admission_oracle::AdmissionOracle>) {
        let _ = self.inner.admission_oracle.set(oracle);
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
                ever_connected: false,
                pending_reconnected_marker: false,
            });
            // ZEB-622: a (non-Connected)→Connected inbound edge on a peer that has
            // connected before is a real recovery. `mark_connected` can't reach
            // the resolver/telemetry, so defer the `reconnected` marker for the
            // loop's next pass to drain. (A fresh slot is inserted `Connected`
            // with `ever_connected: false`, so a first connect takes neither the
            // `was_connected` nor the `ever_connected` branch — no marker.)
            let was_connected = matches!(slot.state, PeerState::Connected { .. });
            if slot.ever_connected && !was_connected {
                slot.pending_reconnected_marker = true;
            }
            slot.epoch = slot.epoch.wrapping_add(1);
            slot.state = PeerState::Connected { since_ms: now_ms() };
            slot.ever_connected = true;
            // The epoch bump voids any outstanding dial's result, so the flag
            // must not outlive it: left set, it would suppress the first
            // re-dial after a subsequent `Dropped` until that stale dial
            // finally resolved.
            slot.dial_in_flight = false;
        }
        // ZEB-804 (spec §8): every registry-swap Connected entry bumps the
        // lifetime `connected_via_registry` counter (a superseding same-peer
        // swap counts again — it is a new registry swap). This is what lets
        // `dialStatus` read "healthy inbound-connected listener" instead of
        // "attempts: 0, is dialing broken?". No-op until
        // `run_reconnect_supervisor` installs the telemetry.
        if let Some(telemetry) = self.inner.dial_telemetry.get() {
            telemetry.record_connected_via_registry();
        }
        self.inner.notify.notify_one();
    }

    /// ZEB-627: forget a departed peer (membership Leave/Kick). Removes the
    /// slot in ANY state — a departed peer must never be dialed again, and
    /// Dormant slots otherwise persist for process life (unbounded over
    /// long-session peer churn). Also drops the peer's pending dirty entry so
    /// a pre-eviction kick can't immediately resurrect the slot.
    ///
    /// ZEB-634 closed the former residual here: a LATER kick — in
    /// particular the departing conn's drop-watcher `Dropped` — used to
    /// recreate a fresh slot that resolve-misses laddered to a
    /// process-lifetime Dormant. `apply_trigger` now declines to arm (and
    /// removes any existing slot for) a `Dropped` whose peer has no live
    /// resolver record, so the eviction is final unless the peer
    /// legitimately re-announces (record-add ⇒ `NewPeer`/`RecordChanged`)
    /// or connects inbound (`mark_connected`).
    /// Non-async and sync-context-safe, like every other handle method.
    pub fn evict_peer(&self, peer: [u8; 32]) {
        // Qodo PR #397 R1: hold BOTH locks across the removal so a concurrent
        // `kick()` cannot land between the dirty-remove and the states-remove
        // and resurrect the slot on the loop's next drain. This is the only
        // nested dirty+states acquisition in the module (the loop drains
        // `dirty` and RELEASES it before taking `states`, and every other
        // path takes exactly one of the two), so the dirty→states order here
        // cannot deadlock. Kicks that arrive AFTER eviction completes are
        // declined at drain time by `apply_trigger`'s record-less-`Dropped`
        // gate (ZEB-634).
        let removed = {
            let mut dirty = self.inner.dirty.lock().expect("dirty lock");
            let mut states = self.inner.states.lock().expect("states lock");
            dirty.remove(&peer);
            states.remove(&peer).is_some()
        };
        if removed {
            self.inner.notify.notify_one();
        }
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

    /// Test-only read accessor for the coalescing dirty set: the pending
    /// (not-yet-drained) trigger for `peer`, or `None` if the peer has no
    /// pending kick. Parallels [`Self::states_snapshot`] (which reads `states`);
    /// this reads `dirty`. Read-only — never mutates and exposes no mutation
    /// capability — so it does not weaken the production type. Gated to
    /// `cfg(test)`: only in-crate unit tests (the transport drop-watcher tests,
    /// ZEB-620 Task 3) observe raised kicks through it, since a `kick` writes
    /// `dirty` (not `states`) and there is no production reader of `dirty`.
    #[cfg(test)]
    pub fn pending_trigger(&self, peer: [u8; 32]) -> Option<ReconnectTrigger> {
        self.inner
            .dirty
            .lock()
            .expect("dirty lock")
            .get(&peer)
            .copied()
    }

    /// Test-only state fabrication: insert (or overwrite) `peer`'s slot in the
    /// given state without running the supervisor loop. The gateway dial
    /// driver's ZEB-910 tests need a `Dormant`/`Retrying` slot to exist so the
    /// targeted-revival pass has something to observe via `states_snapshot`;
    /// production states are only ever loop-materialized.
    #[cfg(test)]
    pub fn set_peer_state_for_test(&self, peer: [u8; 32], state: PeerState) {
        let mut states = self.inner.states.lock().expect("states lock");
        states.insert(
            peer,
            PeerSlot {
                state,
                last_fresh_trigger: Instant::now(),
                dial_in_flight: false,
                epoch: 0,
                ever_connected: false,
                pending_reconnected_marker: false,
            },
        );
    }

    /// Test-only read accessor for the pending-sweep flag: `true` once
    /// [`Self::kick_sweep`] has been called and before the loop drains it.
    /// Parallels [`Self::pending_trigger`] (which reads `dirty`); this reads
    /// `sweep_requested`. Read-only — never mutates, exposes no mutation
    /// capability — so it does not weaken the production type. Gated to
    /// `cfg(test)`: only in-crate unit tests (the presence-edge wiring test,
    /// ZEB-620 Task 5) observe a raised sweep through it, since `kick_sweep`
    /// writes `sweep_requested` and there is no production reader of it.
    #[cfg(test)]
    pub fn sweep_pending(&self) -> bool {
        self.inner.sweep_requested.load(Ordering::Acquire)
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

/// Realized delay = nominal × a uniform multiplier in `[JITTER_LO, JITTER_HI)`.
fn schedule_delay(
    attempt: u32,
    role: DialRole,
    config: &SupervisorConfig,
    rng: &mut ChaCha8Rng,
) -> Duration {
    let nominal = nominal_delay(attempt, role, config);
    let mult: f64 = rng.gen_range(JITTER_LO..JITTER_HI);
    nominal.mul_f64(mult)
}

/// Deterministic-locator form used for every dial (`iroh/<hex(node_id)>`); the
/// iroh transport resolves relay/addrs from the NodeId via its own discovery,
/// so the locator does not embed the record's relay — the record's role here is
/// purely freshness gating. (Originally mirrored a same-named helper in
/// `iroh_dial_driver`, since deleted with the dial-once machinery — this is
/// now the only copy.)
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
    // ZEB-804: give `mark_connected` — the registry-swap seam — the telemetry
    // so it can stamp `connected_via_registry`. Rides the Arc this loop
    // already receives; install-once (a re-run on the same handle keeps the
    // first, identical, Arc).
    handle.set_dial_telemetry(Arc::clone(&telemetry));
    let mut rng = ChaCha8Rng::seed_from_u64(config.jitter_seed.unwrap_or_else(rand::random));
    let sem = Arc::new(Semaphore::new(config.max_concurrent_dials.max(1)));
    let (res_tx, mut res_rx) = mpsc::unbounded_channel::<DialResult>();

    // Sweep bookkeeping (loop-local): the instant of the last performed sweep and
    // the deadline of a sweep deferred because it arrived within the cooldown.
    let mut last_sweep: Option<Instant> = None;
    let mut pending_sweep_at: Option<Instant> = None;
    // ZEB-910: next Dormant-parole tick (paused-clock Instant, so tests drive
    // it through virtual time like every other schedule in this loop).
    // `None` = parole disabled (`parole_interval == ZERO`, PR #659 review —
    // an always-elapsed deadline would busy-spin the select).
    let mut next_parole =
        (config.parole_interval > Duration::ZERO).then(|| Instant::now() + config.parole_interval);

    loop {
        let now = Instant::now();

        // --- ZEB-910 dormant parole ----------------------------------------
        if let Some(due) = next_parole {
            if now >= due {
                run_parole(
                    &inner,
                    &resolver,
                    &telemetry,
                    now,
                    &self_node_id,
                    &config,
                    &mut rng,
                );
                next_parole = Some(now + config.parole_interval);
            }
        }

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
                    &telemetry,
                    &resolver,
                );
            }

            // --- drain inbound-reconnect markers (ZEB-622) -----------------
            // `mark_connected` can't reach the resolver/telemetry, so a real
            // (non-Connected)→Connected inbound edge leaves
            // `pending_reconnected_marker` set; emit it here with the owner +
            // telemetry in hand. No live routing record ⇒ skip the marker (no
            // invented owner).
            for (peer, slot) in states.iter_mut() {
                if !slot.pending_reconnected_marker {
                    continue;
                }
                slot.pending_reconnected_marker = false;
                if let Some(owner) = resolver.resolve_by_node_id(peer).map(|(o, _)| o.0) {
                    telemetry.record_reconnected(*peer, owner);
                }
            }

            let mut dial_capacity_exhausted = false;
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
                        // ZEB-621: if this peer's freshest record is >24h stale,
                        // kick off a fire-and-forget async pkarr re-resolve to
                        // heal a dial route pinned to a long-dead address. Sync +
                        // non-blocking (staleness/cooldown checks inline, network
                        // in a spawned task), so it never gates the dial
                        // dispatched just below. `now_ms()` (wall clock) matches
                        // the record's `announced_at_ms` epoch — not the paused
                        // scheduler `Instant`.
                        resolver.maybe_refresh_stale(owner, *peer, now_ms());
                        // Acquire the permit BEFORE spawning: this bounds live
                        // tasks (not just active dials), so a large due set
                        // can't park a task per peer that later dials a stale
                        // schedule. No permit ⇒ the peer stays due (not
                        // in-flight) and is picked up on a later pass.
                        let permit = match Arc::clone(&sem).try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                dial_capacity_exhausted = true;
                                break;
                            }
                        };
                        slot.epoch = slot.epoch.wrapping_add(1);
                        // ZEB-622: read the ever-connected bit BEFORE marking the
                        // dial in-flight and thread it into the task: a success
                        // when this was already true is a recovery (marker), a
                        // first-ever connect is not.
                        let ever_connected_before = slot.ever_connected;
                        slot.dial_in_flight = true;
                        let epoch = slot.epoch;
                        let peer = *peer;
                        let owner = owner.0;
                        let dial_timeout = config.dial_timeout;
                        let dialer = dialer.clone();
                        let telemetry = telemetry.clone();
                        let res_tx = res_tx.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            telemetry.record_attempt();
                            // A timed-out dial is a failed dial: dropping the
                            // future cancels the attempt, the ladder advances,
                            // and — critically — the permit frees.
                            let ok = matches!(
                                tokio::time::timeout(
                                    dial_timeout,
                                    dialer.dial(peer, iroh_locator(&peer)),
                                )
                                .await,
                                Ok(true)
                            );
                            if ok {
                                telemetry.record_succeeded(peer, owner);
                                // ZEB-622: a dial success on a peer that has
                                // connected before is a recovery — emit the
                                // `reconnected` marker (gated on the ever-connected
                                // bit read at dispatch). The first-ever connect
                                // (`ever_connected_before == false`) emits none, so
                                // the marker no longer duplicates every `succeeded`.
                                if ever_connected_before {
                                    telemetry.record_reconnected(peer, owner);
                                }
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
                        ladder_after_failure(
                            slot,
                            now,
                            &self_node_id,
                            peer,
                            &config,
                            &mut rng,
                            &telemetry,
                            &resolver,
                        );
                    }
                }
            }

            if dial_capacity_exhausted {
                // Peers left due-but-undispatched would make `earliest_deadline`
                // return a past instant and busy-spin the loop. Every held
                // permit belongs to a task that always reports a `DialResult`
                // (the dial timeout bounds it), so waking on the result channel
                // is guaranteed — sleep unbounded until then.
                None
            } else {
                earliest_deadline(&states, now)
            }
        };

        // ZEB-910: the parole tick joins the sleep like the deferred sweep —
        // including when dial capacity is exhausted (`next_deadline == None`):
        // a parole wake during exhaustion is harmless (dispatch skips
        // in-flight slots) and the held-permit wake guarantee is unaffected.
        let deadline = min_opt(min_opt(next_deadline, pending_sweep_at), next_parole);

        tokio::select! {
            biased;
            Some(result) = res_rx.recv() => {
                apply_result(&inner, result, &self_node_id, &config, &mut rng, &telemetry, &resolver);
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
#[allow(clippy::too_many_arguments)]
fn apply_trigger(
    states: &mut HashMap<[u8; 32], PeerSlot>,
    peer: [u8; 32],
    trigger: ReconnectTrigger,
    now: Instant,
    self_node_id: &[u8; 32],
    config: &SupervisorConfig,
    rng: &mut ChaCha8Rng,
    telemetry: &DialTelemetry,
    resolver: &ReachabilityResolver,
) {
    // ZEB-634 (items 1+3): a `Dropped` kick for a peer with NO live resolver
    // record is a departure echo, not a reconnect signal — the canonical
    // source is the departing conn's drop-watcher firing after membership
    // eviction already removed both the records and the slot. Arming it
    // would recreate a slot that resolve-misses ladder to a Dormant that
    // parks for process life (the ZEB-627 residual). Removing instead of
    // re-arming also cleans inbound-conn-only peers (slot via
    // `mark_connected`, zero records — the membership-eviction path can
    // never name their node-id) at conn-drop, the only causally-available
    // moment. Record-backed kicks are exempt by construction: `NewPeer`/
    // `RecordChanged` fire only AFTER the resolver write, so a record-add
    // always re-creates the slot; a live inbound accept re-enters via
    // `mark_connected`. `remove` on an absent key is the decline-to-create
    // no-op. Single lookup (Qodo PR #405 R1): `resolve_by_node_id` is an
    // O(N) scan, and the Connected→Retrying marker below needs the same
    // owner — resolve once per `Dropped` and thread the result through.
    let dropped_owner = if matches!(trigger, ReconnectTrigger::Dropped) {
        match resolver.resolve_by_node_id(&peer) {
            Some((owner, _)) => Some(owner.0),
            None => {
                states.remove(&peer);
                return;
            }
        }
    } else {
        None
    };
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
                    ever_connected: false,
                    pending_reconnected_marker: false,
                },
            );
        }
        Some(slot) => {
            let connected = matches!(slot.state, PeerState::Connected { .. });
            let record_only = connected && !matches!(trigger, ReconnectTrigger::Dropped);
            if record_only {
                slot.last_fresh_trigger = now;
            } else {
                // ZEB-622: `connected` here means a `Dropped`-driven
                // Connected→Retrying edge (a Connected peer only reaches this
                // branch via `Dropped`; record-only triggers took the branch
                // above) — the SINGLE edge that emits `retrying`. Re-arming an
                // already-Retrying/Dormant peer (every other ladder rung, and
                // dormant revival) emits none. `dropped_owner` is the gate's
                // lookup: `Some` by construction on this edge (a record-less
                // `Dropped` returned early above), reused so the marker costs
                // no second resolver scan.
                if connected {
                    if let Some(owner) = dropped_owner {
                        telemetry.record_retrying(peer, owner);
                    }
                }
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

/// ZEB-910: parole a small batch of Dormant peers — re-arm at the base rung
/// with a paired stale-refresh, longest-dormant first. Only RECORD-BACKED
/// peers are candidates: dialing is record-gated, so a parole without a
/// record is pure state churn (soft-fail ladder, zero dials) — a record-less
/// Dormant peer's healing paths are record arrival (auto-kick) and the
/// gateway driver's community-level repair. ZEB-634 eviction semantics are
/// preserved by construction: departed members' slots were REMOVED, not
/// parked, so parole cannot resurrect them.
#[allow(clippy::too_many_arguments)]
fn run_parole(
    inner: &SupervisorInner,
    resolver: &ReachabilityResolver,
    telemetry: &DialTelemetry,
    now: Instant,
    self_node_id: &[u8; 32],
    config: &SupervisorConfig,
    rng: &mut ChaCha8Rng,
) {
    // Phase 1 (PR #659 review): snapshot the Dormant set under the states
    // lock, then RELEASE it before the resolver lookups — `resolve_by_node_id`
    // is an O(resolver) scan per candidate, and holding `states` across
    // O(dormant × resolver) work would block trigger application,
    // `mark_connected`, and dial-result processing.
    let dormant: Vec<([u8; 32], Instant)> = {
        let states = inner.states.lock().expect("states lock");
        states
            .iter()
            .filter_map(|(peer, slot)| match slot.state {
                PeerState::Dormant { .. } if peer != self_node_id => {
                    Some((*peer, slot.last_fresh_trigger))
                }
                _ => None,
            })
            .collect()
    };
    // Longest-dormant first, ordered by `last_fresh_trigger` (paused-clock
    // `Instant`, which orders dormancy onset exactly). The wall-ms `since_ms`
    // in the state is telemetry-only — it free-runs under tokio's paused test
    // clock, so it must not drive scheduling.
    let mut candidates: Vec<([u8; 32], Instant, [u8; 16])> = dormant
        .into_iter()
        .filter_map(|(peer, t)| {
            resolver
                .resolve_by_node_id(&peer)
                .map(|(owner, _)| (peer, t, owner.0))
        })
        .collect();
    candidates.sort_by_key(|(peer, t, _)| (*t, *peer));
    candidates.truncate(config.parole_batch);
    // Phase 2: reacquire and re-arm, revalidating each slot is STILL Dormant —
    // a kick, inbound connect, or eviction may have landed between the phases,
    // and parole must never clobber fresher state.
    let mut states = inner.states.lock().expect("states lock");
    for (peer, _, owner) in candidates {
        let Some(slot) = states.get_mut(&peer) else {
            continue; // evicted between phases
        };
        if !matches!(slot.state, PeerState::Dormant { .. }) {
            continue; // revived/connected between phases — fresher state wins
        }
        // Paired stale-refresh, standard 24h/15min gates — parole is
        // background hygiene, not urgent repair. Sync + non-blocking by
        // contract (composite-key read + cooldown map inline, network in a
        // spawned task), so the brief hold here is fine.
        resolver.maybe_refresh_stale(crate::owner_state_types::OwnerAddr(owner), peer, now_ms());
        let role = dial_role(self_node_id, &peer);
        slot.epoch = slot.epoch.wrapping_add(1);
        // Grants exactly one `dormant_after` window of retries from the base
        // rung; a parolee that keeps failing re-dormants on its own.
        slot.last_fresh_trigger = now;
        let delay = schedule_delay(0, role, config, rng);
        slot.state = PeerState::Retrying {
            attempt: 0,
            next_at: now + delay,
        };
        telemetry.record_paroled(peer, owner);
        tracing::debug!(peer = %hex::encode(&peer[..8]), "ZEB-910: dormant peer paroled");
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
        // ZEB-928 (R4): a sweep re-arms directly from `states`, bypassing `kick` — so it
        // must consult the same filter, else `kick_sweep` would re-dial non-neighbors and
        // blow the degree bound. Also converges the bound downward: a peer that stopped
        // being a neighbor (membership shrank) is no longer re-armed here.
        if let Some(o) = inner.admission_oracle.get() {
            if !o.admit(peer) {
                continue;
            }
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
#[allow(clippy::too_many_arguments)]
fn ladder_after_failure(
    slot: &mut PeerSlot,
    now: Instant,
    self_node_id: &[u8; 32],
    peer: &[u8; 32],
    config: &SupervisorConfig,
    rng: &mut ChaCha8Rng,
    telemetry: &DialTelemetry,
    resolver: &ReachabilityResolver,
) {
    slot.dial_in_flight = false;
    if now.duration_since(slot.last_fresh_trigger) > config.dormant_after {
        slot.state = PeerState::Dormant { since_ms: now_ms() };
        // ZEB-622: emit `dormant` once per Retrying→Dormant transition — this fn
        // is only reached from a Retrying slot (dispatch soft-fail or a failed
        // dial result), so the marker fires per-transition, not per-rung. No live
        // routing record ⇒ skip the marker (no invented owner).
        if let Some(owner) = resolver.resolve_by_node_id(peer).map(|(o, _)| o.0) {
            telemetry.record_dormant(*peer, owner);
        }
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
    telemetry: &DialTelemetry,
    resolver: &ReachabilityResolver,
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
        // ZEB-622: a successful dial is a →Connected edge; arm the recovery bit
        // so the NEXT reconnect (dial or inbound) counts as a recovery. The
        // `reconnected` marker for THIS dial was already emitted by the dial task
        // (gated on the ever-connected bit read before the dial).
        slot.ever_connected = true;
    } else {
        ladder_after_failure(
            slot,
            Instant::now(),
            self_node_id,
            &result.peer,
            config,
            rng,
            telemetry,
            resolver,
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
    use crate::reachability_resolver::ReachabilityFallback;
    use std::sync::atomic::AtomicUsize;

    // ---- helpers ---------------------------------------------------------

    fn peer(n: u8) -> [u8; 32] {
        [n; 32]
    }

    /// A `ReachabilityFallback` that counts every `resolve` call (ZEB-621 Task
    /// 2). Used to prove the supervisor dispatch pass invokes
    /// `maybe_refresh_stale` for a stale peer; the empty payload set leaves the
    /// dial view untouched so the assertion isolates the fallback firing.
    struct CountingFallback {
        calls: Arc<AtomicUsize>,
        payloads: Vec<ReachabilityAnnouncePayload>,
    }

    #[async_trait::async_trait]
    impl ReachabilityFallback<OwnerAddr> for CountingFallback {
        async fn resolve(&self, _addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.payloads.clone()
        }
    }

    /// Seed a live routing record so the record-gated supervisor will dial `p`.
    ///
    /// `announced_at_ms: 1` is deliberately ancient and LOAD-BEARING: against the
    /// real wall-clock `now_ms()` the supervisor feeds `maybe_refresh_stale`, this
    /// record is unconditionally >24h stale, which is the precondition
    /// `stale_record_dispatch_triggers_pkarr_refresh` relies on to exercise the
    /// ZEB-621 stale-refresh path. Do NOT bump this timestamp to "now" without
    /// giving that test its own stale seed, or it will silently stop testing the
    /// refresh trigger.
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
            // Far above every test's virtual timeline, so parked dials only
            // time out in tests that override this deliberately.
            dial_timeout: Duration::from_secs(600),
            higher_id_fallback_delay: Duration::from_millis(fallback_ms),
            // ZEB-910: parole disabled by default (1h — beyond every
            // pre-parole test's virtual timeline); parole tests override via
            // struct-update syntax.
            parole_interval: Duration::from_secs(3600),
            parole_batch: 2,
            jitter_seed: Some(0xC0FFEE),
        }
    }

    /// ZEB-910: `cfg()` with parole enabled.
    fn cfg_with_parole(base: SupervisorConfig, interval_ms: u64, batch: usize) -> SupervisorConfig {
        SupervisorConfig {
            parole_interval: ms(interval_ms),
            parole_batch: batch,
            ..base
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

    /// Jitter multiplies each nominal rung by a uniform factor in
    /// `[JITTER_LO, JITTER_HI)`, so the realized delay for a nominal rung lies in
    /// `[JITTER_LO·nominal, JITTER_HI·nominal)`. The high bound adds a small slack
    /// for scheduler dispatch latency (≈0 under a paused clock); the low bound is
    /// the exact envelope floor (dispatch latency only ever lengthens the gap).
    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }
    fn rung_lo(nominal_ms: u64) -> Duration {
        ms((nominal_ms as f64 * JITTER_LO) as u64)
    }
    fn rung_hi(nominal_ms: u64) -> Duration {
        ms((nominal_ms as f64 * JITTER_HI) as u64 + 60)
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
        assert!(times.len() > before, "Dropped kick should produce a dial");
        // `before` dials preceded the kick (quiescent above), so the first dial
        // after it is the re-armed base dial. Under wide jitter a further ladder
        // dial may follow inside the 2s window, so index the first, not the last.
        let after = times[before];
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
        // Jitter is U[0.5,1.5): the 10s dormancy line can be crossed as early as
        // the 3rd failed dial (rungs near 1.5x) or as late as the 5th (rungs near
        // 0.5x), so the envelope-honest count is 3..=5.
        assert!(
            (3..=5).contains(&dormant_count),
            "expected 3..=5 pre-dormant dials, got {dormant_count}"
        );
        // Long quiescent window: no dials once dormant.
        tokio::time::sleep(ms(100_000)).await;
        assert_eq!(dialer.count_for(p), dormant_count, "no dials while dormant");

        // A fresh kick revives at the base rung.
        let revive_at = Instant::now();
        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(2_000)).await;
        let times = dialer.times_for(p);
        // A revived peer re-arms at the base rung; under wide jitter a second
        // ladder dial can also fall inside the 2s window, so assert the *first*
        // post-revival dial (not an exact count) lands at base.
        assert!(times.len() > dormant_count, "revival produces a dial");
        assert_between(
            times[dormant_count] - revive_at,
            rung_lo(1_000),
            rung_hi(1_000),
            "revived dial at base",
        );
    }

    // ------------------------------------------------------------------
    // ZEB-910: Dormant parole — periodic, batch-bounded, record-gated.
    // ------------------------------------------------------------------

    /// A record-backed Dormant peer is revived by the parole tick and dials
    /// again; the `paroled` counter moves.
    #[tokio::test(start_paused = true)]
    async fn parole_revives_record_backed_dormant_peer() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        // dormant_after 10s; parole every 60s.
        let config = cfg_with_parole(cfg(1_000, 64_000, 10_000, 30_000, 8, 3_000), 60_000, 2);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(40_000)).await;
        let dormant_count = dialer.count_for(p);
        assert!(dormant_count >= 3, "peer must ladder before dormancy");
        assert!(
            handle
                .states_snapshot()
                .iter()
                .any(|(id, st)| id == &p && matches!(st, PeerStateWire::Dormant { .. })),
            "peer must be Dormant before the parole tick"
        );

        // Cross the parole tick (t=60s): revival at the base rung.
        tokio::time::sleep(ms(60_000)).await;
        assert!(
            dialer.count_for(p) > dormant_count,
            "parole must revive the dormant peer into real dials"
        );
        assert!(
            telemetry.summary().paroled >= 1,
            "paroled counter must move"
        );
    }

    /// Batch bound + longest-dormant first: with batch=1 and three staggered
    /// dormant peers, ticks revive them one per interval, oldest first.
    #[tokio::test(start_paused = true)]
    async fn parole_batch_bound_oldest_first() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let (p1, p2, p3) = (peer(1), peer(2), peer(3));
        seed(&resolver, p1);
        seed(&resolver, p2);
        seed(&resolver, p3);
        let handle = SupervisorHandle::new();
        // dormant_after 10s; parole every 200s, batch 1. Kicks staggered 60s
        // apart so dormancy onsets are strictly ordered despite jitter.
        let config = cfg_with_parole(cfg(1_000, 64_000, 10_000, 30_000, 8, 3_000), 200_000, 1);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p1, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(60_000)).await;
        handle.kick(p2, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(60_000)).await;
        handle.kick(p3, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(75_000)).await; // t=195s: all three dormant
        let (c1, c2, c3) = (
            dialer.count_for(p1),
            dialer.count_for(p2),
            dialer.count_for(p3),
        );

        // First tick (t=200s): only the LONGEST-dormant (p1) revives.
        tokio::time::sleep(ms(10_000)).await; // t=205s
        assert!(
            dialer.count_for(p1) > c1,
            "oldest dormant peer revives first"
        );
        assert_eq!(dialer.count_for(p2), c2, "batch=1: p2 stays dormant");
        assert_eq!(dialer.count_for(p3), c3, "batch=1: p3 stays dormant");

        // Let p1's granted window play out fully (base→…→re-dormant by ~215s)
        // BEFORE capturing its count — mid-ladder rungs are its own parole
        // window, not a second parole.
        tokio::time::sleep(ms(30_000)).await; // t=235s: p1 re-dormant
        let c1b = dialer.count_for(p1);

        // Second tick (t=400s): p1 re-dormanted with a FRESHER trigger stamp,
        // so p2 is now the oldest.
        tokio::time::sleep(ms(170_000)).await; // t=405s
        assert!(dialer.count_for(p2) > c2, "second tick revives p2");
        assert_eq!(
            dialer.count_for(p1),
            c1b,
            "p1 must not be re-paroled ahead of older candidates"
        );
        assert_eq!(dialer.count_for(p3), c3, "p3 waits for the third tick");
    }

    /// Dialing is record-gated, so parole skips record-less Dormant slots —
    /// reviving them is pure state churn with zero dials.
    #[tokio::test(start_paused = true)]
    async fn parole_skips_record_less_dormant() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1); // deliberately NOT seeded: no routing record
        let handle = SupervisorHandle::new();
        let config = cfg_with_parole(cfg(1_000, 64_000, 10_000, 30_000, 8, 3_000), 60_000, 2);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        // Record-less: every dispatch soft-fails, the peer ladders to Dormant
        // without a single real dial.
        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(40_000)).await;
        assert_eq!(dialer.count_for(p), 0, "record-less peers are never dialed");
        assert!(handle
            .states_snapshot()
            .iter()
            .any(|(id, st)| id == &p && matches!(st, PeerStateWire::Dormant { .. })));

        // Several parole ticks later: still Dormant, still zero dials, no
        // parole counted.
        tokio::time::sleep(ms(150_000)).await;
        assert_eq!(dialer.count_for(p), 0);
        assert!(
            handle
                .states_snapshot()
                .iter()
                .any(|(id, st)| id == &p && matches!(st, PeerStateWire::Dormant { .. })),
            "a record-less dormant slot must stay dormant across parole ticks"
        );
        assert_eq!(telemetry.summary().paroled, 0);
    }

    /// Parole is self-bounding: a parolee that keeps failing re-dormants once
    /// `dormant_after` elapses — each tick grants one bounded retry window.
    #[tokio::test(start_paused = true)]
    async fn parolee_re_dormants_after_window() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        // parole every 120s: revival at t=120s, re-dormancy by ~t≈135-145s,
        // observation at t=230s (before the next tick at 240s).
        let config = cfg_with_parole(cfg(1_000, 64_000, 10_000, 30_000, 8, 3_000), 120_000, 2);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(230_000)).await;
        assert!(
            handle
                .states_snapshot()
                .iter()
                .any(|(id, st)| id == &p && matches!(st, PeerStateWire::Dormant { .. })),
            "a failing parolee must re-dormant within its window"
        );
        assert_eq!(
            telemetry.summary().paroled,
            1,
            "exactly one tick paroled it"
        );
    }

    /// The parole pairs a stale-refresh with the re-arm: an ancient record
    /// fires the resolver fallback once the per-owner cooldown allows.
    #[tokio::test(start_paused = true)]
    async fn parole_fires_stale_refresh() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let calls = Arc::new(AtomicUsize::new(0));
        resolver.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&calls),
            payloads: vec![],
        }));
        let p = peer(1);
        seed(&resolver, p); // announced_at=1: ancient under every bar
        let handle = SupervisorHandle::new();
        // Parole at 16 virtual minutes — past the 15-min per-owner cooldown
        // the dispatch-time ZEB-621 refresh consumed at the first dial.
        let config = cfg_with_parole(cfg(1_000, 64_000, 10_000, 30_000, 8, 3_000), 960_000, 2);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(40_000)).await; // dormant; dispatch refresh fired once
        let calls_before = calls.load(Ordering::SeqCst);
        assert!(
            calls_before >= 1,
            "the ZEB-621 dispatch refresh fires first"
        );

        tokio::time::sleep(ms(940_000)).await; // cross the parole tick (t=960s)
        assert!(
            calls.load(Ordering::SeqCst) > calls_before,
            "parole must pair a stale-refresh with the revival"
        );
    }

    /// PR #659 review: `Duration::ZERO` disables parole outright — no
    /// revivals, no busy-spin (an always-elapsed deadline would otherwise
    /// starve the paused-clock auto-advance and hang this very test).
    #[tokio::test(start_paused = true)]
    async fn zero_parole_interval_disables_parole() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        let config = cfg_with_parole(cfg(1_000, 64_000, 10_000, 30_000, 8, 3_000), 0, 2);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(40_000)).await;
        let dormant_count = dialer.count_for(p);

        tokio::time::sleep(ms(300_000)).await;
        assert_eq!(
            dialer.count_for(p),
            dormant_count,
            "no parole revival with a zero interval"
        );
        assert_eq!(telemetry.summary().paroled, 0);
        assert!(handle
            .states_snapshot()
            .iter()
            .any(|(id, st)| id == &p && matches!(st, PeerStateWire::Dormant { .. })));
    }

    /// Parole touches ONLY Dormant slots: Connected and mid-ladder Retrying
    /// peers are left alone (exactly one parole in the tick).
    #[tokio::test(start_paused = true)]
    async fn parole_leaves_retrying_and_connected_alone() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let (d, r, c) = (peer(1), peer(2), peer(3));
        seed(&resolver, d);
        seed(&resolver, r);
        seed(&resolver, c);
        let handle = SupervisorHandle::new();
        let config = cfg_with_parole(cfg(1_000, 64_000, 10_000, 30_000, 8, 3_000), 60_000, 4);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.mark_connected(c);
        handle.kick(d, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(55_000)).await; // d dormant by ~25s
        handle.kick(r, ReconnectTrigger::NewPeer); // r mid-ladder at the tick
        tokio::time::sleep(ms(10_000)).await; // cross t=60s

        assert_eq!(
            telemetry.summary().paroled,
            1,
            "exactly the Dormant peer is paroled — batch 4 must not touch \
             Connected or Retrying slots"
        );
        assert!(
            handle
                .states_snapshot()
                .iter()
                .any(|(id, st)| id == &c && matches!(st, PeerStateWire::Connected { .. })),
            "the Connected peer stays Connected"
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

        // Permits are acquired before spawn, so the 7 undispatched peers are
        // deferred (still due), not parked in tasks. As permits free they must
        // all get their dial — deferral is not loss.
        dialer.release();
        tokio::time::sleep(ms(10_000)).await;
        for n in 1..=10u8 {
            assert!(
                dialer.count_for(peer(n)) >= 1,
                "peer {n} deferred at capacity was never dialed"
            );
        }
    }

    /// Qodo (PR #392): `mark_connected` while a dial is outstanding must clear
    /// `dial_in_flight` — otherwise a subsequent `Dropped` re-arm is suppressed
    /// until the stale dial resolves (unbounded if it hangs).
    #[tokio::test(start_paused = true)]
    async fn mark_connected_mid_dial_does_not_suppress_redial() {
        let dialer = RecordingDialer::parking();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 4, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(rung_hi(1_000)).await;
        assert_eq!(dialer.count_for(p), 1, "first dial dispatched and parked");

        // Inbound connect races the parked outbound dial, then the link drops.
        handle.mark_connected(p);
        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(rung_hi(1_000) + ms(100)).await;
        assert_eq!(
            dialer.count_for(p),
            2,
            "re-dial after Dropped despite the stale dial still parked"
        );
        dialer.release();
    }

    /// Greptile (PR #392): `Dropped` while a dial is in-flight WITHOUT a prior
    /// `mark_connected` re-arms the schedule but keeps dispatch suppressed
    /// until the stale dial resolves — bounded by `dial_timeout`, after which
    /// the re-armed rung dispatches promptly. Pins the deliberate asymmetry
    /// with `mark_connected` (which clears the flag itself) so a future
    /// tightening of `dial_timeout` can't silently turn this into an unbounded
    /// stall.
    #[tokio::test(start_paused = true)]
    async fn dropped_mid_dial_suppression_is_bounded_by_dial_timeout() {
        let dialer = RecordingDialer::parking();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        let mut config = cfg(1_000, 64_000, 3_600_000, 30_000, 4, 3_000);
        config.dial_timeout = ms(8_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(rung_hi(1_000)).await;
        assert_eq!(dialer.count_for(p), 1, "first dial dispatched and parked");

        // Dropped arrives while the dial is still genuinely in flight — no
        // mark_connected ever ran for this peer.
        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(rung_hi(1_000) + ms(500)).await;
        assert_eq!(
            dialer.count_for(p),
            1,
            "re-armed schedule stays suppressed while the stale dial is in flight"
        );

        // The stale dial times out → in-flight clears, its stale-epoch result
        // is discarded, and the re-armed (long overdue) rung dispatches.
        tokio::time::sleep(ms(10_000)).await;
        assert_eq!(
            dialer.count_for(p),
            2,
            "re-dial dispatches once the stale dial resolves via dial_timeout"
        );
        dialer.release();
    }

    /// CodeRabbit (PR #392): a hung dial must not hold its permit forever.
    /// With `max_concurrent_dials = 1`, the second peer can only ever dial if
    /// the first peer's hung dial times out and releases the permit.
    #[tokio::test(start_paused = true)]
    async fn hung_dial_times_out_ladders_and_frees_permit() {
        let dialer = RecordingDialer::parking();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let a = peer(1);
        let b = peer(2);
        seed(&resolver, a);
        seed(&resolver, b);
        let handle = SupervisorHandle::new();
        let mut config = cfg(1_000, 64_000, 3_600_000, 30_000, 1, 3_000);
        config.dial_timeout = ms(10_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(a, ReconnectTrigger::NewPeer);
        handle.kick(b, ReconnectTrigger::NewPeer);
        // Base rung ≈1s (jittered), then the hung dial times out at +10s and
        // frees the sole permit for the other peer.
        tokio::time::sleep(ms(25_000)).await;
        assert!(
            dialer.count_for(a) >= 1 && dialer.count_for(b) >= 1,
            "both peers dialed (a: {}, b: {}) — the permit must free on timeout",
            dialer.count_for(a),
            dialer.count_for(b)
        );
        assert!(
            telemetry.summary().failed >= 1,
            "a timed-out dial is recorded as failed"
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

    /// ZEB-804 (spec §8/§10): `mark_connected` — the registry-swap seam — with
    /// a telemetry installed stamps `connected_via_registry` and NOTHING else:
    /// not `succeeded` (no ladder dial happened), no ring marker. A
    /// superseding same-peer swap counts again; a handle with no telemetry
    /// installed (hermetic tests, pre-loop boot window) must not panic.
    #[test]
    fn mark_connected_stamps_connected_via_registry_only() {
        let telemetry = Arc::new(DialTelemetry::new());
        let handle = SupervisorHandle::new();
        handle.set_dial_telemetry(Arc::clone(&telemetry));
        handle.mark_connected([7u8; 32]);
        handle.mark_connected([7u8; 32]); // superseding swap → counts again
        let s = telemetry.summary();
        assert_eq!(s.connected_via_registry, 2);
        assert_eq!(
            (s.attempts, s.succeeded, s.failed),
            (0, 0, 0),
            "registry-swap connects never move the ladder's dial-outcome counters"
        );
        assert!(s.recent.is_empty(), "counter-only: no ring marker");
        // No telemetry installed → mark_connected is stamp-free and safe.
        let bare = SupervisorHandle::new();
        bare.mark_connected([8u8; 32]);
    }

    /// Chronological (oldest→newest) list of ring-marker/dial outcomes.
    fn outcomes(telemetry: &DialTelemetry) -> Vec<String> {
        telemetry
            .summary()
            .recent
            .iter()
            .map(|h| h.outcome.clone())
            .collect()
    }

    /// ZEB-622: the three ring markers fire only on REAL state edges. A first
    /// connect emits no `reconnected`; a `Connected`→`Retrying` drop emits
    /// exactly one `retrying`; a later dial success (with `ever_connected`
    /// already set) emits `reconnected`.
    #[tokio::test(start_paused = true)]
    async fn ring_markers_fire_on_real_edges_only() {
        let dialer = RecordingDialer::succeeding();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        // self = 0 < p = 1 -> Dialer; dormancy far off so only edges we drive fire.
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 8, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        // First dial succeeds — first-ever connect, so NO `reconnected` marker.
        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(3_000)).await;
        assert_eq!(
            outcomes(&telemetry),
            vec!["succeeded"],
            "first connect: succeeded only, no reconnected marker"
        );

        // Drop the (now Connected) peer -> exactly one `retrying` marker, then the
        // re-armed dial succeeds -> `reconnected` (ever_connected was true).
        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(ms(3_000)).await;
        assert_eq!(
            outcomes(&telemetry),
            vec!["succeeded", "retrying", "succeeded", "reconnected"],
            "drop emits one retrying; recovery dial emits reconnected"
        );
    }

    /// ZEB-622: `dormant` is emitted once per `Retrying`→`Dormant` transition —
    /// not once per ladder rung. A revival that fails back into dormancy emits a
    /// second marker.
    #[tokio::test(start_paused = true)]
    async fn dormant_marker_fires_once_on_dormancy() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        // dormant_after = 10s: ladder fails a handful of rungs then goes dormant.
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
        let dormant_1 = outcomes(&telemetry)
            .iter()
            .filter(|o| *o == "dormant")
            .count();
        assert_eq!(dormant_1, 1, "exactly one dormant marker per transition");

        // Revive: re-arm at base, fail back up the ladder into a SECOND dormancy.
        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(100_000)).await;
        let dormant_2 = outcomes(&telemetry)
            .iter()
            .filter(|o| *o == "dormant")
            .count();
        assert_eq!(
            dormant_2, 2,
            "a second dormancy emits a second marker (per-transition, not per-rung)"
        );
    }

    /// ZEB-622: an inbound reconnect (via `mark_connected`, no dial success) emits
    /// `reconnected` — the marker is deferred by `mark_connected` (no resolver /
    /// telemetry there) and drained by the loop's next pass. A first connect emits
    /// none; the intervening drop emits `retrying`.
    #[tokio::test(start_paused = true)]
    async fn inbound_reconnect_emits_marker_via_mark_connected() {
        // Parking dialer: outbound dials never succeed, so every marker here comes
        // from the mark_connected / drop edges, not a dial-success arm.
        let dialer = RecordingDialer::parking();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 4, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        // First inbound connect (no marker) — mark_connected lands before the loop
        // arms a dial, so the NewPeer kick only refreshes interest.
        handle.kick(p, ReconnectTrigger::NewPeer);
        handle.mark_connected(p);
        tokio::time::sleep(ms(500)).await;
        assert!(
            outcomes(&telemetry).is_empty(),
            "first connect emits no marker, got {:?}",
            outcomes(&telemetry)
        );

        // Drop -> `retrying` (Connected->Retrying edge).
        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(ms(500)).await;
        assert_eq!(
            outcomes(&telemetry),
            vec!["retrying"],
            "drop of a connected peer emits retrying"
        );

        // Inbound reconnect -> `reconnected`, drained by the loop with the resolver.
        handle.mark_connected(p);
        tokio::time::sleep(ms(500)).await;
        assert_eq!(
            outcomes(&telemetry),
            vec!["retrying", "reconnected"],
            "inbound reconnect emits reconnected via the loop drain"
        );
        dialer.release();
    }

    #[tokio::test(start_paused = true)]
    async fn evict_peer_removes_slot_in_any_state() {
        let handle = SupervisorHandle::new();
        let peer = [7u8; 32];
        handle.mark_connected(peer);
        assert_eq!(handle.states_snapshot().len(), 1, "slot exists pre-evict");
        handle.evict_peer(peer);
        assert!(
            handle.states_snapshot().is_empty(),
            "ZEB-627: eviction removes the slot outright"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn evict_peer_unknown_is_noop() {
        let handle = SupervisorHandle::new();
        handle.evict_peer([9u8; 32]);
        assert!(handle.states_snapshot().is_empty());
    }

    /// ZEB-928 (R4): a router-mode oracle drops `kick`s to non-neighbors before they
    /// enter the dirty set; admitted node_ids still queue.
    #[test]
    fn r4_kick_drops_denied_node_ids_router_mode() {
        use crate::admission_oracle::AdmissionOracle;
        let oracle = Arc::new(AdmissionOracle::new(true));
        oracle.publish_admitted(std::collections::BTreeSet::from([[0x81u8; 32]]));
        oracle.bind([1u8; 32], [0x81u8; 32]); // admitted
        oracle.bind([2u8; 32], [0x82u8; 32]); // denied
        let handle = SupervisorHandle::new();
        handle.set_admission_oracle(Arc::clone(&oracle));

        handle.kick([1u8; 32], ReconnectTrigger::NewPeer);
        handle.kick([2u8; 32], ReconnectTrigger::NewPeer);

        assert!(
            handle.pending_trigger([1u8; 32]).is_some(),
            "admitted node_id queued"
        );
        assert!(
            handle.pending_trigger([2u8; 32]).is_none(),
            "denied node_id dropped at kick"
        );
    }

    /// ZEB-928 (R4): the internal `do_sweep` re-arm consults the same oracle — else a
    /// `kick_sweep` would re-dial non-neighbors straight from `states`, bypassing `kick`
    /// and blowing the bound. Admitted known peer → re-armed (Retrying); denied → left
    /// Dormant.
    #[test]
    fn r4_sweep_rearms_only_admitted_known_peers() {
        use crate::admission_oracle::AdmissionOracle;
        let oracle = Arc::new(AdmissionOracle::new(true));
        oracle.publish_admitted(std::collections::BTreeSet::from([[0x81u8; 32]]));
        let admitted = [1u8; 32];
        let denied = [2u8; 32];
        oracle.bind(admitted, [0x81u8; 32]);
        oracle.bind(denied, [0x82u8; 32]);
        let handle = SupervisorHandle::new();
        handle.set_admission_oracle(Arc::clone(&oracle));

        handle.set_peer_state_for_test(admitted, PeerState::Dormant { since_ms: 0 });
        handle.set_peer_state_for_test(denied, PeerState::Dormant { since_ms: 0 });

        let config = cfg(10, 100, 10_000, 60_000, 8, 50);
        let mut rng = ChaCha8Rng::seed_from_u64(0);
        let self_id = [0xFFu8; 32];
        do_sweep(&handle.inner, Instant::now(), &self_id, &config, &mut rng);

        let snap = handle.states_snapshot();
        let admitted_state = snap.iter().find(|(id, _)| *id == admitted).map(|(_, s)| s);
        let denied_state = snap.iter().find(|(id, _)| *id == denied).map(|(_, s)| s);
        assert!(
            matches!(admitted_state, Some(PeerStateWire::Retrying { .. })),
            "admitted known peer re-armed by sweep"
        );
        assert!(
            matches!(denied_state, Some(PeerStateWire::Dormant { .. })),
            "denied known peer NOT re-armed (bound holds under sweep)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn evict_peer_clears_pending_kick_but_not_future_ones() {
        // ZEB-627: eviction drops the peer's PENDING dirty entry (so the loop
        // doesn't immediately resurrect the slot from a pre-eviction kick).
        // A LATER kick still lands in the dirty set — the handle can't know
        // the peer is gone — but since ZEB-634 the loop's `apply_trigger`
        // declines to arm a record-less `Dropped` at drain time (see
        // `dropped_kick_recordless_unknown_creates_no_slot`), so a landed
        // post-eviction kick no longer recreates a slot.
        let handle = SupervisorHandle::new();
        let peer = [3u8; 32];
        handle.kick(peer, ReconnectTrigger::Dropped);
        assert!(handle.pending_trigger(peer).is_some(), "kick pending");
        handle.evict_peer(peer);
        assert!(
            handle.pending_trigger(peer).is_none(),
            "eviction clears the pending kick"
        );
        handle.kick(peer, ReconnectTrigger::Dropped);
        assert_eq!(
            handle.pending_trigger(peer),
            Some(ReconnectTrigger::Dropped),
            "a post-eviction kick still lands in the dirty set (drain-time gate declines it)"
        );
    }

    /// ZEB-634 item 1 (the headline leak): after a membership eviction
    /// (records removed + slot evicted), the departing conn's drop-watcher
    /// `Dropped` must NOT recreate a slot — pre-fix it armed a Retrying slot
    /// that resolve-misses laddered to a process-lifetime Dormant.
    #[tokio::test(start_paused = true)]
    async fn dropped_kick_recordless_unknown_creates_no_slot() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 4, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        // Live peer: inbound connect, then the membership eviction pair
        // (resolver first, slot second — the lib.rs arm order).
        handle.kick(p, ReconnectTrigger::NewPeer);
        handle.mark_connected(p);
        tokio::time::sleep(ms(500)).await;
        assert_eq!(handle.states_snapshot().len(), 1, "connected slot exists");

        resolver.remove_owner(&OwnerAddr([0xAA; 16]));
        handle.evict_peer(p);
        assert!(handle.states_snapshot().is_empty(), "evicted");

        // The departing conn's drop-watcher fires AFTER eviction.
        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(ms(500)).await;
        assert!(
            handle.states_snapshot().is_empty(),
            "ZEB-634: a record-less Dropped must not recreate a slot"
        );
    }

    /// ZEB-643: the eviction pair evicts EXACTLY the node-ids
    /// `remove_owner` deleted. Interleave under pin: a second device (n2)
    /// announces after the arm's would-be capture point but before
    /// `remove_owner`; evicting the RETURNED set clears n2's pending
    /// NewPeer kick (pre-fix, evicting only a stale pre-captured set left
    /// that kick to drain record-less into a process-lifetime Dormant).
    #[tokio::test(start_paused = true)]
    async fn eviction_from_removed_set_clears_interleaved_newpeer_kick() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let n1 = peer(1);
        let n2 = peer(2);
        seed(&resolver, n1);
        let handle = SupervisorHandle::new();
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 4, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        // n1 is a live, connected peer with a slot.
        handle.kick(n1, ReconnectTrigger::NewPeer);
        handle.mark_connected(n1);
        tokio::time::sleep(ms(500)).await;
        assert_eq!(handle.states_snapshot().len(), 1, "connected slot exists");

        // The interleave: a NEW device (n2) lands its record + NewPeer kick
        // after the would-be capture point. No awaits until the eviction
        // pair below, so the kick is still pending when we evict.
        seed(&resolver, n2);
        handle.kick(n2, ReconnectTrigger::NewPeer);
        assert!(handle.pending_trigger(n2).is_some(), "n2 kick queued");

        // The lib.rs arm shape (ZEB-643): evict the RETURNED set.
        let removed = resolver.remove_owner(&OwnerAddr([0xAA; 16]));
        assert_eq!(removed.len(), 2, "both devices deleted");
        for node in &removed {
            handle.evict_peer(*node);
        }
        assert!(
            handle.pending_trigger(n2).is_none(),
            "ZEB-643: the interleaved NewPeer kick is cleared by eviction"
        );

        tokio::time::sleep(ms(500)).await;
        assert!(
            handle.states_snapshot().is_empty(),
            "no slot survives or resurrects after the eviction pair"
        );
    }

    /// Non-regression pin for the gate's scope: a `Dropped` for an unknown
    /// peer that DOES have a live record arms a Retrying slot as before.
    #[tokio::test(start_paused = true)]
    async fn dropped_kick_with_record_still_creates_slot() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let handle = SupervisorHandle::new();
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 4, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(ms(500)).await;
        let snap = handle.states_snapshot();
        assert_eq!(snap.len(), 1, "record-backed Dropped still arms the peer");
        assert!(
            matches!(snap[0].1, PeerStateWire::Retrying { .. }),
            "armed at the ladder, got {:?}",
            snap[0].1
        );
    }

    /// ZEB-634 item 3: an inbound-conn-only peer (slot via `mark_connected`,
    /// zero resolver records — the membership-eviction path can never name
    /// its node-id) is cleaned at conn-drop instead of laddering to a parked
    /// Dormant; a later record-add (⇒ `NewPeer` kick) re-creates the slot.
    #[tokio::test(start_paused = true)]
    async fn dropped_kick_recordless_removes_existing_slot() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        // NO seed: inbound-conn-only peer.
        let handle = SupervisorHandle::new();
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 4, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.mark_connected(p);
        tokio::time::sleep(ms(500)).await;
        assert_eq!(handle.states_snapshot().len(), 1, "inbound slot exists");

        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(ms(500)).await;
        assert!(
            handle.states_snapshot().is_empty(),
            "ZEB-634: record-less Dropped removes the slot (not Dormant-park)"
        );

        // Revival path: a record-add kicks NewPeer (in production via the
        // resolver's installed supervisor handle; manual here) and re-creates.
        seed(&resolver, p);
        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(500)).await;
        assert_eq!(
            handle.states_snapshot().len(),
            1,
            "record-add revives the peer at the base rung"
        );
    }

    /// ZEB-621: the dispatch pass calls `maybe_refresh_stale` for every peer it
    /// resolves, so a peer whose freshest record is >24h stale kicks off an
    /// async pkarr re-resolve as a fire-and-forget side effect of being dialed.
    /// The dial still dispatches (the refresh never gates it), and the counting
    /// fallback firing at least once proves the hook is wired.
    ///
    /// NOTE: `start_paused` controls only the tokio scheduler clock — the
    /// staleness gate itself receives `now_ms()` (real `SystemTime`), and this
    /// test exercises it only because `seed`'s `announced_at_ms: 1` is ancient
    /// against ANY wall clock. A refactor moving the gate onto virtual time
    /// would silently stop testing the stale path here — re-derive the
    /// timestamps if that ever happens.
    #[tokio::test(start_paused = true)]
    async fn stale_record_dispatch_triggers_pkarr_refresh() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
        let counter = Arc::new(AtomicUsize::new(0));
        resolver.set_fallback_source(Arc::new(CountingFallback {
            calls: Arc::clone(&counter),
            payloads: vec![],
        }));
        let handle = SupervisorHandle::new();
        // self = 0 < p = 1 -> Dialer, so the first dial fires at the base rung.
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 8, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(2_000)).await;

        // The stale peer is still dialed — the fire-and-forget refresh must not
        // gate the dispatch.
        assert!(dialer.count_for(p) >= 1, "stale peer is still dialed");
        // …and dispatching it kicked off the async pkarr re-resolve.
        assert!(
            counter.load(Ordering::SeqCst) >= 1,
            "stale record must trigger an async pkarr refresh on dispatch"
        );
    }
}
