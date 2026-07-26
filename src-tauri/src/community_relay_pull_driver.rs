//! ZEB-458 P4 Phase B: recipient-side community-relay PULL transport seam +
//! background pull driver.
//!
//! This is the recipient half of the community-relay store-and-forward path.
//! Phase A gave us [`crate::community_relay_pull::open_and_ingest`] (the pure
//! open+ingest unit) and [`crate::community_relay_pull::RelayIngestCtx`] (its
//! injectable seam, whose production impl `ProdRelayIngestCtx` lives in
//! [`crate::community_relay_prod`]). This module supplies:
//!
//! 1. [`RelayPullTransport`] — a seam for "one full pull session against one
//!    relay" (single iroh bi-stream: query → response → ingest → ack → finish),
//!    so the driver is unit-testable without iroh. The prod impl is
//!    [`IrohRelayPullTransport`].
//! 2. [`CommunityRelayPullDriver`] — the background loop that, for each Joined
//!    community and each fresh advertised relay, runs a pull session.
//!
//! The relay DEPOSIT *client* (sender side) is a separate later task — NOT
//! built here.
//!
//! ## Why the transport is a seam
//!
//! [`IrohRelayPullTransport`] drives the EXACT three-message protocol the T8
//! pull shell (`iroh_community_relay_acceptor::IrohCommunityRelayPullAcceptor`)
//! accepts: write [`RelayPullQuery`], read [`RelayPullResponse`], ingest the
//! opened blobs via [`open_and_ingest`], and — only when something was actually
//! ingested — write a [`RelayPullAckFrame`] for those content ids. The iroh
//! prod transport + `ProdRelayIngestCtx` are exercised end-to-end by the T12
//! integration test (two real iroh endpoints); this module's unit test covers
//! the driver's per-(community, relay) fan-out + query construction against a
//! mock transport.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::Signer;
use tokio::sync::Notify;

use crate::butler_deposit::{read_length_prefixed_with_max, write_length_prefixed};
use crate::community_relay::{
    decode_relay_pull_response, encode_relay_pull_ack_frame, encode_relay_pull_query,
    relay_pull_ack_sig_payload, relay_pull_sig_payload, RelayPullAckFrame, RelayPullQuery,
    RELAY_PULL_MAX_FRAME_BYTES,
};
use crate::community_relay_announce::CommunityRelayEntry;
use crate::community_relay_pull::{open_and_ingest, RelayIngestCtx};
use crate::community_relay_resolver::CommunityRelayResolver;
use crate::iroh_endpoint::IrohEndpoint;
use crate::owner_state_types::SpaceId;

/// Periodic floor between pull passes (~7.5 min) — reuse the advertisement
/// refresh cadence so the driver re-checks the resolver about as often as a
/// volunteer re-advertises. The driver is also woken immediately via its
/// `Notify`, so this is only the idle backstop.
pub const COMMUNITY_RELAY_PULL_INTERVAL_MS: u64 =
    crate::community_relay_announce::COMMUNITY_RELAY_AD_REFRESH_MS;

// =====================================================================
// RelayPullTransport seam
// =====================================================================

/// One full pull session against a relay (single bi-stream, matching the T8
/// pull shell): send query, get response, ingest the opened blobs, send the ack
/// for the successfully-ingested ids, finish. Returns the count ingested.
///
/// Seam so the driver is unit-testable without iroh.
#[async_trait]
pub trait RelayPullTransport: Send + Sync {
    async fn pull_session(
        &self,
        relay: &CommunityRelayEntry,
        query: &RelayPullQuery,
        ingest: &dyn RelayIngestCtx,
        ack_builder: &(dyn for<'a> Fn(&'a [[u8; 32]]) -> RelayPullAckFrame + Send + Sync),
    ) -> Result<usize, String>;
}

// =====================================================================
// IrohRelayPullTransport (production)
// =====================================================================

/// Production [`RelayPullTransport`]: dial the relay's
/// `harmony/community-relay-pull/v1` ALPN and drive the T8 pull shell's
/// single-bi-stream protocol (query → response → [ingest] → ack → finish),
/// bounded by `io_deadline`. Mirrors
/// [`crate::butler_deposit::IrohButlerDepositClient::deposit_to_entry`]'s
/// dial/open_bi/write/read shape against the pull ALPN.
pub struct IrohRelayPullTransport {
    pub endpoint: Arc<IrohEndpoint>,
    /// Bounds the WHOLE dial+query+response+ingest+ack exchange for one relay.
    pub io_deadline: Duration,
}

#[async_trait]
impl RelayPullTransport for IrohRelayPullTransport {
    async fn pull_session(
        &self,
        relay: &CommunityRelayEntry,
        query: &RelayPullQuery,
        ingest: &dyn RelayIngestCtx,
        ack_builder: &(dyn for<'a> Fn(&'a [[u8; 32]]) -> RelayPullAckFrame + Send + Sync),
    ) -> Result<usize, String> {
        let exchange = async {
            // Dial the relay device on the PULL ALPN (mirror deposit_to_entry's
            // EndpointAddr construction: id + optional home_relay).
            let ep_id = iroh::EndpointId::from_bytes(&relay.iroh_endpoint_id)
                .map_err(|e| format!("relay endpoint id: {e}"))?;
            let mut addr = iroh::EndpointAddr::new(ep_id);
            if !relay.home_relay.is_empty() {
                match relay.home_relay.parse::<iroh::RelayUrl>() {
                    Ok(url) => addr = addr.with_relay_url(url),
                    Err(e) => tracing::trace!(
                        relay = %relay.home_relay,
                        "ZEB-458 pull: skip malformed relay home_relay: {e}"
                    ),
                }
            }
            let conn = self
                .endpoint
                .inner()
                .connect(
                    addr,
                    crate::iroh_endpoint::alpn::HARMONY_COMMUNITY_RELAY_PULL_V1,
                )
                .await
                .map_err(|e| format!("connect: {e}"))?;
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi: {e}"))?;

            // Message 1 — write the pull query.
            let query_bytes =
                encode_relay_pull_query(query).map_err(|e| format!("encode query: {e}"))?;
            write_length_prefixed(&mut send, &query_bytes)
                .await
                .map_err(|e| format!("write query: {e}"))?;

            // Message 2 — read the pull response. This frame batches many held
            // blobs, so it uses the larger RELAY_PULL_MAX_FRAME_BYTES cap (the
            // query write above + ack write below stay on the default cap).
            let resp_bytes = read_length_prefixed_with_max(&mut recv, RELAY_PULL_MAX_FRAME_BYTES)
                .await
                .map_err(|e| format!("read response: {e}"))?;
            let resp = decode_relay_pull_response(&resp_bytes)
                .map_err(|e| format!("decode response: {e}"))?;

            // Open + ingest each held blob; collect the content ids that were
            // successfully opened AND durably ingested.
            let ids = open_and_ingest(&resp.entries, ingest).await;
            let ingested_count = ids.len();

            // Message 3 — OPTIONAL ack. Only write it when something was
            // actually ingested; an empty ack would tell the relay nothing and
            // the entries simply stay held (TTL / next pull reclaims). The ack
            // frame is built into owned bytes BEFORE `ids` is dropped.
            if !ids.is_empty() {
                let ack_bytes = encode_relay_pull_ack_frame(&ack_builder(&ids))
                    .map_err(|e| format!("encode ack: {e}"))?;
                write_length_prefixed(&mut send, &ack_bytes)
                    .await
                    .map_err(|e| format!("write ack: {e}"))?;
            }

            send.finish().map_err(|e| format!("finish: {e}"))?;
            // Dialer-driven close: the relay shell waits on `conn.closed()`
            // after serving, so close from this side (mirrors deposit_to_entry).
            conn.close(0u32.into(), b"");
            Ok::<usize, String>(ingested_count)
        };
        tokio::time::timeout(self.io_deadline, exchange)
            .await
            .map_err(|_| "pull IO timeout".to_string())?
    }
}

// =====================================================================
// CommunityRelayPullDriver
// =====================================================================

/// Enumerates the node's currently-Joined communities. A closure seam (rather
/// than a registry handle) keeps the driver unit-testable; production wires a
/// snapshot/read over the community-state registry at start_node (T11).
pub type JoinedCommunitiesFn = Arc<dyn Fn() -> Vec<SpaceId> + Send + Sync>;

/// Background pull driver: for each Joined community C and each fresh
/// advertised relay, build a signed [`RelayPullQuery`] + an ack-builder closure
/// and run one [`RelayPullTransport::pull_session`]. Woken immediately via
/// [`Self::wake_handle`]'s `Notify` and on a periodic floor.
///
/// Spawned by T11 via [`Self::spawn`]; the loop is self-contained (no inline
/// awaits reach back into start_node — see
/// [`crate::reachability_publisher::ReachabilityPublisher`] for the pattern).
pub struct CommunityRelayPullDriver {
    /// This recipient owner's address bytes — `RelayPullQuery.recipient_owner`.
    self_owner: [u8; 16],
    /// Canonical CBOR of this device's `EnrollmentCert` (re-supplied on the
    /// query and the ack so each is self-authenticating to the relay).
    enrollment_cert_bytes: Vec<u8>,
    /// The cert-bound enrolled device signing key — signs the query and ack.
    device_signing_key: Arc<ed25519_dalek::SigningKey>,
    /// Reads fresh advertised relays per community.
    resolver: Arc<CommunityRelayResolver>,
    /// The pull transport seam (iroh prod / mock).
    transport: Arc<dyn RelayPullTransport>,
    /// The recipient ingest seam (`ProdRelayIngestCtx` / stub).
    ingest: Arc<dyn RelayIngestCtx>,
    /// Enumerates the node's Joined communities.
    joined_communities: JoinedCommunitiesFn,
    /// Wakes the loop immediately (post-merge community-state change, IPC, test).
    wake: Arc<Notify>,
    /// Idle backstop between passes.
    interval: Duration,
    /// ZEB-803: pull-side health telemetry, shared with
    /// `network_health_snapshot`. `None` when nothing installed a source (unit
    /// tests, pre-boot); recording is then a no-op.
    telemetry: Option<Arc<crate::network_health::CommunityRelayPullTelemetry>>,
}

impl CommunityRelayPullDriver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        self_owner: [u8; 16],
        enrollment_cert_bytes: Vec<u8>,
        device_signing_key: Arc<ed25519_dalek::SigningKey>,
        resolver: Arc<CommunityRelayResolver>,
        transport: Arc<dyn RelayPullTransport>,
        ingest: Arc<dyn RelayIngestCtx>,
        joined_communities: JoinedCommunitiesFn,
    ) -> Self {
        Self {
            self_owner,
            enrollment_cert_bytes,
            device_signing_key,
            resolver,
            transport,
            ingest,
            joined_communities,
            wake: Arc::new(Notify::new()),
            interval: Duration::from_millis(COMMUNITY_RELAY_PULL_INTERVAL_MS),
            telemetry: None,
        }
    }

    /// ZEB-803: install the pull health telemetry sink. Builder-style so boot
    /// wiring can attach it without growing `new`'s already-long arity.
    pub fn with_telemetry(
        mut self,
        telemetry: Arc<crate::network_health::CommunityRelayPullTelemetry>,
    ) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Clone the wake handle so external callers (an event-loop hook on applied
    /// CommunityRelayAnnounce / membership change, or a test) can trigger an
    /// immediate pull pass without holding the whole `Arc<Self>`.
    pub fn wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }

    /// One pull pass: for every Joined community, pull from every fresh relay.
    /// Errors are logged and skipped — one bad relay never aborts the pass.
    pub async fn run_one_pass(&self, now_ms: u64) {
        // ZEB-803: recorded FIRST and unconditionally — before the joined-set
        // read, before any relay work. This counter's job is to prove the loop
        // is alive, which it can only do if it advances on a pass that finds
        // nothing to do. Moving it inside any branch below would reintroduce
        // the blind spot: a driver whose task died and a driver with no joined
        // communities would once again be indistinguishable.
        if let Some(t) = self.telemetry.as_ref() {
            t.record_pass_start();
        }
        let communities = (self.joined_communities)();
        for community in communities {
            let relays = self.resolver.relays_for_community(&community, now_ms);
            if relays.is_empty() {
                // ZEB-803 silent path 3: a joined community with no fresh relay
                // never entered the loop below and produced no log line at all —
                // indistinguishable from a healthy quiet channel, and a leading
                // candidate for the observed >=33-minute delivery lag. It is
                // NOT a failure (nothing was tried), so it gets its own counter
                // rather than inflating `sessionsFailed`.
                if let Some(t) = self.telemetry.as_ref() {
                    t.record_no_relay();
                }
                tracing::debug!(
                    community = ?community,
                    "ZEB-458 pull: no fresh relay advertised; nothing to pull from"
                );
                continue;
            }
            for relay in &relays {
                let query = self.build_query(&community);
                let ack_builder = self.make_ack_builder(community);
                match self
                    .transport
                    .pull_session(relay, &query, self.ingest.as_ref(), &ack_builder)
                    .await
                {
                    Ok(n) => {
                        // ZEB-803 silent path 2: `n == 0` is a healthy answer
                        // (the relay held nothing for us) and previously logged
                        // NOTHING. Recorded as a success either way; only the
                        // log stays conditional, since an info line per idle
                        // pass would drown the signal it exists to carry.
                        if let Some(t) = self.telemetry.as_ref() {
                            t.record_session_ok(
                                &community.0,
                                &relay.relay_device_id,
                                u32::try_from(n).unwrap_or(u32::MAX),
                            );
                        }
                        if n > 0 {
                            tracing::info!(
                                community = ?community,
                                relay_device = %hex::encode(relay.relay_device_id),
                                ingested = n,
                                "ZEB-458 pull: ingested blobs from relay"
                            );
                        }
                    }
                    Err(e) => {
                        // ZEB-803 silent path 1: this was debug-only, so in a
                        // production log level a failing pull path was
                        // completely invisible. The counter is the durable
                        // record; the log line stays at debug to avoid a
                        // per-pass warn storm on a node with one dead relay.
                        if let Some(t) = self.telemetry.as_ref() {
                            t.record_session_failed(&community.0, &relay.relay_device_id);
                        }
                        tracing::debug!(
                            community = ?community,
                            relay_device = %hex::encode(relay.relay_device_id),
                            error = %e,
                            "ZEB-458 pull: pull session failed; skipping relay"
                        );
                    }
                }
            }
        }
    }

    /// Build a signed pull query for `community`. The signature binds
    /// `recipient_owner ‖ community` via [`relay_pull_sig_payload`].
    fn build_query(&self, community: &SpaceId) -> RelayPullQuery {
        let sig = self
            .device_signing_key
            .sign(&relay_pull_sig_payload(&self.self_owner, community))
            .to_bytes()
            .to_vec();
        RelayPullQuery {
            signer_certs_cbor: Vec::new(),
            recipient_owner: self.self_owner,
            community_id: *community,
            requester_enrollment_cert: self.enrollment_cert_bytes.clone(),
            sig,
        }
    }

    /// Build the ack-builder closure for `community`: given the content ids the
    /// transport durably ingested, produce a signed [`RelayPullAckFrame`]. The
    /// ack signature binds `recipient_owner ‖ community ‖ sorted(content_ids)`
    /// via [`relay_pull_ack_sig_payload`].
    fn make_ack_builder(
        &self,
        community: SpaceId,
    ) -> impl Fn(&[[u8; 32]]) -> RelayPullAckFrame + Send + Sync {
        let self_owner = self.self_owner;
        let cert = self.enrollment_cert_bytes.clone();
        let key = Arc::clone(&self.device_signing_key);
        move |ids: &[[u8; 32]]| {
            let sig = key
                .sign(&relay_pull_ack_sig_payload(&self_owner, &community, ids))
                .to_bytes()
                .to_vec();
            RelayPullAckFrame {
                signer_certs_cbor: Vec::new(),
                recipient_owner: self_owner,
                community_id: community,
                requester_enrollment_cert: cert.clone(),
                content_ids: ids.to_vec(),
                sig,
            }
        }
    }

    /// Spawn the driver loop: one immediate startup pass, then a pass on every
    /// `wake` notification or `interval` tick. Returns the `JoinHandle` the
    /// caller may `abort()` on shutdown. Self-contained — no inline awaits reach
    /// back into start_node (the loop is `tokio::spawn`ed by T11).
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run_one_pass(now_ms()).await;
            let mut ticker = tokio::time::interval(self.interval);
            // After a slow pull pass, Tokio's default `Burst` behavior would
            // fire every missed tick back-to-back, triggering a flurry of
            // immediate re-pulls. `Skip` collapses the backlog to a single
            // tick on the next period boundary.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // `interval` fires the first tick immediately; we just ran the
            // startup pass, so consume it.
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = self.wake.notified() => {
                        self.run_one_pass(now_ms()).await;
                    }
                    _ = ticker.tick() => {
                        self.run_one_pass(now_ms()).await;
                    }
                }
            }
        })
    }
}

/// Wall-clock now in epoch-ms (the resolver's freshness clock).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use ed25519_dalek::Verifier;
    use zeroize::Zeroizing;

    use crate::butler_deposit::DepositPayload;
    use crate::community_relay::COMMUNITY_RELAY_PULL_SIG_DOMAIN;
    use crate::community_relay_announce::CommunityRelayAnnouncePayload;
    use crate::owner_state_types::{Hlc, OwnerAddr};

    // ------------------------------------------------------------------
    // MockTransport: records (relay_device_id, query.community_id, query)
    // per pull_session call and returns a canned ingested count.
    // ------------------------------------------------------------------
    struct MockTransport {
        ingested: usize,
        calls: StdMutex<Vec<([u8; 16], SpaceId, RelayPullQuery)>>,
        /// ZEB-803 (CodeRabbit, PR #556): drive the `Err` arm of
        /// `pull_session`. Without this the failure path — the silent path the
        /// module comment singles out as invisible in production logs — had no
        /// driver-level coverage at all.
        fail: bool,
    }

    impl MockTransport {
        fn new(ingested: usize) -> Self {
            Self {
                ingested,
                calls: StdMutex::new(Vec::new()),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                ingested: 0,
                calls: StdMutex::new(Vec::new()),
                fail: true,
            }
        }

        fn calls(&self) -> Vec<([u8; 16], SpaceId, RelayPullQuery)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RelayPullTransport for MockTransport {
        async fn pull_session(
            &self,
            relay: &CommunityRelayEntry,
            query: &RelayPullQuery,
            _ingest: &dyn RelayIngestCtx,
            ack_builder: &(dyn for<'a> Fn(&'a [[u8; 32]]) -> RelayPullAckFrame + Send + Sync),
        ) -> Result<usize, String> {
            self.calls.lock().unwrap().push((
                relay.relay_device_id,
                query.community_id,
                query.clone(),
            ));
            if self.fail {
                return Err("mock transport: forced failure".to_string());
            }
            // Exercise the ack-builder so a panic in it is caught by the test:
            // pretend we ingested one content id.
            let _ = ack_builder(&[[0xCD; 32]]);
            Ok(self.ingested)
        }
    }

    // ------------------------------------------------------------------
    // StubIngest: a no-op RelayIngestCtx — the mock transport never calls
    // into it, but the driver/transport seam requires one.
    // ------------------------------------------------------------------
    struct StubIngest;

    #[async_trait]
    impl RelayIngestCtx for StubIngest {
        fn device_x25519_privs(&self) -> Vec<Zeroizing<[u8; 32]>> {
            Vec::new()
        }
        async fn ingest_recovered(
            &self,
            _sender_owner: [u8; 16],
            _payload: DepositPayload,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    fn hlc(ms: u64) -> Hlc {
        Hlc {
            wall_ms: ms,
            logical: 0,
            device_id: "test".into(),
        }
    }

    /// A fresh relay advertisement payload for `community`, seeded so the
    /// resolver returns it as fresh at `now`.
    fn fresh_payload(seed: u8, ad_at: u64) -> CommunityRelayAnnouncePayload {
        CommunityRelayAnnouncePayload {
            relay: CommunityRelayEntry {
                relay_device_id: [seed; 16],
                iroh_endpoint_id: [seed; 32],
                relay_device_ed25519_verify: [seed; 32],
                home_relay: "https://r/".into(),
            },
            ad_at,
            identity_signature: [0; 64],
        }
    }

    /// Build a driver wired with the given transport + a resolver seeded with
    /// one fresh relay per supplied community, joined-communities = `communities`.
    fn make_driver(
        transport: Arc<dyn RelayPullTransport>,
        communities: Vec<SpaceId>,
        now: u64,
        seed_relays: bool,
    ) -> (
        Arc<CommunityRelayPullDriver>,
        Arc<ed25519_dalek::SigningKey>,
    ) {
        let resolver = Arc::new(CommunityRelayResolver::new());
        if seed_relays {
            for (i, c) in communities.iter().enumerate() {
                // Distinct relay seed per community so calls are distinguishable.
                let seed = 0x10 + i as u8;
                resolver.update(
                    *c,
                    OwnerAddr([seed; 16]),
                    fresh_payload(seed, now),
                    hlc(now),
                );
            }
        }
        let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));
        let communities_for_closure = communities.clone();
        let joined: JoinedCommunitiesFn = Arc::new(move || communities_for_closure.clone());
        let driver = Arc::new(CommunityRelayPullDriver::new(
            [0x01; 16],
            vec![0xCE, 0x11],
            Arc::clone(&signing_key),
            resolver,
            transport,
            Arc::new(StubIngest),
            joined,
        ));
        (driver, signing_key)
    }

    // ── ZEB-803: pull-side observability at the call site ──
    //
    // The telemetry unit tests in `network_health` prove the counters count.
    // These prove the DRIVER calls them at the right moments — which is the
    // part that failed in production, where every counter would have been
    // correct and simply never incremented.

    /// Build a driver with telemetry attached, returning the shared handle.
    fn make_driver_with_telemetry(
        transport: Arc<dyn RelayPullTransport>,
        communities: Vec<SpaceId>,
        now: u64,
        seed_relays: bool,
    ) -> (
        Arc<CommunityRelayPullDriver>,
        Arc<crate::network_health::CommunityRelayPullTelemetry>,
    ) {
        let (driver, _key) = make_driver(transport, communities, now, seed_relays);
        let telemetry = Arc::new(crate::network_health::CommunityRelayPullTelemetry::new());
        // `make_driver` hands back an Arc; rebuild with the sink attached.
        let driver = Arc::new(
            Arc::try_unwrap(driver)
                .unwrap_or_else(|_| panic!("sole owner"))
                .with_telemetry(Arc::clone(&telemetry)),
        );
        (driver, telemetry)
    }

    #[tokio::test]
    async fn pass_start_is_recorded_even_with_zero_joined_communities() {
        // THE liveness guarantee. A driver whose task died and a driver with
        // nothing to do are indistinguishable in every success counter; they
        // must differ here. If someone later moves `record_pass_start` inside
        // the community loop as a tidy-up, this fails — which is the point,
        // because that edit silently restores the blind spot that let a
        // >=33-minute delivery lag look like a quiet channel.
        let transport = Arc::new(MockTransport::new(0));
        let (driver, telemetry) =
            make_driver_with_telemetry(transport, Vec::new(), 1_700_000_000_000, false);

        driver.run_one_pass(1_700_000_000_000).await;
        driver.run_one_pass(1_700_000_000_001).await;

        let s = telemetry.summary();
        assert_eq!(s.passes_run, 2, "the loop must prove liveness with no work");
        assert!(s.last_pass_ms.is_some());
        assert_eq!(s.sessions_ok, 0);
        assert_eq!(s.sessions_failed, 0);
        assert_eq!(s.passes_no_relay, 0, "no communities ⇒ no no-relay rows");
    }

    #[tokio::test]
    async fn joined_community_with_no_fresh_relay_is_counted_not_silent() {
        // ZEB-803 silent path 3, at the call site. Before this, the resolver
        // returning nothing meant the inner loop was never entered and NOTHING
        // was emitted — not even a debug line — so the state was
        // indistinguishable from a healthy quiet channel.
        let transport = Arc::new(MockTransport::new(0));
        let community = SpaceId([0xAB; 16]);
        let (driver, telemetry) = make_driver_with_telemetry(
            transport.clone(),
            vec![community],
            1_700_000_000_000,
            // NO relays seeded — this is the whole point.
            false,
        );

        driver.run_one_pass(1_700_000_000_000).await;

        let s = telemetry.summary();
        assert_eq!(s.passes_run, 1);
        assert_eq!(s.passes_no_relay, 1, "the silent path must leave a trace");
        assert_eq!(
            s.sessions_failed, 0,
            "nothing was tried, so this is NOT a failure — conflating them \
             would send an operator hunting a transport fault that isn't there"
        );
        assert_eq!(s.sessions_ok, 0);
        // CodeRabbit PR #556: the counter carries this signal; the session ring
        // must stay clean so a many-community node cannot evict its ok/failed
        // rows with non-session events.
        assert!(
            s.recent.is_empty(),
            "no-relay is a counter, not a session outcome"
        );
    }

    #[tokio::test]
    async fn failed_pull_session_is_recorded_by_the_driver() {
        // CodeRabbit PR #556: the Err arm had no driver-level coverage, and it
        // is the path that logs at debug only — invisible in production. A
        // counter that is never incremented is worth nothing, so the call site
        // needs its own test, not just the telemetry unit.
        let now = 1_700_000_000_000u64;
        let transport = Arc::new(MockTransport::failing());
        let community = SpaceId([0xBE; 16]);
        let (driver, telemetry) =
            make_driver_with_telemetry(transport.clone(), vec![community], now, true);

        driver.run_one_pass(now).await;

        let s = telemetry.summary();
        assert_eq!(s.passes_run, 1);
        assert_eq!(s.sessions_failed, 1, "the Err arm must be recorded");
        assert_eq!(s.sessions_ok, 0);
        assert_eq!(s.passes_no_relay, 0, "a relay WAS available and was tried");
        assert_eq!(s.blobs_ingested, 0);
        assert_eq!(s.recent.len(), 1);
        assert_eq!(s.recent[0].outcome, "failed");
    }

    #[tokio::test]
    async fn successful_pull_records_session_and_ingest_counts() {
        let now = 1_700_000_000_000u64;
        let transport = Arc::new(MockTransport::new(3));
        let community = SpaceId([0xCD; 16]);
        let (driver, telemetry) =
            make_driver_with_telemetry(transport.clone(), vec![community], now, true);

        driver.run_one_pass(now).await;

        let s = telemetry.summary();
        assert_eq!(s.passes_run, 1);
        assert_eq!(s.passes_no_relay, 0);
        assert_eq!(s.sessions_ok, 1, "one relay seeded ⇒ one session");
        assert_eq!(s.sessions_failed, 0);
        // CodeRabbit PR #556: with n == 0 this test never reached the
        // `ingested > 0` branch it is named for, so blobs_ingested and
        // last_ingest_ms were untested from the call site.
        assert_eq!(s.blobs_ingested, 3);
        assert!(s.last_ingest_ms.is_some());
        assert_eq!(s.recent.len(), 1);
        assert_eq!(s.recent[0].outcome, "ok");
        assert_eq!(s.recent[0].ingested, 3);
    }

    #[tokio::test]
    async fn driver_without_telemetry_behaves_identically() {
        // The sink is optional so unit tests and pre-boot construction need no
        // wiring. Recording must never be load-bearing for pull behaviour.
        let now = 1_700_000_000_000u64;
        let transport = Arc::new(MockTransport::new(0));
        let community = SpaceId([0xEF; 16]);
        let (driver, _key) = make_driver(transport.clone(), vec![community], now, true);

        driver.run_one_pass(now).await;

        assert_eq!(
            transport.calls().len(),
            1,
            "an unwired driver must still run its pull session"
        );
    }

    #[tokio::test]
    async fn run_one_pass_calls_pull_session_once_per_community_relay_with_signed_query() {
        let now = 1_700_000_000_000u64;
        let c1 = SpaceId([0xC1; 16]);
        let c2 = SpaceId([0xC2; 16]);
        let transport = Arc::new(MockTransport::new(3));
        let (driver, signing_key) = make_driver(transport.clone(), vec![c1, c2], now, true);

        driver.run_one_pass(now).await;

        let calls = transport.calls();
        // Exactly one pull_session per (community, relay): two communities, one
        // fresh relay each.
        assert_eq!(calls.len(), 2, "one pull_session per (community, relay)");

        // The two calls cover c1 and c2 with the right relay seed each.
        let by_community: std::collections::BTreeMap<SpaceId, ([u8; 16], RelayPullQuery)> = calls
            .iter()
            .map(|(rd, c, q)| (*c, (*rd, q.clone())))
            .collect();
        assert!(by_community.contains_key(&c1));
        assert!(by_community.contains_key(&c2));
        assert_eq!(by_community[&c1].0, [0x10; 16], "c1 relay seed");
        assert_eq!(by_community[&c2].0, [0x11; 16], "c2 relay seed");

        // Each query: recipient_owner = self, community matches, cert carried,
        // and the sig verifies under the device key over relay_pull_sig_payload.
        let vk = signing_key.verifying_key();
        for (community, (_rd, query)) in &by_community {
            assert_eq!(query.recipient_owner, [0x01; 16]);
            assert_eq!(query.community_id, *community);
            assert_eq!(query.requester_enrollment_cert, vec![0xCE, 0x11]);
            let sig_bytes: [u8; 64] = query.sig.clone().try_into().expect("64-byte sig");
            let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
            let signed = relay_pull_sig_payload(&query.recipient_owner, community);
            assert!(
                vk.verify(&signed, &sig).is_ok(),
                "query sig must verify under the device key"
            );
            // Defense-in-depth: the signed payload is domain-separated.
            assert!(signed.starts_with(COMMUNITY_RELAY_PULL_SIG_DOMAIN));
        }
    }

    #[tokio::test]
    async fn run_one_pass_skips_community_with_no_fresh_relay() {
        let now = 1_700_000_000_000u64;
        let c1 = SpaceId([0xC1; 16]);
        let transport = Arc::new(MockTransport::new(0));
        // seed_relays = false → resolver has NO ad for c1.
        let (driver, _signing_key) = make_driver(transport.clone(), vec![c1], now, false);

        driver.run_one_pass(now).await;

        assert!(
            transport.calls().is_empty(),
            "a community with no fresh relay ad must be skipped (no pull_session)"
        );
    }

    #[tokio::test]
    async fn run_one_pass_skips_relay_with_stale_ad() {
        // A relay whose ad is past the freshness window must be filtered out by
        // the resolver and never produce a pull_session.
        let now = 1_700_000_000_000u64;
        let c1 = SpaceId([0xC1; 16]);
        let resolver = Arc::new(CommunityRelayResolver::new());
        let stale_at = now - crate::community_relay_announce::COMMUNITY_RELAY_AD_FRESHNESS_MS - 1;
        resolver.update(
            c1,
            OwnerAddr([0x10; 16]),
            fresh_payload(0x10, stale_at),
            hlc(stale_at),
        );
        let transport = Arc::new(MockTransport::new(0));
        let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));
        let joined: JoinedCommunitiesFn = Arc::new(move || vec![c1]);
        let driver = Arc::new(CommunityRelayPullDriver::new(
            [0x01; 16],
            vec![0xCE, 0x11],
            signing_key,
            resolver,
            transport.clone(),
            Arc::new(StubIngest),
            joined,
        ));

        driver.run_one_pass(now).await;

        assert!(
            transport.calls().is_empty(),
            "a relay whose ad is stale must be skipped"
        );
    }

    #[tokio::test]
    async fn ack_builder_signs_the_content_id_list_under_the_device_key() {
        // The ack-builder closure the driver hands the transport must produce a
        // self-authenticating RelayPullAckFrame whose sig verifies over
        // relay_pull_ack_sig_payload.
        let now = 1_700_000_000_000u64;
        let c1 = SpaceId([0xC1; 16]);
        let transport = Arc::new(MockTransport::new(1));
        let (driver, signing_key) = make_driver(transport.clone(), vec![c1], now, true);
        let community = c1;
        let ack_builder = driver.make_ack_builder(community);

        let ids = [[0xAA; 32], [0xBB; 32]];
        let frame = ack_builder(&ids);

        assert_eq!(frame.recipient_owner, [0x01; 16]);
        assert_eq!(frame.community_id, community);
        assert_eq!(frame.requester_enrollment_cert, vec![0xCE, 0x11]);
        assert_eq!(frame.content_ids, ids.to_vec());

        let vk = signing_key.verifying_key();
        let sig_bytes: [u8; 64] = frame.sig.clone().try_into().expect("64-byte sig");
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        let signed = relay_pull_ack_sig_payload(&frame.recipient_owner, &community, &ids);
        assert!(
            vk.verify(&signed, &sig).is_ok(),
            "ack sig must verify under the device key"
        );
    }

    #[tokio::test]
    async fn spawn_runs_a_startup_pass_then_wakes_on_notify() {
        // The spawned loop calls run_one_pass with REAL wall-clock now (via the
        // module's now_ms()), so the resolver must be seeded with a real-time
        // `ad_at` for the relay to read as fresh inside the loop.
        let now = now_ms();
        let c1 = SpaceId([0xC1; 16]);
        let transport = Arc::new(MockTransport::new(0));
        let (driver, _signing_key) = make_driver(transport.clone(), vec![c1], now, true);
        let wake = driver.wake_handle();
        let handle = driver.spawn();

        // Startup pass fires with no wake.
        wait_until(|| !transport.calls().is_empty()).await;

        // A wake triggers a second pass.
        wake.notify_one();
        wait_until(|| transport.calls().len() >= 2).await;

        handle.abort();
    }

    /// Poll `cond` until true; panics after a generous cap so a broken loop
    /// fails fast instead of hanging.
    async fn wait_until(cond: impl Fn() -> bool) {
        for _ in 0..2_000 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("wait_until: condition not met within the cap");
    }
}
