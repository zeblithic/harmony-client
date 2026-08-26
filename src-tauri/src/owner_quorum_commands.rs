//! ZEB-677 S3: quorum revocation ceremony IPCs — `request_quorum_revocation`
//! / `cosign_quorum_request` / `decline_quorum_request`. The pure planner
//! and the doc-mutating cores are NodeState-free (unit- and
//! integration-testable); the `_inner` seams follow `revoke_device_inner`
//! (keychain injected via `KeychainFactory`, ZEB-428), and the `_impl`
//! seams mirror `revoke_device_impl` for the RPC/IPC split (ZEB-445).
//!
//! Ceremony shape (spec §4 + `owner_quorum_sync` module docs): the S1
//! payload binds the signer set, so every detached signature covers the
//! sorted pair `[initiator, cosigner]`. The initiator pre-signs one
//! payload per eligible cosigner at creation (authenticating the request);
//! a co-signer verifies that part before adding its own; the initiator's
//! completion sweep (`owner_quorum_sync::run_quorum_sweep`) assembles and
//! applies the cert through the trust doc's validating `add_revocation`.

use crate::identity_commands::run_blocking;
use crate::owner_commands::{prod_keychain, KeychainFactory};
use crate::owner_quorum_sync::{QuorumReqDoc, QuorumRequestKind, MAX_QUORUM_REQUESTS};
use crate::owner_state::load_owner_state;
use harmony_owner::state::OwnerState;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ZEB-548 Stage 2: `is_master_issued` moved down to the issuer-policy
// chokepoint (`enrollment_verify`, core-types); re-imported so this module's
// call sites resolve unchanged.
pub(crate) use crate::enrollment_verify::is_master_issued;

// ZEB-548 Stage 2: the pure ceremony planners
// (`plan_quorum_revocation_request`, `plan_quorum_epoch_bump_request`,
// `cosign_request_core`, `decline_request_core`) moved down beside the
// `QuorumRequest`/`QuorumReqDoc` types they build (`owner_quorum_sync`);
// re-imported so this module's `_impl` bodies resolve unchanged.
pub(crate) use crate::owner_quorum_sync::{
    cosign_request_core, decline_request_core, plan_quorum_epoch_bump_request,
    plan_quorum_revocation_request,
};

/// Snapshot the resident handles the ceremony IPCs need. All three
/// require the node running with an owner loaded.
type QuorumHandles = (
    std::sync::Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    std::sync::Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
    std::sync::Arc<tokio::sync::Mutex<OwnerState>>,
    std::path::PathBuf,
);

fn snapshot_handles(state: &Mutex<crate::NodeState>) -> Result<QuorumHandles, String> {
    let g = state
        .lock()
        .map_err(|e| format!("NodeState poisoned: {e}"))?;
    let (Some(doc), Some(engine), Some(trust_doc)) = (
        g.owner_quorum_doc.clone(),
        g.owner_quorum_sync.clone(),
        g.owner_trust_doc.clone(),
    ) else {
        return Err(
            "nodeNotRunning: start the node to run a co-sign ceremony (requests must replicate \
             to your other devices)"
                .to_string(),
        );
    };
    let dir = match g.identity_dir.clone() {
        Some(d) => d,
        None => crate::owner_commands::resolve_identity_dir()?,
    };
    Ok((doc, engine, trust_doc, dir))
}

/// Load device keys off the blocking pool under the owner-state write lock
/// (donor: `revoke_device_inner`).
async fn load_keys(
    dir: std::path::PathBuf,
    keychain: KeychainFactory,
) -> Result<crate::owner_state::LoadedOwnerState, String> {
    run_blocking(move || {
        let _guard = crate::owner_commands::OWNER_STATE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        load_owner_state(&dir, keychain())
    })
    .await?
    .ok_or_else(|| "noOwner: no owner identity on this device".to_string())
}

/// Flush best-effort: the mutation is already durable in the resident doc
/// and the dirty latch retries the publish+persist.
async fn flush_warn_only(engine: &crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>, what: &str) {
    if let Err(e) = engine.flush_now().await {
        tracing::warn!(error = %e, "{what}: quorum flush failed; dirty latch will retry");
    }
}

pub(crate) async fn request_quorum_revocation_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    device_vk_hex: String,
    reason: String,
) -> Result<String, String> {
    let (doc, engine, trust_doc, dir) = snapshot_handles(state)?;
    // ZEB-677 S5 — the fleet's current epoch, so the request bundles the
    // pre-built next-epoch carrier doc (crypto cutoff). `None` when the node
    // isn't carrying fleet keys → revoke-only.
    let (carrier_doc_opt, fleet_keys_opt) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (g.fleet_key_epoch_doc.clone(), g.fleet_keys.clone())
    };
    let current_fleet_epoch: Option<u32> = match (&carrier_doc_opt, &fleet_keys_opt) {
        (Some(carrier), Some(keys)) => Some(carrier.lock().await.epoch.max(keys.newest().epoch())),
        _ => None,
    };
    let loaded = load_keys(dir, keychain).await?;
    let trust_snapshot = trust_doc.lock().await.clone();
    let mut request_id = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut request_id);
    let (id_hex, request) = plan_quorum_revocation_request(
        &trust_snapshot,
        &loaded.device_signing_key,
        loaded.master_seed.is_some(),
        &device_vk_hex,
        &reason,
        now_unix_secs(),
        now_unix_ms(),
        request_id,
        current_fleet_epoch,
    )?;
    {
        let mut g = doc.lock().await;
        // Sweep-on-write: settled residue (expired / already-revoked
        // targets) must not trip the duplicate or cap checks below.
        let now_ms = now_unix_ms();
        crate::owner_quorum_sync::prune_settled_requests(&mut g, &trust_snapshot, now_ms);
        // One live request per target: a duplicate would double the banner
        // on every sibling and complete idempotently anyway. A request
        // dead under a VERIFIED decline doesn't block a retry.
        let QuorumRequestKind::Revocation { target_hex, .. } = &request.kind else {
            return Err("internal: request_quorum_revocation must build a revocation".to_string());
        };
        let duplicate = g.requests.iter().any(|(rid, r)| {
            // Only another live revocation for the same target is a duplicate;
            // enrollment requests never collide with a revocation target.
            let QuorumRequestKind::Revocation {
                target_hex: existing,
                ..
            } = &r.kind
            else {
                return false;
            };
            existing == target_hex
                && now_ms <= r.expires_at_ms
                && crate::owner_quorum_sync::verified_decliners(&trust_snapshot, rid, r).is_empty()
        });
        if duplicate {
            return Err(
                "duplicateRequest: a co-sign request for that device is already pending"
                    .to_string(),
            );
        }
        if g.requests.len() >= MAX_QUORUM_REQUESTS {
            return Err(
                "tooManyRequests: too many pending co-sign requests — wait for them to expire"
                    .to_string(),
            );
        }
        g.requests.insert(id_hex.clone(), request);
    }
    engine.notify_dirty();
    flush_warn_only(&engine, "request_quorum_revocation").await;
    emit("owner-quorum-updated");
    Ok(id_hex)
}

/// ZEB-677 S5 — open a STANDALONE quorum fleet-epoch rotation request (the
/// `fleetEpochStale` retry surface on a master-less fleet). Requires the node
/// to be carrying fleet keys; errors on a master-holding device (use the
/// direct `bump_fleet_epoch`).
pub(crate) async fn request_quorum_epoch_bump_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<String, String> {
    let (doc, engine, trust_doc, dir) = snapshot_handles(state)?;
    let (carrier_doc_opt, fleet_keys_opt) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (g.fleet_key_epoch_doc.clone(), g.fleet_keys.clone())
    };
    let current_fleet_epoch = match (&carrier_doc_opt, &fleet_keys_opt) {
        (Some(carrier), Some(keys)) => carrier.lock().await.epoch.max(keys.newest().epoch()),
        _ => {
            return Err(
                "noFleetKeys: this node is not carrying fleet keys; nothing to rotate".to_string(),
            )
        }
    };
    let loaded = load_keys(dir, keychain).await?;
    let trust_snapshot = trust_doc.lock().await.clone();
    let mut request_id = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut request_id);
    let (id_hex, request) = plan_quorum_epoch_bump_request(
        &trust_snapshot,
        &loaded.device_signing_key,
        loaded.master_seed.is_some(),
        current_fleet_epoch,
        now_unix_secs(),
        now_unix_ms(),
        request_id,
    )?;
    {
        let mut g = doc.lock().await;
        let now_ms = now_unix_ms();
        crate::owner_quorum_sync::prune_settled_requests(&mut g, &trust_snapshot, now_ms);
        // One live rotation at a time (a declined one is already pruned above).
        let already_rotating = g.requests.values().any(|r| {
            matches!(r.kind, QuorumRequestKind::EpochBump { .. }) && now_ms <= r.expires_at_ms
        });
        if already_rotating {
            return Err(
                "alreadyRotating: a fleet-key rotation is already pending co-signature".to_string(),
            );
        }
        if g.requests.len() >= MAX_QUORUM_REQUESTS {
            return Err(
                "tooManyRequests: too many pending co-sign requests — wait for them to expire"
                    .to_string(),
            );
        }
        g.requests.insert(id_hex.clone(), request);
    }
    engine.notify_dirty();
    flush_warn_only(&engine, "request_quorum_epoch_bump").await;
    emit("owner-quorum-updated");
    Ok(id_hex)
}

pub(crate) async fn cosign_quorum_request_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    request_id: String,
) -> Result<(), String> {
    let (doc, engine, trust_doc, dir) = snapshot_handles(state)?;
    let loaded = load_keys(dir, keychain).await?;
    let self_id = crate::owner_state::device_id_from_signing_key(&loaded.device_signing_key);
    let trust_snapshot = trust_doc.lock().await.clone();
    let signed = {
        let mut g = doc.lock().await;
        cosign_request_core(
            &mut g,
            &trust_snapshot,
            &loaded.device_signing_key,
            self_id,
            &request_id,
            now_unix_ms(),
        )?
    };
    if signed {
        engine.notify_dirty();
        flush_warn_only(&engine, "cosign_quorum_request").await;
        emit("owner-quorum-updated");
    }
    Ok(())
}

pub(crate) async fn decline_quorum_request_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    request_id: String,
) -> Result<(), String> {
    let (doc, engine, trust_doc, dir) = snapshot_handles(state)?;
    let loaded = load_keys(dir, keychain).await?;
    let self_id = crate::owner_state::device_id_from_signing_key(&loaded.device_signing_key);
    let trust_snapshot = trust_doc.lock().await.clone();
    let declined = {
        let mut g = doc.lock().await;
        decline_request_core(
            &mut g,
            &trust_snapshot,
            &loaded.device_signing_key,
            self_id,
            &request_id,
        )?
    };
    if declined {
        engine.notify_dirty();
        flush_warn_only(&engine, "decline_quorum_request").await;
        emit("owner-quorum-updated");
    }
    Ok(())
}

/// Write (or supersede) THIS device's arm cell, then publish. Flush is
/// best-effort — the replicated doc + dirty latch is the durability
/// boundary (same as every other quorum write path).
async fn write_arm_cell(
    doc: &std::sync::Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    engine: &crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>,
    self_id: [u8; 16],
    armed_until_ms: u64,
    now_ms: u64,
) {
    {
        let mut g = doc.lock().await;
        crate::owner_quorum_sync::stamp_arm_cell(&mut g, self_id, armed_until_ms, now_ms);
    }
    engine.notify_dirty();
    flush_warn_only(engine, "arm_quorum_enrollment").await;
}

/// Arm a 15-minute single-use enrollment co-sign window on THIS device
/// (spec §5.1). Only a master-less device arms — a master-holding device
/// adds devices through normal pairing.
pub(crate) async fn arm_quorum_enrollment_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<u64, String> {
    let (doc, engine, trust_doc, dir) = snapshot_handles(state)?;
    let loaded = load_keys(dir, keychain).await?;
    if loaded.master_seed.is_some() {
        return Err(
            "hasMaster: this device holds the master key — use normal pairing to add a device"
                .to_string(),
        );
    }
    let self_id = crate::owner_state::device_id_from_signing_key(&loaded.device_signing_key);
    // Depth-1: a device that is not an active Master-certed member can never
    // be a quorum signer, so its arm would be dead weight (the sweep and the
    // enrollment planner both skip it). Reject at the IPC boundary — the UI
    // gates this via `canArmEnrollment`, but a direct RPC caller does not.
    {
        let trust = trust_doc.lock().await;
        let eligible = trust
            .enrollments
            .get(&self_id)
            .is_some_and(is_master_issued)
            && !trust.is_revoked(self_id);
        if !eligible {
            return Err(
                "notEligible: this device does not hold an active Master-issued certificate"
                    .to_string(),
            );
        }
    }
    let now_ms = now_unix_ms();
    let armed_until = now_ms.saturating_add(crate::owner_quorum_sync::ARM_WINDOW_MS);
    write_arm_cell(&doc, &engine, self_id, armed_until, now_ms).await;
    emit("owner-quorum-updated");
    Ok(armed_until)
}

/// Disarm THIS device's enrollment window early (spec §5.1 Cancel). Writes
/// an already-expired cell (never deletes — see `stamp_arm_cell`).
pub(crate) async fn disarm_quorum_enrollment_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<(), String> {
    let (doc, engine, _trust_doc, dir) = snapshot_handles(state)?;
    let loaded = load_keys(dir, keychain).await?;
    let self_id = crate::owner_state::device_id_from_signing_key(&loaded.device_signing_key);
    let now_ms = now_unix_ms();
    write_arm_cell(&doc, &engine, self_id, now_ms.saturating_sub(1), now_ms).await;
    emit("owner-quorum-updated");
    Ok(())
}

fn sink_emit(
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
) -> std::sync::Arc<dyn Fn(&str) + Send + Sync> {
    std::sync::Arc::new(move |name: &str| {
        crate::node_event_sink::emit_ser(&*sink, name, &serde_json::Value::Null);
    })
}

/// ZEB-445-style shared IPC/RPC seams.
pub(crate) async fn request_quorum_revocation_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    device_vk_hex: String,
    reason: String,
) -> Result<String, String> {
    request_quorum_revocation_inner(state, prod_keychain, sink_emit(sink), device_vk_hex, reason)
        .await
}

pub(crate) async fn cosign_quorum_request_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    request_id: String,
) -> Result<(), String> {
    cosign_quorum_request_inner(state, prod_keychain, sink_emit(sink), request_id).await
}

pub(crate) async fn decline_quorum_request_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    request_id: String,
) -> Result<(), String> {
    decline_quorum_request_inner(state, prod_keychain, sink_emit(sink), request_id).await
}

pub(crate) async fn arm_quorum_enrollment_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
) -> Result<u64, String> {
    arm_quorum_enrollment_inner(state, prod_keychain, sink_emit(sink)).await
}

pub(crate) async fn disarm_quorum_enrollment_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
) -> Result<(), String> {
    disarm_quorum_enrollment_inner(state, prod_keychain, sink_emit(sink)).await
}

pub(crate) async fn request_quorum_epoch_bump_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
) -> Result<String, String> {
    request_quorum_epoch_bump_inner(state, prod_keychain, sink_emit(sink)).await
}

#[tauri::command]
pub async fn request_quorum_revocation(
    app: tauri::AppHandle,
    device_vk_hex: String,
    reason: String,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<String, String> {
    request_quorum_revocation_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
        device_vk_hex,
        reason,
    )
    .await
}

#[tauri::command]
pub async fn cosign_quorum_request(
    app: tauri::AppHandle,
    request_id: String,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<(), String> {
    cosign_quorum_request_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
        request_id,
    )
    .await
}

#[tauri::command]
pub async fn decline_quorum_request(
    app: tauri::AppHandle,
    request_id: String,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<(), String> {
    decline_quorum_request_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
        request_id,
    )
    .await
}

#[tauri::command]
pub async fn arm_quorum_enrollment(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<u64, String> {
    arm_quorum_enrollment_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
    )
    .await
}

#[tauri::command]
pub async fn disarm_quorum_enrollment(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<(), String> {
    disarm_quorum_enrollment_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
    )
    .await
}

#[tauri::command]
pub async fn request_quorum_epoch_bump(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<String, String> {
    request_quorum_epoch_bump_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
    )
    .await
}
