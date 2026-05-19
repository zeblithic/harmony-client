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
//! Files larger than `FLAT_BUNDLE_MAX` are skipped with a count rather than
//! failed — ZEB-161 (nested bundles) is the planned successor that lifts the
//! cap; until then, oversized files surface in the summary modal under their
//! own bucket.
//!
//! Cancellation is best-effort: the walker checks the shared `AtomicBool` at
//! the top of every recursive call and the top of every per-child loop
//! iteration. On cancel the walker returns immediately with whatever has
//! been ingested so far — partial children remain in the chunk cache (no
//! references hold them, so W-TinyLFU evicts on pressure), and the result's
//! `cancelled` flag is `true`. `root_sidecar_id` is `None` whenever cancel
//! fires before the root's manifest gets built.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};

use crate::content_index::{ContentIndex, ContentKind};
use crate::event_loop::IngestRequest;
use crate::folders::ManifestEntry;
use crate::{ingest_file_at_path, IngestError, FLAT_BUNDLE_MAX};

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
    pub oversized: u64,
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
    pub succeeded: u64,
    pub skipped: SkipCounts,
    pub failed: Vec<FailedEntry>,
    /// Number of failed entries that didn't fit in `failed` (cap =
    /// `FAILED_LIST_CAP`); the modal renders this as "and N more".
    pub failed_overflow: u64,
    pub cancelled: bool,
}

/// Tauri event payload for `folder-ingest-progress`. Emitted once per
/// settled leaf-file ingest and once per directory manifest build.
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
/// a determinate bar. Skips files larger than `FLAT_BUNDLE_MAX` so the
/// count matches what the actual walker will settle (oversized files are
/// `Skipped`, not `Succeeded`).
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
        // The root's own name is not subject to the filter — a user is
        // free to drop a `.config` folder if they want — but every
        // descendant is.
        let is_root = path == root;
        if !is_root && should_filter_name(&name) {
            continue;
        }
        if metadata.is_dir() {
            let read_dir = std::fs::read_dir(&path).ok()?;
            for entry in read_dir.flatten() {
                stack.push(entry.path());
            }
        } else if metadata.is_file() && metadata.len() <= FLAT_BUNDLE_MAX {
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

    let job_id = uuid::Uuid::new_v4().to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut reg = cancel_registry
            .lock()
            .map_err(|e| format!("cancel registry lock: {e}"))?;
        reg.insert(job_id.clone(), cancel.clone());
    }

    // Pre-walk for determinate progress. A failure here is non-fatal — we
    // ship total = -1 and the modal degrades to indeterminate. Doing the
    // pre-walk synchronously (on the tokio runtime thread) is acceptable
    // because it's a stat-only pass; no large reads.
    let total: i64 = pre_walk_count(&root_pathbuf)
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

    let cancelled =
        matches!(walk_outcome, WalkOutcome::Cancelled) || cancel.load(Ordering::Relaxed);

    // total_files_seen is the count of leaves the walker actually touched
    // (succeeded + skipped.oversized + failed) — distinct from the
    // pre-walk's `total`, which estimates from the on-disk tree. The two
    // can disagree if files appear or disappear mid-walk.
    let total_files_seen = counters
        .succeeded
        .saturating_add(counters.skipped.oversized)
        .saturating_add(counters.failed.len() as u64)
        .saturating_add(counters.overflow_count);

    Ok(IngestFolderTreeResult {
        job_id,
        root_sidecar_id: counters.root_sidecar_id,
        root_cid: counters.root_cid,
        root_name,
        total_files_seen,
        succeeded: counters.succeeded,
        skipped: counters.skipped,
        failed: counters.failed,
        failed_overflow: counters.overflow_count,
        cancelled,
    })
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
        // The root's own name is exempt — a user dropping `.config` did so
        // deliberately; every descendant still goes through the filter.
        if !is_root && should_filter_name(&name) {
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
                    WalkOutcome::Failed(msg) => counters.record_fail(&child_path, msg),
                    WalkOutcome::Cancelled => return WalkOutcome::Cancelled,
                }
            }

            if ctx.cancel.load(Ordering::Relaxed) {
                return WalkOutcome::Cancelled;
            }

            if is_root {
                // Sidecar-creating path. Defers to lib.rs's
                // `create_folder_with_children` so the rekey/cascade logic
                // (for non-empty parent_path) and the rollback-on-ingest-
                // failure logic (for root-of-root drops) stays in one
                // place.
                match crate::create_folder_with_children(
                    name.clone(),
                    children,
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
                                return WalkOutcome::Failed(format!(
                                    "create_folder_with_children returned malformed cid {}",
                                    result.cid
                                ));
                            }
                        };
                        ctx.emit_progress(counters, path);
                        WalkOutcome::Ingested(ManifestEntry {
                            cid: cid_bytes,
                            name,
                            kind: ContentKind::Folder,
                        })
                    }
                    Err(e) => WalkOutcome::Failed(e),
                }
            } else {
                // Manifest-only path: nested dirs do NOT get a sidecar.
                // They're reachable through the parent manifest's CID
                // pointer.
                match crate::build_folder_manifest_only(&ctx.ingest_tx, &name, &children).await {
                    Ok(cid) => {
                        ctx.emit_progress(counters, path);
                        WalkOutcome::Ingested(ManifestEntry {
                            cid,
                            name,
                            kind: ContentKind::Folder,
                        })
                    }
                    Err(e) => WalkOutcome::Failed(e.to_string()),
                }
            }
        } else if metadata.is_file() {
            // Cheap oversized check from metadata. The deeper helper
            // re-checks against the bytes actually read (TOCTOU), but the
            // metadata gate keeps us from issuing the 8+ GiB `read` for
            // files we'd reject anyway.
            if metadata.len() > FLAT_BUNDLE_MAX {
                counters.skipped.oversized = counters.skipped.oversized.saturating_add(1);
                return WalkOutcome::Skipped;
            }
            match ingest_file_at_path(&ctx.ingest_tx, &ctx.content_index, path, None, name.clone())
                .await
            {
                Ok(result) => {
                    counters.succeeded = counters.succeeded.saturating_add(1);
                    let cid_bytes = match hex_to_cid(&result.cid) {
                        Some(b) => b,
                        None => {
                            return WalkOutcome::Failed(format!(
                                "ingest returned malformed cid {}",
                                result.cid
                            ));
                        }
                    };
                    ctx.emit_progress(counters, path);
                    WalkOutcome::Ingested(ManifestEntry {
                        cid: cid_bytes,
                        name,
                        kind: ContentKind::Leaf,
                    })
                }
                Err(IngestError::Symlink) => {
                    counters.skipped.symlink = counters.skipped.symlink.saturating_add(1);
                    WalkOutcome::Skipped
                }
                Err(IngestError::Oversized { .. }) => {
                    counters.skipped.oversized = counters.skipped.oversized.saturating_add(1);
                    WalkOutcome::Skipped
                }
                Err(e) => WalkOutcome::Failed(e.to_string()),
            }
        } else {
            // FIFOs, sockets, block/char devices: not addressable in the
            // ingest model; treat as a quiet skip rather than a failure
            // so a stray Unix socket in a project tree doesn't blow up
            // the summary.
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

    #[test]
    fn pre_walk_counts_only_non_filtered_under_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
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
}
