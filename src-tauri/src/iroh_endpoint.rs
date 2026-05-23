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
use zeroize::Zeroizing;

/// ALPN registry for harmony-on-iroh sub-protocols. Constants are
/// referenced by both the endpoint binder (server-side `accept`) and
/// by outbound `connect` callers — keep them in one place so a typo
/// can't silently split the namespace.
pub mod alpn {
    pub const HARMONY_ZENOH_V1: &[u8] = b"harmony/zenoh/v1";
    pub const HARMONY_HANDSHAKE_V1: &[u8] = b"harmony/handshake/v1";
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
    #[error("keychain entry harmony.client/iroh.secret_key length is {len} bytes, expected 32")]
    KeychainBadLength { len: usize },
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

    /// Gracefully close the endpoint and all open connections.
    ///
    /// Safe to call multiple times — `iroh::Endpoint::close` is
    /// idempotent (second call no-ops on the already-closed endpoint).
    pub async fn shutdown(&self) {
        self.inner.close().await;
    }
}

/// Load a persisted iroh `SecretKey` from the OS keychain, or generate
/// and persist a fresh one on first run.
///
/// On keychain read failure we surface [`IrohEndpointError::Keychain`]
/// rather than silently re-generating — losing the secret key changes
/// our [`EndpointId`], breaking any peer that knew us by the old id.
pub fn load_or_create_secret_key() -> Result<SecretKey, IrohEndpointError> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER).map_err(|e| {
        IrohEndpointError::Keychain {
            context: format!("entry creation {KEYCHAIN_SERVICE}/{KEYCHAIN_USER}"),
            source: e,
        }
    })?;
    match entry.get_secret() {
        Ok(bytes) => {
            // Wrap the keychain payload in Zeroizing so the heap copy is
            // wiped on drop — see identity.rs for the canonical pattern.
            let bytes = Zeroizing::new(bytes);
            if bytes.len() != 32 {
                return Err(IrohEndpointError::KeychainBadLength { len: bytes.len() });
            }
            let mut arr: Zeroizing<[u8; 32]> = Zeroizing::new([0u8; 32]);
            arr.copy_from_slice(&bytes);
            Ok(SecretKey::from_bytes(&arr))
        }
        Err(keyring::Error::NoEntry) => {
            let key = SecretKey::generate();
            // Snapshot the secret bytes in a Zeroizing buffer so any
            // intermediate stack copy is wiped after the keychain write.
            let key_bytes: Zeroizing<[u8; 32]> = Zeroizing::new(key.to_bytes());
            entry
                .set_secret(key_bytes.as_ref())
                .map_err(|e| IrohEndpointError::Keychain {
                    context: format!("keychain write {KEYCHAIN_SERVICE}/{KEYCHAIN_USER}"),
                    source: e,
                })?;
            Ok(key)
        }
        Err(e) => Err(IrohEndpointError::Keychain {
            context: format!("keychain read {KEYCHAIN_SERVICE}/{KEYCHAIN_USER}"),
            source: e,
        }),
    }
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
    }
}
