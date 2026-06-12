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
    let identity_dir = crate::owner_commands::resolve_identity_dir()?;
    let loaded = load_owner_state(&identity_dir, keychain)?
        .ok_or_else(|| "no owner identity on this device".to_string())?;
    let master_seed = loaded
        .master_seed
        .ok_or_else(|| "master seed not on this device — cannot enroll".to_string())?;

    let (cmd_tx, _state_rx) = require_pairing_handle(state)?;
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
/// call); the body is synchronous.
pub(crate) async fn get_pairing_state_inner(
    state: &Mutex<NodeState>,
) -> Result<PairingState, String> {
    let (_cmd_tx, state_rx) = require_pairing_handle(state)?;
    let current = state_rx.borrow().clone();
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
