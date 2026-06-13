//! ZEB-321 Phase 1 Task 4: `IrohEndpoint` wrapper + ALPN registry +
//! persistent Ed25519 secret key (OS keychain).
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
//! - Two ALPN constants registered up-front:
//!   - [`alpn::HARMONY_ZENOH_V1`] — zenoh wire protocol carrier
//!   - [`alpn::HARMONY_HANDSHAKE_V1`] — harmony device handshake (Task 7+)
//!
//! ## API adaptations from the plan draft
//!
//! The plan's draft was written against an older iroh API surface.
//! We are targeting `iroh = "0.98"`, where:
//!
//! - `iroh::NodeId` is renamed to `iroh::EndpointId` (a type alias for
//!   `iroh::PublicKey`).
//! - `Endpoint::builder` takes a `Preset` argument; we use
//!   `iroh::endpoint::presets::N0` for production (n0's relay + STUN
//!   defaults).
//! - `RelayMode::Disabled` is reached via `.relay_mode(RelayMode::Disabled)`
//!   on the builder — used only by hermetic tests.
//! - The endpoint accessor for the local id is `.id()`, not `.node_id()`.
//! - Snapshot of the current `EndpointAddr` is `.addr()`; we extract
//!   relay urls + direct ip addrs from it for the spec's snapshot
//!   accessors.
//! - Shutdown is `.close()`, not `.shutdown()`.

use std::net::SocketAddr;

use iroh::endpoint::{presets, Endpoint};
use iroh::{EndpointId, RelayUrl, SecretKey};

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
}

/// OS keychain coordinates for the persistent iroh `SecretKey`.
const KEYCHAIN_SERVICE: &str = "harmony.client";
const KEYCHAIN_USER: &str = "iroh.secret_key";

/// Wrapper around [`iroh::Endpoint`] exposing only the surface used by
/// subsequent ZEB-321 Phase 1 tasks. Keeping the surface small lets us
/// swap iroh versions or back the endpoint with a different transport
/// later without churning every call site.
#[derive(Clone, Debug)]
pub struct IrohEndpoint {
    inner: Endpoint,
}

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

impl IrohEndpoint {
    /// Build and bind an endpoint using `secret_key` as the persistent
    /// identity. Registers both harmony ALPNs and uses the default
    /// (n0 production) relay configuration.
    pub async fn new_with_secret(secret_key: SecretKey) -> Result<Self, IrohEndpointError> {
        let inner = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(vec![
                alpn::HARMONY_ZENOH_V1.to_vec(),
                alpn::HARMONY_HANDSHAKE_V1.to_vec(),
                alpn::HARMONY_PING_V1.to_vec(),
                alpn::HARMONY_FRIEND_V1.to_vec(),
                alpn::HARMONY_FRIEND_PEX_V1.to_vec(),
                alpn::HARMONY_BUTLER_DEPOSIT_V1.to_vec(),
            ])
            .bind()
            .await
            .map_err(|e| IrohEndpointError::Bind(Box::new(e)))?;
        Ok(Self { inner })
    }

    /// This endpoint's stable identity, derived from the persistent
    /// secret key. In iroh 0.98 this is `EndpointId`, a type alias
    /// for `iroh::PublicKey` (was `NodeId` in earlier versions).
    pub fn node_id(&self) -> EndpointId {
        self.inner.id()
    }

    /// Snapshot of the current home relay url, if any has been
    /// negotiated. Returns `None` before the relay round-trip completes
    /// or when `RelayMode::Disabled`.
    pub fn home_relay(&self) -> Option<RelayUrl> {
        self.inner.addr().relay_urls().next().cloned()
    }

    /// Snapshot of the direct addresses other peers can dial us at.
    /// May be empty immediately after bind — typically populated once
    /// the address-lookup service has probed interfaces.
    pub fn direct_addresses(&self) -> Vec<SocketAddr> {
        self.inner.addr().ip_addrs().copied().collect()
    }

    /// Local socket addresses the underlying iroh sockets are bound to.
    ///
    /// Unlike [`Self::direct_addresses`] (which routes through the
    /// `addr()` snapshot that depends on the address-lookup service),
    /// this returns the actual `bind()`-result sockets and is populated
    /// immediately on bind — including for hermetic endpoints built
    /// without the address-lookup service. Used by integration tests
    /// (Task 10) that need to seed `ReachabilityAnnouncePayload::direct_addresses`
    /// with reachable loopback sockets.
    pub fn bound_sockets(&self) -> Vec<SocketAddr> {
        self.inner.bound_sockets()
    }

    /// Escape hatch for in-crate callers that need the full iroh API
    /// (e.g. the zenoh-over-iroh transport in later tasks, which calls
    /// `.connect()` / `.accept()` directly). Kept `pub(crate)` so the
    /// public API stays minimal — external callers should add a method
    /// here rather than reaching into iroh directly.
    // ZEB-321 Phase 1 Task 5 (IrohZenohLinkManager) is the first caller.
    #[allow(dead_code)]
    pub(crate) fn inner(&self) -> &Endpoint {
        &self.inner
    }

    /// Test-only constructor that wraps an already-bound iroh
    /// `Endpoint`. Lets hermetic-mode tests build the endpoint with
    /// `presets::Minimal` + explicit loopback bind (no pkarr / DNS
    /// traffic) and then hand it to higher-level wrappers like
    /// `IrohZenohLinkManager` without going through
    /// [`Self::new_with_secret`] (which uses `presets::N0` and hangs
    /// in offline sandboxes).
    ///
    /// Cfg-gated to `test` + the `test-fixtures` feature so it can't
    /// be reached from a production build. Mirrors the gating
    /// convention used by `community_channel_log`'s
    /// `encrypt_channel_packet_with_nonce` test helper.
    #[cfg(any(test, feature = "test-fixtures"))]
    #[allow(dead_code)] // Only consumed by `#[cfg(test)]` modules; unused
                        // under bare `--features test-fixtures` builds (e.g. clippy --all-targets).
    pub(crate) fn from_endpoint_for_test(inner: Endpoint) -> Self {
        Self { inner }
    }

    /// Public alias of [`Self::from_endpoint_for_test`] for integration
    /// tests (which compile against `pub` items only — `pub(crate)`
    /// items are not visible from `tests/`).
    ///
    /// Cfg-gated to `feature = "test-fixtures"` (no bare `test` — that
    /// only fires for the crate's own unit tests, which already have
    /// `pub(crate)` access). Production builds drop this method
    /// entirely because the gate evaluates false.
    ///
    /// First caller: Task 10's
    /// `community_reachability_two_engine_integration` integration
    /// test, which builds two hermetic loopback endpoints and exchanges
    /// CRDT bytes through the Zenoh-over-Iroh transport.
    #[cfg(feature = "test-fixtures")]
    pub fn from_endpoint_for_integration_test(inner: Endpoint) -> Self {
        Self::from_endpoint_for_test(inner)
    }

    /// Gracefully close the endpoint and all open connections.
    ///
    /// Safe to call multiple times — `iroh::Endpoint::close` is
    /// idempotent (second call no-ops on the already-closed endpoint).
    pub async fn shutdown(&self) {
        self.inner.close().await;
    }
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
/// losing the secret key changes our [`EndpointId`], breaking any peer that knew
/// us by the old id.
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

/// ZEB-347: prime the one-time, process-global initialization that the
/// FIRST `iroh::Endpoint::bind()` in a process pays (~10s on CI, ~30s on
/// some macOS hosts); every subsequent bind in the same process is ~3ms.
///
/// `cargo nextest` runs each test in its own process, so every test that
/// binds a hermetic iroh endpoint pays this init once. Call this ONCE at
/// the top of such a test, OUTSIDE any `tokio::time::timeout` that guards
/// the behavior under test. That timeout exists to catch a *hung
/// behavior* (a lost wakeup, a deadlocked roundtrip), not slow hermetic
/// setup; folding the one-time init into it makes the test flaky under CI
/// parallelism (the init balloons past the budget) for zero real signal.
/// After this returns, the test's own `bind()` is the fast cached path.
///
/// A generous 120s kill-switch still bounds the warm-up itself: the init is
/// normally ~10s (CI) / ~30s (macOS) (~76s under heavy local parallelism), so
/// 120s never fires under legitimate load, but a future iroh regression that
/// makes the bind truly *hang* fails the test in ~2 min instead of stalling
/// until the 30-min job timeout. This keeps a wall-clock bound on the warm-up
/// even though it lives outside the per-test asserted timeout (Qodo + CodeAnt
/// review).
///
/// Marked `pub` + feature-gated so integration tests (which see only the
/// `--features test-fixtures` public surface) can call it too;
/// `#[allow(dead_code)]` because the non-test lib target never calls it.
#[cfg(any(test, feature = "test-fixtures"))]
#[allow(dead_code)]
pub async fn warm_up_iroh_global_init() {
    let builder = Endpoint::builder(presets::Minimal)
        .relay_mode(iroh::endpoint::RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("warm-up bind_addr loopback");
    let ep = tokio::time::timeout(std::time::Duration::from_secs(120), builder.bind())
        .await
        .expect(
            "warm-up iroh bind exceeded its 120s kill-switch — the one-time bind \
             init is normally ~10s (CI) / ~30s (macOS); a stall this long means an \
             iroh bind regression, not normal slowness",
        )
        .expect("warm-up iroh bind");
    ep.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::endpoint::RelayMode;

    /// Lifecycle smoke test against an ephemeral secret with relays
    /// disabled — keeps the test hermetic. Production callers
    /// (`new_with_secret`) keep n0's default relay behavior.
    #[tokio::test]
    async fn iroh_endpoint_inits_with_ephemeral_secret() {
        let secret = SecretKey::generate();
        let expected_id = secret.public();

        // Custom build path that disables relay discovery so the test
        // doesn't depend on outbound DERP reachability. The production
        // `new_with_secret` keeps the default relay mode.
        let inner = Endpoint::builder(presets::N0)
            .secret_key(secret)
            .alpns(vec![
                alpn::HARMONY_ZENOH_V1.to_vec(),
                alpn::HARMONY_HANDSHAKE_V1.to_vec(),
                alpn::HARMONY_PING_V1.to_vec(),
                alpn::HARMONY_FRIEND_V1.to_vec(),
                alpn::HARMONY_FRIEND_PEX_V1.to_vec(),
                alpn::HARMONY_BUTLER_DEPOSIT_V1.to_vec(),
            ])
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .expect("bind ephemeral endpoint");
        let ep = IrohEndpoint { inner };

        // Identity round-trips through the secret key we generated.
        assert_eq!(ep.node_id(), expected_id);

        // Snapshots must not panic. With relays disabled `home_relay`
        // is expected to be `None`; direct addresses may or may not
        // be populated yet (we accept either).
        let _home: Option<RelayUrl> = ep.home_relay();
        let _direct: Vec<SocketAddr> = ep.direct_addresses();

        // Graceful shutdown.
        ep.shutdown().await;
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
    }
}
