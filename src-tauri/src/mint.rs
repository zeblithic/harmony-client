//! Mint command layer — the Tauri IPC surface for the Mint feature.
//!
//! The pure ledger logic (schema migration, connection lifecycle, settings,
//! account + transaction CRUD, CSV export) and the owner-scoped sync engine live
//! in the `harmony-mint` crate (ZEB-548 Stage 1, PR #6). This module keeps only
//! the 12 `#[tauri::command]` wrappers, which need `AppHandle` / `NodeState` /
//! `mint_db_handle` — all binary-scoped. Each wrapper resolves the per-profile
//! SQLite connection, runs the pure function on the blocking pool, and nudges the
//! sync engine's debounced republish. The pure API is re-exported so the rest of
//! harmony-app keeps reaching Mint types/functions via `crate::mint::*`
//! (including `mint_db_handle`'s `crate::mint::open_database`).
//!
//! Spec: `docs/specs/2026-05-19-mint-mvp-design.md`
//! Plan: `docs/plans/2026-05-19-mint-mvp-plan.md`

pub use harmony_mint::mint::*;

// ── Tauri command layer ──────────────────────────────────────────────────────
//
// All commands wrap their sync rusqlite work in tokio::task::spawn_blocking
// so the tokio executor never blocks on file I/O. The `std::sync::Mutex` on
// the connection is correct (not tokio::sync::Mutex) because the lock is
// only held inside the spawn_blocking closure, never across an .await.
// See spec § Architecture > Database connection lifecycle.
//
// `.expect` on the connection mutex matches the project's existing
// poisoning policy (see pin_content): poisoning indicates a panic in
// a prior critical section, and the safe response is to surface it
// rather than silently swallow.

/// Extract the mint sync engine handle (if running) and call `notify_dirty()`.
/// Non-blocking — the debounce window coalesces rapid mutation bursts.
fn notify_mint_dirty(state: &tauri::State<'_, std::sync::Mutex<crate::NodeState>>) {
    if let Ok(guard) = state.lock() {
        if let Some(engine) = guard.mint_sync.as_ref() {
            engine.notify_dirty();
        }
    }
}

#[tauri::command]
pub async fn mint_list_accounts(
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<Vec<Account>, String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        list_accounts(&conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

#[tauri::command]
pub async fn mint_create_account(
    name: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<Account, String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        create_account(&conn, &name).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))??;
    notify_mint_dirty(&state);
    Ok(result)
}

#[tauri::command]
pub async fn mint_rename_account(
    id: String,
    name: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<Account, String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        rename_account(&conn, &id, &name).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))??;
    notify_mint_dirty(&state);
    Ok(result)
}

/// Delete an account from the mint ledger.
///
/// **Constraint:** the mint sync engine must already be initialized (i.e.
/// identity bootstrap must have completed) before this command is called.
/// This is a deliberate safety requirement: the deletion floor entry must be
/// persisted to disk before the SQLite delete is committed (crash-safety
/// ordering). If the engine is absent there is no durable floor path, so we
/// refuse rather than risk a zombie-resurrection window.
#[tauri::command]
pub async fn mint_delete_account(
    id: String,
    reassign_to: Option<String>,
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<(), String> {
    // Extract the sync_state handle + persist path from the engine — required;
    // we do NOT fall back to a temporary floor when the engine is absent (see
    // the doc-comment above the command).
    let (sync_state_handle, sync_state_path_opt, sync_cipher_opt) = {
        let node = state.lock().expect("NodeState poisoned");
        match node.mint_sync.as_ref() {
            Some(e) => (
                e.sync_state_handle(),
                e.sync_state_path().map(|p| p.to_path_buf()),
                e.dataset_cipher(),
            ),
            None => {
                return Err(
                    "mint sync engine not yet initialized — cannot delete account safely \
                     (deletion floor would be lost). Retry after identity bootstrap completes."
                        .to_string(),
                );
            }
        }
    };
    let conn = crate::mint_db_handle(&app, &state)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        let mut st = sync_state_handle.blocking_lock();

        // ── Ordering matters for crash-safety ─────────────────────────────────
        // 1. Insert floor entry in memory.
        // 2. Persist floor to disk.
        // 3. Commit SQLite delete.
        //
        // A crash between steps 2 and 3 leaves a "phantom" floor entry for an
        // account that's still present locally. That's minor: the account
        // remains usable and a subsequent delete attempt will succeed normally.
        //
        // The previous order (SQLite commit FIRST, then floor insert) had the
        // opposite risk: a crash left the SQLite delete committed but the floor
        // empty — on next sync, a peer that still held the account would replay
        // it (zombie resurrection). This ordering eliminates that risk entirely.
        let now = chrono::Utc::now().to_rfc3339();
        st.account_deletion_floor.insert(id.clone(), now);

        if let Some(ref path) = sync_state_path_opt {
            let st_snap = st.clone();
            let cipher = sync_cipher_opt
                .as_ref()
                .expect("ZEB-982: a path-ful mint engine always carries a cipher");
            if let Err(e) = crate::mint_sync_persist::save(cipher, path, &st_snap) {
                // Floor persist failed. Abort — do NOT proceed with the SQLite
                // delete, because if we did and then crashed, the floor entry
                // would vanish on restart (the in-memory insert is not durable
                // without the persist). Rolling back the in-memory insert and
                // returning an error is the safe choice: the account is still
                // present, and the user can retry.
                st.account_deletion_floor.remove(&id);
                return Err(format!(
                    "persist sync_state before account deletion failed: {e} — \
                     delete aborted to prevent zombie resurrection risk"
                ));
            }
        }

        // Floor is now durable. Proceed with the SQLite delete.
        delete_account(&conn, &id, reassign_to.as_deref()).map_err(|e| e.to_string())?;

        Ok::<(), String>(())
    })
    .await
    .map_err(|e| format!("join error: {e}"))??;
    notify_mint_dirty(&state);
    Ok(())
}

#[tauri::command]
pub async fn mint_list_transactions(
    date_from: Option<String>,
    date_to: Option<String>,
    account_id: Option<String>,
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<Vec<Transaction>, String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        let filter = ListFilter {
            date_from,
            date_to,
            account_id,
        };
        list_transactions(&conn, &filter).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

#[tauri::command]
pub async fn mint_get_transaction(
    id: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<Option<Transaction>, String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        get_transaction(&conn, &id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

#[tauri::command]
pub async fn mint_create_transaction(
    payload: NewTransaction,
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<Transaction, String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        create_transaction(&conn, payload).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))??;
    notify_mint_dirty(&state);
    Ok(result)
}

#[tauri::command]
pub async fn mint_update_transaction(
    id: String,
    payload: UpdateTransaction,
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<Transaction, String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        update_transaction(&conn, &id, payload).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))??;
    notify_mint_dirty(&state);
    Ok(result)
}

#[tauri::command]
pub async fn mint_delete_transaction(
    id: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<(), String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        delete_transaction(&conn, &id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))??;
    notify_mint_dirty(&state);
    Ok(())
}

#[tauri::command]
pub async fn mint_get_default_currency(
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<Option<String>, String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        get_default_currency(&conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

#[tauri::command]
pub async fn mint_set_default_currency(
    currency: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<(), String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        set_default_currency(&conn, &currency).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))??;
    notify_mint_dirty(&state);
    Ok(())
}

#[tauri::command]
pub async fn mint_export_csv(
    output_path: String,
    date_from: Option<String>,
    date_to: Option<String>,
    account_id: Option<String>,
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<ExportSummary, String> {
    let conn = crate::mint_db_handle(&app, &state)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().expect("mint_db lock poisoned");
        export_csv(
            &conn,
            std::path::Path::new(&output_path),
            date_from.as_deref(),
            date_to.as_deref(),
            account_id.as_deref(),
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}
