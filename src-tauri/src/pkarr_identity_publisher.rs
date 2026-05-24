//! Case B publisher — publishes alice's iroh routing under HKDF(owner_pub, epoch)
//! when user opts in via "Make me discoverable" toggle. Persisted via PkarrSettings.

use harmony_pkarr::{
    current_epoch_id, derive_ephemeral_key, EphemeralKeyBuilder, PkarrCase, PkarrPublisher,
    PkarrRoutingRecord, RecordBuilder,
};
use std::sync::Arc;

pub struct PkarrIdentityPublisher {
    publisher: Arc<PkarrPublisher>,
    identity_signing_key: ed25519_dalek::SigningKey,
    identity_pub: [u8; 64],
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

const HANDLE: &str = "identity";

impl PkarrIdentityPublisher {
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

    /// Register this device's identity publication. Called when the user enables
    /// "Make me discoverable" in settings (case B opt-in).
    pub async fn enable(&self) {
        // Re-derive the ephemeral key on EVERY publish so it tracks the
        // current epoch (see [`pkarr_invite_publisher`] for the bug history).
        let id_pub_for_key = self.identity_pub;
        let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
            let epoch_id = current_epoch_id(at_ms);
            derive_ephemeral_key(
                PkarrCase::Identity,
                &id_pub_for_key,
                &epoch_id.to_be_bytes(),
            )
        });

        let id_sk = self.identity_signing_key.clone();
        let id_pub = self.identity_pub;
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let builder: RecordBuilder = Arc::new(move |at_ms| {
            PkarrRoutingRecord::sign_new(blob_builder(), id_pub, at_ms, &id_sk)
                .expect("sign — fixed-size buffers should not fail")
        });

        self.publisher
            .register(HANDLE.to_string(), key_builder, builder)
            .await;
    }

    /// Unregister the identity publication. Called when the user disables the toggle.
    pub async fn disable(&self) {
        self.publisher.unregister(HANDLE).await;
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
    async fn enable_then_disable_round_trip() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_id_pub(&sk);
        let id_pub_publisher = PkarrIdentityPublisher::new(
            Arc::clone(&publisher),
            sk,
            id_pub,
            Arc::new(|| b"fake-routing".to_vec()),
        );

        id_pub_publisher.enable().await;
        assert!(publisher
            .active_handles()
            .await
            .contains(&"identity".to_string()));
        id_pub_publisher.disable().await;
        assert!(!publisher
            .active_handles()
            .await
            .contains(&"identity".to_string()));
    }

    #[tokio::test]
    async fn disable_when_not_enabled_is_safe() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_id_pub(&sk);
        let id_pub_publisher = PkarrIdentityPublisher::new(
            Arc::clone(&publisher),
            sk,
            id_pub,
            Arc::new(|| b"fake-routing".to_vec()),
        );

        // Should not panic if disabled before enabled.
        id_pub_publisher.disable().await;
        assert!(publisher.active_handles().await.is_empty());
    }
}
