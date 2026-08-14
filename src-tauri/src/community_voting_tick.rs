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

/// ZEB-720: parse a strictly-**positive** millisecond value from a raw env
/// string, falling back to `default` on absent / unparseable / non-positive
/// input. Used for the two voting-cadence overrides at node bringup:
/// `HARMONY_VOTING_CONTESTABILITY_WINDOW_MS` and `HARMONY_VOTING_TICK_INTERVAL_MS`.
/// The positive filter is load-bearing — a `0` tick interval would panic
/// `tokio::time::interval` ("period must be non-zero"), and a `0`/negative
/// window is meaningless. Pure (takes the raw `Option`) so bringup stays a
/// one-liner and this is unit-testable without mutating process env.
pub fn parse_positive_ms(raw: Option<String>, default: u64) -> u64 {
    raw.and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}
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
    /// ZEB-297: number of `AutoExecAction::SetPower` dispatches the
    /// auto-exec callback intentionally skipped because the local
    /// replica's actor is not an admin in the affected community.
    /// Distinct from `tier2_auto_execs_succeeded` (admin minted the
    /// event) and the implicit error-counted-by-warn path (callback
    /// returned `Err`). Each Tier 2 finalization with a SetPower auto-
    /// exec increments exactly one of these four on every replica.
    pub tier2_auto_execs_skipped_not_admin: u32,
    /// ZEB-734: number of `AutoExecAction::SetPower` dispatches skipped because
    /// the local actor clears the (possibly lowered) `set_power` threshold but
    /// lacks admin power (`max`) and the change is admin-affecting — a direct
    /// SetPower would self-reject and AdminProposal is unavailable (AP2). The
    /// replica defers to an admin. Tracked separately from
    /// `tier2_auto_execs_skipped_not_admin` (which counts actors below the
    /// `set_power` floor) so the two deferral reasons stay distinguishable.
    pub tier2_auto_execs_skipped_admin_affecting_requires_admin: u32,
    /// ZEB-300: number of `AutoExecAction::SetPower` dispatches on an
    /// admin-affecting change under `admin_quorum > 1` where this replica
    /// minted a fresh `AdminProposal::SetPower` (no prior live proposal for
    /// the exact target/level). Routes admin-tier changes through the
    /// AdminProposal quorum machinery per spec §4.5.
    pub tier2_auto_execs_routed_proposal_minted: u32,
    /// ZEB-300: number of admin-affecting dispatches where this replica
    /// countersigned the canonical pending `AdminProposal::SetPower`,
    /// advancing it toward `admin_quorum` signatures.
    pub tier2_auto_execs_routed_proposal_countersigned: u32,
    /// ZEB-300: number of admin-affecting dispatches where this replica had
    /// already signed the canonical proposal and is awaiting other admins'
    /// signatures. Steady state while the routing converges across ticks.
    /// May be bumped on repeated ticks while the poll is Finalized — that is
    /// expected for the re-dispatch loop.
    pub tier2_auto_execs_routed_proposal_pending: u32,
    /// ZEB-300 converge R1: number of dispatches that returned
    /// `AutoExecOutcome::AlreadyApplied` — the SetPower effect was already
    /// present in materialized state, so nothing was minted. Counted
    /// separately from `tier2_auto_execs_attempted` (this is an idempotent
    /// no-op re-dispatch, not an attempt to mint). Can accrue on repeated
    /// ticks while the poll is Finalized once the effect has landed — expected.
    pub tier2_auto_execs_already_applied: u32,
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
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::community_membership::AutoExecOutcome, String>,
                    > + Send,
            >,
        > + Send
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
    /// ZEB-718: identity_dir for persisting a pruned log after the archive
    /// sweep. `None` ⇒ no persistence (test/headless contexts).
    pub identity_dir: Option<std::path::PathBuf>,
    /// ZEB-720: contestability window before a ThresholdReached Tier-2 poll
    /// finalizes. Defaults to `CONTESTABILITY_WINDOW_MS` (24h) at every
    /// production bringup; overridable via `HARMONY_VOTING_CONTESTABILITY_WINDOW_MS`
    /// for deterministic e2e finalization. Read as pure config — the tick
    /// never reads the constant directly.
    pub contestability_window_ms: i128,
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
    // total_conviction dropped back below threshold → revert to Open
    // (emitting `voting-threshold-reverted` so the UI reflects it),
    // clearing `threshold_reached_at_ms` and recording the unsignal
    // wall-clock so the next finalize attempt resets the 24h timer.
    //
    // Conviction totals use the delegation-weighted variant so direct
    // delegators contribute their `(1 + delegator_count) * conviction`
    // weight per spec §5 — the unweighted `total_conviction_at` would
    // silently treat a voter with N delegators as weight 1.
    {
        let logs = ctx.voting_logs.lock().await;
        for (cid, log_mtx) in logs.iter() {
            let mut log = log_mtx.lock().await;
            // Snapshot the delegation graph by clone before borrowing
            // `log.polls` mutably; cheap (small HashMaps) and lets the
            // per-proposal loop pass a `&DelegationGraph` reference.
            let graph_snapshot = log.delegation_graph.clone();
            for (pid, state) in log.polls.iter_mut() {
                if state.meta.tier != Tier::Conviction {
                    continue;
                }
                let t2 = match state.tier_state.as_tier2_mut() {
                    Some(t) => t,
                    None => continue,
                };
                let total = t2.total_conviction_at_with_delegation(now_ms, &graph_snapshot);
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
                        (ctx.emit)(
                            "voting-threshold-reverted",
                            serde_json::json!({
                                "communityId": hex::encode(cid.0),
                                "proposalId": hex::encode(pid.0),
                                "revertedAtMs": now_ms,
                            }),
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    // ── Pass 3a: Tier 2 contestability finalize ─────────────────────
    // Walk Tier 2 polls in ThresholdReached lifecycle; if 24h elapsed
    // since `max(threshold_reached_at_ms, last_unsignal_after_threshold_ms)`
    // with no reversion, transition to Finalized (stamping
    // `meta.finalized_at_ms`) and emit `voting-proposal-finalized`.
    //
    // ZEB-300 converge R1: auto-exec dispatch moved OUT of this
    // finalize-transition block into Pass 3b, which re-dispatches for ALL
    // Finalized SetPower polls while they remain Finalized — not only on the
    // single tick a poll transitions to Finalized. This fixes the
    // simultaneous-finalize stall (two admins each mint an AdminProposal on
    // their finalize tick and neither ever countersigns the other's, so
    // quorum never accrues). The just-finalized poll is still dispatched this
    // same tick.
    {
        let mut to_finalize: Vec<(SpaceId, PollId)> = Vec::new();
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
                    if (now_ms - uncontested_since) >= ctx.contestability_window_ms {
                        to_finalize.push((*cid, *pid));
                    }
                }
            }
        }
        for (cid, pid) in to_finalize {
            // Re-validate lifecycle == ThresholdReached AND the 24h
            // contestability window before mutating. Two separable
            // invariants because Signal{false} during ThresholdReached
            // doesn't drop lifecycle (Pass 2's revert branch handles
            // total-conviction-below-threshold), but it DOES stamp
            // `last_unsignal_after_threshold_ms` which resets the
            // window per spec §5. Without the window re-check the
            // collect→mutate gap is TOCTOU-vulnerable (Cursor R8).
            //
            // Also stamp `meta.finalized_at_ms` so the archive sweep can
            // age this Tier 2 poll AND Pass 3b can anchor the auto-exec
            // retry window. Tier 1 uses its terminal PollResult event's HLC
            // for the same purpose; Tier 2 has no terminal event, so the
            // meta field is the only signal (CR R3 Major).
            let mut did_finalize = false;
            {
                let logs = ctx.voting_logs.lock().await;
                if let Some(log_mtx) = logs.get(&cid) {
                    let mut log = log_mtx.lock().await;
                    if let Some(state) = log.polls.get_mut(&pid) {
                        if state.meta.lifecycle == Lifecycle::ThresholdReached {
                            let window_still_clear =
                                state.tier_state.as_tier2().is_some_and(|t2| {
                                    t2.threshold_reached_at_ms.is_some_and(|reached_at| {
                                        let uncontested_since = reached_at.max(
                                            t2.last_unsignal_after_threshold_ms
                                                .unwrap_or(reached_at),
                                        );
                                        (now_ms - uncontested_since) >= ctx.contestability_window_ms
                                    })
                                });
                            if window_still_clear {
                                state.meta.lifecycle = Lifecycle::Finalized;
                                let stamp = if now_ms < 0 { 0 } else { now_ms as u64 };
                                state.meta.finalized_at_ms = Some(stamp);
                                stats.tier2_proposals_finalized += 1;
                                did_finalize = true;
                            }
                        }
                    }
                }
            }
            if !did_finalize {
                continue;
            }
            (ctx.emit)(
                "voting-proposal-finalized",
                serde_json::json!({
                    "communityId": hex::encode(cid.0),
                    "proposalId": hex::encode(pid.0),
                }),
            );
        }
    }

    // ── Pass 3b: Tier 2 SetPower auto-exec (re-dispatch while Finalized) ─
    // ZEB-300 converge R2 (Greptile #2): collect every Tier 2 poll that is
    // Finalized and carries an `AutoExecAction::SetPower`, then dispatch
    // auto-exec for each. This runs every tick (not only at the finalize
    // transition), so the AdminProposal-routing quorum accumulates across
    // admins' ticks even when two admins finalize simultaneously. Re-dispatch
    // continues for the poll's entire Finalized lifetime — there is NO fixed
    // clock cutoff (the old fixed 1h retry window was shorter than a poll's
    // actionable lifetime, so an AdminProposal that synced to a needed signer
    // after 1h would never get auto-countersigned). The natural bound
    // is the daily archive sweep (Pass 4): it flips the poll to `Archived`
    // ~24h after finalize, ending re-dispatch via the `lifecycle == Finalized`
    // filter below. For admin absences longer than that ~24h Finalized
    // lifetime, recovery is via the manual `countersign_admin_proposal` path
    // (the routed `AdminProposal` persists `ADMIN_PROPOSAL_EXPIRY_MS` = 30
    // days). The just-finalized poll is included here, so single-tick
    // finalize-then-dispatch still holds.
    //
    // Idempotency has two layers. (1) Per-target (ZEB-936): the collection
    // below dedups to the single canonical (newest-created) SetPower poll
    // per target, so conflicting finalized polls for the same member never
    // re-dispatch against each other. (2) Per-poll: once the canonical poll's
    // effect lands in materialized state the helper returns `AlreadyApplied`
    // (both the direct-SetPower and AdminProposal-routed paths guard on
    // `power_levels[target] == level`), so re-dispatch never re-mints after the
    // effect syncs in on a replica. Layer (1) is load-bearing: the per-poll
    // guard alone cannot stop a per-target conflict (two polls with different
    // frozen levels each see the other's value as a mismatch and re-mint).
    //
    // Stat behavior (acceptable): `Pending`/`SkippedNotAdmin` may be bumped
    // on repeated ticks while the poll is Finalized — expected for a
    // re-dispatch loop.
    // `RoutedProposalMinted`/`RoutedProposalCountersigned` each happen at
    // most once per replica per proposal (the planner then returns
    // `Pending`), so they don't inflate. `AlreadyApplied` is counted
    // separately from `tier2_auto_execs_attempted` (an idempotent no-op
    // check is not a mint attempt).
    //
    // Auto-exec runs AFTER releasing the voting_logs lock so the auto-exec
    // helper (which itself takes NodeState locks) cannot deadlock against
    // the tick's outer lock. Canonical selection is therefore a snapshot taken
    // under that lock: if a newer same-target poll finalizes in the window
    // between the snapshot and dispatch, the just-superseded poll can mint once
    // and briefly win by LWW, but the next tick re-selects the now-canonical
    // poll and converges — a bounded one-tick transient, not a persistent
    // divergence (and strictly rarer than the pre-ZEB-936 behavior, where every
    // conflicting finalized poll dispatched on every tick).
    {
        let mut to_dispatch: Vec<(SpaceId, PollId, OwnerAddr, u32)> = Vec::new();
        {
            let logs = ctx.voting_logs.lock().await;
            // ZEB-936: dedup finalized SetPower polls to the CANONICAL
            // (newest-created, tiebroken by poll_id) poll per
            // (community, target). Without this, Pass 3b re-dispatches EVERY
            // finalized SetPower poll every tick, so two finalized polls that
            // disagree on a member's level overwrite each other every tick
            // (materialize is LWW, and the per-poll `already_at_level` guard
            // in `apply_auto_exec_set_power` cannot stop a per-TARGET conflict),
            // minting a fresh CRDT SetPower event per poll per tick — the
            // ~49k-event / ~12 MB state-root storm the fleet measured (ZEB-933).
            // Keeping only the newest-created poll matches materialize's LWW
            // semantics and still re-dispatches that one canonical poll every
            // tick, preserving the quorum>1 countersign accumulation the
            // per-tick re-dispatch exists for.
            let mut canonical: std::collections::HashMap<
                (SpaceId, OwnerAddr),
                (crate::owner_state_types::Hlc, PollId, u32),
            > = std::collections::HashMap::new();
            for (cid, log_mtx) in logs.iter() {
                let log = log_mtx.lock().await;
                for (pid, state) in log.polls.iter() {
                    if state.meta.tier != Tier::Conviction {
                        continue;
                    }
                    if state.meta.lifecycle != Lifecycle::Finalized {
                        continue;
                    }
                    // Defensive: a Finalized poll should always carry a
                    // finalized_at_ms stamp (Pass 3a stamps it at the
                    // finalize transition). Skip a stampless Finalized poll
                    // rather than dispatch on a malformed one. No clock gate:
                    // re-dispatch runs for the whole Finalized lifetime,
                    // bounded by the archive sweep (see Pass 3b header).
                    if state.meta.finalized_at_ms.is_none() {
                        continue;
                    }
                    let auto_exec = match state
                        .tier_state
                        .as_tier2()
                        .map(|t2| t2.config.auto_exec.clone())
                    {
                        Some(a) => a,
                        None => continue,
                    };
                    if let AutoExecAction::SetPower {
                        target_pubkey,
                        new_power,
                    } = auto_exec
                    {
                        // Canonical = newest by the poll's CREATED_AT hlc.
                        // created_at comes from the replicated PollCreate event,
                        // so it is byte-identical on every replica — unlike
                        // finalized_at_ms, which each replica stamps locally at
                        // its own finalize tick and would diverge, letting two
                        // admins pick different canonical polls and re-introduce
                        // a (milder) cross-replica thrash. Tiebreak by poll_id so
                        // equal-hlc polls still resolve identically everywhere.
                        let key = (*cid, target_pubkey);
                        let cand = (state.meta.created_at.clone(), *pid, new_power);
                        match canonical.get_mut(&key) {
                            Some(winner) => {
                                if (&cand.0, &(cand.1).0) > (&winner.0, &(winner.1).0) {
                                    *winner = cand;
                                }
                            }
                            None => {
                                canonical.insert(key, cand);
                            }
                        }
                    }
                }
            }
            for ((cid, target_pubkey), (_created_at, pid, new_power)) in canonical {
                to_dispatch.push((cid, pid, target_pubkey, new_power));
            }
        }
        for (cid, pid, target_pubkey, new_power) in to_dispatch {
            match (ctx.auto_exec_set_power)(cid, target_pubkey, new_power).await {
                Ok(crate::community_membership::AutoExecOutcome::AlreadyApplied) => {
                    // ZEB-300 converge R1: idempotent no-op — the effect is
                    // already present in materialized state. NOT counted as
                    // an attempt (nothing was minted); this is the
                    // re-dispatch stop condition.
                    stats.tier2_auto_execs_already_applied += 1;
                }
                Ok(crate::community_membership::AutoExecOutcome::Applied) => {
                    stats.tier2_auto_execs_attempted += 1;
                    stats.tier2_auto_execs_succeeded += 1;
                }
                Ok(crate::community_membership::AutoExecOutcome::SkippedNotAdmin) => {
                    // ZEB-297: this replica's local actor is not admin in the
                    // community, so the mint would self-reject. Admins race
                    // to mint; HLC LWW dedupes; the first admin's SetPower
                    // propagates here via the existing membership log sync.
                    stats.tier2_auto_execs_attempted += 1;
                    stats.tier2_auto_execs_skipped_not_admin += 1;
                }
                Ok(
                    crate::community_membership::AutoExecOutcome::SkippedAdminAffectingRequiresAdmin,
                ) => {
                    // ZEB-734: local actor clears the (possibly lowered)
                    // set_power threshold but lacks admin power; an
                    // admin-affecting SetPower can only be executed directly by
                    // an admin (and AdminProposal needs proposer power == max),
                    // so this replica defers to an admin replica.
                    stats.tier2_auto_execs_attempted += 1;
                    stats.tier2_auto_execs_skipped_admin_affecting_requires_admin += 1;
                }
                Ok(crate::community_membership::AutoExecOutcome::RoutedProposalMinted) => {
                    // ZEB-300: admin_quorum > 1 + admin-affecting; this
                    // replica minted a fresh AdminProposal to route the
                    // change through quorum.
                    stats.tier2_auto_execs_attempted += 1;
                    stats.tier2_auto_execs_routed_proposal_minted += 1;
                }
                Ok(crate::community_membership::AutoExecOutcome::RoutedProposalCountersigned) => {
                    // ZEB-300: this replica countersigned the canonical
                    // pending AdminProposal, advancing it toward admin_quorum.
                    stats.tier2_auto_execs_attempted += 1;
                    stats.tier2_auto_execs_routed_proposal_countersigned += 1;
                }
                Ok(crate::community_membership::AutoExecOutcome::RoutedProposalPending) => {
                    // ZEB-300: this replica already signed the canonical
                    // proposal and is awaiting other admins' signatures.
                    stats.tier2_auto_execs_attempted += 1;
                    stats.tier2_auto_execs_routed_proposal_pending += 1;
                }
                Err(e) => {
                    stats.tier2_auto_execs_attempted += 1;
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

    // ── Pass 4: Daily archive sweep ─────────────────────────────────
    // At-most-once-per-24h: walk every VotingLog and call
    // archive_finalized_polls(now_ms). The per-log helper handles the
    // 90-day age check, lifecycle transition, and per-poll + top-level
    // event pruning.
    {
        let mut last_sweep = ctx.last_archive_sweep_ms.lock().await;
        if (now_ms - *last_sweep) >= ARCHIVE_SWEEP_INTERVAL_MS {
            let now_wall_ms_u64 = if now_ms < 0 { 0 } else { now_ms as u64 };
            // Snapshot the map's Arcs, then drop the global map lock BEFORE
            // mutating individual logs or touching disk — holding
            // `voting_logs` across N disk writes would block every
            // community's voting for the duration of the sweep. Each log has
            // its own mutex, so cloned Arcs stay valid after the map unlocks.
            let entries: Vec<(SpaceId, std::sync::Arc<tokio::sync::Mutex<VotingLog>>)> = {
                let logs = ctx.voting_logs.lock().await;
                logs.iter().map(|(k, v)| (*k, v.clone())).collect()
            };
            for (space_id, log_mtx) in entries {
                let mut log = log_mtx.lock().await;
                let archived = log.archive_finalized_polls(now_wall_ms_u64);
                // ZEB-718: persist the pruned log so archived events don't
                // resurrect on reload and the on-disk file stays bounded.
                if !archived.is_empty() {
                    if let Some(dir) = ctx.identity_dir.as_ref() {
                        let path = crate::community_voting_persist::voting_path_for(dir, &space_id);
                        let snapshot =
                            crate::community_voting_persist::snapshot_for_persist(&log, &space_id);
                        // Hold this per-community log lock across the write so
                        // the sweep serializes with the engine's `persist_now`
                        // (which shares this same mutex) — preventing a
                        // temp-file race on `voting.cbor`. `spawn_blocking`
                        // keeps the blocking `std::fs` write off the async
                        // worker; the sweep is 24h-cadence so the hold is a
                        // non-issue.
                        let write_result = tokio::task::spawn_blocking(move || {
                            crate::community_voting_persist::write_snapshot(&path, &snapshot)
                        })
                        .await;
                        match write_result {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(community_id = ?space_id, err = %e, "voting archive persist failed")
                            }
                            Err(join_err) => {
                                tracing::warn!(community_id = ?space_id, err = %join_err, "voting archive persist task panicked")
                            }
                        }
                    }
                }
                drop(log);
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
            // SystemTime::now() returning Err means the system clock is
            // before UNIX_EPOCH. We MUST NOT proceed with `now_ms = 0`:
            // that would close every Tier 1 poll (any positive
            // `closes_at` <= 0) and treat every Tier 2 charge interval
            // as negative. Skip this iteration and warn instead.
            let now_ms = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => d.as_millis() as i128,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "voting_tick: SystemTime before UNIX_EPOCH; skipping iteration"
                    );
                    continue;
                }
            };
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
            finalized_at_ms: None,
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
            finalized_at_ms: None,
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
                    Ok(crate::community_membership::AutoExecOutcome::Applied)
                })
            });

        let ctx = VotingTickContext {
            voting_logs: Arc::new(Mutex::new(logs)),
            last_archive_sweep_ms: Arc::new(Mutex::new(last_sweep_ms)),
            emit,
            auto_exec_set_power,
            identity_dir: None,
            contestability_window_ms: CONTESTABILITY_WINDOW_MS,
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
        vs.apply_signal(true, 0, 0, 86_400_000);
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
        vs.apply_signal(true, 0, 0, 86_400_000);
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

    // ZEB-720: finalization is gated on `ctx.contestability_window_ms`, not the
    // bare constant. Same known-good fixture as the 24h test (a well-charged
    // supporting voter, ticked at +25h where conviction stays above threshold);
    // only the window differs between the two cases, so the window value is
    // provably the deciding factor — in BOTH directions. This is the mechanism
    // the e2e scenario relies on to finalize without a real 24h wait.
    #[tokio::test]
    async fn tier2_finalize_respects_ctx_contestability_window() {
        let cid = SpaceId([0x33; 16]);
        let pid = PollId([0x44; 32]);
        let reached_at = 1_000i128;
        let now_ms = reached_at + 25 * 60 * 60 * 1000; // +25h, like the 24h test

        let build = || {
            let cfg = make_tier2_config(AutoExecAction::None);
            let mut t2 = Tier2ProposalState::new(cfg, 1);
            let mut vs = crate::community_voting_conviction::VoterConvictionState::default();
            vs.apply_signal(true, 0, 0, 86_400_000);
            t2.per_voter.insert(OwnerAddr([0xbb; 16]), vs);
            t2.threshold_reached_at_ms = Some(reached_at);
            let mut log = VotingLog::new();
            log.polls.insert(
                pid,
                make_tier2_poll(cid, pid, Lifecycle::ThresholdReached, t2),
            );
            let mut logs = HashMap::new();
            logs.insert(cid, Arc::new(Mutex::new(log)));
            logs
        };

        // Short window (2s): +25h clears it → finalizes.
        let (mut ctx_short, _e, _a) = make_ctx_with_logs(build(), reached_at);
        ctx_short.contestability_window_ms = 2_000;
        let s = run_voting_tick(&ctx_short, now_ms).await.unwrap();
        assert_eq!(
            s.tier2_proposals_finalized, 1,
            "short window: +25h finalizes"
        );

        // Long window (30h > 25h): same now, NOT yet finalized.
        let (mut ctx_long, _e, _a) = make_ctx_with_logs(build(), reached_at);
        ctx_long.contestability_window_ms = 30 * 60 * 60 * 1000;
        let s = run_voting_tick(&ctx_long, now_ms).await.unwrap();
        assert_eq!(
            s.tier2_proposals_finalized, 0,
            "30h window: +25h is too soon"
        );
    }

    // ZEB-720: the shared bringup parser falls back to the default on absent,
    // unparseable, or non-positive input (the non-positive case guards a `0`
    // tick interval from panicking tokio's timer), and honors a valid positive
    // override. Tests the actual production helper, not a copy of its logic.
    #[test]
    fn parse_positive_ms_falls_back_on_absent_bad_or_nonpositive() {
        assert_eq!(parse_positive_ms(None, 24), 24);
        assert_eq!(parse_positive_ms(Some("not-a-number".into()), 24), 24);
        assert_eq!(parse_positive_ms(Some("0".into()), 24), 24);
        assert_eq!(parse_positive_ms(Some("-5".into()), 24), 24);
        assert_eq!(parse_positive_ms(Some("2000".into()), 24), 2000);
    }

    #[tokio::test]
    async fn community_voting_tick_tier2_contestability_not_finalize_within_24h() {
        let cid = SpaceId([0x33; 16]);
        let pid = PollId([0x44; 32]);
        let cfg = make_tier2_config(AutoExecAction::None);
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 0, 86_400_000);
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

    /// Regression: Pass 3 mutation phase must re-validate the 24h
    /// contestability window, not just lifecycle == ThresholdReached.
    /// Without the re-check, a concurrent Signal{false} that stamped
    /// `last_unsignal_after_threshold_ms` between candidate collection
    /// and mutation would slip through (Cursor R8 TOCTOU).
    ///
    /// Simulating the race directly via two tasks is brittle; instead
    /// we pre-stamp `last_unsignal_after_threshold_ms` to exactly the
    /// boundary state that a concurrent Signal{false} would have left
    /// (window-just-reset) and assert the tick does not finalize.
    /// This is the same observable state the mutation phase would see
    /// on lock re-acquisition.
    #[tokio::test]
    async fn community_voting_tick_tier2_contestability_recheck_skips_after_window_reset() {
        let cid = SpaceId([0x33; 16]);
        let pid = PollId([0x44; 32]);
        let cfg = make_tier2_config(AutoExecAction::None);
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 0, 86_400_000);
        t2.per_voter.insert(OwnerAddr([0xbb; 16]), vs);
        let reached_at = 1_000i128;
        t2.threshold_reached_at_ms = Some(reached_at);
        let now_ms = reached_at + 25 * 60 * 60 * 1000;
        // Simulate a concurrent Signal{false} that landed just before
        // the mutation phase re-acquired the lock: window reset to
        // ~now, so 24h has NOT elapsed since the reset.
        t2.last_unsignal_after_threshold_ms = Some(now_ms - 60 * 1000);

        let mut log = VotingLog::new();
        log.polls.insert(
            pid,
            make_tier2_poll(cid, pid, Lifecycle::ThresholdReached, t2),
        );
        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));
        let (ctx, events, _) = make_ctx_with_logs(logs, now_ms);

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        // Pass 3 first-phase scan still uses the original tier2 state
        // (no concurrent IPC actually ran), so the proposal IS in the
        // to_finalize candidate list. The mutation-phase re-check is
        // what must reject it because of the reset window. The end
        // observable: no finalization, no event.
        assert_eq!(
            stats.tier2_proposals_finalized, 0,
            "mutation phase must re-validate contestability window, not just lifecycle"
        );
        let log_mtx = {
            let logs = ctx.voting_logs.lock().await;
            Arc::clone(logs.get(&cid).unwrap())
        };
        let log = log_mtx.lock().await;
        assert_eq!(log.polls[&pid].meta.lifecycle, Lifecycle::ThresholdReached);
        assert!(log.polls[&pid].meta.finalized_at_ms.is_none());
        assert!(
            !events
                .lock()
                .unwrap()
                .iter()
                .any(|(n, _)| n == "voting-proposal-finalized"),
            "no finalize event should fire when window reset between phases"
        );
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
        vs.apply_signal(true, 0, 0, 86_400_000);
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

    /// ZEB-936 regression: Pass 3b must dedup conflicting finalized SetPower
    /// polls for the same target down to the canonical (newest-created) one.
    ///
    /// Before the fix, `to_dispatch` collected EVERY finalized SetPower poll and
    /// re-dispatched each every tick. In a small community every setPower targets
    /// the same member, so two finalized polls at DIFFERENT levels overwrite each
    /// other every tick — each overwrite minting a fresh CRDT SetPower event. Over
    /// a poll's ~24h Finalized lifetime at a 250ms tick that is the ~49k-event /
    /// ~12 MB state-root storm the fleet measured (ZEB-933). The per-poll
    /// `already_at_level` guard cannot stop it because the invariant it needs is
    /// per-TARGET, not per-poll.
    ///
    /// The `auto_exec_set_power` closure below faithfully models
    /// `apply_auto_exec_set_power`'s guard (community_membership.rs:6403-6408):
    /// mint (→ one CRDT event) only when the target's current materialized power
    /// (LWW) differs from this poll's frozen level; otherwise `AlreadyApplied`.
    /// `mints` therefore counts the real CRDT SetPower events that would be
    /// appended to the state log.
    #[tokio::test]
    async fn tier2_setpower_redispatch_dedups_conflicting_same_target_polls() {
        use std::collections::HashMap as StdHashMap;
        use std::sync::Mutex as StdMutex;

        let cid = SpaceId([0x77; 16]);
        let target = OwnerAddr([0xcc; 16]);

        // Two finalized SetPower polls, SAME target, DIFFERENT levels. Canonical
        // selection is by the replicated created_at hlc, tiebroken by poll_id.
        // The setup makes created_at the ONLY key that picks the right winner:
        // the newer-created poll (level 52) has the SMALLER poll_id AND the
        // EARLIER finalized_at_ms, so a fix that keyed on poll_id or on the
        // locally-stamped finalized_at_ms would pick the wrong poll and fail.
        let mk = |pid: PollId, level: u32, created_ms: u64, finalized_ms: u64| {
            let cfg = make_tier2_config(AutoExecAction::SetPower {
                target_pubkey: target,
                new_power: level,
            });
            let t2 = Tier2ProposalState::new(cfg, 1);
            let mut poll = make_tier2_poll(cid, pid, Lifecycle::Finalized, t2);
            poll.meta.created_at = make_hlc(created_ms);
            poll.meta.finalized_at_ms = Some(finalized_ms);
            poll
        };
        // Older decision: EARLIER created_at, larger poll_id, LATER finalize.
        let pid_old = PollId([0x02; 32]);
        // Canonical (newer decision): LATER created_at, smaller poll_id, earlier
        // finalize.
        let pid_new = PollId([0x01; 32]);
        let mut log = VotingLog::new();
        log.polls.insert(pid_old, mk(pid_old, 51, 1_000, 2_500));
        log.polls.insert(pid_new, mk(pid_new, 52, 2_000, 2_200));

        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));
        // last_sweep == now so Pass 4 never archives the polls out from under us.
        let now_ms = 3_000i128;
        let (mut ctx, _events, _ae) = make_ctx_with_logs(logs, now_ms);

        // Faithful model of the `already_at_level` guard: `mints` == real CRDT
        // SetPower events that would be appended; `power` is the materialized LWW
        // power a fresh dispatch reads.
        let power: Arc<StdMutex<StdHashMap<OwnerAddr, u32>>> =
            Arc::new(StdMutex::new(StdHashMap::new()));
        let mints: Arc<StdMutex<usize>> = Arc::new(StdMutex::new(0));
        let power_probe = Arc::clone(&power);
        let mints_probe = Arc::clone(&mints);
        ctx.auto_exec_set_power = Arc::new(move |_cid, tgt, pw| {
            let power = Arc::clone(&power);
            let mints = Arc::clone(&mints);
            Box::pin(async move {
                if power.lock().unwrap().get(&tgt).copied() == Some(pw) {
                    return Ok(crate::community_membership::AutoExecOutcome::AlreadyApplied);
                }
                power.lock().unwrap().insert(tgt, pw);
                *mints.lock().unwrap() += 1;
                Ok(crate::community_membership::AutoExecOutcome::Applied)
            })
        });

        // 30 ticks of re-dispatch (each a real 250 ms tick in production).
        for _ in 0..30 {
            run_voting_tick(&ctx, now_ms).await.unwrap();
        }

        let mint_count = *mints_probe.lock().unwrap();
        assert_eq!(
            mint_count, 1,
            "conflicting same-target finalized SetPower polls must converge to \
             ONE applied mint (canonical newest-created wins), not re-mint every \
             tick; got {mint_count}"
        );
        assert_eq!(
            power_probe.lock().unwrap().get(&target).copied(),
            Some(52),
            "the surviving power must be the canonical (newest-created) poll's level"
        );
    }

    /// ZEB-936 tie-break: when two same-target finalized SetPower polls share a
    /// created_at hlc, the poll_id tie-break must pick the SAME poll on every
    /// replica (the larger poll_id) so selection stays deterministic. Sibling of
    /// `tier2_setpower_redispatch_dedups_conflicting_same_target_polls` (which
    /// covers the primary created_at ordering); without the poll_id tie-break
    /// this case would resolve by arbitrary HashMap order and diverge.
    #[tokio::test]
    async fn tier2_setpower_redispatch_tiebreaks_equal_created_at_by_poll_id() {
        use std::collections::HashMap as StdHashMap;
        use std::sync::Mutex as StdMutex;

        let cid = SpaceId([0x77; 16]);
        let target = OwnerAddr([0xcc; 16]);

        // Equal created_at → the poll_id tie-break decides; the larger poll_id
        // (pid_hi, level 52) is canonical, so its level must survive.
        let mk = |pid: PollId, level: u32| {
            let cfg = make_tier2_config(AutoExecAction::SetPower {
                target_pubkey: target,
                new_power: level,
            });
            let t2 = Tier2ProposalState::new(cfg, 1);
            let mut poll = make_tier2_poll(cid, pid, Lifecycle::Finalized, t2);
            poll.meta.created_at = make_hlc(1_000);
            poll.meta.finalized_at_ms = Some(1_500);
            poll
        };
        let pid_lo = PollId([0x01; 32]);
        let pid_hi = PollId([0x02; 32]);
        let mut log = VotingLog::new();
        log.polls.insert(pid_lo, mk(pid_lo, 51));
        log.polls.insert(pid_hi, mk(pid_hi, 52));

        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));
        let now_ms = 3_000i128;
        let (mut ctx, _events, _ae) = make_ctx_with_logs(logs, now_ms);

        let power: Arc<StdMutex<StdHashMap<OwnerAddr, u32>>> =
            Arc::new(StdMutex::new(StdHashMap::new()));
        let mints: Arc<StdMutex<usize>> = Arc::new(StdMutex::new(0));
        let power_probe = Arc::clone(&power);
        let mints_probe = Arc::clone(&mints);
        ctx.auto_exec_set_power = Arc::new(move |_cid, tgt, pw| {
            let power = Arc::clone(&power);
            let mints = Arc::clone(&mints);
            Box::pin(async move {
                if power.lock().unwrap().get(&tgt).copied() == Some(pw) {
                    return Ok(crate::community_membership::AutoExecOutcome::AlreadyApplied);
                }
                power.lock().unwrap().insert(tgt, pw);
                *mints.lock().unwrap() += 1;
                Ok(crate::community_membership::AutoExecOutcome::Applied)
            })
        });

        for _ in 0..30 {
            run_voting_tick(&ctx, now_ms).await.unwrap();
        }

        assert_eq!(
            *mints_probe.lock().unwrap(),
            1,
            "equal-created_at conflicting polls must still converge to ONE mint via the poll_id tie-break"
        );
        assert_eq!(
            power_probe.lock().unwrap().get(&target).copied(),
            Some(52),
            "the larger poll_id must win the tie-break"
        );
    }

    /// ZEB-297: when the auto-exec callback returns `SkippedNotAdmin`
    /// (the local actor isn't admin in this community), the tick must
    /// increment `tier2_auto_execs_skipped_not_admin` rather than
    /// `tier2_auto_execs_succeeded`. Without this branching, the stats
    /// would lie about how often the dispatch actually mutated state —
    /// every non-admin replica's tick would falsely count as a success.
    #[tokio::test]
    async fn community_voting_tick_tier2_auto_exec_set_power_skipped_when_non_admin() {
        let cid = SpaceId([0x55; 16]);
        let pid = PollId([0x66; 32]);
        let target = OwnerAddr([0xcc; 16]);
        let new_power = 50;
        let cfg = make_tier2_config(AutoExecAction::SetPower {
            target_pubkey: target,
            new_power,
        });
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 0, 86_400_000);
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
        let (mut ctx, _events, _auto_exec_calls) = make_ctx_with_logs(logs, now_ms);

        // Override the captured callback to simulate a non-admin
        // replica: return SkippedNotAdmin instead of Applied.
        ctx.auto_exec_set_power = Arc::new(|_cid, _target, _power| {
            Box::pin(async { Ok(crate::community_membership::AutoExecOutcome::SkippedNotAdmin) })
        });

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier2_proposals_finalized, 1);
        assert_eq!(stats.tier2_auto_execs_attempted, 1);
        assert_eq!(
            stats.tier2_auto_execs_succeeded, 0,
            "skip path must NOT bump the success counter"
        );
        assert_eq!(
            stats.tier2_auto_execs_skipped_not_admin, 1,
            "skip path must bump the dedicated skip counter exactly once"
        );
    }

    /// ZEB-734: when the auto-exec callback returns
    /// `SkippedAdminAffectingRequiresAdmin` (the local actor clears the
    /// possibly-lowered `set_power` threshold but lacks admin power for an
    /// admin-affecting change), the tick must bump the dedicated
    /// `tier2_auto_execs_skipped_admin_affecting_requires_admin` counter — NOT
    /// `skipped_not_admin` (the below-`set_power`-floor case) or `succeeded`.
    /// Keeps the two deferral reasons distinguishable in telemetry.
    #[tokio::test]
    async fn community_voting_tick_tier2_auto_exec_set_power_skipped_admin_affecting_requires_admin(
    ) {
        let cid = SpaceId([0x55; 16]);
        let pid = PollId([0x66; 32]);
        let target = OwnerAddr([0xcc; 16]);
        let new_power = 100; // admin-affecting (promotion to admin)
        let cfg = make_tier2_config(AutoExecAction::SetPower {
            target_pubkey: target,
            new_power,
        });
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 0, 86_400_000);
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
        let (mut ctx, _events, _auto_exec_calls) = make_ctx_with_logs(logs, now_ms);

        // Simulate a sub-admin moderator replica: clears set_power but lacks
        // admin power for an admin-affecting change → defers to an admin race.
        ctx.auto_exec_set_power = Arc::new(|_cid, _target, _power| {
            Box::pin(async {
                Ok(crate::community_membership::AutoExecOutcome::SkippedAdminAffectingRequiresAdmin)
            })
        });

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier2_proposals_finalized, 1);
        assert_eq!(stats.tier2_auto_execs_attempted, 1);
        assert_eq!(
            stats.tier2_auto_execs_succeeded, 0,
            "deferral must NOT bump the success counter"
        );
        assert_eq!(
            stats.tier2_auto_execs_skipped_not_admin, 0,
            "ZEB-734 deferral must NOT be conflated with the below-set_power skip"
        );
        assert_eq!(
            stats.tier2_auto_execs_skipped_admin_affecting_requires_admin, 1,
            "ZEB-734 deferral must bump its dedicated counter exactly once"
        );
    }

    /// ZEB-300: when the auto-exec callback returns `RoutedProposalMinted`
    /// (community has `admin_quorum > 1` and the outcome is admin-affecting,
    /// so this replica minted an `AdminProposal::SetPower`), the tick must
    /// increment `tier2_auto_execs_routed_proposal_minted` rather than
    /// `tier2_auto_execs_succeeded` or `tier2_auto_execs_skipped_not_admin`.
    /// Pins the dispatch's branch so a future variant addition or
    /// counter-name rename can't silently re-route dispatches into the
    /// wrong bucket.
    #[tokio::test]
    async fn community_voting_tick_tier2_auto_exec_set_power_routes_to_proposal_when_quorum_blocks()
    {
        let cid = SpaceId([0x55; 16]);
        let pid = PollId([0x66; 32]);
        let target = OwnerAddr([0xcc; 16]);
        let new_power = 100; // admin-affecting (promotion)
        let cfg = make_tier2_config(AutoExecAction::SetPower {
            target_pubkey: target,
            new_power,
        });
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 0, 86_400_000);
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
        let (mut ctx, _events, _auto_exec_calls) = make_ctx_with_logs(logs, now_ms);

        // Override the captured callback to simulate an admin replica in a
        // multi-admin-quorum community that minted the routing proposal:
        // return RoutedProposalMinted instead of Applied. The real helper
        // does this via apply_auto_exec_admin_proposal_set_power at the
        // engine boundary.
        ctx.auto_exec_set_power = Arc::new(|_cid, _target, _power| {
            Box::pin(async {
                Ok(crate::community_membership::AutoExecOutcome::RoutedProposalMinted)
            })
        });

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier2_proposals_finalized, 1);
        assert_eq!(stats.tier2_auto_execs_attempted, 1);
        assert_eq!(
            stats.tier2_auto_execs_succeeded, 0,
            "routed path must NOT bump the success counter"
        );
        assert_eq!(
            stats.tier2_auto_execs_skipped_not_admin, 0,
            "routed path must NOT collide with the not-admin counter"
        );
        assert_eq!(
            stats.tier2_auto_execs_routed_proposal_minted, 1,
            "routed-mint path must bump the dedicated routed-minted counter exactly once"
        );
        assert_eq!(
            stats.tier2_auto_execs_routed_proposal_countersigned, 0,
            "routed-mint path must NOT bump the countersigned counter"
        );
        assert_eq!(
            stats.tier2_auto_execs_routed_proposal_pending, 0,
            "routed-mint path must NOT bump the pending counter"
        );
    }

    /// ZEB-297 R3 (CodeRabbit Nitpick): when the auto-exec callback
    /// returns `Err(_)` (e.g., NodeState handles missing, engine
    /// rejected the local insert), the tick logs a warning and
    /// continues — none of the three success/skip counters should
    /// bump. Pins the Err branch so a future refactor that re-routes
    /// failures into one of the skip buckets (and lies about how
    /// often auto-exec actually succeeded) fails loudly.
    #[tokio::test]
    async fn community_voting_tick_tier2_auto_exec_set_power_err_path_bumps_no_counters() {
        let cid = SpaceId([0x55; 16]);
        let pid = PollId([0x66; 32]);
        let target = OwnerAddr([0xcc; 16]);
        let new_power = 50;
        let cfg = make_tier2_config(AutoExecAction::SetPower {
            target_pubkey: target,
            new_power,
        });
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 0, 86_400_000);
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
        let (mut ctx, _events, _auto_exec_calls) = make_ctx_with_logs(logs, now_ms);

        // Override the captured callback to simulate a downstream
        // failure (e.g., engine rejected the insert, missing handles).
        ctx.auto_exec_set_power = Arc::new(|_cid, _target, _power| {
            Box::pin(async { Err("simulated apply_auto_exec_set_power failure".to_string()) })
        });

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(stats.tier2_proposals_finalized, 1);
        assert_eq!(stats.tier2_auto_execs_attempted, 1);
        assert_eq!(
            stats.tier2_auto_execs_succeeded, 0,
            "Err path must NOT bump the success counter"
        );
        assert_eq!(
            stats.tier2_auto_execs_skipped_not_admin, 0,
            "Err path must NOT bump the not-admin skip counter"
        );
        assert_eq!(
            stats.tier2_auto_execs_routed_proposal_minted, 0,
            "Err path must NOT bump the routed-minted counter"
        );
    }

    /// ZEB-300 converge R2 (Greptile #2): a Finalized SetPower poll must be
    /// re-dispatched on EVERY tick while it stays Finalized, not only on the
    /// tick it transitions to Finalized — and with NO fixed clock cutoff. The
    /// bound is the poll's Finalized lifetime (until the 24h archive sweep),
    /// NOT an arbitrary 1h window. This is the fix for the simultaneous-
    /// finalize stall (Qodo): if two admins each mint an AdminProposal at
    /// their finalize tick and the routing only ran once, neither would ever
    /// countersign the canonical proposal and quorum would stall forever. The
    /// second tick here lands far beyond the old 1h cutoff to prove it's gone.
    #[tokio::test]
    async fn tier2_auto_exec_redispatches_finalized_poll() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cid = SpaceId([0x77; 16]);
        let pid = PollId([0x88; 32]);
        let target = OwnerAddr([0xcc; 16]);
        let new_power = 100; // admin-affecting → the routed path this fix serves
        let cfg = make_tier2_config(AutoExecAction::SetPower {
            target_pubkey: target,
            new_power,
        });
        let mut t2 = Tier2ProposalState::new(cfg, 1);
        use crate::community_voting_conviction::VoterConvictionState;
        let mut vs = VoterConvictionState::default();
        vs.apply_signal(true, 0, 0, 86_400_000);
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

        let now_ms_1 = reached_at + 25 * 60 * 60 * 1000;
        // last_sweep = first tick's now so the archive pass never runs.
        let (mut ctx, _events, _auto_exec_calls) = make_ctx_with_logs(logs, now_ms_1);

        // Shared cross-tick dispatch counter. Always returns Applied (a mock —
        // it does NOT mutate the stored poll, so the poll stays Finalized and
        // is eligible for re-dispatch on the next tick).
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in_closure = Arc::clone(&calls);
        ctx.auto_exec_set_power = Arc::new(move |_cid, _target, _power| {
            let calls = Arc::clone(&calls_in_closure);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(crate::community_membership::AutoExecOutcome::Applied)
            })
        });

        // Tick 1: poll finalizes (window elapsed) → dispatched once (window 0).
        let stats1 = run_voting_tick(&ctx, now_ms_1).await.unwrap();
        assert_eq!(stats1.tier2_proposals_finalized, 1);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "finalize tick must dispatch exactly once"
        );

        // Tick 2: poll is already Finalized; `now` is FAR beyond the old 1h
        // cutoff (+5h) — re-dispatch must still happen, bounded only by the
        // Finalized lifecycle (the archive sweep, 24h away, has not run).
        let now_ms_2 = now_ms_1 + 5 * 60 * 60 * 1000; // +5h ≫ old 1h window
        let stats2 = run_voting_tick(&ctx, now_ms_2).await.unwrap();
        assert_eq!(
            stats2.tier2_proposals_finalized, 0,
            "already Finalized on tick 2 — no re-finalize"
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "Finalized SetPower poll must be re-dispatched while Finalized, \
             regardless of elapsed time (got {})",
            calls.load(Ordering::SeqCst)
        );
    }

    /// ZEB-300 converge R2 (Greptile #2): re-dispatch is bounded by the
    /// `lifecycle == Finalized` gate — a poll that is no longer Finalized
    /// (e.g. the 24h archive sweep has flipped it to Archived) is NOT
    /// re-dispatched, even though its `finalized_at_ms` stamp still survives.
    /// This is the natural bound that replaced the old fixed 1h retry-window
    /// cutoff: the Finalized lifetime, not an arbitrary clock window.
    #[tokio::test]
    async fn tier2_auto_exec_skips_redispatch_when_not_finalized() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cid = SpaceId([0x77; 16]);
        let pid = PollId([0x88; 32]);
        let target = OwnerAddr([0xcc; 16]);
        let new_power = 100;
        let cfg = make_tier2_config(AutoExecAction::SetPower {
            target_pubkey: target,
            new_power,
        });
        let t2 = Tier2ProposalState::new(cfg, 1);

        // Poll is Archived (the post-sweep terminal state), NOT Finalized. Its
        // `finalized_at_ms` stamp survives archival, so the ONLY thing that can
        // stop re-dispatch here is the `lifecycle == Finalized` gate — this
        // proves that gate (not a clock window) is the bound.
        let mut poll = make_tier2_poll(cid, pid, Lifecycle::Archived, t2);
        poll.meta.finalized_at_ms = Some(1_000);

        let mut log = VotingLog::new();
        log.polls.insert(pid, poll);
        let mut logs = HashMap::new();
        logs.insert(cid, Arc::new(Mutex::new(log)));

        // last_sweep = now so the archive pass is a no-op for this test.
        let now_ms = 1_000i128;
        let (mut ctx, _events, _auto_exec_calls) = make_ctx_with_logs(logs, now_ms);

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in_closure = Arc::clone(&calls);
        ctx.auto_exec_set_power = Arc::new(move |_cid, _target, _power| {
            let calls = Arc::clone(&calls_in_closure);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(crate::community_membership::AutoExecOutcome::Applied)
            })
        });

        let stats = run_voting_tick(&ctx, now_ms).await.unwrap();
        assert_eq!(
            stats.tier2_proposals_finalized, 0,
            "Archived poll must not (re-)finalize"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a non-Finalized (Archived) poll must NOT be re-dispatched"
        );
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
