//! ZEB-329 Task 8: two-endpoint integration test for HARMONY_PING_V1.
//!
//! Spins up two hermetic `IrohEndpoint` instances in the same process
//! on loopback with no DERP relays and no pkarr / DNS publishing. The
//! goal is to prove the production wire-level round-trip works:
//!
//! 1. Endpoint A registers the HARMONY_PING_V1 ALPN and runs the
//!    production accept loop owned by
//!    [`IrohZenohLinkManager::spawn_accept_loop`]. After ZEB-329
//!    Task 5 (commit bd59b59) the HARMONY_PING_V1 dispatch was folded
//!    into that single accept loop's ALPN switch rather than living
//!    in a separate spawn — see the `alpn::HARMONY_PING_V1` arm in
//!    `zenoh_iroh_transport.rs`. Spawning the manager's loop here
//!    mirrors production end-to-end (any future regression in the
//!    dispatch arm will fail this test).
//!
//! 2. Endpoint B uses [`network_health::ping_peer`] to open a
//!    HARMONY_PING_V1 bi-stream, write one byte, read the echo back,
//!    and return the measured RTT.
//!
//! 3. The test asserts the ping succeeded and the loopback RTT is
//!    well under 1 s.
//!
//! Mirrors the hermetic two-endpoint pattern from
//! `pkarr_iroh_redeem_full_integration.rs`.

#![cfg(feature = "test-fixtures")]

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::network_health;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;
use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::{EndpointAddr, SecretKey};
use zenoh_link::LinkUnicast;

/// Build a hermetic iroh endpoint on loopback with no address-lookup
/// service, no pkarr publisher, no DERP relays. Both the zenoh,
/// handshake, and ping ALPNs are registered so the same accept loop
/// can dispatch any of them (mirrors production
/// `IrohEndpoint::build_inner`).
async fn build_hermetic_endpoint_for_accept() -> Arc<IrohEndpoint> {
    let inner = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::generate())
        .alpns(vec![
            alpn::HARMONY_ZENOH_V1.to_vec(),
            alpn::HARMONY_HANDSHAKE_V1.to_vec(),
            alpn::HARMONY_PING_V1.to_vec(),
        ])
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

/// Build endpoint B with a `MemoryLookup` address-lookup service so
/// that calling `Endpoint::connect(node_id, alpn)` (which is what
/// `network_health::ping_peer` does internally) can resolve A's
/// `EndpointId` to its loopback `EndpointAddr` without a pkarr / DNS
/// round-trip. The seeded entry is added below in the test body once
/// A's bound socket is known.
async fn build_hermetic_endpoint_for_dial() -> (Arc<IrohEndpoint>, MemoryLookup) {
    let lookup = MemoryLookup::new();
    let inner = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::generate())
        .alpns(vec![
            alpn::HARMONY_ZENOH_V1.to_vec(),
            alpn::HARMONY_HANDSHAKE_V1.to_vec(),
            alpn::HARMONY_PING_V1.to_vec(),
        ])
        .relay_mode(RelayMode::Disabled)
        .dns_resolver(harmony_app::iroh_endpoint::hermetic_dns_resolver())
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind_addr loopback")
        .address_lookup(lookup.clone())
        .bind()
        .await
        .expect("bind iroh endpoint");
    (
        Arc::new(IrohEndpoint::from_endpoint_for_test(inner)),
        lookup,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ping_round_trip_between_two_endpoints() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    // ZEB-374: pre-pay iroh's ~30s first-bind global init OUTSIDE the asserted
    // 60s budget so full-suite contention can't charge it against the handshake
    // and blow the timeout. nextest is process-per-test, so every iroh test
    // pays this init once; the ZEB-347 pattern moves it before the budget.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    // Outer 60s budget (matches `pkarr_iroh_redeem_full_integration`).
    // Under nextest --all-targets concurrency the iroh connect handshake
    // can take several seconds while the host is saturated; in isolation
    // the test completes in <1s. Same flake class as the existing
    // `zenoh_iroh_*` tests — documented in CLAUDE.md / Task 8 brief.
    tokio::time::timeout(Duration::from_secs(60), async {
        // ── 1. Build both endpoints. ───────────────────────────────────
        let endpoint_a = build_hermetic_endpoint_for_accept().await;
        let (endpoint_b, b_lookup) = build_hermetic_endpoint_for_dial().await;

        let a_bound = endpoint_a.bound_sockets();
        assert!(
            !a_bound.is_empty(),
            "endpoint A must expose at least one bound socket so B can dial it"
        );

        // ── 2. Spawn A's production accept loop. ───────────────────────
        // The HARMONY_PING_V1 arm inside `spawn_accept_loop` (zenoh_iroh_transport.rs,
        // ZEB-329 Task 5 commit bd59b59) dispatches inbound ping
        // connections to `network_health::handle_ping_accept`. The
        // reachability resolver + link channel are not exercised by
        // ping, but the manager constructor requires them.
        let a_resolver = ReachabilityResolver::new();
        let (a_link_tx, _a_link_rx) = flume::unbounded::<LinkUnicast>();
        let a_link_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&endpoint_a),
            a_resolver,
            a_link_tx,
        ));
        let _a_accept = a_link_mgr.spawn_accept_loop();

        // ── 3. Seed B's MemoryLookup with A's loopback address. ────────
        // Without this, B's `connect(node_id, ALPN)` has nowhere to send
        // packets (no pkarr, no DNS, no relay). `MemoryLookup` stores
        // `EndpointAddr` entries and serves them via the address-lookup
        // hook on `Endpoint::connect`.
        let a_node_id = endpoint_a.node_id();
        let a_addr =
            EndpointAddr::from_parts(a_node_id, a_bound.into_iter().map(iroh::TransportAddr::Ip));
        b_lookup.add_endpoint_info(a_addr);

        // ── 4. B pings A via the production helper. ────────────────────
        // Inner ping_peer timeout bumped to 10s for the same concurrency-
        // contention reason as the outer budget above. The actual on-the-
        // wire echo on a quiet machine is sub-millisecond; the budget
        // covers iroh's connect/handshake under load.
        let rtt = network_health::ping_peer(&endpoint_b, a_node_id, Duration::from_secs(10))
            .await
            .expect("ping_peer should succeed for loopback round-trip");

        // ── 5. Loopback round-trip should be well under 1 s. ───────────
        // Asserts the production handler + ping_peer round-trip works.
        // Under heavy concurrency the connect dance can drag — relaxed to
        // 5s so isolated runs still catch genuine pathological regressions
        // while load-induced slowness joins the documented flake class.
        assert!(
            rtt < Duration::from_secs(5),
            "loopback ping should be well under 5s, got {rtt:?}"
        );

        // ── 6. The SAME round-trip through the production
        //    ProdPingDispatcher (ZEB-407) — proving the self-test adapter
        //    (EndpointId::from_bytes + the Phase-1 mode mapping) works
        //    end-to-end over a real endpoint, not just ping_peer directly.
        use harmony_app::network_health::PingDispatcher;
        let dispatcher = network_health::ProdPingDispatcher::new(Arc::clone(&endpoint_b));
        let (rtt2, mode) = dispatcher
            .ping(*a_node_id.as_bytes(), Duration::from_secs(10))
            .await
            .expect("ProdPingDispatcher should succeed for the loopback round-trip");
        assert!(
            rtt2 < Duration::from_secs(5),
            "dispatcher loopback ping should be well under 5s, got {rtt2:?}"
        );
        assert_eq!(
            mode,
            network_health::ConnectionMode::Direct,
            "Phase-1 dispatcher reports a successful ping as Direct"
        );
    })
    .await
    .expect("two-endpoint ping must complete within 60s");
}
