//! ZEB-162 integration tests for `move_content`.
//!
//! The runtime harness lives in the shared `content/harness.rs` module
//! (ZEB-183 extraction); each test seeds a folder tree through the runtime's ingest
//! channel, mutates the sidecar via `ContentIndex`, then drives
//! `move_content_impl` against the live runtime and verifies the
//! resulting sidecar + cache state.

use tokio::sync::oneshot;

use harmony_app::content_index::{ContentKind, SidecarId};
use harmony_app::event_loop::ContentVerbRequest;
use harmony_app::folders;
use harmony_content::cid::{ContentFlags, ContentId};

use crate::harness::{
    fresh_index, ingest_folder, ingest_leaf, insert_top_level, make_leaf, spawn_test_runtime,
};

// ── Test 6: move_a_within_same_top_level_one_level_deep ───────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_a_within_same_top_level_one_level_deep() {
    // T contains A and B. A contains leaf L. Move L into B.
    let (l_cid, l_bytes) = make_leaf(b"hello");
    let a_old = folders::build_folder(
        "A",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build A");
    let b_old = folders::build_folder("B", &[]).expect("build B");
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

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
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

    let result = harmony_app::move_content_impl(
        t_sid.to_string(),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(a_old.bundle_cid.to_bytes()),
        ],
        hex::encode(l_cid),
        "L".to_string(),
        Some(t_sid.to_string()),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(b_old.bundle_cid.to_bytes()),
        ],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("move A");

    // Expected new tree: A empty; B containing L; T rekeyed.
    let a_new = folders::build_folder("A", &[]).expect("build A_new");
    let b_new = folders::build_folder(
        "B",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build B_new");
    let t_new = folders::build_folder(
        "T",
        &[
            folders::ManifestEntry {
                cid: a_new.bundle_cid.to_bytes(),
                name: "A".into(),
                kind: ContentKind::Folder,
            },
            folders::ManifestEntry {
                cid: b_new.bundle_cid.to_bytes(),
                name: "B".into(),
                kind: ContentKind::Folder,
            },
        ],
    )
    .expect("build T_new");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t_new.bundle_cid.to_bytes()))
    );
    assert_eq!(result.dst_new_cid, hex::encode(t_new.bundle_cid.to_bytes()));
    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still there");
    assert_eq!(entry.cid, t_new.bundle_cid.to_bytes(), "T rekeyed");
}

// ── Test 7: move_b_across_top_levels ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_b_across_top_levels() {
    let (l_cid, l_bytes) = make_leaf(b"alpha");
    let t1_old = folders::build_folder(
        "T1",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T1");
    let t2_old = folders::build_folder("T2", &[]).expect("build T2");

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &t1_old).await;
    ingest_folder(&harness, &t2_old).await;

    let (index, _index_dir) = fresh_index();
    let t1_sid = insert_top_level(
        &index,
        t1_old.bundle_cid.to_bytes(),
        "T1",
        ContentKind::Folder,
        false,
        t1_old.bundle_bytes.len() as u64,
    );
    let t2_sid = insert_top_level(
        &index,
        t2_old.bundle_cid.to_bytes(),
        "T2",
        ContentKind::Folder,
        false,
        t2_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::move_content_impl(
        t1_sid.to_string(),
        vec![hex::encode(t1_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        "L".to_string(),
        Some(t2_sid.to_string()),
        vec![hex::encode(t2_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("Case B");

    let t1_new = folders::build_folder("T1", &[]).expect("build T1_new");
    let t2_new = folders::build_folder(
        "T2",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T2_new");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t1_new.bundle_cid.to_bytes()))
    );
    assert_eq!(
        result.dst_new_cid,
        hex::encode(t2_new.bundle_cid.to_bytes())
    );

    let idx = index.lock().unwrap();
    assert_eq!(
        idx.get(&t1_sid).unwrap().cid,
        t1_new.bundle_cid.to_bytes(),
        "T1 rekeyed"
    );
    assert_eq!(
        idx.get(&t2_sid).unwrap().cid,
        t2_new.bundle_cid.to_bytes(),
        "T2 rekeyed"
    );
}

// ── Test 8: move_c_root_to_nested ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_c_root_to_nested() {
    // Top-level leaf L; top-level folder F (empty). Move L into F.
    let (l_cid, l_bytes) = make_leaf(b"root-leaf");
    let f_old = folders::build_folder("F", &[]).expect("build F");

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes.clone()).await;
    ingest_folder(&harness, &f_old).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(
        &index,
        l_cid,
        "L",
        ContentKind::Leaf,
        false,
        l_bytes.len() as u64,
    );
    let f_sid = insert_top_level(
        &index,
        f_old.bundle_cid.to_bytes(),
        "F",
        ContentKind::Folder,
        false,
        f_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::move_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        "L".to_string(),
        Some(f_sid.to_string()),
        vec![hex::encode(f_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("Case C");

    let f_new = folders::build_folder(
        "F",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build F_new");

    assert_eq!(result.src_new_cid, None);
    assert_eq!(result.dst_new_cid, hex::encode(f_new.bundle_cid.to_bytes()));

    let idx = index.lock().unwrap();
    assert!(idx.get(&l_sid).is_none(), "L sidecar entry removed");
    assert_eq!(
        idx.get(&f_sid).unwrap().cid,
        f_new.bundle_cid.to_bytes(),
        "F rekeyed"
    );
}

// ── Test 9: move_d_nested_to_root ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_d_nested_to_root() {
    let (l_cid, l_bytes) = make_leaf(b"to-root");
    let f_old = folders::build_folder(
        "F",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build F");

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &f_old).await;

    let (index, _index_dir) = fresh_index();
    let f_sid = insert_top_level(
        &index,
        f_old.bundle_cid.to_bytes(),
        "F",
        ContentKind::Folder,
        false,
        f_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::move_content_impl(
        f_sid.to_string(),
        vec![hex::encode(f_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        "L".to_string(),
        None,
        vec![],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("Case D");

    let f_new = folders::build_folder("F", &[]).expect("build F_new");
    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(f_new.bundle_cid.to_bytes()))
    );
    assert_eq!(result.dst_new_cid, hex::encode(l_cid));

    let idx = index.lock().unwrap();
    assert_eq!(
        idx.get(&f_sid).unwrap().cid,
        f_new.bundle_cid.to_bytes(),
        "F rekeyed to empty"
    );
    let new_top_sid = SidecarId::parse_str(&result.dst_sidecar_id).expect("parse");
    let new_top = idx.get(&new_top_sid).expect("new top entry");
    assert_eq!(new_top.cid, l_cid);
    assert_eq!(new_top.kind, ContentKind::Leaf);
    assert_eq!(new_top.file_name, "L");
    assert!(!new_top.pinned, "Case D defaults to unpinned");
    assert!(
        new_top.origin.is_none(),
        "Case D mints without provenance — manifests carry no origin, \
         and inferring SelfIngest would violate the honesty rule (ZEB-669 S4)"
    );
}

// ── Test 10: move_b_dst_rekey_conflict_compensating_undo_reverts ──────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_b_dst_rekey_conflict_at_stage_1_no_undo_path() {
    // Pre-rekey dst BEFORE the move (simulating "concurrent rekey
    // landed first"). STAGE 1 fails on CAS conflict; no src mutation
    // is performed.
    let (l_cid, l_bytes) = make_leaf(b"concurrent");
    let t1_old = folders::build_folder(
        "T1",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T1");
    let t2_old = folders::build_folder("T2", &[]).expect("build T2");

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &t1_old).await;
    ingest_folder(&harness, &t2_old).await;

    let (index, _index_dir) = fresh_index();
    let t1_sid = insert_top_level(
        &index,
        t1_old.bundle_cid.to_bytes(),
        "T1",
        ContentKind::Folder,
        false,
        t1_old.bundle_bytes.len() as u64,
    );
    let t2_sid = insert_top_level(
        &index,
        t2_old.bundle_cid.to_bytes(),
        "T2",
        ContentKind::Folder,
        false,
        t2_old.bundle_bytes.len() as u64,
    );

    // Arm a one-shot conflict for T2's STAGE 1 rekey. The armed hook
    // leaves the entry's CID untouched so the boundary verify still
    // passes — STAGE 1 is the first guard to trip, exercising the path
    // this test names.
    {
        let mut idx = index.lock().unwrap();
        idx.arm_next_rekey_conflict(t2_sid, [0xEE; 32]);
    }

    let err = harmony_app::move_content_impl(
        t1_sid.to_string(),
        vec![hex::encode(t1_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        "L".to_string(),
        Some(t2_sid.to_string()),
        vec![hex::encode(t2_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("dst CAS conflict must surface as Err");

    assert!(
        err.contains("concurrent rekey on dst_sidecar_id"),
        "got: {err}"
    );

    // T1 must be untouched (still at its original CID).
    let idx = index.lock().unwrap();
    assert_eq!(
        idx.get(&t1_sid).unwrap().cid,
        t1_old.bundle_cid.to_bytes(),
        "T1 untouched by failed move"
    );
}

// ── Test 11: move_b_src_rekey_conflict_after_dst_commit_undo_reverts_dst ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_b_src_rekey_conflict_after_dst_commit_undo_reverts_dst() {
    // Deterministically exercise the post-STAGE-1 / STAGE-2-conflict
    // path by arming the test-only rekey conflict hook on src's
    // sidecar. The hook fires when move_case_b reaches its STAGE 2
    // rekey on src — STAGE 1 has already committed by then — and
    // returns Conflict, triggering the compensating undo that reverts
    // T2 to its pre-STAGE-1 CID.
    let (l_cid, l_bytes) = make_leaf(b"undo-test");
    let t1_old = folders::build_folder(
        "T1",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T1");
    let t2_old = folders::build_folder("T2", &[]).expect("build T2");

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &t1_old).await;
    ingest_folder(&harness, &t2_old).await;

    let (index, _index_dir) = fresh_index();
    let t1_sid = insert_top_level(
        &index,
        t1_old.bundle_cid.to_bytes(),
        "T1",
        ContentKind::Folder,
        false,
        t1_old.bundle_bytes.len() as u64,
    );
    let t2_sid = insert_top_level(
        &index,
        t2_old.bundle_cid.to_bytes(),
        "T2",
        ContentKind::Folder,
        false,
        t2_old.bundle_bytes.len() as u64,
    );
    let t2_old_cid = t2_old.bundle_cid.to_bytes();

    // Arm the conflict hook on src's sidecar. The boundary verify
    // doesn't call rekey, so it's unaffected; STAGE 1 (dst rekey) also
    // unaffected; STAGE 2 (src rekey) is the FIRST rekey targeting
    // t1_sid in this call and consumes the hook.
    {
        let mut idx = index.lock().unwrap();
        idx.arm_next_rekey_conflict(t1_sid, [0xAB; 32]);
    }

    let err = harmony_app::move_content_impl(
        t1_sid.to_string(),
        vec![hex::encode(t1_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        "L".to_string(),
        Some(t2_sid.to_string()),
        vec![hex::encode(t2_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("STAGE 2 src rekey must fail under armed conflict hook");

    assert!(
        err.contains("concurrent rekey on src"),
        "expected post-STAGE-1 src-conflict path, got: {err}"
    );

    let idx = index.lock().unwrap();
    // T2 was forward-rekeyed at STAGE 1 then compensating-undone after
    // STAGE 2 failed. End state: T2 back at its pre-move CID.
    assert_eq!(
        idx.get(&t2_sid).unwrap().cid,
        t2_old_cid,
        "T2 reverted by compensating undo"
    );
}

// ── Test 12: move_cycle_rejected ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_cycle_rejected() {
    // Top-level folder T containing folder F. Attempt to move T into F.
    let f_old = folders::build_folder("F", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "F".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime("move").await;
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

    // Move T (root) into F — F is a descendant of T, so this is the
    // canonical cycle case. Source path = [T] (T is itself top-level),
    // src_child_cid = T's CID. Destination = inside T, path = [T, F].
    // That maps to Case C (root → nested).
    let err = harmony_app::move_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(t_old.bundle_cid.to_bytes()),
        "T".to_string(),
        Some(t_sid.to_string()),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(f_old.bundle_cid.to_bytes()),
        ],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("cycle must be rejected");
    assert!(
        err.contains("cannot move folder into its own descendant"),
        "got: {err}"
    );
}

// ── Test 13: move_no_op_rejected ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_no_op_rejected() {
    let (l_cid, l_bytes) = make_leaf(b"noop");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
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

    let err = harmony_app::move_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        "L".to_string(),
        Some(t_sid.to_string()),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("no-op must be rejected");
    assert!(
        err.contains("source and destination are identical"),
        "got: {err}"
    );
}

// ── Test 14: move_name_collision_rejected ─────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_name_collision_rejected() {
    // T1 has leaf foo.txt (cid_a). T2 has leaf foo.txt (cid_b) too.
    // Move cid_a from T1 → T2; destination already has an entry named
    // "foo.txt", so reject.
    let (cid_a, bytes_a) = make_leaf(b"a-bytes");
    let (cid_b, bytes_b) = make_leaf(b"b-bytes");
    assert_ne!(cid_a, cid_b);
    let t1_old = folders::build_folder(
        "T1",
        &[folders::ManifestEntry {
            cid: cid_a,
            name: "foo.txt".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T1");
    let t2_old = folders::build_folder(
        "T2",
        &[folders::ManifestEntry {
            cid: cid_b,
            name: "foo.txt".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T2");

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, cid_a, bytes_a).await;
    ingest_leaf(&harness, cid_b, bytes_b).await;
    ingest_folder(&harness, &t1_old).await;
    ingest_folder(&harness, &t2_old).await;

    let (index, _index_dir) = fresh_index();
    let t1_sid = insert_top_level(
        &index,
        t1_old.bundle_cid.to_bytes(),
        "T1",
        ContentKind::Folder,
        false,
        t1_old.bundle_bytes.len() as u64,
    );
    let t2_sid = insert_top_level(
        &index,
        t2_old.bundle_cid.to_bytes(),
        "T2",
        ContentKind::Folder,
        false,
        t2_old.bundle_bytes.len() as u64,
    );

    let err = harmony_app::move_content_impl(
        t1_sid.to_string(),
        vec![hex::encode(t1_old.bundle_cid.to_bytes())],
        hex::encode(cid_a),
        "foo.txt".to_string(),
        Some(t2_sid.to_string()),
        vec![hex::encode(t2_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("name collision must be rejected");
    assert!(
        err.contains("destination already has an entry named 'foo.txt'"),
        "got: {err}"
    );

    // Both top-level entries remain at their original CIDs — boundary
    // rejection means no ingests fired.
    let idx = index.lock().unwrap();
    assert_eq!(idx.get(&t1_sid).unwrap().cid, t1_old.bundle_cid.to_bytes());
    assert_eq!(idx.get(&t2_sid).unwrap().cid, t2_old.bundle_cid.to_bytes());
}

// ── Test 15: move_pin_cascade_a_within_same_root ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_pin_cascade_a_within_same_root() {
    // T pinned, A contains L, B empty; move L from A to B. After the
    // move, T's new manifest still references L (now via B), so the
    // runtime pin cascade should pick L up again.
    let (l_cid, l_bytes) = make_leaf(b"keep-pinned");
    let a_old = folders::build_folder(
        "A",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build A");
    let b_old = folders::build_folder("B", &[]).expect("build B");
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

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &a_old).await;
    ingest_folder(&harness, &b_old).await;
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

    // Pin T through the runtime so its cascade actually fires.
    let (reply_tx, reply_rx) = oneshot::channel();
    harness
        .verb_tx
        .send(ContentVerbRequest::Pin {
            cid: t_old.bundle_cid.to_bytes(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _ = reply_rx.await.unwrap();

    harmony_app::move_content_impl(
        t_sid.to_string(),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(a_old.bundle_cid.to_bytes()),
        ],
        hex::encode(l_cid),
        "L".to_string(),
        Some(t_sid.to_string()),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(b_old.bundle_cid.to_bytes()),
        ],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("Case A move");

    // The pin invariant maintenance dispatches Pin(new_top) because the
    // entry's pin flag is still true. Pin a second time to force a fresh
    // cascade walk against the rebuilt tree (idempotent), then snapshot
    // the pinned_set.
    let new_top_cid = index.lock().unwrap().get(&t_sid).unwrap().cid;
    let (reply_tx, reply_rx) = oneshot::channel();
    harness
        .verb_tx
        .send(ContentVerbRequest::Pin {
            cid: new_top_cid,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _ = reply_rx.await.unwrap();

    let (reply_tx, reply_rx) = oneshot::channel();
    harness
        .verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned = reply_rx.await.unwrap();
    assert!(
        pinned.contains(&l_cid),
        "L remains pinned via the rebuilt T tree"
    );
    assert!(pinned.contains(&new_top_cid), "new T pinned");
}

// ── Test 16: move_d_new_top_level_pin_defaults_unpinned ───────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_d_new_top_level_pin_defaults_unpinned() {
    let (l_cid, l_bytes) = make_leaf(b"to-root-d");
    let f_old = folders::build_folder(
        "F",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build F");

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &f_old).await;

    let (index, _index_dir) = fresh_index();
    let f_sid = insert_top_level(
        &index,
        f_old.bundle_cid.to_bytes(),
        "F",
        ContentKind::Folder,
        true, // F pinned
        f_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::move_content_impl(
        f_sid.to_string(),
        vec![hex::encode(f_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        "L".to_string(),
        None,
        vec![],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("Case D move");

    let new_top_sid = SidecarId::parse_str(&result.dst_sidecar_id).expect("parse");
    let idx = index.lock().unwrap();
    let new_top = idx.get(&new_top_sid).expect("new top entry exists");
    assert!(
        !new_top.pinned,
        "Case D defaults the new top-level entry to unpinned"
    );
}

// ── Test 17: move_top_level_to_root_rejected (Qodo Bug #1 regression) ─────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_top_level_to_root_rejected() {
    // Source is a top-level entry; destination is the root. This is a
    // no-op shape that pre-fix fell into Case D and tried to look up
    // the source root's own CID inside its own manifest, producing a
    // confusing "no entry pointing to child" error. The early-reject
    // guard surfaces it as an honest "source and destination are
    // identical" instead.
    let l_bytes = b"top-level-to-root".to_vec();
    let l_cid = ContentId::for_book(&l_bytes, ContentFlags::default())
        .expect("book cid")
        .to_bytes();

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(&index, l_cid, "L", ContentKind::Leaf, false, 17);

    let err = harmony_app::move_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        "L".to_string(),
        None,
        vec![],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("top-level → root must be rejected");

    assert!(
        err.contains("source and destination are identical"),
        "got: {err}"
    );
}

// ── Test 18: move_case_c_src_concurrently_rekeyed_compensates ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_case_c_src_concurrently_rekeyed_compensates() {
    // Case C STAGE 2 is now CAS-protected: if a concurrent rekey landed
    // on src between boundary verify and STAGE 2, `remove_if_cid_matches`
    // surfaces Conflict instead of silently deleting the freshly-rekeyed
    // user entry. Compensating undo then reverts the destination.
    let (l_cid, l_bytes) = make_leaf(b"case-c-cas");
    let f_old = folders::build_folder("F", &[]).expect("build F");

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &f_old).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(&index, l_cid, "L", ContentKind::Leaf, false, 11);
    let f_sid = insert_top_level(
        &index,
        f_old.bundle_cid.to_bytes(),
        "F",
        ContentKind::Folder,
        false,
        f_old.bundle_bytes.len() as u64,
    );
    let f_old_cid = f_old.bundle_cid.to_bytes();

    // Arm a one-shot conflict on L's STAGE 2 remove. Boundary verify
    // and STAGE 1 (dst rekey on F) still succeed against the unmodified
    // index; the armed hook fires at the CAS-protected
    // `remove_if_cid_matches` call so the test exercises the actual
    // post-STAGE-1 compensating-undo path its name claims.
    {
        let mut idx = index.lock().unwrap();
        idx.arm_next_remove_if_cid_matches_conflict(l_sid, [0xCD; 32]);
    }

    let err = harmony_app::move_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        "L".to_string(),
        Some(f_sid.to_string()),
        vec![hex::encode(f_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("CAS-protected Case C remove must reject divergent src");

    // Boundary verify passed (real CID matched), STAGE 1 committed,
    // STAGE 2 hit the armed conflict — the error must reflect the
    // STAGE-2 path plus a successful compensating undo of dst.
    assert!(
        err.contains("concurrent rekey on src_sidecar_id"),
        "got: {err}"
    );
    assert!(
        err.contains("dst rekey reverted"),
        "compensating undo of dst must succeed; got: {err}"
    );

    // F (destination) must be back at its original CID via the
    // compensating-undo path now that STAGE 1 actually committed.
    let idx = index.lock().unwrap();
    assert_eq!(
        idx.get(&f_sid).unwrap().cid,
        f_old_cid,
        "F ends at its original CID (compensating undo succeeded)"
    );
    // L's sidecar entry must still exist at its original CID — the CAS
    // remove refused to delete it.
    assert_eq!(
        idx.get(&l_sid).expect("L still present").cid,
        l_cid,
        "L untouched by failed STAGE 2",
    );
}

// ── Test 19: move_disambiguates_siblings_with_shared_cid (CodeRabbit round 3) ─

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_disambiguates_siblings_with_shared_cid() {
    // Two sibling sub-folders in T that share the empty-folder CID
    // because both are empty. Pre-fix the source-side walk matched by
    // CID alone and would remove whichever entry .position() hit first
    // (EmptyA), even though the caller asked to move EmptyB. The
    // round-3 fix carries `child_name` through the IPC + walk so the
    // requested sibling is removed.
    let empty = folders::build_folder("", &[]).expect("build empty placeholder");
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
    .expect("build T with two shared-CID siblings");

    let harness = spawn_test_runtime("move").await;
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

    // Move EmptyB (the second sibling) to root via Case D.
    let result = harmony_app::move_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(empty_cid),
        "EmptyB".to_string(),
        None,
        vec![],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("move EmptyB to root");

    // The rebuilt T must contain exactly one entry — and that entry
    // must be EmptyA, not EmptyB. (Both removal outcomes leave a
    // single-entry manifest of the same length, but different names,
    // so different bundle CIDs.)
    let t_new_cid_hex = result.src_new_cid.expect("src rekeyed");
    let t_new_cid_bytes = hex::decode(&t_new_cid_hex).expect("hex decode");
    let mut t_new_cid = [0u8; 32];
    t_new_cid.copy_from_slice(&t_new_cid_bytes);
    let expected_t_new = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: empty_cid,
            name: "EmptyA".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build expected T");
    assert_eq!(
        t_new_cid,
        expected_t_new.bundle_cid.to_bytes(),
        "T must end with [EmptyA] (the requested-move-target EmptyB was removed)",
    );

    // The minted top-level sidecar must carry the moved sibling's name.
    let new_top_sid = SidecarId::parse_str(&result.dst_sidecar_id).expect("parse dst sid");
    let idx = index.lock().unwrap();
    let new_top = idx.get(&new_top_sid).expect("new top entry exists");
    assert_eq!(new_top.file_name, "EmptyB");
    assert_eq!(new_top.cid, empty_cid);
}

// ── Test 20: move_rejects_when_name_does_not_match_cid ────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_rejects_when_name_does_not_match_cid() {
    // If the caller passes a `src_child_name` that disagrees with the
    // manifest's actual entry name for `src_child_cid`, the move must
    // refuse rather than silently grabbing whichever sibling happens
    // to match by CID. This protects against stale frontend state
    // where the row that was dragged no longer exists by the time the
    // command reaches the backend.
    let (l_cid, l_bytes) = make_leaf(b"named-mismatch");
    let a_old = folders::build_folder(
        "A",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "actual-name".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build A");

    let harness = spawn_test_runtime("move").await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &a_old).await;

    let (index, _index_dir) = fresh_index();
    let a_sid = insert_top_level(
        &index,
        a_old.bundle_cid.to_bytes(),
        "A",
        ContentKind::Folder,
        false,
        a_old.bundle_bytes.len() as u64,
    );

    let err = harmony_app::move_content_impl(
        a_sid.to_string(),
        vec![hex::encode(a_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        "stale-name".to_string(),
        None,
        vec![],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("mismatched name must reject");

    assert!(
        err.contains("has no entry named 'stale-name'"),
        "expected name+cid mismatch error, got: {err}"
    );
}
