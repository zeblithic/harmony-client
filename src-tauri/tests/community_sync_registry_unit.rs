//! Tests for `CommunitySyncRegistry` — the multi-community engine
//! lifecycle manager. Covers the spawn → has → stop → shutdown_all
//! happy path, idempotent re-spawn, and `known_ids()` snapshot
//! ordering invariant.

use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};
use std::sync::Arc;
use tokio::sync::mpsc;

struct NopResolver;
impl IdentityResolver for NopResolver {
    fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> {
        None
    }
}

#[tokio::test]
async fn registry_spawns_and_tears_down_per_community() {
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let dir = tempfile::tempdir().expect("tempdir");

    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
    });

    let cid_a = SpaceId([1u8; 16]);
    let mk_a = MembershipKey::new([0xa1; 32]);
    let admin_a = OwnerAddr([0xb1; 16]);

    let (a_pub_tx, _a_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine(
            cid_a, mk_a, admin_a, /* is_invite_only */ false, a_pub_tx, a_sub_rx,
        )
        .await
        .expect("spawn a");

    assert!(registry.has_engine(&cid_a).await);

    registry.stop_engine(&cid_a).await.expect("stop");
    assert!(!registry.has_engine(&cid_a).await);

    registry.shutdown_all().await.expect("shutdown_all");
}

#[tokio::test]
async fn registry_spawn_is_idempotent_and_known_ids_is_sorted() {
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");

    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
    });

    // Spawn two distinct communities. Use unsorted ID order to verify
    // BTreeMap-backed known_ids() returns them sorted.
    let cid_b = SpaceId([0x02; 16]);
    let cid_a = SpaceId([0x01; 16]);

    let (a_pub_tx, _a_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_b_sub_tx, b_sub_rx) = mpsc::channel(8);

    registry
        .spawn_engine(
            cid_b,
            MembershipKey::new([0xb1; 32]),
            OwnerAddr([0xb1; 16]),
            false,
            b_pub_tx,
            b_sub_rx,
        )
        .await
        .expect("spawn b");
    registry
        .spawn_engine(
            cid_a,
            MembershipKey::new([0xa1; 32]),
            OwnerAddr([0xa1; 16]),
            false,
            a_pub_tx,
            a_sub_rx,
        )
        .await
        .expect("spawn a");

    // Idempotent re-spawn: returns Ok, doesn't double-insert.
    let (a2_pub_tx, _a2_pub_rx) = mpsc::channel(8);
    let (_a2_sub_tx, a2_sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine(
            cid_a,
            MembershipKey::new([0xa1; 32]),
            OwnerAddr([0xa1; 16]),
            false,
            a2_pub_tx,
            a2_sub_rx,
        )
        .await
        .expect("re-spawn idempotent");

    // BTreeMap → known_ids() returns SpaceId-Ord order regardless of
    // insertion order.
    let ids = registry.known_ids().await;
    assert_eq!(ids, vec![cid_a, cid_b], "known_ids must be sorted");

    // shutdown_all drains the map: known_ids must be empty afterwards.
    registry.shutdown_all().await.expect("shutdown_all");
    assert!(
        registry.known_ids().await.is_empty(),
        "shutdown_all must empty the map"
    );
}
