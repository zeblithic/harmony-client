//! Unit tests for `community_state_persist`. Exercises the four
//! public entry points (`save_crdt` / `load_crdt` / `save_replay` /
//! `load_replay`) against the three load behaviors that matter:
//! round-trip, missing-file-tolerance, and loud-decode-error.

use harmony_app::community_state_crdt::CommunityState;
use harmony_app::community_state_persist::{
    load_crdt, load_replay, save_crdt, save_replay, PersistError,
};
use harmony_app::community_state_sync::CommunityRootHlcTracker;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

#[test]
fn save_and_load_crdt_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("crdt.cbor");

    let community_id = SpaceId([1u8; 16]);
    let original = CommunityState::new(community_id);
    save_crdt(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        &original,
    )
    .expect("save");
    let loaded = load_crdt(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        community_id,
    )
    .expect("load");
    assert_eq!(loaded.community_id, community_id);
    assert!(loaded.events_is_empty());
}

#[test]
fn load_crdt_missing_file_returns_empty_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nonexistent.cbor");
    let community_id = SpaceId([1u8; 16]);
    let loaded = load_crdt(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        community_id,
    )
    .expect("load missing");
    assert_eq!(loaded.community_id, community_id);
    assert!(loaded.events_is_empty());
}

#[test]
fn load_crdt_truncated_file_self_heals_to_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("truncated.cbor");
    // 2-byte CBOR fragment that's invalid for `CommunityState`'s
    // 2-key map shape — `\x82` opens a 2-element array, which the
    // map-shaped decoder rejects.
    std::fs::write(&path, b"\x82\x00").expect("write garbage");
    let community_id = SpaceId([1u8; 16]);
    let recovered = load_crdt(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        community_id,
    )
    .expect("self-heal returns default");
    assert_eq!(recovered.community_id, community_id);
    assert!(recovered.events_is_empty());

    // The corrupted file should have been quarantined under a
    // `.corrupt.<ts>` suffix so the next save_crdt lands cleanly.
    assert!(
        !path.exists(),
        "corrupted file should have been moved aside"
    );
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read tempdir")
        .filter_map(|r| r.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("truncated.cbor.corrupt.")
        })
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one quarantined sibling");
}

#[test]
fn load_replay_truncated_file_self_heals_to_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("replay.cbor");
    std::fs::write(&path, b"\xff\xff").expect("write garbage");
    let recovered = load_replay(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        &SpaceId([9u8; 16]),
    )
    .expect("self-heal returns default");
    assert!(recovered.per_device.is_empty());
    assert!(
        !path.exists(),
        "corrupted replay file should have been moved aside"
    );
}

#[test]
fn load_crdt_misrouted_legacy_file_returns_mismatch_err() {
    // A LEGACY plaintext file that parses as a different community must
    // still hit the routing guard (hard error, deliberately not
    // quarantined — the file probably belongs to that other community).
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("crdt.cbor");

    let saved_id = SpaceId([1u8; 16]);
    let expected_id = SpaceId([2u8; 16]);
    let legacy =
        harmony_app::owner_state_crypto::canonical_cbor_encode(&CommunityState::new(saved_id))
            .expect("encode");
    std::fs::write(&path, &legacy).expect("write legacy");

    let result = load_crdt(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        expected_id,
    );
    match result {
        Err(PersistError::CommunityIdMismatch { found, expected }) => {
            assert_eq!(found, saved_id);
            assert_eq!(expected, expected_id);
        }
        other => panic!("expected CommunityIdMismatch, got {other:?}"),
    }
    assert!(path.exists(), "mismatched legacy file left in place");
}

#[test]
fn load_crdt_misrouted_sealed_file_quarantines_via_aad() {
    // ZEB-983: a SEALED file misrouted across communities never reaches
    // the id check — the AAD label (which binds the community id) fails
    // the tag first, classifying as content corruption → quarantine +
    // default. The plaintext-era CommunityIdMismatch remains reachable
    // only for legacy files (test above).
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("crdt.cbor");

    let saved_id = SpaceId([1u8; 16]);
    let expected_id = SpaceId([2u8; 16]);
    let cipher = harmony_app::device_dataset_file::test_cipher();
    save_crdt(&cipher, &path, &CommunityState::new(saved_id)).expect("save sealed");

    let loaded = load_crdt(&cipher, &path, expected_id).expect("quarantine + default");
    assert_eq!(loaded.community_id, expected_id);
    assert!(loaded.events_is_empty());
    assert!(!path.exists(), "sealed misrouted file moved aside");
    assert!(
        std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt.")),
        "quarantined sibling exists"
    );
}

#[test]
fn save_and_load_replay_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("replay.cbor");

    let alice = OwnerAddr([0xA1; 16]);
    // ZEB-750: this is a DTO round-trip test, so it populates the on-disk
    // shape directly. `record` moved to the runtime tracker (`admit`/`commit`
    // on `CommunityReplayTracker`); the DTO is now pure data.
    let mut tracker = CommunityRootHlcTracker::default();
    tracker.per_device.insert(
        (alice, "dev".to_string()),
        Hlc {
            wall_ms: 1000,
            logical: 5,
            device_id: "dev".into(),
        },
    );
    save_replay(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        &SpaceId([9u8; 16]),
        &tracker,
    )
    .expect("save");
    let loaded = load_replay(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        &SpaceId([9u8; 16]),
    )
    .expect("load");
    let key = (alice, "dev".to_string());
    assert_eq!(loaded.per_device.get(&key).map(|h| h.wall_ms), Some(1000));
    assert_eq!(loaded.per_device.get(&key).map(|h| h.logical), Some(5));
}

#[test]
fn load_replay_quarantines_and_recovers_from_old_shape() {
    // ZEB-256 breaks tracker shape (per_device key changed from
    // String to (OwnerAddr, String)). Old persisted files MUST not
    // crash boot — load_replay quarantines the unparseable bytes
    // and returns CommunityRootHlcTracker::default(). The engine
    // then rebuilds the tracker organically as publishes arrive.

    use std::collections::BTreeMap;

    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("replay.cbor");

    // Hand-write CBOR bytes for the OLD shape: `{ per_device: {String: Hlc} }`.
    // Use ciborium (the project's CBOR library — serde_cbor isn't a dep).
    #[derive(serde::Serialize)]
    struct OldShape {
        per_device: BTreeMap<String, Hlc>,
    }
    let old = OldShape {
        per_device: {
            let mut m = BTreeMap::new();
            m.insert(
                "old-dev".to_string(),
                Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "old-dev".into(),
                },
            );
            m
        },
    };
    let mut old_bytes: Vec<u8> = Vec::new();
    ciborium::ser::into_writer(&old, &mut old_bytes).expect("encode old shape");
    std::fs::write(&path, &old_bytes).expect("write");

    let recovered = load_replay(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        &SpaceId([9u8; 16]),
    )
    .expect("load_replay self-heals");
    assert!(
        recovered.per_device.is_empty(),
        "recovered tracker MUST be empty (default)"
    );

    // Verify quarantined sibling file exists.
    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        entries
            .iter()
            .any(|n| n.starts_with("replay.cbor.corrupt.")),
        "quarantined file must exist: {entries:?}"
    );

    // After self-heal, save_replay writes the new shape cleanly.
    let mut t = CommunityRootHlcTracker::default();
    let alice = OwnerAddr([0xA1; 16]);
    t.per_device.insert(
        (alice, "new-dev".to_string()),
        Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "new-dev".into(),
        },
    );
    save_replay(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        &SpaceId([9u8; 16]),
        &t,
    )
    .expect("save_replay");
    let reloaded = load_replay(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        &SpaceId([9u8; 16]),
    )
    .expect("reload");
    assert_eq!(reloaded.per_device.len(), 1);
    assert!(reloaded
        .per_device
        .contains_key(&(alice, "new-dev".to_string())));
}
