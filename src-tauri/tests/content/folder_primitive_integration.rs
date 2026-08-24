//! ZEB-158 slice 1: end-to-end tests for folder create/list.
//!
//! Data-layer tests (Tasks 5–6) drive build_folder + sidecar directly.
//! Full event-loop harness tests (Task 7) validate pin cascade + list_folder.
//! Pattern follows content_index_integration.rs's style; integration
//! tests only reach `pub` symbols.

use tempfile::tempdir;

use harmony_app::content_index::{ContentIndex, ContentIndexEntry, ContentKind, SidecarId};
use harmony_app::folders;

use crate::harness::{make_entry, spawn_test_runtime};

#[test]
fn create_folder_at_root_then_list_shows_it() {
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(
        Some(&harmony_app::device_dataset_file::test_cipher()),
        dir.path(),
    );

    // Build what create_folder_at_root would build: an empty folder.
    let built = folders::build_folder("Photos", &[]).expect("build");

    // Insert the sidecar entry that create_folder_at_root would insert.
    let inserted = idx.insert(make_entry(
        SidecarId::new(),
        built.bundle_cid.to_bytes(),
        "Photos",
        built.bundle_bytes.len() as u64,
        ContentKind::Folder,
        false,
    ));
    assert!(inserted, "new entry inserted");

    // Inspect the sidecar — one row with kind Folder, name Photos.
    let rows: Vec<&ContentIndexEntry> = idx.entries().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, ContentKind::Folder);
    assert_eq!(rows[0].file_name, "Photos");
    assert!(rows[0].size_bytes > 0);
    assert!(!rows[0].pinned);

    // Bundle bytes of an empty folder are the 32-byte manifest CID.
    assert_eq!(built.bundle_bytes.len(), 32);
}

#[test]
fn create_nested_folder_updates_top_level_root_cid() {
    // Build root "Photos" folder at depth 0. Sidecar entry is keyed by
    // a fresh SidecarId; its `cid` field holds Photos's bundle CID
    // (call that root_v1).
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(
        Some(&harmony_app::device_dataset_file::test_cipher()),
        dir.path(),
    );
    let photos_v1 = folders::build_folder("Photos", &[]).expect("build v1");

    let photos_sid = SidecarId::new();
    // pinned: true must survive rekey
    idx.insert(make_entry(
        photos_sid,
        photos_v1.bundle_cid.to_bytes(),
        "Photos",
        photos_v1.bundle_bytes.len() as u64,
        ContentKind::Folder,
        true,
    ));

    // Now simulate creating "2026" inside Photos. This should produce:
    //   - A new empty "2026" folder (child of the new Photos bundle).
    //   - A new Photos bundle (root_v2) whose manifest lists "2026".
    //   - Sidecar rekey: same SidecarId, but the entry's `cid` field
    //     flips from photos_v1 to photos_v2. (ZEB-164's symlink-style
    //     model keys entries by SidecarId, not CID — the map key is
    //     stable across rekey.)
    //   - Sidecar's pinned flag still true after rekey.
    let sub = folders::build_folder("2026", &[]).expect("sub build");
    let photos_v2 = folders::build_folder(
        "Photos",
        &[folders::ManifestEntry {
            cid: sub.bundle_cid.to_bytes(),
            name: "2026".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("v2 build");

    let rekeyed = idx.rekey(
        &photos_sid,
        photos_v1.bundle_cid.to_bytes(),
        photos_v2.bundle_cid.to_bytes(),
        photos_v2.bundle_bytes.len() as u64,
        /* new_stored_at_ms */ 2,
    );
    assert!(rekeyed.is_ok(), "rekey succeeds");

    // Under the symlink-style model (ZEB-164), rekey mutates the entry's
    // CID in place rather than re-keying the map: same SidecarId, new CID.
    let after = idx.get(&photos_sid).expect("rekeyed entry present");
    assert_eq!(after.cid, photos_v2.bundle_cid.to_bytes(), "cid is now v2");
    assert!(after.pinned, "pinned survives rekey");
    assert_eq!(after.kind, ContentKind::Folder);
    assert_eq!(after.file_name, "Photos");
    assert!(
        idx.entries_for_cid(&photos_v1.bundle_cid.to_bytes())
            .next()
            .is_none(),
        "no sidecar entry references the old CID"
    );
}

// ── Task 7: Event-loop harness tests ────────────────────────────────────────

use harmony_app::event_loop::ContentVerbRequest;
use tokio::sync::oneshot;

// ── Test 1: pin_folder_cascades_to_nested_leaf ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pin_folder_cascades_to_nested_leaf() {
    use harmony_content::cid::{ContentFlags, ContentId};

    let leaf_bytes = b"hello world".to_vec();
    let leaf_cid = ContentId::for_book(&leaf_bytes, ContentFlags::default()).unwrap();
    let folder = folders::build_folder(
        "FolderWithLeaf",
        &[folders::ManifestEntry {
            cid: leaf_cid.to_bytes(),
            name: "hello.txt".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build folder");

    let harness = spawn_test_runtime("folder").await;

    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(leaf_cid.to_bytes()),
        leaf_bytes,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.manifest_cid.to_bytes()),
        folder.manifest_bytes,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.bundle_cid.to_bytes()),
        folder.bundle_bytes,
        false,
    )
    .await
    .unwrap();

    // Pin the folder root.
    let (reply_tx, reply_rx) = oneshot::channel();
    harness
        .verb_tx
        .send(ContentVerbRequest::Pin {
            cid: folder.bundle_cid.to_bytes(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    assert!(reply_rx.await.unwrap().unwrap());

    // Inspect the pinned set — cascade should include all three CIDs.
    let (reply_tx, reply_rx) = oneshot::channel();
    harness
        .verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned = reply_rx.await.unwrap();

    assert!(
        pinned.contains(&folder.bundle_cid.to_bytes()),
        "folder pinned"
    );
    assert!(
        pinned.contains(&folder.manifest_cid.to_bytes()),
        "manifest pinned via cascade"
    );
    assert!(
        pinned.contains(&leaf_cid.to_bytes()),
        "leaf pinned via cascade"
    );
}

// ── Test 2: pin_intent_survives_restart_for_folder ────────────────────────

#[test]
fn pin_intent_survives_restart_for_folder() {
    let dir = tempdir().unwrap();
    // Capture the sid before the first scope ends so the post-reload lookup
    // proves SidecarId persistence, not just "some folder entry survived".
    let sid = SidecarId::new();

    {
        let mut idx = ContentIndex::load(
            Some(&harmony_app::device_dataset_file::test_cipher()),
            dir.path(),
        );
        let built = folders::build_folder("Pinned", &[]).expect("build");
        idx.insert(make_entry(
            sid,
            built.bundle_cid.to_bytes(),
            "Pinned",
            built.bundle_bytes.len() as u64,
            ContentKind::Folder,
            true,
        ));
    }

    let idx = ContentIndex::load(
        Some(&harmony_app::device_dataset_file::test_cipher()),
        dir.path(),
    );
    let entry = idx
        .get(&sid)
        .expect("folder entry persisted under same sid");
    assert_eq!(entry.kind, ContentKind::Folder);
    assert_eq!(entry.file_name, "Pinned");
    assert!(entry.pinned, "pin intent survives reload");
}

// ── Test 3: list_folder_end_to_end_with_two_children ─────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_end_to_end_with_two_children() {
    use harmony_content::cid::{ContentFlags, ContentId};

    let bytes_a = b"alpha".to_vec();
    let bytes_b = b"beta".to_vec();
    let cid_a = ContentId::for_book(&bytes_a, ContentFlags::default()).unwrap();
    let cid_b = ContentId::for_book(&bytes_b, ContentFlags::default()).unwrap();
    let folder = folders::build_folder(
        "TwoChildren",
        &[
            folders::ManifestEntry {
                cid: cid_a.to_bytes(),
                name: "a.txt".into(),
                kind: ContentKind::Leaf,
            },
            folders::ManifestEntry {
                cid: cid_b.to_bytes(),
                name: "b.txt".into(),
                kind: ContentKind::Leaf,
            },
        ],
    )
    .expect("build");

    let harness = spawn_test_runtime("folder").await;
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(cid_a.to_bytes()),
        bytes_a,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(cid_b.to_bytes()),
        bytes_b,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.manifest_cid.to_bytes()),
        folder.manifest_bytes,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.bundle_cid.to_bytes()),
        folder.bundle_bytes,
        false,
    )
    .await
    .unwrap();

    let empty_pinned = std::collections::HashSet::new();
    let rows = harmony_app::list_folder(
        hex::encode(folder.bundle_cid.to_bytes()),
        harness.verb_tx.clone(),
        &empty_pinned,
    )
    .await
    .expect("list_folder succeeds");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "a.txt");
    assert_eq!(rows[0].kind, "leaf");
    assert_eq!(rows[0].cid, hex::encode(cid_a.to_bytes()));
    assert_eq!(rows[1].name, "b.txt");
    assert!(!rows[0].pinned);
    assert!(!rows[1].pinned);
}

// ── Test 4: list_folder_empty_returns_empty_vec ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_empty_returns_empty_vec() {
    let folder = folders::build_folder("Empty", &[]).expect("build");

    let harness = spawn_test_runtime("folder").await;
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.manifest_cid.to_bytes()),
        folder.manifest_bytes,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.bundle_cid.to_bytes()),
        folder.bundle_bytes,
        false,
    )
    .await
    .unwrap();

    let empty_pinned = std::collections::HashSet::new();
    let rows = harmony_app::list_folder(
        hex::encode(folder.bundle_cid.to_bytes()),
        harness.verb_tx.clone(),
        &empty_pinned,
    )
    .await
    .expect("list_folder succeeds on empty folder");

    assert!(rows.is_empty(), "empty folder returns empty Vec");
}

// ── Test 5: list_folder_not_in_cache_returns_empty ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_not_in_cache_returns_empty() {
    let harness = spawn_test_runtime("folder").await;

    let random_cid_hex = hex::encode([0x42u8; 32]);
    let empty_pinned = std::collections::HashSet::new();
    let rows = harmony_app::list_folder(random_cid_hex, harness.verb_tx.clone(), &empty_pinned)
        .await
        .expect("not-in-cache returns Ok(empty), not Err");

    assert!(rows.is_empty(), "cold cache returns empty, not an error");
}

// ── Test 6: list_folder_malformed_manifest_returns_error ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_malformed_manifest_returns_error() {
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cid::{ContentFlags, ContentId};

    let bad_manifest = b"definitely not a folder manifest".to_vec();
    let bad_manifest_cid = ContentId::for_book(&bad_manifest, ContentFlags::default()).unwrap();
    let leaf_bytes = b"leaf".to_vec();
    let leaf_cid = ContentId::for_book(&leaf_bytes, ContentFlags::default()).unwrap();

    let mut builder = BundleBuilder::new();
    builder.add(bad_manifest_cid);
    builder.add(leaf_cid);
    let (bundle_bytes, bundle_cid) = builder.build_with_flags(ContentFlags::default()).unwrap();

    let harness = spawn_test_runtime("folder").await;
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(leaf_cid.to_bytes()),
        leaf_bytes,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(bad_manifest_cid.to_bytes()),
        bad_manifest,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(bundle_cid.to_bytes()),
        bundle_bytes,
        false,
    )
    .await
    .unwrap();

    let empty_pinned = std::collections::HashSet::new();
    let err = harmony_app::list_folder(
        hex::encode(bundle_cid.to_bytes()),
        harness.verb_tx.clone(),
        &empty_pinned,
    )
    .await
    .expect_err("malformed manifest must surface an error, not an empty Vec");

    assert!(
        err.contains("manifest parse"),
        "error mentions manifest parse: {err}"
    );
}
