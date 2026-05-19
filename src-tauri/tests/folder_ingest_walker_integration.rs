//! ZEB-163 Task 8: integration tests for the folder-ingest walker.
//!
//! Drives `harmony_app::folder_ingest::ingest_folder_tree` against real
//! tempdir fixtures and a recording ingest handler, so each scenario
//! exercises the full path: pre-walk, recursive descent, manifest build,
//! sidecar insert, and progress emission.
//!
//! Coverage is deliberately distinct from the inline `#[cfg(test)] mod
//! tests` block in `src/folder_ingest.rs`:
//!
//! - Inline tests pin **helper-level** invariants
//!   (`should_filter_name`, `pre_walk_count`, `record_fail` cap,
//!   leaf-not-polluting-root, and pre-set cancel flag short-circuits).
//! - This file pins **walker-shape** invariants that need an end-to-end
//!   ingest + sidecar inspection: sorted order, bottom-up build order,
//!   empty subdir, deny-list, symlinks, oversized files, mid-walk cancel,
//!   per-leaf I/O failure, and pre-walk failure on a missing root.
//!
//! The walker is driven with `parent_path = []` (root drops) throughout,
//! so the `content_verb_tx` channel is never exercised — the test handler
//! drops the rx and only holds a live sender.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harmony_app::content_index::{ContentIndex, ContentKind};
use harmony_app::event_loop::{ContentVerbRequest, IngestRequest};
use harmony_app::folder_ingest::{self, CancelRegistry};
use harmony_app::folders;
use harmony_content::cid::{ContentFlags, ContentId};

// ── Fixtures ────────────────────────────────────────────────────────────────

/// In-test ingest handler: records the order of `(cid_hex, bytes)` pairs
/// pushed through `ingest_tx` and ACKs success. Order matters for the
/// bottom-up assertion in `nested_two_level_tree_builds_bottom_up`.
type IngestLog = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

fn spawn_recording_ingest_handler() -> (tokio::sync::mpsc::Sender<IngestRequest>, IngestLog) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<IngestRequest>(128);
    let log: IngestLog = Arc::new(Mutex::new(Vec::new()));
    let log_clone = log.clone();
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            log_clone.lock().unwrap().push((req.cid_hex, req.data));
            let _ = req.reply.send(Ok(()));
        }
    });
    (tx, log)
}

/// Fresh in-memory `ContentIndex` backed by a leaked tempdir — matches the
/// inline `folder_ingest::tests::fresh_content_index` and the lib.rs
/// `path_ingest_tests::fresh_content_index` patterns.
fn fresh_content_index() -> Arc<Mutex<ContentIndex>> {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx = ContentIndex::load(dir.path());
    // Persist directory is examined only via the in-memory entries() iterator
    // here; leaking keeps the directory alive without obscuring the test body.
    std::mem::forget(dir);
    Arc::new(Mutex::new(idx))
}

/// Bag of channels + state every test needs. Held by name so individual
/// scenarios can reach in for the recorded ingest log or the index.
struct WalkerHarness {
    app: tauri::AppHandle<tauri::test::MockRuntime>,
    ingest_tx: tokio::sync::mpsc::Sender<IngestRequest>,
    verb_tx: tokio::sync::mpsc::Sender<ContentVerbRequest>,
    index: Arc<Mutex<ContentIndex>>,
    registry: CancelRegistry,
    log: IngestLog,
}

fn fresh_harness() -> WalkerHarness {
    let app = tauri::test::mock_app();
    let app_handle = app.handle().clone();
    let (ingest_tx, log) = spawn_recording_ingest_handler();
    // Verb-rx is dropped; root drops never route through it. Keeping the
    // sender live is the only requirement for the walker entry to clone
    // it through the call chain.
    let (verb_tx, _verb_rx) = tokio::sync::mpsc::channel::<ContentVerbRequest>(8);
    WalkerHarness {
        app: app_handle,
        ingest_tx,
        verb_tx,
        index: fresh_content_index(),
        registry: Arc::new(Mutex::new(Default::default())),
        log,
    }
}

/// Drive `ingest_folder_tree` against the harness with a root drop
/// (`parent_path = []`, `parent_sidecar_id = None`). Cloning the channels
/// here lets the test hold the originals for post-walk inspection.
async fn run_walker(
    h: &WalkerHarness,
    root_path: PathBuf,
) -> Result<folder_ingest::IngestFolderTreeResult, String> {
    folder_ingest::ingest_folder_tree(
        h.app.clone(),
        h.ingest_tx.clone(),
        h.verb_tx.clone(),
        h.index.clone(),
        h.registry.clone(),
        uuid::Uuid::new_v4().to_string(),
        root_path.to_string_lossy().into_owned(),
        None,
        Vec::new(),
    )
    .await
}

/// Compute the leaf-bytes CID the walker would use for `send_ingest_bytes_only`.
/// Tests use this to look up specific leaves in the recorded ingest log
/// and to assert ordering invariants.
fn leaf_cid_hex(bytes: &[u8]) -> String {
    let cid = ContentId::for_book(bytes, ContentFlags::default()).expect("leaf CID");
    hex::encode(cid.to_bytes())
}

/// Decode the captured manifest bytes for the root sidecar's bundle CID.
/// Returns the ordered `ManifestEntry` list, panicking with a helpful
/// message if the bundle isn't in the log or the manifest book preceding it
/// can't be parsed. Only safe to call once the walker has settled.
fn parse_root_manifest(log: &IngestLog, root_bundle_cid_hex: &str) -> Vec<folders::ManifestEntry> {
    let snapshot = log.lock().unwrap().clone();

    // The bundle for a folder always ingests in the order
    // (manifest book, bundle) — see send_ingest_bytes_only's chunked-or-flat
    // helper and `build_folder_manifest_only` / `create_folder_at_root_with_children`
    // in lib.rs. The bundle bytes start with the bundle header (BundleBuilder
    // output), and we don't need to re-parse them — we read the manifest
    // book directly. The manifest's CID is whichever entry in the log was
    // pushed immediately before the bundle entry.
    let bundle_idx = snapshot
        .iter()
        .position(|(c, _)| c == root_bundle_cid_hex)
        .unwrap_or_else(|| {
            panic!(
                "expected root bundle CID {root_bundle_cid_hex} in ingest log; \
                 saw {:?}",
                snapshot.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>()
            )
        });
    assert!(
        bundle_idx >= 1,
        "root bundle must be preceded by its manifest book in the ingest log"
    );
    let (_manifest_cid_hex, manifest_bytes) = &snapshot[bundle_idx - 1];
    let parsed = folders::parse_manifest(manifest_bytes)
        .expect("root manifest book parses with current MANIFEST_VERSION");
    parsed.folder_manifest.entries
}

// ── Test 1: flat dir of 3 leaves ────────────────────────────────────────────

#[tokio::test]
async fn flat_dir_three_leaves_sorted_alphabetically() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Write in non-alphabetical order so the assertion proves the walker
    // sorts (not just preserves insertion order, which happens to be the
    // OS-dependent read_dir order).
    std::fs::write(dir.path().join("charlie.txt"), b"c-data").expect("write c");
    std::fs::write(dir.path().join("alpha.txt"), b"a-data").expect("write a");
    std::fs::write(dir.path().join("bravo.txt"), b"b-data").expect("write b");

    let h = fresh_harness();
    let result = run_walker(&h, dir.path().to_path_buf())
        .await
        .expect("ingest_folder_tree succeeds on a flat directory of 3 leaves");

    assert_eq!(
        result.succeeded, 3,
        "all 3 leaves must report succeeded; got result = {result:?}"
    );
    assert_eq!(result.skipped.hidden, 0, "no hidden files in fixture");
    assert_eq!(result.skipped.symlink, 0, "no symlinks in fixture");
    assert_eq!(result.skipped.oversized, 0, "no oversized files in fixture");
    assert!(result.failed.is_empty(), "no failures expected");
    assert!(!result.cancelled, "walker was not cancelled");
    assert!(
        result.root_sidecar_id.is_some(),
        "root sidecar id must mint on a successful walk"
    );

    // Exactly one sidecar entry — the root folder. Leaves live only in the
    // manifest; verifying again here (alongside the inline regression test
    // for the 2-leaf case) keeps the "1 sidecar per drop" invariant pinned
    // at the integration level.
    let sidecar_count = h.index.lock().unwrap().entries().count();
    assert_eq!(
        sidecar_count, 1,
        "exactly one sidecar row (the root folder) must be inserted for a flat 3-leaf drop"
    );

    let root_cid = result
        .root_cid
        .as_ref()
        .expect("root_cid must be Some when root_sidecar_id is Some");
    let entries = parse_root_manifest(&h.log, root_cid);
    assert_eq!(entries.len(), 3, "manifest must list all 3 leaves");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["alpha.txt", "bravo.txt", "charlie.txt"],
        "manifest entries must be alphabetically sorted by file name"
    );
    for entry in &entries {
        assert_eq!(entry.kind, ContentKind::Leaf, "all 3 entries are leaves");
    }
}

// ── Test 2: nested 2-level tree builds bottom-up ───────────────────────────

#[tokio::test]
async fn nested_two_level_tree_builds_bottom_up_with_single_sidecar() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("sub")).expect("mkdir sub");
    let leaf_bytes = b"nested-leaf-data";
    std::fs::write(dir.path().join("sub").join("leaf.bin"), leaf_bytes).expect("write leaf");

    let h = fresh_harness();
    let result = run_walker(&h, dir.path().to_path_buf())
        .await
        .expect("ingest_folder_tree succeeds on a nested 2-level tree");

    assert_eq!(result.succeeded, 1, "exactly one leaf settled");
    assert!(result.failed.is_empty(), "no failures expected");
    assert!(!result.cancelled);
    let root_cid = result.root_cid.as_ref().expect("root_cid set");

    // Sidecar invariant: nested directories MUST NOT produce sidecar rows.
    // A 2-level tree produces exactly one sidecar (the root).
    let sidecar_count = h.index.lock().unwrap().entries().count();
    assert_eq!(
        sidecar_count, 1,
        "nested manifest must not produce a sidecar — only the root drop does"
    );

    // Bottom-up build order: the leaf CID must appear in the ingest log
    // BEFORE the root bundle CID — i.e., the walker descended, settled the
    // leaf, built the sub manifest, then built the root manifest.
    let log_snapshot = h.log.lock().unwrap().clone();
    let leaf_cid = leaf_cid_hex(leaf_bytes);
    let leaf_idx = log_snapshot
        .iter()
        .position(|(c, _)| c == &leaf_cid)
        .expect("leaf CID must appear in ingest log");
    let root_idx = log_snapshot
        .iter()
        .position(|(c, _)| c == root_cid)
        .expect("root bundle CID must appear in ingest log");
    assert!(
        leaf_idx < root_idx,
        "bottom-up: leaf must ingest before root bundle (leaf @ {leaf_idx}, root @ {root_idx})"
    );

    // Root manifest entry for `sub` is kind=Folder and references the sub
    // bundle CID — we don't precompute the sub CID, but we can assert the
    // root manifest has exactly one entry and that entry is the sub folder.
    let entries = parse_root_manifest(&h.log, root_cid);
    assert_eq!(
        entries.len(),
        1,
        "root manifest has exactly one child (sub)"
    );
    assert_eq!(entries[0].name, "sub");
    assert_eq!(entries[0].kind, ContentKind::Folder);

    // The sub bundle CID referenced by the root manifest must also appear
    // in the ingest log AFTER the leaf but BEFORE the root bundle —
    // confirming the sub manifest was built between the leaf and the root.
    let sub_cid_hex = hex::encode(entries[0].cid);
    let sub_idx = log_snapshot
        .iter()
        .position(|(c, _)| c == &sub_cid_hex)
        .expect("sub bundle CID must appear in ingest log");
    assert!(
        leaf_idx < sub_idx && sub_idx < root_idx,
        "bottom-up order: leaf ({leaf_idx}) -> sub ({sub_idx}) -> root ({root_idx})"
    );
}

// ── Test 3: empty subdirectory ──────────────────────────────────────────────

#[tokio::test]
async fn empty_subdir_appears_in_root_manifest_as_folder_with_no_leaves() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("empty_sub")).expect("mkdir empty_sub");

    let h = fresh_harness();
    let result = run_walker(&h, dir.path().to_path_buf())
        .await
        .expect("ingest_folder_tree succeeds on an empty subdir tree");

    assert_eq!(
        result.succeeded, 0,
        "no leaves to settle in an empty-subdir tree"
    );
    assert!(result.failed.is_empty());
    assert!(!result.cancelled);
    let root_cid = result.root_cid.as_ref().expect("root_cid set");

    let entries = parse_root_manifest(&h.log, root_cid);
    assert_eq!(entries.len(), 1, "root manifest contains the empty subdir");
    assert_eq!(entries[0].name, "empty_sub");
    assert_eq!(
        entries[0].kind,
        ContentKind::Folder,
        "empty subdir surfaces as kind=Folder even with no children"
    );

    // Only one sidecar (the root) — the empty subdir is manifest-only.
    let sidecar_count = h.index.lock().unwrap().entries().count();
    assert_eq!(sidecar_count, 1);
}

// ── Test 4: deny-list junk files ────────────────────────────────────────────

#[tokio::test]
async fn deny_list_skips_ds_store_thumbs_db_and_dotfiles() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Real leaf survivors so the manifest has a non-empty positive shape.
    std::fs::write(dir.path().join("keeper.txt"), b"keep").expect("write keeper");
    // The 3 spec-listed junk names plus a .git directory whose HEAD must
    // also not appear (descent into a filtered dir must not happen).
    std::fs::write(dir.path().join(".DS_Store"), b"junk").expect("write DS_Store");
    std::fs::write(dir.path().join("Thumbs.db"), b"junk").expect("write Thumbs.db");
    std::fs::create_dir(dir.path().join(".git")).expect("mkdir .git");
    std::fs::write(dir.path().join(".git").join("HEAD"), b"ref:").expect("write .git/HEAD");

    let h = fresh_harness();
    let result = run_walker(&h, dir.path().to_path_buf())
        .await
        .expect("ingest_folder_tree succeeds with deny-list filtering");

    // The walker bumps `skipped.hidden` once per filtered top-level name.
    // Descent into .git is short-circuited at the directory boundary, so
    // .git/HEAD never increments the counter — it's invisible to the walker.
    assert_eq!(
        result.skipped.hidden, 3,
        ".DS_Store + Thumbs.db + .git must each increment hidden once (descent into filtered dirs is suppressed); got {}",
        result.skipped.hidden
    );
    assert_eq!(result.succeeded, 1, "only keeper.txt survives");

    let root_cid = result.root_cid.as_ref().expect("root_cid set");
    let entries = parse_root_manifest(&h.log, root_cid);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["keeper.txt"],
        "manifest must NOT include any filtered junk names"
    );
}

// ── Test 5: symlink to a file (unix-only) ──────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn symlink_to_file_is_skipped() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real.txt");
    std::fs::write(&real, b"real-data").expect("write real");
    let link = dir.path().join("link.txt");
    symlink(&real, &link).expect("create file symlink");

    let h = fresh_harness();
    let result = run_walker(&h, dir.path().to_path_buf())
        .await
        .expect("ingest_folder_tree succeeds with a file symlink in the tree");

    assert_eq!(
        result.skipped.symlink, 1,
        "the file symlink must be counted under symlink skip bucket; got {}",
        result.skipped.symlink
    );
    assert_eq!(
        result.succeeded, 1,
        "only the real file (real.txt) must settle"
    );

    let root_cid = result.root_cid.as_ref().expect("root_cid set");
    let entries = parse_root_manifest(&h.log, root_cid);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["real.txt"],
        "manifest must NOT include the symlink name; got {names:?}"
    );
}

// ── Test 6: symlink to a directory (unix-only) ─────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn symlink_to_directory_is_not_followed() {
    use std::os::unix::fs::symlink;

    // Target dir lives OUTSIDE the walked root so we can prove the walker
    // never descended into it. If the walker followed the symlink, the
    // target's distinctive leaf bytes would land in the ingest log.
    let outside = tempfile::tempdir().expect("outside tempdir");
    let outside_leaf_bytes = b"outside-target-leaf";
    std::fs::write(outside.path().join("inside_target.txt"), outside_leaf_bytes)
        .expect("write outside leaf");

    let dir = tempfile::tempdir().expect("walk-root tempdir");
    // A real leaf survivor so the manifest isn't empty.
    std::fs::write(dir.path().join("plain.txt"), b"plain").expect("write plain");
    let link_dir = dir.path().join("link_to_outside");
    symlink(outside.path(), &link_dir).expect("create dir symlink");

    let h = fresh_harness();
    let result = run_walker(&h, dir.path().to_path_buf())
        .await
        .expect("ingest_folder_tree succeeds with a dir-symlink in the tree");

    assert_eq!(
        result.skipped.symlink, 1,
        "the directory symlink must be counted exactly once; got {}",
        result.skipped.symlink
    );
    assert_eq!(
        result.succeeded, 1,
        "only the real leaf plain.txt should settle, not the symlink target's child"
    );

    // The outside leaf's CID must NOT appear in the ingest log — if it does,
    // the walker followed the symlink.
    let target_cid_hex = leaf_cid_hex(outside_leaf_bytes);
    let log_snapshot = h.log.lock().unwrap().clone();
    assert!(
        log_snapshot.iter().all(|(c, _)| c != &target_cid_hex),
        "symlink target's leaf CID {target_cid_hex} must not appear in ingest log — walker followed a directory symlink"
    );

    let root_cid = result.root_cid.as_ref().expect("root_cid set");
    let entries = parse_root_manifest(&h.log, root_cid);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["plain.txt"],
        "manifest must NOT include the dir symlink; got {names:?}"
    );
}

// ── Test 7: oversized file via sparse `set_len` (unix-only) ────────────────

/// `set_len` on Linux/macOS creates a sparse file: the on-disk allocation
/// stays at a few extents while the apparent size matches what we set. The
/// walker's oversized check reads `metadata().len()` (apparent size), so the
/// skip path fires without us actually allocating ~8 GiB.
///
/// Windows is excluded because NTFS does not auto-sparse via std `set_len`
/// — the operation would zero-fill and run for minutes (or fail under disk
/// pressure). The spec's Task 8 note flags this exact fallback; CI runs on
/// Linux so the gate doesn't reduce coverage in practice.
#[cfg(unix)]
#[tokio::test]
async fn oversized_file_is_skipped_via_sparse_extension() {
    use std::fs::OpenOptions;

    // FLAT_BUNDLE_MAX is `pub(crate)` in lib.rs and not visible to
    // integration tests; recompute it here from the same public
    // harmony-content constants the lib uses. Keeping this expression in
    // sync with `lib.rs::FLAT_BUNDLE_MAX` is a manual contract — if the
    // upstream constants drift, this test will silently produce a
    // too-small "oversized" file. Both sides come from harmony-content's
    // public API so the drift surface is small.
    let flat_bundle_max: u64 = (harmony_content::bundle::MAX_BUNDLE_ENTRIES as u64)
        * (harmony_content::chunker::ChunkerConfig::DEFAULT.min_chunk as u64);

    let dir = tempfile::tempdir().expect("tempdir");
    // One survivor + one oversized sparse file. Survivor proves the walk
    // continues past the skip; oversized proves the skip is counted.
    std::fs::write(dir.path().join("ok.txt"), b"ok").expect("write ok");
    let big_path = dir.path().join("huge.bin");
    let big = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&big_path)
        .expect("open huge.bin");
    big.set_len(flat_bundle_max + 1)
        .expect("sparse set_len past FLAT_BUNDLE_MAX");
    drop(big);

    let h = fresh_harness();
    let result = run_walker(&h, dir.path().to_path_buf())
        .await
        .expect("ingest_folder_tree succeeds with an oversized file in the tree");

    assert_eq!(
        result.skipped.oversized, 1,
        "oversized file must increment the oversized skip bucket; got {}",
        result.skipped.oversized
    );
    assert_eq!(result.succeeded, 1, "ok.txt must still settle");

    let root_cid = result.root_cid.as_ref().expect("root_cid set");
    let entries = parse_root_manifest(&h.log, root_cid);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["ok.txt"],
        "oversized file name must NOT appear in the root manifest; got {names:?}"
    );
}

// ── Test 8: cancel mid-walk via a gated ingest handler ─────────────────────

/// Cancel mid-walk needs a determinism guarantee: the cancel flag must be
/// set *after* the walker has done at least some work but *before* the
/// root manifest gets built. A wall-clock sleep races CI scheduling. The
/// reliable seam is the ingest channel itself — we wedge the FIRST ingest
/// reply on a oneshot gate. While the walker is parked on `.await`-ing
/// that reply, we flip the cancel flag from the test body, then release
/// the gate so the walker proceeds, observes the flag at the next
/// recursion-entry check, and unwinds via `WalkOutcome::Cancelled`.
///
/// This is strictly stronger than the inline
/// `walker_honors_cancel_flag_and_cleans_up_registry` test, which only
/// pins the cancel-BEFORE-walk path; here the cancel is observed
/// mid-iteration.
#[tokio::test]
async fn cancel_mid_walk_settles_with_cancelled_true_and_no_root_sidecar() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 5 files is plenty: the gated handler stalls on file #1, so we have
    // 4 more children plus the root-manifest build still pending when the
    // cancel flag fires. The exact count > 1 is the only contract.
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("file_{i:02}.bin")), b"data")
            .expect("write fixture");
    }

    let h = fresh_harness();

    // Build a custom ingest handler that holds the FIRST reply until the
    // test releases it, but ACKs every subsequent request immediately. The
    // walker parks on the held reply -> we flip the cancel flag -> we
    // release -> walker resumes -> walker checks cancel at top of next
    // child iteration -> WalkOutcome::Cancelled -> root never builds.
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
    let (cancel_observed_tx, cancel_observed_rx) = tokio::sync::oneshot::channel::<()>();
    let (gated_tx, mut gated_rx) = tokio::sync::mpsc::channel::<IngestRequest>(128);

    tokio::spawn(async move {
        // First reply: hold until gate releases.
        let mut gate_rx = Some(gate_rx);
        let mut cancel_observed_tx = Some(cancel_observed_tx);
        while let Some(req) = gated_rx.recv().await {
            if let Some(rx) = gate_rx.take() {
                // Signal the test body that the walker has reached the
                // ingest channel (i.e., it's mid-walk and we can safely
                // flip the cancel flag with no race against the registry
                // insert).
                if let Some(tx) = cancel_observed_tx.take() {
                    let _ = tx.send(());
                }
                let _ = rx.await; // Park until the test releases the gate.
            }
            let _ = req.reply.send(Ok(()));
        }
    });

    // Swap the harness ingest_tx for the gated one. We don't strictly
    // need the original sender after this point.
    let mut h = h;
    h.ingest_tx = gated_tx;
    let registry = h.registry.clone();

    // Spawn the walker. It will reach the first ingest request, signal
    // us via `cancel_observed_rx`, then park.
    let dir_path = dir.path().to_path_buf();
    let app = h.app.clone();
    let ingest_tx = h.ingest_tx.clone();
    let verb_tx = h.verb_tx.clone();
    let index = h.index.clone();
    let walker = tokio::spawn(async move {
        folder_ingest::ingest_folder_tree(
            app,
            ingest_tx,
            verb_tx,
            index,
            registry,
            uuid::Uuid::new_v4().to_string(),
            dir_path.to_string_lossy().into_owned(),
            None,
            Vec::new(),
        )
        .await
    });

    // Wait for the walker to reach the gated ingest. Bound this with a
    // timeout so a regression that hangs the walker before any ingest
    // doesn't deadlock the test.
    tokio::time::timeout(Duration::from_secs(5), cancel_observed_rx)
        .await
        .expect("walker did not reach ingest channel within 5s")
        .expect("cancel_observed signal sender dropped");

    // Walker is parked on the first reply. Flip the cancel flag.
    let flags: Vec<Arc<AtomicBool>> = {
        let reg = h.registry.lock().expect("registry lock");
        reg.values().cloned().collect()
    };
    assert_eq!(
        flags.len(),
        1,
        "exactly one in-flight job registered; got {}",
        flags.len()
    );
    flags[0].store(true, Ordering::Relaxed);

    // Release the gate so the walker can resume and observe the flag.
    let _ = gate_tx.send(());

    let result = walker
        .await
        .expect("walker task panicked")
        .expect("ingest_folder_tree resolves even when cancelled mid-walk");

    assert!(
        result.cancelled,
        "result.cancelled must be true when the cancel flag fires mid-walk; got {result:?}"
    );
    assert!(
        result.root_sidecar_id.is_none(),
        "bail-completely semantics: cancel mid-walk leaves root_sidecar_id = None; got {:?}",
        result.root_sidecar_id
    );
    assert!(
        result.root_cid.is_none(),
        "root_cid must also be None when the root manifest never built"
    );
    // No root sidecar should land in the index either.
    let sidecar_count = h.index.lock().unwrap().entries().count();
    assert_eq!(
        sidecar_count, 0,
        "cancel mid-walk must not insert a partial sidecar; got {sidecar_count}"
    );
    // Registry must be empty after settle regardless of cancellation.
    assert!(
        h.registry.lock().unwrap().is_empty(),
        "registry entry must be stripped after settle (even on cancel)"
    );
}

// ── Test 9: per-leaf I/O error via chmod 000 (unix-only) ───────────────────

/// Drops read permissions on a single file so `tokio::fs::read` returns
/// `PermissionDenied`. The walker must record the failure against that
/// path, continue iterating, and still build the parent manifest with
/// surviving children.
///
/// Windows file ACLs and Rust's std `set_permissions` don't compose into a
/// reliable "block read for the file owner" recipe — the cross-platform
/// fallback (a missing-after-prewalk path) is also racey. Gating to unix
/// keeps the assertion deterministic; coverage of the failure-routing
/// arm itself is uniform across OSes.
#[cfg(unix)]
#[tokio::test]
async fn per_leaf_io_error_is_recorded_and_walk_continues() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("survivor.txt"), b"survive").expect("write survivor");
    let unreadable = dir.path().join("locked.bin");
    std::fs::write(&unreadable, b"forbidden").expect("write locked");
    // 0o000 — no read for owner, group, or others. The walker's
    // tokio::fs::read returns PermissionDenied here. Restore at end of
    // test so tempdir cleanup can run.
    std::fs::set_permissions(&unreadable, Permissions::from_mode(0o000))
        .expect("chmod 000 on locked.bin");

    let h = fresh_harness();
    let result = run_walker(&h, dir.path().to_path_buf())
        .await
        .expect("ingest_folder_tree succeeds despite a per-leaf I/O error");

    // Restore permissions before any assertion can panic so the tempdir
    // Drop can clean up the locked file. The leaked-tempdir pattern used
    // by `fresh_content_index` doesn't apply here — `dir` is the
    // walker root and is on the test's stack.
    std::fs::set_permissions(&unreadable, Permissions::from_mode(0o644))
        .expect("restore perms on locked.bin");

    assert_eq!(
        result.succeeded, 1,
        "survivor.txt must still settle even though locked.bin failed"
    );
    assert_eq!(
        result.failed.len(),
        1,
        "exactly one failed entry expected; got {} ({:?})",
        result.failed.len(),
        result.failed
    );
    assert!(
        result.failed[0].path.contains("locked.bin"),
        "failed entry path must point at the unreadable file; got {}",
        result.failed[0].path
    );
    assert!(!result.cancelled, "no cancellation in this scenario");

    let root_cid = result.root_cid.as_ref().expect("root_cid set");
    let entries = parse_root_manifest(&h.log, root_cid);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["survivor.txt"],
        "manifest must include surviving children and exclude the failed one; got {names:?}"
    );
}

// ── Test 10: pre-walk fails on a missing root path ─────────────────────────

#[tokio::test]
async fn missing_root_path_errors_without_inserting_sidecar() {
    let h = fresh_harness();
    let sidecar_count_before = h.index.lock().unwrap().entries().count();
    assert_eq!(
        sidecar_count_before, 0,
        "fresh content index must start empty for this assertion to be meaningful"
    );

    // Path that definitely doesn't exist. `ingest_folder_tree` stats the
    // root up-front before any registry insert / pre-walk / walker spawn,
    // and surfaces the failure as a clean `Err` for the IPC.
    let missing =
        PathBuf::from("/this/path/does/not/exist/zeb163-folder-walker-pre-walk-fail-marker");
    let err = folder_ingest::ingest_folder_tree(
        h.app.clone(),
        h.ingest_tx.clone(),
        h.verb_tx.clone(),
        h.index.clone(),
        h.registry.clone(),
        uuid::Uuid::new_v4().to_string(),
        missing.to_string_lossy().into_owned(),
        None,
        Vec::new(),
    )
    .await
    .expect_err("ingest_folder_tree must reject a missing root path");

    assert!(
        err.contains("root_path") || err.contains("stat"),
        "error message must mention the root_path / stat failure; got: {err}"
    );

    // No partial sidecar should land.
    let sidecar_count_after = h.index.lock().unwrap().entries().count();
    assert_eq!(
        sidecar_count_after, 0,
        "missing root must not leave a partial sidecar row; got {sidecar_count_after}"
    );
    // And the registry should not retain an orphan entry.
    assert!(
        h.registry.lock().unwrap().is_empty(),
        "missing-root rejection must happen before any registry insert; got {} entries",
        h.registry.lock().unwrap().len()
    );
}
