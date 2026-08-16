//! ZEB-299 integration tests for `rename_content`.
//!
//! Harness boilerplate (`spawn_test_runtime`, `ingest_folder`,
//! `ingest_leaf`, `make_leaf`, `insert_top_level`, `fresh_index`) lives in
//! the shared `content/harness.rs` module (ZEB-183 extraction).

use harmony_app::content_index::ContentKind;
use harmony_app::folders;

use crate::harness::{
    fresh_index, ingest_folder, ingest_leaf, insert_top_level, make_leaf, spawn_test_runtime,
};

// ── Test 1: rename_top_level_file ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_top_level_file() {
    // Top-level leaf row. Renaming via IPC must change `file_name` and
    // leave `cid` untouched — bundle bytes are not rebuilt.
    let (l_cid, l_bytes) = make_leaf(b"hello world");

    let harness = spawn_test_runtime("rename").await;
    ingest_leaf(&harness, l_cid, l_bytes.clone()).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(
        &index,
        l_cid,
        "hello.txt",
        ContentKind::Leaf,
        false,
        l_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        "hello.txt".to_string(),
        "world.txt".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename top-level file");

    assert_eq!(result.src_new_cid, None, "top-level rename never rekeys");

    let idx = index.lock().unwrap();
    let entry = idx.get(&l_sid).expect("L entry still present");
    assert_eq!(entry.file_name, "world.txt", "file_name updated");
    assert_eq!(entry.cid, l_cid, "cid unchanged");
}

// ── Test 2: rename_top_level_folder ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_top_level_folder() {
    // Top-level folder row with children — verify children unaffected,
    // i.e., the folder's own bundle CID is stable.
    let (l_cid, l_bytes) = make_leaf(b"inner");
    let f_old = folders::build_folder(
        "F",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build F");

    let harness = spawn_test_runtime("rename").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &f_old).await;

    let (index, _index_dir) = fresh_index();
    let f_sid = insert_top_level(
        &index,
        f_old.bundle_cid.to_bytes(),
        "Folder",
        ContentKind::Folder,
        false,
        f_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        f_sid.to_string(),
        vec![hex::encode(f_old.bundle_cid.to_bytes())],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "Folder".to_string(),
        "Renamed Folder".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename top-level folder");

    assert_eq!(result.src_new_cid, None);

    let idx = index.lock().unwrap();
    let entry = idx.get(&f_sid).expect("F entry still present");
    assert_eq!(entry.file_name, "Renamed Folder");
    assert_eq!(
        entry.cid,
        f_old.bundle_cid.to_bytes(),
        "folder bundle CID stable — children unaffected",
    );
}

// ── Test 3: rename_nested_one_level_deep ──────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_nested_one_level_deep() {
    // T contains F. Rename F → "bar". T rekeys to the manifest-with-
    // renamed-entry CID; F's own bundle CID is unchanged.
    let f_old = folders::build_folder("foo", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "foo".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime("rename").await;
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "foo".to_string(),
        "bar".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename nested one level");

    let t_new = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "bar".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T_new");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t_new.bundle_cid.to_bytes())),
    );

    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present");
    assert_eq!(
        entry.cid,
        t_new.bundle_cid.to_bytes(),
        "T rekeyed to renamed-entry manifest CID",
    );
}

// ── Test 4: rename_nested_two_levels_deep ─────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_nested_two_levels_deep() {
    // T → A → L. Rename L. Whole chain rekeys.
    let (l_cid, l_bytes) = make_leaf(b"two-deep");
    let a_old = folders::build_folder(
        "A",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build A");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: a_old.bundle_cid.to_bytes(),
            name: "A".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime("rename").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &a_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(a_old.bundle_cid.to_bytes()),
        ],
        hex::encode(l_cid),
        "L".to_string(),
        "Renamed".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename nested two-levels");

    let a_new = folders::build_folder(
        "A",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "Renamed".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build A_new");
    let t_new = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: a_new.bundle_cid.to_bytes(),
            name: "A".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T_new");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t_new.bundle_cid.to_bytes())),
    );

    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present");
    assert_eq!(entry.cid, t_new.bundle_cid.to_bytes(), "T rekeyed");
}

// ── Test 5: rename_disambiguates_siblings_with_shared_cid ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_disambiguates_siblings_with_shared_cid() {
    // T contains two empty folders EmptyA and EmptyB — same CID,
    // different names. Rename EmptyB → "Renamed". The (name, cid) match
    // must pick the right sibling.
    let empty = folders::build_folder("", &[]).expect("build empty");
    let empty_cid = empty.bundle_cid.to_bytes();
    let t_old = folders::build_folder(
        "T",
        &[
            folders::ManifestEntry {
                cid: empty_cid,
                name: "EmptyA".into(),
                kind: ContentKind::Folder,
            },
            folders::ManifestEntry {
                cid: empty_cid,
                name: "EmptyB".into(),
                kind: ContentKind::Folder,
            },
        ],
    )
    .expect("build T");

    let harness = spawn_test_runtime("rename").await;
    ingest_folder(&harness, &empty).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(empty_cid),
        "EmptyB".to_string(),
        "Renamed".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename shared-cid sibling");

    let t_new = folders::build_folder(
        "T",
        &[
            folders::ManifestEntry {
                cid: empty_cid,
                name: "EmptyA".into(),
                kind: ContentKind::Folder,
            },
            folders::ManifestEntry {
                cid: empty_cid,
                name: "Renamed".into(),
                kind: ContentKind::Folder,
            },
        ],
    )
    .expect("build T_new");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t_new.bundle_cid.to_bytes())),
        "rebuilt T must keep EmptyA and rename EmptyB → Renamed",
    );
}

// ── Test 6: rename_empty_name_rejected ────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_empty_name_rejected() {
    // Both "" and "   " (whitespace-only after trim) must reject.
    let (l_cid, l_bytes) = make_leaf(b"empty-reject");

    let harness = spawn_test_runtime("rename").await;
    ingest_leaf(&harness, l_cid, l_bytes.clone()).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(
        &index,
        l_cid,
        "original.txt",
        ContentKind::Leaf,
        false,
        l_bytes.len() as u64,
    );

    for empty_name in ["", "   "] {
        let err = harmony_app::rename_content_impl(
            l_sid.to_string(),
            vec![hex::encode(l_cid)],
            hex::encode(l_cid),
            "original.txt".to_string(),
            empty_name.to_string(),
            harness.ingest_tx.clone(),
            harness.verb_tx.clone(),
            index.clone(),
        )
        .await
        .expect_err("empty/whitespace name must reject");
        assert!(
            err.contains("name cannot be empty"),
            "got: {err} for input {empty_name:?}",
        );
    }

    // Sidecar unchanged.
    let idx = index.lock().unwrap();
    let entry = idx.get(&l_sid).expect("L entry still present");
    assert_eq!(entry.file_name, "original.txt");
}

// ── Test 7: rename_same_name_nested_no_op ─────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_same_name_nested_no_op() {
    // new_name == src_child_name in the nested case — returns the
    // current top-level CID, sidecar untouched, no new bytes ingested.
    let f_old = folders::build_folder("foo", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "foo".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime("rename").await;
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );
    let t_old_cid = t_old.bundle_cid.to_bytes();

    let result = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old_cid)],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "foo".to_string(),
        "foo".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("same-name nested no-op");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t_old_cid)),
        "same-name nested returns current top-level CID",
    );
    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present");
    assert_eq!(entry.cid, t_old_cid, "T sidecar CID unchanged");
}

// ── Test 8: rename_same_name_top_level_no_op ──────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_same_name_top_level_no_op() {
    let (l_cid, l_bytes) = make_leaf(b"same-name-top");

    let harness = spawn_test_runtime("rename").await;
    ingest_leaf(&harness, l_cid, l_bytes.clone()).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(
        &index,
        l_cid,
        "same.txt",
        ContentKind::Leaf,
        false,
        l_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        "same.txt".to_string(),
        "same.txt".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("same-name top-level no-op");

    assert_eq!(result.src_new_cid, None);
    let idx = index.lock().unwrap();
    let entry = idx.get(&l_sid).expect("L entry still present");
    assert_eq!(entry.file_name, "same.txt");
    assert_eq!(entry.cid, l_cid);
}

// ── Test 9: rename_duplicate_sibling_rejected_nested ──────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_duplicate_sibling_rejected_nested() {
    // T contains A and B. Rename A → "B" — rejects with name-collision.
    let a_old = folders::build_folder("A", &[]).expect("build A");
    let b_old = folders::build_folder("B_inner", &[]).expect("build B");
    let t_old = folders::build_folder(
        "T",
        &[
            folders::ManifestEntry {
                cid: a_old.bundle_cid.to_bytes(),
                name: "A".into(),
                kind: ContentKind::Folder,
            },
            folders::ManifestEntry {
                cid: b_old.bundle_cid.to_bytes(),
                name: "B".into(),
                kind: ContentKind::Folder,
            },
        ],
    )
    .expect("build T");

    let harness = spawn_test_runtime("rename").await;
    ingest_folder(&harness, &a_old).await;
    ingest_folder(&harness, &b_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );
    let t_old_cid = t_old.bundle_cid.to_bytes();

    let err = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old_cid)],
        hex::encode(a_old.bundle_cid.to_bytes()),
        "A".to_string(),
        "B".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("duplicate sibling must reject");
    assert!(
        err.contains("parent folder already has an entry named 'B'"),
        "got: {err}",
    );

    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present");
    assert_eq!(entry.cid, t_old_cid, "T unchanged after rejected rename");
}

// ── Test 10: rename_duplicate_sibling_rejected_top_level ──────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_duplicate_sibling_rejected_top_level() {
    let (a_cid, a_bytes) = make_leaf(b"alpha");
    let (b_cid, b_bytes) = make_leaf(b"beta");

    let harness = spawn_test_runtime("rename").await;
    ingest_leaf(&harness, a_cid, a_bytes.clone()).await;
    ingest_leaf(&harness, b_cid, b_bytes.clone()).await;

    let (index, _index_dir) = fresh_index();
    let a_sid = insert_top_level(
        &index,
        a_cid,
        "A",
        ContentKind::Leaf,
        false,
        a_bytes.len() as u64,
    );
    let _b_sid = insert_top_level(
        &index,
        b_cid,
        "B",
        ContentKind::Leaf,
        false,
        b_bytes.len() as u64,
    );

    let err = harmony_app::rename_content_impl(
        a_sid.to_string(),
        vec![hex::encode(a_cid)],
        hex::encode(a_cid),
        "A".to_string(),
        "B".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("duplicate top-level sibling must reject");
    assert!(
        err.contains("a top-level entry named 'B' already exists"),
        "got: {err}",
    );

    let idx = index.lock().unwrap();
    let entry = idx.get(&a_sid).expect("A entry still present");
    assert_eq!(entry.file_name, "A", "A name unchanged after rejection");
}

// ── Test 11: rename_name_mismatch_rejected ────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_name_mismatch_rejected() {
    // Top-level: src_child_name does not match the sidecar entry name.
    let (l_cid, l_bytes) = make_leaf(b"mismatch-top");

    let harness = spawn_test_runtime("rename").await;
    ingest_leaf(&harness, l_cid, l_bytes.clone()).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(
        &index,
        l_cid,
        "actual.txt",
        ContentKind::Leaf,
        false,
        l_bytes.len() as u64,
    );

    let err = harmony_app::rename_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        "wrong.txt".to_string(),
        "renamed.txt".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("top-level name mismatch must reject");
    assert!(
        err.contains(
            "src_child_name 'wrong.txt' does not match src sidecar entry name 'actual.txt'"
        ),
        "got: {err}",
    );

    // Nested: src_child_name does not match the manifest entry name.
    let f_old = folders::build_folder("actual", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "actual".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    let err = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "wrong".to_string(),
        "renamed".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("nested name mismatch must reject");
    assert!(
        err.contains("has no entry named 'wrong' pointing to child"),
        "got: {err}",
    );
}

// ── Test 12: rename_concurrent_rekey_conflict ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_concurrent_rekey_conflict() {
    // Arm the rekey-conflict hook on the cascade root. IPC must surface
    // "concurrent rekey on src_sidecar_id" and the sidecar CID must
    // stay unchanged.
    let f_old = folders::build_folder("foo", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "foo".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime("rename").await;
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );
    let t_old_cid = t_old.bundle_cid.to_bytes();

    {
        let mut idx = index.lock().unwrap();
        idx.arm_next_rekey_conflict(t_sid, [0xAB; 32]);
    }

    let err = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old_cid)],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "foo".to_string(),
        "bar".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("armed conflict hook must surface as Err");
    assert!(
        err.contains("concurrent rekey on src_sidecar_id"),
        "got: {err}",
    );

    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present");
    assert_eq!(
        entry.cid, t_old_cid,
        "T sidecar CID unchanged after rejected rename",
    );
}

// ── Test 13: rename_nested_preserves_pinned_status ───────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_nested_preserves_pinned_status() {
    // Top-level T (pinned) contains F. Renaming F rekeys T's sidecar.
    // The rekey path must preserve the `pinned` flag — losing it would
    // silently demote pinned content to evictable on every rename.
    let f_old = folders::build_folder("foo", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "foo".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime("rename").await;
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        true, // pinned
        t_old.bundle_bytes.len() as u64,
    );

    harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "foo".to_string(),
        "bar".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename nested preserves pinned");

    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present after rekey");
    assert!(
        entry.pinned,
        "pinned flag must survive the rekey — sidecar rekey is not a re-insert",
    );
}
