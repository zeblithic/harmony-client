//! ZEB-291 Phase 2: periodic tick coordinator for the voting subsystem.
//!
//! One tokio task walks all voting_logs every TICK_INTERVAL (60s prod,
//! configurable shorter for tests). Per spec §5/§9: Tier 1 auto-close,
//! Tier 2 threshold detection + reversion, Tier 2 24h contestability
//! finalize + auto-exec, daily archive sweep.
//!
//! Design choice D3 from the ZEB-291 plan: polling sweep, NOT per-proposal
//! tokio timer. One task vs N; survives process restart trivially;
//! handles Unsignal-mid-window correctly without timer-reset complexity.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::community_voting_conviction::AutoExecAction;
use crate::community_voting_core::{Lifecycle, PollId, Tier};
use crate::community_voting_log::VotingLog;
use crate::owner_state_types::{OwnerAddr, SpaceId};

/// Production tick interval. Override via `spawn_voting_tick` in tests.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(60);
/// Tier 2 contestability window per spec §5: 24h after threshold reached
/// (or last unsignal-after-threshold) before the proposal finalizes.
pub const CONTESTABILITY_WINDOW_MS: i128 = 24 * 60 * 60 * 1000;
/// Archive sweep cadence per spec §2: at-most-once per 24h across all logs.
pub const ARCHIVE_SWEEP_INTERVAL_MS: i128 = 24 * 60 * 60 * 1000;

/// Per-tick aggregated stats for tests + observability.
#[derive(Debug, Default, Clone)]
pub struct TickStats {
    pub tier1_polls_closed: u32,
    pub tier2_thresholds_reached: u32,
    pub tier2_thresholds_reverted: u32,
    pub tier2_proposals_finalized: u32,
    pub tier2_auto_execs_attempted: u32,
    pub tier2_auto_execs_succeeded: u32,
    pub archive_swept: bool,
}

/// Type alias for the auto-exec dispatch hook. Returns a boxed future so
/// the tick context is testable via closure injection without dragging
/// the full NodeState into unit tests.
pub type AutoExecSetPowerFn = Arc<
    dyn Fn(
            SpaceId,
            OwnerAddr,
            u32,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

/// Type alias for the Tauri-event emit hook.
pub type EmitFn = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

/// Context handed to `run_voting_tick`. Holds the shared voting_logs
/// map, the last-archive-sweep wall-clock, and two function hooks
/// (event-emit and auto-exec-set-power) so the tick can be unit-tested
/// via closure injection. The Task 20 wiring in `lib.rs` populates these
/// from the real `AppHandle` + `NodeState`.
pub struct VotingTickContext {
    pub voting_logs: Arc<Mutex<HashMap<SpaceId, Arc<Mutex<VotingLog>>>>>,
    pub last_archive_sweep_ms: Arc<Mutex<i128>>,
    pub emit: EmitFn,
    pub auto_exec_set_power: AutoExecSetPowerFn,
}

/// Run one tick cycle. Returns aggregated stats for testing/observability.
///
/// Sub-passes are sequential and each acquires the necessary locks
/// independently to avoid holding the outer `voting_logs` lock across
/// long-running awaits (e.g. auto-exec dispatch).
pub async fn run_voting_tick(ctx: &VotingTickContext, now_ms: i128) -> Result<TickStats, String> {
    let mut stats = TickStats::default();

    // ── Pass 1: Tier 1 auto-close ────────────────────────────────────
    // Walk all Tier 1 polls in Open lifecycle whose `closes_at` is in
    // the past; transition to Closed and emit `voting-poll-closed`.
    //
    // NOTE: the spec calls for signing + applying PollClose + PollResult
    // events so peers observe the closure. That signing path requires
    // NodeState handles (signing_key + hlc_tracker) that this tick
    // module deliberately does not pull in — Task 14.1 will wire the
    // signing path when the tick is plumbed into lib.rs. For now we
    // do the in-place lifecycle transition + Tauri-event emit so the
    // local UI updates correctly; peer-visible closure events come
    // online with Task 14.1.
    {
        let mut to_close: Vec<(SpaceId, PollId)> = Vec::new();
        {
            let logs = ctx.voting_logs.lock().await;
            for (cid, log_mtx) in logs.iter() {
                let log = log_mtx.lock().await;
                for (pid, state) in log.polls.iter() {
                    if state.meta.tier == Tier::Approval
                        && state.meta.lifecycle == Lifecycle::Open
                        && now_ms >= state.meta.closes_at.wall_ms as i128
                    {
                        to_close.push((*cid, *pid));
                    }
                }
            }
        }
        if !to_close.is_empty() {
            let logs = ctx.voting_logs.lock().await;
            for (cid, pid) in &to_close {
                if let Some(log_mtx) = logs.get(cid) {
                    let mut log = log_mtx.lock().await;
                    if let Some(state) = log.polls.get_mut(pid) {
                        state.meta.lifecycle = Lifecycle::Closed;
                        stats.tier1_polls_closed += 1;
                        (ctx.emit)(
                            "voting-poll-closed",
                            serde_json::json!({
                                "communityId": hex::encode(cid.0),
                                "pollId": hex::encode(pid.0),
                            }),
                        );
                    }
                }
            }
        }
    }

    // ── Pass 2: Tier 2 threshold-cross detection + reversion ────────
    // For every Tier 2 poll: if Open and total_conviction crossed
    // threshold → ThresholdReached + emit event. If ThresholdReached and
    // total_conviction dropped back below threshold → revert to Open,
    // clearing `threshold_reached_at_ms` and recording the unsignal
    // wall-clock so the next finalize attempt resets the 24h timer.
    {
        let logs = ctx.voting_logs.lock().await;
        for (cid, log_mtx) in logs.iter() {
            let mut log = log_mtx.lock().await;
            for (pid, state) in log.polls.iter_mut() {
                if state.meta.tier != Tier::Conviction {
                    continue;
                }
                let t2 = match state.tier_state.as_tier2_mut() {
                    Some(t) => t,
                    None => continue,
                };
                let total = t2.total_conviction_at(now_ms);
                let threshold = t2.threshold_conviction_at(now_ms);
                match state.meta.lifecycle {
                    Lifecycle::Open if total >= threshold => {
                        t2.threshold_reached_at_ms = Some(now_ms);
                        t2.last_unsignal_after_threshold_ms = None;
                        state.meta.lifecycle = Lifecycle::ThresholdReached;
                        stats.tier2_thresholds_reached += 1;
                        (ctx.emit)(
                            "voting-threshold-reached",
                            serde_json::json!({
                                "communityId": hex::encode(cid.0),
                                "proposalId": hex::encode(pid.0),
                                "thresholdReachedAtMs": now_ms,
                            }),
                        );
                    }
                    Lifecycle::ThresholdReached if total < threshold => {
                        t2.threshold_reached_at_ms = None;
                        t2.last_unsignal_after_threshold_ms = Some(now_ms);
                        state.meta.lifecycle = Lifecycle::Open;
                        stats.tier2_thresholds_reverted += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    // ── Pass 3: Tier 2 contestability finalize + auto-exec ──────────
    // Walk Tier 2 polls in ThresholdReached lifecycle; if 24h elapsed
    // since `max(threshold_reached_at_ms, last_unsignal_after_threshold_ms)`
    // with no reversion, transition to Finalized and dispatch auto-exec.
    //
    // Auto-exec runs AFTER releasing the voting_logs lock so the
    // auto-exec helper (which itself takes NodeState locks) cannot
    // deadlock against the tick's outer lock.
    {
        let mut to_finalize: Vec<(SpaceId, PollId, AutoExecAction)> = Vec::new();
        {
            let logs = ctx.voting_logs.lock().await;
            for (cid, log_mtx) in logs.iter() {
                let log = log_mtx.lock().await;
                for (pid, state) in log.polls.iter() {
                    if state.meta.tier != Tier::Conviction {
                        continue;
                    }
                    if state.meta.lifecycle != Lifecycle::ThresholdReached {
                        continue;
                    }
                    let t2 = match state.tier_state.as_tier2() {
                        Some(t) => t,
                        None => continue,
                    };
                    let reached_at = match t2.threshold_reached_at_ms {
                        Some(v) => v,
                        // Defensive: ThresholdReached lifecycle without a
                        // reached_at timestamp shouldn't happen, but
                        // skipping here avoids accidentally finalizing.
                        None => continue,
                    };
                    let uncontested_since =
                        reached_at.max(t2.last_unsignal_after_threshold_ms.unwrap_or(reached_at));
                    if (now_ms - uncontested_since) >= CONTESTABILITY_WINDOW_MS {
                        to_finalize.push((*cid, *pid, t2.config.auto_exec.clone()));
                    }
                }
            }
        }
        for (cid, pid, auto_exec) in to_finalize {
            {
                let logs = ctx.voting_logs.lock().await;
                if let Some(log_mtx) = logs.get(&cid) {
                    let mut log = log_mtx.lock().await;
                    if let Some(state) = log.polls.get_mut(&pid) {
                        state.meta.lifecycle = Lifecycle::Finalized;
                        stats.tier2_proposals_finalized += 1;
                    }
                }
            }
            (ctx.emit)(
                "voting-proposal-finalized",
                serde_json::json!({
                    "communityId": hex::encode(cid.0),
                    "proposalId": hex::encode(pid.0),
                }),
            );
            match &auto_exec {
                AutoExecAction::None => {}
                AutoExecAction::SetPower {
                    target_pubkey,
                    new_power,
                } => {
                    stats.tier2_auto_execs_attempted += 1;
                    match (ctx.auto_exec_set_power)(cid, *target_pubkey, *new_power).await {
                        Ok(()) => stats.tier2_auto_execs_succeeded += 1,
                        Err(e) => {
                            tracing::warn!(
                                community = %hex::encode(cid.0),
                                proposal = %hex::encode(pid.0),
                                error = %e,
                                "auto_exec_set_power failed"
                            );
                        }
                    }
                }
            }
        }
    }

    // ── Pass 4: Daily archive sweep ─────────────────────────────────
    // At-most-once-per-24h: walk every VotingLog and call
    // archive_finalized_polls(now_ms). The per-log helper handles the
    // 90-day age check, lifecycle transition, and per-poll + top-level
    // event pruning.
    {
        let mut last_sweep = ctx.last_archive_sweep_ms.lock().await;
        if (now_ms - *last_sweep) >= ARCHIVE_SWEEP_INTERVAL_MS {
            let logs = ctx.voting_logs.lock().await;
            for log_mtx in logs.values() {
                let mut log = log_mtx.lock().await;
                let now_wall_ms_u64 = if now_ms < 0 { 0 } else { now_ms as u64 };
                let _archived = log.archive_finalized_polls(now_wall_ms_u64);
            }
            *last_sweep = now_ms;
            stats.archive_swept = true;
        }
    }

    Ok(stats)
}

/// Spawn the periodic tick task. Returns a JoinHandle; abort to stop.
pub fn spawn_voting_tick(
    ctx: VotingTickContext,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i128)
                .unwrap_or(0);
            if let Err(e) = run_voting_tick(&ctx, now_ms).await {
                tracing::warn!(error = %e, "voting_tick iteration failed (continuing)");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::ChannelId;
    use crate::community_voting_approval::{Tier1PollConfig, Tier1TallyState};
    use crate::community_voting_conviction::{Tier2PollConfig, Tier2ProposalState};
    use crate::community_voting_core::{Eligibility, PollMeta};
    use crate::community_voting_log::{PollState, TierState};
    use crate::owner_state_types::Hlc;

    fn empty_eligibility() -> Eligibility {
        Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        }
    }

    fn make_hlc(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn make_tier1_poll(
        community_id: SpaceId,
        poll_id: PollId,
        opens_at_ms: u64,
        closes_at_ms: u64,
        lifecycle: Lifecycle,
    ) -> PollState {
        let cfg = Tier1PollConfig {
            options: vec!["A".into(), "B".into()],
            window_seconds: ((closes_at_ms - opens_at_ms) / 1000) as u32,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: empty_eligibility(),
            channel_id: ChannelId([0x11; 16]),
        };
        let meta = PollMeta {
            poll_id,
            community_id,
            creator: OwnerAddr([0xaa; 16]),
            tier: Tier::Approval,
            eligibility: empty_eligibility(),
            lifecycle,
            created_at: make_hlc(opens_at_ms),
            opens_at: make_hlc(opens_at_ms),
            closes_at: make_hlc(closes_at_ms),
            extends_at: None,
            channel_id: Some(cfg.channel_id),
        };
        PollState {
            meta,
            events: vec![],
            tier_state: TierState::Tier1(Tier1TallyState::empty(cfg.options.len())),
            tier1_cfg: Some(cfg),
            tier1_snapshot: None,
        }
    }

    fn make_tier2_config(auto_exec: AutoExecAction) -> Tier2PollConfig {
        Tier2PollConfig {
            proposal_text: "test".into(),
            half_life_seconds: 86_400,
            // Threshold values share units with `charge_q32`'s return type
            // (raw ms after the `* hl_ms / LN2_Q32` step). At hl=7 days the
            // asymptotic per-voter conviction is ~6.05e8 (7d * 86_400_000 /
            // ln(2)). T_min=1e7, T_max=2e8 → easily reached by a single
            // supporting voter after a few half-lives.
            threshold_min_q32: 10_000_000,
            threshold_max_q32: 200_000_000,
            beta: 2,
            delegation_allowed: true,
            auto_exec,
            eligibility: empty_eligibility(),
        }
    }

    fn make_tier2_poll(
        community_id: SpaceId,
        poll_id: PollId,
        lifecycle: Lifecycle,
        proposal_state: Tier2ProposalState,
    ) -> PollState {
        let meta = PollMeta {
            poll_id,
            community_id,
            creator: OwnerAddr([0xaa; 16]),
            tier: Tier::Conviction,
            eligibility: empty_eligibility(),
            lifecycle,
            created_at: make_hlc(0),
            opens_at: make_hlc(0),
            closes_at: make_hlc(0),
            extends_at: None,
            channel_id: None,
        };
        PollState {
            meta,
            events: vec![],
            tier_state: TierState::Tier2(proposal_state),
            tier1_cfg: None,
            tier1_snapshot: None,
        }
    }

    /// Captured emit event for test assertions.
    type CapturedEvents = Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>;
    /// Captured auto-exec invocations for test assertions.
    type CapturedAutoExec = Arc<std::sync::Mutex<Vec<(SpaceId, OwnerAddr, u32)>>>;

    fn make_ctx_with_logs(
        logs: HashMap<SpaceId, Arc<Mutex<VotingLog>>>,
        last_sweep_ms: i128,
    ) -> (VotingTickContext, CapturedEvents, CapturedAutoExec) {
        let captured_events: CapturedEvents = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_auto_exec: CapturedAutoExec = Arc::new(std::sync::Mutex::new(Vec::new()));

        let captured_events_clone = Arc::clone(&captured_events);
        let emit: EmitFn = Arc::new(move |event_name: &str, payload: serde_json::Value| {
            captured_events_clone
                .lock()
                .unwrap()
                .push((event_name.to_string(), payload));
        });

        let captured_auto_exec_clone = Arc::clone(&captured_auto_exec);
        let auto_exec_set_power: AutoExecSetPowerFn =
            Arc::new(move |cid: SpaceId, target: OwnerAddr, power: u32| {
                let captured = Arc::clone(&captured_auto_exec_clone);
                Box::pin(async move {
                    captured.lock().unwrap().push((cid, target, power));
                    Ok(())
                })
            });

        let ctx = VotingTickContext {
            voting_logs: Arc::new(Mutex::new(logs)),
            last_archive_sweep_ms: Arc::new(Mutex::new(last_sweep_ms)),
            emit,
            auto_exec_set_power,
        };
        (ctx, captured_events, captured_auto_exec)
    }

    #[tokio::test]
    async fn community_voting_tick_tier1_auto_close_at_window_expiry() {
        let cid = SpaceId([0x11; 16]);
        let pid = PollId([0x22; 32]);
        let mut log = VotingLog::new();
        log.polls.insert(
            pid,
            make_tier1_poll(cid, pid, 1_000, 10_000, Lifecycle::Open),
        );

        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));
        // last_sweep set "now" so the archive pass is a no-op for this test.
        let now_ms = 20_000i128;
        let (ctx, events, _auto_exec) = make_ctx_with_logs(logs, now_ms);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier1_polls_closed, 1);

        let log_mtx = {
            let logs = ctx.voting_logs.lock().await;
            Arc::clone(logs.get(&cid).unwrap())
        };
        let log = log_mtx.lock().await;
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Closed);

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "voting-poll-closed");
    }

    #[tokio::test]
    async fn community_voting_tick_tier1_open_poll_not_closed_before_window() {
        let cid = SpaceId([0x11; 16]);
        let pid = PollId([0x22; 32]);
        let mut log = VotingLog::new();
        log.polls.insert(
            pid,
            make_tier1_poll(cid, pid, 1_000, 100_000, Lifecycle::Open),
        );

        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));
        let now_ms = 50_000i128;
        let (ctx, events, _) = make_ctx_with_logs(logs, now_ms);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier1_polls_closed, 0);

        let log_mtx = {
            let logs = ctx.voting_logs.lock().await;
            Arc::clone(logs.get(&cid).unwrap())
        };
        let log = log_mtx.lock().await;
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Open);
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn community_voting_tick_tier2_threshold_cross_sets_threshold_reached() {
        let cid = SpaceId([0x33; 16]);
        let pid = PollId([0x44; 32]);
        let cfg = make_tier2_config(AutoExecAction::None);
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        // Insert a supporting voter whose accumulated conviction is huge,
        // guaranteeing total >= threshold at now_ms.
        use crate::community_voting_conviction::VoterConvictionState;
        let voter = OwnerAddr([0xbb; 16]);
        let mut vs = VoterConvictionState::default();
        // Signal on at t=0; query at t = many half-lives → conviction ≈ Q32 + accumulated.
        vs.apply_signal(true, 0, 86_400_000);
        t2.per_voter.insert(voter, vs);

        let mut log = VotingLog::new();
        log.polls
            .insert(pid, make_tier2_poll(cid, pid, Lifecycle::Open, t2));

        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));
        let now_ms = 10 * 86_400_000i128; // 10 half-lives in.
        let (ctx, events, _) = make_ctx_with_logs(logs, now_ms);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier2_thresholds_reached, 1);

        let log_mtx = {
            let logs = ctx.voting_logs.lock().await;
            Arc::clone(logs.get(&cid).unwrap())
        };
        let log = log_mtx.lock().await;
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::ThresholdReached);
        let t2 = log.polls[&pid].tier_state.as_tier2().unwrap();
        assert_eq!(t2.threshold_reached_at_ms, Some(now_ms));

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "voting-threshold-reached");
    }

    #[tokio::test]
    async fn community_voting_tick_tier2_conviction_drop_reverts_to_open() {
        let cid = SpaceId([0x33; 16]);
        let pid = PollId([0x44; 32]);
        let cfg = make_tier2_config(AutoExecAction::None);
        // ThresholdReached state but per_voter is empty → total_conviction=0
        // → drops below threshold → revert.
        let mut t2 = Tier2ProposalState::new(cfg, 10);
        t2.threshold_reached_at_ms = Some(0);
        let mut log = VotingLog::new();
        log.polls.insert(
            pid,
            make_tier2_poll(cid, pid, Lifecycle::ThresholdReached, t2),
        );

        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));
        let now_ms = 1_000i128;
        let (ctx, _events, _) = make_ctx_with_logs(logs, now_ms);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier2_thresholds_reverted, 1);

        let log_mtx = {
            let logs = ctx.voting_logs.lock().await;
            Arc::clone(logs.get(&cid).unwrap())
        };
        let log = log_mtx.lock().await;
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Open);
        let t2 = log.polls[&pid].tier_state.as_tier2().unwrap();
        assert_eq!(t2.threshold_reached_at_ms, None);
        assert_eq!(t2.last_unsignal_after_threshold_ms, Some(now_ms));
    }

    #[tokio::test]
    async fn community_voting_tick_tier2_contestability_finalize_after_24h() {
        let cid = SpaceId([0x33; 16]);
        let pid = PollId([0x44; 32]);
        let cfg = make_tier2_config(AutoExecAction::None);
        // ThresholdReached + populated voter so threshold stays met.
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 86_400_000);
        t2.per_voter.insert(OwnerAddr([0xbb; 16]), vs);
        let reached_at = 1_000i128;
        t2.threshold_reached_at_ms = Some(reached_at);

        let mut log = VotingLog::new();
        log.polls.insert(
            pid,
            make_tier2_poll(cid, pid, Lifecycle::ThresholdReached, t2),
        );

        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));
        // 25h after reached_at.
        let now_ms = reached_at + 25 * 60 * 60 * 1000;
        let (ctx, events, _) = make_ctx_with_logs(logs, now_ms);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier2_proposals_finalized, 1);

        let log_mtx = {
            let logs = ctx.voting_logs.lock().await;
            Arc::clone(logs.get(&cid).unwrap())
        };
        let log = log_mtx.lock().await;
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::Finalized);

        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|(n, _)| n == "voting-proposal-finalized"),
            "expected voting-proposal-finalized event, got {:?}",
            events.iter().map(|(n, _)| n).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn community_voting_tick_tier2_contestability_not_finalize_within_24h() {
        let cid = SpaceId([0x33; 16]);
        let pid = PollId([0x44; 32]);
        let cfg = make_tier2_config(AutoExecAction::None);
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 86_400_000);
        t2.per_voter.insert(OwnerAddr([0xbb; 16]), vs);
        let reached_at = 1_000i128;
        t2.threshold_reached_at_ms = Some(reached_at);

        let mut log = VotingLog::new();
        log.polls.insert(
            pid,
            make_tier2_poll(cid, pid, Lifecycle::ThresholdReached, t2),
        );

        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));
        // Only 12h after reached_at.
        let now_ms = reached_at + 12 * 60 * 60 * 1000;
        let (ctx, _events, _) = make_ctx_with_logs(logs, now_ms);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier2_proposals_finalized, 0);

        let log_mtx = {
            let logs = ctx.voting_logs.lock().await;
            Arc::clone(logs.get(&cid).unwrap())
        };
        let log = log_mtx.lock().await;
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::ThresholdReached);
    }

    #[tokio::test]
    async fn community_voting_tick_tier2_auto_exec_set_power_invokes_callback() {
        let cid = SpaceId([0x33; 16]);
        let pid = PollId([0x44; 32]);
        let target = OwnerAddr([0xcc; 16]);
        let new_power = 50;
        let cfg = make_tier2_config(AutoExecAction::SetPower {
            target_pubkey: target,
            new_power,
        });
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 86_400_000);
        t2.per_voter.insert(OwnerAddr([0xbb; 16]), vs);
        let reached_at = 1_000i128;
        t2.threshold_reached_at_ms = Some(reached_at);

        let mut log = VotingLog::new();
        log.polls.insert(
            pid,
            make_tier2_poll(cid, pid, Lifecycle::ThresholdReached, t2),
        );

        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));
        let now_ms = reached_at + 25 * 60 * 60 * 1000;
        let (ctx, _events, auto_exec_calls) = make_ctx_with_logs(logs, now_ms);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier2_auto_execs_attempted, 1);
        assert_eq!(stats.tier2_auto_execs_succeeded, 1);

        let calls = auto_exec_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], (cid, target, new_power));
    }

    #[tokio::test]
    async fn community_voting_tick_archive_sweep_runs_after_24h() {
        let logs = HashMap::new();
        let now_ms = 30 * 60 * 60 * 1000i128; // 30h after epoch.
                                              // last_sweep = (now - 25h) so 25h has elapsed → sweep should run.
        let last_sweep = now_ms - 25 * 60 * 60 * 1000;
        let (ctx, _events, _) = make_ctx_with_logs(logs, last_sweep);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert!(stats.archive_swept);
        // last_sweep advanced to now_ms.
        assert_eq!(*ctx.last_archive_sweep_ms.lock().await, now_ms);
    }

    #[tokio::test]
    async fn community_voting_tick_archive_sweep_skipped_within_24h() {
        let logs = HashMap::new();
        let now_ms = 30 * 60 * 60 * 1000i128;
        let last_sweep = now_ms - 60 * 60 * 1000; // 1h ago.
        let (ctx, _events, _) = make_ctx_with_logs(logs, last_sweep);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert!(!stats.archive_swept);
        // last_sweep unchanged.
        assert_eq!(*ctx.last_archive_sweep_ms.lock().await, last_sweep);
    }

    #[tokio::test]
    async fn community_voting_tick_empty_voting_logs_no_panic() {
        let logs: HashMap<SpaceId, Arc<Mutex<VotingLog>>> = HashMap::new();
        let now_ms = 1_000i128;
        // last_sweep = now so archive pass is no-op.
        let (ctx, events, auto_exec) = make_ctx_with_logs(logs, now_ms);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier1_polls_closed, 0);
        assert_eq!(stats.tier2_thresholds_reached, 0);
        assert_eq!(stats.tier2_thresholds_reverted, 0);
        assert_eq!(stats.tier2_proposals_finalized, 0);
        assert_eq!(stats.tier2_auto_execs_attempted, 0);
        assert!(!stats.archive_swept);
        assert!(events.lock().unwrap().is_empty());
        assert!(auto_exec.lock().unwrap().is_empty());
    }
}
