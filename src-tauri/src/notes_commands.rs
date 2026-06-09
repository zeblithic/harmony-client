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
    let id = id.unwrap_or_else(new_ulid);
    let mut d = doc.lock().await;
    // Mint the HLC (which advances the shared tracker) only when the write will
    // actually apply. For an EXISTING note, compute the candidate HLC and
    // commit it atomically under a SINGLE tracker-lock hold: peek, bail if it
    // wouldn't beat the note's current `updated_at` (a concurrent newer
    // edit/delete already won), else commit it. Doing the peek and the commit
    // under one lock — rather than peek, release, then `mint_next_hlc` which
    // re-reads the clock — closes a race where the wall clock stepping backward
    // between the two reads could let the peek pass while the minted value is
    // stale, advancing the tracker on a no-op (the exact skew this guards
    // against). A brand-new note (no entry yet) always applies, so it mints
    // normally. Lock order is doc→tracker (matches `notes_delete_core`);
    // deadlock-free since no path locks the tracker while holding the doc in
    // the opposite order.
    let at = if let Some(existing) = d.notes.get(&id) {
        let mut t = tracker.lock().await;
        let candidate = crate::fleet_sync::peek_next_hlc(&t, device_id);
        if !candidate.is_strictly_newer_than(&existing.updated_at) {
            return Err(
                "note upsert was superseded (a newer edit or delete already won)".to_string(),
            );
        }
        t.insert(device_id.to_string(), candidate.clone());
        candidate
    } else {
        crate::fleet_sync::mint_next_hlc(tracker, device_id).await
    };
    d.upsert(id.clone(), trimmed, at);
    // Defensive: even after the peek, a concurrent tombstone with a newer HLC
    // could land between the peek and this upsert, making it a no-op (`get`
    // filters tombstoned notes → `None`). Surface that as a recoverable error
    // rather than panicking.
    d.get(&id).map(to_view).ok_or_else(|| {
        "note upsert was superseded (stale write or the note was deleted on another device)"
            .to_string()
    })
}

/// Tombstone a note (LWW on a freshly minted HLC). Returns `Ok(true)` when a
/// live note was tombstoned; `Ok(false)` when the id is absent or already
/// tombstoned (a no-op — no HLC is minted, so the wrapper skips `notify_dirty`
/// and we avoid publishing unchanged state). Returns `Err` when our delete
/// would lose LWW to a concurrent newer edit (mirrors `notes_upsert_core`).
pub(crate) async fn notes_delete_core(
    doc: &Arc<Mutex<NotesDoc>>,
    tracker: &Arc<Mutex<BTreeMap<String, Hlc>>>,
    device_id: &str,
    id: String,
) -> Result<bool, String> {
    // Hold the doc lock across the whole check-peek-delete so the liveness
    // check and the delete observe the same state. Releasing it between (an
    // early `is_none()` check and a re-lock for the delete) is a TOCTOU: a
    // concurrent tombstone — from another IPC or a remote merge — in the gap
    // would make this a CRDT no-op that still returns `Ok(true)` and triggers
    // a spurious `notify_dirty()` publish of unchanged state. Absent or
    // already-tombstoned ids are a no-op (no HLC minted) so the wrapper skips
    // `notify_dirty`. Holding `d` across the tracker-lock is deadlock-free: no
    // path locks the tracker while holding the doc in the opposite order.
    let mut d = doc.lock().await;
    let Some(current_updated_at) = d.get(&id).map(|n| n.updated_at.clone()) else {
        return Ok(false);
    };
    // Peek-and-commit the delete HLC atomically under a SINGLE tracker-lock
    // hold, mirroring `notes_upsert_core`. `mint_next_hlc` only advances THIS
    // device's tracker entry, so a note last written by another device with a
    // newer/future `updated_at` can outrank a freshly minted HLC — and
    // `NotesDoc::delete` is LWW, so the tombstone would silently lose while we
    // still minted (advancing the tracker → durability-indicator skew) and
    // returned `Ok(true)` (a spurious publish of unchanged state). Bail with a
    // recoverable error when the candidate wouldn't beat the live note's
    // `updated_at`; otherwise commit it under the same lock and apply.
    let at = {
        let mut t = tracker.lock().await;
        let candidate = crate::fleet_sync::peek_next_hlc(&t, device_id);
        if !candidate.is_strictly_newer_than(&current_updated_at) {
            return Err("note delete was superseded (a newer edit already won)".to_string());
        }
        t.insert(device_id.to_string(), candidate.clone());
        candidate
    };
    d.delete(&id, at);
    // The committed candidate is strictly newer than the live note's
    // `updated_at` (checked under the same doc lock, no mutation since), so the
    // tombstone wins and `get` now returns `None`. Report from observed state
    // rather than asserting `Ok(true)`.
    Ok(d.get(&id).is_none())
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
    let changed = notes_delete_core(&doc, &tracker, &device_id, id).await?;
    if changed {
        sync.notify_dirty();
    }
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
        let deleted = notes_delete_core(&doc, &tracker, "dev-A", v.id.clone())
            .await
            .unwrap();
        assert!(deleted, "deleting a live note returns Ok(true)");
        assert!(notes_list_core(&doc).await.is_empty());
    }

    #[tokio::test]
    async fn delete_absent_id_is_noop_no_hlc_minted() {
        // Deleting an id that was never created must be a no-op: return
        // Ok(false) AND mint no HLC, so the Tauri wrapper skips notify_dirty
        // and we never publish unchanged state (Greptile P2).
        let doc = Arc::new(Mutex::new(NotesDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));

        // Tracker has no entry for the device before the delete.
        let before = tracker.lock().await.get("dev-A").cloned();
        assert!(before.is_none(), "no HLC minted yet");

        let changed = notes_delete_core(&doc, &tracker, "dev-A", "does-not-exist".into())
            .await
            .unwrap();
        assert!(!changed, "deleting an unknown id returns Ok(false)");

        // The tracker entry for the device is unchanged — no HLC was minted.
        let after = tracker.lock().await.get("dev-A").cloned();
        assert_eq!(after, before, "no HLC minted for a no-op delete");
    }

    #[tokio::test]
    async fn delete_superseded_by_remote_future_edit_errs_and_mints_nothing() {
        // Delete-side mirror of `upsert_superseded_by_newer_delete...`: a note
        // last edited by another device with a strictly-newer (far-future) HLC
        // must NOT be deletable by our locally minted (≈now) HLC. `NotesDoc::
        // delete` is LWW, so our tombstone would silently lose — the bug was
        // that the core still minted (advancing the shared tracker → durability
        // skew) and returned `Ok(true)` (a spurious notify_dirty publish of
        // unchanged state). The fix peeks before minting and bails.
        let doc = Arc::new(Mutex::new(NotesDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));

        // 1. Create the note locally (low HLC via the real mint path).
        let v = notes_upsert_core(&doc, &tracker, "dev-A", None, "keep".into())
            .await
            .unwrap();
        let id = v.id.clone();

        // 2. A remote device edits it with a far-future HLC — directly on the
        //    doc to simulate an inbound merge from a sibling with a clock that
        //    is ahead of ours. The note stays LIVE.
        doc.lock().await.upsert(
            id.clone(),
            "remote edit".into(),
            Hlc {
                wall_ms: u64::MAX,
                logical: u32::MAX,
                device_id: "dev-B".into(),
            },
        );
        assert!(
            doc.lock().await.get(&id).is_some(),
            "note is still live after the remote future edit"
        );

        // 3. dev-B deletes via a FRESH tracker, minting wall_ms ≈ now — strictly
        //    OLDER than the u64::MAX edit — so the LWW delete would lose.
        let fresh_tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let result = notes_delete_core(&doc, &fresh_tracker, "dev-B", id.clone()).await;
        assert!(
            result.is_err(),
            "a delete that would lose LWW must return Err, never Ok(true)"
        );

        // The note stays LIVE — the losing delete did not tombstone it.
        assert!(
            doc.lock().await.get(&id).is_some(),
            "note remains live; the superseded delete was a no-op"
        );
        // And no HLC was minted on the no-op delete (tracker stays unadvanced),
        // so the durability indicator is not skewed and nothing is published.
        assert!(
            fresh_tracker.lock().await.get("dev-B").is_none(),
            "a superseded delete must not mint an HLC (tracker stays unadvanced)"
        );
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
        // And — the fix for the "HLC minted before failed upsert" skew — the
        // superseded write must NOT have advanced the tracker: minting on a
        // no-op would push tracker[dev-B] above any published HLC with no
        // corresponding publish, undercounting the durability indicator.
        assert!(
            stale_tracker.lock().await.get("dev-B").is_none(),
            "a superseded upsert must not mint an HLC (tracker stays unadvanced)"
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
