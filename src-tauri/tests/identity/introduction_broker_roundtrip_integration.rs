//! ZEB-376 (Friends Phase 2b, Task 15): three-node (You–F–X) end-to-end
//! integration test for the Path-C active-introduction flow over the real stack.
//!
//! This is the integration gate for the whole 2b feature: reachability-in-
//! envelope (You's own loopback reachability rides inside the `IntroduceRequest`
//! and the relayed `Introduction`), the CANONICAL introduction-reachability HLC
//! (`friend_intro::introduction_reachability_hlc()` — signed by You, re-derived
//! and verified by X), the broker relay (F's `IrohFriendPexAcceptor`
//! `IntroduceRequest` arm resolves X via Case-D and delivers a signed
//! `Introduction`), X's `PeerIntroPolicy` enforcement, X's self-dial back to You
//! (`complete_introduction`, run INTERNALLY by X's real acceptor on `Proceed`),
//! You's introduction pre-authorization (`PendingOutboundIntroductions` →
//! `AcceptInlineIntroduced`), and owner-state durability (the introduced friend
//! is written into each side's owner-state CRDT).
//!
//! ## Nodes (three hermetic in-process iroh endpoints on loopback)
//!
//! * **You** — the subject. Runs a REAL friend acceptor (`harmony/friend/v1`)
//!   that accepts X's inbound introduction-driven link, with a
//!   `PendingOutboundIntroductions` store pre-authorizing X. You dials F with a
//!   `PexFrame::IntroduceRequest` carrying You's OWN loopback reachability,
//!   device-#2-signed over the canonical introduction HLC.
//! * **F** — the broker. Runs a REAL `IrohFriendPexAcceptor` in the PEX slot,
//!   wired with the Task-9 broker deps (`pkarr_resolver` + `owner_keytree` +
//!   `iroh_endpoint`). On the `IntroduceRequest` it validates X is
//!   Active+referrable, builds+signs an `Introduction`, resolves X's loopback
//!   reachability via Case-D, and delivers the `Introduction` to X.
//! * **X** — the target. Runs a REAL `IrohFriendPexAcceptor` in the PEX slot,
//!   wired with the Task-10/11 deps (`connectivity_settings_path`,
//!   `iroh_endpoint`, `owner_keytree`, `pending_requests`). On the `Introduction`
//!   it verifies F's vouch + the relayed reachability, enforces `PeerIntroPolicy`,
//!   and on `Proceed` dials You back to form the mutual link.
//!
//! ## Which legs run over the real code path
//!
//! * **You→F** (IntroduceRequest): REAL. You synthesizes F's dialable
//!   `EndpointAddr` from F's public `node_id()` + `bound_sockets()` (both
//!   endpoints in-process on loopback, the same no-pkarr synthesis the 2a browse
//!   test uses) and drives the exact production framing into F's real acceptor.
//! * **F→X** (Case-D resolve + Introduction delivery): REAL. The one
//!   infrastructure seam — F's `deliver_introduction_to_target` resolves X via
//!   `resolve_friend_case_d(pkarr_resolver, secret, X)`, which needs a pkarr
//!   record. We stand up a shared in-memory `MockPkarrRelay`: X publishes its OWN
//!   loopback reachability under the F↔X friendship secret via a real
//!   `PkarrFriendPublisher`, and F's real `PkarrResolver` (same relay) resolves
//!   it. This is the real Case-D publish/resolve code path with an in-memory
//!   relay standing in for the DHT — NOT a hand-injected address.
//! * **X→You** (self-dial link on `Proceed`): REAL. X's acceptor spawns
//!   `complete_introduction`, which dials You at the envelope reachability (You's
//!   loopback direct addresses) on `harmony/friend/v1`; You's real friend
//!   acceptor auto-accepts via the pre-auth, and BOTH sides write an
//!   `established_via: Introduction` friend with a sealed rendezvous secret.
//!
//! ## AskMe-accept substitution (documented)
//!
//! The AskMe *stage* runs over the real path (X's acceptor stages an
//! `IntroductionOffer`). The production *accept* (`accept_friend_request_impl` →
//! `complete_introduction`) is `pub(crate)` and therefore unreachable from an
//! external integration-test crate; a re-delivery through the SAME X acceptor is
//! additionally shed by its per-`(voucher, subject)` `IntroRateLimiter` dedupe.
//! So the AskMe accept is driven through the REAL self-dial code by: consuming
//! the staged offer with the PUBLIC `PendingFriendRequests::take_offer` (the
//! exact accept-consume seam the IPC uses), then re-delivering the offer's
//! already-verified `Introduction` to a FRESH X `IrohFriendPexAcceptor` (fresh
//! rate limiter) under an `Open` policy so it runs the SAME
//! `Introduction`→`Proceed`→`complete_introduction` self-dial the auto-Proceed
//! path runs. The X→You link is 100% real code; only the accept *trigger* is
//! substituted (policy-Open re-delivery of the taken offer instead of the
//! pub(crate) `complete_introduction` call).
//!
//! ZEB-374/347: two-endpoint iroh tests flake under contention — run serially
//! (`#[serial_test::serial]` + `--test-threads 1`) with fat per-IO (30s) and
//! outer (90s) timeouts. Never weaken an assertion to mask a timing flake — bump
//! the timeout.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_membership::{mint_test_owner, TestOwner};
use harmony_app::connectivity_settings::ConnectivitySettings;
use harmony_app::friend_graph::{FriendEntry, FriendOrigin, FriendStatus, PeerIntroPolicy};
use harmony_app::friend_intro::{
    encode_pex_frame, sign_introduce_request, sign_introduction, PexFrame,
};
use harmony_app::friend_requests::{
    PendingFriendRequests, PendingKind, PendingOutboundIntroductions,
};
use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::iroh_friend_acceptor::{
    IrohFriendHandshakeAcceptor, MultiplexHandshakeDispatcher,
};
use harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher;
use harmony_app::iroh_pex_acceptor::IrohFriendPexAcceptor;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_crypto::{encrypt_friend_secret, KeyTree};
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_app::pkarr_friend_publisher::{resolve_friend_case_d, PkarrFriendPublisher};
use harmony_app::reachability_record::{
    build_signed_payload_with_key, ReachabilityAnnouncePayload,
};
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;
use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use tokio::sync::Mutex as TokioMutex;
use zenoh_link::LinkUnicast;
use zeroize::Zeroizing;

/// Generous per-IO timeout (ZEB-374: two-endpoint iroh tests flake under
/// contention; deliberately fat so a wedged dial fails loudly rather than masking
/// a wiring bug behind a too-tight deadline).
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Generous outer timeout. Isolated, this test runs in a couple of seconds; 90s
/// is >30x headroom without weakening any assertion.
const OUTER_TIMEOUT: Duration = Duration::from_secs(90);
/// Bound on how long we wait for X's Case-D pkarr record to become resolvable by
/// F through the shared mock relay before driving the flow.
const CASE_D_VISIBLE_TIMEOUT: Duration = Duration::from_secs(20);
/// Bound on how long we poll for the mutual introduction link (or a staged
/// offer) to appear.
const LINK_TIMEOUT: Duration = Duration::from_secs(30);
/// Negative-assertion settle window for the Closed case (a SECURITY property:
/// must-not-link). Equal to the positive `LINK_TIMEOUT` ON PURPOSE: a
/// wrongly-formed link (e.g. a regressed Closed policy that self-dialed) gets the
/// EXACT same budget to appear as the positive cases get to succeed — under known
/// iroh contention a link can take many seconds, so a shorter negative window
/// could false-pass by asserting absence before a slow-but-real link surfaced.
/// The Closed→Reject decision is also pinned by the `decide_introduction_truth_table`
/// unit test, so this settle is the end-to-end belt to that suspenders. (Option
/// (a) of the T15-M2 fix: no clean already-observable positive signal exists for
/// X's Closed-policy processing — a rejection leaves no CRDT/inbox trace and F's
/// F→X delivery is fire-and-forget-spawned, not surfaced — so equalizing the time
/// budget is the sound, deterministic, non-flaky choice over gating on a signal.)
const NO_LINK_SETTLE: Duration = LINK_TIMEOUT;

// ── seeds (avoid the mint_test_owner `N ^ 0xFF` device/master collision) ──────
const YOU_SEED: u8 = 0x10;
const F_SEED: u8 = 0x20;
const X_SEED: u8 = 0x30;
/// The shared F↔X friendship rendezvous secret (both F's sealed graph entry and
/// X's Case-D publisher key it, so F's resolve key matches X's publish key).
const SECRET_FX: [u8; 32] = [0x5f; 32];

/// The canonical introduction-reachability HLC. Spelled as a literal here to
/// match `friend_intro::introduction_reachability_hlc()` (which is `pub(crate)`,
/// unreachable from this external crate): both are
/// `Hlc { wall_ms: 0, logical: 0, device_id: "" }`. Signing You's reachability
/// with any OTHER clock would make X's Task-10 verifier reject the introduction,
/// so this constant is load-bearing.
fn canonical_intro_hlc() -> Hlc {
    Hlc {
        wall_ms: 0,
        logical: 0,
        device_id: String::new(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64
}

fn device2_of(owner: &TestOwner) -> Arc<SigningKey> {
    Arc::new(SigningKey::from_bytes(&owner.device_key.to_bytes()))
}

/// Build a hermetic node iroh endpoint on loopback: no relays, no pkarr, no
/// address lookup. Advertises the four ALPNs the accept loop matches so an
/// inbound friend / friend-PEX dial negotiates and reaches the multiplexer.
/// Mirrors the 2a referral test's `build_server_endpoint`.
async fn build_node_endpoint() -> Arc<IrohEndpoint> {
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
        .expect("bind node iroh endpoint");
    Arc::new(IrohEndpoint::from_endpoint_for_test(inner))
}

/// Build a hermetic RAW client iroh endpoint (the dialer You uses to send its
/// `IntroduceRequest` to F, and the test uses to re-deliver the AskMe offer).
/// `IrohEndpoint::inner()` (the only `connect` seam on the wrapper) is
/// `pub(crate)`, so the dialer drives a raw endpoint directly — same as the 2a
/// referral test's `build_client_endpoint`.
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

/// Synthesize a node's dialable `EndpointAddr` from its public identity + bound
/// loopback sockets (no relay, no pkarr — both endpoints share the process).
fn node_addr(ep: &IrohEndpoint) -> iroh::EndpointAddr {
    let bound = ep.bound_sockets();
    assert!(
        !bound.is_empty(),
        "endpoint must expose bound_sockets() for the dialer"
    );
    let mut addr = iroh::EndpointAddr::new(ep.node_id());
    for sock in bound {
        addr = addr.with_ip_addr(sock);
    }
    addr
}

/// A full valid `FriendEntry` seeded directly (the same direct-insert path the
/// acceptor unit tests use). `master_ed25519` is the friend's REAL master verify
/// key (derived from `mint_test_owner`'s `[seed; 32]`) so the friend-graph key
/// invariant (map key == owner_id derived from this key) holds by construction.
fn friend_entry(
    master_seed: u8,
    status: FriendStatus,
    referrable: bool,
    sealed_secret: Option<Vec<u8>>,
) -> FriendEntry {
    FriendEntry {
        master_ed25519: SigningKey::from_bytes(&[master_seed; 32])
            .verifying_key()
            .to_bytes(),
        display: None,
        status,
        established_via: FriendOrigin::Token,
        referrable,
        learned_at: Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "seed".into(),
        },
        sealed_secret,
    }
}

/// X's own loopback reachability, CBOR-encoded as F's Case-D delivery path
/// expects (`deliver_introduction_to_target` decodes the resolved blob as a
/// `ReachabilityAnnouncePayload`, then synthesizes X's `EndpointAddr` from
/// `iroh_node_id` + `direct_addresses` — it does NOT verify the inner signature,
/// so the stub `identity_signature` is fine).
fn x_case_d_routing_blob(x_ep: &IrohEndpoint) -> Vec<u8> {
    let payload = ReachabilityAnnouncePayload {
        iroh_node_id: *x_ep.node_id().as_bytes(),
        home_relay_url: String::new(),
        direct_addresses: x_ep.bound_sockets(),
        announced_at_ms: now_ms(),
        identity_signature: [0u8; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode X case-d routing blob");
    buf
}

/// Wrap three dispatchers into a multiplexer and install it on a link manager's
/// accept loop (OnceCell — first install wins). Only the relevant slot is a real
/// acceptor; the others are no-op stubs (their ALPNs are never dialed).
async fn install_pex(link_mgr: &Arc<IrohZenohLinkManager>, pex: Arc<dyn IrohHandshakeDispatcher>) {
    let dispatcher: Arc<dyn IrohHandshakeDispatcher> = Arc::new(MultiplexHandshakeDispatcher::new(
        Arc::new(NoopDispatcher),
        Arc::new(NoopDispatcher),
        pex,
    ));
    if link_mgr
        .install_handshake_dispatcher(dispatcher)
        .await
        .is_err()
    {
        panic!("first install must succeed (OnceCell empty)");
    }
}

async fn install_friend(
    link_mgr: &Arc<IrohZenohLinkManager>,
    friend: Arc<dyn IrohHandshakeDispatcher>,
) {
    let dispatcher: Arc<dyn IrohHandshakeDispatcher> = Arc::new(MultiplexHandshakeDispatcher::new(
        Arc::new(NoopDispatcher),
        friend,
        Arc::new(NoopDispatcher),
    ));
    if link_mgr
        .install_handshake_dispatcher(dispatcher)
        .await
        .is_err()
    {
        panic!("first install must succeed (OnceCell empty)");
    }
}

/// Stand up a link manager + accept loop over `ep`, install `pex` in the PEX
/// slot, and return the keep-alive handles.
async fn spawn_pex_node(
    ep: &Arc<IrohEndpoint>,
    pex: Arc<dyn IrohHandshakeDispatcher>,
) -> (
    Arc<IrohZenohLinkManager>,
    tokio::task::JoinHandle<()>,
    flume::Receiver<LinkUnicast>,
) {
    let (link_tx, link_rx) = flume::unbounded::<LinkUnicast>();
    let link_mgr = Arc::new(IrohZenohLinkManager::new(
        Arc::clone(ep),
        ReachabilityResolver::new(),
        link_tx,
    ));
    let accept = link_mgr.spawn_accept_loop();
    install_pex(&link_mgr, pex).await;
    (link_mgr, accept, link_rx)
}

/// Dial `target` on the friend-PEX ALPN, write the framed `PexFrame`, read (and
/// discard) F/X's benign 1-byte ack. Mirrors the production `request_introduction`
/// / `deliver_introduction_to_target` framing (`[u32 LE len][body]` + `finish()`).
async fn send_pex_frame(client: &Endpoint, target: iroh::EndpointAddr, frame: &PexFrame) {
    let body = encode_pex_frame(frame).expect("encode pex frame");
    let conn = tokio::time::timeout(
        IO_TIMEOUT,
        client.connect(target, alpn::HARMONY_FRIEND_PEX_V1),
    )
    .await
    .expect("connect did not time out")
    .expect("connect on friend-pex ALPN");
    let (mut send, mut recv) = tokio::time::timeout(IO_TIMEOUT, conn.open_bi())
        .await
        .expect("open_bi did not time out")
        .expect("open_bi on the friend-pex connection");

    let len = body.len() as u32;
    tokio::time::timeout(IO_TIMEOUT, send.write_all(&len.to_le_bytes()))
        .await
        .expect("write length-prefix did not time out")
        .expect("write length-prefix");
    tokio::time::timeout(IO_TIMEOUT, send.write_all(&body))
        .await
        .expect("write body did not time out")
        .expect("write body");
    send.finish().expect("finish send stream");

    // Read the benign ack ([u32 LE len][body]); content is ignored.
    tokio::time::timeout(IO_TIMEOUT, async {
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf).await.expect("read ack len");
        let n = u32::from_le_bytes(len_buf) as usize;
        assert!(n > 0 && n < 4096, "ack length out of bounds: {n}");
        let mut ack = vec![0u8; n];
        recv.read_exact(&mut ack).await.expect("read ack body");
    })
    .await
    .expect("read ack did not time out");

    conn.close(0u32.into(), b"pex-frame-complete");
}

/// Poll `cond` every 100ms until it returns true or `max` elapses.
async fn wait_until<F, Fut>(max: Duration, mut cond: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + max;
    loop {
        if cond().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Snapshot a friend entry from an owner-state CRDT.
async fn friend_of(crdt: &Arc<TokioMutex<OwnerState>>, who: OwnerAddr) -> Option<FriendEntry> {
    crdt.lock().await.friend_graph.friends.get(&who).cloned()
}

/// True iff `crdt` holds `who` as an Active friend `established_via` Introduction
/// with a sealed rendezvous secret (the fully-formed introduction link).
async fn is_introduction_friend(crdt: &Arc<TokioMutex<OwnerState>>, who: OwnerAddr) -> bool {
    matches!(friend_of(crdt, who).await, Some(e)
        if e.status == FriendStatus::Active
            && e.established_via == FriendOrigin::Introduction
            && e.sealed_secret.is_some())
}

/// The You/F/X harness, wired for a given target policy. Everything is kept alive
/// for the whole test (nextest is process-per-test; leaking on teardown-skip is
/// harmless, but we retain handles so the accept loops / publisher stay live).
struct Harness {
    you: TestOwner,
    f: TestOwner,
    x: TestOwner,
    you_device2: Arc<SigningKey>,
    f_device2: Arc<SigningKey>,
    x_device2: Arc<SigningKey>,

    you_ep: Arc<IrohEndpoint>,
    you_crdt: Arc<TokioMutex<OwnerState>>,
    you_pending_outbound: Arc<PendingOutboundIntroductions>,

    f_addr: iroh::EndpointAddr,
    f_crdt: Arc<TokioMutex<OwnerState>>,
    /// Number of friends F seeded — F must gain NONE (it only relays).
    f_seeded_friend_count: usize,

    /// Kept alive so X's primary endpoint outlives the harness (the accept loop
    /// task holds its own clone, but this is a defensive keep-alive). Not read.
    _x_ep: Arc<IrohEndpoint>,
    x_crdt: Arc<TokioMutex<OwnerState>>,
    x_keytree: Arc<KeyTree>,
    x_pending: Arc<PendingFriendRequests>,

    client: Arc<Endpoint>,

    // keep-alive
    _relay: harmony_pkarr::testing::MockPkarrRelay,
    _pkarr_client: Arc<harmony_pkarr::RelayClient>,
    _x_pub_handle: tokio::task::JoinHandle<()>,
    _accepts: Vec<tokio::task::JoinHandle<()>>,
    _link_rxs: Vec<flume::Receiver<LinkUnicast>>,
    _x_case_d: PkarrFriendPublisher,
}

/// Build the three-node harness for a given X `PeerIntroPolicy`. Writes X's
/// `connectivity-settings.json` (in `settings_dir`) with `policy`, seeds the
/// friend graphs, stands up You/F/X real acceptors, publishes X's Case-D record
/// to a shared mock relay, and blocks until F can resolve it. The four legs are
/// then armed to run over the real code path.
async fn setup(policy: PeerIntroPolicy, settings_dir: &std::path::Path) -> Harness {
    let you = mint_test_owner(YOU_SEED);
    let f = mint_test_owner(F_SEED);
    let x = mint_test_owner(X_SEED);
    let you_device2 = device2_of(&you);
    let f_device2 = device2_of(&f);
    let x_device2 = device2_of(&x);

    let you_keytree = Arc::new(KeyTree::derive(&[YOU_SEED; 32]).expect("you keytree"));
    let f_keytree = Arc::new(KeyTree::derive(&[F_SEED; 32]).expect("f keytree"));
    let x_keytree = Arc::new(KeyTree::derive(&[X_SEED; 32]).expect("x keytree"));

    // ── X's connectivity-settings.json with the target policy. ─────────────
    let settings_path = settings_dir.join("connectivity-settings.json");
    ConnectivitySettings {
        peer_intro_policy: policy,
        ..Default::default()
    }
    .save(&settings_path)
    .expect("write X connectivity-settings.json");

    // ── Endpoints. ─────────────────────────────────────────────────────────
    let you_ep = build_node_endpoint().await;
    let f_ep = build_node_endpoint().await;
    let x_ep = build_node_endpoint().await;
    let client = Arc::new(build_client_endpoint().await);

    // ── Seeded friend graphs (direct inserts). ─────────────────────────────
    // You↔F Active both ways; F↔X Active with X referrable in F's graph and a
    // sealed F↔X secret (the broker needs it to Case-D resolve X).
    let f_sealed_x = encrypt_friend_secret(&f_keytree, &x.owner.0, &SECRET_FX).expect("seal F→X");
    let x_sealed_f = encrypt_friend_secret(&x_keytree, &f.owner.0, &SECRET_FX).expect("seal X→F");

    let you_crdt = Arc::new(TokioMutex::new({
        let mut s = OwnerState::default();
        s.friend_graph.friends.insert(
            f.owner,
            friend_entry(F_SEED, FriendStatus::Active, false, None),
        );
        s
    }));
    let mut f_state = OwnerState::default();
    f_state.friend_graph.friends.insert(
        you.owner,
        friend_entry(YOU_SEED, FriendStatus::Active, false, None),
    );
    f_state.friend_graph.friends.insert(
        x.owner,
        friend_entry(X_SEED, FriendStatus::Active, true, Some(f_sealed_x)),
    );
    let f_seeded_friend_count = f_state.friend_graph.friends.len();
    let f_crdt = Arc::new(TokioMutex::new(f_state));
    let x_crdt = Arc::new(TokioMutex::new({
        let mut s = OwnerState::default();
        s.friend_graph.friends.insert(
            f.owner,
            friend_entry(F_SEED, FriendStatus::Active, false, Some(x_sealed_f)),
        );
        s
    }));

    let you_hlc = Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
        "you-dev".to_string(),
    )));
    let f_hlc = Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
        "f-dev".to_string(),
    )));
    let x_hlc = Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
        "x-dev".to_string(),
    )));

    // ── Shared mock pkarr relay for the F→X Case-D leg. ────────────────────
    let relay = harmony_pkarr::testing::MockPkarrRelay::start().await;
    let pool = harmony_pkarr::RelayPool::new(vec![relay.base_url.clone()]);
    let pkarr_client = Arc::new(harmony_pkarr::RelayClient::new(pool));
    let x_publisher = Arc::new(harmony_pkarr::PkarrPublisher::new(Arc::clone(
        &pkarr_client,
    )));
    let x_pub_handle = Arc::clone(&x_publisher).spawn();
    let f_resolver = Arc::new(harmony_pkarr::PkarrResolver::new(Arc::clone(&pkarr_client)));

    // X publishes its OWN loopback reachability under the F↔X friendship secret,
    // keyed on X's owner (so F's `case_d_resolve_key(secret, epoch, X)` matches
    // X's `case_d_publish_key(secret, epoch, X)`).
    let x_blob = x_case_d_routing_blob(&x_ep);
    let x_case_d = PkarrFriendPublisher::new(
        Arc::clone(&x_publisher),
        x.owner.0,
        Arc::new(move || x_blob.clone()),
    );
    x_case_d
        .register_friend(f.owner.0, Zeroizing::new(SECRET_FX))
        .await;

    // ── You: REAL friend acceptor (accepts X's inbound link), pre-auth store. ─
    let you_pending_outbound = Arc::new(PendingOutboundIntroductions::new());
    let you_friend: Arc<dyn IrohHandshakeDispatcher> = Arc::new(
        IrohFriendHandshakeAcceptor::<()>::new(
            Arc::clone(&you_crdt),
            Arc::clone(&you_hlc),
            "you-dev".to_string(),
            you.owner,
            Some("you".to_string()),
            you.cert.clone(),
            Arc::clone(&you_device2),
            Arc::clone(&you_keytree),
            None,
            None,
        )
        .with_pending_outbound(Some(Arc::clone(&you_pending_outbound))),
    );
    let (you_link_mgr, you_accept, you_rx) = {
        let (link_tx, link_rx) = flume::unbounded::<LinkUnicast>();
        let link_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&you_ep),
            ReachabilityResolver::new(),
            link_tx,
        ));
        let accept = link_mgr.spawn_accept_loop();
        install_friend(&link_mgr, you_friend).await;
        (link_mgr, accept, link_rx)
    };
    let _ = you_link_mgr;

    // ── F: REAL PEX acceptor with the Task-9 broker deps. ──────────────────
    let f_pex: Arc<dyn IrohHandshakeDispatcher> = Arc::new(
        IrohFriendPexAcceptor::new(
            Arc::clone(&f_crdt),
            Arc::clone(&f_hlc),
            "f-dev".to_string(),
            f.owner,
            f.cert.clone(),
            Arc::clone(&f_device2),
        )
        .with_pkarr_resolver(Some(Arc::clone(&f_resolver)))
        .with_owner_keytree(Some(Arc::clone(&f_keytree)))
        .with_iroh_endpoint(Some(Arc::clone(&f_ep))),
    );
    let (f_link_mgr, f_accept, f_rx) = spawn_pex_node(&f_ep, f_pex).await;
    let _ = f_link_mgr;

    // ── X: REAL PEX acceptor with the Task-10/11 deps. ─────────────────────
    let x_pending = Arc::new(PendingFriendRequests::new());
    let x_pex: Arc<dyn IrohHandshakeDispatcher> = Arc::new(
        IrohFriendPexAcceptor::new(
            Arc::clone(&x_crdt),
            Arc::clone(&x_hlc),
            "x-dev".to_string(),
            x.owner,
            x.cert.clone(),
            Arc::clone(&x_device2),
        )
        .with_iroh_endpoint(Some(Arc::clone(&x_ep)))
        .with_owner_keytree(Some(Arc::clone(&x_keytree)))
        .with_connectivity_settings_path(Some(settings_path.clone()))
        .with_pending_requests(Some(Arc::clone(&x_pending))),
    );
    let (x_link_mgr, x_accept, x_rx) = spawn_pex_node(&x_ep, x_pex).await;
    let _ = x_link_mgr;

    // ── Block until X's Case-D record is published + resolvable. ───────────
    // The pkarr resolver NEGATIVE-caches a miss for 60s (harmony-pkarr
    // `NEGATIVE_CACHE_TTL`), so a single resolve BEFORE the async publish lands
    // would poison a shared resolver for the whole poll window. Poll with a FRESH
    // throwaway resolver each attempt (empty cache → every attempt re-hits the
    // relay), leaving F's REAL resolver pristine so its later one-shot delivery
    // resolve is a clean post-publish hit.
    let visible = wait_until(CASE_D_VISIBLE_TIMEOUT, || {
        let pkarr_client = Arc::clone(&pkarr_client);
        let x_owner = x.owner.0;
        async move {
            let probe = Arc::new(harmony_pkarr::PkarrResolver::new(pkarr_client));
            resolve_friend_case_d(&probe, &SECRET_FX, &x_owner)
                .await
                .ok()
                .flatten()
                .is_some()
        }
    })
    .await;
    assert!(
        visible,
        "X's Case-D record must be resolvable through the shared mock relay before \
         driving the flow"
    );
    // Warm F's REAL resolver's POSITIVE cache with the now-published record, so
    // F's spawned `deliver_introduction_to_target` resolve is a guaranteed cache
    // hit (never a transient miss that would negative-cache for 60s and drop the
    // relay). Safe from the negative-cache trap: the record is already confirmed
    // present, and the mock relay is PUT-then-GET consistent.
    let warmed = resolve_friend_case_d(&f_resolver, &SECRET_FX, &x.owner.0)
        .await
        .ok()
        .flatten()
        .is_some();
    assert!(
        warmed,
        "F's resolver must warm-resolve X's Case-D record into its positive cache"
    );

    Harness {
        you,
        f,
        x,
        you_device2,
        f_device2,
        x_device2,
        you_ep,
        you_crdt,
        you_pending_outbound,
        f_addr: node_addr(&f_ep),
        f_crdt,
        f_seeded_friend_count,
        _x_ep: x_ep,
        x_crdt,
        x_keytree,
        x_pending,
        client,
        _relay: relay,
        _pkarr_client: pkarr_client,
        _x_pub_handle: x_pub_handle,
        _accepts: vec![you_accept, f_accept, x_accept],
        _link_rxs: vec![you_rx, f_rx, x_rx],
        _x_case_d: x_case_d,
    }
}

impl Harness {
    /// Build You's `IntroduceRequest` for target X: You's OWN loopback
    /// reachability, device-#2-signed over the CANONICAL introduction HLC (the
    /// exact clock X re-derives + verifies), folded into a signed request aimed
    /// at broker F. Mirrors `request_introduction` steps 4 + 6.
    fn build_introduce_frame(&self) -> PexFrame {
        let reachability = build_signed_payload_with_key(
            *self.you_ep.node_id().as_bytes(),
            String::new(),
            self.you_ep.bound_sockets(),
            now_ms(),
            &self.you.owner,
            &canonical_intro_hlc(),
            Vec::new(),
            0,
            &self.you_device2,
        )
        .expect("sign You's reachability");
        let req = sign_introduce_request(
            &self.you_device2,
            self.you.owner,
            self.f.owner,
            self.x.owner,
            reachability,
            self.you.cert.clone(),
        );
        PexFrame::IntroduceRequest(Box::new(req))
    }

    /// Pre-authorize X (so X's introduction-driven inbound link auto-accepts as
    /// `AcceptInlineIntroduced`) then dial F with the `IntroduceRequest`, exactly
    /// as `request_introduction` does (pre-auth BEFORE the dial).
    async fn request_introduction(&self) {
        self.you_pending_outbound.record(self.x.owner, now_ms());
        let frame = self.build_introduce_frame();
        send_pex_frame(&self.client, self.f_addr.clone(), &frame).await;
    }

    /// Assert F relayed but gained no friendship of its own (F "dropped out": the
    /// You↔X link is peer-to-peer, F's read-only broker never writes a You↔X
    /// edge). F's friend count is unchanged, and F holds neither You-as-X's-friend
    /// nor any new entry.
    async fn assert_f_dropped_out(&self) {
        let f = self.f_crdt.lock().await;
        assert_eq!(
            f.friend_graph.friends.len(),
            self.f_seeded_friend_count,
            "F's read-only broker must not add any friend edge (F dropped out); \
             friends: {:?}",
            f.friend_graph.friends.keys().collect::<Vec<_>>()
        );
    }
}

/// No-op dispatcher for the multiplexer's unused slots.
struct NoopDispatcher;

#[async_trait::async_trait]
impl IrohHandshakeDispatcher for NoopDispatcher {
    async fn handle_connection(&self, _conn: iroh::endpoint::Connection) {}
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();
}

// ══════════════════════════════════════════════════════════════════════════
// Case 1 — Open: You→F→X→You links both sides over the full real path.
// ══════════════════════════════════════════════════════════════════════════
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn introduction_broker_roundtrip_open_policy_links_both_sides() {
    init_tracing();
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(OUTER_TIMEOUT, async {
        let settings_dir = tempfile::tempdir().expect("settings tempdir");
        let h = setup(PeerIntroPolicy::Open, settings_dir.path()).await;

        // You requests the introduction to X, via F.
        h.request_introduction().await;

        // Poll until BOTH sides hold the mutual `established_via: Introduction`
        // friend with a sealed rendezvous secret (Case-D armed). This asserts only
        // the IN-MEMORY CRDT link state: the harness builds X's acceptor WITHOUT
        // `.with_owner_sync_engine(...)`, so `complete_introduction` runs with
        // `sync_engine: None` and does NOT exercise `notify_dirty`. Persistence /
        // replication (the `notify_dirty` + `friend-list-changed` durability path)
        // is scoped OUT of this e2e and covered elsewhere: Task 10 wires the
        // acceptor's `with_owner_sync_engine` (production, lib.rs) and the friend
        // acceptor's `notify_owner_state_dirty` is pinned by
        // `friend_write_arms_owner_state_sync_engine`.
        let you_has_x = wait_until(LINK_TIMEOUT, || {
            let crdt = Arc::clone(&h.you_crdt);
            let x = h.x.owner;
            async move { is_introduction_friend(&crdt, x).await }
        })
        .await;
        let x_has_you = wait_until(LINK_TIMEOUT, || {
            let crdt = Arc::clone(&h.x_crdt);
            let you = h.you.owner;
            async move { is_introduction_friend(&crdt, you).await }
        })
        .await;

        assert!(
            you_has_x,
            "You's owner-state must hold X as an Active Introduction friend with a \
             sealed rendezvous secret"
        );
        assert!(
            x_has_you,
            "X's owner-state must hold You as an Active Introduction friend with a \
             sealed rendezvous secret"
        );

        // Redundant explicit sealed-secret assertions (Case-D armed on both sides).
        let you_entry = friend_of(&h.you_crdt, h.x.owner)
            .await
            .expect("You→X entry");
        let x_entry = friend_of(&h.x_crdt, h.you.owner)
            .await
            .expect("X→You entry");
        assert!(
            you_entry.sealed_secret.is_some(),
            "You must carry a sealed rendezvous secret for X"
        );
        assert!(
            x_entry.sealed_secret.is_some(),
            "X must carry a sealed rendezvous secret for You"
        );

        // F relayed only — it holds no You↔X edge (F dropped out).
        h.assert_f_dropped_out().await;

        // Open never stages an offer (it proceeds directly).
        assert!(
            h.x_pending.list().is_empty(),
            "Open policy proceeds directly and must not stage an introduction offer"
        );
    })
    .await
    .expect("introduction_broker_roundtrip_open_policy timed out");
}

// ══════════════════════════════════════════════════════════════════════════
// Case 2 — AskMe: X stages an offer (no link), then an explicit accept links.
// ══════════════════════════════════════════════════════════════════════════
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn introduction_broker_roundtrip_askme_policy_stages_then_links_on_accept() {
    init_tracing();
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(OUTER_TIMEOUT, async {
        let settings_dir = tempfile::tempdir().expect("settings tempdir");
        let h = setup(PeerIntroPolicy::AskMe, settings_dir.path()).await;

        // You requests the introduction to X, via F.
        h.request_introduction().await;

        // AskMe STAGES an offer (real path): poll X's pending inbox until an
        // IntroductionOffer vouched by F appears.
        let staged = wait_until(LINK_TIMEOUT, || {
            let pending = Arc::clone(&h.x_pending);
            let f = h.f.owner;
            async move {
                pending.list().iter().any(
                    |(_, p)| matches!(&p.kind, PendingKind::IntroductionOffer(o) if o.voucher == f),
                )
            }
        })
        .await;
        assert!(
            staged,
            "AskMe policy must stage an IntroductionOffer vouched by F in X's pending inbox"
        );

        // Staging is mutually exclusive with proceeding, so NO link may exist on
        // either side while the offer is staged (short settle for robustness).
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            friend_of(&h.you_crdt, h.x.owner).await.is_none(),
            "AskMe must not link You→X before the user accepts"
        );
        assert!(
            friend_of(&h.x_crdt, h.you.owner).await.is_none(),
            "AskMe must not link X→You before the user accepts"
        );

        // ── ACCEPT. Consume the staged offer with the PUBLIC accept-consume seam
        //    (`take_offer`), then drive the REAL self-dial (`complete_introduction`)
        //    by re-delivering the offer's already-verified `Introduction` to a
        //    FRESH X acceptor under an Open policy — see the module doc-comment's
        //    "AskMe-accept substitution" note. ──────────────────────────────────
        let offer = h
            .x_pending
            .take_offer(&h.you.owner)
            .expect("staged introduction offer for You must be consumable via take_offer");
        assert_eq!(
            offer.voucher, h.f.owner,
            "the staged offer must be vouched by F"
        );

        // Fresh X acceptor (fresh IntroRateLimiter) under an Open policy, sharing
        // X's owner-state so the resulting link lands in the SAME graph.
        let accept_settings_dir = tempfile::tempdir().expect("accept settings tempdir");
        let accept_settings_path = accept_settings_dir
            .path()
            .join("connectivity-settings.json");
        ConnectivitySettings {
            peer_intro_policy: PeerIntroPolicy::Open,
            ..Default::default()
        }
        .save(&accept_settings_path)
        .expect("write accept-phase Open settings");

        let x2_ep = build_node_endpoint().await;
        let x2_pex: Arc<dyn IrohHandshakeDispatcher> = Arc::new(
            IrohFriendPexAcceptor::new(
                Arc::clone(&h.x_crdt),
                // X's SECOND device mints from its own tracker — one tracker
                // per device, as in production.
                Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
                    "x2-dev".to_string(),
                ))),
                "x2-dev".to_string(),
                h.x.owner,
                h.x.cert.clone(),
                Arc::clone(&h.x_device2),
            )
            .with_iroh_endpoint(Some(Arc::clone(&x2_ep)))
            .with_owner_keytree(Some(Arc::clone(&h.x_keytree)))
            .with_connectivity_settings_path(Some(accept_settings_path)),
        );
        let (_x2_link_mgr, _x2_accept, _x2_rx) = spawn_pex_node(&x2_ep, x2_pex).await;

        // Re-wrap the taken offer as the exact `Introduction` F would relay
        // (voucher=F, subject=You, You's already-verified reachability) and
        // deliver it to the fresh Open acceptor → Proceed → complete_introduction.
        let intro = sign_introduction(
            &h.f_device2,
            h.f.owner,
            h.x.owner,
            h.you.owner,
            h.you.cert.clone(),
            offer.reachability.clone(),
            Hlc {
                wall_ms: now_ms(),
                logical: 0,
                device_id: "f-dev".into(),
            },
            h.f.cert.clone(),
            Vec::new(),
        );
        send_pex_frame(
            &h.client,
            node_addr(&x2_ep),
            &PexFrame::Introduction(Box::new(intro)),
        )
        .await;

        // Poll until the mutual `established_via: Introduction` link forms.
        let you_has_x = wait_until(LINK_TIMEOUT, || {
            let crdt = Arc::clone(&h.you_crdt);
            let x = h.x.owner;
            async move { is_introduction_friend(&crdt, x).await }
        })
        .await;
        let x_has_you = wait_until(LINK_TIMEOUT, || {
            let crdt = Arc::clone(&h.x_crdt);
            let you = h.you.owner;
            async move { is_introduction_friend(&crdt, you).await }
        })
        .await;
        assert!(
            you_has_x,
            "after accept, You must hold X as an Active Introduction friend"
        );
        assert!(
            x_has_you,
            "after accept, X must hold You as an Active Introduction friend"
        );

        h.assert_f_dropped_out().await;
    })
    .await
    .expect("introduction_broker_roundtrip_askme_policy timed out");
}

// ══════════════════════════════════════════════════════════════════════════
// Case 3 — Closed: the introduction is rejected; no link, no offer.
// ══════════════════════════════════════════════════════════════════════════
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn introduction_broker_roundtrip_closed_policy_rejects() {
    init_tracing();
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(OUTER_TIMEOUT, async {
        let settings_dir = tempfile::tempdir().expect("settings tempdir");
        let h = setup(PeerIntroPolicy::Closed, settings_dir.path()).await;

        // You requests the introduction to X, via F.
        h.request_introduction().await;

        // Closed rejects the introduction: over a settle window equal to the
        // positive `LINK_TIMEOUT` budget (so a slow-but-real link would have had
        // the SAME time to appear as the Open case gets to succeed), NO link may
        // ever form on either side and NO offer may be staged.
        let linked_or_staged = wait_until(NO_LINK_SETTLE, || {
            let you_crdt = Arc::clone(&h.you_crdt);
            let x_crdt = Arc::clone(&h.x_crdt);
            let pending = Arc::clone(&h.x_pending);
            let x = h.x.owner;
            let you = h.you.owner;
            async move {
                friend_of(&you_crdt, x).await.is_some()
                    || friend_of(&x_crdt, you).await.is_some()
                    || !pending.list().is_empty()
            }
        })
        .await;
        assert!(
            !linked_or_staged,
            "Closed policy must reject the introduction: no link and no staged offer"
        );

        // Explicit final assertions (the settle window elapsed with no change).
        assert!(
            friend_of(&h.you_crdt, h.x.owner).await.is_none(),
            "Closed: You must not gain X as a friend"
        );
        assert!(
            friend_of(&h.x_crdt, h.you.owner).await.is_none(),
            "Closed: X must not gain You as a friend"
        );
        assert!(
            h.x_pending.list().is_empty(),
            "Closed: X must not stage any introduction offer"
        );
        h.assert_f_dropped_out().await;
    })
    .await
    .expect("introduction_broker_roundtrip_closed_policy timed out");
}
