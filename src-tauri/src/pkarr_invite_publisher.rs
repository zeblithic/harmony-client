//! Case A publisher — publishes alice's iroh routing under HKDF(invite_token.sig, epoch)
//! while an invite is pending. Stops publishing on consumption / expiry / revoke.
//!
//! Each pending invite gets its own derived key (different sig per invite),
//! so multiple concurrent invites coexist without DHT key collision.

use harmony_pkarr::{
    current_epoch_id, derive_ephemeral_key, EphemeralKeyBuilder, PkarrCase, PkarrPublisher,
    PkarrRoutingRecord, RecordBuilder,
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
            // Open-community invites (invite_token: None) intentionally skip
            // case-A pkarr publish. The case-A primitive exists for the
            // ZEB-217 Phase 4 invite-only flow (counter-sig redemption), which
            // will supply a non-None invite_token. For Phase 2b open invites,
            // joiners must already share a transport (LAN Reticulum or the
            // future Phase 2c iroh-via-identity-discovery path) — there is no
            // per-invite secret to key an HKDF record on.
            return;
        };
        let handle = format!("invite:{}", hex::encode(token.sig));
        self.register_case_a(handle, token.sig).await;
    }

    /// Called when the invite is consumed, expires, or is revoked.
    pub async fn unregister_invite(&self, invite_token_sig: &[u8; 64]) {
        let handle = format!("invite:{}", hex::encode(invite_token_sig));
        self.publisher.unregister(&handle).await;
    }

    /// ZEB-370 Phase 1: publish the inviter's iroh routing for a friend token
    /// under the `friend:{hex}` handle namespace (distinct from `invite:` so
    /// friend links never collide with community invites). Reuses Case-A
    /// `PkarrCase::Invite` keying for now; Phase 1b switches this to
    /// `PkarrCase::Friend`. Called after `mint_friend_token` /
    /// `generate_friend_token` succeeds; unregistered on consume/expiry.
    pub async fn register_friend_token(&self, token_sig: &[u8; 64]) {
        let handle = format!("friend:{}", hex::encode(token_sig));
        self.register_case_a(handle, *token_sig).await;
    }

    /// Called when the friend token is consumed, expires, or is revoked.
    pub async fn unregister_friend_token(&self, token_sig: &[u8; 64]) {
        let handle = format!("friend:{}", hex::encode(token_sig));
        self.publisher.unregister(&handle).await;
    }

    /// Shared Case-A registration core for `register_invite` /
    /// `register_friend_token`. Publishes this node's routing blob (from the
    /// struct's `routing_blob_builder` field) under `handle`, keyed by an
    /// ephemeral key derived from `ikm` (the one-shot token sig) + the current
    /// epoch.
    ///
    /// Re-derives the ephemeral key on EVERY publish so it tracks the current
    /// epoch. Capturing the key once at registration would silently break
    /// Case-A discovery after the first epoch boundary (resolvers query the
    /// current-epoch key ± tolerance; a frozen old-epoch key falls outside the
    /// window after one or two boundaries).
    async fn register_case_a(&self, handle: String, ikm: [u8; 64]) {
        let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
            let epoch_id = current_epoch_id(at_ms);
            // TODO(ZEB-370 Phase 1b): this helper is shared by community invites
            // (`invite:` handles) and friend tokens (`friend:` handles); both keep
            // `PkarrCase::Invite` for now. When harmony-core ships `PkarrCase::Friend`,
            // parameterize the case so the friend path derives under it (the resolver
            // side must switch atomically). See `register_friend_token` doc comment.
            derive_ephemeral_key(PkarrCase::Invite, &ikm, &epoch_id.to_be_bytes())
        });

        let id_sk = self.identity_signing_key.clone();
        let id_pub = self.identity_pub;
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let builder: RecordBuilder = Arc::new(move |at_ms| {
            PkarrRoutingRecord::sign_new(blob_builder(), id_pub, at_ms, &id_sk)
                .expect("sign — fixed-size buffers should not fail")
        });

        self.publisher.register(handle, key_builder, builder).await;
    }
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
    async fn unregister_without_prior_register_is_safe() {
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
    async fn friend_token_register_unregister_round_trip() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_identity_pub(&sk);
        let inv_pub = PkarrInvitePublisher::new(
            publisher.clone(),
            sk,
            id_pub,
            Arc::new(|| b"friend-routing".to_vec()),
        );

        let token_sig = [0x44u8; 64];
        let handle = format!("friend:{}", hex::encode(token_sig));

        inv_pub.register_friend_token(&token_sig).await;
        assert!(
            publisher.active_handles().await.contains(&handle),
            "friend handle must be active after register"
        );

        inv_pub.unregister_friend_token(&token_sig).await;
        assert!(
            !publisher.active_handles().await.contains(&handle),
            "friend handle must be gone after unregister"
        );
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
