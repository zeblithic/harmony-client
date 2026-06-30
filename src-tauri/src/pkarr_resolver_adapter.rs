//! Case C fallback (in-community pkarr lookup) — implements Phase 1's
//! `ReachabilityFallback` trait by querying pkarr-relays for peer routing.
//!
//! Triggered automatically by `ReachabilityResolver::resolve_async()` when
//! the in-memory CRDT map has no fresh entry for the requested peer.

use async_trait::async_trait;
use harmony_pkarr::{derive_ephemeral_key, epoch_tolerance_window, PkarrCase, PkarrResolver};
use std::sync::Arc;

use crate::network_health::PkarrFallbackTelemetry;
use crate::owner_state_types::{OwnerAddr, SpaceId};
use crate::reachability_record::ReachabilityAnnouncePayload;
use crate::reachability_resolver::ReachabilityFallback;

/// Closure type that yields the set of community contexts to probe for a given
/// peer address. Injected at boot from lib.rs which has access to NodeState.
pub type ContextsFn = Arc<dyn Fn(&OwnerAddr) -> Vec<PkarrCommunityContext> + Send + Sync>;

/// Context for a single community membership that may yield a routing record
/// for a target peer.
#[derive(Clone)]
pub struct PkarrCommunityContext {
    pub community_id: SpaceId,
    /// 32-byte community epoch key (HKDF input).
    pub epoch_key: [u8; 32],
    /// 64-byte harmony identity pub of the target member.
    pub target_member_identity_pub: [u8; 64],
}

/// Wraps `harmony_pkarr::PkarrResolver` and a closure that produces the set of
/// (community_id, EpochKey, target_member_identity_pub) tuples a seeker should
/// try for a given peer address. The closure is plumbed in from lib.rs which
/// has access to NodeState's community list and per-community EpochKey.
pub struct PkarrResolverAdapter {
    pkarr: Arc<PkarrResolver>,
    contexts: ContextsFn,
    /// ZEB-595: records one (peer, community) probe outcome per community
    /// context tried, surfaced by the Network Health panel via
    /// `ProdPkarrSnapshot::recent_fallback_events`.
    fallback_telemetry: Arc<PkarrFallbackTelemetry>,
}

impl PkarrResolverAdapter {
    pub fn new(
        pkarr: Arc<PkarrResolver>,
        contexts: ContextsFn,
        fallback_telemetry: Arc<PkarrFallbackTelemetry>,
    ) -> Self {
        Self {
            pkarr,
            contexts,
            fallback_telemetry,
        }
    }
}

#[async_trait]
impl ReachabilityFallback for PkarrResolverAdapter {
    async fn resolve(&self, addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
        let ctxs = (self.contexts)(addr);
        if ctxs.is_empty() {
            return Vec::new();
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_millis() as u64;
        let epoch_window = epoch_tolerance_window(now_ms);

        // For each (community_context × epoch), derive the key, query pkarr,
        // collect successful records. First valid response per community wins.
        let mut payloads = Vec::new();
        for ctx in ctxs {
            // ZEB-595: track whether THIS community context yielded a valid
            // routing payload so the Network Health panel can show a per-peer,
            // per-community hit/miss breakdown.
            let mut ctx_hit = false;
            'epoch_loop: for epoch in epoch_window {
                let mut info = Vec::with_capacity(64 + 8);
                info.extend_from_slice(&ctx.target_member_identity_pub);
                info.extend_from_slice(&epoch.to_be_bytes());
                let signing = derive_ephemeral_key(PkarrCase::Community, &ctx.epoch_key, &info);
                let verifying = signing.verifying_key();
                if let Ok(Some(rec)) = self.pkarr.resolve(&verifying).await {
                    // RPK2: verify inner sig.
                    if rec.verify_inner_sig().is_err() {
                        continue;
                    }
                    // RPK3: verify identity match.
                    if rec
                        .verify_identity_match(&ctx.target_member_identity_pub)
                        .is_err()
                    {
                        continue;
                    }
                    // RPK4: verify freshness (future-strict + signed TTL).
                    if rec.verify_freshness(now_ms).is_err() {
                        continue;
                    }
                    // Decode routing_blob into harmony-client's ReachabilityAnnouncePayload.
                    if let Ok(payload) = ciborium::from_reader::<ReachabilityAnnouncePayload, _>(
                        rec.routing_blob.as_slice(),
                    ) {
                        payloads.push(payload);
                        ctx_hit = true;
                        break 'epoch_loop; // First valid per community
                    }
                }
            }
            self.fallback_telemetry
                .record(&addr.0, &ctx.community_id.0, ctx_hit);
        }
        payloads
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use harmony_pkarr::{testing::MockPkarrRelay, RelayClient, RelayPool};

    #[tokio::test]
    async fn empty_contexts_returns_empty() {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let pkarr = Arc::new(PkarrResolver::new(client));
        let telemetry = Arc::new(PkarrFallbackTelemetry::new());
        let adapter =
            PkarrResolverAdapter::new(pkarr, Arc::new(|_addr| Vec::new()), Arc::clone(&telemetry));
        let result = adapter.resolve(&OwnerAddr([0u8; 16])).await;
        assert!(result.is_empty());
        // ZEB-595: no community context to probe -> nothing recorded. The ring
        // must not fill with meaningless "no-context" entries on every resolve.
        assert!(
            telemetry.recent().is_empty(),
            "empty contexts must record no fallback events"
        );
    }

    #[tokio::test]
    async fn records_miss_when_no_record_published() {
        // ZEB-595: a non-empty community context whose pkarr lookup finds
        // nothing must record exactly one MISS for that (peer, community).
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let pkarr = Arc::new(PkarrResolver::new(client));
        let telemetry = Arc::new(PkarrFallbackTelemetry::new());

        let target_addr = OwnerAddr([0x22u8; 16]);
        let community_id = SpaceId([0x33u8; 16]);
        let contexts: ContextsFn = Arc::new(move |addr: &OwnerAddr| {
            if *addr == target_addr {
                vec![PkarrCommunityContext {
                    community_id,
                    epoch_key: [0xBBu8; 32],
                    target_member_identity_pub: [0u8; 64],
                }]
            } else {
                vec![]
            }
        });
        let adapter = PkarrResolverAdapter::new(pkarr, contexts, Arc::clone(&telemetry));

        let out = adapter.resolve(&target_addr).await;
        assert!(out.is_empty(), "nothing published -> no payloads");

        let events = telemetry.recent();
        assert_eq!(events.len(), 1, "one probe -> one recorded event");
        assert!(!events[0].hit, "no record published -> miss");
        assert_eq!(events[0].peer_addr_short, "22222222");
        assert_eq!(events[0].community_id_short, "33333333");
    }

    #[allow(dead_code)]
    fn build_identity_pub(sk: &SigningKey) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        out
    }

    // Full end-to-end (publish to mock, resolve via adapter) is in
    // tests/pkarr_community_fallback_integration.rs (Task 9).
}
