//! ZEB-410 — periodic multi-device liveness heartbeat.
//!
//! Re-signs the local device's `LivenessCert` on a timer (node-start + hourly)
//! so a device — especially a headless `serve` node that never opens the Devices
//! panel — stays `Full` in siblings' `evaluate_trust`. Reuses the conditional
//! `refresh_self_liveness` (~15-day re-sign gate) and the ZEB-668 S1 trust-sync
//! propagation path (`FleetSyncEngine::notify_dirty` → debounced sibling sync +
//! persist). The `run_once`/`spawn` split mirrors `community_voting_tick`.

use std::sync::Arc;
use std::time::Duration;

use harmony_owner::state::OwnerState;

use crate::fleet_sync::FleetSyncEngine;
use crate::owner_state::refresh_self_liveness;

/// Heartbeat check cadence. The first `interval.tick()` fires immediately, so
/// this is also the node-start refresh. Almost every tick is a cheap no-op —
/// `refresh_self_liveness` only re-signs when the cert has aged past ~15 days —
/// so an actual re-sign + replicate fires only ~once per fortnight per device.
/// 1 h matches the existing `reachability_publisher::IDLE_REFRESH_INTERVAL`.
pub const LIVENESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Seconds since the Unix epoch, or `None` if the host clock is before the epoch
/// (a broken/unset clock). Callers must treat `None` as "skip this tick" rather
/// than substituting 0 — signing a liveness cert stamped at 0 would be instantly
/// stale to every peer.
fn now_unix_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .ok()
}

/// One heartbeat iteration: lock the resident trust doc and run the existing
/// conditional self-liveness refresh. Returns whether it re-signed (so the
/// caller can `notify_dirty()` + log). Engine-free by design — mirrors the panel
/// path's `refresh -> if refreshed notify_dirty` split (`owner_commands.rs:729`/
/// `734`), keeping this unit trivially testable with just a doc + key.
pub async fn run_liveness_heartbeat_once(
    doc: &Arc<tokio::sync::Mutex<OwnerState>>,
    device_sk: &ed25519_dalek::SigningKey,
    now_secs: u64,
) -> bool {
    let mut g = doc.lock().await;
    // ZEB-721: regressed-clock detection (cert stamped in the future) now lives in
    // the shared `refresh_self_liveness`, which warns once and reports
    // `ClockRegressed`. Here we only need whether it wrote, to nudge the engine.
    refresh_self_liveness(&mut g, device_sk, now_secs).wrote()
}

/// Spawn the interval loop. On the rare tick that actually re-signs, nudge the
/// `owner-trust-v1` engine so the fresh cert replicates to siblings and persists
/// (the same path the on-panel-load refresh uses). The task runs until aborted
/// via its `JoinHandle` on node stop.
pub fn spawn_liveness_heartbeat(
    doc: Arc<tokio::sync::Mutex<OwnerState>>,
    engine: Arc<FleetSyncEngine<OwnerState>>,
    device_sk: Arc<ed25519_dalek::SigningKey>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            let Some(now) = now_unix_secs() else {
                tracing::warn!(
                    target: "harmony_liveness",
                    "system clock is before the Unix epoch; skipping this heartbeat tick"
                );
                continue;
            };
            if run_liveness_heartbeat_once(&doc, &device_sk, now).await {
                engine.notify_dirty();
                tracing::info!(
                    target: "harmony_liveness",
                    "self-liveness heartbeat re-signed + queued for sibling sync"
                );
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_owner::lifecycle::{mint_owner, MintResult};
    use std::sync::Arc;

    // run_liveness_heartbeat_once faithfully reflects refresh_self_liveness's
    // conditional (~15-day) gate through the async lock: a just-written cert is
    // fresh (no re-sign), and a cert aged past freshness/2 re-signs then is fresh
    // again. The fresh/missing/stale *decision* is already covered by
    // owner_state.rs:1739/1765/1779; these pin the async wrapper + return contract.

    #[tokio::test]
    async fn heartbeat_once_noop_when_fresh() {
        let now = 1_700_000_333;
        let MintResult {
            state,
            device_signing_key,
            ..
        } = mint_owner(now).unwrap();
        let doc = Arc::new(tokio::sync::Mutex::new(state));
        // Guarantee a fresh cert exists at `now` (idempotent regardless of mint).
        let _ = run_liveness_heartbeat_once(&doc, &device_signing_key, now).await;
        // A just-written cert is fresh → no re-sign.
        assert!(
            !run_liveness_heartbeat_once(&doc, &device_signing_key, now).await,
            "a fresh cert must not be re-signed"
        );
    }

    #[tokio::test]
    async fn heartbeat_once_resigns_when_stale() {
        let t0 = 1_700_000_000;
        let MintResult {
            state,
            device_signing_key,
            ..
        } = mint_owner(t0).unwrap();
        let doc = Arc::new(tokio::sync::Mutex::new(state));
        let _ = run_liveness_heartbeat_once(&doc, &device_signing_key, t0).await;
        assert!(
            !run_liveness_heartbeat_once(&doc, &device_signing_key, t0).await,
            "cert is fresh at t0"
        );
        // Advance past the refresh threshold (freshness / 2).
        let later = t0 + harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS / 2 + 1;
        assert!(
            run_liveness_heartbeat_once(&doc, &device_signing_key, later).await,
            "a stale cert must be re-signed"
        );
        assert!(
            !run_liveness_heartbeat_once(&doc, &device_signing_key, later).await,
            "the re-signed cert is fresh again at `later`"
        );
        // The re-signed cert is stamped at `later` (timestamp advanced).
        let g = doc.lock().await;
        let device_id = *g.enrollments.keys().next().unwrap();
        assert_eq!(
            g.liveness.get(&device_id).unwrap().timestamp,
            later,
            "the re-signed cert timestamp advanced to `later`"
        );
    }

    #[tokio::test]
    async fn heartbeat_once_publishes_when_missing() {
        let now = 1_700_000_222;
        let MintResult {
            mut state,
            device_signing_key,
            ..
        } = mint_owner(now).unwrap();
        state.liveness.clear(); // legacy identity with no self-liveness
        let device_id = *state.enrollments.keys().next().unwrap();
        let doc = Arc::new(tokio::sync::Mutex::new(state));
        assert!(
            run_liveness_heartbeat_once(&doc, &device_signing_key, now).await,
            "a missing cert must be published"
        );
        let g = doc.lock().await;
        assert_eq!(
            g.liveness
                .get(&device_id)
                .expect("cert present after refresh")
                .timestamp,
            now,
            "the new cert is stamped at `now`"
        );
    }

    #[tokio::test]
    async fn heartbeat_once_noop_on_regressed_clock() {
        // ZEB-721: a host clock that moves *behind* an already-signed cert must not
        // re-sign (a lower timestamp would lose the liveness CRDT merge) and must
        // leave the cert timestamp untouched — it only logs a warning.
        let t0 = 1_700_000_000;
        let MintResult {
            state,
            device_signing_key,
            ..
        } = mint_owner(t0).unwrap();
        let doc = Arc::new(tokio::sync::Mutex::new(state));
        let _ = run_liveness_heartbeat_once(&doc, &device_signing_key, t0).await;
        let device_id = {
            let g = doc.lock().await;
            *g.enrollments.keys().next().unwrap()
        };
        // Clock regresses 100 days behind the cert.
        let regressed = t0 - 100 * 24 * 60 * 60;
        assert!(
            !run_liveness_heartbeat_once(&doc, &device_signing_key, regressed).await,
            "a future-stamped cert must not be re-signed under a regressed clock"
        );
        let g = doc.lock().await;
        assert_eq!(
            g.liveness.get(&device_id).unwrap().timestamp,
            t0,
            "the cert timestamp must not move backwards"
        );
    }
}
