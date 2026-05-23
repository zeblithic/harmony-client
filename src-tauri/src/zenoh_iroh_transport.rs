//! ZEB-321 Phase 1 Task 6: `zenoh_link::LinkManagerUnicastTrait` impl
//! backed by an iroh `Endpoint` and a CRDT-driven `ReachabilityResolver`.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §7.3.
//!
//! ## Design intent
//!
//! Zenoh's transport plugin surface (see `zenoh-link-commons::unicast`)
//! exposes two pluggable bits:
//!
//! 1. `LinkManagerUnicastTrait` — the upper transport stack calls
//!    `new_link(endpoint)` to open an outbound link by locator and
//!    `new_listener(endpoint)` to advertise an inbound listener.
//! 2. `NewLinkChannelSender` (a `flume::Sender<LinkUnicast>`) — the
//!    accept side dispatches inbound links into Zenoh via this channel.
//!
//! [`IrohZenohLinkManager`] implements the first and owns one of the
//! second. On `new_link()` it parses the locator's address as a hex iroh
//! `EndpointId`, looks up the matching `ReachabilityAnnouncePayload` via
//! [`ReachabilityResolver::resolve_by_node_id`], synthesizes an
//! `EndpointAddr` (id + optional relay + direct addrs), opens a QUIC
//! bidi stream on ALPN `harmony/zenoh/v1`, and wraps the
//! `(SendStream, RecvStream)` pair in [`IrohZenohLink`] (Task 5).
//!
//! ## Locator format (load-bearing choice)
//!
//! Locator address is the **hex-encoded iroh EndpointId**:
//!
//! ```text
//! iroh/<lowercase-hex-32-bytes>
//! ```
//!
//! - Matches Task 5's `paired_stream_roundtrip_via_loopback` test, which
//!   uses `Locator::new("iroh", ep_id.to_string(), "")` — iroh 0.98's
//!   `Display for PublicKey` emits lowercase hex (see
//!   `iroh_base::key`), so `ep_id.to_string()` and
//!   `hex::encode(ep_id.as_bytes())` are identical. Picking either makes
//!   Task 5's link + Task 6's link manager round-trip cleanly.
//! - The locator carries the iroh `EndpointId` (32 bytes), NOT the
//!   harmony `OwnerAddr` (16 bytes). The spec text in §7.3 is
//!   intentional on this point — the resolver's reverse lookup is what
//!   bridges back to the harmony actor.
//!
//! ## Resolver lookup approach
//!
//! [`ReachabilityResolver::resolve`] is keyed by `OwnerAddr`. The
//! locator carries `EndpointId`. We added
//! [`ReachabilityResolver::resolve_by_node_id`] (a linear scan over
//! `list_active_peers()`) rather than indexing-by-node-id from the
//! transport here — the resolver is the source of truth for the LWW
//! projection, so the secondary lookup belongs alongside it.
//!
//! ## API adaptations from the plan draft
//!
//! The plan draft (`docs/plans/2026-05-22-zeb-321-phase1-iroh-foundation-plan.md`
//! lines 1670-1860) was written against unverified zenoh-link / iroh
//! 0.98 surfaces. Adaptations:
//!
//! - **`LinkManagerUnicastTrait` method signatures** — `new_link`,
//!   `new_listener` take `EndPoint` by *value* (not `&EndPoint`).
//!   `get_listeners` and `get_locators` are *async* and return `Vec<…>`.
//! - **`NewLinkChannelSender`** is `flume::Sender<LinkUnicast>` (not
//!   `tokio::mpsc`). Required pulling `flume = "0.11"` in directly.
//! - **iroh 0.98 renames** — `NodeId` → `EndpointId`, `NodeAddr` →
//!   `EndpointAddr`, `Endpoint::connect` takes
//!   `impl Into<EndpointAddr>` (not a builder chain via
//!   `.with_direct_addresses`).
//! - **`EndpointAddr` builders** — `with_relay_url(url)` and
//!   `with_ip_addr(addr)` are the right calls; the plan's
//!   `.with_direct_addresses(std::iter::once(...))` was wrong (the real
//!   API takes a `BTreeSet`-collected `with_addrs(impl IntoIterator)`).
//! - **`Connection::alpn()`** returns `&[u8]` directly (not
//!   `Option<Vec<u8>>` / `Result<…>`).
//! - **`Connection::remote_id()`** — not `.remote_node_id()`. Both
//!   refer to the peer's `EndpointId`.
//! - **`ZResult` / `zerror!`** — come from `zenoh_result`, not from
//!   `zenoh_link` (already discovered in Task 5).

use std::sync::Arc;

use async_trait::async_trait;
use iroh::{EndpointAddr, EndpointId};
use zenoh_link::{
    EndPoint, LinkManagerUnicastTrait, LinkUnicast, LinkUnicastTrait, Locator,
    NewLinkChannelSender,
};
use zenoh_result::{zerror, ZResult};

use crate::iroh_endpoint::{alpn, IrohEndpoint};
use crate::reachability_resolver::ReachabilityResolver;
use crate::zenoh_iroh_link::IrohZenohLink;

/// Locator protocol identifier for harmony's zenoh-over-iroh links.
const IROH_LOCATOR_PROTOCOL: &str = "iroh";

/// Plug-in link manager that lets Zenoh open + accept links over an
/// iroh `Endpoint`, using [`ReachabilityResolver`] to translate a
/// locator's iroh `EndpointId` into a dialable [`EndpointAddr`].
pub struct IrohZenohLinkManager {
    endpoint: Arc<IrohEndpoint>,
    resolver: ReachabilityResolver,
    /// Channel into Zenoh's transport stack for inbound links the
    /// accept loop (spawned via [`IrohZenohLinkManager::spawn_accept_loop`])
    /// produces. Not used by the outbound `new_link` path.
    new_link_tx: NewLinkChannelSender,
}

impl IrohZenohLinkManager {
    pub fn new(
        endpoint: Arc<IrohEndpoint>,
        resolver: ReachabilityResolver,
        new_link_tx: NewLinkChannelSender,
    ) -> Self {
        Self {
            endpoint,
            resolver,
            new_link_tx,
        }
    }

    /// Spawn the inbound-link accept loop. Each accepted connection is
    /// filtered on ALPN `harmony/zenoh/v1`, an `accept_bi` stream pair
    /// is wrapped in [`IrohZenohLink`], and the result is dispatched to
    /// Zenoh via the [`NewLinkChannelSender`] this manager owns.
    ///
    /// Returns the join handle; callers should hold it to drive
    /// shutdown explicitly. Endpoint shutdown causes `accept()` to
    /// return `None`, which ends the loop.
    pub fn spawn_accept_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let ep = mgr.endpoint.inner().clone();
            while let Some(incoming) = ep.accept().await {
                let mgr = Arc::clone(&mgr);
                tokio::spawn(async move {
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("iroh accept→connect failed: {e}");
                            return;
                        }
                    };
                    // ALPN filter — `alpn()` returns the negotiated
                    // peer ALPN as &[u8] directly. iroh has already
                    // checked it's one of ours (the ones we registered
                    // at bind time), but cross-check defensively in
                    // case future code registers handshake or other
                    // sub-protocols on the same endpoint.
                    let alpn_used = conn.alpn();
                    if alpn_used != alpn::HARMONY_ZENOH_V1 {
                        tracing::debug!(
                            "ignoring non-zenoh ALPN: {:?}",
                            std::str::from_utf8(alpn_used).unwrap_or("<binary>")
                        );
                        return;
                    }
                    let (send, recv) = match conn.accept_bi().await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("iroh accept_bi failed: {e}");
                            return;
                        }
                    };
                    let peer_id = conn.remote_id();
                    let src = locator_from_endpoint_id(&mgr.endpoint.node_id());
                    let dst = locator_from_endpoint_id(&peer_id);
                    let link: Arc<dyn LinkUnicastTrait> =
                        Arc::new(IrohZenohLink::new(send, recv, src, dst));
                    if let Err(e) = mgr.new_link_tx.send_async(LinkUnicast(link)).await {
                        tracing::warn!("zenoh new_link channel closed: {e}");
                    }
                });
            }
        })
    }

    /// Parse a `Locator`'s address as a hex iroh `EndpointId`.
    /// Returns `None` for any malformed input (wrong hex length, bad
    /// hex digits, not a valid Ed25519 public key).
    fn parse_endpoint_id(endpoint: &EndPoint) -> Option<EndpointId> {
        let addr = endpoint.address();
        let addr_str: &str = addr.as_str();
        let bytes = hex::decode(addr_str).ok()?;
        let arr: [u8; 32] = bytes.try_into().ok()?;
        EndpointId::from_bytes(&arr).ok()
    }
}

/// Build the canonical locator for a given iroh `EndpointId`.
/// Format: `iroh/<lowercase-hex-32-bytes>`. Matches
/// `iroh::EndpointId`'s `Display` impl (hex), so peers reading the
/// locator can `addr_str.parse::<EndpointId>()` and round-trip.
fn locator_from_endpoint_id(id: &EndpointId) -> Locator {
    Locator::new(IROH_LOCATOR_PROTOCOL, hex::encode(id.as_bytes()), "")
        .expect("iroh locator format is well-known")
}

#[async_trait]
impl LinkManagerUnicastTrait for IrohZenohLinkManager {
    async fn new_link(&self, endpoint: EndPoint) -> ZResult<LinkUnicast> {
        let peer_id = Self::parse_endpoint_id(&endpoint).ok_or_else(|| {
            zerror!(
                "iroh locator address is not a 64-char hex EndpointId: {:?}",
                endpoint.address().as_str()
            )
        })?;
        let (_, record) = self
            .resolver
            .resolve_by_node_id(peer_id.as_bytes())
            .ok_or_else(|| zerror!("no ReachabilityRecord for iroh EndpointId {peer_id}"))?;

        // Build the EndpointAddr from the resolver payload. Skip
        // malformed relay URLs silently — direct addrs alone may still
        // succeed; logging at trace keeps the failure visible without
        // spamming production logs.
        let mut addr = EndpointAddr::new(peer_id);
        if !record.home_relay_url.is_empty() {
            match record.home_relay_url.parse() {
                Ok(url) => addr = addr.with_relay_url(url),
                Err(e) => {
                    tracing::trace!(
                        "skip malformed home_relay_url {:?}: {e}",
                        record.home_relay_url
                    );
                }
            }
        }
        for da in record.direct_addresses {
            addr = addr.with_ip_addr(da);
        }

        let conn = self
            .endpoint
            .inner()
            .connect(addr, alpn::HARMONY_ZENOH_V1)
            .await
            .map_err(|e| zerror!("iroh connect: {e}"))?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| zerror!("iroh open_bi: {e}"))?;
        let src = locator_from_endpoint_id(&self.endpoint.node_id());
        let dst = locator_from_endpoint_id(&peer_id);
        let link: Arc<dyn LinkUnicastTrait> = Arc::new(IrohZenohLink::new(send, recv, src, dst));
        Ok(LinkUnicast(link))
    }

    async fn new_listener(&self, _endpoint: EndPoint) -> ZResult<Locator> {
        // The iroh endpoint is already bound + listening from
        // `IrohEndpoint::new_with_secret`; the accept loop spawned by
        // `spawn_accept_loop` dispatches inbound links into Zenoh. We
        // have nothing to do here beyond returning the local locator
        // so the Zenoh transport stack can record it in its listener
        // set.
        Ok(locator_from_endpoint_id(&self.endpoint.node_id()))
    }

    async fn del_listener(&self, _endpoint: &EndPoint) -> ZResult<()> {
        // No-op — listener lifetime is bound to the underlying
        // `IrohEndpoint`. Endpoint `.shutdown()` closes it.
        Ok(())
    }

    async fn get_listeners(&self) -> Vec<EndPoint> {
        vec![locator_from_endpoint_id(&self.endpoint.node_id()).to_endpoint()]
    }

    async fn get_locators(&self) -> Vec<Locator> {
        vec![locator_from_endpoint_id(&self.endpoint.node_id())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use crate::reachability_record::ReachabilityAnnouncePayload;
    use iroh::endpoint::{presets, Endpoint, RelayMode};
    use iroh::SecretKey;
    use rand::RngCore;
    use std::net::Ipv4Addr;

    /// Build a hermetic iroh endpoint on loopback with no
    /// address-lookup / relay traffic. Mirrors the pattern in
    /// `zenoh_iroh_link::tests` so the test never touches the network.
    ///
    /// Goes through `IrohEndpoint::from_endpoint_for_test` (the
    /// `#[cfg(any(test, feature = "test-fixtures"))]`-gated ctor on
    /// `IrohEndpoint`) so the production path `new_with_secret`
    /// (which uses `presets::N0` + pkarr + DNS and hangs offline)
    /// stays untouched.
    async fn build_hermetic_iroh_endpoint() -> Arc<IrohEndpoint> {
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        let secret = SecretKey::from_bytes(&buf);
        let inner = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![alpn::HARMONY_ZENOH_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind_addr loopback")
            .bind()
            .await
            .expect("bind iroh endpoint");
        Arc::new(IrohEndpoint::from_endpoint_for_test(inner))
    }

    /// Resolver-miss → `new_link` returns an error. This is the only
    /// path we exercise here; a full round-trip lives in Task 10
    /// (two-engine integration) — Task 5 burned six hours on a QUIC
    /// teardown deadlock, and the rule for Task 6 is no actual iroh
    /// connections (see implementer brief, hard rule 6).
    #[tokio::test]
    async fn new_link_errors_on_resolver_miss() {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            new_link_errors_on_resolver_miss_inner(),
        )
        .await
        .expect("test must finish within 5s");
    }

    async fn new_link_errors_on_resolver_miss_inner() {
        let endpoint = build_hermetic_iroh_endpoint().await;
        let resolver = ReachabilityResolver::new();
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let mgr = IrohZenohLinkManager::new(Arc::clone(&endpoint), resolver, new_link_tx);

        // Build a locator with a *different* random iroh EndpointId
        // (one the resolver has never seen). new_link must fail before
        // any QUIC traffic is attempted.
        let mut bogus_id_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bogus_id_bytes);
        let bogus_id = EndpointId::from_bytes(&bogus_id_bytes)
            .expect("random bytes happen to be valid Ed25519 pub key");
        let bogus_locator = locator_from_endpoint_id(&bogus_id);

        let result = mgr.new_link(bogus_locator.to_endpoint()).await;
        assert!(
            result.is_err(),
            "expected resolver miss to surface as Err, got Ok"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("no ReachabilityRecord"),
            "error message should mention missing record, got: {err_msg}"
        );

        endpoint.shutdown().await;
    }

    /// Malformed locator address (not 64-char hex) → `new_link` returns
    /// an error and never touches the resolver.
    #[tokio::test]
    async fn new_link_errors_on_malformed_locator() {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            new_link_errors_on_malformed_locator_inner(),
        )
        .await
        .expect("test must finish within 5s");
    }

    async fn new_link_errors_on_malformed_locator_inner() {
        let endpoint = build_hermetic_iroh_endpoint().await;
        let resolver = ReachabilityResolver::new();
        let (new_link_tx, _rx) = flume::unbounded::<LinkUnicast>();
        let mgr = IrohZenohLinkManager::new(Arc::clone(&endpoint), resolver, new_link_tx);

        // "not-hex" → hex::decode fails inside parse_endpoint_id.
        let bad = Locator::new(IROH_LOCATOR_PROTOCOL, "not-a-hex-endpoint-id", "")
            .expect("locator string itself is structurally valid");
        let result = mgr.new_link(bad.to_endpoint()).await;
        assert!(result.is_err(), "expected malformed-locator Err");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("not a 64-char hex"),
            "error message should mention hex parse failure, got: {err_msg}"
        );

        endpoint.shutdown().await;
    }

    /// `resolve_by_node_id` finds the matching `OwnerAddr` + payload
    /// when the resolver has a record for the given iroh EndpointId.
    /// (The actual `new_link` connect path beyond this point requires
    /// a real iroh peer, which is Task 10's territory.)
    #[test]
    fn resolver_lookup_by_node_id_finds_match() {
        let resolver = ReachabilityResolver::new();
        let actor = OwnerAddr([0xAB; 16]);
        let payload = ReachabilityAnnouncePayload {
            iroh_node_id: [0xCD; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0; 64],
        };
        let hlc = Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test".into(),
        };
        resolver.update(actor, payload.clone(), hlc);

        let found = resolver.resolve_by_node_id(&[0xCD; 32]);
        assert!(found.is_some(), "node-id lookup must hit");
        let (found_actor, found_payload) = found.unwrap();
        assert_eq!(found_actor, actor);
        assert_eq!(found_payload, payload);

        // Wrong node id → None.
        assert!(resolver.resolve_by_node_id(&[0xEE; 32]).is_none());
    }

    /// Sanity: `locator_from_endpoint_id` round-trips through
    /// `parse_endpoint_id`.
    #[test]
    fn locator_round_trips_through_parser() {
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        let id = EndpointId::from_bytes(&buf).expect("valid pub key");
        let locator = locator_from_endpoint_id(&id);

        let parsed = IrohZenohLinkManager::parse_endpoint_id(&locator.to_endpoint())
            .expect("locator parses back into EndpointId");
        assert_eq!(parsed, id);
    }
}
