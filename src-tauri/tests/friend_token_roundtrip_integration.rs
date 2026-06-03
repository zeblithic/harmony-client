//! ZEB-370 Phase 1 (Task 10): two-node end-to-end friend-link redemption over
//! the `harmony/friend/v1` iroh ALPN, with Case-A pkarr resolution.
//!
//! Mirrors the two-node iroh+pkarr harness in
//! `pkarr_iroh_redeem_full_integration.rs`, but for the friend handshake — which
//! is markedly simpler than the community-invite handshake: there is NO shared
//! community CRDT, no engine, no CAS. Each party writes a `FriendEntry` into its
//! OWN owner-state.
//!
//! Flow exercised end-to-end:
//!
//! 1. Node A (`alice`) mints a friend token (`mint_friend_token`), registers it
//!    as a Case-A pkarr publication (`register_friend_token`), and installs the
//!    friend acceptor (via the `MultiplexHandshakeDispatcher`) on her link
//!    manager.
//! 2. Node B (`bob`) decodes the `harmony://friend/<token>` URL and runs
//!    `connectivity_link_friend_iroh_inner`: resolve A's reachability via pkarr
//!    keyed on `token.sig`, synthesize an `EndpointAddr`, connect on
//!    `harmony/friend/v1`, write the signed `FriendLinkRequest`, read the
//!    `FriendLinkAccepted`.
//! 3. On the accept side, A's `process_friend_request` writes B as an
//!    Active/Token friend and unregisters the consumed Case-A handle.
//! 4. On the dial side, B verifies A's accept (cert + signature) and writes A as
//!    an Active/Token friend.
//!
//! Assertions: B's owner-state has `FriendEntry{A, Active, Token}` keyed on A's
//! owner_id; A's owner-state has `FriendEntry{B, Active, Token}` keyed on B's
//! owner_id; A unregistered the `friend:{token.sig}` handle on consume.
//!
//! Run serially (`-j 1`) to dodge the known UDP-port-4242 contention flake
//! (ZEB-347), and wrapped in a generous `tokio::time::timeout`.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::friend_graph::{FriendOrigin, FriendStatus};
use harmony_app::friend_token::{encode_friend_token_url, mint_friend_token};
use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::iroh_friend_acceptor::{
    IrohFriendHandshakeAcceptor, MultiplexHandshakeDispatcher,
};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::Hlc;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;
use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use tokio::sync::Mutex as TokioMutex;
use zenoh_link::LinkUnicast;

/// Build a hermetic iroh endpoint on loopback with no address-lookup, no pkarr
/// publisher, no DERP relays. Both the zenoh + friend ALPNs are registered so
/// the dialer can hit the friend handshake; the accept side dispatches on
/// negotiated ALPN inside `IrohZenohLinkManager::spawn_accept_loop`.
async fn build_hermetic_endpoint() -> Arc<IrohEndpoint> {
    let secret = SecretKey::generate();
    let inner = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![
            alpn::HARMONY_ZENOH_V1.to_vec(),
            alpn::HARMONY_HANDSHAKE_V1.to_vec(),
            alpn::HARMONY_FRIEND_V1.to_vec(),
        ])
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind_addr loopback")
        .bind()
        .await
        .expect("bind iroh endpoint");
    Arc::new(IrohEndpoint::from_endpoint_for_integration_test(inner))
}

/// Build the 64-byte composite identity_pub for a transport signing key:
/// `X25519_pub(32) ‖ Ed25519_pub(32)`. The Ed25519 half must match the signing
/// key so `PkarrRoutingRecord::sign_new`'s key-match guard passes.
fn identity_pub_for(sk: &SigningKey) -> [u8; 64] {
    let x25519_priv = harmony_app::dm_signing::ed25519_priv_to_x25519(sk);
    let x25519_pub = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
    let ed25519_pub = sk.verifying_key().to_bytes();
    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(x25519_pub.as_bytes());
    combined[32..].copy_from_slice(&ed25519_pub);
    combined
}

/// Wait (≤5s) for Alice's case-A pkarr record — keyed on the friend token's
/// signature for the current epoch — to become visible in the mock relay.
async fn await_pkarr_record_visible(
    pkarr_resolver: &harmony_pkarr::PkarrResolver,
    token_sig: &[u8; 64],
) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64;
    let epoch_id = harmony_pkarr::current_epoch_id(now_ms);
    let probe_signing = harmony_pkarr::derive_ephemeral_key(
        harmony_pkarr::PkarrCase::Invite,
        token_sig,
        &epoch_id.to_be_bytes(),
    );
    let probe_verifying = probe_signing.verifying_key();
    let mut record_visible = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(Some(_)) = pkarr_resolver.resolve(&probe_verifying).await {
            record_visible = true;
            break;
        }
    }
    assert!(
        record_visible,
        "alice's friend-token pkarr record must appear in the mock relay within 5s \
         before driving Bob's redeem"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn friend_token_roundtrip_mutual_active_token_friends() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    // ZEB-347: generous outer timeout so a wedged handshake fails the test fast
    // rather than hanging the whole suite.
    tokio::time::timeout(Duration::from_secs(45), async {
        // ── 1. Identities. ──────────────────────────────────────────────
        // Each party has a master owner identity (mint_test_owner) carrying the
        // owner_id + enrolled device-#2 key + Master EnrollmentCert that the
        // friend handshake authenticates against, plus a transport signing key
        // used only to sign the Case-A pkarr routing record.
        let alice = harmony_app::community_membership::mint_test_owner(0xA1);
        let bob = harmony_app::community_membership::mint_test_owner(0xB2);
        let alice_device2 = Arc::new(SigningKey::from_bytes(&alice.device_key.to_bytes()));
        let bob_device2 = Arc::new(SigningKey::from_bytes(&bob.device_key.to_bytes()));

        // Alice's transport signing key (pkarr record signer). Independent of
        // her owner identity; only the inner-sig + skew of the routing record
        // are verified on the dial side (identity-match is skipped for friend
        // tokens), so any consistent (sk, identity_pub) pair works.
        let alice_transport_sk = SigningKey::from_bytes(&[0x5a; 32]);
        let alice_transport_pub = identity_pub_for(&alice_transport_sk);

        // ── 2. Iroh endpoints + link managers (one per "process"). ──────
        let alice_ep = build_hermetic_endpoint().await;
        let bob_ep = build_hermetic_endpoint().await;
        let alice_bound = alice_ep.bound_sockets();
        assert!(
            !alice_bound.is_empty(),
            "alice's hermetic endpoint must expose bound_sockets() for bob's dialer"
        );

        let alice_reachability = ReachabilityResolver::new();
        let bob_reachability = ReachabilityResolver::new();

        let (alice_link_tx, _alice_link_rx) = flume::unbounded::<LinkUnicast>();
        let alice_link_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            alice_reachability.clone(),
            alice_link_tx,
        ));
        let (bob_link_tx, _bob_link_rx) = flume::unbounded::<LinkUnicast>();
        let bob_link_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&bob_ep),
            bob_reachability.clone(),
            bob_link_tx,
        ));
        let alice_accept = alice_link_mgr.spawn_accept_loop();
        let bob_accept = bob_link_mgr.spawn_accept_loop();

        // ── 3. Alice's owner-state + hlc tracker (acceptor dependencies). ─
        let alice_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let alice_hlc_tracker = Arc::new(TokioMutex::new(BTreeMap::<String, Hlc>::new()));

        // ── 4. Mock pkarr relay + Case-A friend-token publisher. ────────
        let relay = harmony_pkarr::testing::MockPkarrRelay::start().await;
        let pool = harmony_pkarr::RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(harmony_pkarr::RelayClient::new(pool));
        let pkarr_publisher = Arc::new(harmony_pkarr::PkarrPublisher::new(Arc::clone(&client)));
        let publisher_handle = Arc::clone(&pkarr_publisher).spawn();
        let pkarr_resolver = Arc::new(harmony_pkarr::PkarrResolver::new(Arc::clone(&client)));

        // Alice's routing record (her iroh reachability). The friend-token
        // publisher signs this blob under the `friend:{token.sig}` handle.
        let alice_routing = harmony_app::reachability_record::ReachabilityAnnouncePayload {
            iroh_node_id: *alice_ep.node_id().as_bytes(),
            home_relay_url: alice_ep
                .home_relay()
                .map(|r| r.to_string())
                .unwrap_or_default(),
            direct_addresses: alice_bound.clone(),
            announced_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            identity_signature: [0xCDu8; 64],
        };
        let alice_routing_blob = {
            let mut buf = Vec::new();
            ciborium::into_writer(&alice_routing, &mut buf).expect("encode alice routing_blob");
            buf
        };
        let alice_routing_blob_clone = alice_routing_blob.clone();
        let friend_pub: Arc<harmony_app::pkarr_invite_publisher::PkarrInvitePublisher> = Arc::new(
            harmony_app::pkarr_invite_publisher::PkarrInvitePublisher::new(
                Arc::clone(&pkarr_publisher),
                alice_transport_sk.clone(),
                alice_transport_pub,
                Arc::new(move || alice_routing_blob_clone.clone()),
            ),
        );

        // ── 5. Install Alice's friend acceptor (behind the multiplexer). ─
        // Some(friend_pub) enables the ZEB-370 unregister-on-consume path.
        let alice_friend_acceptor: Arc<
            dyn harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher,
        > = Arc::new(IrohFriendHandshakeAcceptor::<()>::new(
            Arc::clone(&alice_crdt_state),
            Arc::clone(&alice_hlc_tracker),
            "alice-dev".to_string(),
            alice.owner,
            Some("alice".to_string()),
            alice.cert.clone(),
            Arc::clone(&alice_device2),
            None,
            Some(Arc::clone(&friend_pub)),
        ));
        // No invite traffic in this test; a stub invite dispatcher satisfies the
        // multiplexer's invite slot (only the friend ALPN is ever dialed here).
        let invite_stub: Arc<dyn harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher> =
            Arc::new(NoopDispatcher);
        let alice_dispatcher: Arc<dyn harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher> =
            Arc::new(MultiplexHandshakeDispatcher::new(
                invite_stub,
                alice_friend_acceptor,
            ));
        if alice_link_mgr
            .install_handshake_dispatcher(alice_dispatcher)
            .await
            .is_err()
        {
            panic!("first install must succeed (OnceCell empty)");
        }

        // ── 6. Alice mints + publishes a friend token. ──────────────────
        let minted_at = Hlc {
            wall_ms: 100_500,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let token_payload = mint_friend_token(
            alice.owner,
            Some("alice".to_string()),
            minted_at,
            None, // expires_at
            alice.cert.clone(),
            &alice_device2,
        )
        .expect("mint friend token");
        let token_sig = token_payload.token.sig;
        let token_url = encode_friend_token_url(&token_payload).expect("encode friend token url");

        friend_pub.register_friend_token(&token_sig).await;
        await_pkarr_record_visible(&pkarr_resolver, &token_sig).await;

        // ── 7. Bob redeems. ─────────────────────────────────────────────
        let bob_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let bob_hlc_tracker = Arc::new(TokioMutex::new(BTreeMap::<String, Hlc>::new()));

        let outcome = harmony_app::connectivity_link_friend_iroh_inner(
            token_url,
            Some(Arc::clone(&pkarr_resolver)),
            Some(Arc::clone(&bob_ep)),
            bob.owner,
            bob.cert.clone(),
            Arc::clone(&bob_device2),
            Some("bob".to_string()),
            Arc::clone(&bob_crdt_state),
            Arc::clone(&bob_hlc_tracker),
            "bob-dev".to_string(),
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
        )
        .await
        .expect("connectivity_link_friend_iroh_inner must succeed on the happy path");

        // The outcome surfaces the new friend (Alice) keyed on her owner_id.
        assert_eq!(
            outcome.friend_addr, alice.owner,
            "redeem outcome must report Alice's owner_id as the new friend"
        );
        assert_eq!(
            outcome.display.as_deref(),
            Some("alice"),
            "redeem outcome display must echo Alice's advertised display name"
        );

        // ── 8. Load-bearing assertions. ─────────────────────────────────
        // (a) Bob's owner-state has FriendEntry{Alice, Active, Token}.
        {
            let g = bob_crdt_state.lock().await;
            let entry = g
                .friend_graph
                .friends
                .get(&alice.owner)
                .expect("Bob must have Alice as a friend keyed on her owner_id");
            assert_eq!(entry.status, FriendStatus::Active, "Alice must be Active");
            assert_eq!(
                entry.established_via,
                FriendOrigin::Token,
                "Alice must be established_via Token"
            );
            assert!(!entry.referrable, "friend must default referrable=false");
            assert_eq!(entry.display.as_deref(), Some("alice"));
        }

        // (b) Alice's owner-state has FriendEntry{Bob, Active, Token}, written by
        //     the acceptor's process_friend_request.
        {
            let g = alice_crdt_state.lock().await;
            let entry = g
                .friend_graph
                .friends
                .get(&bob.owner)
                .expect("Alice must have Bob as a friend keyed on his owner_id");
            assert_eq!(entry.status, FriendStatus::Active, "Bob must be Active");
            assert_eq!(
                entry.established_via,
                FriendOrigin::Token,
                "Bob must be established_via Token"
            );
            assert!(!entry.referrable);
            assert_eq!(
                entry.display.as_deref(),
                Some("bob"),
                "Bob's advertised display propagates into Alice's FriendEntry"
            );
        }

        // (c) Alice unregistered the consumed Case-A friend handle.
        let friend_handle = format!("friend:{}", hex::encode(token_sig));
        let mut handle_gone = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !pkarr_publisher
                .active_handles()
                .await
                .contains(&friend_handle)
            {
                handle_gone = true;
                break;
            }
        }
        assert!(
            handle_gone,
            "ZEB-370: Alice's Case-A friend publication must be unregistered \
             (handle {friend_handle:?} dropped from active_handles) within 5s after \
             the friend token is consumed"
        );

        // ── 9. Teardown. ────────────────────────────────────────────────
        publisher_handle.abort();
        alice_accept.abort();
        bob_accept.abort();
        alice_ep.shutdown().await;
        bob_ep.shutdown().await;
    })
    .await
    .expect("friend_token_roundtrip_mutual_active_token_friends timed out at 45s");
}

/// No-op dispatcher for the multiplexer's invite slot — this test only ever
/// dials the friend ALPN, so the invite branch is never exercised.
struct NoopDispatcher;

#[async_trait::async_trait]
impl harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher for NoopDispatcher {
    async fn handle_connection(&self, _conn: iroh::endpoint::Connection) {}
}
