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
//! successful vine publish (covers "first vine ever" flipping the gate OPEN)
//! and after every successful vine delete (covers "last vine gone" flipping it
//! CLOSED, so the published relay set is actively retracted rather than left
//! to the next cadence tick — ZEB-822).

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
    relay_set: Vec<VineRelayEntry>,
    now_ms: u64,
) -> Option<Vec<u8>> {
    if !share || own_vine_count == 0 {
        return None;
    }
    let payload = VineRelayRecordPayload {
        relay_set,
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

/// What the registered closure actually publishes on every tick. When sharing
/// is gated open, the aggregated `relay_set`; if that fails to encode — e.g. a
/// toxic oversized sibling `home_relay` blowing `VINES_RECORD_BLOB_MAX_BYTES` —
/// fall back to advertising SELF ONLY so this device keeps serving rather than
/// silently retracting ALL vine serving (ZEB-820). Gate-closed, or self-only
/// also failing to encode, yields the empty-set retraction. Pure — same
/// live-read inputs the closure captures, so a test can pin behavior without
/// any network/tokio machinery.
fn build_publish_blob(
    share: bool,
    own_vine_count: usize,
    relay_set: Vec<VineRelayEntry>,
    self_entry: &VineRelayEntry,
    now_ms: u64,
) -> Vec<u8> {
    if let Some(blob) = build_blob(share, own_vine_count, relay_set, now_ms) {
        return blob;
    }
    // `build_blob` returned None: either gated closed (share=false / no vines),
    // which must retract, or the aggregated set failed to encode, which warrants
    // a self-only retry so a single toxic sibling row can't suppress this
    // device's own serving.
    if share && own_vine_count > 0 {
        tracing::warn!(
            "ZEB-820: aggregated vine relay set failed to encode (likely \
             VINES_RECORD_BLOB_MAX_BYTES from a large sibling home_relay); \
             falling back to self-only"
        );
        if let Some(blob) = build_blob(share, own_vine_count, vec![self_entry.clone()], now_ms) {
            return blob;
        }
    }
    build_retraction_blob(now_ms)
}

/// Wall-clock now in ms. Test-only since ZEB-820: production publish ticks are
/// stamped by the core `PkarrPublisher` via the `at_ms` passed into the record
/// builder, so `reconcile_locked` no longer samples the clock itself.
#[cfg(test)]
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
    /// SP1 64-hex fleet-net device id of THIS device — the key form used in
    /// `FleetNetDoc::devices` and passed to `build_vine_relay_set` so self's
    /// snapshot row is replaced by the live self entry rather than duplicated.
    self_device_id: String,
    /// Reads a fresh `FleetNetDoc` snapshot on every publish tick (captures the
    /// live `Arc<RwLock<FleetNetDoc>>` in prod; `Arc::new(FleetNetDoc::default)`
    /// in tests → the set collapses to `[self]`, preserving pre-ZEB-820 shape).
    fleet_snapshot: Arc<dyn Fn() -> crate::fleet_net::FleetNetDoc + Send + Sync>,
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
        self_device_id: String,
        fleet_snapshot: Arc<dyn Fn() -> crate::fleet_net::FleetNetDoc + Send + Sync>,
    ) -> Self {
        Self {
            publisher,
            own_addr_hex,
            identity_signing_key,
            identity_pub,
            endpoint,
            share,
            has_own_vines,
            self_device_id,
            fleet_snapshot,
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
    /// path in `reconcile_locked` and the retraction path in
    /// `register_retraction`, so the retraction lands under the exact same
    /// slot it's withdrawing.
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
        let share = self.share.load(Ordering::Relaxed);
        let own_vine_count = (self.has_own_vines)();

        if share && own_vine_count > 0 {
            let id_sk = self.identity_signing_key.clone();
            let id_pub = self.identity_pub;
            let endpoint_for_builder = Arc::clone(&endpoint);
            let share_flag = Arc::clone(&self.share);
            let has_own_vines = Arc::clone(&self.has_own_vines);
            let fleet_snapshot = Arc::clone(&self.fleet_snapshot);
            let self_device_id = self.self_device_id.clone();
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
                // ZEB-820: aggregate self + freshest siblings from a fresh fleet
                // snapshot instead of advertising only self.
                let self_entry = VineRelayEntry {
                    iroh_endpoint_id: endpoint_id,
                    home_relay,
                };
                let relay_set = crate::fleet_net::build_vine_relay_set(
                    &(fleet_snapshot)(),
                    &self_device_id,
                    self_entry.clone(),
                    at_ms,
                );
                let blob = build_publish_blob(share, own_vine_count, relay_set, &self_entry, at_ms);
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

        // Gate closed. BOTH ways it can close actively retract rather than
        // merely unregistering — `unregister` alone only stops FUTURE
        // republishing, it does not withdraw a record already sitting on
        // the DHT, so the last positive relay-set would otherwise stay
        // discoverable for up to its 7-day TTL:
        //
        //  - `share == false`: the explicit settings toggle-off (round 1
        //    fix, Qodo #1).
        //  - `share == true` with zero own vines: the owner deleted their
        //    last vine and never touched settings (ZEB-822). Identical
        //    stale-record exposure, so identical remedy — this used to be a
        //    plain `unregister`, which stranded the last positive record to
        //    TTL decay.
        //
        // Either way there is only something to retract if THIS process
        // still holds the registration; otherwise any record on the DHT is
        // from an earlier process run we no longer publish for (the restart
        // hole — pre-existing on the disable path, accepted, now shared).
        // Note the early return replaces branch 4's old `unregister` call,
        // which was a no-op precisely because the handle is absent here.
        if !self
            .publisher
            .active_handles()
            .await
            .contains(&HANDLE.to_string())
        {
            return;
        }
        self.register_retraction().await;
    }

    /// Register the retraction-only publication: a `RecordBuilder` emitting
    /// the empty-relay-set record, replacing whatever positive record this
    /// device last published under the same slot. Shared by both gate-closed
    /// flavors in `reconcile_locked` so they cannot drift apart.
    ///
    /// NEVER pair this with `unregister`: `unregister` sets the entry's
    /// `cancelled` flag and the core publish loop short-circuits on it
    /// (`publisher.rs`), so register-then-unregister would publish nothing at
    /// all. The handle deliberately STAYS registered afterwards, republishing
    /// the retraction on the normal cadence.
    async fn register_retraction(&self) {
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

    fn self_entry() -> VineRelayEntry {
        VineRelayEntry {
            iroh_endpoint_id: TEST_SELF_ENDPOINT,
            home_relay: "https://relay.example".to_string(),
        }
    }

    fn self_relay_set() -> Vec<VineRelayEntry> {
        vec![self_entry()]
    }

    #[test]
    fn blob_absent_when_gate_off_or_no_vines() {
        assert!(build_blob(false, 3, self_relay_set(), 1_000).is_none());
        assert!(build_blob(true, 0, self_relay_set(), 1_000).is_none());
    }

    #[test]
    fn blob_encodes_given_relay_set_when_enabled() {
        let blob =
            build_blob(true, 3, self_relay_set(), 1_000).expect("enabled with vines publishes");
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
        let blob = build_publish_blob(
            /*share=*/ true,
            /*own_vine_count=*/ 0,
            self_relay_set(),
            &self_entry(),
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
        let blob = build_publish_blob(true, 3, self_relay_set(), &self_entry(), 1_000);
        let p: crate::pkarr_vines::VineRelayRecordPayload =
            ciborium::from_reader(blob.as_slice()).unwrap();
        assert_eq!(p.relay_set.len(), 1);
        assert_eq!(p.relay_set[0].iroh_endpoint_id, TEST_SELF_ENDPOINT);
    }

    /// ZEB-820 (Qodo #1): an oversized sibling `home_relay` blows
    /// `VINES_RECORD_BLOB_MAX_BYTES` for the aggregated set. `build_publish_blob`
    /// must fall back to self-only rather than silently retracting ALL serving.
    #[test]
    fn oversized_sibling_relay_falls_back_to_self_only() {
        let oversized = VineRelayEntry {
            iroh_endpoint_id: [0x99; 32],
            home_relay: "x".repeat(900), // > VINES_RECORD_BLOB_MAX_BYTES (700)
        };
        // Premise: the aggregated (self + oversized sibling) set fails to encode.
        assert!(
            build_blob(true, 3, vec![self_entry(), oversized.clone()], 1_000).is_none(),
            "premise: the oversized aggregated set must fail to encode"
        );
        // The fallback must publish self-only, NOT an empty-set retraction.
        let blob = build_publish_blob(true, 3, vec![self_entry(), oversized], &self_entry(), 1_000);
        let p: crate::pkarr_vines::VineRelayRecordPayload =
            ciborium::from_reader(blob.as_slice()).unwrap();
        assert_eq!(
            p.relay_set.len(),
            1,
            "fell back to self-only, not a retraction"
        );
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
        let publisher = single_relay_publisher(&relay);
        (publisher, relay)
    }

    /// A client whose pool contains exactly ONE relay.
    fn single_relay_client(
        relay: &harmony_pkarr::testing::MockPkarrRelay,
    ) -> Arc<harmony_pkarr::RelayClient> {
        Arc::new(harmony_pkarr::RelayClient::new(
            harmony_pkarr::RelayPool::new(vec![relay.base_url.clone()]),
        ))
    }

    /// A spawned publisher writing to exactly ONE relay. Each
    /// `MockPkarrRelay` keeps a single envelope per key (latest write wins),
    /// so two records competing for the SAME slot key need one relay each —
    /// see `squatted_slot_still_resolves_genuine_relay_set`.
    fn single_relay_publisher(
        relay: &harmony_pkarr::testing::MockPkarrRelay,
    ) -> Arc<PkarrPublisher> {
        let publisher = Arc::new(PkarrPublisher::new(single_relay_client(relay)));
        let _driver = Arc::clone(&publisher).spawn();
        publisher
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
            "self-device".to_string(),
            std::sync::Arc::new(crate::fleet_net::FleetNetDoc::default),
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
            "self-device".to_string(),
            std::sync::Arc::new(crate::fleet_net::FleetNetDoc::default),
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

    /// ZEB-822: deleting your last own vine while "share vines publicly" stays
    /// ON must ACTIVELY retract the published relay set, exactly as an explicit
    /// toggle-off does — not merely unregister the handle and leave the last
    /// positive record discoverable until its 7-day TTL runs out. This is the
    /// one gate-closing path with NO settings change and NO watcher behind it
    /// (see the module doc), so it is reached only when something calls
    /// `reconcile` — which is why ZEB-822 also wired the post-tombstone
    /// `republish()` hook in `delete_vine_impl`. Without that hook the
    /// already-registered gate-open builder would still self-heal, but only at
    /// its next cadence tick (up to ~3.5 days), not on the delete.
    ///
    /// What the assertion turns on: `Ok(vec![])` means a record IS present at
    /// the slot and decodes to an empty relay set — the retraction. An
    /// `Err("no vines record found for creator")` would mean absent/undecodable,
    /// which is the TTL-decay end state this test exists to rule out. So the
    /// poll must accept ONLY the `Ok`-and-empty shape.
    #[tokio::test]
    async fn zero_own_vines_with_share_on_retracts_instead_of_ttl_decay() {
        let (publisher, relay) = test_publisher().await;
        let resolver = harmony_pkarr::PkarrResolver::new(single_relay_client(&relay));

        let endpoint = test_endpoint().await;
        // Real identity, as in `disable_after_enable_publishes_retraction`:
        // `verify_vines_record` binds the record's identity pub to the claimed
        // creator address, so a placeholder hex address would make EVERY
        // resolve return `Err` and the empty-vs-absent distinction this test
        // turns on would be unobservable.
        let identity = crate::vine_signing::test_identity();
        let addr = crate::vine_signing::signer_address(&identity);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let count_for_closure = Arc::clone(&count);
        let vp = PkarrVinesPublisher::new(
            Arc::clone(&publisher),
            addr.clone(),
            crate::vine_signing::identity_signing_key(&identity),
            crate::vine_signing::identity_pub_64(&identity),
            Some(endpoint),
            Arc::new(AtomicBool::new(false)),
            Arc::new(move || count_for_closure.load(Ordering::Relaxed)),
            "self-device".to_string(),
            std::sync::Arc::new(crate::fleet_net::FleetNetDoc::default),
        );

        // Sharing on with one own vine: the real relay-set record lands.
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

        // The owner deletes their last vine. `share` is NEVER touched — the
        // count closure is the only input that changes. Calling `reconcile()`
        // directly stands in for the publisher's whole trigger set, every
        // member of which funnels through this same body: `delete_vine_impl`'s
        // post-tombstone `republish()` hook (the path that reaches THIS
        // scenario in production), `publish_vine_descriptor_impl`'s
        // post-publish hook, `set_vine_settings_impl`'s toggle, and boot
        // `enable()`.
        count.store(0, Ordering::Relaxed);
        vp.reconcile().await;

        // The resolvable record must become the empty-set retraction. Merely
        // unregistering would leave the stale POSITIVE record sitting on the
        // relay — this loop would then spin out and fail.
        let mut attempts = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            attempts += 1;
            assert!(
                attempts < 80,
                "retraction did not land after the last own vine disappeared"
            );
            if let Ok(relay_set) =
                crate::pkarr_vines::resolve_vine_relays(&resolver, &addr, now_ms()).await
            {
                if relay_set.is_empty() {
                    break;
                }
            }
        }

        // And the handle STAYS registered, like the disable path's: the
        // retraction is republished on the normal cadence, and a
        // register-then-unregister would have cancelled the pending publish
        // outright (nothing would ever land).
        assert!(
            publisher
                .active_handles()
                .await
                .contains(&HANDLE.to_string()),
            "the retraction publication must stay registered, not be unregistered"
        );
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
            "self-device".to_string(),
            std::sync::Arc::new(crate::fleet_net::FleetNetDoc::default),
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

    /// ZEB-817 wiring: a squatter record on one relay with a higher seq must
    /// not shadow the genuine relay-set record on another relay, because
    /// `resolve_vine_relays` now verifies per-candidate INSIDE the resolver
    /// (`resolve_window_freshest_with`) instead of verifying only the
    /// freshest-by-seq winner the resolver had already committed to.
    ///
    /// Why the squat is even possible: the vines slot key derives from the
    /// creator's PUBLIC address (`PkarrCase::Vines`), so anyone holding that
    /// address can compute the slot and publish a record there that passes
    /// the outer signature, its own inner signature AND freshness — the inner
    /// sig verifies against the record's own embedded `harmony_identity_pub`,
    /// which is self-certified. Only `verify_vines_record`'s identity-pub→
    /// address binding separates the squat from the genuine record, and that
    /// check used to run after the resolver had already picked one winner by
    /// seq (and pinned its seq-highwater + positive cache with it).
    #[tokio::test]
    async fn squatted_slot_still_resolves_genuine_relay_set() {
        const ATTACKER_ENDPOINT: [u8; 32] = [9u8; 32];

        let genuine_relay = harmony_pkarr::testing::MockPkarrRelay::start().await;
        let squat_relay = harmony_pkarr::testing::MockPkarrRelay::start().await;

        // The resolver under test spans BOTH relays — what a follower with a
        // multi-relay pool actually sees. Squat listed FIRST: it carries both
        // the higher seq and the earlier answer.
        let resolver = harmony_pkarr::PkarrResolver::new(Arc::new(
            harmony_pkarr::RelayClient::new(harmony_pkarr::RelayPool::new(vec![
                squat_relay.base_url.clone(),
                genuine_relay.base_url.clone(),
            ])),
        ));

        // The genuine creator must be a REAL identity: `verify_vines_record`
        // binds the record's identity pub to the claimed creator address, so
        // an arbitrary hex address (as the bookkeeping-only tests above use)
        // would fail the binding for EVERY candidate and prove nothing.
        let identity = crate::vine_signing::test_identity();
        let genuine_addr = crate::vine_signing::signer_address(&identity);
        let endpoint = test_endpoint().await;
        let genuine_endpoint_id = *endpoint.node_id().as_bytes();
        let vp = PkarrVinesPublisher::new(
            single_relay_publisher(&genuine_relay),
            genuine_addr.clone(),
            crate::vine_signing::identity_signing_key(&identity),
            crate::vine_signing::identity_pub_64(&identity),
            Some(endpoint),
            Arc::new(AtomicBool::new(false)),
            Arc::new(|| 1),
            "self-device".to_string(),
            std::sync::Arc::new(crate::fleet_net::FleetNetDoc::default),
        );
        vp.enable().await;

        // Wait for the genuine record to land BEFORE publishing the squat, so
        // the squat is guaranteed the higher BEP44 seq (seq is the
        // `SignedPacket` timestamp, minted at publish time).
        let mut attempts = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            attempts += 1;
            assert!(attempts < 80, "genuine vines publish did not land");
            if let Ok(relay_set) =
                crate::pkarr_vines::resolve_vine_relays(&resolver, &genuine_addr, now_ms()).await
            {
                if !relay_set.is_empty() {
                    break;
                }
            }
        }

        // The squatter: a DIFFERENT identity signing its own record, under
        // the SAME slot key (derived from the GENUINE creator's address), on
        // its own relay.
        let attacker = crate::vine_signing::test_identity();
        let attacker_sk = crate::vine_signing::identity_signing_key(&attacker);
        let attacker_pub = crate::vine_signing::identity_pub_64(&attacker);
        let addr_for_key = genuine_addr.clone();
        let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
            vines_key_for_epoch(&addr_for_key, current_epoch_id(at_ms))
                .expect("creator address hex comes from a real identity — always valid hex")
        });
        let record_builder: RecordBuilder = Arc::new(move |at_ms| {
            let blob = build_vines_record_blob(&VineRelayRecordPayload {
                relay_set: vec![VineRelayEntry {
                    iroh_endpoint_id: ATTACKER_ENDPOINT,
                    home_relay: "https://attacker.example".to_string(),
                }],
                issued_at_ms: at_ms,
            })
            .expect("single-entry blob is within budget");
            PkarrRoutingRecord::sign_new(
                blob,
                attacker_pub,
                at_ms,
                at_ms + REACHABILITY_RECORD_TTL_MS,
                &attacker_sk,
            )
            .expect("sign — fixed-size buffers should not fail")
        });
        let squat_publisher = single_relay_publisher(&squat_relay);
        squat_publisher
            .register("squat".to_string(), key_builder, record_builder)
            .await;

        // Wait for the squat's PUT to actually land. Probed through a
        // squat-relay-ONLY resolver — a separate instance, so its cache and
        // seq-highwater cannot contaminate the resolver under test — and via
        // raw `resolve_freshest`, so the wait is independent of whether
        // `resolve_vine_relays` filters the squat.
        let squat_probe = harmony_pkarr::PkarrResolver::new(single_relay_client(&squat_relay));
        let slot_key = vines_key_for_epoch(&genuine_addr, current_epoch_id(now_ms()))
            .expect("creator address hex is valid")
            .verifying_key();
        let mut attempts = 0;
        let squat_announced_at_ms = loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            attempts += 1;
            assert!(attempts < 80, "squat publish did not land");
            if let Ok(Some(rec)) = squat_probe.resolve_freshest(&slot_key).await {
                assert_eq!(
                    rec.harmony_identity_pub, attacker_pub,
                    "the squat relay must be holding the ATTACKER's record"
                );
                break rec.announced_at_ms;
            }
        };

        // Pin the premise AT assertion time (CodeRabbit PR #564 round 1):
        // the squat must actually outrank the genuine record, or a genuine
        // republish landing after the squat would let the assertions below
        // pass even with per-candidate verification reverted — a silent
        // false positive. BEP44 `seq` is not surfaced by the resolver API,
        // but seq and `announced_at_ms` are both minted from the
        // publish-time clock, and the wait loops above serialize the two
        // mints (the genuine record was OBSERVED landed before the squat
        // ever registered) — so strictly later announced_at pins the
        // strictly higher seq.
        let genuine_probe = harmony_pkarr::PkarrResolver::new(single_relay_client(&genuine_relay));
        let genuine_rec = genuine_probe
            .resolve_freshest(&slot_key)
            .await
            .expect("genuine-relay probe must not error")
            .expect("genuine relay must still hold the genuine record");
        assert_eq!(
            genuine_rec.harmony_identity_pub,
            crate::vine_signing::identity_pub_64(&identity),
            "the genuine relay must be holding the GENUINE record"
        );
        assert!(
            squat_announced_at_ms > genuine_rec.announced_at_ms,
            "premise: the squat record must be strictly fresher than the genuine \
             one (squat {squat_announced_at_ms} vs genuine {})",
            genuine_rec.announced_at_ms
        );

        // The squat is fresher by seq and answers first, but it fails the
        // address binding — the genuine relay set must still resolve.
        let relay_set = crate::pkarr_vines::resolve_vine_relays(&resolver, &genuine_addr, now_ms())
            .await
            .expect("genuine relay set must resolve despite the squatted slot");
        assert!(
            relay_set
                .iter()
                .any(|e| e.iroh_endpoint_id == genuine_endpoint_id),
            "genuine endpoint must be present in the resolved relay set"
        );
        assert!(
            relay_set
                .iter()
                .all(|e| e.iroh_endpoint_id != ATTACKER_ENDPOINT),
            "attacker endpoint must never appear in a resolved relay set"
        );
    }

    /// The other side of ZEB-822's retraction: sharing ON with zero own vines
    /// from a FRESH state (nothing ever registered by this process) must
    /// register nothing — neither the positive record nor a retraction. A
    /// retraction here would be this process publishing a record for a slot it
    /// never published to, purely as a side effect of the user enabling a
    /// setting; the active-handle check in `reconcile_locked` is what prevents
    /// it. Uses a real identity so the "resolve stays Err" half is meaningful
    /// (a placeholder address fails `verify_vines_record`'s binding and would
    /// return `Err` even if a record HAD been published).
    #[tokio::test]
    async fn enable_does_not_register_without_own_vines() {
        let (publisher, relay) = test_publisher().await;
        let resolver = harmony_pkarr::PkarrResolver::new(single_relay_client(&relay));
        let endpoint = test_endpoint().await;
        let identity = crate::vine_signing::test_identity();
        let addr = crate::vine_signing::signer_address(&identity);
        let vp = PkarrVinesPublisher::new(
            Arc::clone(&publisher),
            addr.clone(),
            crate::vine_signing::identity_signing_key(&identity),
            crate::vine_signing::identity_pub_64(&identity),
            Some(endpoint),
            Arc::new(AtomicBool::new(false)),
            Arc::new(|| 0),
            "self-device".to_string(),
            std::sync::Arc::new(crate::fleet_net::FleetNetDoc::default),
        );

        vp.enable().await;
        assert!(!publisher
            .active_handles()
            .await
            .contains(&HANDLE.to_string()));

        // Give the background publish driver a window to issue any PUT a
        // wrongly-registered builder would have produced, then confirm the
        // slot is genuinely empty — not merely that bookkeeping looks right.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            crate::pkarr_vines::resolve_vine_relays(&resolver, &addr, now_ms())
                .await
                .is_err(),
            "nothing was ever published for this slot — enabling with zero vines \
             must not publish a retraction for a record that does not exist"
        );
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
            "self-device".to_string(),
            std::sync::Arc::new(crate::fleet_net::FleetNetDoc::default),
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
            "self-device".to_string(),
            std::sync::Arc::new(crate::fleet_net::FleetNetDoc::default),
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
            "self-device".to_string(),
            std::sync::Arc::new(crate::fleet_net::FleetNetDoc::default),
        );

        vp.enable().await;
        assert!(publisher.active_handles().await.is_empty());
    }

    /// ZEB-820: with a fleet snapshot carrying fresh siblings, the published
    /// record resolves to the AGGREGATED set (self + siblings), not just self.
    #[tokio::test]
    async fn aggregated_set_includes_fresh_siblings() {
        let (publisher, relay) = test_publisher().await;
        let resolver = harmony_pkarr::PkarrResolver::new(single_relay_client(&relay));

        let endpoint = test_endpoint().await;
        let self_endpoint_id = *endpoint.node_id().as_bytes();
        let identity = crate::vine_signing::test_identity();
        let addr = crate::vine_signing::signer_address(&identity);

        // Two fresh siblings (ids differ from the publisher's self device id).
        const SIB_A: [u8; 32] = [0xA1; 32];
        const SIB_B: [u8; 32] = [0xB2; 32];
        let fleet = std::sync::Arc::new(move || {
            let now = now_ms();
            let mut doc = crate::fleet_net::FleetNetDoc::default();
            let mk = |ep: [u8; 32], relay: &str| crate::fleet_net::FleetNetRow {
                iroh_endpoint_id: ep,
                home_relay: relay.to_string(),
                seen_at: crate::owner_state_types::Hlc {
                    wall_ms: now,
                    logical: 0,
                    device_id: String::new(),
                },
                feed_binding: None,
            };
            doc.devices
                .insert("sib-a".to_string(), mk(SIB_A, "https://a.example"));
            doc.devices
                .insert("sib-b".to_string(), mk(SIB_B, "https://b.example"));
            doc
        });

        let vp = PkarrVinesPublisher::new(
            std::sync::Arc::clone(&publisher),
            addr.clone(),
            crate::vine_signing::identity_signing_key(&identity),
            crate::vine_signing::identity_pub_64(&identity),
            Some(endpoint),
            std::sync::Arc::new(AtomicBool::new(false)),
            std::sync::Arc::new(|| 1),
            "self-device".to_string(),
            fleet,
        );

        vp.enable().await;

        let mut attempts = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            attempts += 1;
            assert!(attempts < 80, "aggregated vines publish did not land");
            if let Ok(relay_set) =
                crate::pkarr_vines::resolve_vine_relays(&resolver, &addr, now_ms()).await
            {
                if relay_set.iter().any(|e| e.iroh_endpoint_id == SIB_A)
                    && relay_set.iter().any(|e| e.iroh_endpoint_id == SIB_B)
                {
                    // Self is force-included too — the publisher's live endpoint.
                    assert_eq!(relay_set.len(), 3, "self + 2 siblings");
                    assert!(
                        relay_set
                            .iter()
                            .any(|e| e.iroh_endpoint_id == self_endpoint_id),
                        "the live publisher endpoint must be included"
                    );
                    return;
                }
            }
        }
    }
}
