//! ZEB-163 recursive folder-tree walker.
//!
//! Drives an OS-side directory tree through the same ingest pipeline that
//! `ingest_content` and `create_folder` use, but without per-file user
//! interaction: every leaf file becomes a chunked-or-flat bundle, every
//! interior directory becomes a folder manifest, and the dropped root
//! becomes a single new sidecar entry under the caller's chosen parent.
//!
//! Filter policy (`should_filter_name`): deny dotfiles, `.DS_Store`,
//! `Thumbs.db`, `desktop.ini`, `.git/**`. Symlinks are never followed —
//! `symlink_metadata` checks the file type before any recursion or `read`,
//! so a symlinked directory cannot loop the walker.
//!
//! Every leaf file is streamed through the FastCDC chunker + nested-bundle
//! tree builder (ZEB-161), regardless of size; the per-leaf size cap that
//! preceded streaming is gone.
//!
//! Cancellation is best-effort: the walker checks the shared `AtomicBool` at
//! the top of every recursive call and the top of every per-child loop
//! iteration. On cancel the walker returns immediately, and (ZEB-1014) each
//! recursion level rolls back its accumulated children on the way out via
//! `ContentVerbRequest::RollbackIngest` — composing to a best-effort evict
//! of everything the cancelled walk had admitted, instead of leaving it for
//! W-TinyLFU pressure. The result's `cancelled` flag is `true`, and
//! `root_sidecar_id` is `None` whenever cancel fires before the root's
//! manifest gets built. The same rollback runs for a leaf whose mid-stream
//! ingest fails (its partial admitted chunk set) and for a directory whose
//! manifest build fails (its whole already-ingested subtree).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};

use crate::content_index::{ContentIndex, ContentKind};
use crate::event_loop::IngestRequest;
use crate::folders::ManifestEntry;

/// Cap on individually-listed failed entries in the summary. Walks that
/// exceed this bound roll up the rest under `overflow_count`; the modal
/// surfaces "...and N more" without scrolling tens of thousands of paths.
pub const FAILED_LIST_CAP: usize = 50;

/// Per-job ingest counters mutated as the walker descends. Folded into the
/// IPC result before the registry entry is removed.
#[derive(Debug, Default)]
struct WalkCounters {
    succeeded: u64,
    skipped: SkipCounts,
    failed: Vec<FailedEntry>,
    overflow_count: u64,
    completed_events: u64,
    root_sidecar_id: Option<String>,
    root_cid: Option<String>,
}

impl WalkCounters {
    fn record_fail(&mut self, path: &Path, message: String) {
        if self.failed.len() < FAILED_LIST_CAP {
            self.failed.push(FailedEntry {
                path: path.to_string_lossy().into_owned(),
                message,
            });
        } else {
            self.overflow_count += 1;
        }
    }
}

/// Skip buckets surfaced in the summary modal. Each non-fatal filter
/// outcome maps to exactly one increment so the totals add up to the
/// number of pre-walk-counted candidates the walker considered.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkipCounts {
    pub hidden: u64,
    pub symlink: u64,
    /// FIFOs, sockets, block/char devices — entries that aren't addressable
    /// in the ingest model. Bucketed separately so they don't vanish from
    /// summary accounting (a stray Unix socket in a project tree shouldn't
    /// silently disappear).
    pub other: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedEntry {
    pub path: String,
    pub message: String,
}

/// Result returned to the frontend when `ingest_folder_tree` settles.
/// `cancelled: true` is set whenever the cancel registry's flag fired
/// before the walker reached the root's manifest build; `root_sidecar_id`
/// and `root_cid` are `Some` iff the root's bundle landed and its sidecar
/// row was inserted.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestFolderTreeResult {
    pub job_id: String,
    pub root_sidecar_id: Option<String>,
    pub root_cid: Option<String>,
    pub root_name: String,
    pub total_files_seen: u64,
    /// Pre-walk leaf count taken before the walker started. Used by the
    /// frontend's cancelled headline as the denominator ("Cancelled —
    /// added 4 of 100 files") so a mid-walk cancel doesn't claim a
    /// truncated total. `-1` when the pre-walk failed (unreadable root,
    /// permission errors); the modal falls back to `total_files_seen`
    /// in that case.
    pub pre_walk_total: i64,
    pub succeeded: u64,
    pub skipped: SkipCounts,
    pub failed: Vec<FailedEntry>,
    /// Number of failed entries that didn't fit in `failed` (cap =
    /// `FAILED_LIST_CAP`); the modal renders this as "and N more".
    pub failed_overflow: u64,
    pub cancelled: bool,
}

/// Tauri event payload for `folder-ingest-progress`. Emitted once per
/// settled leaf-file ingest. Directory manifest builds are deliberately
/// NOT counted here — `pre_walk_count` only enumerates non-filtered leaf
/// files, so emitting on dir builds would push `completed` past `total`
/// for any tree with subdirectories.
///
/// `total` is `-1` when the pre-walk couldn't enumerate the tree (e.g. the
/// root was unreadable mid-flight); the frontend's progress modal
/// degrades to indeterminate in that case.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderIngestProgressEvent {
    pub job_id: String,
    pub completed: u64,
    pub total: i64,
    pub current_path: String,
}

/// Cancellation registry stored on `NodeState`. Each in-flight
/// `ingest_folder_tree` IPC inserts an `Arc<AtomicBool>` keyed by its
/// minted `job_id`; `cancel_folder_ingest` flips the bool to `true`. The
/// walker entry strips the entry on settle, so completed jobs don't leak.
pub type CancelRegistry = Arc<Mutex<std::collections::HashMap<String, Arc<AtomicBool>>>>;

/// Returns true for names the walker must skip without descent or ingest.
///
/// `starts_with('.')` is intentionally broad — `.git`, `.gitignore`,
/// `.env`, `._DS_Store` and macOS resource forks all get the same
/// treatment. The exact-match arms cover the platform-junk that doesn't
/// start with a dot (`Thumbs.db`, `desktop.ini`).
pub fn should_filter_name(name: &str) -> bool {
    name == ".DS_Store" || name == "Thumbs.db" || name == "desktop.ini" || name.starts_with('.')
}

/// Cheap pre-walk that counts non-filtered leaf files under `root`. Used
/// to populate the progress event's `total` field so the modal can render
/// a determinate bar. Every regular file is counted — ZEB-161's streaming
/// pipeline handles arbitrary file sizes, so there is no per-leaf size cap
/// to apply here.
///
/// Returns `None` on any I/O error so the IPC can ship `total = -1` to the
/// frontend without aborting the actual walk. Symlinks are not followed
/// here either — `symlink_metadata` is the gate at every recursion.
fn pre_walk_count(root: &Path) -> Option<u64> {
    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Deny-list applies to every entry including the root — the IPC
        // entrypoint rejects deny-listed roots up-front, so this branch
        // is defence-in-depth for internal callers.
        if should_filter_name(&name) {
            continue;
        }
        if metadata.is_dir() {
            // Skip unreadable subdirs (e.g. permission-denied) so a single
            // hiccup degrades only the count, not the whole progress bar to
            // indeterminate. The actual walker also tolerates per-entry I/O
            // errors and continues; pre-walk should match its forgiveness.
            let Ok(read_dir) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry_res in read_dir {
                let Ok(entry) = entry_res else { continue };
                stack.push(entry.path());
            }
        } else if metadata.is_file() {
            total += 1;
        }
    }
    Some(total)
}

/// Outcome reported by a recursive `walk` call to its parent. Carries the
/// manifest entry to splice into the parent's children list on success,
/// communicates "skipped — counted in counters, no entry to splice" via
/// `Skipped`, and propagates fatal-only-to-this-subtree failures via
/// `Failed` (recorded against the parent's failure list).
enum WalkOutcome {
    Ingested(ManifestEntry),
    Skipped,
    Failed(String),
    Cancelled,
}

/// Per-call scratch space passed by reference down the recursion. Avoids
/// re-cloning the `AppHandle` and channel handles at every node.
struct WalkCtx<R: Runtime> {
    app: AppHandle<R>,
    ingest_tx: tokio::sync::mpsc::Sender<IngestRequest>,
    content_index: Arc<Mutex<ContentIndex>>,
    /// Relaxed ordering is sufficient — this is a one-way "asked to stop"
    /// flag with no companion data being published; we just need eventual
    /// visibility across threads, not a happens-before relationship with
    /// any other write.
    cancel: Arc<AtomicBool>,
    job_id: String,
    root_path: PathBuf,
    total: i64,
}

impl<R: Runtime> WalkCtx<R> {
    fn emit_progress(&self, counters: &mut WalkCounters, current_path: &Path) {
        counters.completed_events = counters.completed_events.saturating_add(1);
        let rel = current_path
            .strip_prefix(&self.root_path)
            .unwrap_or(current_path);
        let event = FolderIngestProgressEvent {
            job_id: self.job_id.clone(),
            completed: counters.completed_events,
            total: self.total,
            current_path: rel.to_string_lossy().into_owned(),
        };
        if let Err(e) = self.app.emit("folder-ingest-progress", &event) {
            tracing::warn!(
                job_id = %self.job_id,
                err = ?e,
                "failed to emit folder-ingest-progress",
            );
        }
    }
}

/// Entry point. Validates `root_path` once at the boundary, mints a
/// `job_id`, registers a fresh cancel flag, runs the walker, and removes
/// the cancel-registry entry on settle. The caller's `parent_sidecar_id`
/// and `parent_path` are passed through to the root's
/// `create_folder_with_children` path verbatim.
#[allow(clippy::too_many_arguments)]
pub async fn ingest_folder_tree<R: Runtime>(
    app: AppHandle<R>,
    ingest_tx: tokio::sync::mpsc::Sender<IngestRequest>,
    content_verb_tx: tokio::sync::mpsc::Sender<crate::event_loop::ContentVerbRequest>,
    content_index: Arc<Mutex<ContentIndex>>,
    cancel_registry: CancelRegistry,
    job_id: String,
    root_path: String,
    parent_sidecar_id: Option<String>,
    parent_path: Vec<String>,
) -> Result<IngestFolderTreeResult, String> {
    let root_pathbuf = PathBuf::from(&root_path);
    let root_name = root_pathbuf
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root_path.clone());

    // Validate up-front: the root MUST exist and be a directory. Without
    // this gate the walker's first symlink_metadata would surface as a
    // generic `failed[0]` entry against a nonexistent path, which is a
    // worse UX than a clean IPC reject (the frontend can show "couldn't
    // open folder" rather than an empty-summary modal with a confusing
    // single-row failure).
    let meta = std::fs::symlink_metadata(&root_pathbuf)
        .map_err(|e| format!("root_path stat failed: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("root_path is a symlink; refuse to follow".into());
    }
    if !meta.is_dir() {
        return Err("root_path is not a directory".into());
    }
    // Bot finding 4: the deny-list applies to the root too. A user
    // dropping `.git` silently ingesting its non-hidden children would
    // be a confusing data-leak. Reject at the IPC boundary with a clear
    // message rather than silently filtering — the user explicitly chose
    // this directory.
    if should_filter_name(&root_name) {
        return Err(format!(
            "Cannot ingest hidden/system folder as root: {root_name}"
        ));
    }

    // job_id is minted by the frontend before invoking this IPC so the
    // Cancel button can target the in-flight walk during the pre-walk
    // window (Bot finding 3). The walker uses the supplied id verbatim
    // for the registry insert, progress emits, and the result field.
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut reg = cancel_registry
            .lock()
            .map_err(|e| format!("cancel registry lock: {e}"))?;
        reg.insert(job_id.clone(), cancel.clone());
    }

    // Pre-walk for determinate progress. A failure here is non-fatal — we
    // ship total = -1 and the modal degrades to indeterminate. The pre-walk
    // does synchronous `std::fs::symlink_metadata` + `std::fs::read_dir` at
    // every node, so on massive trees or network-mounted FS the call can
    // block a tokio worker for seconds. Offload to a blocking thread so the
    // runtime stays responsive (cancel IPCs, progress emits, other IO can
    // still make progress).
    let root_for_pre_walk = root_pathbuf.clone();
    let total: i64 = tokio::task::spawn_blocking(move || pre_walk_count(&root_for_pre_walk))
        .await
        .ok()
        .flatten()
        .map(|n| n.try_into().unwrap_or(i64::MAX))
        .unwrap_or(-1);

    let ctx = WalkCtx {
        app,
        ingest_tx,
        content_index: content_index.clone(),
        cancel: cancel.clone(),
        job_id: job_id.clone(),
        root_path: root_pathbuf.clone(),
        total,
    };
    let mut counters = WalkCounters::default();

    let walk_outcome = walk(
        &ctx,
        &root_pathbuf,
        root_name.clone(),
        /* is_root = */ true,
        parent_sidecar_id.as_deref(),
        &parent_path,
        &content_verb_tx,
        &mut counters,
    )
    .await;

    // Strip the cancel entry on settle. Any in-flight cancel IPC racing
    // this remove finds an empty slot and returns Ok (best-effort
    // semantics — see `cancel_folder_ingest`).
    {
        if let Ok(mut reg) = cancel_registry.lock() {
            reg.remove(&job_id);
        }
    }

    // Only count the post-walk cancel flag if the walker never reached the
    // root manifest build — a late `cancel_folder_ingest` racing the result
    // assembly mustn't flip a successful ingest's `cancelled` flag (Bot
    // finding 8). `root_sidecar_id.is_some()` means the root settled, so
    // the cancel arrived after the work was complete.
    let cancelled = matches!(walk_outcome, WalkOutcome::Cancelled)
        || (cancel.load(Ordering::Relaxed) && counters.root_sidecar_id.is_none());

    // When the walker fails AT the root (e.g. create_folder_with_children
    // errors, or collect_sorted_dir_entries errors on root), the message
    // would otherwise be silently dropped — the result would have no root
    // sidecar, an empty failed list, and `cancelled = false`, leaving the
    // frontend to render a generic "failed before completing" headline
    // with zero diagnostic info. Push the message into `failed` so the
    // Failed section in the summary modal renders the real error.
    if let WalkOutcome::Failed(msg) = walk_outcome {
        counters.record_fail(&root_pathbuf, msg);
    }

    // total_files_seen is the count of leaves the walker actually touched
    // (succeeded + failed) — distinct from the pre-walk's `total`, which
    // estimates from the on-disk tree. The two can disagree if files
    // appear or disappear mid-walk.
    let total_files_seen = counters
        .succeeded
        .saturating_add(counters.failed.len() as u64)
        .saturating_add(counters.overflow_count);

    Ok(IngestFolderTreeResult {
        job_id,
        root_sidecar_id: counters.root_sidecar_id,
        root_cid: counters.root_cid,
        root_name,
        total_files_seen,
        pre_walk_total: total,
        succeeded: counters.succeeded,
        skipped: counters.skipped,
        failed: counters.failed,
        failed_overflow: counters.overflow_count,
        cancelled,
    })
}

/// ZEB-1014: best-effort rollback of a walk level's accumulated children
/// when the level itself fails or the walk is cancelled. Without it the
/// already-ingested subtrees are referenced by nothing (their parent
/// manifest never builds) and sit as unpinned cache orphans until
/// W-TinyLFU pressure or restart. `RollbackIngest`'s doom-set walks
/// descendants, so passing just the top-level child cids covers each
/// child's whole subtree; its keep-set spares anything a pinned root
/// claims via content dedup.
async fn rollback_children(
    content_verb_tx: &tokio::sync::mpsc::Sender<crate::event_loop::ContentVerbRequest>,
    children: &[ManifestEntry],
    context: &'static str,
) {
    if children.is_empty() {
        return;
    }
    let cids: Vec<harmony_content::cid::ContentId> = children
        .iter()
        .map(|c| harmony_content::cid::ContentId::from_bytes(c.cid))
        .collect();
    crate::rollback_ingested_cids(Some(content_verb_tx), &cids, context).await;
}

/// Recursive walker. Returns a `WalkOutcome` to its parent describing how
/// to splice this node into the parent's children list.
///
/// Async recursion is boxed via `async fn` -> `BoxFuture` to avoid the
/// "recursive `async fn` is not allowed" compile error.
#[allow(clippy::too_many_arguments)]
fn walk<'a, R: Runtime>(
    ctx: &'a WalkCtx<R>,
    path: &'a Path,
    name: String,
    is_root: bool,
    parent_sidecar_id: Option<&'a str>,
    parent_path: &'a [String],
    content_verb_tx: &'a tokio::sync::mpsc::Sender<crate::event_loop::ContentVerbRequest>,
    counters: &'a mut WalkCounters,
) -> futures::future::BoxFuture<'a, WalkOutcome> {
    Box::pin(async move {
        if ctx.cancel.load(Ordering::Relaxed) {
            return WalkOutcome::Cancelled;
        }

        let metadata = match tokio::fs::symlink_metadata(path).await {
            Ok(m) => m,
            Err(e) => return WalkOutcome::Failed(format!("stat: {e}")),
        };
        if metadata.file_type().is_symlink() {
            counters.skipped.symlink = counters.skipped.symlink.saturating_add(1);
            return WalkOutcome::Skipped;
        }
        // Deny-list applies to every entry. The IPC entrypoint already
        // rejects deny-listed roots with a clear error before the walker
        // starts; this guard is defence-in-depth for internal callers
        // and covers every descendant.
        if should_filter_name(&name) {
            counters.skipped.hidden = counters.skipped.hidden.saturating_add(1);
            return WalkOutcome::Skipped;
        }

        if metadata.is_dir() {
            // Enumerate then sort by name for determinism. read_dir
            // returns entries in OS order, which is inode-ordered on
            // ext4, alphabetical on APFS, and effectively undefined on
            // NTFS — tests would fail intermittently against the raw
            // order. Sorting now also stabilises manifest CIDs across
            // platforms for the same on-disk tree.
            let entries = match collect_sorted_dir_entries(path).await {
                Ok(v) => v,
                Err(e) => return WalkOutcome::Failed(format!("read_dir: {e}")),
            };

            let mut children: Vec<ManifestEntry> = Vec::with_capacity(entries.len());
            for (child_name, child_path) in entries {
                if ctx.cancel.load(Ordering::Relaxed) {
                    // ZEB-1014: this level's settled children are about to
                    // become unreferenced (their parent manifest will never
                    // build) — roll them back on the way out. Deeper levels
                    // do the same as `Cancelled` unwinds, composing to a
                    // best-effort evict of everything the cancelled walk
                    // admitted.
                    rollback_children(content_verb_tx, &children, "folder walk cancel").await;
                    return WalkOutcome::Cancelled;
                }
                match walk(
                    ctx,
                    &child_path,
                    child_name,
                    /* is_root = */ false,
                    None,
                    &[],
                    content_verb_tx,
                    counters,
                )
                .await
                {
                    WalkOutcome::Ingested(entry) => children.push(entry),
                    WalkOutcome::Skipped => {}
                    // A failed child already rolled back its own partial
                    // work (leaf arm / deeper dir arm); this level's other
                    // children stay — the walk continues without it.
                    WalkOutcome::Failed(msg) => counters.record_fail(&child_path, msg),
                    WalkOutcome::Cancelled => {
                        rollback_children(content_verb_tx, &children, "folder walk cancel").await;
                        return WalkOutcome::Cancelled;
                    }
                }
            }

            if ctx.cancel.load(Ordering::Relaxed) {
                rollback_children(content_verb_tx, &children, "folder walk cancel").await;
                return WalkOutcome::Cancelled;
            }

            if is_root {
                // Sidecar-creating path. Defers to lib.rs's
                // `create_folder_with_children` so the rekey/cascade logic
                // (for non-empty parent_path) and the rollback-on-ingest-
                // failure logic (for root-of-root drops) stays in one
                // place.
                // `children` is cloned into the call so the failure arm can
                // still roll the accumulated subtrees back (ZEB-1014); the
                // entries are 32-byte cids + names, cheap relative to the
                // walk itself.
                match crate::create_folder_with_children(
                    name.clone(),
                    children.clone(),
                    parent_sidecar_id.map(|s| s.to_string()),
                    parent_path.to_vec(),
                    &ctx.ingest_tx,
                    content_verb_tx,
                    &ctx.content_index,
                )
                .await
                {
                    Ok(result) => {
                        counters.root_sidecar_id = Some(result.sidecar_id.clone());
                        counters.root_cid = Some(result.cid.clone());
                        let cid_bytes = match hex_to_cid(&result.cid) {
                            Some(b) => b,
                            None => {
                                // Deliberately NO children rollback here:
                                // the root sidecar row landed and references
                                // this subtree — evicting the bytes would
                                // leave a listed-but-unfetchable folder.
                                return WalkOutcome::Failed(format!(
                                    "create_folder_with_children returned malformed cid {}",
                                    result.cid
                                ));
                            }
                        };
                        // Intentionally NO emit_progress here: dir-manifest
                        // builds aren't counted in `pre_walk_count`'s total,
                        // so firing here would push `completed` past `total`
                        // (e.g. dir/{a,b,sub/{c}} → total=3 but completed
                        // would reach 5). Progress fires only on leaf settle.
                        WalkOutcome::Ingested(ManifestEntry {
                            cid: cid_bytes,
                            name,
                            kind: ContentKind::Folder,
                        })
                    }
                    Err(e) => {
                        // ZEB-1014: the root create failed (sidecar row is
                        // rolled back or never inserted by the callee), so
                        // every accumulated subtree is unreferenced — evict
                        // it. The root's own manifest/bundle partial sends
                        // stay with W-TinyLFU (KB-scale; the callee doesn't
                        // report which of the two landed).
                        rollback_children(content_verb_tx, &children, "folder walk root").await;
                        WalkOutcome::Failed(e)
                    }
                }
            } else {
                // Manifest-only path: nested dirs do NOT get a sidecar.
                // They're reachable through the parent manifest's CID
                // pointer.
                match crate::build_folder_manifest_only(&ctx.ingest_tx, &name, &children).await {
                    Ok(cid) => {
                        // Intentionally NO emit_progress here — see the
                        // root-build branch above. Only leaf settles fire
                        // progress so `completed` stays bounded by
                        // `pre_walk_count`'s leaf-only `total`.
                        WalkOutcome::Ingested(ManifestEntry {
                            cid,
                            name,
                            kind: ContentKind::Folder,
                        })
                    }
                    Err(e) => {
                        // ZEB-1014: this dir's manifest never landed, so its
                        // whole already-ingested subtree is unreferenced —
                        // the parent records the failure and walks on, and
                        // nothing will ever point at these bytes. Evict them.
                        rollback_children(content_verb_tx, &children, "folder walk dir").await;
                        WalkOutcome::Failed(e.to_string())
                    }
                }
            }
        } else if metadata.is_file() {
            // Stream the leaf through the FastCDC chunker (ZEB-161): memory
            // is bounded by the chunker buffer (~1 MiB) regardless of file
            // size. CRITICAL: no per-leaf sidecar entry — leaves live in the
            // parent folder's manifest only; a per-leaf sidecar here would
            // pollute the root listing (ZEB-163 task 2 invariant).
            use harmony_content::chunker::ChunkerConfig;
            let file = match tokio::fs::File::open(path).await {
                Ok(f) => f,
                Err(e) => return WalkOutcome::Failed(format!("open: {e}")),
            };
            let reader = tokio::io::BufReader::new(file);
            // Pass `Some(&ctx.cancel)` so an in-flight cancel observes within
            // ~1 MiB of read time — without this, large-file ingest is
            // effectively non-cancellable until EOF. The walker's top-of-
            // recursion check already short-circuits between leaves; this
            // closes the long-pole gap inside a single large file.
            // ZEB-1014: the collecting variant records every ACKNOWLEDGED
            // admission so both failure arms below can evict the exact
            // partial set instead of leaving it for W-TinyLFU pressure.
            let mut sent: Vec<harmony_content::cid::ContentId> = Vec::new();
            match crate::streaming_ingest_collecting(
                reader,
                &ctx.ingest_tx,
                ChunkerConfig::DEFAULT,
                Some(&ctx.cancel),
                crate::IngestOptions::default(),
                &mut sent,
            )
            .await
            {
                Ok((root, _bytes)) => {
                    counters.succeeded = counters.succeeded.saturating_add(1);
                    ctx.emit_progress(counters, path);
                    WalkOutcome::Ingested(ManifestEntry {
                        cid: root.to_bytes(),
                        name,
                        kind: ContentKind::Leaf,
                    })
                }
                // Structural match on the typed `IngestError::Cancelled`
                // variant — `streaming_ingest_collecting` returns it when
                // its cancel parameter fires. Route to
                // `WalkOutcome::Cancelled` so the summary modal shows the
                // cancelled headline (and doesn't bucket it under
                // `Failed`). Any other error stays in the failure list.
                // Both arms roll back this file's partial admissions —
                // the parent levels' unwind handles their settled children.
                Err(crate::IngestError::Cancelled) => {
                    crate::rollback_ingested_cids(
                        Some(content_verb_tx),
                        &sent,
                        "folder walk leaf cancel",
                    )
                    .await;
                    WalkOutcome::Cancelled
                }
                Err(e) => {
                    crate::rollback_ingested_cids(Some(content_verb_tx), &sent, "folder walk leaf")
                        .await;
                    WalkOutcome::Failed(e.to_string())
                }
            }
        } else {
            // FIFOs, sockets, block/char devices: not addressable in the
            // ingest model; treat as a quiet skip rather than a failure
            // so a stray Unix socket in a project tree doesn't blow up
            // the summary. Bucketed under `other` so the summary
            // accounting reflects them rather than silently dropping.
            counters.skipped.other = counters.skipped.other.saturating_add(1);
            WalkOutcome::Skipped
        }
    })
}

/// Enumerate `path`'s children and return them sorted lexicographically by
/// file name. The sort is load-bearing: the resulting manifest's child
/// order is the byte-level order in the bundle, so two clients walking the
/// same on-disk tree must produce the same manifest CID (otherwise a re-
/// drop wouldn't dedupe).
async fn collect_sorted_dir_entries(path: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut rd = tokio::fs::read_dir(path).await?;
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        out.push((name, entry.path()));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Decode a 64-char lowercase-hex CID string into the 32-byte form the
/// manifest uses. Returns `None` for any malformed input — callers turn
/// `None` into a typed `WalkOutcome::Failed` so the surrounding manifest
/// build still completes for the other siblings.
fn hex_to_cid(hex: &str) -> Option<[u8; 32]> {
    let bytes = ::hex::decode(hex).ok()?;
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_helper_matches_spec() {
        assert!(should_filter_name(".DS_Store"));
        assert!(should_filter_name("Thumbs.db"));
        assert!(should_filter_name("desktop.ini"));
        assert!(should_filter_name(".git"));
        assert!(should_filter_name(".gitignore"));
        assert!(should_filter_name(".env"));
        // Dotfile is the catch-all — even unusual ones get filtered.
        assert!(should_filter_name(".anything_at_all"));
        assert!(!should_filter_name("README.md"));
        assert!(!should_filter_name("photo.jpg"));
        assert!(!should_filter_name("My Documents"));
    }

    /// Build a walk-root tempdir with a NON-dot prefix.
    ///
    /// `tempfile::tempdir()` defaults to a `.tmp<rand>` basename; the deny-list
    /// in `should_filter_name` rejects any `.`-prefixed name — including the walk
    /// root itself (see the root guard at ~line 291) — so a default tempdir root
    /// is filtered out wholesale and the walk sees zero files (ZEB-332 / ZEB-306).
    /// Production roots are user-chosen and never dot-prefixed, so this is purely
    /// a test-scaffolding concern; the production filter is intentionally unchanged.
    fn walk_root_tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ingest-test")
            .tempdir()
            .expect("tempdir")
    }

    #[test]
    fn pre_walk_counts_only_non_filtered_leaves() {
        let dir = walk_root_tempdir();
        std::fs::write(dir.path().join("a.txt"), b"a").expect("write");
        std::fs::write(dir.path().join("b.txt"), b"b").expect("write");
        std::fs::write(dir.path().join(".hidden"), b"x").expect("write");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub").join("c.txt"), b"c").expect("write");
        // Sub-dir contents under a hidden ancestor MUST also be excluded —
        // pre_walk_count never descends through filtered names.
        std::fs::create_dir(dir.path().join(".git")).expect("mkdir");
        std::fs::write(dir.path().join(".git").join("HEAD"), b"h").expect("write");

        let n = pre_walk_count(dir.path()).expect("pre_walk");
        assert_eq!(n, 3, "expected a.txt + b.txt + sub/c.txt");
    }

    #[test]
    fn record_fail_caps_at_50_and_counts_overflow() {
        let mut c = WalkCounters::default();
        for i in 0..(FAILED_LIST_CAP + 7) {
            c.record_fail(Path::new(&format!("/x/{i}")), format!("err {i}"));
        }
        assert_eq!(c.failed.len(), FAILED_LIST_CAP);
        assert_eq!(c.overflow_count, 7);
    }

    /// Spawn a draining ingest handler so `send_ingest`'s oneshot reply
    /// doesn't stall the walker. Returns the sender plus a join handle
    /// (dropped at end of test — channel closes when sender is dropped).
    fn spawn_draining_ingest_handler() -> tokio::sync::mpsc::Sender<IngestRequest> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<IngestRequest>(64);
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = req.reply.send(Ok(()));
            }
        });
        tx
    }

    /// Build a fresh shared `ContentIndex` backed by a leaked tempdir
    /// (matches the lib.rs `path_ingest_tests::fresh_content_index` pattern).
    fn fresh_content_index() -> Arc<Mutex<ContentIndex>> {
        let dir = tempfile::tempdir().expect("tempdir");
        let idx = ContentIndex::load(Some(&crate::device_dataset_file::test_cipher()), dir.path());
        std::mem::forget(dir);
        Arc::new(Mutex::new(idx))
    }

    /// Regression for ZEB-163 task 2: dropping a folder with N leaf files
    /// must add exactly ONE sidecar entry (the root folder), not N+1.
    /// Before the bytes-only-leaf fix (b52fef6), the walker called
    /// `send_ingest_with_name` per leaf, which inserted a sidecar row each
    /// time and surfaced "cat.jpg" and "dog.jpg" as standalone root rows
    /// alongside the new folder.
    ///
    /// This drives the real walker via `ingest_folder_tree` over a 2-leaf
    /// tmpdir and inspects the shared sidecar index before/after. The
    /// previous version of this test only called `send_ingest_bytes_only`
    /// against a local index that the function under test couldn't touch
    /// even in a buggy implementation, so it asserted nothing.
    #[tokio::test]
    async fn walker_leaf_ingest_does_not_pollute_root_listing() {
        let app = tauri::test::mock_app();
        let handle = app.handle().clone();

        let ingest_tx = spawn_draining_ingest_handler();
        // Walker only routes through verb_tx for nested-parent creates
        // (non-empty parent_path). Root drops never touch it, so a
        // dropped receiver is fine — we just need a live sender.
        let (verb_tx, _verb_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::ContentVerbRequest>(8);
        let index = fresh_content_index();
        let registry: CancelRegistry = Arc::new(Mutex::new(Default::default()));

        let dir = walk_root_tempdir();
        std::fs::write(dir.path().join("cat.jpg"), b"meow").expect("write cat");
        std::fs::write(dir.path().join("dog.jpg"), b"woof").expect("write dog");

        let before = index.lock().unwrap().entries().count();

        let result = ingest_folder_tree(
            handle,
            ingest_tx,
            verb_tx,
            index.clone(),
            registry,
            "test-job-pollute".to_string(),
            dir.path().to_string_lossy().into_owned(),
            None,
            Vec::new(),
        )
        .await
        .expect("ingest_folder_tree");

        let after = index.lock().unwrap().entries().count();
        assert_eq!(
            after - before,
            1,
            "walker must add exactly ONE sidecar (the dropped root folder), \
             not N+1 for N leaf files — got delta {} for 2-leaf tree",
            after - before
        );
        assert_eq!(result.succeeded, 2, "both leaves should have settled");
        assert!(result.root_sidecar_id.is_some(), "root sidecar must mint");
        assert!(!result.cancelled, "no cancel flag was set");
    }

    /// Pins the cancellation contract: when the cancel flag is true before
    /// any work runs, `walk` returns `WalkOutcome::Cancelled` immediately
    /// (checked at top-of-recursion, line ~357) and the IPC surface
    /// reports `cancelled: true` with the registry entry stripped on
    /// settle (the `ingest_folder_tree` epilogue removes the slot
    /// unconditionally — see lines ~307-311).
    ///
    /// We drive both contracts in one test by calling `walk` directly
    /// for the cancel-result assertion (avoids racing the registry to
    /// pre-seed the flag) and `ingest_folder_tree` separately for the
    /// registry-cleanup assertion against a normal-settle walk.
    #[tokio::test]
    async fn walker_honors_cancel_flag_and_cleans_up_registry() {
        let app = tauri::test::mock_app();
        let handle = app.handle().clone();

        let ingest_tx = spawn_draining_ingest_handler();
        let (verb_tx, _verb_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::ContentVerbRequest>(8);
        let index = fresh_content_index();

        let dir = walk_root_tempdir();
        std::fs::write(dir.path().join("a.txt"), b"a").expect("write a");
        std::fs::write(dir.path().join("b.txt"), b"b").expect("write b");

        // --- Part 1: cancel-before-walk returns WalkOutcome::Cancelled.
        let cancel = Arc::new(AtomicBool::new(true));
        let ctx = WalkCtx {
            app: handle.clone(),
            ingest_tx: ingest_tx.clone(),
            content_index: index.clone(),
            cancel: cancel.clone(),
            job_id: "test-job".to_string(),
            root_path: dir.path().to_path_buf(),
            total: 2,
        };
        let mut counters = WalkCounters::default();
        let outcome = walk(
            &ctx,
            dir.path(),
            "root".to_string(),
            /* is_root = */ true,
            None,
            &[],
            &verb_tx,
            &mut counters,
        )
        .await;
        assert!(
            matches!(outcome, WalkOutcome::Cancelled),
            "pre-set cancel flag must short-circuit at top-of-recursion"
        );

        // --- Part 2: a normal-settle IPC strips its registry entry.
        let registry: CancelRegistry = Arc::new(Mutex::new(Default::default()));
        let result = ingest_folder_tree(
            handle,
            ingest_tx,
            verb_tx,
            index.clone(),
            registry.clone(),
            "test-job-cleanup".to_string(),
            dir.path().to_string_lossy().into_owned(),
            None,
            Vec::new(),
        )
        .await
        .expect("ingest_folder_tree");
        assert!(
            !result.cancelled,
            "normal settle must report cancelled=false"
        );
        assert!(
            registry.lock().unwrap().is_empty(),
            "registry entry must be stripped after settle, got {} entries",
            registry.lock().unwrap().len()
        );
    }
}
