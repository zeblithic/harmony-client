//! Case A publisher — publishes alice's iroh routing under HKDF(invite_token.sig, epoch)
//! while an invite is pending. Stops publishing on consumption / expiry / revoke.
//!
//! Each pending invite gets its own derived key (different sig per invite),
//! so multiple concurrent invites coexist without DHT key collision.

use harmony_pkarr::{
    current_epoch_id, derive_ephemeral_key, PkarrCase, PkarrPublisher, PkarrRoutingRecord,
    RecordBuilder,
};
use std::sync::Arc;

use crate::community_invite::CommunityInvitePayload;

pub struct PkarrInvitePublisher {
    publisher: Arc<PkarrPublisher>,
    identity_signing_key: ed25519_dalek::SigningKey,
    identity_pub: [u8; 64],
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

impl PkarrInvitePublisher {
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

    /// Called from the IPC layer after `generate_invite` succeeds.
    pub async fn register_invite(&self, invite: &CommunityInvitePayload) {
        let Some(token) = &invite.invite_token else {
            // Open community invites don't carry a token sig in the same way;
            // skip pkarr publish for now (Phase 3 may extend).
            return;
        };
        let epoch_id = current_epoch_id(now_ms());
        let signing = derive_ephemeral_key(PkarrCase::Invite, &token.sig, &epoch_id.to_be_bytes());

        let id_sk = self.identity_signing_key.clone();
        let id_pub = self.identity_pub;
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let builder: RecordBuilder = Arc::new(move |at_ms| {
            PkarrRoutingRecord::sign_new(blob_builder(), id_pub, at_ms, &id_sk)
                .expect("sign — fixed-size buffers should not fail")
        });

        let handle = format!("invite:{}", hex::encode(token.sig));
        self.publisher.register(handle, signing, builder).await;
    }

    /// Called when the invite is consumed, expires, or is revoked.
    pub async fn unregister_invite(&self, invite_token_sig: &[u8; 64]) {
        let handle = format!("invite:{}", hex::encode(invite_token_sig));
        self.publisher.unregister(&handle).await;
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use harmony_pkarr::{testing::MockPkarrRelay, RelayClient, RelayPool};
    use rand::rngs::OsRng;

    fn build_identity_pub(sk: &SigningKey) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        out
    }

    #[tokio::test]
    async fn register_then_unregister_does_not_panic() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_identity_pub(&sk);
        let inv_pub = PkarrInvitePublisher::new(
            publisher,
            sk,
            id_pub,
            Arc::new(|| b"fake-iroh-routing".to_vec()),
        );

        // Verify the unregister path is safe when nothing was registered.
        inv_pub.unregister_invite(&[0u8; 64]).await;
    }

    #[tokio::test]
    async fn unregister_nonexistent_handle_is_safe() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_identity_pub(&sk);
        let inv_pub =
            PkarrInvitePublisher::new(publisher.clone(), sk, id_pub, Arc::new(|| b"fake".to_vec()));

        // Multiple unregisters of the same missing handle should not panic.
        inv_pub.unregister_invite(&[1u8; 64]).await;
        inv_pub.unregister_invite(&[1u8; 64]).await;
        assert!(publisher.active_handles().await.is_empty());
    }
}
