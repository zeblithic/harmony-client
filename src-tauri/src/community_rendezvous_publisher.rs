//! Beacon-side publish of the community rendezvous slot record. A member that
//! is a relay advertiser at rank i < N publishes its reachability under
//! `rendezvous_slot_key(epoch_key, i, epoch)`. Reuses the same routing blob and
//! pkarr register/unregister seam as the member-keyed community publisher
//! ([`crate::pkarr_community_publisher::PkarrCommunityPublisher`]).
//!
//! The slot key is re-derived on EVERY publish (inside the `key_builder`
//! closure) so it tracks the current epoch, exactly as the member-keyed
//! community publisher does — registering once at the start of epoch N would
//! otherwise keep publishing under the epoch-N key after the boundary while
//! resolvers query under epoch N±1.
//!
//! # Self-promotion: pure decision now, driver deferred
//!
//! [`should_self_promote`] is the PURE, deterministic decision an under-filled
//! community uses to elect a fill-in beacon (lowest-ranked eligible online
//! candidate, power-aware). The periodic auto-promotion DRIVER — the background
//! task that gathers the live online/power state, calls [`should_self_promote`],
//! and triggers [`CommunityRendezvousPublisher::refresh_slot`] — is an
//! INTENTIONAL follow-up, not built here. The spec frames self-promotion
//! convergence (how aggressively to promote, how to damp oscillation) as an
//! OPEN QUESTION to tune from observed data, and the shipping relay-advertiser
//! opt-in (ZEB-380) already fills slots whenever any advertiser is online. The
//! [`RendezvousObservability`] counters exist precisely to inform that future
//! driver's tuning (promotion rate, slot-fill latency, demotions).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

use harmony_pkarr::{
    current_epoch_id, EphemeralKeyBuilder, PkarrPublisher, PkarrRoutingRecord, RecordBuilder,
};

use crate::community_rendezvous::{
    rendezvous_slot_key, slot_for_advertiser_diverse, RENDEZVOUS_SLOT_COUNT,
};
use crate::owner_state_types::{EpochKey, OwnerAddr, SpaceId};

/// Abstraction over the pkarr publish registry used by the rendezvous
/// publisher. The real [`PkarrPublisher`] (held behind an `Arc`) implements it
/// in production; tests substitute a spy that records the calls. Mirrors the
/// concrete `PkarrPublisher::register`/`unregister` signatures + closure type
/// aliases verbatim so the production path is a thin pass-through.
#[async_trait::async_trait]
pub trait RendezvousSink: Send + Sync {
    async fn register(
        &self,
        handle: String,
        key_builder: EphemeralKeyBuilder,
        builder: RecordBuilder,
    );
    async fn unregister(&self, handle: &str);
}

#[async_trait::async_trait]
impl RendezvousSink for PkarrPublisher {
    async fn register(
        &self,
        handle: String,
        key_builder: EphemeralKeyBuilder,
        builder: RecordBuilder,
    ) {
        PkarrPublisher::register(self, handle, key_builder, builder).await;
    }

    async fn unregister(&self, handle: &str) {
        PkarrPublisher::unregister(self, handle).await;
    }
}

/// Publishes this node's iroh reachability under the community rendezvous slot
/// it currently claims (by advertiser rank). Construct it with the SAME inputs
/// the member-keyed [`crate::pkarr_community_publisher::PkarrCommunityPublisher`]
/// takes, so both publish the identical routing blob via the identical
/// `sign_new` seam.
pub struct CommunityRendezvousPublisher {
    sink: Arc<dyn RendezvousSink>,
    identity_signing_key: ed25519_dalek::SigningKey,
    identity_pub: [u8; 64],
    /// ZEB-827: the node's enrolled community device signing key, used to mint
    /// the membership vouch carried in the rendezvous blob.
    device_signing_key: Arc<ed25519_dalek::SigningKey>,
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    /// Currently-registered rendezvous slot per community, so a rank change
    /// unregisters the stale slot handle before registering the new one.
    registered_slots: Mutex<HashMap<SpaceId, u16>>,
    /// Counters feeding the (deferred) self-promotion driver's tuning. Bumped
    /// whenever a slot is newly filled by [`Self::refresh_slot`].
    observability: RendezvousObservability,
    /// ZEB-891 (CodeRabbit/Qodo #646): edge-trigger flag for the over-budget
    /// warning — warn once on the false→true transition (re-armed on recovery)
    /// so a persistent oversized-fixed-fields misconfig doesn't re-log every
    /// refresh cycle × community. Node-global: the size-driving fixed fields
    /// (relay URL + butler set) are the same across communities. `Arc` so it can
    /// be cloned into the per-slot `RecordBuilder` closure (which is `move`).
    over_budget_warned: Arc<std::sync::atomic::AtomicBool>,
}

impl CommunityRendezvousPublisher {
    /// Production constructor — mirrors `PkarrCommunityPublisher::new` so the
    /// same `(publisher, identity_signing_key, identity_pub, routing_blob_builder)`
    /// inputs build both publishers at boot.
    pub fn new(
        publisher: Arc<PkarrPublisher>,
        identity_signing_key: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        device_signing_key: Arc<ed25519_dalek::SigningKey>,
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    ) -> Self {
        Self::new_with_sink(
            publisher,
            identity_signing_key,
            identity_pub,
            device_signing_key,
            routing_blob_builder,
        )
    }

    /// Construct over any [`RendezvousSink`] (the real `PkarrPublisher` in
    /// production, a spy in tests).
    pub fn new_with_sink(
        sink: Arc<dyn RendezvousSink>,
        identity_signing_key: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        device_signing_key: Arc<ed25519_dalek::SigningKey>,
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    ) -> Self {
        Self {
            sink,
            identity_signing_key,
            identity_pub,
            device_signing_key,
            routing_blob_builder,
            registered_slots: Mutex::new(HashMap::new()),
            observability: RendezvousObservability::default(),
            over_budget_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Observability counters (promotions / slot-fills / slot-fill latency /
    /// demotions). Read by the deferred self-promotion driver and by
    /// [`RendezvousObservability::log`] to inform convergence tuning.
    pub fn observability(&self) -> &RendezvousObservability {
        &self.observability
    }

    /// Emit a `tracing::info!` reading the current observability counters. The
    /// (deferred) self-promotion driver calls this periodically; it also keeps
    /// every counter on a read path so none is dead code under `-D warnings`.
    pub fn log_observability(&self) {
        self.observability.log();
    }

    fn slot_handle(community_id: &SpaceId, slot: u16) -> String {
        format!("rendezvous:{}:{}", hex::encode(community_id.0), slot)
    }

    /// (Re)compute this node's rendezvous slot for `community_id` from the
    /// current advertiser set and publish accordingly:
    ///
    /// - `slot_for_advertiser_diverse(&advertisers, &me, N) == Some(slot)` →
    ///   register a pkarr publication keyed by `rendezvous_slot_key(epoch_key,
    ///   slot, current_epoch_id(now))` under handle `rendezvous:<cid>:<slot>`. If
    ///   the node previously held a *different* slot for this community, that
    ///   stale handle is unregistered first.
    /// - `None` (not an advertiser, or ranks ≥ cap after diversity interleave) →
    ///   unregister any rendezvous handle this node previously registered.
    ///
    /// ZEB-940: `advertisers` carries each advertiser's `(owner address,
    /// diversity bucket)` so the slot claim spreads beacons across relay regions
    /// rather than clustering by address. Re-derives the slot key on every
    /// publish (in the `key_builder` closure) so it tracks the epoch, mirroring
    /// the member-keyed community publisher.
    pub async fn refresh_slot(
        &self,
        community_id: SpaceId,
        epoch_key: EpochKey,
        advertisers: Vec<(OwnerAddr, String)>,
        me: OwnerAddr,
    ) {
        // Decision + map mutation happen UNDER the lock (fast, no await);
        // the awaited register/unregister run OUTSIDE it so a slow pkarr op on
        // one community can't head-of-line-block refreshes for every other
        // community. The lock guards only the `registered_slots` map.
        let mut opt_unregister_handle: Option<String> = None;
        let mut opt_register_slot: Option<u16> = None;
        let mut did_fill = false;
        {
            let mut registered = self.registered_slots.lock().await;
            let prior_slot = registered.get(&community_id).copied();
            match slot_for_advertiser_diverse(&advertisers, &me, RENDEZVOUS_SLOT_COUNT) {
                Some(slot) => {
                    // A rank change must drop the stale slot handle before the
                    // new one is registered (each slot has a single writer).
                    if let Some(old) = prior_slot {
                        if old != slot {
                            opt_unregister_handle = Some(Self::slot_handle(&community_id, old));
                        }
                    }
                    // Always (re-)register the current slot — this is the
                    // periodic-refresh mechanism, so we re-register even when
                    // the slot is unchanged.
                    opt_register_slot = Some(slot);
                    // Count a slot-fill only when this newly registers a slot
                    // (no prior slot, or a rank change) — not on a no-op
                    // re-register of the same slot we already hold.
                    if prior_slot != Some(slot) {
                        did_fill = true;
                    }
                    registered.insert(community_id, slot);
                }
                None => {
                    if let Some(old) = prior_slot {
                        opt_unregister_handle = Some(Self::slot_handle(&community_id, old));
                        registered.remove(&community_id);
                    }
                }
            }
        } // drop the guard before any await

        if let Some(handle) = opt_unregister_handle {
            self.sink.unregister(&handle).await;
            // A relinquished slot (rank change to a different slot, or opt-out to
            // None) is a demotion — record it so the (deferred) self-promotion
            // driver's tuning counter isn't dead.
            self.observability.record_demotion();
        }

        if let Some(slot) = opt_register_slot {
            // Clone the epoch-key bytes into the closure so the slot key is
            // re-derived against the live epoch on every publish.
            let epoch_key_bytes = *epoch_key.as_bytes();
            let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
                let epoch_id = current_epoch_id(at_ms);
                let ek = EpochKey::new(epoch_key_bytes);
                rendezvous_slot_key(&ek, slot, epoch_id)
            });

            let id_sk = self.identity_signing_key.clone();
            let id_pub = self.identity_pub;
            let device_sk = Arc::clone(&self.device_signing_key);
            let vouch_community = community_id;
            let blob_builder = Arc::clone(&self.routing_blob_builder);
            let over_budget_warned = Arc::clone(&self.over_budget_warned);
            let builder: RecordBuilder = Arc::new(move |at_ms| {
                let base = blob_builder();
                let ttl_at = at_ms + crate::reachability_record::REACHABILITY_RECORD_TTL_MS;
                // ZEB-827: wrap the reachability payload with a fresh membership
                // vouch signed by our enrolled community device key. If the base
                // blob can't be decoded (e.g. empty — no endpoint yet), publish
                // it unchanged (a vouchless blob; strict resolvers skip it, old
                // ones still read it).
                let routing = match ciborium::from_reader::<
                    crate::reachability_record::ReachabilityAnnouncePayload,
                    _,
                >(base.as_slice())
                {
                    Ok(mut reach) => {
                        // ZEB-880: the rendezvous blob uniquely also carries a
                        // MembershipVouch, so a full 2-entry butler_set (~290 B)
                        // cannot fit under pkarr's size cap at any address count.
                        // Cap it to ONE seal target rather than strip it — the
                        // gateway dial driver seeds this payload into the
                        // ReachabilityResolver (PkarrLive), and butler_deposit
                        // reads that butler_set for offline-DM, so a full strip
                        // would strand seal targets for rendezvous-discovered
                        // peers. Then bound the addresses (reserving envelope +
                        // the "mv" vouch) so the signed record fits.
                        crate::reachability_bound::cap_butler_set(&mut reach, 1);
                        let bound = crate::reachability_bound::bound_record_payload(
                            &mut reach,
                            crate::reachability_bound::RECORD_ENVELOPE_BYTES
                                + crate::reachability_bound::RENDEZVOUS_VOUCH_BYTES,
                        );
                        if bound.dropped > 0 {
                            tracing::debug!(
                                dropped = bound.dropped,
                                kept = reach.direct_addresses.len(),
                                "ZEB-880: trimmed rendezvous direct addresses to fit the pkarr record size cap"
                            );
                        }
                        if bound.over_budget {
                            // ZEB-891: even after capping butler to 1 and trimming
                            // every address, the rendezvous record (fixed fields +
                            // the "mv" membership vouch) still exceeds the cap. It
                            // fails `RecordTooLarge` every publish cycle → this node
                            // is undiscoverable via community rendezvous. No trim can
                            // recover it; surface it so an operator can shorten the
                            // configured relay URL. Edge-triggered (warn once per
                            // false→true transition) so a persistent misconfig
                            // doesn't flood the log every refresh cycle × community.
                            if !over_budget_warned.swap(true, std::sync::atomic::Ordering::Relaxed)
                            {
                                tracing::warn!(
                                    home_relay_url_len = reach.home_relay_url.len(),
                                    butler_entries = reach.butler_set.len(),
                                    max_record_cbor_bytes =
                                        crate::reachability_bound::MAX_RECORD_CBOR_BYTES,
                                    "ZEB-891: rendezvous reachability record still exceeds the \
                                     pkarr size cap after capping butler and trimming ALL direct \
                                     addresses — its fixed fields plus the membership vouch are \
                                     too large. It will fail to publish (RecordTooLarge) every \
                                     cycle, leaving this node undiscoverable via community \
                                     rendezvous. Shorten the configured relay URL or reduce the \
                                     butler/fleet set."
                                );
                            }
                        } else {
                            // Under budget: re-arm so a later breakage warns again.
                            over_budget_warned.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                        let vouch = crate::membership_vouch::mint_membership_vouch(
                            &device_sk,
                            vouch_community,
                            &id_pub,
                            at_ms,
                            ttl_at,
                        );
                        crate::community_rendezvous::encode_rendezvous_blob(&reach, Some(&vouch))
                    }
                    Err(_) => base,
                };
                PkarrRoutingRecord::sign_new(routing, id_pub, at_ms, ttl_at, &id_sk)
                    .expect("sign — fixed-size buffers should not fail")
            });

            let handle = Self::slot_handle(&community_id, slot);
            self.sink.register(handle, key_builder, builder).await;
            if did_fill {
                self.observability.record_slot_fill(0);
            }
        }
    }
}

/// Coarse power/availability class of a candidate beacon, used by
/// [`should_self_promote`] to prefer always-on nodes over battery-constrained
/// ones. Mirrors the relay/butler opt-in's availability notion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerTier {
    /// Desktop / opted-in / on AC power — preferred for promotion.
    HighAvailability,
    /// Default class with no special availability signal.
    Normal,
    /// Mobile / battery — defers; promotes only as a last resort.
    LowPower,
}

/// Promote `me` only if the live slot set is under-filled AND `me` is the
/// lowest-ranked eligible online candidate. Eligibility is power-aware: prefer
/// HighAvailability, then Normal; LowPower promotes only when no better
/// candidate exists. Ranking among a tier is by address (same as slot claim),
/// so the decision is deterministic and convergent without coordination.
///
/// PURE decision only — the periodic driver that gathers live online/power
/// state and calls [`CommunityRendezvousPublisher::refresh_slot`] is a deferred
/// follow-up (see module docs).
pub fn should_self_promote(
    _advertisers: &[OwnerAddr],
    live_slot_count: usize,
    candidates_ranked: &[(OwnerAddr, PowerTier)],
    me: &OwnerAddr,
) -> bool {
    if live_slot_count >= RENDEZVOUS_SLOT_COUNT {
        return false;
    }
    fn tier_rank(t: PowerTier) -> u8 {
        match t {
            PowerTier::HighAvailability => 0,
            PowerTier::Normal => 1,
            PowerTier::LowPower => 2,
        }
    }
    // Best tier present among online candidates.
    let Some(best_tier) = candidates_ranked.iter().map(|(_, t)| tier_rank(*t)).min() else {
        return false;
    };
    // Lowest-address candidate within the best tier is the promoter.
    let promoter = candidates_ranked
        .iter()
        .filter(|(_, t)| tier_rank(*t) == best_tier)
        .map(|(a, _)| *a)
        .min_by(|x, y| x.0.cmp(&y.0));
    promoter.map(|p| p.0 == me.0).unwrap_or(false)
}

/// Observability counters for rendezvous slot dynamics. These feed the
/// (deferred) self-promotion driver's convergence/oscillation tuning — see the
/// module docs. Counters are read by [`Self::log`] (and via
/// [`CommunityRendezvousPublisher::observability`]), so none is dead code.
#[derive(Default)]
pub struct RendezvousObservability {
    /// Times this node self-promoted into an under-filled slot.
    pub promotions: AtomicU64,
    /// Times this node newly registered (filled) a rendezvous slot.
    pub slot_fills: AtomicU64,
    /// Per-fill latency samples in ms (now − under-filled-observed-at).
    pub slot_fill_latency_ms: Mutex<Vec<u64>>,
    /// Times this node relinquished a slot (rank loss / dropping out).
    pub demotions: AtomicU64,
}

impl RendezvousObservability {
    /// Record a self-promotion event (an under-filled slot we elected to fill).
    pub fn record_promotion(&self) {
        self.promotions.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a newly-filled slot plus its fill latency sample.
    pub fn record_slot_fill(&self, latency_ms: u64) {
        self.slot_fills.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut samples) = self.slot_fill_latency_ms.try_lock() {
            samples.push(latency_ms);
        }
    }

    /// Record a demotion (slot relinquished).
    pub fn record_demotion(&self) {
        self.demotions.fetch_add(1, Ordering::Relaxed);
    }

    /// Emit a `tracing::info!` summarizing the counters. Reads every field so
    /// the struct can never be flagged dead under `-D warnings`.
    pub fn log(&self) {
        let promotions = self.promotions.load(Ordering::Relaxed);
        let slot_fills = self.slot_fills.load(Ordering::Relaxed);
        let demotions = self.demotions.load(Ordering::Relaxed);
        let latency_samples = self
            .slot_fill_latency_ms
            .try_lock()
            .map(|s| s.len())
            .unwrap_or(0);
        tracing::info!(
            promotions,
            slot_fills,
            demotions,
            latency_samples,
            "rendezvous self-promotion observability"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[derive(Clone)]
    struct Registration {
        handle: String,
        // ZEB-827: the built record's inspectable fields (the mock runs the
        // RecordBuilder at a fixed at_ms so tests can assert the blob/vouch).
        routing_blob: Vec<u8>,
        harmony_identity_pub: [u8; 64],
        announced_at_ms: u64,
    }

    const MOCK_BUILD_AT_MS: u64 = 1_700_000_000_000;

    #[derive(Default)]
    struct SpyInner {
        registrations: Vec<Registration>,
        unregistrations: Vec<String>,
    }

    /// Records `register`/`unregister` calls so the slot-claim behavior can be
    /// asserted without a live relay or DHT.
    #[derive(Default)]
    struct MockPublisher {
        inner: Mutex<SpyInner>,
    }

    impl MockPublisher {
        async fn registrations(&self) -> Vec<Registration> {
            self.inner.lock().await.registrations.clone()
        }
        async fn unregistrations(&self) -> Vec<String> {
            self.inner.lock().await.unregistrations.clone()
        }
        async fn last_registration(&self) -> Option<Registration> {
            self.inner.lock().await.registrations.last().cloned()
        }
    }

    #[async_trait::async_trait]
    impl RendezvousSink for MockPublisher {
        async fn register(
            &self,
            handle: String,
            _key_builder: EphemeralKeyBuilder,
            builder: RecordBuilder,
        ) {
            // Run the RecordBuilder at a fixed instant so tests can inspect the
            // published record (blob + vouch) deterministically.
            let rec = builder(MOCK_BUILD_AT_MS);
            self.inner.lock().await.registrations.push(Registration {
                handle,
                routing_blob: rec.routing_blob,
                harmony_identity_pub: rec.harmony_identity_pub,
                announced_at_ms: rec.announced_at_ms,
            });
        }
        async fn unregister(&self, handle: &str) {
            self.inner
                .lock()
                .await
                .unregistrations
                .push(handle.to_string());
        }
    }

    fn build_id_pub(sk: &SigningKey) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        out
    }

    fn publisher_for(spy: Arc<MockPublisher>) -> CommunityRendezvousPublisher {
        let sk = SigningKey::from_bytes(&[11u8; 32]);
        let id_pub = build_id_pub(&sk);
        CommunityRendezvousPublisher::new_with_sink(
            spy,
            sk,
            id_pub,
            Arc::new(SigningKey::from_bytes(&[42u8; 32])),
            Arc::new(|| b"routing".to_vec()),
        )
    }

    /// Build a minimal valid `ReachabilityAnnouncePayload` blob for the routing
    /// blob builder (so the vouch-wrapping path is exercised, not the fallback).
    fn reachability_blob() -> Vec<u8> {
        let p = crate::reachability_record::ReachabilityAnnouncePayload {
            iroh_node_id: [1u8; 32],
            home_relay_url: String::new(),
            direct_addresses: vec![],
            announced_at_ms: 1_000,
            identity_signature: [0u8; 64],
            butler_set: vec![],
            bs_at: 0,
        };
        let mut b = Vec::new();
        ciborium::into_writer(&p, &mut b).unwrap();
        b
    }

    /// AVALON's overflowing shape: 2 IPv4 + 3 global IPv6 direct addresses and
    /// a full 2-entry butler set (ZEB-880 repro).
    fn avalon_reachability_blob() -> Vec<u8> {
        let butler = |s: u8| crate::reachability_record::ButlerSetEntry {
            device_id: [s; 16],
            iroh_endpoint_id: [s.wrapping_add(1); 32],
            device_ed25519_verify: [s.wrapping_add(2); 32],
            home_relay: "https://usw1-1.relay.n0.iroh.link/".to_string(),
            pinned: false,
        };
        let p = crate::reachability_record::ReachabilityAnnouncePayload {
            iroh_node_id: [3u8; 32],
            home_relay_url: "https://usw1-1.relay.n0.iroh.link/".to_string(),
            direct_addresses: vec![
                "165.162.82.51:35102".parse().unwrap(),
                "192.168.1.59:63933".parse().unwrap(),
                "[2603:8002:ddf0:3380::1787]:63934".parse().unwrap(),
                "[2603:8002:ddf0:3380:6b34:be5b:30f8:5f6e]:63934"
                    .parse()
                    .unwrap(),
                "[2603:8002:ddf0:3380:bc5e:7e59:7bfb:19a6]:63934"
                    .parse()
                    .unwrap(),
            ],
            announced_at_ms: 1_000,
            identity_signature: [0u8; 64],
            butler_set: vec![butler(0x10), butler(0x20)],
            bs_at: 1_000,
        };
        let mut b = Vec::new();
        ciborium::into_writer(&p, &mut b).unwrap();
        b
    }

    /// ZEB-880 regression: an AVALON-shaped reachability blob (butler + 5 addrs)
    /// overflowed pkarr's size cap as a rendezvous record. The publisher must
    /// cap the butler set (keeping a seal target for the offline-DM seed path)
    /// and bound addresses so the built record fits — retaining the vouch, one
    /// butler entry, and at least one dial address.
    #[tokio::test]
    async fn refresh_slot_bounds_oversized_record_to_fit() {
        use crate::community_rendezvous::decode_rendezvous_blob;
        use crate::reachability_bound::{MAX_RECORD_CBOR_BYTES, RECORD_ENVELOPE_BYTES};

        // Guard the premise: the un-fixed record (butler + 5 addrs + vouch +
        // envelope) genuinely overflows.
        let unfixed = avalon_reachability_blob();
        assert!(
            unfixed.len()
                + crate::reachability_bound::RENDEZVOUS_VOUCH_BYTES
                + RECORD_ENVELOPE_BYTES
                > MAX_RECORD_CBOR_BYTES,
            "premise: the un-fixed AVALON rendezvous record must overflow"
        );

        let spy = Arc::new(MockPublisher::default());
        let device_sk = SigningKey::from_bytes(&[42u8; 32]);
        let id_sk = SigningKey::from_bytes(&[11u8; 32]);
        let id_pub = build_id_pub(&id_sk);
        let publisher = CommunityRendezvousPublisher::new_with_sink(
            spy.clone(),
            id_sk,
            id_pub,
            Arc::new(device_sk),
            Arc::new(avalon_reachability_blob),
        );

        let cid = SpaceId([7u8; 16]);
        let me = OwnerAddr([5u8; 16]);
        publisher
            .refresh_slot(cid, EpochKey::new([2u8; 32]), vec![(me, String::new())], me)
            .await;

        let reg = spy
            .last_registration()
            .await
            .expect("a record was registered");
        // The full record CBOR = routing_blob + envelope. It must fit the cap.
        assert!(
            reg.routing_blob.len() + RECORD_ENVELOPE_BYTES <= MAX_RECORD_CBOR_BYTES,
            "rendezvous record still over budget: blob {} + envelope {} > {}",
            reg.routing_blob.len(),
            RECORD_ENVELOPE_BYTES,
            MAX_RECORD_CBOR_BYTES
        );
        let (payload, vouch) = decode_rendezvous_blob(&reg.routing_blob).expect("blob decodes");
        assert!(vouch.is_some(), "vouch still present");
        assert_eq!(
            payload.butler_set.len(),
            1,
            "butler_set capped to one seal target for the offline-DM seed path"
        );
        assert!(
            !payload.direct_addresses.is_empty(),
            "at least one dial address retained"
        );
    }

    #[tokio::test]
    async fn refresh_slot_emits_vouch_verifiable_under_device_key() {
        use crate::community_rendezvous::decode_rendezvous_blob;
        use crate::membership_vouch::verify_membership_vouch;
        use std::collections::HashSet;

        let spy = Arc::new(MockPublisher::default());
        let device_sk = SigningKey::from_bytes(&[42u8; 32]);
        let id_sk = SigningKey::from_bytes(&[11u8; 32]);
        let id_pub = build_id_pub(&id_sk);
        let publisher = CommunityRendezvousPublisher::new_with_sink(
            spy.clone(),
            id_sk,
            id_pub,
            Arc::new(device_sk.clone()),
            Arc::new(reachability_blob),
        );

        let cid = SpaceId([7u8; 16]);
        let me = OwnerAddr([5u8; 16]);
        // `me` is the sole advertiser, so it ranks slot 0 and a record is built.
        publisher
            .refresh_slot(cid, EpochKey::new([2u8; 32]), vec![(me, String::new())], me)
            .await;

        let reg = spy
            .last_registration()
            .await
            .expect("a record was registered");
        let (_payload, vouch) = decode_rendezvous_blob(&reg.routing_blob).expect("blob decodes");
        let vouch = vouch.expect("vouch present");
        let enrolled: HashSet<[u8; 32]> =
            [device_sk.verifying_key().to_bytes()].into_iter().collect();
        assert!(verify_membership_vouch(
            &vouch,
            cid,
            &reg.harmony_identity_pub,
            &enrolled,
            reg.announced_at_ms,
        )
        .is_ok());
    }

    #[tokio::test]
    async fn rank0_advertiser_registers_slot0() {
        let spy = Arc::new(MockPublisher::default());
        let p = publisher_for(Arc::clone(&spy));
        let cid = SpaceId([1u8; 16]);
        let me = OwnerAddr([1u8; 16]);
        // me is the lowest address → rank 0.
        let others = vec![(OwnerAddr([2u8; 16]), String::new()), (me, String::new())];
        p.refresh_slot(cid, EpochKey::new([5u8; 32]), others, me)
            .await;
        let regs = spy.registrations().await;
        assert_eq!(regs.len(), 1);
        assert!(regs[0].handle.contains("rendezvous"));
        assert!(regs[0].handle.contains(&hex::encode(cid.0)));
        assert!(regs[0].handle.ends_with(":0"), "rank-0 → slot 0");
    }

    #[tokio::test]
    async fn refresh_slot_uses_diversity_bucket_not_address_rank() {
        // ZEB-940 wiring: `me` (address 3) is behind relay B; two lower-address
        // advertisers (1, 2) are behind relay A. A plain address rank would place
        // `me` at slot 2; the diversity round-robin (relay B is the 2nd bucket,
        // first round) places it at slot 1. Asserting the registered handle ends
        // `:1` proves refresh_slot consumes the bucket, not just the address.
        let spy = Arc::new(MockPublisher::default());
        let p = publisher_for(Arc::clone(&spy));
        let cid = SpaceId([1u8; 16]);
        let me = OwnerAddr([3u8; 16]);
        let advertisers = vec![
            (OwnerAddr([1u8; 16]), "relayA".to_string()),
            (OwnerAddr([2u8; 16]), "relayA".to_string()),
            (me, "relayB".to_string()),
        ];
        p.refresh_slot(cid, EpochKey::new([5u8; 32]), advertisers, me)
            .await;
        let regs = spy.registrations().await;
        assert_eq!(regs.len(), 1);
        assert!(
            regs[0].handle.ends_with(":1"),
            "diversity slot 1 (relay-B bucket), not address-rank slot 2; got {}",
            regs[0].handle
        );
    }

    #[tokio::test]
    async fn non_advertiser_unregisters_slot() {
        let spy = Arc::new(MockPublisher::default());
        let p = publisher_for(Arc::clone(&spy));
        let cid = SpaceId([1u8; 16]);
        let me = OwnerAddr([9u8; 16]); // not in the set
        let others = vec![
            (OwnerAddr([1u8; 16]), String::new()),
            (OwnerAddr([2u8; 16]), String::new()),
        ];
        // Pretend we were a beacon before (registered slot 0), then dropped out.
        p.refresh_slot(
            cid,
            EpochKey::new([5u8; 32]),
            others.clone(),
            OwnerAddr([1u8; 16]),
        )
        .await;
        p.refresh_slot(cid, EpochKey::new([5u8; 32]), others, me)
            .await;
        assert!(spy
            .unregistrations()
            .await
            .iter()
            .any(|h| h.contains("rendezvous")));
        // Relinquishing the slot (opt-out to None) records a demotion.
        assert_eq!(p.observability().demotions.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn lowest_ranked_eligible_online_promotes() {
        let a = OwnerAddr([1u8; 16]);
        let b = OwnerAddr([2u8; 16]);
        // Slots under-filled (0 live of N). Both online + high-availability.
        let ranked = vec![
            (a, PowerTier::HighAvailability),
            (b, PowerTier::HighAvailability),
        ];
        assert!(
            should_self_promote(&[a, b], 0, &ranked, &a),
            "lowest rank promotes"
        );
        assert!(
            !should_self_promote(&[a, b], 0, &ranked, &b),
            "higher rank defers"
        );
    }

    #[test]
    fn low_power_defers_to_high_availability() {
        let a = OwnerAddr([1u8; 16]); // lowest rank but low power
        let b = OwnerAddr([2u8; 16]); // higher rank but high availability
        let ranked = vec![(a, PowerTier::LowPower), (b, PowerTier::HighAvailability)];
        assert!(
            !should_self_promote(&[a, b], 0, &ranked, &a),
            "low-power defers"
        );
        assert!(
            should_self_promote(&[a, b], 0, &ranked, &b),
            "HA promotes as last resort"
        );
    }

    #[test]
    fn filled_slots_suppress_promotion() {
        let a = OwnerAddr([1u8; 16]);
        let ranked = vec![(a, PowerTier::HighAvailability)];
        assert!(!should_self_promote(
            &[a],
            RENDEZVOUS_SLOT_COUNT,
            &ranked,
            &a
        ));
    }

    #[tokio::test]
    async fn refresh_slot_increments_slot_fills_counter() {
        let spy = Arc::new(MockPublisher::default());
        let p = publisher_for(Arc::clone(&spy));
        let cid = SpaceId([1u8; 16]);
        let me = OwnerAddr([1u8; 16]);
        let others = vec![(OwnerAddr([2u8; 16]), String::new()), (me, String::new())];
        // Before any refresh, the counter is zero.
        assert_eq!(p.observability().slot_fills.load(Ordering::Relaxed), 0);
        // A refresh that registers slot 0 bumps the slot-fill counter to 1.
        p.refresh_slot(cid, EpochKey::new([5u8; 32]), others.clone(), me)
            .await;
        assert_eq!(p.observability().slot_fills.load(Ordering::Relaxed), 1);
        // A no-op re-register of the SAME slot must not double-count.
        p.refresh_slot(cid, EpochKey::new([5u8; 32]), others, me)
            .await;
        assert_eq!(p.observability().slot_fills.load(Ordering::Relaxed), 1);
        // The fill recorded a latency sample too.
        assert_eq!(p.observability().slot_fill_latency_ms.lock().await.len(), 1);
        // Exercise the tracing read path so log() is covered + non-dead.
        p.log_observability();
    }
}
