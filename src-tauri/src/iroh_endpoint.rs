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
//! ## API surface notes (iroh 1.0)
//!
//! The plan's draft was written against an older iroh API surface.
//! We target `iroh = "1.0"`, where:
//!
//! - `iroh::NodeId` is `iroh::EndpointId` (a type alias for
//!   `iroh::PublicKey`).
//! - `Endpoint::builder` takes a `Preset` argument; we use
//!   `iroh::endpoint::presets::N0` for production. In iroh 1.0 the N0
//!   preset's default relay map is n0's STABLE production cluster
//!   (`use1-1.relay.n0.iroh.link.` etc.), so the production path takes
//!   the preset defaults with no `.relay_mode()` override — ZEB-619
//!   retired the ZEB-617 stable-relay pin that 0.98's canary default
//!   required here. The `default_relay_map_is_stable_non_canary` test
//!   guards that default against a future regression back to canary.
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
    /// ZEB-458 (SP2 P4): community sealed-relay deposit protocol
    /// (`RelayDepositFrame` / `RelayDepositAck`, see `community_relay` +
    /// `iroh_community_relay_acceptor`). Routed by the accept loop to the
    /// late-installed `IrohCommunityRelayDepositAcceptor` (see
    /// `IrohZenohLinkManager::install_community_relay_deposit_acceptor`);
    /// connections arriving before install are closed (sender retries — a
    /// failed relay rung never makes delivery worse). Re-exported from
    /// `community_relay` so the wire ALPN string lives in exactly one place.
    pub const HARMONY_COMMUNITY_RELAY_DEPOSIT_V1: &[u8] =
        crate::community_relay::COMMUNITY_RELAY_DEPOSIT_ALPN;
    /// ZEB-458 (SP2 P4): community sealed-relay pull protocol (`RelayPullQuery`
    /// → `RelayPullResponse` → optional `RelayPullAckFrame`, see
    /// `community_relay` + `iroh_community_relay_acceptor`). Routed to the
    /// late-installed `IrohCommunityRelayPullAcceptor`.
    pub const HARMONY_COMMUNITY_RELAY_PULL_V1: &[u8] =
        crate::community_relay::COMMUNITY_RELAY_PULL_ALPN;
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
    /// ZEB-624: authoritative, in-process view of the endpoint's CONFIGURED
    /// relay URLs. iroh 1.0.1's `Endpoint` exposes relay-map *mutators*
    /// ([`Endpoint::insert_relay`]/[`Endpoint::remove_relay`]) but no reader of
    /// the full configured map (only `home_relay_status()`, the
    /// negotiated/connected subset), so we track the configured set here: seeded
    /// at build (the custom list, or the `presets::N0` default relay map) and
    /// kept in lock-step by [`Self::apply_relay_urls`] — the ONLY path that
    /// mutates the endpoint's relay map. Shared via `Arc` so every `Clone` of
    /// this wrapper observes the same live set.
    relay_urls: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<RelayUrl>>>,
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
    /// identity. Registers the harmony ALPNs and takes the `presets::N0`
    /// relay defaults — in iroh 1.0 the N0 preset's default relay map is
    /// n0's stable production cluster (ZEB-619 retired the ZEB-617 pin).
    ///
    /// Delegates to [`Self::new_with_secret_and_relays`] with `None` (follow the
    /// preset defaults); ZEB-624 introduced the custom-relay variant for the
    /// user-configurable iroh relay list.
    pub async fn new_with_secret(secret_key: SecretKey) -> Result<Self, IrohEndpointError> {
        Self::new_with_secret_and_relays(secret_key, None).await
    }

    /// ZEB-624: build + bind like [`Self::new_with_secret`] but with an optional
    /// user-configured custom relay list. `None` (or an empty list) follows the
    /// `presets::N0` default relay map (n0 stable); `Some(non-empty)` pins
    /// exactly those relays via `RelayMode::custom`. The CONFIGURED relay set is
    /// recorded in `relay_urls` so [`Self::relay_map_urls`] can report it and
    /// [`Self::apply_relay_urls`] can diff against it for live relay-map edits.
    pub async fn new_with_secret_and_relays(
        secret_key: SecretKey,
        custom_relays: Option<Vec<RelayUrl>>,
    ) -> Result<Self, IrohEndpointError> {
        Self::new_with_secret_and_relays_inner(secret_key, custom_relays, None).await
    }

    /// ZEB-626: test seam. Same as [`Self::new_with_secret_and_relays`] but with
    /// [`hermetic_dns_resolver`] injected, so N0-path unit tests (relay-map
    /// logic) skip iroh's eager system-DNS-config read at bind — a synchronous
    /// SystemConfiguration XPC that stalls ~22s/process on macOS for
    /// unentitled processes (test binaries). Production uses the plain
    /// constructor and iroh's default system resolver.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub async fn new_with_secret_and_relays_hermetic_dns(
        secret_key: SecretKey,
        custom_relays: Option<Vec<RelayUrl>>,
    ) -> Result<Self, IrohEndpointError> {
        Self::new_with_secret_and_relays_inner(
            secret_key,
            custom_relays,
            Some(hermetic_dns_resolver()),
        )
        .await
    }

    async fn new_with_secret_and_relays_inner(
        secret_key: SecretKey,
        custom_relays: Option<Vec<RelayUrl>>,
        dns_resolver: Option<iroh::dns::DnsResolver>,
    ) -> Result<Self, IrohEndpointError> {
        let builder = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(vec![
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
            ]);
        // Seed the tracked configured-relay set from the SAME source the builder
        // binds with: the custom list, else the `presets::N0` default relay map.
        let (builder, configured): (_, std::collections::BTreeSet<RelayUrl>) = match custom_relays {
            Some(urls) if !urls.is_empty() => {
                let set = urls.iter().cloned().collect();
                (
                    builder.relay_mode(iroh::endpoint::RelayMode::custom(urls)),
                    set,
                )
            }
            _ => {
                let set = iroh::endpoint::default_relay_mode()
                    .relay_map()
                    .urls::<Vec<RelayUrl>>()
                    .into_iter()
                    .collect();
                (builder, set)
            }
        };
        let builder = match dns_resolver {
            Some(resolver) => builder.dns_resolver(resolver),
            None => builder,
        };
        let inner = builder
            .bind()
            .await
            .map_err(|e| IrohEndpointError::Bind(Box::new(e)))?;
        Ok(Self::from_parts(inner, configured))
    }

    /// Wrap an already-bound iroh [`Endpoint`] with the given CONFIGURED relay
    /// set. The single struct-literal constructor so the `relay_urls` tracking
    /// invariant lives in one place.
    fn from_parts(inner: Endpoint, relay_urls: std::collections::BTreeSet<RelayUrl>) -> Self {
        Self {
            inner,
            relay_urls: std::sync::Arc::new(std::sync::Mutex::new(relay_urls)),
        }
    }

    /// ZEB-624: the endpoint's CONFIGURED relay URLs as normalized strings,
    /// sorted. Reads the tracked set (`relay_urls`) — the authoritative view of
    /// what the endpoint's relay map holds, since iroh 1.0.1's `Endpoint` has no
    /// reader for the full configured map. The trailing slash `RelayUrl`'s
    /// `Display` adds (a relay is a host-only base) is stripped so this matches
    /// the persisted / validated wire form (`connectivity_settings`'
    /// `validate_iroh_relay_urls` normalizes the same way) — the two feed the
    /// same `get_iroh_relays` field, so they must agree. The strings still
    /// round-trip through `RelayUrl::from_str` (URL parsing re-adds the root
    /// path), which is how callers reconstruct `RelayUrl`s from them.
    pub fn relay_map_urls(&self) -> Vec<String> {
        let mut urls: Vec<String> = {
            let guard = self.relay_urls.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .iter()
                .map(|u| u.to_string().trim_end_matches('/').to_string())
                .collect()
        };
        urls.sort();
        urls
    }

    /// ZEB-624: reconcile the endpoint's relay map to exactly `target` — insert
    /// each target relay not already configured, remove each configured relay not
    /// in `target` — updating the tracked set in lock-step. Returns `(inserted,
    /// removed)` counts; `(0, 0)` when already reconciled (idempotent).
    /// `insert_relay`/`remove_relay` no-op on a closed endpoint (torn-down node),
    /// which the counts still reflect so a caller's log matches the intended diff.
    pub async fn apply_relay_urls(&self, target: &[RelayUrl]) -> (usize, usize) {
        let current: std::collections::BTreeSet<RelayUrl> = {
            let guard = self.relay_urls.lock().unwrap_or_else(|p| p.into_inner());
            guard.clone()
        };
        let target_set: std::collections::BTreeSet<RelayUrl> = target.iter().cloned().collect();
        let mut inserted = 0usize;
        let mut removed = 0usize;
        for url in &target_set {
            if !current.contains(url) {
                self.inner
                    .insert_relay(
                        url.clone(),
                        std::sync::Arc::new(iroh::RelayConfig::from(url.clone())),
                    )
                    .await;
                inserted += 1;
            }
        }
        for url in &current {
            if !target_set.contains(url) {
                self.inner.remove_relay(url).await;
                removed += 1;
            }
        }
        if inserted > 0 || removed > 0 {
            let mut guard = self.relay_urls.lock().unwrap_or_else(|p| p.into_inner());
            *guard = target_set;
        }
        (inserted, removed)
    }

    /// This endpoint's stable identity, derived from the persistent
    /// secret key. `EndpointId` is a type alias for `iroh::PublicKey`
    /// (named `NodeId` before iroh 0.94).
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

    /// A `'static` stream of [`iroh::EndpointAddr`] updates sourced from
    /// iroh's own `watch_addr` watcher, boxed so the reachability publisher
    /// can merge it into its network-change arm (ZEB-621).
    ///
    /// Uses `stream_updates_only`, which **skips the watcher's current
    /// value** — subscribing at boot does not itself emit an item, so the
    /// publisher's unconditional startup publish is never doubled. Only
    /// genuine subsequent changes (home-relay flap, direct-address churn)
    /// drive the stream. The stream ends when the last [`iroh::Endpoint`]
    /// clone drops.
    pub fn watch_addr_stream(&self) -> futures::stream::BoxStream<'static, iroh::EndpointAddr> {
        use futures::StreamExt as _;
        use iroh::Watcher as _;
        self.inner.watch_addr().stream_updates_only().boxed()
    }

    /// Nudge iroh to re-probe the local network (interfaces + relays).
    ///
    /// Thin passthrough to [`iroh::Endpoint::network_change`]. Added here in
    /// ZEB-621 Task 3; the address-change pipeline (Task 6) calls it so a
    /// locally-detected change prompts iroh to refresh before we republish.
    pub async fn network_change(&self) {
        self.inner.network_change().await;
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
        // Hermetic test endpoints bind with `RelayMode::Disabled` (empty relay
        // map), so seed an empty tracked set. Tests that exercise the relay-map
        // surface go through `new_with_secret_and_relays` instead.
        Self::from_parts(inner, std::collections::BTreeSet::new())
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

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::endpoint::RelayMode;

    /// Lifecycle smoke test against an ephemeral secret with relays
    /// disabled — keeps the test hermetic. Production callers
    /// (`new_with_secret`) use the `presets::N0` stable relay defaults.
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
                alpn::HARMONY_COMMUNITY_RELAY_DEPOSIT_V1.to_vec(),
                alpn::HARMONY_COMMUNITY_RELAY_PULL_V1.to_vec(),
                alpn::HARMONY_TUNNEL_V1.to_vec(),
                alpn::HARMONY_TUNNEL_V2.to_vec(),
            ])
            .relay_mode(RelayMode::Disabled)
            .dns_resolver(hermetic_dns_resolver())
            .bind()
            .await
            .expect("bind ephemeral endpoint");
        let ep = IrohEndpoint::from_parts(inner, std::collections::BTreeSet::new());

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

    /// Parse `relay_map_urls()` output back into a `RelayUrl` set — the
    /// round-trip comparison the ZEB-624 endpoint tests use so they don't depend
    /// on iroh's exact URL string canonicalization (trailing slash / FQDN dot).
    fn relay_url_set(ep: &IrohEndpoint) -> std::collections::BTreeSet<RelayUrl> {
        ep.relay_map_urls()
            .iter()
            .map(|s| s.parse::<RelayUrl>().expect("relay_map_urls round-trips"))
            .collect()
    }

    /// ZEB-624: a custom relay list supplied at build overrides the n0 preset
    /// default map — the configured relay map is EXACTLY the custom list.
    /// Asserts via `RelayUrl` round-trip equality (not a raw string literal) so
    /// the test is agnostic to iroh's URL canonicalization.
    #[tokio::test]
    async fn custom_relay_list_overrides_default_map() {
        let secret = SecretKey::generate();
        let custom: RelayUrl = "https://relay.example.com"
            .parse()
            .expect("parse custom relay url");
        let ep = IrohEndpoint::new_with_secret_and_relays_hermetic_dns(
            secret,
            Some(vec![custom.clone()]),
        )
        .await
        .expect("bind endpoint with custom relay");
        assert_eq!(
            relay_url_set(&ep),
            std::collections::BTreeSet::from([custom.clone()])
        );
        ep.shutdown().await;
    }

    /// ZEB-624: `apply_relay_urls` diffs the target against the configured set —
    /// one insert + one remove when swapping [A] → [B], then a no-op when the
    /// same target is re-applied (idempotent). No relay traffic is generated by
    /// merely holding a relay map, so this stays hermetic.
    #[tokio::test]
    async fn apply_relay_urls_diffs_insert_and_remove() {
        let secret = SecretKey::generate();
        let a: RelayUrl = "https://relay-a.example.com".parse().expect("parse A");
        let b: RelayUrl = "https://relay-b.example.com".parse().expect("parse B");
        let ep =
            IrohEndpoint::new_with_secret_and_relays_hermetic_dns(secret, Some(vec![a.clone()]))
                .await
                .expect("bind endpoint with [A]");
        assert_eq!(
            relay_url_set(&ep),
            std::collections::BTreeSet::from([a.clone()])
        );

        // Swap to [B]: B inserted, A removed.
        let (inserted, removed) = ep.apply_relay_urls(std::slice::from_ref(&b)).await;
        assert_eq!((inserted, removed), (1, 1));
        assert_eq!(
            relay_url_set(&ep),
            std::collections::BTreeSet::from([b.clone()])
        );

        // Re-applying the same target is a no-op.
        let (inserted2, removed2) = ep.apply_relay_urls(std::slice::from_ref(&b)).await;
        assert_eq!((inserted2, removed2), (0, 0));
        assert_eq!(
            relay_url_set(&ep),
            std::collections::BTreeSet::from([b.clone()])
        );
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
        assert_eq!(alpn::HARMONY_TUNNEL_V1, b"harmony/tunnel/v1");
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
