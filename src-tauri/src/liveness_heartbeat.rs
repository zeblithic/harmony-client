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
use crate::owner_state::{refresh_self_liveness, LivenessRefreshOutcome};

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
/// conditional self-liveness refresh, returning its [`LivenessRefreshOutcome`]
/// (the caller uses `.wrote()` to decide `notify_dirty()`, and `ClockRegressed`
/// to log on transition). Engine-free by design, keeping this unit trivially
/// testable with just a doc + key.
pub async fn run_liveness_heartbeat_once(
    doc: &Arc<tokio::sync::Mutex<OwnerState>>,
    device_sk: &ed25519_dalek::SigningKey,
    now_secs: u64,
) -> LivenessRefreshOutcome {
    let mut g = doc.lock().await;
    refresh_self_liveness(&mut g, device_sk, now_secs)
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
        // ZEB-721: log a regressed clock only on the healthy→regressed transition,
        // not every hourly tick, so a persistently broken clock doesn't spam WARN.
        let mut clock_was_regressed = false;
        loop {
            tick.tick().await;
            let Some(now) = now_unix_secs() else {
                tracing::warn!(
                    target: "harmony_liveness",
                    "system clock is before the Unix epoch; skipping this heartbeat tick"
                );
                continue;
            };
            let outcome = run_liveness_heartbeat_once(&doc, &device_sk, now).await;
            match outcome {
                LivenessRefreshOutcome::ClockRegressed { skew_secs } => {
                    if !clock_was_regressed {
                        tracing::warn!(
                            target: "harmony_liveness",
                            skew_secs,
                            "self-liveness cert is stamped in the future — host clock regressed; not renewing until the clock recovers"
                        );
                        clock_was_regressed = true;
                    }
                }
                _ => clock_was_regressed = false,
            }
            if outcome.wrote() {
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
            !run_liveness_heartbeat_once(&doc, &device_signing_key, now)
                .await
                .wrote(),
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
            !run_liveness_heartbeat_once(&doc, &device_signing_key, t0)
                .await
                .wrote(),
            "cert is fresh at t0"
        );
        // Advance past the refresh threshold (freshness / 2).
        let later = t0 + harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS / 2 + 1;
        assert!(
            run_liveness_heartbeat_once(&doc, &device_signing_key, later)
                .await
                .wrote(),
            "a stale cert must be re-signed"
        );
        assert!(
            !run_liveness_heartbeat_once(&doc, &device_signing_key, later)
                .await
                .wrote(),
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
            run_liveness_heartbeat_once(&doc, &device_signing_key, now)
                .await
                .wrote(),
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
    async fn heartbeat_once_reports_regressed_clock_and_does_not_resign() {
        // ZEB-721: a host clock that moves *behind* an already-signed cert must not
        // re-sign (a lower timestamp would lose the liveness CRDT merge), must leave
        // the cert timestamp untouched, and must report `ClockRegressed` (the spawn
        // loop logs it on the healthy→regressed transition, deduplicated).
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
        assert_eq!(
            run_liveness_heartbeat_once(&doc, &device_signing_key, regressed).await,
            LivenessRefreshOutcome::ClockRegressed {
                skew_secs: t0 - regressed
            },
            "a future-stamped cert must report ClockRegressed and not re-sign"
        );
        let g = doc.lock().await;
        assert_eq!(
            g.liveness.get(&device_id).unwrap().timestamp,
            t0,
            "the cert timestamp must not move backwards"
        );
    }
}
