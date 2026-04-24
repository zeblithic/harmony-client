//! ZEB-158 slice 1: end-to-end tests for folder create/list.
//!
//! Data-layer tests (Tasks 5–6) drive build_folder + sidecar directly.
//! Full event-loop harness tests (Task 7) validate pin cascade + list_folder.
//! Pattern follows content_index_integration.rs's style; integration
//! tests only reach `pub` symbols.

use tempfile::tempdir;

use harmony_app::content_index::{
    ContentIndex, ContentIndexEntry, ContentKind, ReplicationTier, Sensitivity,
};
use harmony_app::folders;

#[test]
fn create_folder_at_root_then_list_shows_it() {
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(dir.path());

    // Build what create_folder_at_root would build: an empty folder.
    let built = folders::build_folder("Photos", &[]).expect("build");

    // Insert the sidecar entry that create_folder_at_root would insert.
    let inserted = idx.insert(ContentIndexEntry {
        cid: built.bundle_cid.to_bytes(),
        file_name: "Photos".into(),
        size_bytes: built.bundle_bytes.len() as u64,
        stored_at_ms: 1,
        sensitivity: Sensitivity::Private,
        replication_tier: ReplicationTier::Default,
        licensed: false,
        archived: false,
        pinned: false,
        kind: ContentKind::Folder,
    });
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
    // Build root "Photos" folder at depth 0. Sidecar has entry keyed by
    // Photos's bundle CID (call it root_v1).
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(dir.path());
    let photos_v1 = folders::build_folder("Photos", &[]).expect("build v1");

    idx.insert(ContentIndexEntry {
        cid: photos_v1.bundle_cid.to_bytes(),
        file_name: "Photos".into(),
        size_bytes: photos_v1.bundle_bytes.len() as u64,
        stored_at_ms: 1,
        sensitivity: Sensitivity::Private,
        replication_tier: ReplicationTier::Default,
        licensed: false,
        archived: false,
        pinned: true,  // must survive rekey
        kind: ContentKind::Folder,
    });

    // Now simulate creating "2026" inside Photos. This should produce:
    //   - A new empty "2026" folder (child of the new Photos bundle).
    //   - A new Photos bundle (root_v2) whose manifest lists "2026".
    //   - Sidecar rekey: old key photos_v1 → new key photos_v2.
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
        &photos_v1.bundle_cid.to_bytes(),
        photos_v2.bundle_cid.to_bytes(),
        photos_v2.bundle_bytes.len() as u64,
        /* new_stored_at_ms */ 2,
    );
    assert!(rekeyed, "rekey succeeds");

    let after = idx
        .get(&photos_v2.bundle_cid.to_bytes())
        .expect("rekeyed entry present");
    assert!(after.pinned, "pinned survives rekey");
    assert_eq!(after.kind, ContentKind::Folder);
    assert_eq!(after.file_name, "Photos");
    assert!(idx.get(&photos_v1.bundle_cid.to_bytes()).is_none(),
        "old entry removed");
}
