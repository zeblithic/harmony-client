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
    /// Mirrors `NodeState.vine_share_publicly` — and IS the exact same
    /// atomic `ProdVineRelayServeCtx::share_gate` reads live on every
    /// anonymous relay request (wired up at construction in `lib.rs`).
    /// Round 2 fix (Greptile P1): `set_desired_share` writes this
    /// SYNCHRONOUSLY from `set_vine_settings_impl`, before the detached
    /// `reconcile()` task is even spawned — so local serving reflects a
    /// settings toggle immediately, and `reconcile` (which re-reads this
    /// value fresh at execution time, never a captured parameter) always
    /// converges on the latest desired value no matter which detached
    /// reconcile task happens to finish its network I/O last. See
    /// `reconcile`'s doc comment for the full race analysis.
    share: Arc<AtomicBool>,
    has_own_vines: Arc<dyn Fn() -> usize + Send + Sync>,
    /// Serializes `reconcile()` bodies. Without this, two detached
    /// settings-toggle tasks (or a toggle racing the post-publish
    /// `republish` hook) could interleave their `PkarrPublisher::register`/
    /// `unregister` calls and leave the DHT publication inconsistent with
    /// the (already-correct) `share` atomic — Greptile P1, ZEB-811 round 2.
    reconcile_lock: tokio::sync::Mutex<()>,
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
            reconcile_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Mark sharing on and reconcile. Safe to use as a single-shot call
    /// (e.g. at boot, from the loaded setting) where there is no concurrent
    /// settings toggle to race — `set_vine_settings_impl` (the seam that
    /// DOES race) calls `set_desired_share` + `reconcile` directly instead,
    /// so the synchronous atomic write happens before ANY detached task is
    /// spawned; see those methods' doc comments.
    pub async fn enable(&self) {
        self.set_desired_share(true);
        self.reconcile().await;
    }

    /// Mark sharing off and reconcile — see `enable`'s doc comment for the
    /// single-shot-caller caveat.
    pub async fn disable(&self) {
        self.set_desired_share(false);
        self.reconcile().await;
    }

    /// Write the desired share-publicly value to the shared atomic
    /// SYNCHRONOUSLY (no `.await`). This is the SAME `Arc<AtomicBool>`
    /// `ProdVineRelayServeCtx::share_gate` reads live on every anonymous
    /// relay request, so a toggle takes effect for local SERVING
    /// immediately — before `reconcile`'s (network-bound) pkarr
    /// publish/retract has even started, let alone finished.
    ///
    /// This is also the linchpin of `reconcile`'s race-freedom (ZEB-811
    /// round 2, Greptile P1): `set_vine_settings_impl` calls this
    /// synchronously, then spawns a detached `reconcile()` task. Two rapid
    /// settings calls therefore always apply their `set_desired_share`
    /// writes in true call order (both happen under `NodeState`'s mutex,
    /// never interleaved) — so by the time EITHER detached reconcile task
    /// starts running, `share` already holds the FINAL desired value, no
    /// matter which task was spawned first or which one's network I/O
    /// happens to finish last.
    pub fn set_desired_share(&self, desired: bool) {
        self.share.store(desired, Ordering::Relaxed);
    }

    /// Re-evaluate the gate against the CURRENT `share` flag (re-read
    /// HERE — under the lock, at execution time — never a value captured
    /// when a caller's task was spawned) and own vine count, and
    /// register/retract/unregister accordingly. Serialized by
    /// `reconcile_lock`: two detached settings-toggle tasks (or a toggle
    /// racing the post-publish `republish` hook) can therefore never
    /// interleave their `PkarrPublisher::register`/`unregister` calls.
    ///
    /// Why this is race-free regardless of task ordering: `set_desired_share`
    /// always completes synchronously before its caller spawns a
    /// `reconcile()` task, so by the time ANY reconcile body runs, `share`
    /// already holds the latest setting. Whichever reconcile happens to
    /// acquire `reconcile_lock` FIRST applies that (already-correct) value;
    /// every subsequent reconcile — including one spawned earlier but
    /// scheduled later — reads the SAME value and reapplies it redundantly
    /// (idempotent, harmless). The convergent outcome depends only on the
    /// latest `set_desired_share` call, never on which task's network I/O
    /// happens to finish last.
    pub async fn reconcile(&self) {
        let _guard = self.reconcile_lock.lock().await;
        self.reconcile_locked().await;
    }

    /// Shared ephemeral-key builder: derives the current epoch's vines slot
    /// key from this device's own address. Used by both the real-content
    /// and retraction paths inside `reconcile_locked` so the retraction
    /// lands under the exact same slot it's withdrawing.
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
        self.reconcile().await;
    }

    /// The actual reconcile body — only ever called with `reconcile_lock`
    /// held (via `reconcile`). Never call this directly.
    async fn reconcile_locked(&self) {
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

        if build_blob(share, own_vine_count, endpoint_id, home_relay, now_ms).is_some() {
            let id_sk = self.identity_signing_key.clone();
            let id_pub = self.identity_pub;
            let endpoint_for_builder = Arc::clone(&endpoint);
            let share_flag = Arc::clone(&self.share);
            let has_own_vines = Arc::clone(&self.has_own_vines);
            let record_builder: RecordBuilder = Arc::new(move |at_ms| {
                // Fresh read on EVERY publish (never boot-frozen — ZEB-521):
                // a home relay captured once at register time would go
                // stale for the life of the process.
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
            return;
        }

        // Gate closed. `share == false` (explicit toggle-off) actively
        // retracts a previously-registered positive record instead of
        // merely unregistering — `unregister` alone only stops FUTURE
        // republishing, it does not withdraw a record already sitting on
        // the DHT, so the last positive relay-set would otherwise stay
        // discoverable for up to its 7-day TTL (round 1 fix, Qodo #1,
        // preserved here). `share == true` but no own vines (yet, or not
        // anymore) has nothing meaningful to retract in v1 — plain
        // unregister is enough, unchanged from the original behavior.
        if share {
            self.publisher.unregister(HANDLE).await;
            return;
        }
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

    /// ZEB-811 round 2 (Greptile P1): a detached settings-toggle reconcile
    /// racing another must converge on whichever setting was applied LAST —
    /// a stale `enable`'s reconcile finishing AFTER a later `disable`'s
    /// must never reopen serving.
    ///
    /// Modeled deterministically, without sleeps or real thread-scheduling
    /// races: `reconcile()` takes no parameter and always re-reads `share`
    /// at ITS OWN execution time, so the only thing that determines its
    /// outcome is the ORDER reconcile CALLS actually *execute* in relative
    /// to `share`'s writes — never which settings call originally
    /// triggered them, and never genuine OS-thread interleaving. So this
    /// test directly encodes "the disable's own reconcile runs first, then
    /// a stale/delayed reconcile — standing in for whatever earlier
    /// `enable` call queued it — finally gets its turn LAST" simply by
    /// invoking `reconcile()` in that exact sequence. (An earlier version
    /// of this test tried to force a genuine race via `tokio::spawn` on a
    /// multi-thread runtime; it was flaky by construction — both
    /// `set_desired_share` writes complete so much faster than a freshly
    /// spawned task can be scheduled that neither task reliably observed
    /// the transient `true` value, so nothing was ever registered for the
    /// "retraction" to land on. Sequencing the calls directly is both
    /// deterministic and sufficient: it exercises the exact same read-path
    /// `reconcile_locked` takes regardless of how it was invoked.)
    #[tokio::test]
    async fn stale_reconcile_never_reopens_serving_after_a_later_disable() {
        let (publisher, relay) = test_publisher().await;
        let pool = harmony_pkarr::RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(harmony_pkarr::RelayClient::new(pool));
        let resolver = Arc::new(harmony_pkarr::PkarrResolver::new(client));

        let endpoint = test_endpoint().await;
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

        // Sharing was already on (an earlier settings state) — establish
        // and confirm real content is registered and resolvable.
        vp.enable().await;
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

        // The LATEST settings call disables sharing: its synchronous write
        // (mirrors `set_vine_settings_impl`) plus its own detached
        // reconcile both run, retracting the real content.
        vp.set_desired_share(false);
        vp.reconcile().await;
        let mut attempts = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            attempts += 1;
            assert!(attempts < 80, "retraction did not land after disable");
            if let Ok(relay_set) =
                crate::pkarr_vines::resolve_vine_relays(&resolver, &addr, now_ms()).await
            {
                if relay_set.is_empty() {
                    break;
                }
            }
        }

        // A STALE reconcile — standing in for a duplicate/delayed
        // reconcile task from the EARLIER `enable` that only gets its turn
        // now, well after the disable above already landed — must NOT
        // re-register real content just because it "was" spawned back
        // when sharing was on. `reconcile()` has nothing to remember: it
        // only ever reads current state, which is still `false`.
        vp.reconcile().await;

        // Give the mock relay a moment to reflect any (incorrect) PUT the
        // stale reconcile might have issued, then assert it's STILL the
        // retraction.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let relay_set = crate::pkarr_vines::resolve_vine_relays(&resolver, &addr, now_ms())
            .await
            .expect("record must still resolve (the handle stays registered post-retraction)");
        assert!(
            relay_set.is_empty(),
            "a stale reconcile must never reopen serving after a later disable"
        );
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
