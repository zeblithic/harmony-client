//! ZEB-321 Phase 1 Task 10: two-engine cross-WAN integration test.
//!
//! Verifies the full Phase 1 architecture end-to-end on loopback:
//! two distinct [`IrohEndpoint`]s, each with its own
//! [`ReachabilityResolver`] pre-seeded with the other's record, open a
//! `harmony/zenoh/v1` QUIC stream via [`IrohZenohLinkManager`], and
//! round-trip a small CRDT-shaped payload through the
//! [`zenoh_link::LinkUnicastTrait`] wrappers on both ends.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §13.
//!
//! ## Adaptations from the plan draft (lines 2353-2483)
//!
//! 1. **Hermetic endpoints over `presets::N0`.** The plan calls
//!    [`IrohEndpoint::new_with_secret`], which boots `presets::N0` and
//!    initiates outbound pkarr / DERP discovery — that hangs in offline
//!    sandboxes. We build with `presets::Minimal + clear_ip_transports
//!    + bind_addr(LOCALHOST, 0)` (the pattern Tasks 5 + 6 established),
//!    handed to `IrohEndpoint::from_endpoint_for_test`.
//!
//! 2. **Locator hex-encodes the iroh `EndpointId`, not the
//!    `OwnerAddr`.** Plan's `hex::encode(owner_b.0)` would produce
//!    16 bytes of hex — the resolver's `resolve_by_node_id` is keyed on
//!    the 32-byte iroh `EndpointId`, so the lookup would miss. We use
//!    `hex::encode(ep_b.node_id().as_bytes())`.
//!
//! 3. **`ReachabilityAnnouncePayload::direct_addresses` seeded from
//!    `ep.bound_sockets()`, not `ep.direct_addresses()`.** Hermetic
//!    endpoints (no address-lookup service) have an empty
//!    `addr().ip_addrs()` snapshot — but `bound_sockets()` returns the
//!    actual `bind()`-result loopback socket immediately. Without
//!    that, the dialer's [`iroh::Endpoint::connect`] call has no IP to
//!    target and will spin waiting for relay traffic that never comes.
//!    A new `pub fn bound_sockets()` wrapper on [`IrohEndpoint`] was
//!    added in this commit alongside the test.
//!
//! 4. **Outer wall-clock timeout.** Tasks 5 + 9 burned hours on hangs;
//!    the standing convention is a `tokio::time::timeout` around the
//!    whole test body. 30s budget mirrors Task 5's
//!    `paired_stream_roundtrip` (which observed ~20s under heavy CI
//!    parallelism).
//!
//! ## QUIC close discipline
//!
//! Mirrors the Task 5 lesson — the integration test path is different
//! though: we don't hold a [`iroh::endpoint::Connection`] handle, only
//! the wrapped [`LinkUnicastTrait`] streams via [`LinkUnicast`]. So:
//!
//! 1. Both sides call `link.close()` after the exchange to mark the
//!    send-halves finished (idempotent — see [`IrohZenohLink::close`]).
//!    `LinkUnicast: Deref<Target = Arc<dyn LinkUnicastTrait>>` so the
//!    auto-deref dispatches to the underlying trait method; no need to
//!    poke at the `NewLink` enum directly.
//! 2. Drop the link Arcs.
//! 3. `ep_a.shutdown().await` + `ep_b.shutdown().await` — endpoint
//!    shutdown is the universal close primitive; it closes all open
//!    connections cleanly.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;

use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use zenoh_link::{EndPoint, LinkManagerUnicastTrait, LinkUnicast};

/// Build a hermetic iroh endpoint on loopback with no address-lookup,
/// no pkarr publisher, no DERP relays. Mirrors the pattern used by
/// `zenoh_iroh_link::tests::paired_stream_roundtrip_via_loopback` and
/// `zenoh_iroh_transport::tests::build_hermetic_iroh_endpoint`, except
/// it goes through [`IrohEndpoint::from_endpoint_for_test`]
/// (gated behind `feature = "test-fixtures"`, the `pub` alias of the
/// in-crate `pub(crate) from_endpoint_for_test`) so the rest of the
/// test uses the same `IrohEndpoint` wrapper production code uses.
async fn build_hermetic_endpoint() -> Arc<IrohEndpoint> {
    let secret = SecretKey::generate();
    let inner = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![alpn::HARMONY_ZENOH_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .dns_resolver(harmony_app::iroh_endpoint::hermetic_dns_resolver())
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind_addr loopback")
        .bind()
        .await
        .expect("bind iroh endpoint");
    Arc::new(IrohEndpoint::from_endpoint_for_test(inner))
}

#[tokio::test]
async fn two_engines_exchange_via_iroh_zenoh() {
    // ZEB-347: prime the one-time process-global iroh bind init before the
    // asserted timeout (see `iroh_endpoint::warm_up_iroh_global_init`).
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;
    tokio::time::timeout(Duration::from_secs(30), inner())
        .await
        .expect("two_engines_exchange_via_iroh_zenoh must complete within 30s");
}

async fn inner() {
    // Two independent endpoints on loopback. Each gets a distinct
    // `EndpointId` (Ed25519 pubkey) via a fresh `SecretKey`.
    let ep_a = build_hermetic_endpoint().await;
    let ep_b = build_hermetic_endpoint().await;

    // Sanity-check the load-bearing assumption from adaptation #3:
    // hermetic endpoints expose their bound socket immediately via
    // `bound_sockets()`. If this regressed, the test would silently
    // dial against an empty address list and hang for 30s.
    let ep_b_sockets = ep_b.bound_sockets();
    assert!(
        !ep_b_sockets.is_empty(),
        "ep_b.bound_sockets() must be non-empty for hermetic endpoint — \
         got empty list, dialer would have nothing to target"
    );

    // Each engine has its own resolver, seeded with the OTHER engine's
    // ReachabilityAnnouncePayload. Mirrors the production data flow:
    // ReachabilityAnnounce events flow through the membership CRDT,
    // the resolver projects them via `update()`, the link manager
    // reads them via `resolve_by_node_id()`.
    let resolver_a = ReachabilityResolver::new();
    let resolver_b = ReachabilityResolver::new();

    let owner_a = OwnerAddr([0xAA; 16]);
    let owner_b = OwnerAddr([0xBB; 16]);

    let hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "fix".into(),
    };

    // identity_signature is left at [0; 64] — the transport layer
    // (this test) doesn't verify it; only the CRDT apply path does
    // (covered separately by `wire_format_reachability_announce_fixtures`).
    let payload_a = ReachabilityAnnouncePayload {
        iroh_node_id: *ep_a.node_id().as_bytes(),
        home_relay_url: ep_a.home_relay().map(|r| r.to_string()).unwrap_or_default(),
        direct_addresses: ep_a.bound_sockets(),
        announced_at_ms: hlc.wall_ms,
        identity_signature: [0; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    };
    let payload_b = ReachabilityAnnouncePayload {
        iroh_node_id: *ep_b.node_id().as_bytes(),
        home_relay_url: ep_b.home_relay().map(|r| r.to_string()).unwrap_or_default(),
        direct_addresses: ep_b_sockets,
        announced_at_ms: hlc.wall_ms,
        identity_signature: [0; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    };

    resolver_a.update(owner_b, payload_b.clone(), hlc.clone());
    resolver_b.update(owner_a, payload_a.clone(), hlc.clone());

    // Set up A's link manager. The accept loop is wired even though
    // A is the dialer in this test — keeps the manager + endpoint in
    // their normal production configuration.
    let (tx_a, _rx_a) = flume::unbounded::<LinkUnicast>();
    let mgr_a = Arc::new(IrohZenohLinkManager::new(
        Arc::clone(&ep_a),
        resolver_a,
        tx_a,
    ));
    let _accept_a = mgr_a.spawn_accept_loop();

    // Set up B's link manager + accept loop. B is the acceptor — its
    // `rx_b` will receive the wrapped link once A dials in.
    let (tx_b, rx_b) = flume::unbounded::<LinkUnicast>();
    let mgr_b = Arc::new(IrohZenohLinkManager::new(
        Arc::clone(&ep_b),
        resolver_b,
        tx_b,
    ));
    let _accept_b = mgr_b.spawn_accept_loop();

    // A dials B. Locator format: `iroh/<lowercase-hex-32-bytes>` of
    // ep_b's EndpointId — matches `locator_from_endpoint_id` in
    // `zenoh_iroh_transport`. The resolver lookup keyed on the hex'd
    // EndpointId will hit `payload_b` and produce the dialable
    // `EndpointAddr` (id + direct_addresses).
    let endpoint_str = hex::encode(ep_b.node_id().as_bytes());
    let endpoint =
        EndPoint::new("iroh", &endpoint_str, "", "").expect("construct iroh endpoint locator");
    let link_a = mgr_a
        .new_link(endpoint)
        .await
        .expect("new_link must resolve owner_b and open the harmony/zenoh/v1 stream");

    // A writes a CRDT-shaped payload through its LinkUnicast wrapper.
    // The payload itself is arbitrary 16 bytes — we just need the
    // bytes to round-trip through both ends of the IrohZenohLink so
    // we know the full plumbing (send-stream, recv-stream, locator
    // assembly, accept-loop dispatch) works.
    link_a
        .write_all(b"hello-iroh-zenoh", None)
        .await
        .expect("write through LinkUnicast wrapper");

    // B's accept loop should have produced the matching link by now.
    // Bounded inner timeout — if accept-loop dispatch ever regresses,
    // we want to fail fast rather than waste the full outer 30s budget.
    let link_b = tokio::time::timeout(Duration::from_secs(10), rx_b.recv_async())
        .await
        .expect("rx_b receive timed out — accept loop did not dispatch link in 10s")
        .expect("rx_b channel closed unexpectedly");

    let mut buf = [0u8; 16];
    link_b
        .read_exact(&mut buf, None)
        .await
        .expect("read_exact through B's LinkUnicast wrapper");
    assert_eq!(
        &buf, b"hello-iroh-zenoh",
        "payload mis-roundtripped through Iroh+Zenoh wrappers"
    );

    // Optional reverse direction — B writes back, A reads. Validates
    // bi-directional flow through the bidi stream pair, not just
    // one-way fire-and-forget.
    link_b
        .write_all(b"ack-iroh-zenoh-1", None)
        .await
        .expect("B writes ack");
    let mut ack = [0u8; 16];
    link_a
        .read_exact(&mut ack, None)
        .await
        .expect("A reads ack");
    assert_eq!(&ack, b"ack-iroh-zenoh-1");

    // Graceful close discipline. `LinkUnicastTrait::close` is
    // documented (see `zenoh_iroh_link::IrohZenohLink::close`) to be
    // idempotent and to swallow `ClosedStream`, so both ends can
    // safely call it without coordination.
    link_a.close().await.expect("close A");
    link_b.close().await.expect("close B");
    drop(link_a);
    drop(link_b);

    // Endpoint shutdown closes all open connections cleanly. iroh's
    // `Endpoint::close` is idempotent, so the shutdowns can run
    // concurrently or sequentially — sequencing here is fine.
    ep_a.shutdown().await;
    ep_b.shutdown().await;
}
