use crate::identity::KeychainStore;
use crate::owner_state::load_owner_state;
use crate::pairing::state_machine::PairingCommand;
use crate::pairing::types::PairingState;
use crate::NodeState;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::sync::Mutex;
use tauri::State;
use uuid::Uuid;

#[tauri::command]
pub async fn start_inviter_pairing(
    display_name: String,
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    start_inviter_pairing_inner(&state, display_name).await
}

/// ZEB-446: seam shared by the Tauri wrapper and the RPC registry, so the
/// headless coordination instance's owner-side GUI (or its API) can drive
/// enrollment. Loads owner_state + master_seed from the persisted ZEB-170
/// artifacts; on a named profile the keychain constructor refuses and the
/// encrypted-file vault serves the load (ZEB-446 vault routing).
pub(crate) async fn start_inviter_pairing_inner(
    state: &Mutex<NodeState>,
    display_name: String,
) -> Result<(), String> {
    // Production composition root: ambient keychain. The ZEB-428/ZEB-446
    // constructor gate refuses in test builds and on named profiles (the
    // encrypted-file vault serves the load there); tests inject through
    // the _with_keychain seam below and never construct the real store.
    start_inviter_pairing_with_keychain(state, display_name, KeychainStore::new().ok()).await
}

pub(crate) async fn start_inviter_pairing_with_keychain(
    state: &Mutex<NodeState>,
    display_name: String,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    // Precondition first (PR #245 round 3, CodeRabbit): a pre-node call
    // must fail with the pairing error — same string as every other
    // pairing command — and must not touch persisted owner secrets on a
    // request that cannot succeed anyway.
    let (cmd_tx, _state_rx) = require_pairing_handle(state)?;

    let identity_dir = crate::owner_commands::resolve_identity_dir()?;
    let loaded = load_owner_state(&identity_dir, keychain)?
        .ok_or_else(|| "no owner identity on this device".to_string())?;
    let master_seed = loaded
        .master_seed
        .ok_or_else(|| "master seed not on this device — cannot enroll".to_string())?;
    // ZEB-668 S5: a joiner enrolled after a fleet epoch bump needs BOTH the
    // epoch-0 material (fleet-keys carrier access) and the current epoch's
    // (live datasets). The resident key set knows the current epoch.
    let fleet_current_epoch = {
        let guard = state.lock().unwrap_or_else(|p| p.into_inner());
        guard.fleet_keys.as_ref().map_or(0, |k| k.newest().epoch)
    };
    cmd_tx
        .send(PairingCommand::StartInviter {
            display_name,
            owner_state: loaded.state,
            master_seed,
            fleet_current_epoch,
        })
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn start_joiner_pairing(
    display_name: String,
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    start_joiner_pairing_inner(&state, display_name).await
}

pub(crate) async fn start_joiner_pairing_inner(
    state: &Mutex<NodeState>,
    display_name: String,
) -> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let (cmd_tx, _state_rx) = require_pairing_handle(state)?;
    cmd_tx
        .send(PairingCommand::StartJoiner {
            display_name,
            signing_key,
        })
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn select_pairing_peer(
    peer_session_id: String,
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    select_pairing_peer_inner(&state, peer_session_id).await
}

pub(crate) async fn select_pairing_peer_inner(
    state: &Mutex<NodeState>,
    peer_session_id: String,
) -> Result<(), String> {
    let id = Uuid::parse_str(&peer_session_id).map_err(|e| format!("invalid uuid: {e}"))?;
    let (cmd_tx, _) = require_pairing_handle(state)?;
    cmd_tx
        .send(PairingCommand::SelectPeer {
            peer_session_id: id,
        })
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn confirm_pairing_sas(state: State<'_, Mutex<NodeState>>) -> Result<(), String> {
    confirm_pairing_sas_inner(&state).await
}

pub(crate) async fn confirm_pairing_sas_inner(state: &Mutex<NodeState>) -> Result<(), String> {
    let (cmd_tx, _) = require_pairing_handle(state)?;
    cmd_tx
        .send(PairingCommand::ConfirmSas)
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn cancel_pairing(state: State<'_, Mutex<NodeState>>) -> Result<(), String> {
    cancel_pairing_inner(&state).await
}

pub(crate) async fn cancel_pairing_inner(state: &Mutex<NodeState>) -> Result<(), String> {
    let (cmd_tx, _) = require_pairing_handle(state)?;
    cmd_tx
        .send(PairingCommand::Cancel)
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn get_pairing_state(state: State<'_, Mutex<NodeState>>) -> Result<PairingState, String> {
    get_pairing_state_inner(&state).await
}

/// async for uniformity with the other seams (the rpc! macro awaits every
/// call); the body is synchronous except for the Complete-state trust
/// resync below.
pub(crate) async fn get_pairing_state_inner(
    state: &Mutex<NodeState>,
) -> Result<PairingState, String> {
    let (_cmd_tx, state_rx) = require_pairing_handle(state)?;
    let current = state_rx.borrow().clone();
    // ZEB-668 S1: a completed pairing wrote the new enrollment to
    // owner_state.cbor via save_owner_state_atomic — the resident trust doc
    // doesn't see file writes. Fold disk into the resident doc here so the
    // fresh enrollment replicates to siblings and the Devices panel (which
    // renders from the resident doc while the node runs) shows the new
    // device. Idempotent (monotonic merge), so repeated Complete polls are
    // harmless; the engine is only nudged when something new was learned.
    if matches!(current, PairingState::Complete { .. }) {
        let resident = {
            let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
            match (
                guard.owner_trust_doc.clone(),
                guard.owner_trust_sync.clone(),
                guard.identity_dir.clone(),
            ) {
                (Some(doc), Some(engine), Some(dir)) => Some((doc, engine, dir)),
                _ => None,
            }
        };
        if let Some((doc, engine, dir)) = resident {
            if let Err(e) =
                crate::owner_trust_sync::resync_trust_from_disk(&doc, &engine, &dir).await
            {
                // Fail open: pairing itself succeeded and disk is correct;
                // the next poll (or next boot) retries the fold.
                tracing::warn!(error = %e, "pairing complete: trust resync from disk failed");
            }
        }
    }
    Ok(current)
}

fn require_pairing_handle(
    state: &Mutex<NodeState>,
) -> Result<
    (
        tokio::sync::mpsc::Sender<PairingCommand>,
        tokio::sync::watch::Receiver<PairingState>,
    ),
    String,
> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let h = guard
        .pairing_handle
        .as_ref()
        .ok_or_else(|| "pairing not initialized — start node first".to_string())?;
    Ok((h.cmd_tx.clone(), h.state_rx.clone()))
}
