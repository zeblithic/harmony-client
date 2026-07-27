//! Case E publisher — publishes this device's own vine relay-set record
//! (self iroh endpoint + home relay) under the vines pkarr slot
//! (`harmony_pkarr::PkarrCase::Vines`) when the user opts in via "Share
//! vines publicly" AND has at least one own published vine.
//!
//! **v1 simplification (deliberate):** unlike the reachability flavor
//! (case B/C/D), there is no separate network-change watcher here. The
//! record carries only an iroh endpoint id + home relay URL — both stable
//! across address churn, since iroh re-resolves direct addresses under the
//! same endpoint id. Freshness comes entirely from the core
//! `PkarrPublisher`'s own scheduled cadence (`compute_next_publish_at`) plus
//! the explicit [`PkarrVinesPublisher::republish`] hook fired after every
//! successful vine publish (covers "first vine ever" flipping the gate).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use harmony_pkarr::{
    current_epoch_id, EphemeralKeyBuilder, PkarrPublisher, PkarrRoutingRecord, RecordBuilder,
};

use crate::iroh_endpoint::IrohEndpoint;
use crate::pkarr_vines::{build_vines_record_blob, vines_key_for_epoch, VineRelayEntry, VineRelayRecordPayload};
use crate::reachability_record::REACHABILITY_RECORD_TTL_MS;

/// Publication handle for the case-E vines record.
pub(crate) const HANDLE: &str = "vines";

/// Pure gate + payload builder: `None` when the record should not be
/// published (sharing off, or nothing to advertise yet), `Some(blob)`
/// otherwise. Called both directly by tests and, with live values, by the
/// registered closure — see the module doc for why re-invoking this on
/// every publish tick (rather than gating once at registration) is safe.
fn build_blob(
    share: bool,
    own_vine_count: usize,
    endpoint_id: [u8; 32],
    home_relay: String,
    now_ms: u64,
) -> Option<Vec<u8>> {
    if !share || own_vine_count == 0 {
        return None;
    }
    let payload = VineRelayRecordPayload {
        relay_set: vec![VineRelayEntry {
            iroh_endpoint_id: endpoint_id,
            home_relay,
        }],
        issued_at_ms: now_ms,
    };
    build_vines_record_blob(&payload).ok()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub struct PkarrVinesPublisher {
    publisher: Arc<PkarrPublisher>,
    own_addr_hex: String,
    identity_signing_key: ed25519_dalek::SigningKey,
    identity_pub: [u8; 64],
    /// `None` when no iroh endpoint is up (degraded boot) — nothing dialable
    /// to advertise, so the gate never passes regardless of settings/vines.
    endpoint: Option<Arc<IrohEndpoint>>,
    /// Mirrors `NodeState.vine_share_publicly`; kept in sync by `enable`/
    /// `disable` (called from `set_vine_settings_impl`'s detached toggle)
    /// and re-read fresh on every publish tick so a settings flip that
    /// races an in-flight publish still degrades gracefully.
    share: Arc<AtomicBool>,
    has_own_vines: Arc<dyn Fn() -> usize + Send + Sync>,
}

impl PkarrVinesPublisher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        publisher: Arc<PkarrPublisher>,
        own_addr_hex: String,
        identity_signing_key: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        endpoint: Option<Arc<IrohEndpoint>>,
        share: Arc<AtomicBool>,
        has_own_vines: Arc<dyn Fn() -> usize + Send + Sync>,
    ) -> Self {
        Self {
            publisher,
            own_addr_hex,
            identity_signing_key,
            identity_pub,
            endpoint,
            share,
            has_own_vines,
        }
    }

    /// Mark sharing on and (re-)register iff the gate passes (own vine
    /// count > 0). Called at boot when the loaded setting is true, and from
    /// `set_vine_settings_impl` when the user flips the toggle on.
    pub async fn enable(&self) {
        self.share.store(true, Ordering::Relaxed);
        self.sync_registration().await;
    }

    /// Mark sharing off and unregister unconditionally. Called from
    /// `set_vine_settings_impl` when the user flips the toggle off.
    pub async fn disable(&self) {
        self.share.store(false, Ordering::Relaxed);
        self.publisher.unregister(HANDLE).await;
    }

    /// Re-evaluate the gate against the CURRENT `share` flag + own vine
    /// count and register/unregister accordingly. Equivalent to `enable`
    /// minus forcing `share` on — used after every successful
    /// `publish_vine_descriptor` so "first vine ever" flips the has-vines
    /// gate without waiting for the next scheduled cadence tick. A
    /// re-register on an already-registered, already-gated-open handle is a
    /// cheap no-op (replaces the map entry with an equivalent one and wakes
    /// the core publish loop early — `publisher.rs:97`).
    pub async fn republish(&self) {
        self.sync_registration().await;
    }

    async fn sync_registration(&self) {
        let Some(endpoint) = self.endpoint.clone() else {
            self.publisher.unregister(HANDLE).await;
            return;
        };
        let now_ms = now_ms();
        let endpoint_id = *endpoint.node_id().as_bytes();
        let home_relay = endpoint
            .home_relay()
            .map(|r| r.to_string())
            .unwrap_or_default();
        let share = self.share.load(Ordering::Relaxed);
        let own_vine_count = (self.has_own_vines)();

        if build_blob(share, own_vine_count, endpoint_id, home_relay, now_ms).is_none() {
            self.publisher.unregister(HANDLE).await;
            return;
        }

        let own_addr_for_key = self.own_addr_hex.clone();
        let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
            let epoch_id = current_epoch_id(at_ms);
            vines_key_for_epoch(&own_addr_for_key, epoch_id)
                .expect("own address hex is derived from our own identity — always valid hex")
        });

        let id_sk = self.identity_signing_key.clone();
        let id_pub = self.identity_pub;
        let endpoint_for_builder = Arc::clone(&endpoint);
        let share_flag = Arc::clone(&self.share);
        let has_own_vines = Arc::clone(&self.has_own_vines);
        let record_builder: RecordBuilder = Arc::new(move |at_ms| {
            // Fresh read on EVERY publish (never boot-frozen — ZEB-521): a
            // home relay captured once at register time would go stale for
            // the life of the process.
            let endpoint_id = *endpoint_for_builder.node_id().as_bytes();
            let home_relay = endpoint_for_builder
                .home_relay()
                .map(|r| r.to_string())
                .unwrap_or_default();
            let share = share_flag.load(Ordering::Relaxed);
            let own_vine_count = has_own_vines();
            let blob = build_blob(share, own_vine_count, endpoint_id, home_relay, at_ms)
                .unwrap_or_else(|| {
                    // The gate flipped closed between registration and this
                    // scheduled tick (e.g. the owner's last vine was deleted
                    // without an explicit settings toggle — no separate
                    // watcher exists for that in v1, see module doc).
                    // Publish an explicit empty-set retraction rather than
                    // stale self-entry content.
                    build_vines_record_blob(&VineRelayRecordPayload {
                        relay_set: vec![],
                        issued_at_ms: at_ms,
                    })
                    .unwrap_or_default()
                });
            PkarrRoutingRecord::sign_new(
                blob,
                id_pub,
                at_ms,
                at_ms + REACHABILITY_RECORD_TTL_MS,
                &id_sk,
            )
            .expect("sign — fixed-size buffers should not fail")
        });

        self.publisher
            .register(HANDLE.to_string(), key_builder, record_builder)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SELF_ENDPOINT: [u8; 32] = [7u8; 32];

    fn test_builder(share: bool, own_vine_count: usize) -> impl Fn() -> Option<Vec<u8>> {
        move || {
            build_blob(
                share,
                own_vine_count,
                TEST_SELF_ENDPOINT,
                "https://relay.example".to_string(),
                1_000,
            )
        }
    }

    #[test]
    fn blob_absent_when_gate_off_or_no_vines() {
        let b = test_builder(/*share=*/ false, /*own_vines=*/ 3);
        assert!(b().is_none());
        let b = test_builder(true, 0);
        assert!(b().is_none());
    }

    #[test]
    fn blob_contains_self_entry_when_enabled() {
        let b = test_builder(true, 3);
        let blob = b().expect("enabled with vines publishes");
        let p: crate::pkarr_vines::VineRelayRecordPayload =
            ciborium::from_reader(blob.as_slice()).unwrap();
        assert_eq!(p.relay_set.len(), 1);
        assert_eq!(p.relay_set[0].iroh_endpoint_id, TEST_SELF_ENDPOINT);
    }

    async fn test_endpoint() -> Arc<IrohEndpoint> {
        Arc::new(
            crate::iroh_endpoint::new_with_secret_and_relays_hermetic_dns(
                iroh::SecretKey::generate(),
                None,
            )
            .await
            .expect("bind hermetic test endpoint"),
        )
    }

    /// Returns the publisher alongside the mock relay (kept alive by the
    /// caller — these tests only assert on `active_handles`, never a real
    /// network PUT, but a dropped relay pointlessly noises up the logs each
    /// time the background driver retries against a closed listener).
    async fn test_publisher() -> (Arc<PkarrPublisher>, harmony_pkarr::testing::MockPkarrRelay) {
        let relay = harmony_pkarr::testing::MockPkarrRelay::start().await;
        let pool = harmony_pkarr::RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(harmony_pkarr::RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _driver = Arc::clone(&publisher).spawn();
        (publisher, relay)
    }

    fn build_id_pub(sk: &ed25519_dalek::SigningKey) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        out
    }

    #[tokio::test]
    async fn enable_registers_when_own_vines_exist() {
        let (publisher, _relay) = test_publisher().await;
        let endpoint = test_endpoint().await;
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let vp = PkarrVinesPublisher::new(
            Arc::clone(&publisher),
            "aabbcc".to_string(),
            sk.clone(),
            build_id_pub(&sk),
            Some(endpoint),
            Arc::new(AtomicBool::new(false)),
            Arc::new(|| 1),
        );

        vp.enable().await;
        assert!(publisher
            .active_handles()
            .await
            .contains(&HANDLE.to_string()));

        vp.disable().await;
        assert!(!publisher
            .active_handles()
            .await
            .contains(&HANDLE.to_string()));
    }

    #[tokio::test]
    async fn enable_does_not_register_without_own_vines() {
        let (publisher, _relay) = test_publisher().await;
        let endpoint = test_endpoint().await;
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let vp = PkarrVinesPublisher::new(
            Arc::clone(&publisher),
            "aabbcc".to_string(),
            sk.clone(),
            build_id_pub(&sk),
            Some(endpoint),
            Arc::new(AtomicBool::new(false)),
            Arc::new(|| 0),
        );

        vp.enable().await;
        assert!(!publisher
            .active_handles()
            .await
            .contains(&HANDLE.to_string()));
    }

    #[tokio::test]
    async fn republish_registers_after_first_vine_flips_gate() {
        let (publisher, _relay) = test_publisher().await;
        let endpoint = test_endpoint().await;
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_for_closure = Arc::clone(&count);
        let vp = PkarrVinesPublisher::new(
            Arc::clone(&publisher),
            "aabbcc".to_string(),
            sk.clone(),
            build_id_pub(&sk),
            Some(endpoint),
            Arc::new(AtomicBool::new(false)),
            Arc::new(move || count_for_closure.load(Ordering::Relaxed)),
        );

        vp.enable().await;
        assert!(
            !publisher
                .active_handles()
                .await
                .contains(&HANDLE.to_string()),
            "no vines yet — gate must stay closed"
        );

        // First vine ever published.
        count.store(1, Ordering::Relaxed);
        vp.republish().await;
        assert!(publisher
            .active_handles()
            .await
            .contains(&HANDLE.to_string()));
    }

    #[tokio::test]
    async fn disable_when_not_enabled_is_safe() {
        let (publisher, _relay) = test_publisher().await;
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let vp = PkarrVinesPublisher::new(
            Arc::clone(&publisher),
            "aabbcc".to_string(),
            sk.clone(),
            build_id_pub(&sk),
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(|| 1),
        );

        vp.disable().await;
        assert!(publisher.active_handles().await.is_empty());
    }

    #[tokio::test]
    async fn no_endpoint_never_registers() {
        let (publisher, _relay) = test_publisher().await;
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let vp = PkarrVinesPublisher::new(
            Arc::clone(&publisher),
            "aabbcc".to_string(),
            sk.clone(),
            build_id_pub(&sk),
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(|| 3),
        );

        vp.enable().await;
        assert!(publisher.active_handles().await.is_empty());
    }
}
