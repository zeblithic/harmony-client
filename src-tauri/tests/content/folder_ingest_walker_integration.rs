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
//!   empty subdir, deny-list, symlinks, mid-walk cancel, per-leaf I/O
//!   failure, and pre-walk failure on a missing root.
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
    let idx = ContentIndex::load(
        Some(&harmony_app::device_dataset_file::test_cipher()),
        dir.path(),
    );
    // Persist directory is examined only via the in-memory entries() iterator
    // here; leaking keeps the directory alive without obscuring the test body.
    std::mem::forget(dir);
    Arc::new(Mutex::new(idx))
}

/// Build a walk-root tempdir with a NON-dot prefix.
///
/// `tempfile::tempdir()` defaults to a `.tmp<rand>` basename, which the walker
/// rejects at the root guard (`should_filter_name` denies any `.`-prefixed
/// name) with "Cannot ingest hidden/system folder as root" — so the walk sees
/// nothing and every assertion below fails (ZEB-332 / ZEB-306). Production
/// roots are user-chosen and never dot-prefixed; this is purely a
/// test-scaffolding concern. Mirrors `folder_ingest::tests::walk_root_tempdir`.
fn walk_root_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ingest-test")
        .tempdir()
        .expect("tempdir")
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
    let dir = walk_root_tempdir();
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
    assert_eq!(
        result.skipped.other, 0,
        "no FIFOs/sockets/devices in fixture"
    );
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
    let dir = walk_root_tempdir();
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
    let dir = walk_root_tempdir();
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
    let dir = walk_root_tempdir();
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

    let dir = walk_root_tempdir();
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

    let dir = walk_root_tempdir();
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

// ── Test 7: cancel mid-walk via a gated ingest handler ─────────────────────

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
    let dir = walk_root_tempdir();
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

// ── Test 8: per-leaf I/O error via chmod 000 (unix-only) ───────────────────

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

    let dir = walk_root_tempdir();
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

// ── Test 9a: walk-fails-at-root surfaces the message in `failed` ──────────

/// Round-4 bot fix regression: when the root walk itself fails (e.g.
/// `create_folder_with_children` errors on the root's manifest send),
/// the failure message used to be silently dropped — `result.failed`
/// stayed empty, `root_sidecar_id` was None, `cancelled` was false, and
/// the frontend rendered a generic "failed before completing" headline
/// with zero diagnostic info. The walker now pushes the message into
/// `result.failed` so the summary modal's Failed section renders the
/// real error.
///
/// We drive the root-fail by closing the ingest_tx receiver before the
/// walker starts: the very first send for the root's manifest book
/// returns Err("event loop not running"), bubbling up as a
/// `WalkOutcome::Failed` from the root call.
#[tokio::test]
async fn root_walk_failure_message_is_surfaced_in_failed_list() {
    // Custom harness: live sender, dropped receiver. The walker's
    // pre-walk uses sync `std::fs::*` (no channel), so it completes; the
    // root manifest send is the first thing that hits the closed channel.
    let app = tauri::test::mock_app();
    let app_handle = app.handle().clone();
    let (ingest_tx, ingest_rx) = tokio::sync::mpsc::channel::<IngestRequest>(8);
    drop(ingest_rx);
    let (verb_tx, _verb_rx) = tokio::sync::mpsc::channel::<ContentVerbRequest>(8);
    let index = fresh_content_index();
    let registry: CancelRegistry = Arc::new(Mutex::new(Default::default()));

    let dir = walk_root_tempdir();
    // Empty root — the only ingest sends are for the root manifest
    // book + bundle. Both fail at the closed channel.
    let result = folder_ingest::ingest_folder_tree(
        app_handle,
        ingest_tx,
        verb_tx,
        index.clone(),
        registry,
        uuid::Uuid::new_v4().to_string(),
        dir.path().to_string_lossy().into_owned(),
        None,
        Vec::new(),
    )
    .await
    .expect("ingest_folder_tree must return Ok even when the root walk fails");

    assert!(
        result.root_sidecar_id.is_none(),
        "root_sidecar_id must be None when the root walk fails; got {:?}",
        result.root_sidecar_id
    );
    assert!(
        !result.cancelled,
        "root failure must NOT flip the cancelled flag — that's reserved for explicit cancel"
    );
    assert_eq!(
        result.failed.len(),
        1,
        "the root failure message must be surfaced as exactly one entry in `failed`; \
         got {} entries ({:?})",
        result.failed.len(),
        result.failed
    );
    let entry = &result.failed[0];
    assert_eq!(
        entry.path,
        dir.path().to_string_lossy(),
        "the failed entry's path must point at the root that failed; got {}",
        entry.path
    );
    assert!(
        !entry.message.is_empty(),
        "the failed entry's message must carry diagnostic info; got empty string"
    );

    // Sidecar count is unchanged — the root manifest never landed.
    assert_eq!(
        index.lock().unwrap().entries().count(),
        0,
        "root manifest failure must not leave a partial sidecar"
    );
}

// ── Test 9: pre-walk fails on a missing root path ─────────────────────────

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

// ── Test 10: depth-2+ nested-bundle round-trip (small-chunker fixture) ──────
//
// ZEB-161 Task 5: end-to-end pin for the streaming + tree-build path. The
// `streaming_ingest_tests` block inside `src/lib.rs` exercises
// `build_bundle_tree` against synthetic leaf CIDs (no chunker, no I/O); the
// `chunked_ingest_pin_cascade_fetch_burn_roundtrip` integration test pins
// the *depth-1* shape on a small in-memory buffer. Neither covers the
// depth-2+ tree that streaming_ingest must build when the chunk count
// exceeds `MAX_BUNDLE_ENTRIES` (= 32_767 at MAX_PAYLOAD_SIZE / CID_SIZE).
//
// ZEB-397: depth-2 is driven by leaf COUNT (> MAX_BUNDLE_ENTRIES), not by
// byte volume. The chunk count on a sparse-zero stream is
// `ceil(size / max_chunk)`: zeros never satisfy the FastCDC gear-hash mask
// (the rolling hash over a constant byte converges to a fixed non-zero
// residue, so no content boundary ever fires), forcing every cut at
// `max_chunk`. So rather than reach >32_767 leaves the brute-force way
// (36 GiB ÷ 1 MiB `ChunkerConfig::DEFAULT` chunks ≈ 36_864 leaves — ~17 min
// of FastCDC hashing that made this the per-PR CI long pole, ZEB-397), we
// shrink `max_chunk` to 4 KiB and reach the same >32_767 leaves with a
// ~140 MiB sparse fixture that hashes in well under a second. The tree-build
// path exercised is byte-for-byte identical — only the chunk size differs.
//
// The fixture is a sparse tempfile (`set_len` creates a hole; ≈0 real disk
// on ext4/APFS/NTFS/ZFS, read back as a continuous run of zero bytes). No
// HARMONY_LARGE_TESTS gate and no dedicated CI job any more — at ~140 MiB
// this runs in the normal `cargo nextest run` flow on every PR.
//
// Captured-channel pattern: we route `streaming_ingest`'s ingest sends into
// an mpsc the test owns, recording every (CID, bytes) pair on the way past.
// That gives us:
//   - the root CID (returned by streaming_ingest) — used for the depth
//     assertion and to find the root bundle bytes in the captured log;
//   - the leaf count (CIDs whose `cid_type() == CidType::Book`);
//   - the root bundle's inline-metadata sentinel, parsed via
//     `parse_inline_metadata` to confirm the size/chunk-count round-trip.
//
// No NodeRuntime is needed — that's covered by
// `chunked_ingest_pin_cascade_fetch_burn_roundtrip` for the depth-1 case.
// Standing doctrine (see plan): don't try to expose `walk_recursive` from
// `harmony_content` cross-crate; the captured channel is the right seam.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_bundle_tree_round_trip() {
    use harmony_content::bundle::parse_bundle;
    use harmony_content::chunker::ChunkerConfig;
    use harmony_content::cid::{CidType, ContentId};

    // ZEB-397: a deliberately tiny chunker so a ~140 MiB sparse fixture
    // produces > MAX_BUNDLE_ENTRIES (32_767) leaves and forces depth-2.
    // Valid per ChunkerConfig::validate (min < avg < max ≤ MAX_PAYLOAD_SIZE;
    // avg a power of two). On zero input every cut is forced at max_chunk
    // (4 KiB), so leaf count ≈ EXPECTED_SIZE / 4 KiB.
    const TINY_CHUNKER: ChunkerConfig = ChunkerConfig {
        min_chunk: 1024,
        avg_chunk: 2048,
        max_chunk: 4096,
    };

    // 36_000 forced max_chunk cuts + 1 tail byte → 36_001 leaves: comfortably
    // above MAX_BUNDLE_ENTRIES (32_767, proving depth-2) and below the 50_000
    // regression ceiling. The +1 also keeps total_size off a clean boundary —
    // an off-by-one canary for the chunker's tail-byte accounting.
    const EXPECTED_SIZE: u64 = 36_000 * 4096 + 1;

    // Sparse tempfile. `set_len` on modern filesystems (ext4, APFS, NTFS, ZFS)
    // creates a hole rather than allocating zeros; the file *appears* as
    // EXPECTED_SIZE but consumes ~0 real bytes until written. The chunker
    // reads it as a continuous run of zero bytes via tokio::fs::File.
    let tmp_dir = tempfile::tempdir().expect("tempdir for sparse fixture");
    let path = tmp_dir.path().join("sparse_zero_fixture.bin");
    {
        let file = std::fs::File::create(&path).expect("create sparse fixture");
        file.set_len(EXPECTED_SIZE).expect("set_len sparse fixture");
        // Drop closes the handle before the async reader opens it.
    }

    // Captured-channel pattern (mirrors `spawn_ingest_drain` in
    // `streaming_ingest_tests` and the forwarder in
    // `chunked_ingest_pin_cascade_fetch_burn_roundtrip`): drive
    // `streaming_ingest` through a channel we own.
    //
    // Memory note: recording every (cid_hex, bytes) pair would retain
    // ~O(file_size) bytes (every 4 KiB leaf cloned into the captured Vec).
    // The drain below counts leaves and discards their bytes (only the CID
    // type matters), and stores bundle payloads in a CID-keyed map so the
    // root bundle's bytes can be parsed for the inline-metadata assertion.
    // Total retained RAM is bounded by leaf-COUNT × 0 + bundle-count ×
    // bundle-size (~MAX_BUNDLE_ENTRIES × 32 B per bundle), well under a MiB.
    struct DrainOutput {
        leaf_count: usize,
        /// CID hex → bundle payload bytes. Only Bundle CIDs are stored;
        /// Book (leaf) payloads are counted via `leaf_count` and the
        /// bytes are discarded immediately.
        bundle_payloads: std::collections::HashMap<String, Vec<u8>>,
    }

    let (capture_tx, mut capture_rx) =
        tokio::sync::mpsc::channel::<harmony_app::event_loop::IngestRequest>(1024);
    let drain: tokio::task::JoinHandle<DrainOutput> = tokio::spawn(async move {
        let mut leaf_count = 0usize;
        let mut bundle_payloads: std::collections::HashMap<String, Vec<u8>> = Default::default();
        while let Some(req) = capture_rx.recv().await {
            let raw = hex::decode(&req.cid_hex).expect("captured cid_hex must decode");
            let arr: [u8; 32] = raw.as_slice().try_into().expect("32-byte CID");
            match ContentId::from_bytes(arr).cid_type() {
                CidType::Book => {
                    // Discard the bytes — only the count matters for the
                    // leaf assertion below, and retaining them would push
                    // the test into multi-GiB RSS.
                    leaf_count += 1;
                }
                _ => {
                    // Bundles (and any other non-Book CIDs) are tiny — keep
                    // the bytes so the root's inline-metadata sentinel can
                    // be parsed.
                    bundle_payloads.insert(req.cid_hex.clone(), req.data.clone());
                }
            }
            // Ack immediately so streaming_ingest's send_ingest helper sees
            // a fast Ok — the runtime's storage tier is not involved here.
            let _ = req.reply.send(Ok(()));
        }
        DrainOutput {
            leaf_count,
            bundle_payloads,
        }
    });

    let start = std::time::Instant::now();
    let reader = tokio::fs::File::open(&path)
        .await
        .expect("open sparse fixture for async read");
    let (root, _total_bytes) =
        harmony_app::streaming_ingest(reader, &capture_tx, TINY_CHUNKER, None)
            .await
            .expect("streaming_ingest must succeed on the sparse fixture");
    drop(capture_tx);
    let captured = drain.await.expect("capture drain joins cleanly");
    let elapsed = start.elapsed();

    // Smoke-test bound: ingest of a ~140 MiB sparse-zero fixture is fast
    // (zeros hash cheaply; the cost is ~36 k chunk sends through the channel).
    // Slow shared CI hosts can still exceed 60 s — warn rather than fail.
    if elapsed.as_secs() > 60 {
        eprintln!("WARNING: nested_bundle_tree_round_trip ingest took {elapsed:?} (>60s)");
    }

    // ── Depth assertion: depth-2 or deeper ─────────────────────────────
    match root.cid_type() {
        CidType::Bundle(depth) => {
            assert!(
                depth >= 2,
                "expected Bundle(>=2) (leaf count must exceed MAX_BUNDLE_ENTRIES), got Bundle({depth})"
            );
        }
        other => panic!("expected Bundle CID for multi-chunk input, got {other:?}"),
    }

    // ── Leaf-count assertion: > 32_767, < 50_000 ──────────────────────
    // At TINY_CHUNKER on sparse zeros the chunker forces max_chunk (4 KiB)
    // cuts: EXPECTED_SIZE / 4 KiB ≈ 36_001 leaves. The lower bound > 32_767
    // is the only value that strictly proves depth-2 was forced by leaf
    // count exceeding MAX_BUNDLE_ENTRIES. Upper bound < 50_000 catches a
    // regression where cuts shrink toward min_chunk (e.g. 1 KiB cuts would
    // yield ~140_000 leaves — still depth-2 but ~4× the expected count).
    let leaf_count = captured.leaf_count;
    assert!(
        leaf_count > 32_767 && leaf_count < 50_000,
        "expected leaf count in (32_767, 50_000) for the sparse fixture, got {leaf_count}"
    );

    // ── Inline-metadata round-trip on the root bundle ───────────────────
    // The captured bundle map has the root's bytes — look it up by CID hex
    // and parse it, then check that the first child CID is the InlineData
    // sentinel with the original byte count + chunk count.
    let root_hex = hex::encode(root.to_bytes());
    let root_bytes = captured
        .bundle_payloads
        .get(&root_hex)
        .expect("root bundle bytes must appear in the captured stream");
    let entries = parse_bundle(root_bytes).expect("root bundle bytes parse");
    assert!(
        !entries.is_empty(),
        "root bundle must have at least the metadata sentinel + one child"
    );
    assert_eq!(
        entries[0].cid_type(),
        CidType::InlineData,
        "root bundle's first entry must be the inline-metadata sentinel"
    );
    let (total_size, chunk_count, _ts, _mime) = entries[0]
        .parse_inline_metadata()
        .expect("first entry parses as inline metadata");
    assert_eq!(
        total_size, EXPECTED_SIZE,
        "inline metadata's total_size must match the sparse file's set_len"
    );
    assert!(
        chunk_count > 32_767 && chunk_count < 50_000,
        "inline metadata's chunk_count must be in (32_767, 50_000); got {chunk_count}"
    );
    // Chunk count in the metadata should match the count of Book leaves we
    // observed on the captured channel — this is the round-trip that pins
    // streaming_ingest's bookkeeping to the build_bundle_tree metadata.
    assert_eq!(
        chunk_count as usize, leaf_count,
        "inline metadata's chunk_count must equal the number of Book leaves captured \
         (metadata = {chunk_count}, captured leaves = {leaf_count})"
    );
}
