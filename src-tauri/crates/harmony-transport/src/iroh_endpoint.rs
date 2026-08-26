//! ZEB-321 Phase 1 Task 4: `IrohEndpoint` wrapper + ALPN registry +
//! persistent Ed25519 secret key (OS keychain).
//!
//! ZEB-739 (iroh-tier extraction): the `IrohEndpoint` type itself — the
//! `iroh::Endpoint` wrapper with relay/rebind lifecycle — now lives in the
//! reusable [`harmony_iroh`] crate and is **re-exported** here so every existing
//! `crate::iroh_endpoint::IrohEndpoint` path (and the zenoh-over-iroh bridge that
//! depends on that exact type identity) keeps compiling unchanged. What stays
//! client-side is the app-specific surface the core crate deliberately does NOT
//! carry: the harmony ALPN wire strings ([`alpn`] + [`all_client_alpns`]), the
//! persistent-key provisioning ([`load_or_create_secret_key`], keychain vault),
//! the client [`IrohEndpointError`] (which also folds keychain/vault failures),
//! and the production/hermetic constructors that bake in the client ALPN set.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §7.1
//! (transport layer) and §7.4 (zenoh-over-iroh).
//!
//! ## Design intent
//!
//! - Long-lived [`iroh::Endpoint`] bound at app startup, used by the
//!   zenoh-over-iroh transport plus the harmony handshake protocol.
//! - The endpoint's identity (Ed25519 `SecretKey`) MUST persist across
//!   app restarts so peers can dial us by a stable `EndpointId` — we
//!   store it in the OS keychain at `service="harmony.client"`,
//!   `user="iroh.secret_key"`.
//! - The harmony ALPNs (see [`alpn`]) are registered up-front on the endpoint;
//!   [`all_client_alpns`] is the single source the production + hermetic
//!   constructors advertise.

use async_trait::async_trait;
pub use harmony_iroh::endpoint::IrohEndpoint;
use iroh::endpoint::Connection;
use iroh::{RelayUrl, SecretKey};

/// Pluggable dispatcher invoked by `IrohZenohLinkManager`'s accept
/// loop when an inbound connection negotiates an ALPN other than
/// `harmony/zenoh/v1`. The link manager passes the accepted
/// `Connection` directly — implementations are responsible for opening
/// any bi-streams and consuming the connection.
///
/// ZEB-548 Stage 2: this contract lives in `iroh_endpoint` (spine
/// transport core) so the accept loop dispatches inbound connections
/// through a trait object and stays decoupled from the higher-tier
/// acceptor modules that implement it (invite/friend/pex, community
/// relay, vine relay, tunnel). Those modules `impl` this trait and
/// register via the link manager's `install_*_acceptor` methods; the
/// spine never names their concrete types. Re-exported from
/// `iroh_invite_acceptor` for byte-stable call sites.
#[async_trait]
pub trait IrohHandshakeDispatcher: Send + Sync + 'static {
    /// Called once per inbound connection that survives the ALPN
    /// filter. Implementations may run synchronously or spawn a task;
    /// the accept loop awaits this call. Errors are not propagated —
    /// implementations should log and return.
    async fn handle_connection(&self, conn: Connection);
}

/// ALPN registry for harmony-on-iroh sub-protocols. Constants are
/// referenced by both the endpoint binder (server-side `accept`) and
/// by outbound `connect` callers — keep them in one place so a typo
/// can't silently split the namespace.
pub mod alpn {
    pub const HARMONY_ZENOH_V1: &[u8] = b"harmony/zenoh/v1";
    pub const HARMONY_HANDSHAKE_V1: &[u8] = b"harmony/handshake/v1";
    /// ZEB-329: self-test only — peer ping with 1-byte echo. Produces
    /// no app-level state; safe to ignore for all non-self-test code.
    pub const HARMONY_PING_V1: &[u8] = b"harmony/ping/v1";
    /// ZEB-370: friend-link control protocol (`FriendLinkRequest` /
    /// `FriendLinkAccepted`). Dispatched by the accept loop the same way
    /// `HARMONY_HANDSHAKE_V1` is (see `iroh_friend_acceptor`).
    pub const HARMONY_FRIEND_V1: &[u8] = b"harmony/friend/v1";
    /// ZEB-375 (Friends Phase 2a): friend-PEX referral-catalog protocol
    /// (`CatalogRequest` / signed `ReferralCatalog`). Dispatched by the accept
    /// loop the same way `HARMONY_FRIEND_V1` is (see `iroh_friend_acceptor`'s
    /// `MultiplexHandshakeDispatcher`, which routes it to the PEX acceptor).
    pub const HARMONY_FRIEND_PEX_V1: &[u8] = b"harmony/friend-pex/v1";
    /// ZEB-418 (SP2 P1): butler-deposit protocol (`DepositFrame` /
    /// `DepositAck`, see `butler_deposit` + `iroh_butler_acceptor`). Routed
    /// by the accept loop to the late-installed `IrohButlerDepositAcceptor`
    /// (see `IrohZenohLinkManager::install_butler_deposit_acceptor`);
    /// connections arriving before install are closed — the sender's
    /// fallback chain treats that as a rung-2 failure and retries.
    pub const HARMONY_BUTLER_DEPOSIT_V1: &[u8] = b"harmony/butler-deposit/v1";
    /// ZEB-458 (SP2 P4): community sealed-relay deposit protocol
    /// (`RelayDepositFrame` / `RelayDepositAck`, see `community_relay` +
    /// `iroh_community_relay_acceptor`). Routed by the accept loop to the
    /// late-installed `IrohCommunityRelayDepositAcceptor` (see
    /// `IrohZenohLinkManager::install_community_relay_deposit_acceptor`);
    /// connections arriving before install are closed (sender retries — a
    /// failed relay rung never makes delivery worse). ZEB-548 Stage 2: the
    /// wire ALPN literal lives here (the transport core owns the accept-loop
    /// dispatch key); `community_relay` no longer defines it.
    pub const HARMONY_COMMUNITY_RELAY_DEPOSIT_V1: &[u8] = b"harmony/community-relay-deposit/v1";
    /// ZEB-458 (SP2 P4): community sealed-relay pull protocol (`RelayPullQuery`
    /// → `RelayPullResponse` → optional `RelayPullAckFrame`, see
    /// `community_relay` + `iroh_community_relay_acceptor`). Routed to the
    /// late-installed `IrohCommunityRelayPullAcceptor`.
    pub const HARMONY_COMMUNITY_RELAY_PULL_V1: &[u8] = b"harmony/community-relay-pull/v1";
    /// ZEB-473 (Move 1a): post-quantum DM tunnel protocol — the `harmony-tunnel`
    /// PQ session carrying `FrameTag::Dm` bodies over iroh QUIC. Routed by the
    /// accept loop to the late-installed tunnel acceptor (see
    /// `IrohZenohLinkManager::install_tunnel_acceptor`); connections arriving
    /// before install are closed (the sender's deposit fallback covers it).
    pub const HARMONY_TUNNEL_V1: &[u8] = b"harmony/tunnel/v1";
    /// ZEB-623: tunnel ALPN *generation 2* — a wire-incompatible framing bump
    /// (first frame is now the versioned `protocol_versioning::TunnelHello`
    /// capabilities hello). Registered alongside `/v1` during the N/N-1
    /// deprecation window so a one-generation-behind peer still connects; a
    /// dialer tries the newest generation first and falls back to `/v1` on
    /// connect-failure. Retire `/v1` only after
    /// `protocol_versioning::MIN_SUPPORTED_TUNNEL_ALPN_GENERATION` advances to 2.
    pub const HARMONY_TUNNEL_V2: &[u8] = b"harmony/tunnel/v2";
    /// ZEB-811: public-read vine descriptor + video fan-out protocol (see
    /// `vine_relay`). Deliberately UNAUTHENTICATED — public vine sharing is
    /// the design center — bounded instead by an admission cap, frame caps,
    /// and a per-session byte budget (`vine_relay` module docs). Routed by
    /// the accept loop to the late-installed `VineRelayAcceptor` (see
    /// `IrohZenohLinkManager::install_vine_relay_acceptor`); connections
    /// arriving before install are closed (the follower's pull driver
    /// retries next cadence). ZEB-548 Stage 2: the wire ALPN literal lives
    /// here (the transport core owns the accept-loop dispatch key);
    /// `vine_relay` re-exports it.
    pub const HARMONY_VINE_RELAY_V1: &[u8] = b"harmony/vine-relay/v1";
}

/// The full set of harmony ALPNs the client endpoint advertises, in the same
/// order the production builder historically listed them. Single-sourced here so
/// the production constructor ([`new_with_secret_and_relays`]) and the hermetic
/// test constructor ([`new_with_secret_and_relays_hermetic_dns`]) can't drift.
pub fn all_client_alpns() -> Vec<Vec<u8>> {
    vec![
        alpn::HARMONY_ZENOH_V1.to_vec(),
        alpn::HARMONY_HANDSHAKE_V1.to_vec(),
        alpn::HARMONY_PING_V1.to_vec(),
        alpn::HARMONY_FRIEND_V1.to_vec(),
        alpn::HARMONY_FRIEND_PEX_V1.to_vec(),
        alpn::HARMONY_BUTLER_DEPOSIT_V1.to_vec(),
        alpn::HARMONY_COMMUNITY_RELAY_DEPOSIT_V1.to_vec(),
        alpn::HARMONY_COMMUNITY_RELAY_PULL_V1.to_vec(),
        alpn::HARMONY_TUNNEL_V1.to_vec(),
        alpn::HARMONY_TUNNEL_V2.to_vec(),
        alpn::HARMONY_VINE_RELAY_V1.to_vec(),
    ]
}

/// OS keychain coordinates for the persistent iroh `SecretKey`.
const KEYCHAIN_SERVICE: &str = "harmony.client";
const KEYCHAIN_USER: &str = "iroh.secret_key";

/// Client-side endpoint construction error. Wraps the core
/// [`harmony_iroh::IrohEndpointError`] bind failure (via [`Self::Bind`]) and
/// additionally carries the keychain/vault provisioning failures the core crate
/// deliberately does NOT model (it takes an already-materialized secret key).
#[derive(Debug, thiserror::Error)]
pub enum IrohEndpointError {
    #[error("iroh endpoint bind failed")]
    Bind(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error("keychain {context}")]
    Keychain {
        context: String,
        #[source]
        source: keyring::Error,
    },
    /// ZEB-363: failure folding the iroh key into the single keychain vault
    /// (covers the bad-length case the accessor validates internally).
    #[error("secret vault: {context}")]
    Vault { context: String },
}

/// ZEB-624: build + bind an endpoint using `secret_key` as the persistent
/// identity, registering the harmony ALPNs ([`all_client_alpns`]) and an optional
/// user-configured custom relay list. `None` (or an empty list) follows the
/// `presets::N0` default relay map (n0 stable); `Some(non-empty)` pins exactly
/// those relays.
///
/// ZEB-739: delegates to [`harmony_iroh::endpoint::IrohEndpoint::new_with_secret`],
/// mapping the client's `Option<Vec<RelayUrl>>` onto the core
/// [`RelayConfig`](harmony_iroh::endpoint::RelayConfig) enum and baking in the
/// client ALPN set via [`AlpnConfig`](harmony_iroh::endpoint::AlpnConfig). Was an
/// associated `IrohEndpoint::new_with_secret_and_relays`; now a free fn because
/// the type is a foreign re-export.
pub async fn new_with_secret_and_relays(
    secret_key: SecretKey,
    custom_relays: Option<Vec<RelayUrl>>,
) -> Result<IrohEndpoint, IrohEndpointError> {
    let relays = match custom_relays {
        Some(urls) if !urls.is_empty() => harmony_iroh::endpoint::RelayConfig::Custom(urls),
        _ => harmony_iroh::endpoint::RelayConfig::N0Default,
    };
    harmony_iroh::endpoint::IrohEndpoint::new_with_secret(
        secret_key,
        relays,
        harmony_iroh::endpoint::AlpnConfig::new(all_client_alpns()),
    )
    .await
    .map_err(|e| IrohEndpointError::Bind(Box::new(e)))
}

/// ZEB-626: test seam. Like [`new_with_secret_and_relays`] but with
/// [`hermetic_dns_resolver`] injected, so N0-path unit tests skip iroh's eager
/// system-DNS-config read at bind — a synchronous SystemConfiguration XPC that
/// stalls ~22s/process on macOS for unentitled processes (test binaries).
///
/// ZEB-739: the core crate's hermetic-DNS constructor is `#[cfg(test)]`-only (not
/// reachable cross-crate), so this stays client-side: it builds the raw
/// `iroh::Endpoint` with the client ALPNs + the non-resolving resolver and wraps
/// it via the core [`from_endpoint_for_test`](harmony_iroh::endpoint::IrohEndpoint::from_endpoint_for_test)
/// seam. The wrapped endpoint's *tracked* configured-relay set is empty (the
/// relay-map tracking + `apply_relay_urls` diff logic is exercised by the core
/// crate's own tests now); its callers here only need a bindable hermetic
/// endpoint that never dials for real.
#[cfg(any(test, feature = "test-fixtures"))]
pub async fn new_with_secret_and_relays_hermetic_dns(
    secret_key: SecretKey,
    custom_relays: Option<Vec<RelayUrl>>,
) -> Result<IrohEndpoint, IrohEndpointError> {
    use iroh::endpoint::{presets, Endpoint};
    let mut builder = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .alpns(all_client_alpns())
        .dns_resolver(hermetic_dns_resolver());
    if let Some(urls) = custom_relays {
        if !urls.is_empty() {
            builder = builder.relay_mode(iroh::endpoint::RelayMode::custom(urls));
        }
    }
    let inner = builder
        .bind()
        .await
        .map_err(|e| IrohEndpointError::Bind(Box::new(e)))?;
    Ok(IrohEndpoint::from_endpoint_for_test(inner))
}

/// Load a persisted iroh `SecretKey`, or generate and persist a fresh one on
/// first run.
///
/// Returns `(secret_key, freshly_created)` so callers can know whether a
/// new identity was just minted (true) or an existing entry was loaded
/// (false). The `freshly_created` flag drives the first-run welcome
/// modal in `start_node` (ZEB-331).
///
/// ZEB-449: the key is sourced via
/// [`crate::identity::app_key_or_create_with_fallback`] with
/// [`VaultSlot::Iroh`](crate::identity::VaultSlot::Iroh). It **prefers the OS
/// keychain vault** but falls back to an encrypted file at
/// `~/.harmony/iroh_sk.enc` (resolved via
/// [`resolve_path`](crate::identity::resolve_path) +
/// [`EncryptedFileStore::from_env`](crate::identity::EncryptedFileStore)) when
/// the keychain is unavailable or unusable — so headless / kill-switched nodes
/// still get a transport key instead of booting with transport disabled. The
/// fallback is **lazy**: the path is resolved and the passphrase env parsed only
/// when the keychain fails, so a malformed `HARMONY_PASSPHRASE` or a missing
/// `HOME` never breaks a working keychain. Read/create failures (keychain *or*
/// file) map to [`IrohEndpointError::Vault`]; only the legacy-entry construction
/// above maps to [`IrohEndpointError::Keychain`]. We never silently re-generate:
/// losing the secret key changes our [`EndpointId`](iroh::EndpointId), breaking
/// any peer that knew us by the old id.
///
/// # Freshness testing note
///
/// The freshness behavior is unit-tested only at the serialization
/// boundary (`StartNodeResponse`). The keychain branch — that the
/// `Err(keyring::Error::NoEntry)` arm produces `freshly_created=true` and
/// the `Ok(bytes)` arm produces `freshly_created=false` — is verified by
/// the Task 10 manual smoke checklist (deleting the keychain entry and
/// confirming the welcome modal fires) until a mock-keyring abstraction
/// is introduced.
pub fn load_or_create_secret_key() -> Result<(SecretKey, bool), IrohEndpointError> {
    // ZEB-363: the iroh key is consolidated into the single `harmony`/`identity`
    // keychain vault item rather than its own `harmony.client`/`iroh.secret_key`
    // item. `vault_app_key_or_create` folds in (and deletes) any pre-existing
    // legacy item — preserving the EndpointId — and otherwise generates a fresh
    // key, all within the one item so macOS prompts for keychain access once.
    let legacy = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER).map_err(|e| {
        IrohEndpointError::Keychain {
            context: format!("legacy entry creation {KEYCHAIN_SERVICE}/{KEYCHAIN_USER}"),
            source: e,
        }
    })?;
    load_or_create_secret_key_inner(&legacy)
}

/// ZEB-457: [`load_or_create_secret_key`] with an injected legacy keychain
/// entry, so integration tests can drive the REAL env-resolution wiring
/// (`resolve_path` + `EncryptedFileStore::from_env` + the fallback
/// orchestrator) end-to-end with a `keyring::mock` credential. The
/// fresh-create fallback path best-effort-deletes the legacy entry — a
/// real-keychain write that tests must never reach (ZEB-428), which rules
/// out calling the production wrapper above from a test.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn load_or_create_secret_key_with_legacy(
    legacy: &keyring::Entry,
) -> Result<(SecretKey, bool), IrohEndpointError> {
    load_or_create_secret_key_inner(legacy)
}

fn load_or_create_secret_key_inner(
    legacy: &keyring::Entry,
) -> Result<(SecretKey, bool), IrohEndpointError> {
    // ZEB-449: prefer the keychain vault, but fall back to an encrypted file
    // (`~/.harmony/iroh_sk.enc`, under HARMONY_PASSPHRASE) when no keychain is
    // available or usable — so headless / kill-switched nodes still get a
    // transport key instead of booting with transport disabled. The fallback
    // factory is lazy: path resolution + passphrase parsing happen ONLY when the
    // keychain is unusable, so a keychain-healthy node never hard-fails on a
    // missing HOME or a malformed passphrase.
    let (key_bytes, freshly_created) = crate::identity::app_key_or_create_with_fallback(
        crate::identity::VaultSlot::Iroh,
        legacy,
        || {
            let path = crate::identity::resolve_path(None)?.with_file_name("iroh_sk.enc");
            crate::identity::EncryptedFileStore::from_env(path)
        },
    )
    .map_err(|context| IrohEndpointError::Vault { context })?;
    Ok((SecretKey::from_bytes(&key_bytes), freshly_created))
}

/// ZEB-347/ZEB-626: serialize residual first-`bind()` initialization in this
/// process ahead of tests that assert tight timeouts.
///
/// History: the first `iroh::Endpoint::bind()` in a process used to stall
/// ~30-66s on macOS (~76s under heavy local parallelism; a separate ~10s
/// was once observed on ubuntu CI with a different, never-pinned cause).
/// ZEB-626's diagnosis (2026-07-04) showed the macOS stall was never a
/// process-global iroh init OR teardown: netwatch's interface enumeration
/// (the `netdev` crate) queried each wireless interface's transmit rate via
/// CoreWLAN (sync XPC into `wifid`, ~44s), and iroh's eagerly-built system
/// DNS resolver read macOS DNS config via `SCDynamicStoreCreateWithOptions`
/// (sync XPC into `configd`, ~22s) — both stalled for unentitled processes
/// (every test binary). Both are gone from the test suite: the vendored
/// netdev patch (vendor/netdev) removes the CoreWLAN query, and every
/// endpoint-binding test path injects [`hermetic_dns_resolver`] to skip
/// the system-conf read. Post-fix, a single-endpoint bind+close test
/// measures ~0.06s (was 66.0s).
///
/// `cargo nextest` runs each test in its own process, so any residual
/// per-process first-bind cost (crypto-provider init, netmon route-socket
/// setup) is paid per test. Call this ONCE at the top of an endpoint-binding
/// test, OUTSIDE any `tokio::time::timeout` that guards the behavior under
/// test — that timeout exists to catch a *hung behavior*, not setup. If
/// first-bind cost ever regresses, sample the process mid-stall (ZEB-626
/// diagnosis method; see docs/specs/2026-07-04-zeb-626-netdev-corewlan-stall-design.md
/// §3) before widening any timeout.
///
/// A generous 120s kill-switch still bounds the warm-up itself: a future
/// regression that makes the bind truly *hang* fails the test in ~2 min
/// instead of stalling until the job timeout.
///
/// Marked `pub` + feature-gated so integration tests (which see only the
/// `--features test-fixtures` public surface) can call it too;
/// `#[allow(dead_code)]` because the non-test lib target never calls it.
#[cfg(any(test, feature = "test-fixtures"))]
#[allow(dead_code)]
pub async fn warm_up_iroh_global_init() {
    use iroh::endpoint::{presets, Endpoint};
    let builder = Endpoint::builder(presets::Minimal)
        .relay_mode(iroh::endpoint::RelayMode::Disabled)
        .dns_resolver(hermetic_dns_resolver())
        .clear_ip_transports()
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("warm-up bind_addr loopback");
    let ep = tokio::time::timeout(std::time::Duration::from_secs(120), builder.bind())
        .await
        .expect(
            "warm-up iroh bind exceeded its 120s kill-switch — post-ZEB-626 a \
             hermetic bind is ~0.06s; a stall this long means an iroh bind \
             regression (sample the process mid-stall before widening timeouts)",
        )
        .expect("warm-up iroh bind");
    ep.close().await;
}

/// ZEB-626: a DNS resolver for hermetic test endpoints that never reads the
/// system DNS configuration. iroh's `Builder::bind` eagerly constructs the
/// system resolver when none is supplied, and hickory's macOS
/// `read_system_conf` blocks in `SCDynamicStoreCreateWithOptions` — a
/// synchronous SystemConfiguration XPC that stalls ~22s/process for
/// unentitled callers (every test binary). Hermetic tests dial loopback by
/// address with relays disabled and never resolve a name, so the nameserver
/// below (loopback port 1) is intentionally unanswering.
///
/// Symptom if a future hermetic test DOES resolve a name: a fast
/// connection-refused / timed-out DNS error mentioning `127.0.0.1:1`. If you
/// hit that, the test is no longer hermetic — either dial by address or give
/// that one test a real resolver; do NOT point this helper at a live
/// nameserver (it would re-couple every hermetic test to the host network).
#[cfg(any(test, feature = "test-fixtures"))]
pub fn hermetic_dns_resolver() -> iroh::dns::DnsResolver {
    iroh::dns::DnsResolver::with_nameserver(std::net::SocketAddr::from(([127, 0, 0, 1], 1)))
}

/// Tunable timeouts for the dialer side of the ZEB-325 Phase 2c invite
/// handshake. Each `tokio::time::timeout(..)` wrapping a
/// `connect` / `open_bi` / response-read call uses one of these
/// durations.
///
/// Production wiring (the `connectivity_redeem_invite_iroh` IPC) calls
/// [`Self::from_env`] so operators can override without recompiling;
/// integration tests construct directly to keep wall-clock short and
/// to avoid mutating process env (`std::env::set_var` is unsafe in
/// multithreaded contexts — see ZEB-325 PR #159 F10).
#[derive(Debug, Clone, Copy)]
pub struct HandshakeDialConfig {
    /// Timeout for the QUIC `connect()` call (initial dial + hole-
    /// punch). Distinct from the response-read timeout so diagnostics
    /// can tell "couldn't reach the inviter at all" apart from
    /// "reached them but they never responded".
    pub connect_timeout: std::time::Duration,
    /// Timeout for `Connection::open_bi()` after `connect()` succeeds.
    /// Usually returns near-immediately on a healthy connection;
    /// bounded for the pathological case where the peer never opens
    /// its receive window.
    pub open_bi_timeout: std::time::Duration,
    /// Timeout for reading the length-prefixed handshake response
    /// (acceptor's CBOR-encoded JoinCountersign). Replaces the
    /// previous direct `std::env::var` read at the call site.
    pub response_read_timeout: std::time::Duration,
    /// Timeout for the dialer's request writes (length-prefix +
    /// packet body) and `send.finish()`. ZEB-325 PR #159 R3-2
    /// (Cursor MEDIUM): previously unbounded — the dial / open_bi /
    /// response-read awaits were all wrapped, but the request send
    /// path wasn't, so a misbehaving acceptor's flow-control freeze
    /// could pin the dialer indefinitely. Production override:
    /// `HARMONY_INVITE_HANDSHAKE_WRITE_TIMEOUT_MS` (defaults to
    /// 30_000ms; clamped to >= 1ms).
    pub write_timeout: std::time::Duration,
}

impl Default for HandshakeDialConfig {
    fn default() -> Self {
        Self {
            connect_timeout: std::time::Duration::from_millis(30_000),
            open_bi_timeout: std::time::Duration::from_millis(30_000),
            response_read_timeout: std::time::Duration::from_millis(30_000),
            write_timeout: std::time::Duration::from_millis(30_000),
        }
    }
}

impl HandshakeDialConfig {
    /// Production constructor: reads `HARMONY_INVITE_HANDSHAKE_TIMEOUT_MS`
    /// (the historical single-knob env var) and applies it uniformly to
    /// connect, open_bi, and response read. Unset / unparseable → 30s.
    /// The write-side timeout uses a dedicated
    /// `HARMONY_INVITE_HANDSHAKE_WRITE_TIMEOUT_MS` knob (added in
    /// ZEB-325 PR #159 R3-2) so operators can tune request-send
    /// resilience independently from the read budget.
    ///
    /// ZEB-325 PR #159 R3: clamp to >= 1ms. A zero from env override
    /// would otherwise produce instant `tokio::time::timeout(0, …)`
    /// failures, surfacing `inviter_unreachable` on every redeem +
    /// (if the caller retries) a tight loop.
    pub fn from_env() -> Self {
        let ms: u64 = std::env::var("HARMONY_INVITE_HANDSHAKE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30_000)
            .max(1);
        let d = std::time::Duration::from_millis(ms);
        let write_ms: u64 = std::env::var("HARMONY_INVITE_HANDSHAKE_WRITE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30_000)
            .max(1);
        let write_d = std::time::Duration::from_millis(write_ms);
        Self {
            connect_timeout: d,
            open_bi_timeout: d,
            response_read_timeout: d,
            write_timeout: write_d,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::endpoint::{presets, Endpoint, RelayMode};
    use iroh::SecretKey;

    /// Lifecycle smoke test against an ephemeral secret with relays
    /// disabled — keeps the test hermetic. Production callers
    /// ([`new_with_secret_and_relays`]) use the `presets::N0` stable relay
    /// defaults.
    #[tokio::test]
    async fn iroh_endpoint_inits_with_ephemeral_secret() {
        let secret = SecretKey::generate();
        let expected_id = secret.public();

        // Custom build path that disables relay discovery so the test doesn't
        // depend on outbound DERP reachability, then wrap via the core test seam.
        let inner = Endpoint::builder(presets::N0)
            .secret_key(secret)
            .alpns(all_client_alpns())
            .relay_mode(RelayMode::Disabled)
            .dns_resolver(hermetic_dns_resolver())
            .bind()
            .await
            .expect("bind ephemeral endpoint");
        let ep = IrohEndpoint::from_endpoint_for_test(inner);

        // Identity round-trips through the secret key we generated.
        assert_eq!(ep.node_id(), expected_id);

        // Snapshots must not panic. With relays disabled `home_relay`
        // is expected to be `None`; direct addresses may or may not
        // be populated yet (we accept either).
        let _home: Option<RelayUrl> = ep.home_relay();
        let _direct: Vec<std::net::SocketAddr> = ep.direct_addresses();

        // Graceful shutdown.
        ep.shutdown().await;
    }

    /// ZEB-617 regression guard, retargeted by ZEB-619: the relay map the
    /// production builder actually gets must be the stable production
    /// cluster. `presets::N0::apply` sets
    /// `builder.relay_mode(default_relay_mode())` (iroh 1.0.1
    /// endpoint/presets.rs:136), so asserting on that same public function
    /// couples this test to the exact production path. 0.98's preset
    /// silently put the fleet on n0's CANARY relays (no SLA, decommissioned
    /// 2026-09-30); if a future iroh bump regresses the default, this must
    /// fail loudly.
    #[test]
    fn default_relay_map_is_stable_non_canary() {
        let map = iroh::endpoint::default_relay_mode().relay_map();
        let urls: Vec<String> = map.urls::<Vec<_>>().iter().map(|u| u.to_string()).collect();
        assert!(!urls.is_empty(), "default relay map must not be empty");
        for url in &urls {
            assert!(
                !url.contains("canary"),
                "canary relay leaked into defaults: {url}"
            );
            // No trailing dot in the needle: RelayUrl's Display currently
            // keeps the FQDN root dot, but the guard must not depend on
            // that canonicalization detail.
            assert!(
                url.contains(".relay.n0.iroh.link"),
                "unexpected relay host: {url}"
            );
        }
    }

    #[test]
    fn alpn_constants_are_correct() {
        assert_eq!(alpn::HARMONY_ZENOH_V1, b"harmony/zenoh/v1");
        assert_eq!(alpn::HARMONY_HANDSHAKE_V1, b"harmony/handshake/v1");
        assert_eq!(alpn::HARMONY_PING_V1, b"harmony/ping/v1");
        assert_eq!(alpn::HARMONY_FRIEND_V1, b"harmony/friend/v1");
        assert_eq!(alpn::HARMONY_FRIEND_PEX_V1, b"harmony/friend-pex/v1");
        assert_eq!(
            alpn::HARMONY_BUTLER_DEPOSIT_V1,
            b"harmony/butler-deposit/v1"
        );
        assert_eq!(alpn::HARMONY_TUNNEL_V1, b"harmony/tunnel/v1");
        assert_eq!(alpn::HARMONY_TUNNEL_V2, b"harmony/tunnel/v2");
    }

    /// ZEB-626 patch-presence tripwire, part 1 (deterministic, all
    /// platforms): referencing the vendored crate's marker const in a
    /// `const` block means an UNPATCHED netdev (which lacks the const)
    /// fails to COMPILE this test target — no reliance on host hardware.
    /// (Qodo round-1 finding: the behavioral test below passes vacuously
    /// on a Mac with no WiFi interface.)
    const _: () = assert!(
        netdev::ZEBLITHIC_ZEB_626_PATCH,
        "unpatched netdev in the graph (ZEB-626) — refresh vendor/netdev per its README"
    );

    /// ZEB-626 patch-presence tripwire, part 2 (behavioral, macOS). The
    /// vendored netdev (vendor/netdev/README.zeblithic.md) must never
    /// compute transmit_speed for WIRELESS interfaces on macOS: the
    /// upstream implementation fills it via a synchronous CoreWLAN->wifid
    /// XPC call (~44s/process for unentitled callers), paid inside the
    /// first Endpoint::bind() of every process via netwatch. Wired
    /// interfaces legitimately get a link speed from the shared unix
    /// SIOCGIFXMEDIA path (vendor/netdev/src/os/unix/link_speed.rs) — the
    /// patch leaves that untouched, so the assertion is scoped to
    /// Wireless80211. If this fails, an unpatched netdev re-entered the
    /// graph — refresh vendor/netdev per its README. (Vacuous on a Mac
    /// with no WiFi interface; the const assertion above is the
    /// deterministic net.)
    #[test]
    #[cfg(target_os = "macos")]
    fn vendored_netdev_never_computes_transmit_speed_on_macos() {
        use netdev::prelude::InterfaceType;
        for iface in netdev::interface::get_interfaces() {
            if iface.if_type != InterfaceType::Wireless80211 {
                continue;
            }
            assert!(
                iface.transmit_speed.is_none(),
                "wireless interface {} has transmit_speed {:?} — unpatched netdev \
                 (CoreWLAN query) back in the graph (ZEB-626)",
                iface.name,
                iface.transmit_speed
            );
        }
    }
}
