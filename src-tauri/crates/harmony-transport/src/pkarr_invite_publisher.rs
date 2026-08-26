//! Case A publisher — publishes alice's iroh routing under HKDF(invite_token.sig, epoch)
//! while an invite is pending. Stops publishing on consumption / expiry / revoke.
//!
//! Each pending invite gets its own derived key (different sig per invite),
//! so multiple concurrent invites coexist without DHT key collision.

use harmony_pkarr::{
    current_epoch_id, derive_ephemeral_key, EphemeralKeyBuilder, PkarrCase, PkarrPublisher,
    PkarrRoutingRecord, RecordBuilder,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct PkarrInvitePublisher {
    publisher: Arc<PkarrPublisher>,
    identity_signing_key: ed25519_dalek::SigningKey,
    identity_pub: [u8; 64],
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    /// ZEB-370 consent gate: the friend tokens THIS node has minted + published
    /// and not yet consumed, keyed on `token_sig`, with their optional expiry
    /// (`expires_at` ms, `None` = no expiry). The inbound friend acceptor
    /// consults [`Self::is_friend_token_active`] against this map BEFORE adding a
    /// friend, so a self-signed request carrying an arbitrary `token_sig` this
    /// node never minted is rejected (fail-closed). Held under a `std::sync`
    /// mutex — the map is touched without holding the lock across any `.await`,
    /// so no async mutex is warranted.
    live_friend_tokens: Mutex<HashMap<[u8; 64], Option<u64>>>,
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
            live_friend_tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Called from the IPC layer after `generate_invite` succeeds, with the
    /// invite token's signature (`invite_token.sig`) — or `None` for an
    /// open-community invite (`invite_token: None`). Taking the sig directly
    /// keeps this publisher free of community-invite payload knowledge; the
    /// composition root extracts it.
    pub async fn register_invite(&self, invite_token_sig: Option<[u8; 64]>) {
        let Some(sig) = invite_token_sig else {
            // Open-community invites (invite_token: None) intentionally skip
            // case-A pkarr publish. The case-A primitive exists for the
            // ZEB-217 Phase 4 invite-only flow (counter-sig redemption), which
            // will supply a non-None invite_token. For Phase 2b open invites,
            // joiners must already share a transport (LAN Reticulum or the
            // future Phase 2c iroh-via-identity-discovery path) — there is no
            // per-invite secret to key an HKDF record on.
            return;
        };
        let handle = format!("invite:{}", hex::encode(sig));
        self.register_case_a(handle, sig).await;
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
    ///
    /// Records `(token_sig -> expires_at)` in the live-token map FIRST (so the
    /// consent gate [`Self::is_friend_token_active`] sees it the instant the
    /// pkarr handle goes live), then registers the Case-A pkarr handle.
    pub async fn register_friend_token(&self, token_sig: &[u8; 64], expires_at: Option<u64>) {
        // Record the live token before publishing so an inbound handshake racing
        // the first publish still sees the gate as open.
        self.live_friend_tokens
            .lock()
            .expect("live_friend_tokens mutex poisoned")
            .insert(*token_sig, expires_at);
        let handle = format!("friend:{}", hex::encode(token_sig));
        self.register_case_a(handle, *token_sig).await;
    }

    /// Called when the friend token is consumed, expires, or is revoked. Removes
    /// the live-token map entry (closing the consent gate for it) AND stops the
    /// Case-A pkarr republication.
    pub async fn unregister_friend_token(&self, token_sig: &[u8; 64]) {
        self.live_friend_tokens
            .lock()
            .expect("live_friend_tokens mutex poisoned")
            .remove(token_sig);
        let handle = format!("friend:{}", hex::encode(token_sig));
        self.publisher.unregister(&handle).await;
    }

    /// ZEB-370 consent gate: is `token_sig` a friend token THIS node minted +
    /// published and has not yet consumed, AND is it unexpired at `now_ms`?
    ///
    /// True iff the token is present in the live-token map (registered, not yet
    /// `unregister`ed) AND (`expires_at` is `None` OR `expires_at > now_ms`).
    /// The inbound friend acceptor requires this before adding a friend, so a
    /// peer presenting a self-signed request with an arbitrary `token_sig` that
    /// does not correspond to a live minted token is rejected (fail-closed).
    ///
    /// `async` for call-site symmetry with the other publisher methods; the map
    /// is a `std::sync::Mutex` and the lock is never held across an `.await`.
    pub async fn is_friend_token_active(&self, token_sig: &[u8; 64], now_ms: u64) -> bool {
        let map = self
            .live_friend_tokens
            .lock()
            .expect("live_friend_tokens mutex poisoned");
        match map.get(token_sig) {
            Some(None) => true,                             // registered, no expiry
            Some(Some(expires_at)) => *expires_at > now_ms, // registered, unexpired
            None => false,                                  // never registered / consumed
        }
    }

    /// ZEB-370 consent gate (ATOMIC one-shot consume). Under the SAME
    /// `live_friend_tokens` lock, atomically: if `token_sig` is present AND
    /// (`expires_at` is `None` OR `expires_at > now_ms`), REMOVE it from the map
    /// and return `true`; otherwise leave the map unchanged and return `false`.
    ///
    /// This closes a TOCTOU race in the inbound consent gate: a read-only
    /// `is_friend_token_active` check followed by a later removal lets two
    /// concurrent handshakes redeeming the same `token_sig` BOTH pass. Because the
    /// check-and-remove here happen under one lock with no `.await` in between,
    /// exactly one concurrent caller can observe the token live and consume it; all
    /// others see it already gone and get `false`. The read-only
    /// [`Self::is_friend_token_active`] is retained for tests / call-site symmetry.
    ///
    /// `async` for call-site symmetry with the other publisher methods; the map is
    /// a `std::sync::Mutex` and the lock is never held across an `.await`.
    pub async fn try_consume_friend_token(&self, token_sig: &[u8; 64], now_ms: u64) -> bool {
        let mut map = self
            .live_friend_tokens
            .lock()
            .expect("live_friend_tokens mutex poisoned");
        let live = match map.get(token_sig) {
            Some(None) => true,                             // registered, no expiry
            Some(Some(expires_at)) => *expires_at > now_ms, // registered, unexpired
            None => false,                                  // never registered / consumed
        };
        if live {
            // Atomic one-shot: remove under the same lock so a racing caller that
            // also saw it live cannot also consume it.
            map.remove(token_sig);
        }
        live
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
            PkarrRoutingRecord::sign_new(
                blob_builder(),
                id_pub,
                at_ms,
                at_ms + crate::reachability_record::REACHABILITY_RECORD_TTL_MS,
                &id_sk,
            )
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

    /// `register_invite` takes the invite token sig directly (community-invite
    /// payload knowledge lives at the composition root). `None` — an
    /// open-community invite — must publish nothing; `Some(sig)` must register
    /// the `invite:{hex}` case-A handle, and `unregister_invite` retires it.
    #[tokio::test]
    async fn register_invite_publishes_only_for_token_bearing_invites() {
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
            Arc::new(|| b"fake-iroh-routing".to_vec()),
        );

        // Open-community invite (no token) → no case-A publication at all.
        inv_pub.register_invite(None).await;
        assert!(
            publisher.active_handles().await.is_empty(),
            "open-community invite must not publish a case-A record"
        );

        let token_sig = [0x55u8; 64];
        let handle = format!("invite:{}", hex::encode(token_sig));

        inv_pub.register_invite(Some(token_sig)).await;
        assert!(
            publisher.active_handles().await.contains(&handle),
            "invite handle must be active after register"
        );

        inv_pub.unregister_invite(&token_sig).await;
        assert!(
            !publisher.active_handles().await.contains(&handle),
            "invite handle must be gone after unregister"
        );
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

        inv_pub.register_friend_token(&token_sig, None).await;
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

    /// FIX (consent-gate): `is_friend_token_active` is the authority the inbound
    /// friend acceptor consults before adding a friend — it must be true ONLY
    /// for a token this node registered (minted + published) and has not yet
    /// consumed, and only while unexpired.
    #[tokio::test]
    async fn is_friend_token_active_tracks_registration_and_expiry() {
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

        let never_registered = [0x01u8; 64];
        let unexpiring = [0x02u8; 64];
        let expiring = [0x03u8; 64];

        let now_ms = 1_000_000u64;

        // never-registered → false.
        assert!(
            !inv_pub
                .is_friend_token_active(&never_registered, now_ms)
                .await,
            "a token_sig this node never registered must not be active"
        );

        // registered with no expiry → active now and far in the future.
        inv_pub.register_friend_token(&unexpiring, None).await;
        assert!(
            inv_pub.is_friend_token_active(&unexpiring, now_ms).await,
            "a registered, non-expiring token must be active"
        );
        assert!(
            inv_pub
                .is_friend_token_active(&unexpiring, now_ms + 10_000_000)
                .await,
            "a non-expiring token stays active regardless of clock"
        );

        // registered with a future expiry → active before, inactive at/after.
        inv_pub
            .register_friend_token(&expiring, Some(now_ms + 5_000))
            .await;
        assert!(
            inv_pub.is_friend_token_active(&expiring, now_ms).await,
            "a registered token before its expiry must be active"
        );
        assert!(
            !inv_pub
                .is_friend_token_active(&expiring, now_ms + 5_000)
                .await,
            "a token at its expiry instant (expires_at <= now) must be inactive"
        );
        assert!(
            !inv_pub
                .is_friend_token_active(&expiring, now_ms + 6_000)
                .await,
            "a token past its expiry must be inactive"
        );

        // after unregister → false (consumed one-shot).
        inv_pub.unregister_friend_token(&unexpiring).await;
        assert!(
            !inv_pub.is_friend_token_active(&unexpiring, now_ms).await,
            "an unregistered (consumed) token must not be active"
        );
    }

    /// FIX (ZEB-370 cursor review — TOCTOU): `try_consume_friend_token` is the
    /// ATOMIC one-shot consume the inbound consent gate uses. The first call for a
    /// live, unexpired token returns `true` AND removes it from the live map; every
    /// subsequent call (and unknown / expired tokens) returns `false`.
    #[tokio::test]
    async fn try_consume_friend_token_is_one_shot() {
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

        let unknown = [0x01u8; 64];
        let unexpiring = [0x02u8; 64];
        let expiring = [0x03u8; 64];
        let now_ms = 1_000_000u64;

        // Unknown token → false (never registered).
        assert!(
            !inv_pub.try_consume_friend_token(&unknown, now_ms).await,
            "a token this node never registered must not be consumable"
        );

        // Registered + unexpired → first consume true, second false (one-shot).
        inv_pub.register_friend_token(&unexpiring, None).await;
        assert!(
            inv_pub.try_consume_friend_token(&unexpiring, now_ms).await,
            "the first consume of a live token must succeed"
        );
        assert!(
            !inv_pub.try_consume_friend_token(&unexpiring, now_ms).await,
            "the second consume of the same token must fail (already consumed)"
        );
        // Consuming removed it from the live map: the read-only check agrees.
        assert!(
            !inv_pub.is_friend_token_active(&unexpiring, now_ms).await,
            "a consumed token must no longer be active"
        );

        // Expired token → false, and it stays registered? It is removed only on a
        // successful consume; an expired token is NOT consumed (returns false).
        inv_pub.register_friend_token(&expiring, Some(now_ms)).await;
        assert!(
            !inv_pub.try_consume_friend_token(&expiring, now_ms).await,
            "a token at/after its expiry must not be consumable"
        );
    }

    /// FIX (ZEB-370 cursor review — TOCTOU concurrency): two concurrent handshakes
    /// redeeming the SAME live token must not both pass the gate. Spawn two tasks
    /// that both call `try_consume_friend_token` on the same registered token and
    /// assert EXACTLY ONE wins — the one-shot guarantee under concurrency.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn try_consume_friend_token_exactly_one_winner_under_concurrency() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_identity_pub(&sk);
        let inv_pub = Arc::new(PkarrInvitePublisher::new(
            publisher.clone(),
            sk,
            id_pub,
            Arc::new(|| b"friend-routing".to_vec()),
        ));

        let token_sig = [0x55u8; 64];
        let now_ms = 1_000_000u64;
        inv_pub.register_friend_token(&token_sig, None).await;

        let a = {
            let inv_pub = Arc::clone(&inv_pub);
            tokio::spawn(async move { inv_pub.try_consume_friend_token(&token_sig, now_ms).await })
        };
        let b = {
            let inv_pub = Arc::clone(&inv_pub);
            tokio::spawn(async move { inv_pub.try_consume_friend_token(&token_sig, now_ms).await })
        };
        let (ra, rb) = tokio::join!(a, b);
        let won_a = ra.expect("task a joined");
        let won_b = rb.expect("task b joined");

        assert!(
            won_a ^ won_b,
            "exactly one of the two concurrent consumers must win (got a={won_a}, b={won_b})"
        );
        // And the token is fully consumed afterward.
        assert!(
            !inv_pub.try_consume_friend_token(&token_sig, now_ms).await,
            "after the concurrent race the token must be consumed (no third winner)"
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
