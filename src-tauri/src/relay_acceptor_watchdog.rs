//! ZEB-803: self-healing watchdog for the community-relay acceptor.
//!
//! Pure decision core (`evaluate`) + a generic harness. The core is a state
//! machine over the observed signals (last-served-pull time, zenoh transport
//! peers, last inbound pull attempt, wall clock) that decides Hold / tier-1
//! probe / tier-2 restart / escalate. ZEB-971: a stall means UNSERVED DEMAND —
//! serve-staleness only counts while someone is actually present to serve
//! (zenoh peers, or pull attempts arriving), so an idle node never
//! self-restarts. All correctness lives here and is unit-tested with injected
//! values; the live levers sit behind the traits in the harness section.

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
    /// Reconnect-supervisor `Connected` count. ZEB-971: DIAGNOSTICS ONLY — the
    /// supervisor's state machine can hold a zombie `Connected` long after the
    /// transport died (the 0.2.9 field incident: `connected=1` for a peer
    /// asleep 26.8h), so decisions key on `zenoh_peers` and
    /// `last_pull_attempt_ms` instead. Kept in the inputs because the
    /// supervisor-vs-zenoh disagreement in a WARN line is exactly what
    /// diagnosed the incident.
    pub connected_peers: u32,
    /// ZEB-971: zenoh's OWN live transport-peer count
    /// ([`crate::network_health::ZenohTransportPeers`]). The demand signal — a
    /// dead link drops out at the zenoh link lease, so this cannot go zombie.
    pub zenoh_peers: u32,
    /// ZEB-971: most recent inbound relay-pull ATTEMPT of any outcome
    /// (served/rejected/failed). `None` = never attempted this lifetime. An
    /// arriving pull is demand evidence even when zenoh reads zero peers
    /// (e.g. a relay-mediated puller with no zenoh session).
    pub last_pull_attempt_ms: Option<u64>,
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
    /// ZEB-971: when zenoh demand last transitioned 0 → >0 (`None` while no
    /// zenoh peer is present). Staleness is measured from
    /// `max(last_served_ms, demand_since_ms)`, so a peer reconnecting after an
    /// idle stretch grants a full stall-threshold grace window before the
    /// accumulated idle staleness can count as a fault.
    pub demand_since_ms: Option<u64>,
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
            demand_since_ms: None,
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

fn stale_threshold_ms(cfg: &WatchdogConfig) -> u64 {
    cfg.stale_multiplier as u64 * cfg.cadence_ms
}

/// ZEB-971: is there anyone to serve RIGHT NOW? Zenoh transport peers are
/// live demand; a pull attempt within one stall window is recent demand. The
/// supervisor's `connected_peers` deliberately does NOT count — it can hold a
/// zombie `Connected` long after the transport died.
fn demand_present(cfg: &WatchdogConfig, inputs: &WatchdogInputs) -> bool {
    inputs.zenoh_peers > 0
        || inputs
            .last_pull_attempt_ms
            .is_some_and(|t| inputs.now_ms.saturating_sub(t) <= stale_threshold_ms(cfg))
}

/// ZEB-971: a stall is UNSERVED DEMAND, not raw staleness. Serve-staleness is
/// not evidence of malfunction when there was nobody to serve — the 0.2.9
/// field false-positive fired at 26.8h of "staleness" that was simply the sole
/// peer being asleep.
fn stall_detected(cfg: &WatchdogConfig, inputs: &WatchdogInputs, mem: &WatchdogMemory) -> bool {
    if !mem.served_ever {
        return false;
    }
    let Some(served_ms) = inputs.last_served_ms else {
        return false;
    };
    let threshold = stale_threshold_ms(cfg);
    // Path A — sustained zenoh demand unserved for a full threshold. Measured
    // from `max(last_served, demand_since)`: staleness accumulated while no
    // peer was present is not fault time, and a peer reconnecting after an
    // idle stretch gets a full threshold of grace before this can fire.
    // (`demand_since_ms` is `Some` only while `zenoh_peers > 0` — `evaluate`
    // refreshes it from this same sample before dispatching here.)
    let zenoh_stall = mem
        .demand_since_ms
        .is_some_and(|since| inputs.now_ms.saturating_sub(served_ms.max(since)) > threshold);
    // Path B — pull attempts arriving in-window while serving is stale. The
    // connection arriving is itself the demand proof (rejected/failed pulls
    // included), and needs no zenoh session (relay-mediated pullers).
    let attempt_stall = inputs.last_pull_attempt_ms.is_some_and(|attempt_ms| {
        inputs.now_ms.saturating_sub(attempt_ms) <= threshold
            && inputs.now_ms.saturating_sub(served_ms) > threshold
    });
    zenoh_stall || attempt_stall
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

    // ZEB-971: track the zenoh-demand edge. `demand_since_ms` is the moment
    // demand last went 0 → >0; it clears the instant zenoh reads empty, so
    // the stall clock in `stall_detected` only runs while someone is present.
    if inputs.zenoh_peers == 0 {
        mem.demand_since_ms = None;
    } else if mem.demand_since_ms.is_none() {
        mem.demand_since_ms = Some(inputs.now_ms);
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
            // for Tier 1. If demand vanished during the cooldown there is
            // nothing to serve and a disruptive restart cannot help — re-arm
            // Normal so a fresh stall re-tries the cheap probe first, while
            // KEEPING `consecutive_restarts` so a genuinely persistent stall still
            // escalates. ZEB-971: judged on the demand signals, not the
            // supervisor count — a zombie `Connected` alone must not justify
            // escalating the ladder.
            if !demand_present(cfg, inputs) {
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
                // ZEB-971: log every demand signal beside the supervisor count
                // — the 0.2.9 incident was diagnosed off exactly this
                // disagreement (supervisor `connected=1` vs zenoh 0), and this
                // WARN is the record of which gate let the fire through.
                let attempt_age_ms = inputs
                    .last_pull_attempt_ms
                    .map(|t| inputs.now_ms.saturating_sub(t));
                tracing::warn!(
                    staleness_ms = ?staleness,
                    connected = inputs.connected_peers,
                    zenoh_peers = inputs.zenoh_peers,
                    attempt_age_ms = ?attempt_age_ms,
                    "ZEB-803 watchdog: relay serving stalled — tier 1 network_change()"
                );
                self.actuator.probe_network().await;
            }
            Verdict::RestartNode => {
                tracing::warn!(
                    connected = inputs.connected_peers,
                    zenoh_peers = inputs.zenoh_peers,
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

    /// Inputs with `peers` filling BOTH `connected_peers` and `zenoh_peers`
    /// (the healthy case where the supervisor and zenoh agree) and no pull
    /// attempt. Demand-gate tests construct `WatchdogInputs` explicitly to
    /// pull the two counts apart.
    fn inputs(now_ms: u64, last_served_ms: Option<u64>, peers: u32) -> WatchdogInputs {
        WatchdogInputs {
            now_ms,
            last_served_ms,
            connected_peers: peers,
            zenoh_peers: peers,
            last_pull_attempt_ms: None,
        }
    }

    fn served_mem() -> WatchdogMemory {
        // a node that has served at least once, sitting Normal — with zenoh
        // demand present since t=0, so staleness measured from
        // max(last_served, demand_since) reduces to the pre-ZEB-971 staleness
        // in these scenario tests.
        WatchdogMemory {
            served_ever: true,
            demand_since_ms: Some(0),
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
        let v = evaluate(&test_cfg(), &inputs(5000, Some(4000), 2), &mut m);
        assert_eq!(v, Verdict::Hold);
        assert_eq!(m.phase, Phase::Normal);
    }

    #[test]
    fn never_served_holds_even_when_stale() {
        let mut m = WatchdogMemory::default(); // served_ever = false
        let v = evaluate(&test_cfg(), &inputs(100_000, None, 5), &mut m);
        assert_eq!(v, Verdict::Hold);
        assert!(!m.served_ever);
    }

    #[test]
    fn no_connected_peers_holds() {
        let mut m = served_mem();
        let v = evaluate(&test_cfg(), &inputs(10_000, Some(1000), 0), &mut m);
        assert_eq!(v, Verdict::Hold);
    }

    #[test]
    fn stall_fires_tier1_probe() {
        let mut m = served_mem();
        let v = evaluate(&test_cfg(), &inputs(10_000, Some(1000), 2), &mut m);
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
        evaluate(&test_cfg(), &inputs(10_000, Some(1000), 2), &mut m);
        // Δ = 1000 < 2000 cooldown, still stale → Hold
        let v = evaluate(&test_cfg(), &inputs(11_000, Some(1000), 2), &mut m);
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
        evaluate(&test_cfg(), &inputs(10_000, Some(1000), 2), &mut m);
        // a serve landed after the baseline (10_500 > 1000) → recovered
        let v = evaluate(&test_cfg(), &inputs(11_000, Some(10_500), 2), &mut m);
        assert_eq!(v, Verdict::Hold);
        assert_eq!(m.phase, Phase::Normal);
        assert_eq!(m.consecutive_restarts, 0);
        assert_eq!(m.baseline_served_ms, None);
    }

    #[test]
    fn probe_failure_escalates_to_restart() {
        let mut m = served_mem();
        evaluate(&test_cfg(), &inputs(10_000, Some(1000), 2), &mut m);
        // Δ = 2500 >= 2000 cooldown, still stale → tier-2 restart
        let v = evaluate(&test_cfg(), &inputs(12_500, Some(1000), 2), &mut m);
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
        let v = evaluate(&test_cfg(), &inputs(5000, Some(1000), 2), &mut m);
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
        let v1 = evaluate(&test_cfg(), &inputs(5000, Some(1000), 2), &mut m);
        assert_eq!(v1, Verdict::Hold);
        assert_eq!(m.phase, Phase::Escalated);
        // a fresh serve (2000 > 1000) → reset to Normal
        let v2 = evaluate(&test_cfg(), &inputs(6000, Some(2000), 2), &mut m);
        assert_eq!(v2, Verdict::Hold);
        assert_eq!(m.phase, Phase::Normal);
    }

    #[test]
    fn served_ever_is_sticky_across_none() {
        let mut m = WatchdogMemory::default();
        evaluate(&test_cfg(), &inputs(1000, Some(500), 1), &mut m);
        assert!(m.served_ever);
        // a later None sample (fresh telemetry after a restart) must not clear it
        evaluate(&test_cfg(), &inputs(2000, None, 1), &mut m);
        assert!(m.served_ever);
    }

    #[test]
    fn persistent_stall_is_one_probe_three_restarts_then_escalate() {
        let cfg = test_cfg();
        let mut m = served_mem();
        let mut actions = Vec::new();
        // stale last_served fixed at 1000; step now by 3000 (> cooldown 2000) each tick
        for now in [5000u64, 8000, 11_000, 14_000, 17_000, 20_000] {
            let v = evaluate(&cfg, &inputs(now, Some(1000), 2), &mut m);
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

    // --- ZEB-971 demand-gate tests ---

    /// The 0.2.9 field incident shape: sole peer asleep 26.8h, staleness huge,
    /// and the reconnect supervisor still holding a zombie `Connected`
    /// (`connected=1`). With zero zenoh peers and no inbound pull attempt
    /// there is NO demand — an idle node must never escalate toward a
    /// self-restart, no matter how stale the serve stamp reads.
    #[test]
    fn idle_node_with_zombie_connected_but_no_demand_never_fires() {
        let cfg = test_cfg();
        let mut m = served_mem();
        m.demand_since_ms = None;
        for now in [100_000u64, 200_000, 300_000] {
            let v = evaluate(
                &cfg,
                &WatchdogInputs {
                    now_ms: now,
                    last_served_ms: Some(1000),
                    connected_peers: 1, // the zombie
                    zenoh_peers: 0,
                    last_pull_attempt_ms: None,
                },
                &mut m,
            );
            assert_eq!(v, Verdict::Hold);
            assert_eq!(m.phase, Phase::Normal);
        }
    }

    /// A peer reconnecting after an idle stretch grants a FULL stall-threshold
    /// grace window: staleness accumulated while nobody was present is not
    /// fault time. Without this, the demand gate would fire the instant the
    /// sole peer came back (staleness already 26.8h > threshold).
    #[test]
    fn demand_reappearing_after_idle_gets_full_grace_window() {
        let cfg = test_cfg(); // threshold = 3 * 1000 = 3000ms
        let mut m = served_mem();
        m.demand_since_ms = None; // idle until now
        let present = |now_ms: u64| WatchdogInputs {
            now_ms,
            last_served_ms: Some(1000), // ancient — idle staleness
            connected_peers: 1,
            zenoh_peers: 1,
            last_pull_attempt_ms: None,
        };
        let v0 = evaluate(&cfg, &present(100_000), &mut m);
        assert_eq!(v0, Verdict::Hold, "demand just appeared — grace, not fire");
        assert_eq!(m.demand_since_ms, Some(100_000), "demand edge stamped");
        // within the grace window (Δ = 3000, not yet PAST the threshold)
        let v1 = evaluate(&cfg, &present(103_000), &mut m);
        assert_eq!(v1, Verdict::Hold);
        // a full unserved threshold after the demand edge → genuine stall
        let v2 = evaluate(&cfg, &present(103_500), &mut m);
        assert_eq!(v2, Verdict::ProbeNetwork);
    }

    /// Pull attempts arriving without serves are demand even with zero zenoh
    /// peers (a relay-mediated puller has no zenoh session) — rejected/failed
    /// pulls while serving is stale must still fire the ladder.
    #[test]
    fn fresh_pull_attempts_without_serves_fire_with_zero_zenoh_peers() {
        let cfg = test_cfg();
        let mut m = served_mem();
        m.demand_since_ms = None;
        let v = evaluate(
            &cfg,
            &WatchdogInputs {
                now_ms: 10_000,
                last_served_ms: Some(1000),
                connected_peers: 0,
                zenoh_peers: 0,
                last_pull_attempt_ms: Some(9_500), // arriving but not served
            },
            &mut m,
        );
        assert_eq!(v, Verdict::ProbeNetwork);
    }

    /// A stale attempt stamp is history, not demand: attempts outside one
    /// stall window must not fire. (Guard: passes before and after ZEB-971 —
    /// pins that the attempt path has a freshness window at all.)
    #[test]
    fn stale_pull_attempt_is_not_demand() {
        let cfg = test_cfg();
        let mut m = served_mem();
        m.demand_since_ms = None;
        let v = evaluate(
            &cfg,
            &WatchdogInputs {
                now_ms: 100_000,
                last_served_ms: Some(1000),
                connected_peers: 0,
                zenoh_peers: 0,
                last_pull_attempt_ms: Some(1000), // ancient
            },
            &mut m,
        );
        assert_eq!(v, Verdict::Hold);
        assert_eq!(m.phase, Phase::Normal);
    }

    /// Zenoh going dark during a cooldown re-arms Normal even while the
    /// supervisor still claims a zombie `Connected` — nothing to serve, so a
    /// disruptive restart cannot help (extends the ZEB-803 connected==0
    /// re-arm to the demand signals).
    #[test]
    fn demand_gone_during_cooldown_rearms_normal_despite_zombie_connected() {
        let cfg = test_cfg();
        let mut m = served_mem();
        let v0 = evaluate(&cfg, &inputs(10_000, Some(1000), 2), &mut m);
        assert_eq!(v0, Verdict::ProbeNetwork);
        // cooldown elapsed, still stale — but demand vanished; supervisor
        // zombie alone must not justify a restart.
        let v1 = evaluate(
            &cfg,
            &WatchdogInputs {
                now_ms: 12_500,
                last_served_ms: Some(1000),
                connected_peers: 1, // zombie
                zenoh_peers: 0,
                last_pull_attempt_ms: None,
            },
            &mut m,
        );
        assert_eq!(v1, Verdict::Hold);
        assert_eq!(m.phase, Phase::Normal, "no demand → re-arm, not restart");
        assert_eq!(m.consecutive_restarts, 0);
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
            // `connected` fills both peer counts — harness tests exercise the
            // tier machinery, not the demand gate (that lives in the pure
            // `evaluate` tests).
            inputs(now_ms, self.last_served_ms, self.connected)
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
        let v0 = evaluate(&cfg, &inputs(10_000, Some(1000), 2), &mut m);
        assert_eq!(v0, Verdict::ProbeNetwork);
        // Cooldown elapsed, still stale, but ALL peers gone → re-arm Normal, NO restart.
        let v1 = evaluate(&cfg, &inputs(12_500, Some(1000), 0), &mut m);
        assert_eq!(v1, Verdict::Hold);
        assert_eq!(m.phase, Phase::Normal);
        assert_eq!(m.consecutive_restarts, 0);
        // Peers return, still stale → ZEB-971 grace first (the returning peer
        // must get a full unserved threshold before staleness counts) …
        let v2 = evaluate(&cfg, &inputs(20_000, Some(1000), 2), &mut m);
        assert_eq!(v2, Verdict::Hold, "fresh demand edge → grace, not fire");
        // … then a fresh PROBE (cheap lever first), not a restart.
        let v3 = evaluate(&cfg, &inputs(23_500, Some(1000), 2), &mut m);
        assert_eq!(v3, Verdict::ProbeNetwork);
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
            demand_since_ms: Some(0),
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
