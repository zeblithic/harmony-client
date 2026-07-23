//! ZEB-418 SP2 P3a: channel-history backfill — the async drivers over the
//! paging + retry latch. The pure latch state machines were extracted to the
//! reusable `harmony_crdt_sync` core crate (ZEB-737 / ZEB-571 item 3); this
//! module keeps the async interpreters (`run_backfill_driver` /
//! `run_root_fetch_driver`), the epoch/resync helpers, and the `Hlc`
//! watermark binding, and re-exports the latch primitives (see below).
//!
//! When a device joins a channel (or reconnects after downtime) it asks
//! online holders for the channel history it missed. The serving side
//! caps every reply page (hard cap 1000 events per request), and a query
//! can go entirely unanswered when no holder is online. This module is
//! the pure decision core for the requesting side.
//!
//! ## Spec D24 semantics (as amended)
//!
//! - **Satisfied** means a *completed* short or empty page was received:
//!   a served "nothing more" is an answer; an unanswered query is not.
//! - **Full page** (`events >= limit`, `limit > 0`) means the holder hit
//!   its page cap and more history may remain. If the verified watermark
//!   moved (`max_hlc_seen` is `Some` and differs from the requested
//!   `since`), advance `since` and re-request **immediately** — no
//!   backoff between progressing pages. This is the paging loop. A full
//!   page that does NOT move the watermark (hostile holder serving
//!   garbage that fails verification, all-duplicate replies,
//!   cross-config limits) instead arms the same backoff as a no-reply:
//!   the latch stays unsatisfied (history may genuinely remain) but
//!   stops hammering the same window.
//! - **No reply** (zero responders) backs off exponentially: first retry
//!   after [`BACKFILL_RETRY_BASE_MS`] (30 s), doubling per consecutive
//!   miss, capped at [`BACKFILL_RETRY_CAP_MS`] (600 s) between attempts,
//!   retrying forever — the async driver enforces shutdown, not the
//!   latch.
//! - [`BackfillLatch::reset`] re-arms a satisfied latch with a new
//!   watermark (transport-recovery hook — wired to transport-epoch
//!   bumps by [`run_backfill_driver`] since ZEB-434 Task 7).
//!
//! ## Why pure logic
//!
//! The latch holds no zenoh handle and no tokio types: decisions go in
//! (page outcomes, wall-clock milliseconds) and actions come out
//! ([`BackfillAction::Request`] / [`BackfillAction::WaitUntil`] /
//! [`BackfillAction::Idle`]), so the full paging/backoff/in-flight state
//! space is testable without a runtime. [`run_backfill_driver`] is the
//! async interpreter of those actions — transport access stays behind
//! injected closures, so the driver too is testable with stubs.
//! Precedent: `dm_outhold_apply`'s sweeper core + sweeper task split.
//!
//! Spec: `docs/specs/2026-06-10-zeb-418-sp2-p3a-channel-backfill-design.md`.

use crate::owner_state_types::Hlc;

// ZEB-737 (ZEB-571 item 3): the paging/backoff latch primitives were extracted
// to the reusable `harmony_crdt_sync` core crate. This module keeps the async
// drivers (`run_backfill_driver` / `run_root_fetch_driver`), the epoch/resync
// helpers, and the app-specific `Hlc` watermark binding. It re-exports the
// core primitives — and binds the generic ones to `Hlc` via type aliases — so
// the drivers, the consumer sites, and the behavior tests below are unchanged.
pub use harmony_crdt_sync::{
    RootFetchAction, RootFetchLatch, BACKFILL_RETRY_BASE_MS, BACKFILL_RETRY_CAP_MS,
};

/// This app's paging-backfill latch: [`harmony_crdt_sync::BackfillLatch`]
/// bound to the community [`Hlc`] watermark.
pub type BackfillLatch = harmony_crdt_sync::BackfillLatch<Hlc>;
/// The next-action a [`BackfillLatch`] emits (bound to [`Hlc`]).
pub type BackfillAction = harmony_crdt_sync::BackfillAction<Hlc>;
/// A completed backfill reply page fed to a [`BackfillLatch`] (bound to [`Hlc`]).
pub type PageOutcome = harmony_crdt_sync::PageOutcome<Hlc>;

/// ZEB-592: which catch-up strategy the backfill driver follows after probing
/// for an RBSR responder. The driver tries RBSR first; an old peer (no
/// `rbsr/**` queryable) draws zero replies on round 0 and the driver falls back
/// to the watermark-vector path. A reconcile that fails to converge inside the
/// round cap falls back to a full reconcile (`since = None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileMode {
    /// Keep running RBSR rounds.
    RbsrContinue,
    /// No `rbsr/**` responder answered round 0 → use the legacy watermark GET.
    VectorFallback,
    /// Round cap hit without convergence → fall back to a full reconcile.
    FullReconcile,
    /// Difference drained — catch-up complete.
    Done,
}

/// Decide the mode after RBSR round 0 by how many replies the GET drew. Zero
/// replies means no `rbsr/**` responder was online (old peer or none) → fall
/// back to the watermark-vector path; any reply means an RBSR peer answered.
pub fn reconcile_mode_after_round0(rbsr_replies: usize) -> ReconcileMode {
    if rbsr_replies == 0 {
        ReconcileMode::VectorFallback
    } else {
        ReconcileMode::RbsrContinue
    }
}

/// Decide the mode after an RBSR round: converged → `Done`; at/over the round
/// cap → `FullReconcile`; otherwise keep running rounds.
pub fn reconcile_mode_after_round(round: u32, converged: bool) -> ReconcileMode {
    if converged {
        ReconcileMode::Done
    } else if round >= crate::channel_rbsr::MAX_RBSR_ROUNDS {
        ReconcileMode::FullReconcile
    } else {
        ReconcileMode::RbsrContinue
    }
}

#[cfg(test)]
mod reconcile_mode_tests {
    use super::*;

    #[test]
    fn round0_zero_replies_selects_vector_fallback() {
        assert_eq!(
            reconcile_mode_after_round0(0),
            ReconcileMode::VectorFallback
        );
        assert_eq!(reconcile_mode_after_round0(1), ReconcileMode::RbsrContinue);
        assert_eq!(reconcile_mode_after_round0(7), ReconcileMode::RbsrContinue);
    }

    #[test]
    fn round_cap_falls_back_to_full_reconcile() {
        assert_eq!(
            reconcile_mode_after_round(crate::channel_rbsr::MAX_RBSR_ROUNDS, false),
            ReconcileMode::FullReconcile
        );
        assert_eq!(reconcile_mode_after_round(3, true), ReconcileMode::Done);
        assert_eq!(
            reconcile_mode_after_round(3, false),
            ReconcileMode::RbsrContinue
        );
    }
}

// ── Shared epoch re-arm helpers (ZEB-434) ───────────────────────────────────

/// Cooldown between transport-epoch-triggered re-arm queries (60 s).
/// Spec D7: a flapping link must not storm; deferred, not dropped.
/// Governs BOTH [`run_backfill_driver`] and [`run_root_fetch_driver`].
pub const EPOCH_REARM_COOLDOWN_MS: u64 = 60_000;

/// Anti-entropy floor (ZEB-425): re-arm a satisfied backfill/root-fetch
/// latch at most this long after it last (re-)synced, regardless of
/// transport-epoch bumps. 1 h — well above [`EPOCH_REARM_COOLDOWN_MS`], so
/// it only acts when the edge-triggered re-arm never fires (holders
/// reachable only via a router, a queryable that matched after its peer's
/// zid was already seen, or a same-zid reconnect). Governs BOTH
/// [`run_backfill_driver`] and [`run_root_fetch_driver`].
pub const PERIODIC_RESYNC_FLOOR_MS: u64 = 3_600_000;

/// Per-driver jitter span (ms) added on top of [`PERIODIC_RESYNC_FLOOR_MS`]
/// so drivers that reach Idle together — notably the burst spawned at
/// startup — do NOT form a thundering herd of synchronized re-queries
/// (Qodo, PR #257). 10 min. Each driver picks its own offset once via
/// [`periodic_resync_interval_ms`].
pub const PERIODIC_RESYNC_JITTER_MS: u64 = 600_000;

/// Production resync interval for one driver: the floor plus uniform
/// jitter in `[0, PERIODIC_RESYNC_JITTER_MS)`, chosen ONCE per spawn so
/// satisfied latches de-synchronize and don't re-query in lockstep. Tests
/// pass a fixed `Some(ms)` directly instead, for deterministic timing.
pub fn periodic_resync_interval_ms() -> u64 {
    use rand::Rng;
    PERIODIC_RESYNC_FLOOR_MS + rand::thread_rng().gen_range(0..PERIODIC_RESYNC_JITTER_MS)
}

/// Safety floor (ms) for the persist-aware resync wake (ZEB-599). The
/// caller always hands the driver a FUTURE first deadline (past-due
/// channels are spread by [`first_resync_deadline`]), so this only
/// guards a deadline that lands in the past via clock skew — clamping
/// keeps the wake strictly positive so it can never collapse into
/// [`resync_tick`]'s `Some(0)` = disabled meaning.
const MIN_RESYNC_WAKE_MS: u64 = 1_000;

/// Persist hook + restart-aware first deadline for the ZEB-425/584
/// anti-entropy floor (ZEB-599).
///
/// Threaded into [`run_backfill_driver`] as `Some(..)` in production so
/// the floor survives process restarts: the FIRST periodic full
/// reconcile this session fires at the absolute wall-clock
/// [`first_deadline_ms`](Self::first_deadline_ms) (derived by the caller
/// from the persisted last-full-reconcile via [`first_resync_deadline`])
/// instead of `spawn + interval`, and each floor fire invokes
/// [`on_full_reconcile`](Self::on_full_reconcile) with the current
/// wall-ms so the caller persists the new timestamp.
///
/// `None` preserves the exact legacy floor behaviour (interval measured
/// from each spawn/Idle, no persistence) — used by every test call site
/// and any caller without a persistence sink.
#[derive(Clone)]
pub struct ResyncPersist {
    /// Absolute wall-clock ms at which the first periodic full reconcile
    /// should fire this session.
    pub first_deadline_ms: u64,
    /// Invoked (synchronously, non-blocking) with the wall-clock ms at
    /// each floor fire, right after `latch.reset(None)`. The production
    /// callback offloads the tiny sidecar write to `spawn_blocking`, so
    /// the driver never parks a worker on fsync.
    pub on_full_reconcile: std::sync::Arc<dyn Fn(u64) + Send + Sync>,
}

impl std::fmt::Debug for ResyncPersist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResyncPersist")
            .field("first_deadline_ms", &self.first_deadline_ms)
            .finish_non_exhaustive()
    }
}

/// Absolute wall-clock ms for the FIRST periodic full reconcile this
/// session, restart-aware (ZEB-599).
///
/// - `last = Some(t)` not yet due (`t + interval > now`) ⇒ honor the
///   persisted schedule (`t + interval`): a restart resumes the
///   remaining wait instead of resetting a fresh interval.
/// - `last = Some(t)` past-due ⇒ spread across the jitter window
///   (`now + (interval - PERIODIC_RESYNC_FLOOR_MS)`, i.e. the per-driver
///   jitter already baked into `interval`) so a boot herd of overdue
///   channels doesn't fire in lockstep.
/// - `last = None` (never reconciled) ⇒ `now + interval`, identical to
///   the legacy interval-from-spawn.
///
/// A persisted `last` AHEAD of `now` (a backward clock correction, VM
/// restore, or corrupt stamp) is untrustworthy and would otherwise honor
/// a far-future deadline that stalls the backstop for hours — so `last`
/// is clamped to `now` first, degrading to `now + interval` (Qodo #380).
pub fn first_resync_deadline(last: Option<u64>, interval_ms: u64, now_ms: u64) -> u64 {
    match last {
        Some(t) => {
            // Clamp a future/skewed stamp so `due` can never sit far ahead.
            let t = t.min(now_ms);
            let due = t.saturating_add(interval_ms);
            if due > now_ms {
                due
            } else {
                now_ms.saturating_add(interval_ms.saturating_sub(PERIODIC_RESYNC_FLOOR_MS))
            }
        }
        None => now_ms.saturating_add(interval_ms),
    }
}

/// Wait for a transport-epoch bump. Pends forever when no watch is
/// wired; returns false when the epoch sender dropped — callers
/// degrade to no-epoch mode (`epoch_rx = None`) rather than exiting,
/// so a dropped watch never strands an unsatisfied latch.
async fn epoch_bump(epoch_rx: &mut Option<tokio::sync::watch::Receiver<u64>>) -> bool {
    match epoch_rx.as_mut() {
        Some(rx) => rx.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Fire after `interval_ms` when set to a positive value; pend forever
/// otherwise so its `select!` arm never wins. `None` AND `Some(0)` both
/// disable the floor: a zero interval would complete immediately on every
/// Idle loop, spinning re-arm/fetch with no delay (CodeAnt, PR #257).
/// Mirrors [`epoch_bump`]'s pend-when-absent shape (ZEB-425).
async fn resync_tick(interval_ms: Option<u64>) {
    match interval_ms {
        Some(ms) if ms > 0 => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
        _ => std::future::pending().await,
    }
}

/// Defer an epoch-triggered re-arm to the cooldown boundary (deferred,
/// not dropped). Returns false if shutdown fires during the wait.
async fn cooldown_wait(
    last_request_at: Option<u64>,
    now: u64,
    shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    let Some(last) = last_request_at else {
        return true;
    };
    let target = last.saturating_add(EPOCH_REARM_COOLDOWN_MS);
    if now >= target {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(target - now)) => true,
        changed = shutdown_rx.changed() => {
            // Err = sender dropped (registry entry gone):
            // same as an explicit shutdown signal.
            !(changed.is_err() || *shutdown_rx.borrow())
        }
    }
}

// ── Async driver (ZEB-418 P3a Task 4) ───────────────────────────────────────

/// Outcome of one page-fetch attempt, as seen by the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageFetch {
    /// Query completed cleanly (replies, effective_limit).
    Completed(usize, usize),
    /// No responders / query aborted — transient; back off and retry.
    NoReply,
    /// The engine/adapter is gone for good — stop the driver.
    EngineGone,
}

/// Floor for the driver's backoff sleeps (ms). Defensive: guards
/// against a hot loop if the injected clock ever lags the latch's
/// `next_retry_at` (the latch's in-flight `WaitUntil` clamp can hand
/// back a target equal to `now`).
const BACKFILL_DRIVER_MIN_WAIT_MS: u64 = 250;

/// Drive one channel's [`BackfillLatch`] to satisfaction, then — when
/// `epoch_rx` is `Some` — park and re-arm on transport-epoch bumps
/// (ZEB-434 Task 7, closing P3a spec §9's deferred transport-recovery
/// hook).
///
/// Spawned by `ChannelLogRegistry::spawn` for every freshly inserted
/// engine entry — running it unconditionally at engine start unifies
/// the spec's join + reconnect triggers: a fresh joiner's empty log
/// gives a `None` watermark (request full history), a reconnecting
/// device's reloaded log gives `Some(watermark)` (catch-up).
///
/// - `request_page(since)` issues one backfill query and resolves to
///   a [`PageFetch`]: `Completed(replies, limit)` after the reply
///   stream closes cleanly; `NoReply` on no-reply/abort (mapped to
///   backoff via [`BackfillLatch::on_no_reply`]); `EngineGone` when
///   the engine/adapter is permanently gone (no recovery hook exists
///   — the driver returns instead of retrying forever).
/// - `current_watermark()` re-reads the max HLC from the LOG after a
///   full page. The watermark is deliberately NOT taken from the raw
///   reply packets: only verified events land in the log, so a hostile
///   holder serving garbage can't advance `since` past history it
///   never actually delivered. (Async because the log sits behind a
///   `tokio::sync::Mutex` — a sync `try_lock` here could miss under
///   contention and stall `since`, spinning the no-backoff paging
///   loop.)
/// - `shutdown_rx` flipping to `true` — or its sender dropping —
///   ends the driver promptly during a backoff wait.
/// - `epoch_rx = None` with `resync_interval_ms = None` preserves the
///   legacy contract exactly: return-on-Idle, no parking (used by tests
///   and any caller without a transport watch). With `Some`, a bump while
///   Idle (or mid-backoff) re-arms via [`BackfillLatch::reset`] with a
///   FRESH `current_watermark()` read — the verified log watermark at
///   re-arm time, never the spawn-time one and never anything
///   reply-derived, so a hostile holder can't have advanced it. Re-arm
///   queries are deferred to [`EPOCH_REARM_COOLDOWN_MS`] since the last
///   request — deferred, not dropped (same flap-storm guard as
///   [`run_root_fetch_driver`], spec D7). If the epoch SENDER drops,
///   the driver degrades to the no-epoch contract (keeps any backoff
///   retries running; returns on Idle only if resync is also disabled)
///   instead of exiting mid-latch.
/// - `full_resync_rx` (ZEB-599 Direction 1) is the presence-driven
///   reachability watch. A bump re-arms with a FULL reconcile
///   (`since = None`, like the periodic floor) rather than the epoch arm's
///   incremental watermark, so a relay-mediated cross-WAN peer's
///   below-watermark backlog heals within seconds of the peer appearing
///   instead of waiting the ~1h floor. Same cooldown + sender-drop
///   degradation as `epoch_rx`; `None` disables it (used by tests / callers
///   with no presence signal).
/// - `resync_interval_ms = Some(ms)` arms the ZEB-425 anti-entropy floor:
///   a satisfied latch re-arms every `ms` even with NO epoch bump. ZEB-584:
///   the floor re-arms with a FULL reconcile (`since = None`), NOT the
///   incremental watermark — the scalar `current_watermark()` high-water
///   mark can't recover a peer's offline-window entry whose HLC sorts below
///   the returning member's max (the incremental catch-up's blind spot), so
///   the periodic floor re-pulls from scratch. It only acts when the
///   edge-triggered re-arm never fires (router-only holders, late-matching
///   queryables, same-zid reconnects) or when the incremental watermark hides
///   a sub-max gap. No cooldown — the interval is itself the rate limit (a
///   re-armed latch with still-no-holders backs off via `WaitUntil`, never
///   straight back to Idle, so the floor cannot storm). `None` disables it
///   (production passes [`PERIODIC_RESYNC_FLOOR_MS`]).
/// - `now_ms` injects the wall clock (dm_outhold_apply testability
///   precedent): production passes a `SystemTime`-based closure;
///   tests pair a `tokio::time::Instant`-based closure with paused
///   time so no test ever sleeps for real.
/// - `resync_persist` (ZEB-599) makes the floor restart-aware — see
///   [`ResyncPersist`].
///
/// Each parameter is a distinct injected dependency (transport, clock,
/// lifecycle, persistence), kept separate for test injection rather than
/// bundled into an opaque config struct.
#[allow(clippy::too_many_arguments)]
pub async fn run_backfill_driver<Rq, RqFut, Wm, WmFut>(
    mut latch: BackfillLatch,
    request_page: Rq,
    current_watermark: Wm,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    mut epoch_rx: Option<tokio::sync::watch::Receiver<u64>>,
    // ZEB-599 Direction 1: presence-driven reachability watch. Bumped by the
    // per-community presence subscriber when a new device enters the roster (a
    // new potential holder became reachable). A bump re-arms the latch with a
    // FULL reconcile (`since = None`) — the fast, relay-mediated analogue of the
    // ~1h periodic floor — so a NAT'd cross-WAN peer's below-watermark backlog
    // heals within seconds instead of waiting the hour. Same `Option<watch>`
    // shape + sender-drop degradation as `epoch_rx`; `None` (tests, callers with
    // no presence) preserves prior behaviour exactly.
    mut full_resync_rx: Option<tokio::sync::watch::Receiver<u64>>,
    resync_interval_ms: Option<u64>,
    now_ms: impl Fn() -> u64,
    // ZEB-599: `Some(..)` makes the periodic floor restart-aware — the
    // first fire lands at an absolute (persisted) deadline and every
    // fire persists a fresh timestamp. `None` = exact legacy behaviour.
    resync_persist: Option<ResyncPersist>,
) where
    Rq: Fn(Option<Hlc>) -> RqFut,
    RqFut: std::future::Future<Output = PageFetch>,
    Wm: Fn() -> WmFut,
    WmFut: std::future::Future<Output = Option<Hlc>>,
{
    let mut last_request_at: Option<u64> = None;
    // ZEB-599: absolute-deadline resync when a persistence sink is
    // wired. Seeded from the caller's restart-aware first deadline;
    // advanced by `resync_interval_ms` after each fire. `None` (no
    // persist) leaves the legacy interval-from-spawn path untouched.
    let mut resync_deadline: Option<u64> = resync_persist.as_ref().map(|p| p.first_deadline_ms);
    // ZEB-599 D3: `harmony_channel` debug events pin which re-arm path a
    // recovery came through on a live fleet (RUST_LOG=info,harmony_channel=debug).
    tracing::debug!(
        target: "harmony_channel",
        floor_deadline_ms = ?resync_deadline,
        restart_aware = resync_persist.is_some(),
        "backfill driver started"
    );
    loop {
        // Cheap pre-check: covers "stopped before the driver's first
        // poll" (spawn/stop race) without waiting for a `changed()`.
        if *shutdown_rx.borrow() {
            return;
        }
        match latch.next_action(now_ms()) {
            BackfillAction::Idle => {
                if epoch_rx.is_none()
                    && full_resync_rx.is_none()
                    && !matches!(resync_interval_ms, Some(ms) if ms > 0)
                {
                    return;
                }
                // ZEB-599: with a persist sink, arm the floor at the
                // absolute (restart-aware) deadline, recomputed each Idle
                // entry so epoch-churn can't postpone it. Without a sink,
                // fall back to the legacy interval-measured-from-now.
                let resync_arg = match (&resync_persist, resync_deadline) {
                    (Some(_), Some(deadline)) => match resync_interval_ms {
                        Some(ms) if ms > 0 => {
                            Some(deadline.saturating_sub(now_ms()).max(MIN_RESYNC_WAKE_MS))
                        }
                        // Sink wired but floor disabled — respect disable.
                        _ => None,
                    },
                    // Legacy: interval measured from now (or disabled).
                    _ => resync_interval_ms,
                };
                tokio::select! {
                    bumped = epoch_bump(&mut epoch_rx) => {
                        if !bumped {
                            // Epoch sender dropped: degrade to no-epoch
                            // mode rather than abandoning the latch —
                            // backoff/shutdown flow continues; Idle then
                            // returns unless the resync floor still parks it.
                            epoch_rx = None;
                            continue;
                        }
                        if !cooldown_wait(last_request_at, now_ms(), &mut shutdown_rx).await {
                            return;
                        }
                        let wm = current_watermark().await;
                        tracing::debug!(
                            target: "harmony_channel",
                            since = ?wm,
                            "re-arm: transport-epoch bump (incremental)"
                        );
                        latch.reset(wm);
                    }
                    kicked = epoch_bump(&mut full_resync_rx) => {
                        // ZEB-599 Direction 1: a presence-driven reachability
                        // kick re-arms with a FULL reconcile (`since = None`) —
                        // the fast, relay-mediated analogue of the periodic floor
                        // (`latch.reset(None)` below). Full, NOT the incremental
                        // `current_watermark()` the epoch arm uses, because a
                        // returning/relay peer may hold entries whose HLC sorts
                        // below our max (the ZEB-584 blind spot). Cooldown-gated
                        // like the epoch arm so presence churn can't storm; the
                        // ~1h floor stays the ultimate backstop and its persisted
                        // deadline is deliberately untouched here (a presence
                        // reconcile is a bonus, not a floor fire).
                        if !kicked {
                            // Presence sender dropped: degrade to no-presence
                            // mode (mirrors the epoch-sender-drop path above).
                            full_resync_rx = None;
                            continue;
                        }
                        if !cooldown_wait(last_request_at, now_ms(), &mut shutdown_rx).await {
                            return;
                        }
                        tracing::debug!(
                            target: "harmony_channel",
                            "re-arm: presence kick (full reconcile, since=None)"
                        );
                        latch.reset(None);
                    }
                    _ = resync_tick(resync_arg) => {
                        // ZEB-425 anti-entropy floor + ZEB-584 full reconcile:
                        // re-arm with `since = None` (pull from scratch), NOT
                        // the incremental `current_watermark()`. A scalar HLC
                        // high-water mark is not a completeness certificate —
                        // `max_hlc = M` only means "the latest event I hold is
                        // M," not "I hold every event ≤ M." A returning member
                        // that was offline while a peer authored an entry whose
                        // HLC sorts below M never refetches it on the
                        // incremental path (the responder serves only
                        // `hlc > since`). The periodic floor closes that blind
                        // spot by re-pulling from the start. (Heals any sub-max
                        // gap that fits within one backfill page; full
                        // multi-page diff reconciliation — per-peer watermarks
                        // or prolly-tree/CAS range sync — is ZEB-585.) The
                        // edge-triggered epoch re-arm stays incremental — see
                        // the `WaitUntil`/`Idle` epoch arms above. No cooldown:
                        // the interval is the rate limit (a re-armed no-holder
                        // latch backs off via WaitUntil, never straight back to
                        // Idle).
                        tracing::debug!(
                            target: "harmony_channel",
                            "re-arm: periodic floor fired (full reconcile, since=None)"
                        );
                        latch.reset(None);
                        // ZEB-599: record + persist this full reconcile,
                        // then set the next deadline a full interval out.
                        if let Some(p) = resync_persist.as_ref() {
                            let fired_at = now_ms();
                            (p.on_full_reconcile)(fired_at);
                            if let Some(ms) = resync_interval_ms {
                                resync_deadline = Some(fired_at.saturating_add(ms));
                                tracing::debug!(
                                    target: "harmony_channel",
                                    fired_at_ms = fired_at,
                                    next_deadline_ms = ?resync_deadline,
                                    "floor fire persisted; next absolute deadline set"
                                );
                            }
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        // Err = sender dropped (registry entry gone):
                        // same as an explicit shutdown signal.
                        if changed.is_err() || *shutdown_rx.borrow() { return; }
                    }
                }
            }
            BackfillAction::WaitUntil(target) => {
                let wait_ms = target
                    .saturating_sub(now_ms())
                    .max(BACKFILL_DRIVER_MIN_WAIT_MS);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {}
                    bumped = epoch_bump(&mut epoch_rx) => {
                        // Mid-backoff bump: a new peer is exactly the
                        // signal that retrying now is worthwhile (D7).
                        if !bumped {
                            // Epoch sender dropped: degrade to no-epoch
                            // mode rather than abandoning the (still
                            // unsatisfied) latch — backoff retries and
                            // shutdown handling continue unchanged.
                            epoch_rx = None;
                            continue;
                        }
                        if !cooldown_wait(last_request_at, now_ms(), &mut shutdown_rx).await {
                            return;
                        }
                        let wm = current_watermark().await;
                        tracing::debug!(
                            target: "harmony_channel",
                            since = ?wm,
                            "re-arm: transport-epoch bump mid-backoff (incremental)"
                        );
                        latch.reset(wm);
                    }
                    kicked = epoch_bump(&mut full_resync_rx) => {
                        // ZEB-599 Direction 1: a presence kick mid-backoff — a
                        // new holder just became reachable, so restart from
                        // scratch with a FULL reconcile (see the Idle arm).
                        // Cooldown-gated like the epoch arm.
                        if !kicked {
                            full_resync_rx = None;
                            continue;
                        }
                        if !cooldown_wait(last_request_at, now_ms(), &mut shutdown_rx).await {
                            return;
                        }
                        tracing::debug!(
                            target: "harmony_channel",
                            "re-arm: presence kick mid-backoff (full reconcile, since=None)"
                        );
                        latch.reset(None);
                    }
                    changed = shutdown_rx.changed() => {
                        // Err = sender dropped (registry entry gone):
                        // same as an explicit shutdown signal.
                        if changed.is_err() || *shutdown_rx.borrow() {
                            return;
                        }
                    }
                }
            }
            BackfillAction::Request { since } => {
                last_request_at = Some(now_ms());
                tracing::debug!(
                    target: "harmony_channel",
                    since = ?since,
                    "backfill page request"
                );
                match request_page(since).await {
                    PageFetch::Completed(replies, limit) => {
                        tracing::debug!(
                            target: "harmony_channel",
                            replies,
                            limit,
                            "backfill page completed"
                        );
                        let full_page = limit > 0 && replies >= limit;
                        // Watermark re-read from the LOG (only verified
                        // events land there) rather than trusted from the
                        // raw reply packets — a hostile holder serving
                        // garbage can't advance `since`. See the
                        // `current_watermark` doc above.
                        let max_hlc_seen = if full_page {
                            current_watermark().await
                        } else {
                            None
                        };
                        latch.on_page_complete(
                            PageOutcome {
                                events: replies,
                                max_hlc_seen,
                                limit,
                            },
                            now_ms(),
                        );
                    }
                    PageFetch::NoReply => {
                        tracing::debug!(
                            target: "harmony_channel",
                            "backfill page no-reply (no responder) → backoff"
                        );
                        latch.on_no_reply(now_ms());
                    }
                    // Permanent: the engine/adapter is gone for good and
                    // no recovery hook exists — stop instead of burning
                    // eternal futile retries until engine stop.
                    PageFetch::EngineGone => return,
                }
            }
        }
    }
}

// ── ZEB-434: community state-root fetch latch ────────────────────────────────

/// Outcome of one root-fetch attempt, as seen by the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootFetch {
    /// ≥1 responder replied (replies ingest via the engine's normal
    /// inbound path — receipt is transport-level, same bar as P3a).
    Answered,
    /// Zero responders / query aborted — back off and retry.
    NoReply,
    /// Engine/adapter permanently gone — stop the driver.
    EngineGone,
}

/// Drive one community's [`RootFetchLatch`]: query at spawn, back off
/// while unanswered, and — when `epoch_rx` is `Some` — park on
/// satisfaction and re-arm on transport-epoch bumps (ZEB-434 D6/D7).
///
/// - `request_root()` issues one state-root query and resolves to a
///   [`RootFetch`]. Reply payloads travel through the engine's normal
///   inbound path; the driver only sees counts.
/// - `epoch_rx = None` with `resync_interval_ms = None` preserves
///   return-on-Idle (used by tests and any caller without a transport
///   watch).
/// - Re-arm queries are deferred to [`EPOCH_REARM_COOLDOWN_MS`] since
///   the last request — deferred, not dropped (spec D7). If the epoch
///   SENDER drops, the driver degrades to the no-epoch contract (keeps
///   any backoff retries running; returns on Idle only if resync is also
///   disabled) instead of exiting mid-latch.
/// - `full_resync_rx` (ZEB-599 Direction 1, parity with
///   [`run_backfill_driver`]) is the presence-driven reachability watch. A
///   bump re-arms the latch (`reset()`) — for a page-less root fetch,
///   `reset()` IS the full re-fetch, so a relay-mediated cross-WAN peer's
///   below-watermark state heals within seconds of the peer appearing
///   instead of waiting the ~1h floor. Same cooldown + sender-drop
///   degradation as `epoch_rx`; `None` disables it (used by tests / callers
///   with no presence signal).
/// - `resync_interval_ms = Some(ms)` arms the ZEB-425 anti-entropy floor:
///   a satisfied latch re-arms (`reset()`) every `ms` even with NO epoch
///   bump, covering holders the never-seen-zid signal misses (router-only
///   peers, late-matching queryables, same-zid reconnects). No cooldown —
///   the interval is the rate limit (a re-armed no-responder latch backs
///   off via `WaitUntil`, never straight back to Idle, so it cannot
///   storm). `None` disables it (production passes
///   [`PERIODIC_RESYNC_FLOOR_MS`]).
/// - `now_ms` injects the wall clock (paused-time testability — same
///   precedent as `run_backfill_driver`).
/// - `resync_persist` (ZEB-599) makes the floor restart-aware — the first
///   fire lands at an absolute (persisted) deadline and every fire
///   persists a fresh timestamp. `None` = exact legacy behaviour. See
///   [`ResyncPersist`].
#[allow(clippy::too_many_arguments)]
pub async fn run_root_fetch_driver<Rq, RqFut>(
    mut latch: RootFetchLatch,
    request_root: Rq,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    mut epoch_rx: Option<tokio::sync::watch::Receiver<u64>>,
    // ZEB-618: presence-driven reachability kick (ZEB-599 D1 parity
    // with run_backfill_driver). A kick re-arms the latch — for a
    // root fetch, reset() IS the full re-fetch. Cooldown-gated like
    // the epoch arm; sender-drop degrades to None.
    mut full_resync_rx: Option<tokio::sync::watch::Receiver<u64>>,
    resync_interval_ms: Option<u64>,
    now_ms: impl Fn() -> u64,
    // ZEB-618: Some(..) makes the floor restart-aware (ZEB-584 parity):
    // first fire at the persisted absolute deadline, each fire persists.
    resync_persist: Option<ResyncPersist>,
) where
    Rq: Fn() -> RqFut,
    RqFut: std::future::Future<Output = RootFetch>,
{
    let mut last_request_at: Option<u64> = None;
    // ZEB-618: absolute-deadline resync when a persistence sink is
    // wired. Seeded from the caller's restart-aware first deadline;
    // advanced by `resync_interval_ms` after each fire. `None` (no
    // persist) leaves the legacy interval-from-spawn path untouched.
    let mut resync_deadline: Option<u64> = resync_persist.as_ref().map(|p| p.first_deadline_ms);
    // ZEB-599 D3: `harmony_channel` debug events pin which re-arm path a
    // recovery came through on a live fleet (RUST_LOG=info,harmony_channel=debug).
    tracing::debug!(
        target: "harmony_channel",
        floor_deadline_ms = ?resync_deadline,
        restart_aware = resync_persist.is_some(),
        "root fetch driver started"
    );
    loop {
        if *shutdown_rx.borrow() {
            return;
        }
        match latch.next_action(now_ms()) {
            RootFetchAction::Idle => {
                if epoch_rx.is_none()
                    && full_resync_rx.is_none()
                    && !matches!(resync_interval_ms, Some(ms) if ms > 0)
                {
                    return;
                }
                // ZEB-618: with a persist sink, arm the floor at the
                // absolute (restart-aware) deadline, recomputed each Idle
                // entry so epoch-churn can't postpone it. Without a sink,
                // fall back to the legacy interval-measured-from-now.
                let resync_arg = match (&resync_persist, resync_deadline) {
                    (Some(_), Some(deadline)) => match resync_interval_ms {
                        Some(ms) if ms > 0 => {
                            Some(deadline.saturating_sub(now_ms()).max(MIN_RESYNC_WAKE_MS))
                        }
                        // Sink wired but floor disabled — respect disable.
                        _ => None,
                    },
                    // Legacy: interval measured from now (or disabled).
                    _ => resync_interval_ms,
                };
                tokio::select! {
                    bumped = epoch_bump(&mut epoch_rx) => {
                        if !bumped {
                            // Epoch sender dropped: degrade to no-epoch
                            // mode rather than abandoning the latch —
                            // backoff/shutdown flow continues; Idle then
                            // returns unless the resync floor still parks it.
                            epoch_rx = None;
                            continue;
                        }
                        if !cooldown_wait(last_request_at, now_ms(), &mut shutdown_rx).await {
                            return;
                        }
                        tracing::debug!(
                            target: "harmony_channel",
                            "root re-arm: transport-epoch bump"
                        );
                        latch.reset();
                    }
                    kicked = epoch_bump(&mut full_resync_rx) => {
                        // ZEB-618 (ZEB-599 D1): a presence-driven reachability
                        // kick re-arms the root latch — for a page-less root
                        // fetch, `reset()` IS the full re-fetch (the fast,
                        // relay-mediated analogue of the periodic floor).
                        // Cooldown-gated like the epoch arm so presence churn
                        // can't storm; the ~1h floor stays the ultimate
                        // backstop and its persisted deadline is deliberately
                        // untouched here (a presence reconcile is a bonus, not
                        // a floor fire).
                        if !kicked {
                            // Presence sender dropped: degrade to no-presence
                            // mode (mirrors the epoch-sender-drop path above).
                            full_resync_rx = None;
                            continue;
                        }
                        if !cooldown_wait(last_request_at, now_ms(), &mut shutdown_rx).await {
                            return;
                        }
                        tracing::debug!(
                            target: "harmony_channel",
                            "root re-arm: presence kick"
                        );
                        latch.reset();
                    }
                    _ = resync_tick(resync_arg) => {
                        // ZEB-425 anti-entropy floor: re-arm regardless of
                        // epoch bumps. No cooldown — see the
                        // `resync_interval_ms` doc (the interval is the rate
                        // limit; a re-armed no-responder latch backs off via
                        // WaitUntil, never straight back to Idle).
                        tracing::debug!(
                            target: "harmony_channel",
                            "root re-arm: periodic floor fired"
                        );
                        latch.reset();
                        // ZEB-618: record + persist this full reconcile,
                        // then set the next deadline a full interval out.
                        if let Some(p) = resync_persist.as_ref() {
                            let fired_at = now_ms();
                            (p.on_full_reconcile)(fired_at);
                            if let Some(ms) = resync_interval_ms {
                                resync_deadline = Some(fired_at.saturating_add(ms));
                                tracing::debug!(
                                    target: "harmony_channel",
                                    fired_at_ms = fired_at,
                                    next_deadline_ms = ?resync_deadline,
                                    "root floor fire persisted; next absolute deadline set"
                                );
                            }
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        // Err = sender dropped (registry entry gone):
                        // same as an explicit shutdown signal.
                        if changed.is_err() || *shutdown_rx.borrow() { return; }
                    }
                }
            }
            RootFetchAction::WaitUntil(target) => {
                let wait_ms = target
                    .saturating_sub(now_ms())
                    .max(BACKFILL_DRIVER_MIN_WAIT_MS);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {}
                    bumped = epoch_bump(&mut epoch_rx) => {
                        // Mid-backoff bump: a new peer is exactly the
                        // signal that retrying now is worthwhile (D7).
                        if !bumped {
                            // Epoch sender dropped: degrade to no-epoch
                            // mode rather than abandoning the (still
                            // unsatisfied) latch — backoff retries and
                            // shutdown handling continue unchanged.
                            epoch_rx = None;
                            continue;
                        }
                        if !cooldown_wait(last_request_at, now_ms(), &mut shutdown_rx).await {
                            return;
                        }
                        tracing::debug!(
                            target: "harmony_channel",
                            "root re-arm: transport-epoch bump mid-backoff"
                        );
                        latch.reset();
                    }
                    kicked = epoch_bump(&mut full_resync_rx) => {
                        // ZEB-618 (ZEB-599 D1): a presence kick mid-backoff — a
                        // new holder just became reachable, so re-arm now (see
                        // the Idle arm). Cooldown-gated like the epoch arm.
                        if !kicked {
                            full_resync_rx = None;
                            continue;
                        }
                        if !cooldown_wait(last_request_at, now_ms(), &mut shutdown_rx).await {
                            return;
                        }
                        tracing::debug!(
                            target: "harmony_channel",
                            "root re-arm: presence kick mid-backoff"
                        );
                        latch.reset();
                    }
                    changed = shutdown_rx.changed() => {
                        // Err = sender dropped (registry entry gone):
                        // same as an explicit shutdown signal.
                        if changed.is_err() || *shutdown_rx.borrow() { return; }
                    }
                }
            }
            RootFetchAction::Request => {
                last_request_at = Some(now_ms());
                match request_root().await {
                    RootFetch::Answered => latch.on_reply(now_ms()),
                    RootFetch::NoReply => latch.on_no_reply(now_ms()),
                    RootFetch::EngineGone => return,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny Hlc builder for tests (mirrors `dm_outhold_apply` tests).
    fn hlc(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: "dev".into(),
        }
    }

    #[test]
    fn fresh_latch_requests_from_watermark() {
        let mut latch = BackfillLatch::new(Some(hlc(100)));
        assert!(!latch.is_satisfied());
        assert_eq!(
            latch.next_action(0),
            BackfillAction::Request {
                since: Some(hlc(100))
            }
        );
    }

    #[test]
    fn full_page_advances_since_and_rerequests_immediately() {
        let mut latch = BackfillLatch::new(None);
        // Arm in-flight with the first request.
        assert_eq!(
            latch.next_action(0),
            BackfillAction::Request { since: None }
        );
        latch.on_page_complete(
            PageOutcome {
                events: 1000,
                max_hlc_seen: Some(hlc(500)),
                limit: 1000,
            },
            0,
        );
        // Full page → paging loop: re-request immediately from max HLC.
        assert_eq!(
            latch.next_action(0),
            BackfillAction::Request {
                since: Some(hlc(500))
            }
        );
        assert!(!latch.is_satisfied());
    }

    #[test]
    fn full_page_without_progress_backs_off_instead_of_spinning() {
        let mut latch = BackfillLatch::new(Some(hlc(100)));
        assert_eq!(
            latch.next_action(0),
            BackfillAction::Request {
                since: Some(hlc(100))
            }
        );
        // Full page whose verified watermark equals the requested
        // `since`: nothing new landed in the log (hostile holder
        // serving garbage that fails verification, or all-duplicate
        // replies). Re-requesting immediately would replay the same
        // window in a tight zero-backoff loop.
        latch.on_page_complete(
            PageOutcome {
                events: 1000,
                max_hlc_seen: Some(hlc(100)),
                limit: 1000,
            },
            0,
        );
        assert!(
            !latch.is_satisfied(),
            "no-progress must NOT satisfy — history may genuinely remain"
        );
        // Backs off like a no-reply instead of an immediate Request.
        assert_eq!(
            latch.next_action(0),
            BackfillAction::WaitUntil(BACKFILL_RETRY_BASE_MS)
        );
        // Once past the delay, the request fires again (same window) —
        // the holder set can change, so liveness is preserved.
        assert_eq!(
            latch.next_action(BACKFILL_RETRY_BASE_MS),
            BackfillAction::Request {
                since: Some(hlc(100))
            }
        );
    }

    #[test]
    fn first_retry_respects_cap_when_base_exceeds_cap() {
        // Misconfigured base > cap: the first retry must clamp to the
        // cap rather than wait the full (over-cap) base.
        let mut latch = BackfillLatch::new_with_backoff(None, 1_000, 300);
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(0);
        assert_eq!(latch.next_action(0), BackfillAction::WaitUntil(300));
    }

    #[test]
    fn short_page_satisfies() {
        let mut latch = BackfillLatch::new(None);
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        latch.on_page_complete(
            PageOutcome {
                events: 3,
                max_hlc_seen: Some(hlc(42)),
                limit: 1000,
            },
            0,
        );
        assert!(latch.is_satisfied());
        assert_eq!(latch.next_action(0), BackfillAction::Idle);
    }

    #[test]
    fn empty_completed_page_satisfies() {
        let mut latch = BackfillLatch::new(Some(hlc(7)));
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        latch.on_page_complete(
            PageOutcome {
                events: 0,
                max_hlc_seen: None,
                limit: 1000,
            },
            0,
        );
        assert!(latch.is_satisfied());
        assert_eq!(latch.next_action(0), BackfillAction::Idle);
    }

    #[test]
    fn no_reply_backs_off_exponentially_with_cap() {
        let mut latch = BackfillLatch::new(None);

        // t=0: first request goes out.
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        // No reply at t=0 → delay becomes 30_000;
        // next_retry_at = 0 + 30_000 = 30_000.
        latch.on_no_reply(0);
        assert_eq!(latch.next_action(0), BackfillAction::WaitUntil(30_000));

        // t=30_000: eligible again.
        assert!(matches!(
            latch.next_action(30_000),
            BackfillAction::Request { .. }
        ));
        // No reply at t=30_000 → delay doubles to 60_000;
        // next_retry_at = 30_000 + 60_000 = 90_000.
        latch.on_no_reply(30_000);
        assert_eq!(latch.next_action(30_000), BackfillAction::WaitUntil(90_000));

        // t=90_000: request; no reply → delay 120_000;
        // next_retry_at = 90_000 + 120_000 = 210_000.
        assert!(matches!(
            latch.next_action(90_000),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(90_000);
        assert_eq!(
            latch.next_action(90_000),
            BackfillAction::WaitUntil(210_000)
        );

        // t=210_000: request; no reply → delay 240_000;
        // next_retry_at = 210_000 + 240_000 = 450_000.
        assert!(matches!(
            latch.next_action(210_000),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(210_000);
        assert_eq!(
            latch.next_action(210_000),
            BackfillAction::WaitUntil(450_000)
        );

        // t=450_000: request; no reply → delay 480_000;
        // next_retry_at = 450_000 + 480_000 = 930_000.
        assert!(matches!(
            latch.next_action(450_000),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(450_000);
        assert_eq!(
            latch.next_action(450_000),
            BackfillAction::WaitUntil(930_000)
        );

        // t=930_000: request; no reply → doubling would give 960_000,
        // CAPPED at 600_000; next_retry_at = 930_000 + 600_000
        // = 1_530_000 — interval is exactly the cap.
        assert!(matches!(
            latch.next_action(930_000),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(930_000);
        assert_eq!(
            latch.next_action(930_000),
            BackfillAction::WaitUntil(1_530_000)
        );

        // t=1_530_000: request; no reply → stays capped at 600_000;
        // next_retry_at = 1_530_000 + 600_000 = 2_130_000.
        assert!(matches!(
            latch.next_action(1_530_000),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(1_530_000);
        assert_eq!(
            latch.next_action(1_530_000),
            BackfillAction::WaitUntil(2_130_000)
        );
    }

    #[test]
    fn reset_unsatisfies_with_new_watermark() {
        let mut latch = BackfillLatch::new(None);
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        latch.on_page_complete(
            PageOutcome {
                events: 0,
                max_hlc_seen: None,
                limit: 1000,
            },
            0,
        );
        assert!(latch.is_satisfied());

        latch.reset(Some(hlc(900)));
        assert!(!latch.is_satisfied());
        assert_eq!(
            latch.next_action(0),
            BackfillAction::Request {
                since: Some(hlc(900))
            }
        );
    }

    // ── ZEB-418 P3a Task 4: async driver ─────────────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn driver_retries_until_holder_appears_then_satisfies() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        // Calls 1-2: no reply (no holder online). Call 3: a holder
        // answers with a short page (3 < 256) → satisfied.
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    PageFetch::NoReply
                } else {
                    PageFetch::Completed(3, 256)
                }
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            None,
            // ZEB-599: no presence watch in this test.
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        )
        .await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            3,
            "exactly three requests: two unanswered, one satisfied"
        );
        // Backoff schedule: no-reply at t=0 → retry at 30s; no-reply at
        // 30s → retry at 90s. Paused time must have advanced past both
        // marks for the driver future to have completed.
        assert!(
            start.elapsed() >= Duration::from_millis(90_000),
            "driver must wait out the 30s and 90s backoff marks, \
             elapsed {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn driver_pages_through_full_pages_without_backoff() {
        let requests = Arc::new(AtomicUsize::new(0));
        let sinces: Arc<StdMutex<Vec<Option<Hlc>>>> = Arc::new(StdMutex::new(Vec::new()));
        let counter = Arc::clone(&requests);
        let since_log = Arc::clone(&sinces);
        // Two full pages (256/256) then a short page (5/256).
        let request_page = move |since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            let since_log = Arc::clone(&since_log);
            async move {
                since_log.lock().unwrap().push(since);
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    PageFetch::Completed(256, 256)
                } else {
                    PageFetch::Completed(5, 256)
                }
            }
        };
        // The log-side watermark advances on each read — the driver
        // must re-read it after every FULL page and pass it as the
        // next `since`.
        let watermark_reads = Arc::new(AtomicUsize::new(0));
        let wm_counter = Arc::clone(&watermark_reads);
        let current_watermark = move || {
            let n = wm_counter.fetch_add(1, Ordering::SeqCst) as u64;
            async move { Some(hlc(1_000 + n)) }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            current_watermark,
            shutdown_rx,
            None,
            // ZEB-599: no presence watch in this test.
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        )
        .await;
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        // Immediate paging: under paused time the future completes
        // without ever sleeping — any backoff between pages would
        // show up as auto-advanced elapsed time.
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "full-page paging must re-request immediately with no backoff"
        );
        assert_eq!(
            *sinces.lock().unwrap(),
            vec![None, Some(hlc(1_000)), Some(hlc(1_001))],
            "since must advance to the log watermark after each full page"
        );
        assert_eq!(
            watermark_reads.load(Ordering::SeqCst),
            2,
            "watermark is re-read once per FULL page only"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn driver_stops_on_shutdown_signal() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::NoReply
            }
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            None,
            // ZEB-599: no presence watch in this test.
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Let the driver issue request #1 and arm its 30s backoff
        // sleep. yield_now keeps the test task runnable so paused time
        // does NOT auto-advance here.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        shutdown_tx.send(true).expect("send shutdown");
        driver.await.expect("driver task");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "no retry may fire after the shutdown signal"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn driver_aborted_query_backs_off() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        // Call 1: aborted query (oneshot RecvError maps to NoReply).
        // Call 2: clean empty page → satisfied.
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    PageFetch::NoReply
                } else {
                    PageFetch::Completed(0, 256)
                }
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            None,
            // ZEB-599: no presence watch in this test.
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Just shy of the 30s backoff: the retry must NOT have fired.
        tokio::time::advance(Duration::from_millis(29_999)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "second request must wait out the full 30s backoff"
        );
        // Cross the 30s mark: retry fires, clean empty page satisfies.
        tokio::time::advance(Duration::from_millis(2)).await;
        driver.await.expect("driver task");
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn driver_exits_on_engine_gone() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        // The request closure reports the engine/adapter permanently
        // gone (query bridge closed): the driver must return instead
        // of arming eternal futile retries.
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::EngineGone
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            None,
            // ZEB-599: no presence watch in this test.
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        )
        .await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "EngineGone must end the driver after exactly one request"
        );
        // Under paused time any backoff sleep would auto-advance the
        // clock — zero elapsed proves the driver exited immediately.
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "driver must exit without arming any backoff wait"
        );
    }

    // ZEB-434 Task 7: epoch-park/re-arm on the channel-log driver.
    // (The legacy return-on-Idle path with `epoch_rx = None` is pinned
    // by `driver_retries_until_holder_appears_then_satisfies` above,
    // which awaits driver completion — satisfied → Idle → return.)
    #[tokio::test(start_paused = true)]
    async fn backfill_driver_rearms_on_epoch_bump_with_fresh_watermark() {
        let requests = Arc::new(AtomicUsize::new(0));
        let sinces: Arc<StdMutex<Vec<Option<Hlc>>>> = Arc::new(StdMutex::new(Vec::new()));
        let counter = Arc::clone(&requests);
        let since_log = Arc::clone(&sinces);
        // Every request answers with a short page (immediately satisfied).
        let request_page = move |since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            let since_log = Arc::clone(&since_log);
            async move {
                since_log.lock().unwrap().push(since);
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::Completed(0, 256)
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(Some(hlc(100))),
            request_page,
            // Watermark moved to 200 by the time the re-arm fires.
            || async { Some(hlc(200)) },
            shutdown_rx,
            Some(epoch_rx),
            // ZEB-599: no presence watch in this test.
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(!driver.is_finished(), "must park on Idle with epoch watch");
        epoch_tx.send(1).expect("bump");
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        // Re-arm must reset() with the CURRENT log watermark, not the
        // spawn-time one.
        assert_eq!(
            sinces.lock().unwrap().clone(),
            vec![Some(hlc(100)), Some(hlc(200))]
        );
        driver.abort();
    }

    /// ZEB-599 Direction 1: a presence-driven reachability kick re-arms a
    /// satisfied (Idle-parked) latch with a FULL reconcile (`since = None`),
    /// NOT the incremental watermark — so a relay-mediated cross-WAN peer's
    /// below-watermark backlog is refetched within the cooldown instead of
    /// waiting the ~1h floor. Contrast
    /// `backfill_driver_rearms_on_epoch_bump_with_fresh_watermark`, whose
    /// epoch (direct-peer) re-arm stays incremental.
    #[tokio::test(start_paused = true)]
    async fn backfill_driver_rearms_full_reconcile_on_presence_kick() {
        let requests = Arc::new(AtomicUsize::new(0));
        let sinces: Arc<StdMutex<Vec<Option<Hlc>>>> = Arc::new(StdMutex::new(Vec::new()));
        let counter = Arc::clone(&requests);
        let since_log = Arc::clone(&sinces);
        // Every request answers with a short page (immediately satisfied).
        let request_page = move |since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            let since_log = Arc::clone(&since_log);
            async move {
                since_log.lock().unwrap().push(since);
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::Completed(0, 256)
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (presence_tx, presence_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(Some(hlc(100))),
            request_page,
            // Watermark has advanced to 200 — a FULL reconcile must IGNORE it
            // and request `None`, unlike the incremental epoch re-arm.
            || async { Some(hlc(200)) },
            shutdown_rx,
            None,              // no transport-epoch watch — isolate the presence path
            Some(presence_rx), // ZEB-599 presence reachability watch
            None,              // no periodic floor — isolate the presence kick
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // First request satisfies the latch → the driver parks on Idle
        // (parked because the presence watch is wired, though no epoch/floor is).
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "must park on Idle with a presence watch (no epoch, no floor)"
        );
        // A presence kick + cooldown elapse → a second request fires as a FULL
        // reconcile (`since = None`), NOT the incremental watermark (200).
        presence_tx.send(1).expect("presence bump");
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        for _ in 0..128 {
            if requests.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sinces.lock().unwrap().clone(),
            vec![Some(hlc(100)), None],
            "presence kick must re-arm with a FULL reconcile (None), not the \
             incremental watermark"
        );
        driver.abort();
    }

    /// ZEB-599 Direction 1 (PR #384 review, CodeRabbit): a presence kick that
    /// arrives while the driver is mid-backoff (`WaitUntil`, latch unsatisfied)
    /// must re-arm with a FULL reconcile (`since = None`) after the cooldown —
    /// NOT the incremental watermark (which the epoch `WaitUntil` arm uses),
    /// and NOT the backoff-retry's original `since`. Mirror of
    /// `root_driver_epoch_bump_mid_backoff_requeries_after_cooldown`, but on the
    /// presence path and asserting the full-vs-incremental `since`.
    #[tokio::test(start_paused = true)]
    async fn backfill_driver_presence_kick_mid_backoff_requeries_full_after_cooldown() {
        let requests = Arc::new(AtomicUsize::new(0));
        let sinces: Arc<StdMutex<Vec<Option<Hlc>>>> = Arc::new(StdMutex::new(Vec::new()));
        let counter = Arc::clone(&requests);
        let since_log = Arc::clone(&sinces);
        // NoReply leaves the latch unsatisfied → the driver backs off
        // (`WaitUntil`) instead of parking on Idle.
        let request_page = move |since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            let since_log = Arc::clone(&since_log);
            async move {
                since_log.lock().unwrap().push(since);
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::NoReply
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (presence_tx, presence_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(Some(hlc(100))),
            request_page,
            // Watermark advanced to 200 — a FULL re-arm must IGNORE it.
            || async { Some(hlc(200)) },
            shutdown_rx,
            None,              // no transport-epoch watch — isolate the presence path
            Some(presence_rx), // ZEB-599 presence reachability watch
            None,              // no periodic floor — isolate the presence kick
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Request #1 fires (NoReply) → the driver enters its backoff (WaitUntil).
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "must back off (WaitUntil) on NoReply"
        );
        // Presence kick mid-backoff. Send + yield BEFORE advancing time so the
        // presence arm (ready immediately) wins the select over the not-yet-
        // elapsed backoff sleep; the driver then parks inside `cooldown_wait`,
        // abandoning the sleep future. Advancing past the cooldown boundary
        // fires the FULL reconcile deterministically.
        presence_tx.send(1).expect("presence bump");
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        for _ in 0..128 {
            if requests.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sinces.lock().unwrap().clone(),
            vec![Some(hlc(100)), None],
            "presence kick mid-backoff must re-arm FULL (None), not the \
             incremental watermark (200) nor the backoff-retry since (100)"
        );
        driver.abort();
    }

    /// ZEB-599 Direction 1 (PR #384 review, CodeRabbit): a presence SENDER drop
    /// mid-backoff must degrade the driver to no-presence mode — backoff
    /// retries continue — not exit with the latch unsatisfied (which would
    /// strand the channel without backfill until engine restart). Mirror of
    /// `backfill_driver_epoch_sender_drop_degrades_to_backoff_retries` on the
    /// presence path (exercises the `full_resync_rx = None; continue;` branch).
    #[tokio::test(start_paused = true)]
    async fn backfill_driver_presence_sender_drop_degrades_to_backoff_retries() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::NoReply
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (presence_tx, presence_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None },
            shutdown_rx,
            None,              // no transport-epoch watch — isolate the presence-drop path
            Some(presence_rx), // ZEB-599 presence reachability watch
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Request #1 fires (NoReply) and the driver enters its backoff.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Drop the presence sender, then give the driver time to process the
        // closed-watch wakeup and re-enter the (now presence-less) wait.
        drop(presence_tx);
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "presence sender drop mid-backoff must not end the driver"
        );
        tokio::time::advance(Duration::from_millis(BACKFILL_RETRY_BASE_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        driver.abort();
    }

    /// PR #230 review (CodeAnt): an epoch SENDER drop mid-backoff must
    /// degrade the driver to no-epoch mode — backoff retries continue —
    /// not exit with the latch unsatisfied (which would strand the
    /// channel without backfill until engine restart).
    #[tokio::test(start_paused = true)]
    async fn backfill_driver_epoch_sender_drop_degrades_to_backoff_retries() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::NoReply
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None },
            shutdown_rx,
            Some(epoch_rx),
            // ZEB-599: no presence watch in this test.
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Request #1 fires (NoReply) and the driver enters its backoff.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Drop the sender, then give the driver time to process the
        // closed-watch wakeup and re-enter the (now epoch-less) wait.
        drop(epoch_tx);
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "sender drop mid-backoff must not end the driver"
        );
        tokio::time::advance(Duration::from_millis(BACKFILL_RETRY_BASE_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        driver.abort();
    }

    #[test]
    fn in_flight_guard_blocks_second_request() {
        let mut latch = BackfillLatch::new(None);
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        // Second poll while in-flight must NOT hand out another Request.
        let second = latch.next_action(0);
        assert!(
            matches!(second, BackfillAction::WaitUntil(_)),
            "expected WaitUntil while in-flight, got {second:?}"
        );
        // Completing a short page clears in-flight and satisfies.
        latch.on_page_complete(
            PageOutcome {
                events: 1,
                max_hlc_seen: Some(hlc(10)),
                limit: 1000,
            },
            0,
        );
        assert_eq!(latch.next_action(0), BackfillAction::Idle);
    }

    // ── ZEB-434: RootFetchLatch + root-fetch driver tests ────────────────

    #[test]
    fn root_latch_satisfies_on_reply() {
        let mut latch = RootFetchLatch::new();
        assert!(!latch.is_satisfied());
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
        latch.on_reply(0);
        assert!(latch.is_satisfied());
        assert_eq!(latch.next_action(0), RootFetchAction::Idle);
    }

    #[test]
    fn root_latch_backs_off_exponentially_with_cap() {
        let mut latch = RootFetchLatch::new();

        // t=0: first request goes out.
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
        // No reply at t=0 → delay becomes BACKFILL_RETRY_BASE_MS (30_000);
        // next_retry_at = 0 + 30_000 = 30_000.
        latch.on_no_reply(0);
        assert_eq!(
            latch.next_action(0),
            RootFetchAction::WaitUntil(BACKFILL_RETRY_BASE_MS)
        );

        // t=30_000: eligible again.
        assert_eq!(latch.next_action(30_000), RootFetchAction::Request);
        // No reply at t=30_000 → delay doubles to 60_000;
        // next_retry_at = 30_000 + 60_000 = 90_000.
        latch.on_no_reply(30_000);
        assert_eq!(
            latch.next_action(30_000),
            RootFetchAction::WaitUntil(90_000)
        );

        // t=90_000: request; no reply → delay 120_000;
        // next_retry_at = 90_000 + 120_000 = 210_000.
        assert_eq!(latch.next_action(90_000), RootFetchAction::Request);
        latch.on_no_reply(90_000);
        assert_eq!(
            latch.next_action(90_000),
            RootFetchAction::WaitUntil(210_000)
        );

        // t=210_000: request; no reply → delay 240_000;
        // next_retry_at = 210_000 + 240_000 = 450_000.
        assert_eq!(latch.next_action(210_000), RootFetchAction::Request);
        latch.on_no_reply(210_000);
        assert_eq!(
            latch.next_action(210_000),
            RootFetchAction::WaitUntil(450_000)
        );

        // t=450_000: request; no reply → delay 480_000;
        // next_retry_at = 450_000 + 480_000 = 930_000.
        assert_eq!(latch.next_action(450_000), RootFetchAction::Request);
        latch.on_no_reply(450_000);
        assert_eq!(
            latch.next_action(450_000),
            RootFetchAction::WaitUntil(930_000)
        );

        // t=930_000: request; no reply → doubling would give 960_000,
        // CAPPED at BACKFILL_RETRY_CAP_MS (600_000);
        // next_retry_at = 930_000 + 600_000 = 1_530_000 — interval is exactly the cap.
        assert_eq!(latch.next_action(930_000), RootFetchAction::Request);
        latch.on_no_reply(930_000);
        assert_eq!(
            latch.next_action(930_000),
            RootFetchAction::WaitUntil(1_530_000)
        );

        // t=1_530_000: request; no reply → stays capped at 600_000;
        // next_retry_at = 1_530_000 + 600_000 = 2_130_000.
        assert_eq!(latch.next_action(1_530_000), RootFetchAction::Request);
        latch.on_no_reply(1_530_000);
        assert_eq!(
            latch.next_action(1_530_000),
            RootFetchAction::WaitUntil(2_130_000)
        );
    }

    #[test]
    fn root_latch_in_flight_guard() {
        let mut latch = RootFetchLatch::new();
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
        // Second poll while in-flight must NOT hand out another Request.
        let second = latch.next_action(0);
        assert!(
            matches!(second, RootFetchAction::WaitUntil(_)),
            "expected WaitUntil while in-flight, got {second:?}"
        );
    }

    #[test]
    fn root_latch_reset_unsatisfies() {
        let mut latch = RootFetchLatch::new();
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
        latch.on_reply(0);
        assert!(latch.is_satisfied());
        latch.reset();
        assert!(!latch.is_satisfied());
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_retries_until_answered_then_parks_when_epoch_some() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 2 {
                    RootFetch::NoReply
                } else {
                    RootFetch::Answered
                }
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            Some(epoch_rx),
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Request #1 fires at spawn (no sleep involved) — yield until
        // it lands. NOTE: a bare `while < 2 { yield_now }` would
        // deadlock under start_paused: the busy test task keeps the
        // runtime non-idle so paused time never auto-advances across
        // the driver's 30s backoff sleep. Advance the clock EXPLICITLY.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(30_001)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Satisfied + epoch_rx Some → the driver must PARK, not return.
        assert!(
            !driver.is_finished(),
            "driver must park on Idle when epoch watch present"
        );
        // Epoch bump → re-arm. Cooldown (60s since last request) defers
        // the re-query; advancing past it lets request #3 fire.
        epoch_tx.send(1).expect("epoch bump");
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        driver.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_returns_on_idle_when_epoch_none() {
        let request_root = move || async move { RootFetch::Answered };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            None,
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        )
        .await;
        // If we get here without hanging, the driver returned on Idle.
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_stops_on_shutdown_while_parked() {
        let answered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let answered_clone = Arc::clone(&answered);
        let request_root = move || {
            let answered_clone = Arc::clone(&answered_clone);
            async move {
                answered_clone.store(true, Ordering::SeqCst);
                RootFetch::Answered
            }
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            Some(epoch_rx),
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Wait deterministically until the request has been answered,
        // then let the driver advance from the completed request into
        // the parked select!.
        while !answered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        shutdown_tx.send(true).expect("send shutdown");
        driver.await.expect("driver task");
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_exits_on_engine_gone() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::EngineGone
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            Some(epoch_rx),
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        )
        .await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "EngineGone must end the driver after exactly one request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_epoch_bump_mid_backoff_requeries_after_cooldown() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::NoReply
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            Some(epoch_rx),
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Wait for request #1 to fire.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Bump epoch mid-backoff (driver is sleeping through 30s backoff).
        epoch_tx.send(1).expect("epoch bump");
        // Advance past the cooldown (60s since request #1 at t=0).
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        driver.abort();
    }

    /// PR #230 review (CodeAnt): an epoch SENDER drop mid-backoff must
    /// degrade the driver to no-epoch mode — backoff retries continue —
    /// not exit with the latch unsatisfied (which would strand the
    /// community without a root fetch until engine restart).
    #[tokio::test(start_paused = true)]
    async fn root_driver_epoch_sender_drop_degrades_to_backoff_retries() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::NoReply
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            Some(epoch_rx),
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Request #1 fires (NoReply) and the driver enters its 30s backoff.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Drop the sender, then give the driver time to process the
        // closed-watch wakeup and re-enter the (now epoch-less) wait.
        drop(epoch_tx);
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "sender drop mid-backoff must not end the driver"
        );
        tokio::time::advance(Duration::from_millis(BACKFILL_RETRY_BASE_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        driver.abort();
    }

    /// Pins the "deferred, not dropped" semantics of `cooldown_wait`.
    ///
    /// Sequence (paused time, all clocks start at 0):
    ///   t=0:     request #1 fires and is answered (latch satisfied).
    ///   t≈0:     epoch bump arrives a few yields later.
    ///   t≈0:     cooldown_wait measures from `last_request_at` = 0,
    ///            so the boundary is EPOCH_REARM_COOLDOWN_MS (60_000 ms).
    ///
    /// Assertion 1 (deferred): at t = 60_000 − 1 the re-arm query must
    ///   NOT have fired — the cooldown has not been crossed.
    ///
    /// Assertion 2 (not dropped): advancing 2 ms more (to t = 60_001)
    ///   crosses the boundary; the re-arm MUST fire.
    ///
    /// This test would fail if `cooldown_wait` were deleted (the re-arm
    /// would fire immediately after the epoch bump, so the "deferred"
    /// assertion at 60_000 − 1 would see count = 2, not 1).
    #[tokio::test(start_paused = true)]
    async fn root_driver_epoch_rearm_defers_to_cooldown_boundary() {
        // Satisfied latch parks; epoch bump arrives immediately after
        // the first request. The re-query must NOT fire before the
        // 60s cooldown boundary (deferred), and MUST fire after it
        // (not dropped).
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::Answered
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            Some(epoch_rx),
            None,
            None,
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Wait for request #1 (answered → satisfied). `last_request_at`
        // is stamped at this point, which is t ≈ 0 under paused time.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Send the epoch bump. The driver is now parked in the Idle
        // select!; it will receive the bump and enter cooldown_wait.
        // Paused time does NOT auto-advance (the test task keeps
        // running), so cooldown_wait's sleep does not fire yet.
        epoch_tx.send(1).expect("bump");
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Just shy of the boundary: deferred, not yet fired.
        // The cooldown target = last_request_at(≈0) + 60_000 = 60_000.
        // Advancing to 60_000 − 1 must leave the count at 1.
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS - 1)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "re-arm query must wait out the cooldown boundary"
        );
        // Cross it: fired (not dropped).
        // Advancing 2 ms brings us to 60_001, past the 60_000 boundary.
        tokio::time::advance(Duration::from_millis(2)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        driver.abort();
    }

    // ── ZEB-425: periodic re-sync anti-entropy floor ─────────────────

    /// With NO epoch bump, a satisfied channel-backfill latch must still
    /// re-arm on the periodic floor — and a re-armed latch that finds no
    /// holder must back off (never storm straight back into Idle).
    #[tokio::test(start_paused = true)]
    async fn backfill_periodic_resync_fires_then_backs_off_no_storm() {
        const RESYNC_MS: u64 = 200_000;
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        // Req #1: clean empty page → satisfied (parks on Idle).
        // Req #2 (the periodic re-arm) onward: no holder → NoReply →
        // backoff.
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    PageFetch::Completed(0, 256)
                } else {
                    PageFetch::NoReply
                }
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        // epoch_rx = None: the ONLY thing that can re-arm a satisfied
        // latch here is the periodic floor.
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            None,
            // ZEB-599: no presence watch in this test.
            None,
            Some(RESYNC_MS),
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Req #1 fires + satisfies → driver parks (resync keeps it alive
        // despite epoch_rx = None).
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "resync-enabled driver must park on Idle, not return"
        );
        // Just shy of the floor: no re-arm yet.
        tokio::time::advance(Duration::from_millis(RESYNC_MS - 1)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "periodic re-arm must not fire before the interval elapses"
        );
        // Cross the floor: re-arm fires req #2 (NoReply → backoff).
        tokio::time::advance(Duration::from_millis(2)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        // No storm: req #2 got NoReply, so the latch is now backing off
        // (30 s), NOT immediately re-Idle. A third request must wait out
        // the backoff, not fire instantly.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "a re-armed no-holder latch must back off, not storm"
        );
        tokio::time::advance(Duration::from_millis(BACKFILL_RETRY_BASE_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        driver.abort();
    }

    /// ZEB-584: the periodic anti-entropy floor must re-arm with a FULL
    /// reconcile (`since = None`), NOT the incremental log watermark. A
    /// returning member seeds the latch with `Some(max_hlc)` (efficient
    /// reconnect catch-up), but a scalar high-water mark can't recover a
    /// peer's offline-window entry whose HLC sorts below that max — the
    /// responder serves only `hlc > since`. The floor's from-scratch re-pull
    /// closes the gap. (The epoch-bump re-arm stays incremental — see
    /// `backfill_driver_rearms_on_epoch_bump_with_fresh_watermark`.)
    #[tokio::test(start_paused = true)]
    async fn backfill_periodic_resync_rearms_with_full_reconcile_none() {
        const RESYNC_MS: u64 = 200_000;
        // Capture the `since` of every request the driver issues.
        let sinces: Arc<std::sync::Mutex<Vec<Option<Hlc>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&sinces);
        let request_page = move |since: Option<Hlc>| {
            let captured = Arc::clone(&captured);
            async move {
                captured.lock().unwrap().push(since);
                // Clean empty page → satisfied, so the ONLY thing that can
                // re-issue a request here is the periodic floor.
                PageFetch::Completed(0, 256)
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        // A returning member: non-empty reloaded log → watermark Some(M).
        let watermark = hlc(500);
        let wm = watermark.clone();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(Some(watermark.clone())),
            request_page,
            // current_watermark stays Some(M): re-pulling old/dup events
            // never raises a returning member's log max, which is exactly
            // why the incremental path can't page below it.
            move || {
                let wm = wm.clone();
                async move { Some(wm) }
            },
            shutdown_rx,
            None, // epoch_rx None: ONLY the periodic floor can re-arm.
            None, // ZEB-599: no presence watch in this test.
            Some(RESYNC_MS),
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Req #1: the initial reconnect catch-up uses the incremental
        // watermark.
        while sinces.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sinces.lock().unwrap()[0],
            Some(watermark.clone()),
            "the initial reconnect request uses the incremental watermark"
        );
        // Cross the floor → the re-arm must request a FULL reconcile.
        tokio::time::advance(Duration::from_millis(RESYNC_MS + 1)).await;
        while sinces.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sinces.lock().unwrap()[1],
            None,
            "ZEB-584: the periodic floor must re-arm with since=None (full \
             reconcile) so a sub-max-HLC offline-window entry is re-fetched"
        );
        driver.abort();
    }

    // ── ZEB-599: restart-aware (persisted-deadline) resync floor ─────
    #[test]
    fn first_resync_deadline_cases() {
        // Not yet due: honor the persisted schedule (`last + interval`) so
        // a restart resumes the remaining wait instead of resetting.
        assert_eq!(
            first_resync_deadline(Some(1_000), 100_000, 5_000),
            101_000,
            "not-yet-due resumes the persisted schedule"
        );
        // Never reconciled: identical to legacy interval-from-spawn.
        assert_eq!(
            first_resync_deadline(None, 100_000, 5_000),
            105_000,
            "no persisted timestamp → now + interval (legacy)"
        );
        // Past-due: fire within the jitter window (`now + (interval -
        // FLOOR)`), never in lockstep at `now` — spreads a boot herd.
        let interval = PERIODIC_RESYNC_FLOOR_MS + 250_000;
        let now = 10 * PERIODIC_RESYNC_FLOOR_MS; // far past `0 + interval`
        assert_eq!(
            first_resync_deadline(Some(0), interval, now),
            now + 250_000,
            "past-due spreads across the jitter window, not a lockstep now-fire"
        );
        // Future/skewed stamp (last > now): clamp to now → `now + interval`,
        // never a far-future deadline that stalls the backstop.
        assert_eq!(
            first_resync_deadline(Some(5_000 + 100_000), 100_000, 5_000),
            105_000,
            "a persisted stamp ahead of now degrades to legacy now + interval"
        );
    }

    /// ZEB-599: with a persist sink the FIRST periodic full reconcile
    /// fires at the absolute (restart-aware) deadline — NOT `spawn +
    /// interval` — each fire persists a fresh wall-ms, and subsequent
    /// fires fall on the steady-state interval.
    #[tokio::test(start_paused = true)]
    async fn backfill_persist_floor_first_fire_at_deadline_then_interval() {
        const DEADLINE_MS: u64 = 40_000;
        const INTERVAL_MS: u64 = 200_000;
        let sinces: Arc<StdMutex<Vec<Option<Hlc>>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured = Arc::clone(&sinces);
        let request_page = move |since: Option<Hlc>| {
            let captured = Arc::clone(&captured);
            async move {
                captured.lock().unwrap().push(since);
                PageFetch::Completed(0, 256) // short page → satisfied → park
            }
        };
        let fired: Arc<StdMutex<Vec<u64>>> = Arc::new(StdMutex::new(Vec::new()));
        let fired_cb = Arc::clone(&fired);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            None,
            // ZEB-599: no presence watch in this test.
            None, // no epoch watch: only the floor can re-arm
            Some(INTERVAL_MS),
            move || start.elapsed().as_millis() as u64,
            Some(ResyncPersist {
                first_deadline_ms: DEADLINE_MS,
                on_full_reconcile: Arc::new(move |ts| fired_cb.lock().unwrap().push(ts)),
            }),
        ));
        // Req #1 (initial full pull) satisfies → park on the persisted deadline.
        while sinces.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Just shy of the 40s deadline: no fire. (Legacy interval-from-spawn
        // would fire at 200s, so any fire at 40s proves the absolute
        // deadline is in force.)
        tokio::time::advance(Duration::from_millis(DEADLINE_MS - 1)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sinces.lock().unwrap().len(),
            1,
            "floor must not fire before the absolute deadline"
        );
        assert!(
            fired.lock().unwrap().is_empty(),
            "nothing persisted before the first fire"
        );
        // Cross 40s → first floor fire: FULL reconcile (since=None) + persist.
        tokio::time::advance(Duration::from_millis(2)).await;
        while sinces.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sinces.lock().unwrap()[1],
            None,
            "the floor re-arms with a full reconcile (since=None)"
        );
        {
            let stamps = fired.lock().unwrap();
            assert_eq!(stamps.len(), 1, "exactly one persist per floor fire");
            assert!(
                stamps[0] >= DEADLINE_MS,
                "persisted timestamp is the fire wall-clock (>= deadline)"
            );
        }
        // Second fire is a full INTERVAL after the first (steady state),
        // NOT the 40s deadline again.
        tokio::time::advance(Duration::from_millis(INTERVAL_MS - 1)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sinces.lock().unwrap().len(),
            2,
            "second fire waits the full steady-state interval"
        );
        tokio::time::advance(Duration::from_millis(2)).await;
        while sinces.lock().unwrap().len() < 3 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            fired.lock().unwrap().len(),
            2,
            "each floor fire persists a fresh timestamp"
        );
        driver.abort();
    }

    /// With NO epoch bump, a satisfied root-fetch latch (community or mail
    /// root, both `run_root_fetch_driver`) must still re-arm on the floor.
    #[tokio::test(start_paused = true)]
    async fn root_fetch_periodic_resync_refetches_when_no_epoch_bump() {
        const RESYNC_MS: u64 = 200_000;
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        // Every request is answered → satisfied → parks each round.
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::Answered
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            None,            // no epoch watch
            None,            // no presence watch
            Some(RESYNC_MS), // periodic floor is the only re-arm
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "resync-enabled root driver must park on Idle, not return"
        );
        tokio::time::advance(Duration::from_millis(RESYNC_MS - 1)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "root re-arm must not fire before the interval elapses"
        );
        tokio::time::advance(Duration::from_millis(2)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        driver.abort();
    }

    // ── ZEB-618: run_root_fetch_driver presence-kick + persisted floor ──

    /// ZEB-618: a presence kick while Idle re-arms the root latch
    /// (cooldown-gated), producing a fresh Request — parity with
    /// `run_backfill_driver`'s Direction-1 presence arm.
    #[tokio::test(start_paused = true)]
    async fn root_driver_presence_kick_rearms() {
        let (kick_tx, kick_rx) = tokio::sync::watch::channel(0u64);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::Answered
            }
        };
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            None,          // no epoch watch
            Some(kick_rx), // presence kick wired
            None,          // floor disabled
            move || start.elapsed().as_millis() as u64,
            None, // no persist
        ));
        // Request #1 fires at spawn → answered → parks on Idle.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        // Presence kick → re-arm. Cooldown (60s since request #1 at t≈0)
        // defers the re-query; advancing past it lets request #2 fire.
        kick_tx.send_modify(|e| *e = e.wrapping_add(1));
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "presence kick must re-arm the root fetch after cooldown"
        );
        driver.abort();
    }

    /// ZEB-618: with a persist sink the FIRST floor fire lands at the
    /// absolute (restart-aware) deadline — NOT `spawn + interval` — and
    /// the fire invokes `on_full_reconcile` with the fire wall-ms.
    #[tokio::test(start_paused = true)]
    async fn root_driver_persisted_floor_first_fire_at_deadline() {
        const DEADLINE_MS: u64 = 5_000;
        const INTERVAL_MS: u64 = 60_000;
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::Answered // satisfied → parks
            }
        };
        let fired: Arc<StdMutex<Vec<u64>>> = Arc::new(StdMutex::new(Vec::new()));
        let fired_cb = Arc::clone(&fired);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            None,              // no epoch watch
            None,              // no presence watch
            Some(INTERVAL_MS), // floor: only the persisted deadline can re-arm
            move || start.elapsed().as_millis() as u64,
            Some(ResyncPersist {
                first_deadline_ms: DEADLINE_MS,
                on_full_reconcile: Arc::new(move |ts| fired_cb.lock().unwrap().push(ts)),
            }),
        ));
        // Req #1 (initial pull) satisfies → parks on the persisted deadline.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Just shy of the 5s deadline: no fire. (Legacy interval-from-spawn
        // would fire at 60s, so any fire at 5s proves the absolute deadline.)
        tokio::time::advance(Duration::from_millis(DEADLINE_MS - 1)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "floor must not fire before the absolute deadline"
        );
        assert!(
            fired.lock().unwrap().is_empty(),
            "nothing persisted before the first fire"
        );
        // Cross 5s → first floor fire: re-arm + persist.
        tokio::time::advance(Duration::from_millis(2)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        let stamps = fired.lock().unwrap();
        assert_eq!(stamps.len(), 1, "exactly one persist per floor fire");
        assert!(
            stamps[0] >= DEADLINE_MS,
            "persisted timestamp is the fire wall-clock (>= deadline)"
        );
        driver.abort();
    }

    /// ZEB-625 (1): a presence kick re-arms a FULL reconcile but must NOT
    /// advance the persisted resync floor — `on_full_reconcile` stays
    /// uncalled and the floor still fires at the ORIGINAL absolute
    /// deadline. (Kick arms do `latch.reset` only; only the resync_tick
    /// arm persists — the invariant was code-verified but never pinned.)
    #[tokio::test(start_paused = true)]
    async fn backfill_presence_kick_does_not_advance_persisted_floor() {
        const DEADLINE_MS: u64 = 200_000;
        const INTERVAL_MS: u64 = 500_000;
        let sinces: Arc<StdMutex<Vec<Option<Hlc>>>> = Arc::new(StdMutex::new(Vec::new()));
        let since_log = Arc::clone(&sinces);
        let request_page = move |since: Option<Hlc>| {
            let since_log = Arc::clone(&since_log);
            async move {
                since_log.lock().unwrap().push(since);
                PageFetch::Completed(0, 256) // short page → satisfied → park
            }
        };
        let fired: Arc<StdMutex<Vec<u64>>> = Arc::new(StdMutex::new(Vec::new()));
        let fired_cb = Arc::clone(&fired);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (presence_tx, presence_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            None,              // no transport-epoch watch — isolate presence vs floor
            Some(presence_rx), // presence kick wired
            Some(INTERVAL_MS),
            move || start.elapsed().as_millis() as u64,
            Some(ResyncPersist {
                first_deadline_ms: DEADLINE_MS,
                on_full_reconcile: Arc::new(move |ts| fired_cb.lock().unwrap().push(ts)),
            }),
        ));
        // Req #1 at spawn satisfies → parks on Idle.
        while sinces.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Presence kick past the re-arm cooldown → req #2 (full reconcile).
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        presence_tx.send(1).expect("presence bump");
        for _ in 0..128 {
            if sinces.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(sinces.lock().unwrap().len(), 2, "kick re-armed a request");
        assert!(
            fired.lock().unwrap().is_empty(),
            "ZEB-625: a presence kick must NOT invoke on_full_reconcile"
        );
        // The floor still fires at the ORIGINAL absolute deadline: just shy
        // → nothing; cross it → exactly one persisted fire.
        let now = start.elapsed().as_millis() as u64;
        tokio::time::advance(Duration::from_millis((DEADLINE_MS - 1).saturating_sub(now))).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            fired.lock().unwrap().is_empty(),
            "floor must not fire early — the kick moved nothing"
        );
        tokio::time::advance(Duration::from_millis(2)).await;
        for _ in 0..128 {
            if !fired.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let stamps = fired.lock().unwrap();
        assert_eq!(stamps.len(), 1, "exactly one floor fire");
        assert!(
            stamps[0] >= DEADLINE_MS,
            "fire at the original absolute deadline (kick did not advance it)"
        );
        driver.abort();
    }

    /// ZEB-625 (1): root-driver twin of
    /// `backfill_presence_kick_does_not_advance_persisted_floor`.
    #[tokio::test(start_paused = true)]
    async fn root_presence_kick_does_not_advance_persisted_floor() {
        const DEADLINE_MS: u64 = 200_000;
        const INTERVAL_MS: u64 = 500_000;
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::Answered // satisfied → parks
            }
        };
        let fired: Arc<StdMutex<Vec<u64>>> = Arc::new(StdMutex::new(Vec::new()));
        let fired_cb = Arc::clone(&fired);
        let (kick_tx, kick_rx) = tokio::sync::watch::channel(0u64);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            None,          // no epoch watch
            Some(kick_rx), // presence kick wired
            Some(INTERVAL_MS),
            move || start.elapsed().as_millis() as u64,
            Some(ResyncPersist {
                first_deadline_ms: DEADLINE_MS,
                on_full_reconcile: Arc::new(move |ts| fired_cb.lock().unwrap().push(ts)),
            }),
        ));
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Presence kick past the cooldown → req #2; no persist.
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        kick_tx.send_modify(|e| *e = e.wrapping_add(1));
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert!(
            fired.lock().unwrap().is_empty(),
            "ZEB-625: a presence kick must NOT invoke on_full_reconcile"
        );
        // Floor fires at the ORIGINAL absolute deadline.
        let now = start.elapsed().as_millis() as u64;
        tokio::time::advance(Duration::from_millis((DEADLINE_MS - 1).saturating_sub(now))).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            fired.lock().unwrap().is_empty(),
            "floor must not fire early — the kick moved nothing"
        );
        tokio::time::advance(Duration::from_millis(2)).await;
        for _ in 0..128 {
            if !fired.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let stamps = fired.lock().unwrap();
        assert_eq!(stamps.len(), 1, "exactly one floor fire");
        assert!(
            stamps[0] >= DEADLINE_MS,
            "fire at the original absolute deadline (kick did not advance it)"
        );
        driver.abort();
    }

    /// ZEB-625 (2): a presence kick arriving MID-BACKOFF (the root
    /// driver's WaitUntil arm) re-arms the latch after the cooldown —
    /// direct coverage for the arm previously validated only by
    /// mirror-faithfulness with the backfill driver. The 600s backoff is
    /// load-bearing: with the default 30s base, the ordinary backoff retry
    /// fires inside the 60s cooldown advance and a page-less req #2 is
    /// indistinguishable from the kick's — the test would pass with the
    /// presence arm deleted (task-4 review, mutation-verified).
    #[tokio::test(start_paused = true)]
    async fn root_presence_kick_mid_backoff_rearms() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::NoReply // unanswered → the latch backs off (WaitUntil)
            }
        };
        let (kick_tx, kick_rx) = tokio::sync::watch::channel(0u64);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            // Backoff parked far past the cooldown window (see doc comment):
            // the presence kick is the ONLY possible source of req #2.
            RootFetchLatch::new_with_backoff(600_000, 600_000),
            request_root,
            shutdown_rx,
            None,          // no epoch watch
            Some(kick_rx), // presence kick wired
            None,          // no floor — isolate the WaitUntil presence arm
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Req #1 fires and goes unanswered → the driver parks in WaitUntil.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "driver parks in WaitUntil (NoReply backoff), not exit"
        );
        // Mid-backoff presence kick; the cooldown defers the re-query
        // until we advance past it.
        kick_tx.send_modify(|e| *e = e.wrapping_add(1));
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "req #2 must come from the mid-backoff presence kick (backoff parked at 600s)"
        );
        driver.abort();
    }

    /// ZEB-618: presence-kick sender drop degrades to None — the driver
    /// keeps running on the periodic floor (mirrors epoch-sender-drop).
    #[tokio::test(start_paused = true)]
    async fn root_driver_presence_sender_drop_degrades() {
        const RESYNC_MS: u64 = 200_000;
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::Answered // satisfied → parks
            }
        };
        let (kick_tx, kick_rx) = tokio::sync::watch::channel(0u64);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            None,            // no epoch watch
            Some(kick_rx),   // presence kick wired...
            Some(RESYNC_MS), // ...but the floor is the backstop
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Req #1 fires → answered → parks on Idle.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Drop the presence sender; the driver must degrade to no-presence
        // mode (not exit) and keep parking on the floor.
        drop(kick_tx);
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "presence sender drop must not end the driver"
        );
        // The floor still re-arms after the presence watch is gone.
        tokio::time::advance(Duration::from_millis(RESYNC_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "floor must still re-arm after presence sender drop"
        );
        driver.abort();
    }

    /// Shutdown must end the driver promptly even while it is parked on
    /// the periodic-resync floor (no epoch watch) — not block for the
    /// full interval.
    #[tokio::test(start_paused = true)]
    async fn backfill_resync_enabled_stops_on_shutdown_while_parked() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::Completed(0, 256) // satisfied → parks
            }
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            None,
            // ZEB-599: no presence watch in this test.
            None,
            Some(PERIODIC_RESYNC_FLOOR_MS), // the real 1 h floor
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(!driver.is_finished(), "must park on the resync floor");
        // Shutdown while parked — must win against the 1 h resync sleep.
        shutdown_tx.send(true).expect("send shutdown");
        driver.await.expect("driver task");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "no re-arm may fire once shutdown is signaled"
        );
    }

    /// `Some(0)` must behave exactly like `None` (disabled): the driver
    /// returns on Idle instead of spinning the resync arm with no delay
    /// (CodeAnt PR #257 — a zero interval would hot-loop re-arm/fetch).
    #[tokio::test(start_paused = true)]
    async fn resync_zero_interval_is_disabled_returns_on_idle() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::Completed(0, 256) // satisfied
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        // epoch None + resync Some(0): 0 == disabled → return on Idle.
        // If 0 were honored as an interval this future would never
        // complete (hot loop / park), hanging the test.
        run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            None,
            // ZEB-599: no presence watch in this test.
            None,
            Some(0),
            move || start.elapsed().as_millis() as u64,
            None,
        )
        .await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "Some(0) must be disabled: one request, then return on Idle (no spin)"
        );
    }
}
