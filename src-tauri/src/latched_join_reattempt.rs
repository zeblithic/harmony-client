//! ZEB-903: reachability-driven re-attempt driver for latched-pending
//! iroh joins.
//!
//! When an iroh invite redeem fails post-write, ZEB-899 latches a
//! pending Space (`joined` + `pending: true`) instead of falsely
//! reporting `inviter_unreachable`. Without this module, convergence
//! from that latched state is passive — the joiner waits for CRDT-sync
//! gossip on the next session (minutes). The driver here subscribes to
//! the transport-epoch watch (bumped on Zenoh peer up-edges,
//! `event_loop.rs`) and re-runs the one-round-trip fast handshake, so
//! convergence takes seconds once reachability returns.
//!
//! Spec: `docs/superpowers/specs/2026-08-12-zeb903-latched-join-reattempt-design.md`.

/// Minimum spacing between re-attempts per community. An up-edge inside
/// the window is deferred to the boundary (not dropped), mirroring
/// `channel_backfill::cooldown_wait`.
pub const REATTEMPT_COOLDOWN_MS: u64 = 30_000;

/// True = proceed with the attempt (immediately, or after deferring to
/// the cooldown boundary). False = the shutdown watch flipped (or its
/// sender dropped) during the wait — the caller must exit.
///
/// Uses `tokio::time::Instant` exclusively (no wall-clock reads) so
/// paused-clock tests can drive the boundary deterministically.
async fn cooldown_wait(
    last_attempt: Option<tokio::time::Instant>,
    shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    let Some(last) = last_attempt else {
        return true;
    };
    let target = last + std::time::Duration::from_millis(REATTEMPT_COOLDOWN_MS);
    if tokio::time::Instant::now() >= target {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep_until(target) => true,
        changed = shutdown_rx.changed() => {
            !(changed.is_err() || *shutdown_rx.borrow())
        }
    }
}

/// Owned-clone bundle of everything
/// `connectivity_redeem_invite_iroh_inner` needs, captured by the IPC
/// impl BEFORE its own inner call (which consumes its arguments).
/// `sink` is optional so tests can omit event emission; production
/// passes the real `NodeEventSink`.
pub struct ReattemptContext {
    pub invite_url: String,
    pub pkarr_resolver: Option<std::sync::Arc<harmony_pkarr::PkarrResolver>>,
    pub reachability_resolver: Option<crate::reachability_resolver::ReachabilityResolver>,
    pub iroh_endpoint: Option<std::sync::Arc<crate::iroh_endpoint::IrohEndpoint>>,
    pub crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    pub hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<harmony_crdt_sync::ReplayTracker<String, crate::owner_state_types::Hlc>>,
    >,
    pub adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
    pub device_id: String,
    pub self_owner: crate::owner_state_types::OwnerAddr,
    pub community_signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    pub enrollment_cert: harmony_owner::certs::EnrollmentCert,
    pub community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    pub community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    pub transport_epoch_rx: Option<tokio::sync::watch::Receiver<u64>>,
    pub dm_outbox: std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    pub channel_log_registry:
        std::sync::Arc<crate::community_channel_log_engine::ChannelLogRegistry>,
    pub sync_engine: Option<std::sync::Arc<crate::owner_state_sync::SyncEngine>>,
    pub identity_dir: Option<std::path::PathBuf>,
    pub sink: Option<std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>>,
    pub dial_config: crate::HandshakeDialConfig,
}

/// Register (latest-wins) + spawn the per-community re-attempt driver.
/// Returns the task handle for tests; production drops it. `None` =
/// nothing spawned: the URL failed to decode (cannot happen right after
/// a successful redeem of the same URL) or there is no transport-epoch
/// watch to subscribe to (some tests / degraded boot — the passive
/// convergence paths still apply).
pub async fn spawn_reattempt_driver(ctx: ReattemptContext) -> Option<tokio::task::JoinHandle<()>> {
    let payload = crate::community_invite::decode_invite_url(&ctx.invite_url).ok()?;
    let community_id = payload.community_id;
    let mut epoch_rx = ctx.transport_epoch_rx.clone()?;
    // CodeAnt PR #664 r1: fix the seen-version HERE, before the task is
    // spawned — not at task start. A cloned receiver inherits its
    // source's (possibly ancient) seen version, and marking it at task
    // start instead would discard any up-edge that lands between this
    // call and the task's first poll. After this line the invariant is
    // exact: every bump AFTER spawn triggers, everything before is seen.
    epoch_rx.borrow_and_update();
    let (registration_gen, shutdown_rx) = ctx
        .community_registry
        .register_latched_reattempt(community_id)
        .await;
    if *shutdown_rx.borrow() {
        // Registry already closed (registration raced shutdown_all —
        // CodeRabbit r1): nothing to arm.
        return None;
    }
    tracing::info!(
        community_id = %hex::encode(community_id.0),
        "ZEB-903: latched-join re-attempt driver armed"
    );
    Some(tokio::spawn(run_reattempt_driver(
        ctx,
        community_id,
        registration_gen,
        epoch_rx,
        shutdown_rx,
    )))
}

/// The demand predicate: the Space still exists, still carries
/// `pending_join_at`, AND has not been left. Space gone or pending
/// cleared (gossip convergence or a manual retry finished the join) ⇒
/// the demand collapsed. The `left_at` gate (CodeAnt PR #664 r1) is
/// load-bearing: leaving a community retains the row as a tombstone
/// WITHOUT clearing `pending_join_at`, and the redeem commit's ZEB-427
/// rejoin path deliberately clears `left_at` — so a background driver
/// that ignored `left_at` would rejoin (and relist) a community the
/// user explicitly left. A manual re-redeem of the same URL remains an
/// intentional rejoin; only the background path is gated.
fn space_demand_exists(
    state: &crate::owner_state_crdt::OwnerState,
    community_id: &crate::owner_state_types::SpaceId,
) -> bool {
    state
        .spaces
        .get(community_id)
        .is_some_and(|s| s.pending_join_at.is_some() && s.left_at.is_none())
}

/// Async wrapper over [`space_demand_exists`] for the driver loop.
async fn space_still_pending(
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    community_id: &crate::owner_state_types::SpaceId,
) -> bool {
    let g = crdt_state.lock().await;
    space_demand_exists(&g, community_id)
}

/// The driver loop. Reacts to FUTURE transport up-edges only (the latch
/// was committed seconds after a failed handshake — re-dialing
/// immediately would just re-fail), defers attempts to the cooldown
/// boundary, and collapses when the demand disappears, the join
/// converges, shutdown flips, or the epoch sender is gone.
async fn run_reattempt_driver(
    ctx: ReattemptContext,
    community_id: crate::owner_state_types::SpaceId,
    registration_gen: u64,
    mut epoch_rx: tokio::sync::watch::Receiver<u64>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut last_attempt: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
            changed = epoch_rx.changed() => {
                if changed.is_err() {
                    // Event loop gone — no further reachability signals
                    // will ever arrive.
                    break;
                }
                epoch_rx.borrow_and_update();
            }
        }
        if !cooldown_wait(last_attempt, &mut shutdown_rx).await {
            break;
        }
        if !space_still_pending(&ctx.crdt_state, &community_id).await {
            break;
        }
        last_attempt = Some(tokio::time::Instant::now());
        match attempt_once(&ctx, community_id, &shutdown_rx).await {
            Ok(outcome) if outcome.status == "joined" && !outcome.pending => {
                tracing::info!(
                    community_id = %hex::encode(community_id.0),
                    "ZEB-903: re-attempt converged the latched pending join"
                );
                break;
            }
            Ok(outcome) => {
                tracing::debug!(
                    community_id = %hex::encode(community_id.0),
                    status = %outcome.status,
                    pending = outcome.pending,
                    "ZEB-903: re-attempt did not converge; waiting for the next up-edge"
                );
            }
            Err(e) => {
                tracing::debug!(
                    community_id = %hex::encode(community_id.0),
                    error = %e,
                    "ZEB-903: re-attempt errored; waiting for the next up-edge"
                );
            }
        }
    }
    ctx.community_registry
        .unregister_latched_reattempt(&community_id, registration_gen)
        .await;
}

/// One full handshake attempt. No-op progress sink (a background driver
/// must never ghost-drive the redeem dialog's stage display); real nav
/// sink when a `NodeEventSink` is present (the sidebar refreshes when
/// background convergence lands). The fence checks TWO things at the
/// inner's pre-commit gate: this driver's own shutdown watch (a
/// teardown racing an in-flight attempt suppresses the commit exactly
/// like a generation trip), and — CodeAnt PR #664 r1 — that the demand
/// still exists (a leave landing DURING the attempt must not be
/// resurrected by the commit, whose ZEB-427 rejoin path clears
/// `left_at`). The demand re-check uses `try_lock`: the inner does not
/// hold the owner-state lock at fence time, so contention is rare — and
/// on contention the attempt aborts conservatively (the next up-edge
/// retries). A sub-lock-width residual window remains between the
/// fence and the commit's own lock acquisition; the manual redeem path
/// has the same (wider) window today, and closing it fully would need a
/// leave-aware commit inside the shared inner — out of scope here. The
/// driver deliberately holds no NodeState reference: its lifecycle is
/// the registry's lifecycle, and node restart runs `shutdown_all` on
/// the old registry (spec §2.2).
async fn attempt_once(
    ctx: &ReattemptContext,
    community_id: crate::owner_state_types::SpaceId,
    shutdown_rx: &tokio::sync::watch::Receiver<bool>,
) -> Result<crate::RedemptionOutcome, crate::community_invite::RedeemInviteError> {
    let fence_rx = shutdown_rx.clone();
    let fence_crdt = std::sync::Arc::clone(&ctx.crdt_state);
    let fence_check = move || -> Result<(), crate::community_invite::RedeemInviteError> {
        if *fence_rx.borrow() {
            return Err(crate::community_invite::RedeemInviteError::new(
                crate::community_invite::RedeemInviteErrorCode::GenerationChanged,
                "latched-join re-attempt driver shut down mid-attempt; commit suppressed"
                    .to_string(),
            ));
        }
        let demand_ok = match fence_crdt.try_lock() {
            Ok(g) => space_demand_exists(&g, &community_id),
            // Contention: cannot verify the demand — abort conservatively.
            Err(_) => false,
        };
        if !demand_ok {
            return Err(crate::community_invite::RedeemInviteError::new(
                crate::community_invite::RedeemInviteErrorCode::GenerationChanged,
                "latched-join demand collapsed mid-attempt (converged, left, or \
                 unverifiable); commit suppressed"
                    .to_string(),
            ));
        }
        Ok(())
    };
    let sink = ctx.sink.clone();
    let nav_emit_sink = move |payload: crate::NavUpdatedPayload| {
        if let Some(s) = sink.as_ref() {
            crate::node_event_sink::emit_ser(s.as_ref(), "nav-updated", &payload);
        }
    };
    crate::connectivity_redeem_invite_iroh_inner(
        ctx.invite_url.clone(),
        ctx.pkarr_resolver.clone(),
        ctx.reachability_resolver.clone(),
        ctx.iroh_endpoint.clone(),
        std::sync::Arc::clone(&ctx.crdt_state),
        std::sync::Arc::clone(&ctx.hlc_tracker),
        ctx.adopt_floor.clone(),
        ctx.device_id.clone(),
        ctx.self_owner,
        std::sync::Arc::clone(&ctx.community_signing_key),
        ctx.enrollment_cert.clone(),
        std::sync::Arc::clone(&ctx.community_registry),
        ctx.community_adapter_tx.clone(),
        ctx.transport_epoch_rx.clone(),
        std::sync::Arc::clone(&ctx.dm_outbox),
        std::sync::Arc::clone(&ctx.channel_log_registry),
        ctx.sync_engine.clone(),
        ctx.identity_dir.clone(),
        |_progress| {},
        nav_emit_sink,
        ctx.dial_config,
        fence_check,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §2.1 / plan U1: no prior attempt is immediate; a prior
    /// attempt defers to the cooldown boundary (not dropped, not early).
    #[tokio::test(start_paused = true)]
    async fn cooldown_defers_to_boundary_not_drops() {
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        assert!(
            cooldown_wait(None, &mut rx).await,
            "no prior attempt must proceed immediately"
        );
        let start = tokio::time::Instant::now();
        assert!(
            cooldown_wait(Some(start), &mut rx).await,
            "cooldown must proceed once the boundary is reached"
        );
        assert!(
            tokio::time::Instant::now()
                >= start + std::time::Duration::from_millis(REATTEMPT_COOLDOWN_MS),
            "cooldown must defer to the boundary, not return early"
        );
    }

    /// Spec §2.1 / plan U1: a shutdown flip during the cooldown wait
    /// aborts (returns false) instead of proceeding at the boundary.
    #[tokio::test(start_paused = true)]
    async fn cooldown_aborts_on_shutdown_flip() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let wait =
            tokio::spawn(
                async move { cooldown_wait(Some(tokio::time::Instant::now()), &mut rx).await },
            );
        tx.send(true).expect("send shutdown");
        assert!(
            !wait.await.expect("join cooldown task"),
            "shutdown during cooldown must return false"
        );
    }

    /// A dropped shutdown sender (registry entry gone) is equivalent to
    /// an explicit shutdown — the wait must abort, not hang or proceed.
    #[tokio::test(start_paused = true)]
    async fn cooldown_aborts_on_sender_drop() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let wait =
            tokio::spawn(
                async move { cooldown_wait(Some(tokio::time::Instant::now()), &mut rx).await },
            );
        drop(tx);
        assert!(
            !wait.await.expect("join cooldown task"),
            "sender drop during cooldown must return false"
        );
    }
}
