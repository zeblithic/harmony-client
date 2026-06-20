//! Case C publisher — publishes alice's iroh routing per community she's in,
//! keyed by HKDF(EpochKey ‖ own_identity_pub, epoch). Used by other community
//! members' resolvers when Phase 1's CRDT-broadcast routing is stale.

use harmony_pkarr::{
    current_epoch_id, derive_ephemeral_key, EphemeralKeyBuilder, PkarrCase, PkarrPublisher,
    PkarrRoutingRecord, RecordBuilder,
};
use std::sync::Arc;

use crate::owner_state_types::SpaceId;

pub struct PkarrCommunityPublisher {
    publisher: Arc<PkarrPublisher>,
    identity_signing_key: ed25519_dalek::SigningKey,
    identity_pub: [u8; 64],
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

impl PkarrCommunityPublisher {
    pub fn new(
        publisher: Arc<PkarrPublisher>,
        identity_signing_key: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    ) -> Self {
        Self {
            publisher,
            identity_signing_key,
            identity_pub,
            routing_blob_builder,
        }
    }

    /// Called when this device joins (or creates) a community. Registers a
    /// per-community pkarr publication under HKDF(EpochKey ‖ identity_pub, epoch).
    pub async fn on_community_joined(&self, community_id: SpaceId, epoch_key: [u8; 32]) {
        // Re-derive the ephemeral key on EVERY publish so it tracks the
        // current epoch (see [`pkarr_invite_publisher`] for the bug history).
        let id_pub_for_key = self.identity_pub;
        let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
            let epoch_id = current_epoch_id(at_ms);
            let mut info = Vec::with_capacity(64 + 8);
            info.extend_from_slice(&id_pub_for_key);
            info.extend_from_slice(&epoch_id.to_be_bytes());
            derive_ephemeral_key(PkarrCase::Community, &epoch_key, &info)
        });

        let id_sk = self.identity_signing_key.clone();
        let id_pub = self.identity_pub;
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let builder: RecordBuilder = Arc::new(move |at_ms| {
            PkarrRoutingRecord::sign_new(
                blob_builder(),
                id_pub,
                at_ms,
                at_ms + crate::reachability_record::REACHABILITY_RECORD_TTL_MS,
                &id_sk,
            )
            .expect("sign — fixed-size buffers should not fail")
        });

        let handle = format!("community:{}", hex::encode(community_id.0));
        self.publisher.register(handle, key_builder, builder).await;
    }

    /// Called when this device leaves or is kicked from a community. Removes the
    /// per-community pkarr publication.
    pub async fn on_community_left_or_kicked(&self, community_id: SpaceId) {
        let handle = format!("community:{}", hex::encode(community_id.0));
        self.publisher.unregister(&handle).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use harmony_pkarr::{testing::MockPkarrRelay, RelayClient, RelayPool};
    use rand::rngs::OsRng;

    fn build_id_pub(sk: &SigningKey) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        out
    }

    #[tokio::test]
    async fn join_then_leave_round_trip() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_id_pub(&sk);
        let com_pub = PkarrCommunityPublisher::new(
            Arc::clone(&publisher),
            sk,
            id_pub,
            Arc::new(|| b"routing".to_vec()),
        );

        // SpaceId is [u8; 16] — use 16-byte deterministic fixture.
        let community_id = SpaceId([7u8; 16]);
        let epoch_key = [0xAAu8; 32];
        com_pub.on_community_joined(community_id, epoch_key).await;
        assert!(publisher
            .active_handles()
            .await
            .iter()
            .any(|h| h.starts_with("community:")));
        com_pub.on_community_left_or_kicked(community_id).await;
        assert!(!publisher
            .active_handles()
            .await
            .iter()
            .any(|h| h.starts_with("community:")));
    }

    #[tokio::test]
    async fn leave_unjoined_community_is_safe() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_id_pub(&sk);
        let com_pub = PkarrCommunityPublisher::new(
            Arc::clone(&publisher),
            sk,
            id_pub,
            Arc::new(|| b"routing".to_vec()),
        );

        // Leaving a community we never joined should not panic.
        com_pub
            .on_community_left_or_kicked(SpaceId([0u8; 16]))
            .await;
        assert!(publisher.active_handles().await.is_empty());
    }
}
