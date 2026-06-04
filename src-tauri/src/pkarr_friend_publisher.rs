//! ZEB-371 Phase 1b: Case-D publisher. Publishes this node's iroh reachability
//! under a per-friend, secret-derived pkarr slot so each active friend can
//! resolve us across WAN without any global discoverability. One registered
//! handle per active friend; mirrors `PkarrIdentityPublisher`.

use crate::friend_rendezvous::{case_d_publish_key, seal_case_d_payload};
use harmony_pkarr::{
    current_epoch_id, EphemeralKeyBuilder, PkarrPublisher, PkarrRoutingRecord, RecordBuilder,
};
use std::sync::Arc;

pub struct PkarrFriendPublisher {
    publisher: Arc<PkarrPublisher>,
    self_owner: [u8; 16],
    /// Builds the raw (unsealed) iroh routing blob for the current reachability.
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

fn friend_handle(friend_owner: &[u8; 16]) -> String {
    format!("friend:{}", hex::encode(friend_owner))
}

impl PkarrFriendPublisher {
    pub fn new(
        publisher: Arc<PkarrPublisher>,
        self_owner: [u8; 16],
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    ) -> Self {
        Self {
            publisher,
            self_owner,
            routing_blob_builder,
        }
    }

    /// Begin (or refresh) Case-D publication for one active friend, keyed on the
    /// shared `secret`. Idempotent: re-registering the same friend replaces the
    /// builders (e.g. to pick up a changed reachability blob).
    pub async fn register_friend(&self, friend_owner: [u8; 16], secret: [u8; 32]) {
        let self_owner = self.self_owner;
        let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
            case_d_publish_key(&secret, current_epoch_id(at_ms), &self_owner)
        });
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let builder: RecordBuilder = Arc::new(move |at_ms| {
            let epoch = current_epoch_id(at_ms);
            let cd_key = case_d_publish_key(&secret, epoch, &self_owner);
            let mut id_pub = [0u8; 64];
            id_pub[32..].copy_from_slice(&cd_key.verifying_key().to_bytes());
            let sealed =
                seal_case_d_payload(&secret, epoch, &blob_builder()).expect("case-d payload seal");
            PkarrRoutingRecord::sign_new(sealed, id_pub, at_ms, &cd_key)
                .expect("sign — derived key matches embedded id_pub by construction")
        });
        self.publisher
            .register(friend_handle(&friend_owner), key_builder, builder)
            .await;
    }

    /// Stop Case-D publication for a friend (on revoke / secret cleared).
    pub async fn unregister_friend(&self, friend_owner: &[u8; 16]) {
        self.publisher
            .unregister(&friend_handle(friend_owner))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_pkarr::testing::MockPkarrRelay;
    use harmony_pkarr::{PkarrPublisher, RelayClient, RelayPool};
    use std::sync::Arc;

    #[tokio::test]
    async fn register_then_unregister_friend_slot() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let secret = [5u8; 32];
        let self_owner = [0xAA; 16];
        let friend = [0xBB; 16];
        let fp = PkarrFriendPublisher::new(
            Arc::clone(&publisher),
            self_owner,
            Arc::new(|| b"routing".to_vec()),
        );
        fp.register_friend(friend, secret).await;
        assert!(publisher
            .active_handles()
            .await
            .iter()
            .any(|h| h.starts_with("friend:")));
        fp.unregister_friend(&friend).await;
        assert!(!publisher
            .active_handles()
            .await
            .iter()
            .any(|h| h.starts_with("friend:")));
    }
}
