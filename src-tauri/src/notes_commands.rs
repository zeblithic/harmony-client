//! Owner-private Notes IPC surface (ZEB-417 SP1). Tauri commands
//! `notes_list` / `notes_upsert` / `notes_delete` plus their Tauri-free
//! testable cores. The dataset handles live on `NodeState` and stay `None`
//! until the FleetSyncEngine is wired at startup (next task); until then the
//! commands reject with "notes dataset not loaded".

use crate::notes_crdt::{Note, NotesDoc};
use crate::owner_state_types::Hlc;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Flattened, frontend-facing view of a live note. `timestamp` is the
/// wall-clock millisecond of the last upsert (the HLC's `wall_ms`).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct NoteView {
    pub id: String,
    pub text: String,
    pub timestamp: u64,
}

fn to_view(n: &Note) -> NoteView {
    NoteView {
        id: n.id.clone(),
        text: n.text.clone(),
        timestamp: n.updated_at.wall_ms,
    }
}

fn new_ulid() -> String {
    ulid::Ulid::new().to_string()
}

// ---- Testable cores (no Tauri State) ----

/// Live notes, oldest-first by id. ULID ids sort lexicographically in
/// creation order, so the id sort is a stable oldest-first ordering.
pub(crate) async fn notes_list_core(doc: &Arc<Mutex<NotesDoc>>) -> Vec<NoteView> {
    let d = doc.lock().await;
    let mut v: Vec<NoteView> = d.list().into_iter().map(to_view).collect();
    v.sort_by(|a, b| a.id.cmp(&b.id)); // ULID id == creation order; stable oldest-first
    v
}

/// Insert or update a note. Empty/whitespace text is rejected. A fresh
/// monotone HLC is minted from the shared tracker so the upsert orders
/// correctly against state-root publishes. `id == None` creates a new
/// ULID-keyed note; `Some(id)` edits an existing one (LWW on the HLC).
pub(crate) async fn notes_upsert_core(
    doc: &Arc<Mutex<NotesDoc>>,
    tracker: &Arc<Mutex<BTreeMap<String, Hlc>>>,
    device_id: &str,
    id: Option<String>,
    text: String,
) -> Result<NoteView, String> {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return Err("note text is empty".into());
    }
    let at = crate::fleet_sync::mint_next_hlc(tracker, device_id).await;
    let id = id.unwrap_or_else(new_ulid);
    let mut d = doc.lock().await;
    d.upsert(id.clone(), trimmed, at);
    Ok(to_view(
        d.get(&id).expect("note present immediately after upsert"),
    ))
}

/// Tombstone a note (LWW on a freshly minted HLC). Deleting a missing or
/// already-deleted id is a no-op.
pub(crate) async fn notes_delete_core(
    doc: &Arc<Mutex<NotesDoc>>,
    tracker: &Arc<Mutex<BTreeMap<String, Hlc>>>,
    device_id: &str,
    id: String,
) -> Result<(), String> {
    let at = crate::fleet_sync::mint_next_hlc(tracker, device_id).await;
    doc.lock().await.delete(&id, at);
    Ok(())
}

const NOTES_NOT_LOADED_MSG: &str = "notes dataset not loaded";

// ---- Tauri wrappers (snapshot handles under the sync mutex, drop, await) ----

/// List all live notes, oldest-first.
#[tauri::command]
pub async fn notes_list(
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<Vec<NoteView>, String> {
    // Snapshot the handle under the sync mutex; release it before any await.
    let doc = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.notes_doc.clone().ok_or(NOTES_NOT_LOADED_MSG)?
    };
    Ok(notes_list_core(&doc).await)
}

/// Insert or update a note, then schedule a sync publish.
#[tauri::command]
pub async fn notes_upsert(
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
    id: Option<String>,
    text: String,
) -> Result<NoteView, String> {
    let (doc, tracker, device_id, sync) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.notes_doc.clone().ok_or(NOTES_NOT_LOADED_MSG)?,
            g.notes_tracker.clone().ok_or(NOTES_NOT_LOADED_MSG)?,
            g.notes_device_id.clone().ok_or(NOTES_NOT_LOADED_MSG)?,
            g.notes_sync.clone().ok_or(NOTES_NOT_LOADED_MSG)?,
        )
    };
    let view = notes_upsert_core(&doc, &tracker, &device_id, id, text).await?;
    sync.notify_dirty();
    Ok(view)
}

/// Tombstone a note, then schedule a sync publish.
#[tauri::command]
pub async fn notes_delete(
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
    id: String,
) -> Result<(), String> {
    let (doc, tracker, device_id, sync) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.notes_doc.clone().ok_or(NOTES_NOT_LOADED_MSG)?,
            g.notes_tracker.clone().ok_or(NOTES_NOT_LOADED_MSG)?,
            g.notes_device_id.clone().ok_or(NOTES_NOT_LOADED_MSG)?,
            g.notes_sync.clone().ok_or(NOTES_NOT_LOADED_MSG)?,
        )
    };
    notes_delete_core(&doc, &tracker, &device_id, id).await?;
    sync.notify_dirty();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upsert_lists_and_deletes() {
        let doc = Arc::new(Mutex::new(NotesDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let v = notes_upsert_core(&doc, &tracker, "dev-A", None, "  hi  ".into())
            .await
            .unwrap();
        assert_eq!(v.text, "hi");
        let listed = notes_list_core(&doc).await;
        assert_eq!(listed.len(), 1);
        notes_delete_core(&doc, &tracker, "dev-A", v.id.clone())
            .await
            .unwrap();
        assert!(notes_list_core(&doc).await.is_empty());
    }

    #[tokio::test]
    async fn upsert_rejects_blank() {
        let doc = Arc::new(Mutex::new(NotesDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        assert!(
            notes_upsert_core(&doc, &tracker, "dev-A", None, "   ".into())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn upsert_mints_monotone_hlcs() {
        // two upserts produce strictly increasing updated_at via the shared tracker
        let doc = Arc::new(Mutex::new(NotesDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let a = notes_upsert_core(&doc, &tracker, "dev-A", None, "one".into())
            .await
            .unwrap();
        let b = notes_upsert_core(&doc, &tracker, "dev-A", None, "two".into())
            .await
            .unwrap();
        assert!(b.timestamp >= a.timestamp); // wall_ms monotone (logical bumps within same ms)
    }
}
