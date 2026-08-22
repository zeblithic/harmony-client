//! ZEB-803: self-healing watchdog for the community-relay acceptor.
//!
//! Pure decision core (`evaluate`) + a generic harness. The core is a state
//! machine over three observed signals (last-served-pull time, connected-peer
//! count, wall clock) that decides Hold / tier-1 probe / tier-2 restart /
//! escalate. All correctness lives here and is unit-tested with injected values;
//! the live levers sit behind the traits in the harness section.

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Tunable knobs. Production values are derived from the relay pull cadence at
/// boot; tests pass small values.
#[derive(Clone, Copy, Debug)]
pub struct WatchdogConfig {
    /// Relay serve cadence (= `COMMUNITY_RELAY_PULL_INTERVAL_MS`, ~7m30s).
    pub cadence_ms: u64,
    /// Stall threshold = `stale_multiplier * cadence_ms`.
    pub stale_multiplier: u32,
    /// Watchdog tick period.
    pub eval_interval_ms: u64,
    /// Grace after a tier-1 `network_change()` before re-judging.
    pub tier1_cooldown_ms: u64,
    /// Grace after a tier-2 full restart before re-judging.
    pub tier2_cooldown_ms: u64,
    /// Consecutive full restarts without recovery before escalating.
    pub max_restarts: u32,
    /// ZEB-970: wall-clock bound on ONE tier-2 restart. A restart that has not
    /// completed within this bound is judged wedged (in the field: `stop_inner`
    /// stuck in the unbounded iroh `Endpoint::close()` on a dead network) — the
    /// watchdog escalates loudly instead of silently parking forever. The
    /// actuator's `restart_node` future is DROPPED at the bound, so
    /// implementations must detach the real work (see the trait doc); a late
    /// completion still brings the node up.
    pub restart_wedge_bound_ms: u64,
}

/// One sample of the observed signals.
#[derive(Clone, Copy, Debug)]
pub struct WatchdogInputs {
    pub now_ms: u64,
    /// `None` = never served in this telemetry lifetime (the `0 → None` sentinel).
    pub last_served_ms: Option<u64>,
    pub connected_peers: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Probe,
    Restart,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Normal,
    Cooldown { since_ms: u64, tier: Tier },
    Escalated,
}

/// Persistent watchdog state. Lives in a process-global so it survives the
/// in-process Tier-2 restart (see the prod wiring). `served_ever` is sticky.
#[derive(Clone, Copy, Debug)]
pub struct WatchdogMemory {
    pub served_ever: bool,
    pub phase: Phase,
    pub consecutive_restarts: u32,
    /// `last_served_ms` captured at the last action — the recovery baseline.
    pub baseline_served_ms: Option<u64>,
    pub last_action_ms: Option<u64>,
    pub last_action_tier: Option<Tier>,
}

impl Default for WatchdogMemory {
    fn default() -> Self {
        Self {
            served_ever: false,
            phase: Phase::Normal,
            consecutive_restarts: 0,
            baseline_served_ms: None,
            last_action_ms: None,
            last_action_tier: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Hold,
    ProbeNetwork,
    RestartNode,
    Escalate,
}

fn cooldown_for(cfg: &WatchdogConfig, tier: Tier) -> u64 {
    match tier {
        Tier::Probe => cfg.tier1_cooldown_ms,
        Tier::Restart => cfg.tier2_cooldown_ms,
    }
}

/// A remedy is confirmed only when a serve has landed *after* the action's
/// baseline — never raw staleness, since a successful remedy can't clear
/// staleness until the next remote pull (~1 cadence away).
fn recovered(inputs: &WatchdogInputs, mem: &WatchdogMemory) -> bool {
    match inputs.last_served_ms {
        Some(c) => mem.baseline_served_ms.is_none_or(|b| c > b),
        None => false,
    }
}

fn stall_detected(cfg: &WatchdogConfig, inputs: &WatchdogInputs, mem: &WatchdogMemory) -> bool {
    mem.served_ever
        && inputs.connected_peers > 0
        && match inputs.last_served_ms {
            Some(ts) => {
                inputs.now_ms.saturating_sub(ts) > cfg.stale_multiplier as u64 * cfg.cadence_ms
            }
            None => false,
        }
}

fn fire_restart(
    cfg: &WatchdogConfig,
    inputs: &WatchdogInputs,
    mem: &mut WatchdogMemory,
) -> Verdict {
    if mem.consecutive_restarts >= cfg.max_restarts {
        mem.phase = Phase::Escalated;
        return Verdict::Escalate;
    }
    mem.consecutive_restarts += 1;
    mem.baseline_served_ms = inputs.last_served_ms;
    mem.last_action_ms = Some(inputs.now_ms);
    mem.last_action_tier = Some(Tier::Restart);
    mem.phase = Phase::Cooldown {
        since_ms: inputs.now_ms,
        tier: Tier::Restart,
    };
    Verdict::RestartNode
}

fn reset(mem: &mut WatchdogMemory) {
    mem.phase = Phase::Normal;
    mem.consecutive_restarts = 0;
    mem.baseline_served_ms = None;
}

/// The pure decision core. Mutates `mem` (phase/counters/baselines) and returns
/// the action the harness should take.
pub fn evaluate(
    cfg: &WatchdogConfig,
    inputs: &WatchdogInputs,
    mem: &mut WatchdogMemory,
) -> Verdict {
    if inputs.last_served_ms.is_some() {
        mem.served_ever = true; // sticky
    }

    match mem.phase {
        Phase::Escalated => {
            if recovered(inputs, mem) {
                reset(mem);
            }
            Verdict::Hold // terminal until a serve resets it
        }
        Phase::Cooldown { since_ms, tier } => {
            if recovered(inputs, mem) {
                reset(mem);
                return Verdict::Hold;
            }
            if inputs.now_ms.saturating_sub(since_ms) < cooldown_for(cfg, tier) {
                return Verdict::Hold; // still waiting to see if the remedy took
            }
            // ZEB-803 (CodeRabbit + CodeAnt): the stall gate is only re-checked
            // for Tier 1. If every peer disconnected during the cooldown there is
            // nothing to serve and a disruptive restart cannot help — re-arm
            // Normal so a fresh stall re-tries the cheap probe first, while
            // KEEPING `consecutive_restarts` so a genuinely persistent stall still
            // escalates.
            if inputs.connected_peers == 0 {
                mem.phase = Phase::Normal;
                return Verdict::Hold;
            }
            // cooldown elapsed, still not recovered → escalate to a restart
            // (tier-1 failure → first restart; tier-2 failure → repeat/Escalate)
            fire_restart(cfg, inputs, mem)
        }
        Phase::Normal => {
            if !stall_detected(cfg, inputs, mem) {
                return Verdict::Hold;
            }
            mem.baseline_served_ms = inputs.last_served_ms;
            mem.last_action_ms = Some(inputs.now_ms);
            mem.last_action_tier = Some(Tier::Probe);
            mem.phase = Phase::Cooldown {
                since_ms: inputs.now_ms,
                tier: Tier::Probe,
            };
            Verdict::ProbeNetwork
        }
    }
}

// ---------------------------------------------------------------------------
// Harness — the live loop over injected sensors/actuators. All I/O is behind
// these traits so the decision core stays pure and the harness is testable
// without a live iroh endpoint.
// ---------------------------------------------------------------------------

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

pub trait ServingSensor: Send + Sync {
    fn sample(&self, now_ms: u64) -> WatchdogInputs;
}

#[async_trait::async_trait]
pub trait RemediationActuator: Send + Sync {
    /// Tier 1: in-place re-probe (`endpoint.network_change()`).
    async fn probe_network(&self);
    /// Tier 2: full-node restart (`stop_inner` + `start_node_inner`).
    ///
    /// ZEB-970 contract: the watchdog wraps this future in
    /// `WatchdogConfig::restart_wedge_bound_ms` and DROPS it on expiry.
    /// Implementations must therefore be cancel-safe: run the actual
    /// stop/start as a detached task and await its handle here, so a dropped
    /// future abandons only the *wait*, never the restart itself.
    async fn restart_node(&self);
}

pub struct RelayAcceptorWatchdog<S, A, C> {
    cfg: WatchdogConfig,
    memory: Arc<Mutex<WatchdogMemory>>,
    sensor: S,
    actuator: A,
    clock: C,
}

impl<S: ServingSensor, A: RemediationActuator, C: Clock> RelayAcceptorWatchdog<S, A, C> {
    pub fn new(
        cfg: WatchdogConfig,
        memory: Arc<Mutex<WatchdogMemory>>,
        sensor: S,
        actuator: A,
        clock: C,
    ) -> Self {
        Self {
            cfg,
            memory,
            sensor,
            actuator,
            clock,
        }
    }

    /// Background loop: evaluate every `eval_interval_ms` until shutdown.
    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        // ZEB-803 (CodeRabbit): if shutdown was already requested before we
        // started, never act — `watch::changed()` does not observe the initial
        // value as a change, so check the current value explicitly.
        if *shutdown.borrow() {
            return;
        }
        // `.max(1)`: `tokio::time::interval` panics on a zero period, which a
        // reduced cadence (or a tiny test config) could divide down to.
        let mut interval =
            tokio::time::interval(Duration::from_millis(self.cfg.eval_interval_ms.max(1)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                // `biased`: the shutdown branch wins over the immediately-ready
                // first tick and over a tick that becomes ready concurrently, so a
                // stop — including the watchdog's own Tier-2 restart — ends this
                // task instead of acting on a torn-down node.
                biased;
                _ = shutdown.changed() => break,
                _ = interval.tick() => {
                    if *shutdown.borrow() {
                        break;
                    }
                    self.tick().await;
                }
            }
        }
    }

    /// One evaluation: sample → decide → act. `pub(crate)` so harness tests can
    /// drive it directly without coordinating tokio timers.
    pub(crate) async fn tick(&self) {
        let inputs = self.sensor.sample(self.clock.now_ms());
        let verdict = {
            let mut m = self.memory.lock().expect("watchdog memory poisoned");
            evaluate(&self.cfg, &inputs, &mut m)
        };
        match verdict {
            Verdict::Hold => {}
            Verdict::ProbeNetwork => {
                let staleness = inputs
                    .last_served_ms
                    .map(|t| inputs.now_ms.saturating_sub(t));
                tracing::warn!(
                    staleness_ms = ?staleness,
                    connected = inputs.connected_peers,
                    "ZEB-803 watchdog: relay serving stalled — tier 1 network_change()"
                );
                self.actuator.probe_network().await;
            }
            Verdict::RestartNode => {
                tracing::warn!(
                    connected = inputs.connected_peers,
                    "ZEB-803 watchdog: tier-1 probe did not restore serving — tier 2 full-node restart"
                );
                // ZEB-970: bound the restart. Without this, a `stop_inner`
                // wedged in the unbounded iroh close parked this await — and
                // the watchdog — forever, with zero log output (the field
                // incident: node down until manual app relaunch). Dropping the
                // actuator future at the bound is safe because the prod
                // actuator detaches the real work; a late completion still
                // brings the node up, and `Phase::Escalated` then clears
                // through the normal `recovered()` path on the first serve.
                // Log the clamped value below — with a zero config the enforced
                // bound is 1ms, and the diagnostic must match it (CodeRabbit
                // PR #720).
                let bound_ms = self.cfg.restart_wedge_bound_ms.max(1);
                let bound = Duration::from_millis(bound_ms);
                if tokio::time::timeout(bound, self.actuator.restart_node())
                    .await
                    .is_err()
                {
                    tracing::error!(
                        bound_ms,
                        "ZEB-970 watchdog: tier-2 restart exceeded its wall-clock bound — \
                         likely wedged stopping the node; escalating (node may stay down \
                         until app relaunch; a late completion still brings it up)"
                    );
                    self.memory.lock().expect("watchdog memory poisoned").phase = Phase::Escalated;
                }
            }
            Verdict::Escalate => {
                tracing::error!(
                    "ZEB-803 watchdog: relay serving still stalled after max restarts — escalating, no further automatic action"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_cfg() -> WatchdogConfig {
        // threshold = 3 * 1000 = 3000ms; cooldowns 2000ms
        WatchdogConfig {
            cadence_ms: 1000,
            stale_multiplier: 3,
            eval_interval_ms: 300,
            tier1_cooldown_ms: 2000,
            tier2_cooldown_ms: 2000,
            max_restarts: 3,
            restart_wedge_bound_ms: 2000,
        }
    }

    fn served_mem() -> WatchdogMemory {
        // a node that has served at least once, sitting Normal
        WatchdogMemory {
            served_ever: true,
            ..WatchdogMemory::default()
        }
    }

    #[test]
    fn memory_default_is_normal_and_zeroed() {
        let m = WatchdogMemory::default();
        assert!(!m.served_ever);
        assert_eq!(m.phase, Phase::Normal);
        assert_eq!(m.consecutive_restarts, 0);
        assert_eq!(m.baseline_served_ms, None);
    }

    #[test]
    fn healthy_serve_holds() {
        let mut m = served_mem();
        let v = evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 5000,
                last_served_ms: Some(4000),
                connected_peers: 2,
            },
            &mut m,
        );
        assert_eq!(v, Verdict::Hold);
        assert_eq!(m.phase, Phase::Normal);
    }

    #[test]
    fn never_served_holds_even_when_stale() {
        let mut m = WatchdogMemory::default(); // served_ever = false
        let v = evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 100_000,
                last_served_ms: None,
                connected_peers: 5,
            },
            &mut m,
        );
        assert_eq!(v, Verdict::Hold);
        assert!(!m.served_ever);
    }

    #[test]
    fn no_connected_peers_holds() {
        let mut m = served_mem();
        let v = evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 10_000,
                last_served_ms: Some(1000),
                connected_peers: 0,
            },
            &mut m,
        );
        assert_eq!(v, Verdict::Hold);
    }

    #[test]
    fn stall_fires_tier1_probe() {
        let mut m = served_mem();
        let v = evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 10_000,
                last_served_ms: Some(1000),
                connected_peers: 2,
            },
            &mut m,
        );
        assert_eq!(v, Verdict::ProbeNetwork);
        assert_eq!(
            m.phase,
            Phase::Cooldown {
                since_ms: 10_000,
                tier: Tier::Probe
            }
        );
        assert_eq!(m.baseline_served_ms, Some(1000));
        assert_eq!(m.last_action_tier, Some(Tier::Probe));
    }

    #[test]
    fn cooldown_probe_within_holds() {
        let mut m = served_mem();
        evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 10_000,
                last_served_ms: Some(1000),
                connected_peers: 2,
            },
            &mut m,
        );
        // Δ = 1000 < 2000 cooldown, still stale → Hold
        let v = evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 11_000,
                last_served_ms: Some(1000),
                connected_peers: 2,
            },
            &mut m,
        );
        assert_eq!(v, Verdict::Hold);
        assert_eq!(
            m.phase,
            Phase::Cooldown {
                since_ms: 10_000,
                tier: Tier::Probe
            }
        );
    }

    #[test]
    fn probe_recovery_resets() {
        let mut m = served_mem();
        evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 10_000,
                last_served_ms: Some(1000),
                connected_peers: 2,
            },
            &mut m,
        );
        // a serve landed after the baseline (10_500 > 1000) → recovered
        let v = evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 11_000,
                last_served_ms: Some(10_500),
                connected_peers: 2,
            },
            &mut m,
        );
        assert_eq!(v, Verdict::Hold);
        assert_eq!(m.phase, Phase::Normal);
        assert_eq!(m.consecutive_restarts, 0);
        assert_eq!(m.baseline_served_ms, None);
    }

    #[test]
    fn probe_failure_escalates_to_restart() {
        let mut m = served_mem();
        evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 10_000,
                last_served_ms: Some(1000),
                connected_peers: 2,
            },
            &mut m,
        );
        // Δ = 2500 >= 2000 cooldown, still stale → tier-2 restart
        let v = evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 12_500,
                last_served_ms: Some(1000),
                connected_peers: 2,
            },
            &mut m,
        );
        assert_eq!(v, Verdict::RestartNode);
        assert_eq!(m.consecutive_restarts, 1);
        assert_eq!(
            m.phase,
            Phase::Cooldown {
                since_ms: 12_500,
                tier: Tier::Restart
            }
        );
    }

    #[test]
    fn restart_cap_escalates() {
        let mut m = WatchdogMemory {
            served_ever: true,
            phase: Phase::Cooldown {
                since_ms: 0,
                tier: Tier::Restart,
            },
            consecutive_restarts: 3, // already at cap
            baseline_served_ms: Some(1000),
            ..WatchdogMemory::default()
        };
        // cooldown elapsed, not recovered, at cap → Escalate
        let v = evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 5000,
                last_served_ms: Some(1000),
                connected_peers: 2,
            },
            &mut m,
        );
        assert_eq!(v, Verdict::Escalate);
        assert_eq!(m.phase, Phase::Escalated);
    }

    #[test]
    fn escalated_holds_until_recovery() {
        let mut m = WatchdogMemory {
            served_ever: true,
            phase: Phase::Escalated,
            baseline_served_ms: Some(1000),
            ..WatchdogMemory::default()
        };
        // not recovered → stays Escalated
        let v1 = evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 5000,
                last_served_ms: Some(1000),
                connected_peers: 2,
            },
            &mut m,
        );
        assert_eq!(v1, Verdict::Hold);
        assert_eq!(m.phase, Phase::Escalated);
        // a fresh serve (2000 > 1000) → reset to Normal
        let v2 = evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 6000,
                last_served_ms: Some(2000),
                connected_peers: 2,
            },
            &mut m,
        );
        assert_eq!(v2, Verdict::Hold);
        assert_eq!(m.phase, Phase::Normal);
    }

    #[test]
    fn served_ever_is_sticky_across_none() {
        let mut m = WatchdogMemory::default();
        evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 1000,
                last_served_ms: Some(500),
                connected_peers: 1,
            },
            &mut m,
        );
        assert!(m.served_ever);
        // a later None sample (fresh telemetry after a restart) must not clear it
        evaluate(
            &test_cfg(),
            &WatchdogInputs {
                now_ms: 2000,
                last_served_ms: None,
                connected_peers: 1,
            },
            &mut m,
        );
        assert!(m.served_ever);
    }

    #[test]
    fn persistent_stall_is_one_probe_three_restarts_then_escalate() {
        let cfg = test_cfg();
        let mut m = served_mem();
        let mut actions = Vec::new();
        // stale last_served fixed at 1000; step now by 3000 (> cooldown 2000) each tick
        for now in [5000u64, 8000, 11_000, 14_000, 17_000, 20_000] {
            let v = evaluate(
                &cfg,
                &WatchdogInputs {
                    now_ms: now,
                    last_served_ms: Some(1000),
                    connected_peers: 2,
                },
                &mut m,
            );
            if v != Verdict::Hold {
                actions.push(v);
            }
        }
        assert_eq!(
            actions,
            vec![
                Verdict::ProbeNetwork,
                Verdict::RestartNode,
                Verdict::RestartNode,
                Verdict::RestartNode,
                Verdict::Escalate
            ]
        );
    }

    // --- harness tests (direct tick() driving; no tokio-timer coordination) ---

    struct MockClock(Arc<AtomicU64>);
    impl Clock for MockClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    struct MockSensor {
        last_served_ms: Option<u64>,
        connected: u32,
    }
    impl ServingSensor for MockSensor {
        fn sample(&self, now_ms: u64) -> WatchdogInputs {
            WatchdogInputs {
                now_ms,
                last_served_ms: self.last_served_ms,
                connected_peers: self.connected,
            }
        }
    }

    struct RecordingActuator(Arc<Mutex<Vec<&'static str>>>);
    #[async_trait::async_trait]
    impl RemediationActuator for RecordingActuator {
        async fn probe_network(&self) {
            self.0.lock().unwrap().push("probe");
        }
        async fn restart_node(&self) {
            self.0.lock().unwrap().push("restart");
        }
    }

    #[tokio::test]
    async fn harness_drives_probe_then_restarts_then_escalate() {
        let clock = Arc::new(AtomicU64::new(0));
        let actions = Arc::new(Mutex::new(Vec::new()));
        let wd = RelayAcceptorWatchdog::new(
            test_cfg(),
            Arc::new(Mutex::new(served_mem())),
            MockSensor {
                last_served_ms: Some(1000),
                connected: 2,
            }, // persistently stale
            RecordingActuator(actions.clone()),
            MockClock(clock.clone()),
        );
        // step the injected clock past a cooldown each tick
        for now in [5000u64, 8000, 11_000, 14_000, 17_000, 20_000] {
            clock.store(now, Ordering::Relaxed);
            wd.tick().await;
        }
        assert_eq!(
            *actions.lock().unwrap(),
            vec!["probe", "restart", "restart", "restart"]
        );

        // and no action fires during a cooldown tick
        let actions2 = Arc::new(Mutex::new(Vec::new()));
        let clock2 = Arc::new(AtomicU64::new(10_000));
        let wd2 = RelayAcceptorWatchdog::new(
            test_cfg(),
            Arc::new(Mutex::new(served_mem())),
            MockSensor {
                last_served_ms: Some(1000),
                connected: 2,
            },
            RecordingActuator(actions2.clone()),
            MockClock(clock2.clone()),
        );
        wd2.tick().await; // stall → probe
        clock2.store(10_500, Ordering::Relaxed); // Δ 500 < 2000 cooldown
        wd2.tick().await; // cooldown → no action
        assert_eq!(*actions2.lock().unwrap(), vec!["probe"]);
    }

    #[test]
    fn peers_gone_during_cooldown_rearms_normal_not_restart() {
        let cfg = test_cfg();
        let mut m = served_mem();
        // Stall → Tier 1 probe, enters Cooldown{Probe}.
        let v0 = evaluate(
            &cfg,
            &WatchdogInputs {
                now_ms: 10_000,
                last_served_ms: Some(1000),
                connected_peers: 2,
            },
            &mut m,
        );
        assert_eq!(v0, Verdict::ProbeNetwork);
        // Cooldown elapsed, still stale, but ALL peers gone → re-arm Normal, NO restart.
        let v1 = evaluate(
            &cfg,
            &WatchdogInputs {
                now_ms: 12_500,
                last_served_ms: Some(1000),
                connected_peers: 0,
            },
            &mut m,
        );
        assert_eq!(v1, Verdict::Hold);
        assert_eq!(m.phase, Phase::Normal);
        assert_eq!(m.consecutive_restarts, 0);
        // Peers return, still stale → a fresh PROBE (cheap lever first), not a restart.
        let v2 = evaluate(
            &cfg,
            &WatchdogInputs {
                now_ms: 20_000,
                last_served_ms: Some(1000),
                connected_peers: 2,
            },
            &mut m,
        );
        assert_eq!(v2, Verdict::ProbeNetwork);
    }

    /// Memory pre-armed one step before tier-2: a probe cooldown that has
    /// elapsed without recovery, so the next stale+connected sample fires
    /// `Verdict::RestartNode`.
    fn pre_tier2_mem() -> WatchdogMemory {
        WatchdogMemory {
            served_ever: true,
            phase: Phase::Cooldown {
                since_ms: 0,
                tier: Tier::Probe,
            },
            consecutive_restarts: 0,
            baseline_served_ms: Some(1000),
            last_action_ms: Some(0),
            last_action_tier: Some(Tier::Probe),
        }
    }

    /// ZEB-970: an actuator whose restart wedges forever — the field failure
    /// (`stop_inner` stuck in the unbounded iroh close on a dead network).
    struct WedgedActuator;
    #[async_trait::async_trait]
    impl RemediationActuator for WedgedActuator {
        async fn probe_network(&self) {}
        async fn restart_node(&self) {
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn tier2_wedge_bound_escalates_instead_of_hanging() {
        let mem = Arc::new(Mutex::new(pre_tier2_mem()));
        let wd = RelayAcceptorWatchdog::new(
            test_cfg(),
            mem.clone(),
            MockSensor {
                last_served_ms: Some(1000),
                connected: 2,
            }, // persistently stale
            WedgedActuator,
            MockClock(Arc::new(AtomicU64::new(10_000))), // probe cooldown elapsed
        );
        // The tick must RETURN once the wedge bound (2s in test_cfg) fires.
        // The outer bound is test-side only, far above the config bound, so a
        // hang here is the ZEB-970 bug, not a tight-timing flake.
        tokio::time::timeout(Duration::from_millis(60_000), wd.tick())
            .await
            .expect("tick must return at the wedge bound — hanging forever is the ZEB-970 bug");
        assert_eq!(
            mem.lock().unwrap().phase,
            Phase::Escalated,
            "a wedged tier-2 restart must escalate loudly, not park silently"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn tier2_restart_within_bound_does_not_escalate() {
        let mem = Arc::new(Mutex::new(pre_tier2_mem()));
        let actions = Arc::new(Mutex::new(Vec::new()));
        let wd = RelayAcceptorWatchdog::new(
            test_cfg(),
            mem.clone(),
            MockSensor {
                last_served_ms: Some(1000),
                connected: 2,
            },
            RecordingActuator(actions.clone()),
            MockClock(Arc::new(AtomicU64::new(10_000))),
        );
        wd.tick().await;
        assert_eq!(*actions.lock().unwrap(), vec!["restart"]);
        let m = *mem.lock().unwrap();
        assert_eq!(
            m.phase,
            Phase::Cooldown {
                since_ms: 10_000,
                tier: Tier::Restart
            },
            "a restart that completes within the bound keeps the normal cooldown phase"
        );
    }

    #[tokio::test]
    async fn run_does_not_act_when_shutdown_already_requested() {
        // `watch::channel(true)` — shutdown already requested at spawn.
        let (_tx, rx) = tokio::sync::watch::channel(true);
        let actions = Arc::new(Mutex::new(Vec::new()));
        let wd = RelayAcceptorWatchdog::new(
            test_cfg(),
            Arc::new(Mutex::new(served_mem())),
            MockSensor {
                last_served_ms: Some(1000),
                connected: 2,
            }, // would stall
            RecordingActuator(actions.clone()),
            MockClock(Arc::new(AtomicU64::new(1_000_000))),
        );
        // Returns immediately without ticking.
        wd.run(rx).await;
        assert!(actions.lock().unwrap().is_empty());
    }
}
