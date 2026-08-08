//! Case B publisher — publishes alice's iroh routing under HKDF(owner_pub, epoch)
//! when user opts in via "Make me discoverable" toggle. Persisted via ConnectivitySettings.

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

/// Publication handle for the case-B identity record. Exposed `pub(crate)`
/// so the Network Health self-test (ZEB-385) can check "is the identity
/// publication active?" against this single source of truth instead of a
/// duplicated string literal.
pub(crate) const HANDLE: &str = "identity";

/// ZEB-879: observed result of a runtime "Make me discoverable" toggle.
///
/// The runtime toggle used to call [`PkarrIdentityPublisher::enable`] /
/// [`disable`][PkarrIdentityPublisher::disable] fire-and-forget with no log on
/// either path, so a stalled enable was indistinguishable from a working one —
/// the "silently unreachable with no error at all" symptom (ZEB-879). The
/// caller now drives an explicit info/warn log off this outcome, and tests pin
/// the behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleOutcome {
    /// Enabled AND the case-B publication is registered with the driver.
    EnabledActive,
    /// Enabled but the publication is NOT registered afterwards — a wiring
    /// regression; the node may stay unreachable despite the setting being on.
    EnabledInactive,
    /// Disabled and the publication is unregistered.
    Disabled,
}

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
            PkarrRoutingRecord::sign_new(
                blob_builder(),
                id_pub,
                at_ms,
                at_ms + crate::reachability_record::REACHABILITY_RECORD_TTL_MS,
                &id_sk,
            )
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

    /// Whether this device's identity (case-B) publication is currently
    /// registered with the pkarr driver.
    ///
    /// Reads the driver's active handle set — the SAME "is it publishing?"
    /// source of truth the ZEB-385 Network Health self-test uses — rather than a
    /// duplicated flag that could drift from the driver's real state.
    pub async fn is_active(&self) -> bool {
        self.publisher
            .active_handles()
            .await
            .iter()
            .any(|h| h == HANDLE)
    }

    /// Apply a runtime discoverability toggle and report the observed outcome so
    /// the caller can log it (ZEB-879 — the toggle was previously silent).
    ///
    /// On enable, verifies the publication registered. Registration completes
    /// synchronously inside [`enable`][Self::enable] (`register` inserts under
    /// the driver's state lock before returning), so a single post-enable
    /// [`is_active`][Self::is_active] check is authoritative — no polling window
    /// is needed. An `EnabledInactive` result therefore signals a genuine wiring
    /// regression, not a not-yet-settled race.
    pub async fn toggle_and_verify(&self, enabled: bool) -> ToggleOutcome {
        if !enabled {
            self.disable().await;
            return ToggleOutcome::Disabled;
        }
        self.enable().await;
        if self.is_active().await {
            ToggleOutcome::EnabledActive
        } else {
            ToggleOutcome::EnabledInactive
        }
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

    #[tokio::test]
    async fn is_active_reflects_registration() {
        // ZEB-879: `is_active` is the ZEB-385 "is case-B publishing?" source of
        // truth (the driver's active handle set) — false before enable, true
        // after enable, false again after disable.
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_id_pub(&sk);
        let p = PkarrIdentityPublisher::new(
            Arc::clone(&publisher),
            sk,
            id_pub,
            Arc::new(|| b"fake-routing".to_vec()),
        );

        assert!(!p.is_active().await, "inactive before enable");
        p.enable().await;
        assert!(p.is_active().await, "active after enable");
        p.disable().await;
        assert!(!p.is_active().await, "inactive after disable");
    }

    #[tokio::test]
    async fn toggle_and_verify_enable_reports_active() {
        // ZEB-879: the runtime toggle was previously fire-and-forget + silent;
        // `toggle_and_verify(true)` confirms the publication registered so the
        // caller can log the outcome instead of leaving the node silently
        // (un)reachable.
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_id_pub(&sk);
        let p = PkarrIdentityPublisher::new(
            Arc::clone(&publisher),
            sk,
            id_pub,
            Arc::new(|| b"fake-routing".to_vec()),
        );

        assert_eq!(
            p.toggle_and_verify(true).await,
            ToggleOutcome::EnabledActive
        );
        assert!(p.is_active().await);
    }

    #[tokio::test]
    async fn toggle_and_verify_disable_reports_disabled() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_id_pub(&sk);
        let p = PkarrIdentityPublisher::new(
            Arc::clone(&publisher),
            sk,
            id_pub,
            Arc::new(|| b"fake-routing".to_vec()),
        );

        p.enable().await;
        assert_eq!(p.toggle_and_verify(false).await, ToggleOutcome::Disabled);
        assert!(!p.is_active().await);
    }
}
