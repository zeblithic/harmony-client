//! ZEB-373 Task 6: dynamic mid-session iroh-dial acceptance integration test.
//!
//! Proves the NEW path end-to-end at the link layer: ONE real zenoh Runtime A
//! (built via [`event_loop::open_session_with_runtime`], with the iroh factory +
//! per-session ctx wired exactly like production in `lib.rs`/`event_loop.rs`)
//! starts with an EMPTY resolver, then learns peer B **mid-session** via
//! `resolver_a.update(...)`. That first-learn kicks the reconnect supervisor
//! (`NewPeer`, ZEB-620, the successor to ZEB-373's `DialHint` dial-once driver) →
//! `run_reconnect_supervisor` dials B through the live Runtime's `connect_peer`
//! (`RuntimePeerDialer`) → B's accept loop receives the inbound iroh link.
//!
//! ## Why ONE Runtime, not two
//!
//! The iroh session ctx (`set_iroh_session_ctx`) is a PROCESS-GLOBAL singleton
//! ("one node per process"), so two real Runtimes with iroh can't coexist in a
//! single test process. B is therefore a BARE [`IrohZenohLinkManager`] + accept
//! loop (no Runtime, no factory) — exactly how the existing two-engine test
//! (`community_reachability_two_engine_integration`) asserts at the link layer.
//! `nextest` runs each test in its own process, so the global ctx is safe here.
//!
//! ## What we assert (and what we DON'T)
//!
//! We assert on **B's accept loop receiving the inbound link** (`rx_b`), NOT on
//! `connect_peer`'s bool. B runs no zenoh `TransportManager`, so no zenoh
//! handshake completes and `connect_peer` may legitimately return `false` — but
//! the iroh QUIC stream still reaches B's accept loop, which is the link-layer
//! proof we want. We additionally assert the driver recorded a dial attempt.
//!
//! Helpers (`build_hermetic_endpoint`, the payload/Hlc/OwnerAddr shapes, the
//! `warm_up_iroh_global_init` + outer `tokio::time::timeout` flake-guard) are
//! reused verbatim from `community_reachability_two_engine_integration.rs`.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use harmony_app::iroh_dial_driver::RuntimePeerDialer;
use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::iroh_zenoh_registration::{
    ensure_iroh_factory_registered, iroh_listen_locator, merge_iroh_listen_endpoints,
    set_iroh_session_ctx, IrohSessionCtx,
};
use harmony_app::network_health::DialTelemetry;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::reconnect_supervisor::{
    run_reconnect_supervisor, SupervisorConfig, SupervisorHandle,
};
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;

use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use zenoh_link::LinkUnicast;

/// Build a hermetic iroh endpoint on loopback (no address-lookup, no pkarr, no
/// DERP relays). Reused verbatim from
/// `community_reachability_two_engine_integration::build_hermetic_endpoint`.
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

/// Build the `ReachabilityAnnouncePayload` for `ep`, mirroring the two-engine
/// test: `direct_addresses` come from `bound_sockets()` (hermetic endpoints
/// expose the loopback socket immediately; `direct_addresses()` is empty).
fn payload_for(ep: &IrohEndpoint, announced_at_ms: u64) -> ReachabilityAnnouncePayload {
    ReachabilityAnnouncePayload {
        iroh_node_id: *ep.node_id().as_bytes(),
        home_relay_url: ep.home_relay().map(|r| r.to_string()).unwrap_or_default(),
        direct_addresses: ep.bound_sockets(),
        announced_at_ms,
        identity_signature: [0; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// ZEB-402: this was `#[ignore]`d (2026-06-07, #201) because the real-iroh dial
// timed out under CI dial-latency spikes. The ROOT CAUSE was the iroh first-bind
// stalls (netdev CoreWLAN XPC + iroh's eager system-DNS-config read), fixed at
// the source in ZEB-626 (#396) — the same change that added the
// `hermetic_dns_resolver` this test now uses (see `build_hermetic_endpoint`).
// Post-fix the dial completes in ~130 ms on loopback: 20/20 in isolation and
// steady under concurrent real-iroh contention, ~150x headroom under the 20s
// inner budget. So the test is back on the blocking gate (the `#[ignore]` was
// simply left behind after ZEB-626). The tight inner budget stays deliberately
// tight — it fails fast on a routing regression (ZEB-373), not on live timing.
async fn mid_session_dial_connects_to_a_peer_learned_after_open() {
    // ZEB-347: prime the one-time process-global iroh bind init before the
    // asserted timeout (see `iroh_endpoint::warm_up_iroh_global_init`).
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;
    tokio::time::timeout(Duration::from_secs(60), inner())
        .await
        .expect("mid_session_dial must complete within 60s");
}

async fn inner() {
    // ── 1. Two hermetic endpoints on loopback. ─────────────────────────────
    let ep_a = build_hermetic_endpoint().await;
    let ep_b = build_hermetic_endpoint().await;

    // Sanity (same load-bearing assumption as the two-engine test): the
    // hermetic endpoint exposes its bound socket immediately, otherwise the
    // dialer would have nothing to target and we'd hang the full budget.
    assert!(
        !ep_b.bound_sockets().is_empty(),
        "ep_b.bound_sockets() must be non-empty — dialer would have no target"
    );

    let hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "fix".into(),
    };

    // ── 2. B: BARE manager + accept loop (no Runtime, no factory). ─────────
    // `rx_b` captures inbound links A dials in. B's resolver is irrelevant
    // (B never dials), so a fresh empty one is fine.
    let (tx_b, rx_b) = flume::unbounded::<LinkUnicast>();
    let mgr_b = Arc::new(IrohZenohLinkManager::new(
        Arc::clone(&ep_b),
        ReachabilityResolver::new(),
        tx_b,
    ));
    let _accept_b = mgr_b.spawn_accept_loop();

    // ── 3. A: manager + accept loop + REAL Runtime via factory/ctx path. ───
    // resolver_a starts EMPTY — no static seed. It learns B mid-session below.
    let resolver_a = ReachabilityResolver::new();
    let (tx_a, rx_a) = flume::unbounded::<LinkUnicast>();
    let mgr_a = Arc::new(IrohZenohLinkManager::new(
        Arc::clone(&ep_a),
        resolver_a.clone(),
        tx_a,
    ));
    let _accept_a = mgr_a.spawn_accept_loop();

    // Wire the global iroh factory + per-session ctx exactly like lib.rs does
    // before zenoh::open: the ctx holds A's manager (for outbound new_link) and
    // the SAME rx whose tx the manager was built with (inbound forwarder feed).
    ensure_iroh_factory_registered();
    set_iroh_session_ctx(IrohSessionCtx {
        manager: Arc::clone(&mgr_a),
        new_link_rx: rx_a,
    });

    // Build A's zenoh Config: listen on A's own iroh locator so the factory runs
    // at open (mirrors event_loop's merge_iroh_listen_endpoints). connect/endpoints
    // is left empty — A learns + dials B strictly mid-session, not via static seed.
    let mut cfg_a = zenoh::Config::default();
    let self_loc = iroh_listen_locator(ep_a.node_id().as_bytes());
    let current = cfg_a.get_json("listen/endpoints").ok();
    let eps = merge_iroh_listen_endpoints(current.as_deref(), &self_loc, "peer");
    cfg_a
        .insert_json5("listen/endpoints", &eps)
        .expect("insert listen/endpoints");
    // Hermetic: no multicast scouting, no gossip — keep the session local.
    cfg_a
        .insert_json5("scouting/multicast/enabled", "false")
        .expect("disable multicast scouting");

    let (rt_a, _sess_a) = harmony_app::event_loop::open_session_with_runtime(cfg_a)
        .await
        .expect("A opens its zenoh session via open_session_with_runtime");

    // ── 4. Supervisor on A: install the handle + spawn run_reconnect_supervisor. ─
    // ZEB-620: the resolver kicks the supervisor directly (no DialHint mpsc). A
    // fast config so the first dial fires within the 20s budget regardless of the
    // A/B NodeId dial-role ordering (base + fallback both ~200ms).
    let telemetry = Arc::new(DialTelemetry::new());
    let handle = SupervisorHandle::new();
    resolver_a.set_supervisor(handle.clone());
    let dialer = Arc::new(RuntimePeerDialer::new(rt_a.clone()));
    let self_nid = *ep_a.node_id().as_bytes();
    let config = SupervisorConfig {
        retry_base: Duration::from_millis(200),
        retry_cap: Duration::from_secs(4),
        dormant_after: Duration::from_secs(3600),
        presence_sweep_cooldown: Duration::from_secs(30),
        max_concurrent_dials: 4,
        // Real-time test over live endpoints: keep the dial bound far above the
        // worst contention-stretched dial (nextest iroh-endpoint throttle group)
        // so it never fires here — a slow hermetic dial is not a hung dial.
        dial_timeout: Duration::from_secs(300),
        higher_id_fallback_delay: Duration::from_millis(200),
        // ZEB-910: parole disabled for this real-time test (never fires inside
        // its wall-clock window).
        parole_interval: Duration::from_secs(3600),
        parole_batch: 2,
        jitter_seed: Some(0xD1A1),
    };
    tokio::spawn(run_reconnect_supervisor(
        handle.clone(),
        dialer,
        Arc::new(resolver_a.clone()),
        Arc::clone(&telemetry),
        self_nid,
        config,
    ));

    // ── 5. MID-SESSION: A learns B → first-learn kicks the supervisor `NewPeer`. ─
    resolver_a.update(OwnerAddr([0xBB; 16]), payload_for(&ep_b, hlc.wall_ms), hlc);

    // ── 6. Assert B's accept loop received an inbound link from A's dial. ──
    // connect_peer may return false (B runs no zenoh TransportManager) — that's
    // EXPECTED. The iroh QUIC stream still reaches B's accept loop, which is the
    // link-layer proof. Bounded inner timeout to fail fast on a routing regression.
    let link = tokio::time::timeout(Duration::from_secs(20), rx_b.recv_async())
        .await
        .expect("B did not receive an inbound link from A's dynamic dial within 20s")
        .expect("rx_b channel closed unexpectedly");
    let _ = link;

    // The supervisor recorded at least one dial attempt for the mid-session learn.
    assert!(
        telemetry.summary().attempts >= 1,
        "supervisor must record at least one dial attempt for the mid-session-learned peer"
    );

    // Clean shutdown of both endpoints (closes open connections cleanly).
    ep_a.shutdown().await;
    ep_b.shutdown().await;
}
