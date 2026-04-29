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
    // Load owner_state + master_seed from the persisted ZEB-170 artifacts.
    let identity_dir = crate::owner_commands::resolve_identity_dir()?;
    let loaded = load_owner_state(&identity_dir, KeychainStore::new().ok())?
        .ok_or_else(|| "no owner identity on this device".to_string())?;
    let master_seed = loaded
        .master_seed
        .ok_or_else(|| "master seed not on this device — cannot enroll".to_string())?;

    let (cmd_tx, _state_rx) = require_pairing_handle(&state)?;
    cmd_tx
        .send(PairingCommand::StartInviter {
            display_name,
            owner_state: loaded.state,
            master_seed,
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
    let signing_key = SigningKey::generate(&mut OsRng);
    let (cmd_tx, _state_rx) = require_pairing_handle(&state)?;
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
    let id = Uuid::parse_str(&peer_session_id).map_err(|e| format!("invalid uuid: {e}"))?;
    let (cmd_tx, _) = require_pairing_handle(&state)?;
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
    let (cmd_tx, _) = require_pairing_handle(&state)?;
    cmd_tx
        .send(PairingCommand::ConfirmSas)
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn cancel_pairing(state: State<'_, Mutex<NodeState>>) -> Result<(), String> {
    let (cmd_tx, _) = require_pairing_handle(&state)?;
    cmd_tx
        .send(PairingCommand::Cancel)
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn get_pairing_state(state: State<'_, Mutex<NodeState>>) -> Result<PairingState, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let h = guard
        .pairing_handle
        .as_ref()
        .ok_or_else(|| "pairing not initialized — start node first".to_string())?;
    let current = h.state_rx.borrow().clone();
    Ok(current)
}

fn require_pairing_handle(
    state: &State<'_, Mutex<NodeState>>,
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
