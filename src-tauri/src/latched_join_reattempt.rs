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
