//! Owner-private Notes IPC surface (ZEB-417 SP1). Tauri commands
//! `notes_list` / `notes_upsert` / `notes_delete` plus their Tauri-free
//! testable cores. The dataset handles live on `NodeState` and stay `None`
//! until the FleetSyncEngine is wired at startup (next task); until then the
//! commands reject with "notes dataset not loaded".

use crate::notes_crdt::{Note, NotesDoc};
use crate::owner_state_types::Hlc;
use harmony_crdt_sync::ReplayTracker;
use serde::Serialize;
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
    tracker: &Arc<Mutex<ReplayTracker<String, Hlc>>>,
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
        let candidate = crate::fleet_sync::peek_next_hlc(t.accepted(), device_id);
        if !candidate.is_strictly_newer_than(&existing.updated_at) {
            return Err(
                "note upsert was superseded (a newer edit or delete already won)".to_string(),
            );
        }
        t.observe_local(candidate.clone());
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
    tracker: &Arc<Mutex<ReplayTracker<String, Hlc>>>,
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
        let candidate = crate::fleet_sync::peek_next_hlc(t.accepted(), device_id);
        if !candidate.is_strictly_newer_than(&current_updated_at) {
            return Err("note delete was superseded (a newer edit already won)".to_string());
        }
        t.observe_local(candidate.clone());
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
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".into(),
        )));
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
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".into(),
        )));

        // Tracker has no entry for the device before the delete.
        let before = tracker.lock().await.accepted().get("dev-A").cloned();
        assert!(before.is_none(), "no HLC minted yet");

        let changed = notes_delete_core(&doc, &tracker, "dev-A", "does-not-exist".into())
            .await
            .unwrap();
        assert!(!changed, "deleting an unknown id returns Ok(false)");

        // The tracker entry for the device is unchanged — no HLC was minted.
        let after = tracker.lock().await.accepted().get("dev-A").cloned();
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
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".into(),
        )));

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
        let fresh_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".into(),
        )));
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
            fresh_tracker.lock().await.accepted().get("dev-B").is_none(),
            "a superseded delete must not mint an HLC (tracker stays unadvanced)"
        );
    }

    #[tokio::test]
    async fn upsert_rejects_blank() {
        let doc = Arc::new(Mutex::new(NotesDoc::default()));
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".into(),
        )));
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
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".into(),
        )));

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
        let stale_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".to_string(),
        )));
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
            stale_tracker.lock().await.accepted().get("dev-B").is_none(),
            "a superseded upsert must not mint an HLC (tracker stays unadvanced)"
        );
    }

    #[tokio::test]
    async fn upsert_mints_monotone_hlcs() {
        // two upserts produce strictly increasing updated_at via the shared tracker
        let doc = Arc::new(Mutex::new(NotesDoc::default()));
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".to_string(),
        )));
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
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".to_string(),
        )));
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let merger: Merger<NotesDoc> = Arc::new(|local, remote| local.merge_from(remote));

        let engine = FleetSyncEngine::<NotesDoc>::new(FleetSyncConfig {
            keys: crate::owner_state_crypto::FleetKeySet::new(kt),
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
            sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
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

    /// ZEB-361 — two-engine cross-DEVICE convergence proofs for the Notes
    /// dataset.
    ///
    /// Notes ship as the first consumer of the SP1 fleet-sync substrate
    /// (ZEB-417), and post-ZEB-492 the engine is built from the distributed
    /// `KeyTree` on cert-only devices too — yet Notes was the only fleet dataset
    /// with no cross-device sync test. The CRDT merge unit tests in
    /// `notes_crdt.rs` exercise `merge_from` in memory only, and the
    /// single-engine `notes_engine_publishes_on_local_write` proves just the
    /// SEND half (a local write emits a publish frame). These tests close the
    /// loop: two real `FleetSyncEngine<NotesDoc>` instances — configured exactly
    /// as `start_node` configures the production engine (`merge_from` merger,
    /// `NotesPersist`-shaped sink, `publish_seen: true`, lookup tag
    /// `b"notes-v1"`) — sharing one CAS and cross-wired publisher↔subscriber,
    /// driving the REAL `notes_*_core` write path, must converge across the full
    /// pipeline (encode → CAS put → encrypt → wire frame → decrypt → CAS fetch →
    /// decode → merge → tracker advance). This is ZEB-361's literal acceptance
    /// ("a note on device A appears on B, survives offline edits on both sides,
    /// converges deterministically"). The Zenoh hop the frames ride in
    /// production can't be unit-tested without a live session; the forwarder
    /// stands in for it byte-for-byte.
    mod two_engine_sync {
        use super::{notes_delete_core, notes_list_core, notes_upsert_core};
        use crate::content_store::{ContentStore, InMemoryStub};
        use crate::fleet_sync::{
            FleetPersist, FleetSyncConfig, FleetSyncEngine, Merger, SyncError, DEFAULT_DEBOUNCE_MS,
        };
        use crate::notes_crdt::NotesDoc;
        use crate::owner_state_crypto::KeyTree;
        use crate::owner_state_types::Hlc;
        use std::collections::BTreeMap;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::{mpsc, Mutex};

        /// Convergence/wire-round-trip is the assertion; on-disk durability is
        /// covered separately by `notes_persist.rs`. A no-op sink keeps these
        /// tests free of per-engine tempdir plumbing.
        struct NoopNotesPersist;
        impl FleetPersist<NotesDoc> for NoopNotesPersist {
            fn persist(&self, _: &NotesDoc, _: &BTreeMap<String, Hlc>) -> Result<(), SyncError> {
                Ok(())
            }
        }

        /// One device's engine plus the handles the harness shuttles between
        /// devices. `out_rx`/`in_tx` are moved into the forwarder at wiring time.
        use harmony_crdt_sync::ReplayTracker;

        struct NotesEngine {
            engine: FleetSyncEngine<NotesDoc>,
            doc: Arc<Mutex<NotesDoc>>,
            tracker: Arc<Mutex<ReplayTracker<String, Hlc>>>,
            out_rx: mpsc::Receiver<Vec<u8>>,
            in_tx: mpsc::Sender<Vec<u8>>,
        }

        /// Build a notes engine for `device_id` against a shared `kt`/`cas`
        /// (same owner fleet → same lookup/encryption keys, distinct device id).
        fn build(device_id: &str, kt: Arc<KeyTree>, cas: Arc<dyn ContentStore>) -> NotesEngine {
            let (out_tx, out_rx) = mpsc::channel(64);
            let (in_tx, in_rx) = mpsc::channel(64);
            let doc = Arc::new(Mutex::new(NotesDoc::default()));
            let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
                device_id.to_string(),
            )));
            let merger: Merger<NotesDoc> = Arc::new(|local, remote| local.merge_from(remote));
            let engine = FleetSyncEngine::<NotesDoc>::new(FleetSyncConfig {
                keys: crate::owner_state_crypto::FleetKeySet::new(kt),
                device_id: device_id.to_string(),
                state: Arc::clone(&doc),
                merger,
                replay_tracker: Arc::clone(&tracker),
                content_store: cas,
                publisher_tx: out_tx,
                subscriber_rx: in_rx,
                persist: Arc::new(NoopNotesPersist),
                lookup_key_tag: b"notes-v1",
                debounce_ms: DEFAULT_DEBOUNCE_MS,
                publish_seen: true,
                on_applied: None,
                sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
            });
            NotesEngine {
                engine,
                doc,
                tracker,
                out_rx,
                in_tx,
            }
        }

        /// Stand-in for the Zenoh hop: pump A's outbound frames into B's inbox
        /// and vice versa. Exits cleanly once both engines have shut down — a
        /// dropped publisher closes the matching `recv()` arm, and a closed peer
        /// inbox makes `send()` fail, both of which stop the loop. Stopping on a
        /// failed `send` is explicit (not a silent drop) so a mid-test delivery
        /// failure ends the task — and surfaces via `teardown`'s join — rather
        /// than masquerading as a convergence timeout.
        fn spawn_forwarder(
            mut a_out: mpsc::Receiver<Vec<u8>>,
            b_in: mpsc::Sender<Vec<u8>>,
            mut b_out: mpsc::Receiver<Vec<u8>>,
            a_in: mpsc::Sender<Vec<u8>>,
        ) -> tokio::task::JoinHandle<()> {
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        Some(f) = a_out.recv() => {
                            if b_in.send(f).await.is_err() {
                                break;
                            }
                        }
                        Some(f) = b_out.recv() => {
                            if a_in.send(f).await.is_err() {
                                break;
                            }
                        }
                        else => break,
                    }
                }
            })
        }

        /// Deterministic teardown: shut both engines down (which drops their
        /// publishers, so the forwarder's `recv()` arms go quiet and it breaks),
        /// then JOIN the forwarder with a bound. Joining — rather than `abort()`
        /// — surfaces any forwarder-task panic and makes teardown ordering
        /// deterministic (Qodo).
        async fn teardown(
            a_engine: FleetSyncEngine<NotesDoc>,
            b_engine: FleetSyncEngine<NotesDoc>,
            fwd: tokio::task::JoinHandle<()>,
        ) {
            let _ = a_engine.shutdown().await;
            let _ = b_engine.shutdown().await;
            match tokio::time::timeout(Duration::from_secs(2), fwd).await {
                Ok(joined) => joined.expect("forwarder task panicked"),
                Err(_) => panic!("forwarder did not terminate after both engines shut down"),
            }
        }

        /// Bounded condition-polling — a deterministic readiness barrier, not a
        /// fixed-sleep "assume ready" (the ZEB-459/ZEB-469 pattern).
        async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
        where
            F: FnMut() -> Fut,
            Fut: std::future::Future<Output = bool>,
        {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                if cond().await {
                    return true;
                }
                if tokio::time::Instant::now() > deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn note_written_on_one_device_appears_on_the_sibling() {
            let kt = Arc::new(KeyTree::derive(&[0x33u8; 32]).expect("kt"));
            let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
            let a = build("dev-A", Arc::clone(&kt), Arc::clone(&cas));
            let b = build("dev-B", Arc::clone(&kt), Arc::clone(&cas));
            let fwd = spawn_forwarder(a.out_rx, b.in_tx, b.out_rx, a.in_tx);

            let note = notes_upsert_core(&a.doc, &a.tracker, "dev-A", None, "buy milk".into())
                .await
                .expect("local upsert on A");
            a.engine.flush_now().await.expect("flush A");

            let b_doc = Arc::clone(&b.doc);
            let id = note.id.clone();
            let converged = wait_until(
                || {
                    let b_doc = Arc::clone(&b_doc);
                    let id = id.clone();
                    async move {
                        notes_list_core(&b_doc)
                            .await
                            .iter()
                            .any(|n| n.id == id && n.text == "buy milk")
                    }
                },
                Duration::from_secs(5),
            )
            .await;
            assert!(converged, "B never received A's note within 5s");

            teardown(a.engine, b.engine, fwd).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn independent_notes_on_both_devices_converge_to_the_union() {
            let kt = Arc::new(KeyTree::derive(&[0x44u8; 32]).expect("kt"));
            let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
            let a = build("dev-A", Arc::clone(&kt), Arc::clone(&cas));
            let b = build("dev-B", Arc::clone(&kt), Arc::clone(&cas));
            let fwd = spawn_forwarder(a.out_rx, b.in_tx, b.out_rx, a.in_tx);

            // Each device adds its own note while the other is "offline" (the
            // frames cross only once both flush) — accumulate, no key collision.
            let na = notes_upsert_core(&a.doc, &a.tracker, "dev-A", None, "from-A".into())
                .await
                .unwrap();
            let nb = notes_upsert_core(&b.doc, &b.tracker, "dev-B", None, "from-B".into())
                .await
                .unwrap();
            a.engine.flush_now().await.unwrap();
            b.engine.flush_now().await.unwrap();

            let a_doc = Arc::clone(&a.doc);
            let b_doc = Arc::clone(&b.doc);
            let (ia, ib) = (na.id.clone(), nb.id.clone());
            let converged = wait_until(
                || {
                    let (a_doc, b_doc) = (Arc::clone(&a_doc), Arc::clone(&b_doc));
                    let (ia, ib) = (ia.clone(), ib.clone());
                    async move {
                        let al = notes_list_core(&a_doc).await;
                        let bl = notes_list_core(&b_doc).await;
                        al.iter().any(|n| n.id == ia)
                            && al.iter().any(|n| n.id == ib)
                            && bl.iter().any(|n| n.id == ia)
                            && bl.iter().any(|n| n.id == ib)
                    }
                },
                Duration::from_secs(5),
            )
            .await;
            assert!(
                converged,
                "both devices did not converge to the union of their notes within 5s"
            );

            teardown(a.engine, b.engine, fwd).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn concurrent_edits_to_the_same_note_converge_over_the_wire() {
            let kt = Arc::new(KeyTree::derive(&[0x55u8; 32]).expect("kt"));
            let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
            let a = build("dev-A", Arc::clone(&kt), Arc::clone(&cas));
            let b = build("dev-B", Arc::clone(&kt), Arc::clone(&cas));
            let fwd = spawn_forwarder(a.out_rx, b.in_tx, b.out_rx, a.in_tx);

            // Seed a shared note on A and let it replicate to B.
            let seed = notes_upsert_core(&a.doc, &a.tracker, "dev-A", None, "v1".into())
                .await
                .unwrap();
            a.engine.flush_now().await.unwrap();
            let id = seed.id.clone();
            {
                let b_doc = Arc::clone(&b.doc);
                let idc = id.clone();
                assert!(
                    wait_until(
                        || {
                            let b_doc = Arc::clone(&b_doc);
                            let idc = idc.clone();
                            async move { notes_list_core(&b_doc).await.iter().any(|n| n.id == idc) }
                        },
                        Duration::from_secs(5),
                    )
                    .await,
                    "seed note did not replicate to B"
                );
            }

            // Both devices edit the SAME id while neither has yet seen the
            // other's edit — issued concurrently via `join!` (independent docs +
            // trackers, no shared state) so the harness orders neither write
            // before the other. The real HLC-minting write path mints each
            // device's HLC independently and the higher one wins; both sides
            // must converge to that winner regardless of which it is.
            let (ra, rb) = tokio::join!(
                notes_upsert_core(
                    &a.doc,
                    &a.tracker,
                    "dev-A",
                    Some(id.clone()),
                    "edited-by-A".into(),
                ),
                notes_upsert_core(
                    &b.doc,
                    &b.tracker,
                    "dev-B",
                    Some(id.clone()),
                    "edited-by-B".into(),
                ),
            );
            ra.unwrap();
            rb.unwrap();
            a.engine.flush_now().await.unwrap();
            b.engine.flush_now().await.unwrap();

            let a_doc = Arc::clone(&a.doc);
            let b_doc = Arc::clone(&b.doc);
            let idc = id.clone();
            let converged = wait_until(
                || {
                    let (a_doc, b_doc, idc) = (Arc::clone(&a_doc), Arc::clone(&b_doc), idc.clone());
                    async move {
                        let at = notes_list_core(&a_doc)
                            .await
                            .into_iter()
                            .find(|n| n.id == idc)
                            .map(|n| n.text);
                        let bt = notes_list_core(&b_doc)
                            .await
                            .into_iter()
                            .find(|n| n.id == idc)
                            .map(|n| n.text);
                        match (at, bt) {
                            (Some(at), Some(bt)) => {
                                at == bt && (at == "edited-by-A" || at == "edited-by-B")
                            }
                            _ => false,
                        }
                    }
                },
                Duration::from_secs(5),
            )
            .await;
            assert!(
                converged,
                "concurrent edits did not converge to a single LWW winner on both devices"
            );

            teardown(a.engine, b.engine, fwd).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn delete_on_one_device_tombstones_on_the_sibling() {
            let kt = Arc::new(KeyTree::derive(&[0x66u8; 32]).expect("kt"));
            let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
            let a = build("dev-A", Arc::clone(&kt), Arc::clone(&cas));
            let b = build("dev-B", Arc::clone(&kt), Arc::clone(&cas));
            let fwd = spawn_forwarder(a.out_rx, b.in_tx, b.out_rx, a.in_tx);

            let note = notes_upsert_core(&a.doc, &a.tracker, "dev-A", None, "ephemeral".into())
                .await
                .unwrap();
            a.engine.flush_now().await.unwrap();
            let id = note.id.clone();

            // Replicate the create to B before deleting.
            {
                let b_doc = Arc::clone(&b.doc);
                let idc = id.clone();
                assert!(
                    wait_until(
                        || {
                            let b_doc = Arc::clone(&b_doc);
                            let idc = idc.clone();
                            async move { notes_list_core(&b_doc).await.iter().any(|n| n.id == idc) }
                        },
                        Duration::from_secs(5),
                    )
                    .await,
                    "note did not replicate to B before delete"
                );
            }

            // Delete on A; B must converge to NOT listing it (tombstone wins).
            let deleted = notes_delete_core(&a.doc, &a.tracker, "dev-A", id.clone())
                .await
                .unwrap();
            assert!(deleted, "deleting a live note returns Ok(true)");
            a.engine.flush_now().await.unwrap();

            let b_doc = Arc::clone(&b.doc);
            let idc = id.clone();
            let converged = wait_until(
                || {
                    let b_doc = Arc::clone(&b_doc);
                    let idc = idc.clone();
                    async move { !notes_list_core(&b_doc).await.iter().any(|n| n.id == idc) }
                },
                Duration::from_secs(5),
            )
            .await;
            assert!(
                converged,
                "B still lists the note A deleted (tombstone did not propagate)"
            );

            teardown(a.engine, b.engine, fwd).await;
        }
    }
}
