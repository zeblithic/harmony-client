//! ZEB-375 (Friends Phase 2a, Task 8): two-node end-to-end referral-catalog
//! browse over the `harmony/friend-pex/v1` iroh ALPN.
//!
//! This is the live-connection counterpart to the pure-decision unit tests in
//! `src/iroh_pex_acceptor.rs` and `src/referral_catalog.rs`. Its whole purpose is
//! to validate Tasks 4 (the `IrohFriendPexAcceptor::serve` handler) + 5 (the
//! ALPN/dispatcher wiring: `route_handshake_alpn` → `MultiplexHandshakeDispatcher`
//! pex slot, and the accept-loop allowlist branch in `zenoh_iroh_transport.rs`)
//! by driving a REAL dial over a REAL iroh connection into the REAL acceptor.
//!
//! Harness mirrors `friend_token_roundtrip_integration.rs`: the server `A` builds
//! an `IrohEndpoint` + `IrohZenohLinkManager`, installs a
//! `MultiplexHandshakeDispatcher` on its accept loop, and answers inbound PEX
//! dials. Unlike that test (which drives the FRIEND ALPN and puts the real
//! acceptor in the friend slot), here the real `IrohFriendPexAcceptor` lives in
//! the multiplexer's PEX slot and the invite+friend slots are no-op stubs — only
//! the friend-PEX ALPN is ever dialed.
//!
//! The browser/dialer side uses a raw `iroh::Endpoint` (the `IrohEndpoint`
//! wrapper's `.inner()` is `pub(crate)`, unreachable from an integration crate),
//! synthesizes the server's `EndpointAddr` from its public `node_id()` +
//! `bound_sockets()` (no pkarr needed — both endpoints are in-process on
//! loopback), and performs the exact `connect` + `open_bi` + `[u32 LE len][body]`
//! framing the production browse path (`lib.rs::browse_friend_referrals_*`) uses.
//!
//! ZEB-374 / ZEB-347: two-endpoint iroh tests flake under contention. Run
//! serially (`#[serial_test::serial]` + `--test-threads 1`) and give every
//! connect/open_bi/read a generous (30s) timeout, all under a generous outer
//! timeout. Never weaken an assertion to paper over a timing flake — bump the
//! timeout instead.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_membership::mint_test_owner;
use harmony_app::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};
use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::iroh_friend_acceptor::MultiplexHandshakeDispatcher;
use harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher;
use harmony_app::iroh_pex_acceptor::IrohFriendPexAcceptor;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::Hlc;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::referral_catalog::{
    decode_referral_catalog, encode_catalog_request, sign_catalog_request, verify_referral_catalog,
    ReferralCatalog, PEX_MAX_PACKET_LEN,
};
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;
use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use tokio::sync::Mutex as TokioMutex;
use zenoh_link::LinkUnicast;

/// Generous per-IO and outer timeouts. ZEB-374: two-endpoint iroh tests flake
/// under contention; these are deliberately fat so a wedged dial fails the test
/// rather than masking a real wiring bug behind a too-tight deadline.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
// ZEB-626: back to the pre-ZEB-619 90s. The "drain teardown" that forced
// 180s was actually two synchronous macOS XPC stalls at first bind
// (netdev's CoreWLAN->wifid query + iroh's eager SystemConfiguration/
// configd DNS read), removed at the source (vendor/netdev +
// hermetic_dns_resolver); this test now runs in ~0.06s isolated, so 90s
// is >1000x headroom without weakening any assertion.
const OUTER_TIMEOUT: Duration = Duration::from_secs(90);

/// Build a hermetic SERVER iroh endpoint (the `IrohEndpoint` wrapper the link
/// manager + accept loop need) on loopback with no address-lookup, no pkarr, no
/// relays. Advertises the friend-PEX ALPN (plus the zenoh + handshake + friend
/// ALPNs the accept loop matches) so an inbound PEX dial negotiates and reaches
/// the multiplexer. Mirrors `friend_token_roundtrip`'s `build_hermetic_endpoint`.
async fn build_server_endpoint() -> Arc<IrohEndpoint> {
    let secret = SecretKey::generate();
    let inner = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![
            alpn::HARMONY_ZENOH_V1.to_vec(),
            alpn::HARMONY_HANDSHAKE_V1.to_vec(),
            alpn::HARMONY_FRIEND_V1.to_vec(),
            alpn::HARMONY_FRIEND_PEX_V1.to_vec(),
        ])
        .relay_mode(RelayMode::Disabled)
        .dns_resolver(harmony_app::iroh_endpoint::hermetic_dns_resolver())
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind_addr loopback")
        .bind()
        .await
        .expect("bind server iroh endpoint");
    Arc::new(IrohEndpoint::from_endpoint_for_test(inner))
}

/// Build a hermetic raw CLIENT iroh endpoint (the dialer). We need full
/// `iroh::Endpoint::connect` access, which the `IrohEndpoint` wrapper only
/// exposes via the in-crate `pub(crate) inner()` — unreachable from this
/// integration crate. So the browser drives a raw endpoint directly.
async fn build_client_endpoint() -> Endpoint {
    let secret = SecretKey::generate();
    Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![alpn::HARMONY_FRIEND_PEX_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .dns_resolver(harmony_app::iroh_endpoint::hermetic_dns_resolver())
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind_addr loopback")
        .bind()
        .await
        .expect("bind client iroh endpoint")
}

/// The 32-byte master Ed25519 verify key for a `mint_test_owner(seed)` identity.
/// `mint_test_owner` derives the master key from `[seed; 32]`, so the
/// `FriendEntry.master_ed25519` invariant (map key == owner_id derived from this
/// key) holds by construction when we key the entry on that owner's `OwnerAddr`.
fn master_verify_key(seed: u8) -> [u8; 32] {
    SigningKey::from_bytes(&[seed; 32])
        .verifying_key()
        .to_bytes()
}

/// A FriendEntry seeded directly into the server's friend graph. `master_ed25519`
/// is the friend's REAL master verify key so the `owner_id == owner_id_from_master`
/// invariant holds; we insert directly (the same path the `iroh_pex_acceptor` /
/// `referral_catalog` unit tests use) rather than via `apply_friend_update`,
/// because the serve-decision reads `friend_graph.friends` directly and only keys
/// on `status` + `referrable`.
fn friend_entry(seed: u8, status: FriendStatus, referrable: bool, display: &str) -> FriendEntry {
    FriendEntry {
        master_ed25519: master_verify_key(seed),
        display: Some(display.to_string()),
        status,
        established_via: FriendOrigin::Token,
        referrable,
        learned_at: Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "seed".into(),
        },
        sealed_secret: None,
    }
}

/// Synthesize the server's dialable `EndpointAddr` from its public identity +
/// bound loopback sockets (no relay, no pkarr — both endpoints share the
/// process). Mirrors the inline synthesis in `lib.rs`'s friend-PEX browse path.
fn server_addr(server_ep: &IrohEndpoint) -> iroh::EndpointAddr {
    let bound = server_ep.bound_sockets();
    assert!(
        !bound.is_empty(),
        "server endpoint must expose bound_sockets() for the dialer"
    );
    let mut addr = iroh::EndpointAddr::new(server_ep.node_id());
    for sock in bound {
        addr = addr.with_ip_addr(sock);
    }
    addr
}

/// Dial the server on the friend-PEX ALPN, write the framed request, read the
/// framed `ReferralCatalog`. Mirrors the production browse path's framing exactly
/// (`[u32 LE len][body]`, `finish()`, bounded read by `PEX_MAX_PACKET_LEN`).
async fn browse_catalog(
    client_ep: &Endpoint,
    target: iroh::EndpointAddr,
    request_bytes: &[u8],
) -> ReferralCatalog {
    let conn = tokio::time::timeout(
        IO_TIMEOUT,
        client_ep.connect(target, alpn::HARMONY_FRIEND_PEX_V1),
    )
    .await
    .expect("connect did not time out")
    .expect("connect to server on friend-pex ALPN");

    let (mut send, mut recv) = tokio::time::timeout(IO_TIMEOUT, conn.open_bi())
        .await
        .expect("open_bi did not time out")
        .expect("open_bi on the friend-pex connection");

    // Write [u32 LE len][body] + finish().
    let len = request_bytes.len() as u32;
    tokio::time::timeout(IO_TIMEOUT, send.write_all(&len.to_le_bytes()))
        .await
        .expect("write length-prefix did not time out")
        .expect("write request length-prefix");
    tokio::time::timeout(IO_TIMEOUT, send.write_all(request_bytes))
        .await
        .expect("write body did not time out")
        .expect("write request body");
    send.finish().expect("finish send stream");

    // Read [u32 LE len][body], bounded by PEX_MAX_PACKET_LEN.
    let body = tokio::time::timeout(IO_TIMEOUT, async {
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf)
            .await
            .expect("read response length-prefix");
        let len = u32::from_le_bytes(len_buf) as usize;
        assert!(
            len > 0 && len <= PEX_MAX_PACKET_LEN,
            "response length out of bounds: len={len} max={PEX_MAX_PACKET_LEN}"
        );
        let mut body = vec![0u8; len];
        recv.read_exact(&mut body)
            .await
            .expect("read response body");
        body
    })
    .await
    .expect("read response did not time out");

    conn.close(0u32.into(), b"browse-complete");
    decode_referral_catalog(&body).expect("decode ReferralCatalog from server")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn referral_catalog_roundtrip_serves_referrable_friends_over_iroh() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    tokio::time::timeout(OUTER_TIMEOUT, async {
        // ── 1. Identities. ──────────────────────────────────────────────
        let a = mint_test_owner(0xA1); // SERVER (catalog author)
        let b = mint_test_owner(0xB2); // BROWSER + an Active friend of A (not referrable)
        let x = mint_test_owner(0xC3); // Active + referrable friend of A (SHOULD appear)
        let y = mint_test_owner(0xD4); // Active but NOT-referrable friend of A (absent)
        let c = mint_test_owner(0xE5); // NOT in A's friend graph (Case 2)

        // ── 2. A's owner-state, seeded friend graph. ────────────────────
        // Direct inserts (the path the iroh_pex_acceptor / referral_catalog unit
        // tests use). Each entry's master_ed25519 is the friend's REAL master
        // verify key so the friend-graph key invariant holds.
        let mut a_owner_state = OwnerState::default();
        a_owner_state.friend_graph.friends.insert(
            b.owner,
            friend_entry(0xB2, FriendStatus::Active, false, "bob"),
        );
        a_owner_state.friend_graph.friends.insert(
            x.owner,
            friend_entry(0xC3, FriendStatus::Active, true, "xena"),
        );
        a_owner_state.friend_graph.friends.insert(
            y.owner,
            friend_entry(0xD4, FriendStatus::Active, false, "yuri"),
        );

        // ── 3. A's iroh endpoint + link manager + accept loop. ──────────
        let a_ep = build_server_endpoint().await;
        let a_reachability = ReachabilityResolver::new();
        let (a_link_tx, _a_link_rx) = flume::unbounded::<LinkUnicast>();
        let a_link_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&a_ep),
            a_reachability.clone(),
            a_link_tx,
        ));
        let a_accept = a_link_mgr.spawn_accept_loop();

        // ── 4. The REAL PEX acceptor, installed via the multiplexer. ────
        // Real IrohFriendPexAcceptor in the PEX slot; no-op stubs in the
        // invite + friend slots (only the friend-PEX ALPN is dialed here).
        let pex_acceptor: Arc<dyn IrohHandshakeDispatcher> = Arc::new(IrohFriendPexAcceptor::new(
            Arc::new(TokioMutex::new(a_owner_state)),
            Arc::new(TokioMutex::new(BTreeMap::<String, Hlc>::new())),
            "a-dev".to_string(),
            a.owner,
            a.cert.clone(),
            Arc::new(SigningKey::from_bytes(&a.device_key.to_bytes())),
        ));
        let invite_stub: Arc<dyn IrohHandshakeDispatcher> = Arc::new(NoopDispatcher);
        let friend_stub: Arc<dyn IrohHandshakeDispatcher> = Arc::new(NoopDispatcher);
        let a_dispatcher: Arc<dyn IrohHandshakeDispatcher> = Arc::new(
            MultiplexHandshakeDispatcher::new(invite_stub, friend_stub, pex_acceptor),
        );
        if a_link_mgr
            .install_handshake_dispatcher(a_dispatcher)
            .await
            .is_err()
        {
            panic!("first install must succeed (OnceCell empty)");
        }

        let target = server_addr(&a_ep);

        // ── Case 1 — Active friend B browses A. ─────────────────────────
        let b_ep = build_client_endpoint().await;
        let b_req = sign_catalog_request(&b.device_key, b.owner, a.owner, b.cert.clone());
        let b_req_bytes = encode_catalog_request(&b_req).expect("encode B's catalog request");
        let cat = browse_catalog(&b_ep, target.clone(), &b_req_bytes).await;

        // The catalog must be validly signed by A and subject-bound to B.
        verify_referral_catalog(
            &cat,
            a.owner,
            b.owner,
            &harmony_app::revoked_device_projection::RevokedDeviceProjection::new(),
            0,
        )
        .expect("Case 1: catalog must verify as authored by A, addressed to B");
        // Only X (Active + referrable) is served. Y is excluded (not referrable);
        // B is excluded (B is referrable=false, and is the requester).
        assert_eq!(
            cat.entries.len(),
            1,
            "Case 1: exactly one referrable friend (X) is served, got {:?}",
            cat.entries
        );
        assert_eq!(
            cat.entries[0].peer_owner, x.owner,
            "Case 1: the single served entry must be X (the referrable friend)"
        );
        assert!(
            !cat.entries.iter().any(|e| e.peer_owner == y.owner),
            "Case 1: Y (not referrable) must never be served"
        );
        assert!(
            !cat.entries.iter().any(|e| e.peer_owner == b.owner),
            "Case 1: B (the requester, not referrable) must never be served"
        );
        b_ep.close().await;

        // ── Case 2 — non-friend C gets an empty (benign) catalog. ───────
        let c_ep = build_client_endpoint().await;
        let c_req = sign_catalog_request(&c.device_key, c.owner, a.owner, c.cert.clone());
        let c_req_bytes = encode_catalog_request(&c_req).expect("encode C's catalog request");
        let cat = browse_catalog(&c_ep, target.clone(), &c_req_bytes).await;

        // Still a validly signed catalog, subject-bound to C — but EMPTY: a
        // non-friend leaks NOTHING about A's referrable friends.
        verify_referral_catalog(
            &cat,
            a.owner,
            c.owner,
            &harmony_app::revoked_device_projection::RevokedDeviceProjection::new(),
            0,
        )
        .expect("Case 2: empty catalog must verify as authored by A, addressed to C");
        assert!(
            cat.entries.is_empty(),
            "Case 2: a non-friend must receive an empty catalog (no leak), got {:?}",
            cat.entries
        );
        c_ep.close().await;

        // ── Teardown. ───────────────────────────────────────────────────
        a_accept.abort();
        a_ep.shutdown().await;
    })
    .await
    .expect("referral_catalog_roundtrip timed out");
}

/// No-op dispatcher for the multiplexer's invite + friend slots — this test only
/// ever dials the friend-PEX ALPN, so those branches are never exercised.
struct NoopDispatcher;

#[async_trait::async_trait]
impl IrohHandshakeDispatcher for NoopDispatcher {
    async fn handle_connection(&self, _conn: iroh::endpoint::Connection) {}
}
