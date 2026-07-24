//! Case C fallback (in-community pkarr lookup) — implements Phase 1's
//! `ReachabilityFallback` trait by querying pkarr-relays for peer routing.
//!
//! Triggered automatically by `ReachabilityResolver::resolve_async()` when
//! the in-memory CRDT map has no fresh entry for the requested peer.

use async_trait::async_trait;
use harmony_pkarr::{derive_ephemeral_key, epoch_tolerance_window, PkarrCase, PkarrResolver};
use std::sync::Arc;

use crate::community_membership::MemberStatus;
use crate::community_state_sync::{CommunitySyncRegistry, IdentityResolver};
use crate::network_health::{PkarrFallbackOutcome, PkarrFallbackTelemetry};
use crate::owner_state_types::{OwnerAddr, SpaceId};
use crate::reachability_record::ReachabilityAnnouncePayload;
use crate::reachability_resolver::ReachabilityFallback;

/// Closure type that yields the set of community contexts to probe for a given
/// peer address. Injected at boot from lib.rs which has access to NodeState.
///
/// ZEB-596: **async** — producing the contexts requires walking the community
/// registry (per-community EpochKey + the target's identity_pub) which is only
/// reachable through `.await`. `ReachabilityFallback::resolve` is already async
/// and the resolver drops its own lock before invoking the fallback, so this
/// adds no trait change and no new lock-ordering risk. The closure takes
/// `OwnerAddr` by value (it is `Copy`) so the returned future can be `'static`.
pub type ContextsFn = Arc<
    dyn Fn(OwnerAddr) -> futures::future::BoxFuture<'static, Vec<PkarrCommunityContext>>
        + Send
        + Sync,
>;

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
impl ReachabilityFallback<OwnerAddr> for PkarrResolverAdapter {
    async fn resolve(&self, addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
        // ZEB-596: the contexts closure walks the community registry (async).
        // It gathers all contexts and returns BEFORE we issue any pkarr relay
        // round-trips below, so no community-engine / owner-state lock is held
        // across the network IO.
        let ctxs = (self.contexts)(*addr).await;
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
            // ZEB-595: classify this community context's probe so the Network
            // Health panel can distinguish a clean "no record here" (Miss) from
            // a probe that couldn't produce a trustworthy answer (Error) — the
            // two must not be conflated during incident triage (Qodo #377).
            let mut ctx_hit = false;
            let mut ctx_error = false;
            'epoch_loop: for epoch in epoch_window {
                let mut info = Vec::with_capacity(64 + 8);
                info.extend_from_slice(&ctx.target_member_identity_pub);
                info.extend_from_slice(&epoch.to_be_bytes());
                let signing = derive_ephemeral_key(PkarrCase::Community, &ctx.epoch_key, &info);
                let verifying = signing.verifying_key();
                match self.pkarr.resolve(&verifying).await {
                    Ok(Some(rec)) => {
                        // RPK2/RPK3: a record is present but its inner signature
                        // or identity binding doesn't check out — corrupt or
                        // spoofed, not a clean miss.
                        if rec.verify_inner_sig().is_err()
                            || rec
                                .verify_identity_match(&ctx.target_member_identity_pub)
                                .is_err()
                        {
                            ctx_error = true;
                            continue;
                        }
                        // RPK4: stale/expired record (future-strict + signed
                        // TTL). The peer's record simply lapsed — a legitimate
                        // "no current record", i.e. a Miss, not an Error.
                        if rec.verify_freshness(now_ms).is_err() {
                            continue;
                        }
                        // Decode routing_blob into harmony-client's ReachabilityAnnouncePayload.
                        match ciborium::from_reader::<ReachabilityAnnouncePayload, _>(
                            rec.routing_blob.as_slice(),
                        ) {
                            Ok(payload) => {
                                payloads.push(payload);
                                ctx_hit = true;
                                break 'epoch_loop; // First valid per community
                            }
                            // Present but undecodable routing blob — anomalous.
                            Err(_) => ctx_error = true,
                        }
                    }
                    // Relay reachable, nothing published at this key — clean miss.
                    Ok(None) => {}
                    // Resolver/transport failure — the probe couldn't complete.
                    Err(_) => ctx_error = true,
                }
            }
            let outcome = if ctx_hit {
                PkarrFallbackOutcome::Hit
            } else if ctx_error {
                PkarrFallbackOutcome::Error
            } else {
                PkarrFallbackOutcome::Miss
            };
            self.fallback_telemetry
                .record(&addr.0, &ctx.community_id.0, outcome);
        }
        payloads
    }
}

/// ZEB-596: build the case-C community contexts for a target peer by walking
/// every community this device is in (spec §7.4). For each community where
/// `target` is a currently-`Joined` member, yields `(community_id, the
/// community's live EpochKey, target identity_pub)`.
///
/// Two skip conditions:
/// * the target's 64-byte harmony identity_pub must resolve from our
///   owner-device cache — without it the case-C HKDF key can't be derived, and
///   since identity_pub is a single global per-owner value it is resolved
///   ONCE up front (a `None` means no contexts at all);
/// * the community's live epoch key must be present in the owner-state CRDT —
///   a community whose `Space.current_epoch_key` we haven't synced is skipped
///   rather than probed with a guessed key.
///
/// The epoch key comes from `live_epoch_key` (`Space.current_epoch_key`), the
/// SAME source the engine's own encrypt/decrypt uses — NOT the engine's
/// spawn-time `membership_key`, which goes stale after an epoch rotation
/// (ZEB-249 backward secrecy) and differs across devices/sessions (Qodo #378).
/// All contexts are gathered before returning — the caller
/// (`PkarrResolverAdapter::resolve`) only issues pkarr relay round-trips after
/// this returns, so no engine / owner-state lock is held across network IO.
pub async fn community_contexts_for_target(
    registry: &CommunitySyncRegistry,
    crdt_state: &Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    resolver: &(dyn IdentityResolver + Send + Sync),
    target: OwnerAddr,
) -> Vec<PkarrCommunityContext> {
    // identity_pub is a single global per-owner value — resolve it once rather
    // than re-locking the owner-state mutex per shared community (Qodo #378).
    let Some(identity_pub) = resolver.resolve(&target).await else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let community_ids = registry.known_ids().await;
    for cid in &community_ids {
        let Some(engine) = registry.engine_arc(cid).await else {
            continue;
        };
        // Probe only communities where the target is a current member — this
        // is what makes case C members-only (we hold the EpochKey only as a
        // co-member) and avoids deriving keys for peers who have left.
        let target_joined = {
            let state_arc = engine.state();
            let st = state_arc.lock().await;
            let mat = st.materialized(engine.admin_addr());
            mat.members
                .get(&target)
                .map(|m| m.status == MemberStatus::Joined)
                .unwrap_or(false)
        };
        if !target_joined {
            continue;
        }
        // The community's LIVE epoch key (Space.current_epoch_key), per
        // community. `membership_key()` is only the spawn-time fallback (used
        // when crdt_state is None); a missing/incomplete Space surfaces as Err
        // and we skip rather than probe with a stale key.
        let fallback = engine.membership_key();
        let epoch_key =
            match crate::community_state_sync::live_epoch_key(*cid, Some(crdt_state), &fallback)
                .await
            {
                Ok((k, _epoch)) => *k.as_bytes(),
                Err(e) => {
                    // Skip this community from case-C discovery rather than probe
                    // with a stale key. Log so a silently-skipped community is
                    // visible in production (Greptile #378); debug-level because a
                    // missing/incomplete Space is an expected transient right after
                    // join, before the Space CRDT has populated current_epoch_key.
                    tracing::debug!(
                        community = ?cid,
                        error = %e,
                        "case-C pkarr discovery: skipping community, live epoch key unavailable"
                    );
                    continue;
                }
            };
        out.push(PkarrCommunityContext {
            community_id: *cid,
            epoch_key,
            target_member_identity_pub: identity_pub,
        });
    }
    out
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
        let contexts: ContextsFn = Arc::new(|_addr: OwnerAddr| {
            Box::pin(async { Vec::new() })
                as futures::future::BoxFuture<'static, Vec<PkarrCommunityContext>>
        });
        let adapter = PkarrResolverAdapter::new(pkarr, contexts, Arc::clone(&telemetry));
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
        let contexts: ContextsFn = Arc::new(move |addr: OwnerAddr| {
            let ctxs = if addr == target_addr {
                vec![PkarrCommunityContext {
                    community_id,
                    epoch_key: [0xBBu8; 32],
                    target_member_identity_pub: [0u8; 64],
                }]
            } else {
                vec![]
            };
            Box::pin(async move { ctxs })
                as futures::future::BoxFuture<'static, Vec<PkarrCommunityContext>>
        });
        let adapter = PkarrResolverAdapter::new(pkarr, contexts, Arc::clone(&telemetry));

        let out = adapter.resolve(&target_addr).await;
        assert!(out.is_empty(), "nothing published -> no payloads");

        let events = telemetry.recent();
        assert_eq!(events.len(), 1, "one probe -> one recorded event");
        // Relay reachable, nothing published (Ok(None)) -> a clean MISS, NOT an
        // Error. This is the distinction Qodo #377 asked for.
        assert_eq!(
            events[0].outcome,
            PkarrFallbackOutcome::Miss,
            "no record published is a miss, not an error"
        );
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
