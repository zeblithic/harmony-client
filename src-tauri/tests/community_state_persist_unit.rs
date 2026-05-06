//! Unit tests for `community_state_persist`. Exercises the four
//! public entry points (`save_crdt` / `load_crdt` / `save_replay` /
//! `load_replay`) against the three load behaviors that matter:
//! round-trip, missing-file-tolerance, and loud-decode-error.

use harmony_app::community_state_crdt::CommunityState;
use harmony_app::community_state_persist::{
    load_crdt, load_replay, save_crdt, save_replay, PersistError,
};
use harmony_app::community_state_sync::CommunityRootHlcTracker;
use harmony_app::owner_state_types::{Hlc, SpaceId};

#[test]
fn save_and_load_crdt_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("crdt.cbor");

    let community_id = SpaceId([1u8; 16]);
    let original = CommunityState::new(community_id);
    save_crdt(&path, &original).expect("save");
    let loaded = load_crdt(&path, community_id).expect("load");
    assert_eq!(loaded.community_id, community_id);
    assert!(loaded.events.is_empty());
}

#[test]
fn load_crdt_missing_file_returns_empty_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nonexistent.cbor");
    let community_id = SpaceId([1u8; 16]);
    let loaded = load_crdt(&path, community_id).expect("load missing");
    assert_eq!(loaded.community_id, community_id);
    assert!(loaded.events.is_empty());
}

#[test]
fn load_crdt_truncated_file_returns_err() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("truncated.cbor");
    // 2-byte CBOR fragment that's invalid for `CommunityState`'s
    // 2-key map shape — `\x82` opens a 2-element array, which the
    // map-shaped decoder rejects.
    std::fs::write(&path, b"\x82\x00").expect("write garbage");
    let result = load_crdt(&path, SpaceId([1u8; 16]));
    assert!(matches!(result, Err(PersistError::CborDecode(_))));
}

#[test]
fn save_and_load_replay_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("replay.cbor");

    let mut tracker = CommunityRootHlcTracker::default();
    tracker.record(Hlc {
        wall_ms: 1000,
        logical: 5,
        device_id: "dev".into(),
    });
    save_replay(&path, &tracker).expect("save");
    let loaded = load_replay(&path).expect("load");
    assert_eq!(loaded.per_device.get("dev").map(|h| h.wall_ms), Some(1000));
    assert_eq!(loaded.per_device.get("dev").map(|h| h.logical), Some(5));
}
