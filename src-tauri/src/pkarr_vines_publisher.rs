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
use crate::pkarr_vines::{
    build_vines_record_blob, vines_key_for_epoch, VineRelayEntry, VineRelayRecordPayload,
};
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

/// Explicit empty-relay-set retraction, published when the gate has flipped
/// closed since registration (e.g. the owner's last vine was deleted
/// without an explicit settings toggle — no separate watcher for that path
/// in v1, see module doc). `unwrap_or_default` on the encode is safe: an
/// empty `relay_set` can never exceed `pkarr_vines`'s blob size budget.
fn build_retraction_blob(now_ms: u64) -> Vec<u8> {
    build_vines_record_blob(&VineRelayRecordPayload {
        relay_set: vec![],
        issued_at_ms: now_ms,
    })
    .unwrap_or_default()
}

/// What the registered closure actually publishes on every tick: the real
/// self-entry blob when the gate is open, or the retraction above when it
/// isn't. Pure — same live-read inputs the closure captures, so a test can
/// pin "the next closure invocation" behavior without any network/tokio
/// machinery.
fn build_blob_or_retraction(
    share: bool,
    own_vine_count: usize,
    endpoint_id: [u8; 32],
    home_relay: String,
    now_ms: u64,
) -> Vec<u8> {
    build_blob(share, own_vine_count, endpoint_id, home_relay, now_ms)
        .unwrap_or_else(|| build_retraction_blob(now_ms))
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

    /// Mark sharing off. `unregister` alone only stops FUTURE republishing —
    /// it does not withdraw a record already sitting on the DHT, so the last
    /// positive relay-set would otherwise stay discoverable for up to its
    /// 7-day TTL after the user disables sharing. If the handle is currently
    /// registered (there is something to retract), replace its content with
    /// the explicit empty-`relay_set` retraction instead: re-registering
    /// schedules an immediate publish tick (`PkarrPublisher::register`'s doc
    /// comment) and leaves the handle registered afterward, since
    /// unregistering right after would race that in-flight publish's
    /// `cancelled` check and could cancel it before the PUT executes.
    /// `enable`/`republish` overwrite this with real content again on the
    /// next toggle. If nothing was ever registered, this is a safe no-op
    /// (nothing to retract), matching the prior behavior. Called from
    /// `set_vine_settings_impl` when the user flips the toggle off.
    pub async fn disable(&self) {
        self.share.store(false, Ordering::Relaxed);

        if !self
            .publisher
            .active_handles()
            .await
            .contains(&HANDLE.to_string())
        {
            return;
        }

        let id_sk = self.identity_signing_key.clone();
        let id_pub = self.identity_pub;
        let record_builder: RecordBuilder = Arc::new(move |at_ms| {
            PkarrRoutingRecord::sign_new(
                build_retraction_blob(at_ms),
                id_pub,
                at_ms,
                at_ms + REACHABILITY_RECORD_TTL_MS,
                &id_sk,
            )
            .expect("sign — fixed-size buffers should not fail")
        });

        self.publisher
            .register(HANDLE.to_string(), self.key_builder(), record_builder)
            .await;
    }

    /// Shared ephemeral-key builder: derives the current epoch's vines slot
    /// key from this device's own address. Used by both the real-content
    /// path (`sync_registration`) and the retraction path (`disable`) so the
    /// retraction lands under the exact same slot it's withdrawing.
    fn key_builder(&self) -> EphemeralKeyBuilder {
        let own_addr_for_key = self.own_addr_hex.clone();
        Arc::new(move |at_ms| {
            let epoch_id = current_epoch_id(at_ms);
            vines_key_for_epoch(&own_addr_for_key, epoch_id)
                .expect("own address hex is derived from our own identity — always valid hex")
        })
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
            let blob =
                build_blob_or_retraction(share, own_vine_count, endpoint_id, home_relay, at_ms);
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
            .register(HANDLE.to_string(), self.key_builder(), record_builder)
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

    /// Pins the gate-flip-closed scenario the registered `RecordBuilder`
    /// closure must handle without a `disable`/`republish` call landing
    /// first (e.g. the owner's last vine was deleted, dropping
    /// `own_vine_count` to 0, with `share` never touched): the closure
    /// calls `build_blob_or_retraction` with these exact live-read inputs
    /// on its next invocation, so exercising the pure combinator directly
    /// pins that behavior without any network/tokio machinery.
    #[test]
    fn retraction_blob_when_gate_flips_closed_after_registration() {
        let blob = build_blob_or_retraction(
            /*share=*/ true,
            /*own_vine_count=*/ 0,
            TEST_SELF_ENDPOINT,
            "https://relay.example".to_string(),
            1_000,
        );
        let p: crate::pkarr_vines::VineRelayRecordPayload =
            ciborium::from_reader(blob.as_slice()).unwrap();
        assert!(
            p.relay_set.is_empty(),
            "gate closed — must retract, never republish stale self-entry content"
        );
        assert_eq!(p.issued_at_ms, 1_000);
    }

    /// Same combinator, gate-open branch: must match `build_blob`'s output
    /// exactly (the closure's normal-path behavior), not the retraction.
    #[test]
    fn full_blob_when_gate_open() {
        let blob = build_blob_or_retraction(
            true,
            3,
            TEST_SELF_ENDPOINT,
            "https://relay.example".to_string(),
            1_000,
        );
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
        // Disable actively retracts (Qodo finding, ZEB-811 review): the
        // handle stays registered — now emitting the empty-relay-set
        // retraction rather than being unregistered outright, which would
        // leave the last positive record resolvable until its 7-day TTL.
        // `disable_after_enable_publishes_retraction` below proves the
        // resolvable content is actually the retraction, not just that the
        // handle is still registered.
        assert!(publisher
            .active_handles()
            .await
            .contains(&HANDLE.to_string()));
    }

    /// Proves the actual fix, not just the registration-bookkeeping side
    /// effect above: after `enable` publishes a real relay-set record and
    /// `disable` is called, a follower resolving the creator's vines slot
    /// must see the empty-relay-set retraction, not the stale positive
    /// record riding out its TTL.
    #[tokio::test]
    async fn disable_after_enable_publishes_retraction() {
        let (publisher, relay) = test_publisher().await;
        let pool = harmony_pkarr::RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(harmony_pkarr::RelayClient::new(pool));
        let resolver = Arc::new(harmony_pkarr::PkarrResolver::new(client));

        let endpoint = test_endpoint().await;
        // `verify_vines_record` binds the record's identity pub to the
        // CLAIMED creator address (`address_for_identity_pub_hex`), so
        // unlike the bookkeeping-only tests above, `addr` here must be a
        // REAL derived address for the signing identity — an arbitrary
        // hex string (like the bookkeeping tests use) would fail that
        // binding check and every resolve would return `Err`, not the
        // positive-then-retraction sequence this test exercises.
        let identity = crate::vine_signing::test_identity();
        let addr = crate::vine_signing::signer_address(&identity);
        let sk = crate::vine_signing::identity_signing_key(&identity);
        let id_pub = crate::vine_signing::identity_pub_64(&identity);
        let vp = PkarrVinesPublisher::new(
            Arc::clone(&publisher),
            addr.clone(),
            sk,
            id_pub,
            Some(endpoint),
            Arc::new(AtomicBool::new(false)),
            Arc::new(|| 1),
        );

        vp.enable().await;

        // Wait for the positive record to land, so the retraction we wait
        // for next is a genuine overwrite rather than a race with the very
        // first publish.
        let mut attempts = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            attempts += 1;
            assert!(attempts < 80, "initial vines publish did not land");
            if let Ok(relay_set) =
                crate::pkarr_vines::resolve_vine_relays(&resolver, &addr, now_ms()).await
            {
                if !relay_set.is_empty() {
                    break;
                }
            }
        }

        vp.disable().await;

        // Poll until the resolved record is the empty-relay-set retraction.
        let mut attempts = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            attempts += 1;
            assert!(attempts < 80, "retraction did not land after disable");
            if let Ok(relay_set) =
                crate::pkarr_vines::resolve_vine_relays(&resolver, &addr, now_ms()).await
            {
                if relay_set.is_empty() {
                    return;
                }
            }
        }
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
