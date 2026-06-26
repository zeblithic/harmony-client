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

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use harmony_pkarr::{
    current_epoch_id, EphemeralKeyBuilder, PkarrPublisher, PkarrRoutingRecord, RecordBuilder,
};

use crate::community_rendezvous::{rendezvous_slot_key, slot_for_advertiser};
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
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    /// Currently-registered rendezvous slot per community, so a rank change
    /// unregisters the stale slot handle before registering the new one.
    registered_slots: Mutex<HashMap<SpaceId, u16>>,
}

impl CommunityRendezvousPublisher {
    /// Production constructor — mirrors `PkarrCommunityPublisher::new` so the
    /// same `(publisher, identity_signing_key, identity_pub, routing_blob_builder)`
    /// inputs build both publishers at boot.
    pub fn new(
        publisher: Arc<PkarrPublisher>,
        identity_signing_key: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    ) -> Self {
        Self::new_with_sink(
            publisher,
            identity_signing_key,
            identity_pub,
            routing_blob_builder,
        )
    }

    /// Construct over any [`RendezvousSink`] (the real `PkarrPublisher` in
    /// production, a spy in tests).
    pub fn new_with_sink(
        sink: Arc<dyn RendezvousSink>,
        identity_signing_key: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    ) -> Self {
        Self {
            sink,
            identity_signing_key,
            identity_pub,
            routing_blob_builder,
            registered_slots: Mutex::new(HashMap::new()),
        }
    }

    fn slot_handle(community_id: &SpaceId, slot: u16) -> String {
        format!("rendezvous:{}:{}", hex::encode(community_id.0), slot)
    }

    /// (Re)compute this node's rendezvous slot for `community_id` from the
    /// current advertiser set and publish accordingly:
    ///
    /// - `slot_for_advertiser(&advertisers, &me) == Some(slot)` → register a
    ///   pkarr publication keyed by `rendezvous_slot_key(epoch_key, slot,
    ///   current_epoch_id(now))` under handle `rendezvous:<cid>:<slot>`. If the
    ///   node previously held a *different* slot for this community, that stale
    ///   handle is unregistered first.
    /// - `None` (not an advertiser, or rank ≥ cap) → unregister any rendezvous
    ///   handle this node previously registered for the community.
    ///
    /// Re-derives the slot key on every publish (in the `key_builder` closure)
    /// so it tracks the epoch, mirroring the member-keyed community publisher.
    pub async fn refresh_slot(
        &self,
        community_id: SpaceId,
        epoch_key: EpochKey,
        advertisers: Vec<OwnerAddr>,
        me: OwnerAddr,
    ) {
        let mut registered = self.registered_slots.lock().await;
        let prior_slot = registered.get(&community_id).copied();

        match slot_for_advertiser(&advertisers, &me) {
            Some(slot) => {
                // A rank change must drop the stale slot handle before the new
                // one is registered (each rendezvous slot has a single writer).
                if let Some(old) = prior_slot {
                    if old != slot {
                        self.sink
                            .unregister(&Self::slot_handle(&community_id, old))
                            .await;
                    }
                }

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

                let handle = Self::slot_handle(&community_id, slot);
                self.sink.register(handle, key_builder, builder).await;
                registered.insert(community_id, slot);
            }
            None => {
                if let Some(old) = prior_slot {
                    self.sink
                        .unregister(&Self::slot_handle(&community_id, old))
                        .await;
                    registered.remove(&community_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[derive(Clone)]
    struct Registration {
        handle: String,
    }

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
    }

    #[async_trait::async_trait]
    impl RendezvousSink for MockPublisher {
        async fn register(
            &self,
            handle: String,
            _key_builder: EphemeralKeyBuilder,
            _builder: RecordBuilder,
        ) {
            self.inner
                .lock()
                .await
                .registrations
                .push(Registration { handle });
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
            Arc::new(|| b"routing".to_vec()),
        )
    }

    #[tokio::test]
    async fn rank0_advertiser_registers_slot0() {
        let spy = Arc::new(MockPublisher::default());
        let p = publisher_for(Arc::clone(&spy));
        let cid = SpaceId([1u8; 16]);
        let me = OwnerAddr([1u8; 16]);
        // me is the lowest address → rank 0.
        let others = vec![OwnerAddr([2u8; 16]), me];
        p.refresh_slot(cid, EpochKey::new([5u8; 32]), others, me)
            .await;
        let regs = spy.registrations().await;
        assert_eq!(regs.len(), 1);
        assert!(regs[0].handle.contains("rendezvous"));
        assert!(regs[0].handle.contains(&hex::encode(cid.0)));
        assert!(regs[0].handle.ends_with(":0"), "rank-0 → slot 0");
    }

    #[tokio::test]
    async fn non_advertiser_unregisters_slot() {
        let spy = Arc::new(MockPublisher::default());
        let p = publisher_for(Arc::clone(&spy));
        let cid = SpaceId([1u8; 16]);
        let me = OwnerAddr([9u8; 16]); // not in the set
        let others = vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])];
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
    }
}
