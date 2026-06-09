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
    // `upsert` is a no-op for a stale HLC and `get` filters tombstoned notes,
    // so a concurrent delete/newer-edit on another device can make this `get`
    // return `None`. Surface that as a recoverable error rather than panicking
    // (reachable once edits target an existing id, incl. the idempotent
    // migration in notes-migrate.ts).
    d.get(&id).map(to_view).ok_or_else(|| {
        "note upsert was superseded (stale write or the note was deleted on another device)"
            .to_string()
    })
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
    async fn upsert_superseded_by_newer_delete_returns_err_no_panic() {
        // Regression for the `.expect("note present immediately after upsert")`
        // panic: an upsert with a STALE HLC against a note that a concurrent
        // device already deleted with a strictly-newer HLC is a no-op in the
        // CRDT, so `get` returns None. The core must surface that as Err, never
        // panic (which would crash the backend).
        let doc = Arc::new(Mutex::new(NotesDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));

        // 1. Create the note (low HLC via the real mint path).
        let v = notes_upsert_core(&doc, &tracker, "dev-A", None, "original".into())
            .await
            .unwrap();
        let id = v.id.clone();

        // 2. A concurrent device deletes it with a strictly-newer (far-future)
        //    HLC — directly on the doc to simulate an inbound merge from a
        //    sibling, independent of dev-B's local tracker.
        doc.lock().await.delete(
            &id,
            Hlc {
                wall_ms: u64::MAX,
                logical: u32::MAX,
                device_id: "dev-B".into(),
            },
        );
        assert!(
            doc.lock().await.get(&id).is_none(),
            "note is tombstoned after the far-future delete"
        );

        // 3. dev-B re-upserts the SAME id with its own fresh tracker, which
        //    mints a wall_ms ≈ now — strictly OLDER than the u64::MAX delete,
        //    so the CRDT upsert is a no-op and `get` returns None.
        let stale_tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let result = notes_upsert_core(
            &doc,
            &stale_tracker,
            "dev-B",
            Some(id.clone()),
            "edit".into(),
        )
        .await;
        assert!(
            result.is_err(),
            "stale upsert against a tombstone must return Err, not panic"
        );
        // The note stays deleted (the stale edit did not resurrect it).
        assert!(doc.lock().await.get(&id).is_none());
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

    /// End-to-end engine-wiring proof: a real `FleetSyncEngine<NotesDoc>`
    /// configured exactly as `start_node` configures it (NotesPersist sink,
    /// `merge_from` merger, `publish_seen: true`, lookup tag `b"notes-v1"`)
    /// must emit an outbound wire frame on the publisher channel when a local
    /// note write is followed by `notify_dirty` + `flush_now`. This exercises
    /// the engine + merger + persist + channel wiring the Zenoh adapter sits
    /// on top of (the Zenoh hop itself can't be unit-tested without a session).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn notes_engine_publishes_on_local_write() {
        use crate::content_store::{ContentStore, InMemoryStub};
        use crate::fleet_sync::{FleetSyncConfig, FleetSyncEngine, Merger, DEFAULT_DEBOUNCE_MS};
        use crate::notes_persist::NotesPersist;
        use crate::owner_state_crypto::KeyTree;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let kt = Arc::new(KeyTree::derive(&[0x33u8; 32]).expect("derive kt"));
        let doc = Arc::new(Mutex::new(NotesDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let merger: Merger<NotesDoc> = Arc::new(|local, remote| local.merge_from(remote));

        let engine = FleetSyncEngine::<NotesDoc>::new(FleetSyncConfig {
            kt,
            device_id: "dev-A".to_string(),
            state: Arc::clone(&doc),
            merger,
            replay_tracker: Arc::clone(&tracker),
            content_store: cas,
            publisher_tx: out_tx,
            subscriber_rx: in_rx,
            persist: Arc::new(NotesPersist {
                doc_path: dir.path().join("notes.cbor"),
                replay_path: dir.path().join("notes_replay.cbor"),
            }),
            lookup_key_tag: b"notes-v1",
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            publish_seen: true,
            on_applied: None,
            sibling_acks: Arc::new(Mutex::new(BTreeMap::new())),
        });

        // A local note write through the production IPC core, then force the
        // engine to publish.
        notes_upsert_core(&doc, &tracker, "dev-A", None, "hello".into())
            .await
            .unwrap();
        engine.notify_dirty();
        engine.flush_now().await.unwrap();

        // The local write must have driven a (non-empty) publish frame onto
        // the outbound channel.
        let frame = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("publish frame produced within 5s")
            .expect("publisher channel yielded Some(frame)");
        assert!(!frame.is_empty(), "published wire frame must be non-empty");

        let _ = engine.shutdown().await;
    }
}
