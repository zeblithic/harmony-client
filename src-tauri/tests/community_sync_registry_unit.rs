//! Tests for `CommunitySyncRegistry` — the multi-community engine
//! lifecycle manager. Covers the spawn → has → stop → shutdown_all
//! happy path. Re-spawn idempotency and `known_ids()` snapshots will
//! be exercised by Task 12's owner-state subscription scan tests.

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
