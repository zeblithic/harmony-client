//! ZEB-368: seam tests for the iroh↔Zenoh transport registration.
//!
//! Verifies the four load-bearing seams that let the live Zenoh session own
//! iroh as a first-class unicast transport via the vendored `zenoh-link` fork
//! plus the harmony-side `iroh_zenoh_registration` module:
//!
//! 1. **Dispatch routing** — `iroh/<hex>` locators parse to
//!    [`zenoh_link::LinkKind::Iroh`] and live in the supported-link set.
//! 2. **Factory wiring** — [`zenoh_link::LinkManagerBuilderUnicast::make`]
//!    errors (never panics) when no ctx is set, and returns `Ok(manager)`
//!    once a ctx is installed via [`harmony_app::iroh_zenoh_registration::set_iroh_session_ctx`].
//! 3. **Boot-seed builder** — [`harmony_app::iroh_zenoh_registration::boot_seed_node_ids_by_recency`]
//!    dedups peers and skips self (ZEB-620 successor to `iroh_connect_locators`).
//! 4. **Real `zenoh::open`** — a session opened with an `iroh/<hex>` listen
//!    endpoint succeeds, proving the factory routed the iroh listener through
//!    Zenoh's transport-manager setup without panicking or bailing.
//!
//! Plus a best-effort forwarder-delivery test (5).
//!
//! ## Test isolation (CRITICAL)
//!
//! The factory + ctx are a process-global singleton ([`std::sync::OnceLock`]).
//! `cargo nextest` runs each test in its OWN process, so each test gets fresh
//! globals — which is exactly what these tests require. The "no factory" test
//! (2a) must observe an unregistered factory; under nextest it is always
//! isolated from the tests that register one, so no guard is needed. (Under
//! plain `cargo test`, tests sharing a process would clobber the global; we
//! only run via nextest — see `CLAUDE.md`.)
//!
//! Patterns (hermetic endpoint build, ALPN, real `LinkUnicast` via loopback
//! dial) mirror `tests/community_reachability_two_engine_integration.rs`.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::iroh_zenoh_registration::{
    boot_seed_node_ids_by_recency, ensure_iroh_factory_registered, set_iroh_session_ctx,
    IrohSessionCtx,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;

use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use zenoh_link::{EndPoint, LinkManagerUnicastTrait, LinkUnicast};

/// Build a hermetic iroh endpoint on loopback with no address-lookup, no pkarr
/// publisher, no DERP relays. Identical to the helper in the two-engine
/// integration test: goes through [`IrohEndpoint::from_endpoint_for_test`]
/// (gated behind `feature = "test-fixtures"`) so the rest of the test uses the
/// same `IrohEndpoint` wrapper production code uses.
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

/// Build the `iroh/<hex>` `EndPoint` locator for an endpoint's own node id.
fn self_iroh_endpoint(ep: &IrohEndpoint) -> EndPoint {
    let hex = hex::encode(ep.node_id().as_bytes());
    EndPoint::new("iroh", &hex, "", "").expect("construct iroh self endpoint locator")
}

// ──────────────────────────────────────────────────────────────────────────
// (1) Dispatch routing
// ──────────────────────────────────────────────────────────────────────────

/// `iroh/<hex>` parses to `LinkKind::Iroh`, and Iroh is in the supported set.
#[test]
fn iroh_locator_dispatches_to_iroh_linkkind() {
    let hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let ep = zenoh_link::EndPoint::new("iroh", hex, "", "").expect("iroh endpoint");
    assert_eq!(
        zenoh_link::LinkKind::try_from(&ep).unwrap(),
        zenoh_link::LinkKind::Iroh
    );
    assert!(zenoh_link::ALL_SUPPORTED_LINKS.contains(&zenoh_link::LinkKind::Iroh));
}

// ──────────────────────────────────────────────────────────────────────────
// (2) Factory wiring
// ──────────────────────────────────────────────────────────────────────────

/// With no factory ctx set, `make` for an iroh endpoint must return an `Err`
/// (the registered factory closure can't find a ctx, or the factory itself is
/// unregistered) — never a panic. Under nextest this test runs in its own
/// process, so the global factory slot is pristine. (See module isolation note.)
#[test]
fn make_without_factory_errors_not_panics() {
    let (tx, _rx) = flume::unbounded::<zenoh_link::LinkUnicast>();
    let ep = zenoh_link::EndPoint::new(
        "iroh",
        "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        "",
        "",
    )
    .unwrap();
    let res = zenoh_link::LinkManagerBuilderUnicast::make(tx, &ep);
    assert!(res.is_err(), "missing iroh factory must error, not panic");
}

/// With a ctx set + the factory registered, `make` for an iroh endpoint finds
/// the ctx and returns `Ok(manager)` (the harmony manager handed back to
/// Zenoh for outbound `new_link`). This runs in its own nextest process, so
/// registering the factory here does not bleed into (2a).
#[tokio::test]
async fn make_with_ctx_returns_manager() {
    let ep = build_hermetic_endpoint().await;
    let resolver = ReachabilityResolver::new();

    // The manager's own accept-loop sender. The ctx's `new_link_rx` is the
    // matching receiver — the factory clones it for the inbound forwarder.
    let (mgr_tx, mgr_rx) = flume::unbounded::<LinkUnicast>();
    let manager = Arc::new(IrohZenohLinkManager::new(Arc::clone(&ep), resolver, mgr_tx));

    set_iroh_session_ctx(IrohSessionCtx {
        manager: Arc::clone(&manager),
        new_link_rx: mgr_rx,
    });
    ensure_iroh_factory_registered();

    // Zenoh's accept-queue sender (what the real TransportManager owns). The
    // factory spawns a forwarder draining the ctx's `new_link_rx` into this.
    let (zenoh_tx, _zenoh_rx) = flume::unbounded::<LinkUnicast>();
    let iroh_self_ep = self_iroh_endpoint(&ep);
    let res = zenoh_link::LinkManagerBuilderUnicast::make(zenoh_tx, &iroh_self_ep);
    assert!(
        res.is_ok(),
        "factory found ctx and returned the iroh manager: {:?}",
        res.err()
    );

    ep.shutdown().await;
}

// ──────────────────────────────────────────────────────────────────────────
// (3) Outbound builder
// ──────────────────────────────────────────────────────────────────────────

/// ZEB-620: `boot_seed_node_ids_by_recency` produces one node-id per distinct
/// peer, deduped across owners, with self skipped (the successor to ZEB-368's
/// `iroh_connect_locators`, which returned `iroh/<hex>` strings for zenoh's
/// static connect set — boot peers now seed the reconnect supervisor instead).
#[test]
fn boot_seed_node_ids_dedup_and_skip_self() {
    let resolver = ReachabilityResolver::new();
    let hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "fix".into(),
    };
    let self_nid = [0xAAu8; 32];
    let peer_nid = [0xBBu8; 32];
    let mk = |nid| ReachabilityAnnouncePayload {
        iroh_node_id: nid,
        home_relay_url: String::new(),
        direct_addresses: vec![],
        announced_at_ms: hlc.wall_ms,
        identity_signature: [0; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    };
    resolver.update(OwnerAddr([0x01; 16]), mk(self_nid), hlc.clone());
    resolver.update(OwnerAddr([0x02; 16]), mk(peer_nid), hlc.clone());
    resolver.update(OwnerAddr([0x03; 16]), mk(peer_nid), hlc.clone());
    assert_eq!(
        boot_seed_node_ids_by_recency(&resolver, &self_nid),
        vec![peer_nid]
    );
}

// ──────────────────────────────────────────────────────────────────────────
// (4) Real `zenoh::open` with an iroh listener
// ──────────────────────────────────────────────────────────────────────────

/// A session opened with an `iroh/<hex>` listen endpoint succeeds. This proves
/// the vendored fork routed the iroh listener through Zenoh's transport-manager
/// listener setup (via our registered factory) without panicking or bailing.
///
/// Bonus listener-info assertion is omitted: zenoh 1.9 `Session` exposes no
/// public accessor for active local listeners — only `config()`, which merely
/// echoes the input config, and a private runtime/transport state. So the
/// load-bearing assertion is `zenoh::open` returning `Ok`.
///
/// Multi-thread flavor is REQUIRED: zenoh's runtime panics under tokio's
/// current-thread scheduler (`zenoh-runtime` rejects it explicitly), and the
/// default `#[tokio::test]` is current-thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zenoh_open_with_iroh_listener_succeeds() {
    // ZEB-347: prime the one-time process-global iroh bind init before any
    // asserted timeout (see `iroh_endpoint::warm_up_iroh_global_init`).
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    let ep = build_hermetic_endpoint().await;
    let resolver = ReachabilityResolver::new();

    let (mgr_tx, mgr_rx) = flume::unbounded::<LinkUnicast>();
    let manager = Arc::new(IrohZenohLinkManager::new(Arc::clone(&ep), resolver, mgr_tx));
    // Wire the accept loop so the manager is in its production configuration
    // when Zenoh's listener creation drives it.
    let _accept = manager.spawn_accept_loop();

    set_iroh_session_ctx(IrohSessionCtx {
        manager: Arc::clone(&manager),
        new_link_rx: mgr_rx,
    });
    ensure_iroh_factory_registered();

    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    let self_loc = format!("\"iroh/{}\"", hex::encode(ep.node_id().as_bytes()));
    config
        .insert_json5("listen/endpoints", &format!("[{self_loc}]"))
        .unwrap();

    let session = tokio::time::timeout(Duration::from_secs(30), zenoh::open(config))
        .await
        .expect("zenoh::open within 30s")
        .expect("zenoh::open Ok with iroh listener");

    session.close().await.expect("close");
    ep.shutdown().await;
}

// ──────────────────────────────────────────────────────────────────────────
// (5) Forwarder delivers a real inbound link (best-effort)
// ──────────────────────────────────────────────────────────────────────────

/// The factory spawns a forwarder that drains the ctx's `new_link_rx` into
/// Zenoh's `NewLinkChannelSender`. We obtain a REAL `LinkUnicast` via a
/// loopback dial (B dials A's manager; A's accept loop yields a wrapped link
/// on its `new_link_rx`), and assert it surfaces on Zenoh's receiver — proving
/// the inbound path is end-to-end live, not just factory-Ok.
#[tokio::test]
async fn forwarder_delivers_real_inbound_link() {
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;
    tokio::time::timeout(Duration::from_secs(30), forwarder_inner())
        .await
        .expect("forwarder_delivers_real_inbound_link must complete within 30s");
}

async fn forwarder_inner() {
    // A is the acceptor whose manager owns the ctx. Its accept loop pushes the
    // wrapped inbound link onto `acceptor_rx` — which is the ctx's `new_link_rx`.
    let ep_a = build_hermetic_endpoint().await;
    let resolver_a = ReachabilityResolver::new();
    let (acceptor_tx, acceptor_rx) = flume::unbounded::<LinkUnicast>();
    let mgr_a = Arc::new(IrohZenohLinkManager::new(
        Arc::clone(&ep_a),
        resolver_a,
        acceptor_tx,
    ));
    let _accept_a = mgr_a.spawn_accept_loop();

    // Install the ctx pointing at A's manager + A's accept-loop receiver, then
    // register the factory and invoke `make` so the forwarder is spawned. The
    // forwarder drains `acceptor_rx` into `zenoh_tx`.
    let (zenoh_tx, zenoh_rx) = flume::unbounded::<LinkUnicast>();
    set_iroh_session_ctx(IrohSessionCtx {
        manager: Arc::clone(&mgr_a),
        new_link_rx: acceptor_rx,
    });
    ensure_iroh_factory_registered();
    let iroh_self_ep = self_iroh_endpoint(&ep_a);
    zenoh_link::LinkManagerBuilderUnicast::make(zenoh_tx, &iroh_self_ep)
        .expect("make spawns the inbound forwarder");

    // B dials A. B's resolver is seeded with A's reachability so its
    // `new_link` can build the dialable `EndpointAddr`.
    let ep_b = build_hermetic_endpoint().await;
    let ep_a_sockets = ep_a.bound_sockets();
    assert!(
        !ep_a_sockets.is_empty(),
        "ep_a.bound_sockets() must be non-empty — dialer would have nothing to target"
    );
    let resolver_b = ReachabilityResolver::new();
    let hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "fix".into(),
    };
    let payload_a = ReachabilityAnnouncePayload {
        iroh_node_id: *ep_a.node_id().as_bytes(),
        home_relay_url: ep_a.home_relay().map(|r| r.to_string()).unwrap_or_default(),
        direct_addresses: ep_a_sockets,
        announced_at_ms: hlc.wall_ms,
        identity_signature: [0; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    };
    resolver_b.update(OwnerAddr([0xBB; 16]), payload_a, hlc);

    let (tx_b, _rx_b) = flume::unbounded::<LinkUnicast>();
    let mgr_b = Arc::new(IrohZenohLinkManager::new(
        Arc::clone(&ep_b),
        resolver_b,
        tx_b,
    ));
    let _accept_b = mgr_b.spawn_accept_loop();

    let dial_ep = EndPoint::new("iroh", hex::encode(ep_a.node_id().as_bytes()), "", "")
        .expect("construct dial locator for ep_a");
    let link_b = mgr_b
        .new_link(dial_ep)
        .await
        .expect("B opens the harmony/zenoh/v1 stream to A");

    // Drive a byte so A's accept loop wakes and wraps the inbound connection.
    link_b
        .write_all(b"fwd-probe-0001!!", None)
        .await
        .expect("B writes probe through its LinkUnicast wrapper");

    // The forwarder should move A's accepted inbound link onto Zenoh's receiver.
    let forwarded = tokio::time::timeout(Duration::from_secs(10), zenoh_rx.recv_async())
        .await
        .expect("forwarder did not deliver inbound link to zenoh_rx within 10s")
        .expect("zenoh_rx channel closed unexpectedly");

    // The forwarded link is A's wrapped inbound link. Read the probe byte to
    // confirm it's the live link (not a placeholder).
    let mut buf = [0u8; 16];
    forwarded
        .read_exact(&mut buf, None)
        .await
        .expect("read_exact through forwarded LinkUnicast wrapper");
    assert_eq!(
        &buf, b"fwd-probe-0001!!",
        "forwarded link did not carry the probe bytes"
    );

    link_b.close().await.expect("close B");
    forwarded.close().await.expect("close forwarded");
    drop(link_b);
    drop(forwarded);
    ep_a.shutdown().await;
    ep_b.shutdown().await;
}
