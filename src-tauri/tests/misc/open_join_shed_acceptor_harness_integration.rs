//! ZEB-864: acceptor-harness regression test for the open-join Tier-1 shed path.
//!
//! ZEB-853 B7 added a pre-auth Tier-1 connection shield to the invite acceptor:
//! `handle_invite_handshake_inbound` calls `open_join_conn_limiter.admit_connection`
//! and, on shed, returns `HandshakeAcceptError::ConnectionShed` BEFORE any stream
//! read / decode / crypto (iroh_invite_acceptor.rs: shed at :~393, sitting after
//! `accept_bi()` :~344 but before the length-prefix read :~398). Two properties are
//! load-bearing and, until this test, correct-by-inspection only:
//!
//!   1. **Pre-decode shed** — the gate runs before the packet is read/decoded, so a
//!      caller holding only the public open-invite link cannot force unbounded
//!      pre-consent ed25519 work by opening connections.
//!   2. **Oracle-safe zero-byte shed** — on a shed the acceptor writes NOTHING,
//!      byte-identical to the benign no-response outcomes (`CountersignTimeout` /
//!      `CommunityNotFound`), so there is no rejection-content oracle.
//!
//! This test drives an actual shed through the real acceptor over a real localhost
//! iroh connection (mirroring the friend acceptor's `t7_drive_handshake` raw-loopback
//! rig), with the test playing the dialer so the exact stream shape is under control.
//!
//! **Case A (shed).** A zero-cap `OpenJoinConnLimiter` forces every connection to
//! shed. The dialer opens a bi-stream, writes a 1-byte stub, and finishes the send
//! half (flushing it so `accept_bi()` returns promptly). The handler must return
//! exactly `ConnectionShed` — NOT a read/timeout error — proving it shed before
//! attempting the length-prefix read; and the dialer must receive zero response
//! data bytes.
//!
//! **Case B (control — the regression teeth).** The IDENTICAL dialer against a
//! permissive limiter: the handler now passes the gate, reaches the length-prefix
//! read, and — the dialer having written only 1 of 4 bytes and finished — hits EOF
//! there, returning `ReadPrefix`. Since the only variable between the cases is the
//! limiter cap, the `ConnectionShed` in Case A is attributable to the gate at its
//! pre-decode position — not incidentally to the stream shape. Without this control,
//! an edit moving the gate below the read could still spuriously pass Case A.
//!
//! **Why direct-drive (not the production dispatcher).** ZEB-864 mandates driving
//! the `pub handle_invite_handshake_inbound` seam directly (mirroring the friend
//! acceptor's `t7_drive_handshake`) so the handler's return VARIANT — the pre-decode
//! `ConnectionShed` vs. a read-stage error — is inspectable; the production
//! `handle_connection` dispatcher only exposes the dialer-observable outcome. That
//! observable outcome (zero response bytes + close) IS still validated here: the
//! dialer reads its recv over the real wire and the server task performs the same
//! bounded `conn.closed()` wait the dispatcher does. Pinning the dispatcher's own
//! `ConnectionShed`→close mapping end-to-end is tracked separately (ZEB-870).

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_membership::{mint_test_owner, EventId};
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::DmOutbox;
use harmony_app::iroh_endpoint::{alpn, warm_up_iroh_global_init};
use harmony_app::iroh_invite_acceptor::{
    HandshakeAcceptError, HandshakeAcceptorConfig, IrohInviteHandshakeAcceptor,
};
use harmony_app::open_join_admit::OpenJoinConnLimiter;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{DeviceIdentityHash, OwnerAddr};
use harmony_identity::PrivateIdentity;
use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::{EndpointAddr, SecretKey};
use tokio::sync::Mutex as TokioMutex;

/// The acceptor never resolves an identity before the shed (or before the
/// length-prefix read in the control), so a null resolver suffices.
struct NullResolver;

#[async_trait::async_trait]
impl IdentityResolver for NullResolver {
    async fn resolve(&self, _addr: &OwnerAddr) -> Option<[u8; 64]> {
        None
    }
}

/// `SigningKey` from a `PrivateIdentity` (bytes 32..64 are the ed25519 secret).
/// Mirrors `signing_key_from` in `community_open_join_cross_wan_integration.rs`.
fn signing_key_from(identity: &PrivateIdentity) -> SigningKey {
    let priv_bytes = identity.to_private_bytes();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&priv_bytes[32..64]);
    SigningKey::from_bytes(&secret)
}

fn dup_identity(src: &PrivateIdentity) -> PrivateIdentity {
    PrivateIdentity::from_private_bytes(&src.to_private_bytes())
        .expect("PrivateIdentity round-trip via to/from_private_bytes")
}

/// Build a hermetic, relay-disabled raw iroh endpoint on loopback registered with
/// the handshake ALPN. Mirrors the friend acceptor's `t7_loopback_endpoint` — a raw
/// `iroh::Endpoint` (not the `IrohEndpoint` wrapper) so the test can call `.accept()`
/// / `.connect()` directly.
async fn loopback_endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::generate())
        .alpns(vec![alpn::HARMONY_HANDSHAKE_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .dns_resolver(harmony_app::iroh_endpoint::hermetic_dns_resolver())
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind_addr loopback")
        .bind()
        .await
        .expect("bind hermetic iroh endpoint")
}

/// Build Alice's invite acceptor with the given connection-shield limiter. The
/// registry / dm_outbox / crdt_state are constructed but never queried before the
/// shed (only `dm_outbox.self_owner` is snapshotted at the very top of the handler),
/// so they are minimal stubs. Returns the acceptor plus the tempdir guard (its path
/// is copied into the registry config and must outlive the acceptor).
fn build_acceptor(
    limiter: OpenJoinConnLimiter,
) -> (Arc<IrohInviteHandshakeAcceptor<()>>, tempfile::TempDir) {
    let alice_identity = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_sk = Arc::new(signing_key_from(&alice_identity));
    let alice_comm = mint_test_owner(0xA1);
    let alice_comm_sk = Arc::new(SigningKey::from_bytes(&alice_comm.device_key.to_bytes()));
    let alice_addr = alice_comm.owner;

    // Content store whose CAS channel is never serviced — nothing sends before the
    // shed, so the dropped receiver is never observed.
    let (cas_op_tx, _cas_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
    let cs: Arc<dyn ContentStore> =
        Arc::new(RuntimeContentStore::new(cas_op_tx, Duration::from_secs(2)));
    let dir = tempfile::tempdir().expect("acceptor tempdir");

    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "alice-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::new(NullResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_comm_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    let dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
        "alice-dev".into(),
        alice_addr,
        DeviceIdentityHash(alice_identity.identity.address_hash),
        Arc::clone(&alice_sk),
        Arc::new(dup_identity(&alice_identity)),
        Arc::clone(&alice_comm_sk),
        alice_comm.cert.clone(),
    )));
    let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));

    let acceptor = Arc::new(
        IrohInviteHandshakeAcceptor::<()>::with_config(
            registry,
            dm_outbox,
            crdt_state,
            None,
            None,
            HandshakeAcceptorConfig {
                // Generous so `accept_bi` never spuriously times out under CI load —
                // the iroh connect handshake can take seconds under
                // `nextest --all-targets` saturation (matches the cross-WAN harness's
                // 10s deadline). Case B does NOT depend on this timeout: the dialer
                // finishes its send, so the handler's length-prefix read hits EOF
                // immediately (`ReadPrefix`), keeping the control deterministic
                // regardless of scheduler load.
                io_deadline: Duration::from_millis(10_000),
                poll_deadline: Duration::from_millis(10_000),
                poll_interval: Duration::from_millis(20),
            },
        )
        .with_open_join_conn_limiter(limiter),
    );
    (acceptor, dir)
}

/// Drive one inbound handshake against `acceptor` over a raw iroh loopback pair,
/// with the test as the dialer. The dialer opens a bi-stream, writes a 1-byte stub,
/// and finishes the send half (flushing the byte so the server's `accept_bi()`
/// returns, and leaving an incomplete-then-EOF length prefix). Returns the handler's
/// authoritative server-side `Result` and the number of response DATA bytes the
/// dialer received (0 for a shed — reset/EOF, no content).
async fn drive(
    acceptor: Arc<IrohInviteHandshakeAcceptor<()>>,
) -> (Result<EventId, HandshakeAcceptError>, usize) {
    let server_ep = loopback_endpoint().await;
    let client_ep = loopback_endpoint().await;

    let mut server_addr = EndpointAddr::new(server_ep.id());
    for sock in server_ep.bound_sockets() {
        server_addr = server_addr.with_ip_addr(sock);
    }

    // Server: accept exactly one connection and run the REAL handler, then hold the
    // connection open (bounded) until the dialer drives the close — mirroring
    // production `handle_connection`, which waits on `conn.closed()`. `server_ep` is
    // owned by this task, so it (and the connection derived from it) stays alive.
    let server_task = tokio::spawn(async move {
        let incoming = server_ep
            .accept()
            .await
            .expect("server: incoming connection");
        let conn = incoming.await.expect("server: accept→connect");
        let result = acceptor.handle_invite_handshake_inbound(&conn).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
        result
    });

    // Dialer: connect on the handshake ALPN, open a bi-stream, write a 1-byte stub
    // (an incomplete length prefix), and FINISH the send half. write+finish flushes
    // the byte so the server's `accept_bi()` returns promptly (no reliance on
    // scheduler timing), and the incomplete-then-EOF stream makes a permissive
    // acceptor's length-prefix `read_exact` return `UnexpectedEof` immediately →
    // `ReadPrefix` (Case B), deterministically and without depending on any timeout.
    let conn = client_ep
        .connect(server_addr, alpn::HARMONY_HANDSHAKE_V1)
        .await
        .expect("client: dial");
    let (mut send, mut recv) = conn.open_bi().await.expect("client: open_bi");
    send.write_all(&[0u8])
        .await
        .expect("client: write 1-byte stub");
    send.finish()
        .expect("client: finish send (flush the stub + FIN)");

    // Read any response, bounded. A shed writes nothing (reset/EOF); a permissive
    // acceptor reads the incomplete prefix, hits EOF, and drops its send half.
    // Either way, count the DATA bytes received — the oracle property is "zero
    // response content".
    let mut buf = [0u8; 64];
    let data_len = match tokio::time::timeout(Duration::from_secs(3), recv.read(&mut buf)).await {
        Ok(Ok(Some(n))) => n,
        Ok(Ok(None)) => 0,    // clean EOF
        Ok(Err(_reset)) => 0, // stream reset — no data
        Err(_elapsed) => 0,   // nothing arrived within the bound
    };

    conn.close(0u32.into(), b"zeb864-done");
    let result = server_task.await.expect("server task join");
    (result, data_len)
}

/// Case A — zero-cap limiter: the handler sheds before decode (`ConnectionShed`, not
/// a read/timeout error) and writes zero response bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_join_shed_returns_connection_shed_pre_decode_zero_bytes() {
    // ZEB-374: pre-pay iroh's first-bind global init OUTSIDE the asserted budget.
    warm_up_iroh_global_init().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let (acceptor, _dir) = build_acceptor(OpenJoinConnLimiter::with_caps(0, 60_000));
        let (result, data_len) = drive(acceptor).await;
        assert!(
            matches!(result, Err(HandshakeAcceptError::ConnectionShed)),
            "zero-cap limiter must shed before decode, got {result:?}"
        );
        assert_eq!(
            data_len, 0,
            "shed must write zero response bytes (no rejection-content oracle)"
        );
    })
    .await
    .expect("test completed within budget");
}

/// Case B (control) — permissive limiter, IDENTICAL dialer: the handler passes the
/// gate and reaches the length-prefix read, which hits EOF on the incomplete,
/// finished stream (`ReadPrefix`). Pins that Case A's shed is caused by the gate at
/// its pre-decode position, not by the stream shape — the only variable is the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_join_permissive_limiter_reaches_length_prefix_read() {
    warm_up_iroh_global_init().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let (acceptor, _dir) = build_acceptor(OpenJoinConnLimiter::new());
        let (result, _data_len) = drive(acceptor).await;
        assert!(
            matches!(result, Err(HandshakeAcceptError::ReadPrefix(_))),
            "permissive limiter must pass the gate and reach the length-prefix read (EOF → ReadPrefix), got {result:?}"
        );
    })
    .await
    .expect("test completed within budget");
}
