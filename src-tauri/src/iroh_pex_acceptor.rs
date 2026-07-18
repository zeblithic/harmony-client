//! ZEB-375 (Friends Phase 2a): friend-PEX catalog acceptor. Serves a signed
//! ReferralCatalog on the `harmony/friend-pex/v1` ALPN to an authenticated
//! Active friend (empty, benign catalog to anyone else). Read-only: never
//! mutates owner-state.

use std::sync::Arc;

use iroh::endpoint::Connection;
use tokio::sync::Mutex as TokioMutex;

use crate::friend_graph::{FriendGraph, FriendStatus};
use crate::friend_intro::{decode_pex_frame_or_catalog, PexDecoded, PexFrame};
use crate::iroh_friend_acceptor::{FriendAcceptorConfig, SelfHandshakeStatics};
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_types::{Hlc, OwnerAddr};
use crate::referral_catalog::*;
// EnrollmentCert via the SAME path `iroh_friend_acceptor.rs` uses.
use harmony_owner::certs::EnrollmentCert;

/// PURE serve decision. Authenticate (`to_addr` must be us) → friend-gate →
/// build the signed catalog.
///
/// * Active friend → full catalog (their referrable friends).
/// * authenticated non-friend → EMPTY catalog (still signed + subject-bound +
///   benign — it deliberately does NOT distinguish "I have no referrables" from
///   "you are not my friend").
/// * auth / `to_addr` failure → `Err` (caller closes the stream, serves
///   nothing — fail closed).
///
/// Read-only: takes `fg` by shared ref; never mutates owner-state.
#[allow(clippy::too_many_arguments)]
pub fn serve_catalog_for_request(
    fg: &FriendGraph,
    req: &CatalogRequest,
    self_owner: OwnerAddr,
    self_enrollment: EnrollmentCert,
    device2: &ed25519_dalek::SigningKey,
    at: Hlc,
    // ZEB-680 §1: threaded to the inner `authenticate_catalog_request` so a
    // revoked requester is rejected before any catalog is served.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_secs: u64,
) -> Result<ReferralCatalog, ReferralAuthError> {
    // 1. Authenticate the request against OUR owner address (rejects a request
    //    addressed to someone else, a bad cert, or a bad signature).
    authenticate_catalog_request(req, self_owner, revoked, now_secs)?;
    // 2. Friend-gate: only an ACTIVE friend gets a non-empty catalog. Anyone
    //    else (authenticated but unknown, or Pending/Revoked) gets EMPTY.
    let is_friend = fg
        .friends
        .get(&req.from_addr)
        .map(|e| e.status == FriendStatus::Active)
        .unwrap_or(false);
    let entries = if is_friend {
        collect_referrable_entries(fg)
    } else {
        Vec::new()
    };
    // 3. Sign the (possibly empty) catalog, subject-bound to the requester.
    Ok(sign_referral_catalog(
        device2,
        self_owner,
        req.from_addr,
        entries,
        at,
        self_enrollment,
    ))
}

/// Inbound dispatcher for the `harmony/friend-pex/v1` ALPN. Holds the read-only
/// handles the pure serve-decision needs plus the IO plumbing. Mirrors
/// `iroh_friend_acceptor::IrohFriendHandshakeAcceptor` (same `crdt_state` type,
/// same HLC tracker, same framing) but is strictly read-only: it never writes
/// owner-state.
pub struct IrohFriendPexAcceptor {
    /// Owner-state CRDT root (SAME type the handshake acceptor holds). Read-only
    /// here: we snapshot `friend_graph` under the lock and drop the guard before
    /// any network write — the owner-state lock is NEVER held across IO.
    crdt_state: Arc<TokioMutex<OwnerState>>,
    /// Shared HLC tracker (`device_id → last Hlc`), bumped per served catalog to
    /// stamp `ReferralCatalog.at`. Same map the handshake acceptor uses.
    hlc_tracker: Arc<TokioMutex<std::collections::BTreeMap<String, Hlc>>>,
    device_id: String,
    self_owner: OwnerAddr,
    self_enrollment: EnrollmentCert,
    device2_signing_key: Arc<ed25519_dalek::SigningKey>,
    config: FriendAcceptorConfig,
    /// ZEB-376 Task 9: F's active-introduction broker deps — the handles the
    /// `IntroduceRequest` arm needs to Case-D resolve + dial the target (X) and
    /// relay a signed `Introduction`. All optional (default `None`) so existing
    /// `new`/`with_config` callers — including the 2a `referral_catalog_roundtrip`
    /// integration test — keep compiling unchanged. The arm guards on their
    /// presence (logs + skips the F→X dial when any is absent) rather than
    /// panicking. (Task 10/11 add MORE deps here for X's `Introduction` arm.)
    pkarr_resolver: Option<Arc<harmony_pkarr::PkarrResolver>>,
    iroh_endpoint: Option<Arc<crate::iroh_endpoint::IrohEndpoint>>,
    owner_keytree: Option<Arc<crate::owner_state_crypto::KeyTree>>,
    /// ZEB-376 Task 10: X's `Introduction`-arm self-dial deps (EXTEND Task 9's
    /// broker deps above). All optional (default `None`) so existing `new`/
    /// `with_config` callers keep compiling; the arm degrades gracefully (logs +
    /// skips the X→introducee dial, or falls back to the default policy) when a
    /// handle is absent rather than panicking.
    ///
    /// Path to the connectivity-settings JSON so the `Introduction` arm reads
    /// `PeerIntroPolicy` FRESH per introduction (live-apply, no restart). `None`
    /// (tests) → the documented `FriendsOfFriends` default.
    connectivity_settings_path: Option<std::path::PathBuf>,
    /// This node's IMMUTABLE self-handshake statics (identity pub + PQ keys) used
    /// to rebuild X's own dialer `SelfHandshakeReachability` fresh per dial (the
    /// volatile home relay is read fresh from `iroh_endpoint` at dial time — the
    /// same per-dial convention `build_self_handshake_reachability` uses). `None`
    /// (tests / iroh unbound) → X dials advertising the empty self bundle.
    self_statics: Option<SelfHandshakeStatics>,
    /// ZEB-376 Task 10 (durability fix): the post-`Linked` handles
    /// [`crate::complete_introduction`] needs so an auto-`Proceed` introduction is
    /// PERSISTED + REPLICATED + SURFACED — the SAME handles the friend acceptor /
    /// `add_friend_by_key_impl` thread. All optional (default `None`) so existing
    /// `new`/`with_config` callers keep compiling; a missing handle skips that step
    /// (logs) inside `complete_introduction` rather than panicking.
    ///
    /// Owner-state `SyncEngine`: `notify_dirty()` after a successful link so the
    /// introduced friend is flushed on shutdown + replicated (both are
    /// dirty-gated). `None` (tests) → not armed.
    owner_sync_engine: Option<Arc<crate::owner_state_sync::SyncEngine>>,
    /// Case-D friend publisher: reconcile the new friend's reachability slot
    /// immediately (no wait for the next tick). `None` (tests) → skipped.
    friend_publisher: Option<Arc<crate::pkarr_friend_publisher::PkarrFriendPublisher>>,
    /// Event sink for `friend-list-changed` so the UI refreshes on the auto-link.
    /// `None` (tests) → skipped.
    event_sink: Option<Arc<dyn crate::node_event_sink::NodeEventSink>>,
    /// ZEB-376 Task 11 (AskMe): the process-local pending-request inbox X stages
    /// an `IntroductionOffer` into when `PeerIntroPolicy` is `AskMe`. The SAME
    /// store the friend handshake acceptor + accept/decline IPCs hold. `None`
    /// (tests) → the `Stage` decision logs and skips rather than staging.
    pending_requests: Option<Arc<crate::friend_requests::PendingFriendRequests>>,
    /// ZEB-376 Task 13 (abuse hygiene): process-local per-`key` cap +
    /// `(key, subject)` dedupe applied at the TOP of BOTH introduction arms —
    /// F's `IntroduceRequest` (per-requester) and X's `Introduction`
    /// (per-voucher) — before any real work, so a compromised/spammy F or
    /// requester cannot flood. It needs no external handle, so it is a plain
    /// (non-`Option`) field constructed in `with_config`: the production acceptor
    /// AND every test path always has one. A shed is LOGGED then answered with
    /// the same benign ack a normal outcome writes (no oracle).
    intro_rate_limiter: Arc<crate::friend_intro::IntroRateLimiter>,
    /// ZEB-680 §1: the live by-owner revoked-device projection, consulted by the
    /// inbound catalog + introduction verifiers (`serve_catalog_for_request`,
    /// `authenticate_introduce_request`, `build_introduction_for_request`,
    /// `verify_introduction`). A plain (non-`Option`) field like
    /// `intro_rate_limiter` — `with_config` seeds the EMPTY projection (revokes
    /// nothing) and production overrides it with the real `NodeState` handle via
    /// [`Self::with_revoked`]. Clone shares the inner `Arc<RwLock<..>>`.
    revoked: crate::revoked_device_projection::RevokedDeviceProjection,
    /// ZEB-680 §2: live handle to this node's owner trust doc. Cloned into the
    /// spawned X→introducee link (`spawn_complete_introduction`) so X's request
    /// carries X's own-fleet revocations, built FRESH from the live trust snapshot
    /// per introduction. `None` (tests / owner not loaded) carries none;
    /// production wires the real `NodeState` handle via [`Self::with_self_trust_doc`].
    self_trust_doc: Option<Arc<TokioMutex<harmony_owner::state::OwnerState>>>,
}

impl IrohFriendPexAcceptor {
    /// Build a PEX acceptor with the default timeouts.
    pub fn new(
        crdt_state: Arc<TokioMutex<OwnerState>>,
        hlc_tracker: Arc<TokioMutex<std::collections::BTreeMap<String, Hlc>>>,
        device_id: String,
        self_owner: OwnerAddr,
        self_enrollment: EnrollmentCert,
        device2_signing_key: Arc<ed25519_dalek::SigningKey>,
    ) -> Self {
        Self::with_config(
            crdt_state,
            hlc_tracker,
            device_id,
            self_owner,
            self_enrollment,
            device2_signing_key,
            FriendAcceptorConfig::default(),
        )
    }

    /// Build a PEX acceptor with explicit timeouts (tests pass sub-second
    /// deadlines; production passes the handshake acceptor's `config`).
    pub fn with_config(
        crdt_state: Arc<TokioMutex<OwnerState>>,
        hlc_tracker: Arc<TokioMutex<std::collections::BTreeMap<String, Hlc>>>,
        device_id: String,
        self_owner: OwnerAddr,
        self_enrollment: EnrollmentCert,
        device2_signing_key: Arc<ed25519_dalek::SigningKey>,
        config: FriendAcceptorConfig,
    ) -> Self {
        Self {
            crdt_state,
            hlc_tracker,
            device_id,
            self_owner,
            self_enrollment,
            device2_signing_key,
            config,
            pkarr_resolver: None,
            iroh_endpoint: None,
            owner_keytree: None,
            connectivity_settings_path: None,
            self_statics: None,
            owner_sync_engine: None,
            friend_publisher: None,
            event_sink: None,
            pending_requests: None,
            intro_rate_limiter: Arc::new(crate::friend_intro::IntroRateLimiter::new()),
            // ZEB-680 §1: default to the EMPTY projection (revokes nothing) —
            // production overrides via `with_revoked` with the real NodeState
            // handle. Tests keep the empty default.
            revoked: crate::revoked_device_projection::RevokedDeviceProjection::new(),
            // ZEB-680 §2: default to no trust doc → X's introduction link carries
            // no revocations; production wires the live handle.
            self_trust_doc: None,
        }
    }

    /// ZEB-376 Task 9: wire the pkarr resolver F's `IntroduceRequest` arm uses to
    /// Case-D resolve the target (X)'s reachability. Fluent setter (default
    /// `None`) so existing call sites keep compiling without an explicit `None`.
    pub fn with_pkarr_resolver(
        mut self,
        resolver: Option<Arc<harmony_pkarr::PkarrResolver>>,
    ) -> Self {
        self.pkarr_resolver = resolver;
        self
    }

    /// ZEB-376 Task 9: wire the iroh endpoint F dials the target (X) over on the
    /// `harmony/friend-pex/v1` ALPN. Fluent setter (default `None`).
    pub fn with_iroh_endpoint(
        mut self,
        endpoint: Option<Arc<crate::iroh_endpoint::IrohEndpoint>>,
    ) -> Self {
        self.iroh_endpoint = endpoint;
        self
    }

    /// ZEB-376 Task 9: wire the owner `KeyTree` F uses to Case-D decrypt its
    /// sealed rendezvous secret for the target (X). Fluent setter (default
    /// `None`).
    pub fn with_owner_keytree(
        mut self,
        keytree: Option<Arc<crate::owner_state_crypto::KeyTree>>,
    ) -> Self {
        self.owner_keytree = keytree;
        self
    }

    /// ZEB-376 Task 10: wire the connectivity-settings path so X's `Introduction`
    /// arm reads `PeerIntroPolicy` FRESH per introduction (live-apply, no
    /// restart). Fluent setter (default `None`).
    pub fn with_connectivity_settings_path(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.connectivity_settings_path = path;
        self
    }

    /// ZEB-376 Task 10: wire this node's IMMUTABLE self-handshake statics so X's
    /// `Introduction` arm can rebuild X's own dialer `SelfHandshakeReachability`
    /// (device bundle + node id + PQ keys) fresh per dial. Fluent setter (default
    /// `None`). The volatile home relay is NOT stored here — it is read fresh from
    /// the wired `iroh_endpoint` at dial time.
    pub fn with_self_statics(mut self, statics: Option<SelfHandshakeStatics>) -> Self {
        self.self_statics = statics;
        self
    }

    /// ZEB-376 Task 10 (durability fix): wire the owner-state `SyncEngine` so a
    /// successful auto-`Proceed` introduction link arms a debounced publish +
    /// shutdown-flush (both dirty-gated). The SAME engine the friend acceptor
    /// holds. Fluent setter (default `None`).
    pub fn with_owner_sync_engine(
        mut self,
        engine: Option<Arc<crate::owner_state_sync::SyncEngine>>,
    ) -> Self {
        self.owner_sync_engine = engine;
        self
    }

    /// ZEB-376 Task 10 (durability fix): wire the Case-D friend publisher so a
    /// successful auto-`Proceed` introduction link immediately reconciles the new
    /// friend's reachability slot. The SAME publisher the friend acceptor holds.
    /// Fluent setter (default `None`).
    pub fn with_friend_publisher(
        mut self,
        publisher: Option<Arc<crate::pkarr_friend_publisher::PkarrFriendPublisher>>,
    ) -> Self {
        self.friend_publisher = publisher;
        self
    }

    /// ZEB-376 Task 10 (durability fix): wire the event sink so a successful
    /// auto-`Proceed` introduction link emits `friend-list-changed` and the UI
    /// refreshes. Fluent setter (default `None`).
    pub fn with_event_sink(
        mut self,
        sink: Option<Arc<dyn crate::node_event_sink::NodeEventSink>>,
    ) -> Self {
        self.event_sink = sink;
        self
    }

    /// ZEB-376 Task 11 (AskMe): wire the process-local pending-request inbox X
    /// stages an `IntroductionOffer` into when `PeerIntroPolicy` is `AskMe`. The
    /// SAME store the friend handshake acceptor + accept/decline IPCs hold.
    /// Fluent setter (default `None`).
    pub fn with_pending_requests(
        mut self,
        pending: Option<Arc<crate::friend_requests::PendingFriendRequests>>,
    ) -> Self {
        self.pending_requests = pending;
        self
    }

    /// ZEB-680 §1: wire in the live `RevokedDeviceProjection` so the inbound
    /// catalog + introduction verifiers reject a revoked device. Fluent setter
    /// (default: the EMPTY projection from `with_config`). PRODUCTION MUST call
    /// this with the real `NodeState` handle; a fresh `new()` here would silently
    /// disable enforcement.
    pub fn with_revoked(
        mut self,
        revoked: crate::revoked_device_projection::RevokedDeviceProjection,
    ) -> Self {
        self.revoked = revoked;
        self
    }

    /// ZEB-680 §2: wire the live owner trust doc so X's auto-`Proceed`
    /// introduction link carries X's own-fleet revocations (built fresh per
    /// introduction). Fluent setter (default `None` from `with_config`).
    pub fn with_self_trust_doc(
        mut self,
        trust_doc: Option<Arc<TokioMutex<harmony_owner::state::OwnerState>>>,
    ) -> Self {
        self.self_trust_doc = trust_doc;
        self
    }

    /// Bump-and-return a fresh HLC stamped with this device's id. Mirrors
    /// `iroh_friend_acceptor::IrohFriendHandshakeAcceptor::next_hlc`.
    async fn next_hlc(&self) -> Hlc {
        let now_ms = wall_now_ms();
        let mut tracker = self.hlc_tracker.lock().await;
        let entry = tracker.entry(self.device_id.clone()).or_insert(Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: self.device_id.clone(),
        });
        if now_ms > entry.wall_ms {
            entry.wall_ms = now_ms;
            entry.logical = 0;
        } else {
            entry.logical = entry.logical.saturating_add(1);
        }
        entry.clone()
    }

    /// ZEB-376 Task 10: X's `Introduction`-arm `Proceed` action — gather the
    /// threaded self-dial handles, rebuild X's own dialer reachability fresh, and
    /// `tokio::spawn` [`crate::complete_introduction`] so `serve()` stays
    /// single-shot (the F-relayed stream's ack does not block on the X→introducee
    /// link). When the dial endpoint/keytree are absent (tests / iroh unbound)
    /// this logs and skips rather than panicking — the same graceful degrade as
    /// the F-broker `IntroduceRequest` arm.
    fn spawn_complete_introduction(&self, intro: &crate::friend_intro::Introduction) {
        let (Some(endpoint), Some(keytree)) =
            (self.iroh_endpoint.clone(), self.owner_keytree.clone())
        else {
            tracing::debug!(
                "ZEB-376: introduction Proceed: dial endpoint/keytree unavailable; \
                 skipping X→introducee link"
            );
            return;
        };
        // Rebuild X's own dialer reachability fresh — including a fresh home-relay
        // read from the live endpoint — the SAME per-dial convention the
        // request/redeem paths use via `build_self_handshake_reachability`.
        let self_reachability = crate::build_self_handshake_reachability(
            self.self_statics.as_ref().map(|s| s.identity_pub_64),
            self.self_statics.as_ref().map(|s| s.pq_dsa_pubkey.clone()),
            self.self_statics.as_ref().map(|s| s.pq_kem_pubkey.clone()),
            self.iroh_endpoint.as_ref(),
        );
        let subject = intro.subject;
        let reachability = intro.reachability.clone();
        let self_owner = self.self_owner;
        let self_enrollment = self.self_enrollment.clone();
        let self_device2 = Arc::clone(&self.device2_signing_key);
        let crdt_state = Arc::clone(&self.crdt_state);
        let hlc_tracker = Arc::clone(&self.hlc_tracker);
        let device_id = self.device_id.clone();
        // ZEB-376 Task 10 (durability fix): the post-`Linked` handles so the
        // introduced friend is persisted + replicated + surfaced (skipped
        // gracefully inside `complete_introduction` when a handle is `None`).
        let sync_engine = self.owner_sync_engine.clone();
        let friend_publisher = self.friend_publisher.clone();
        let event_sink = self.event_sink.clone();
        // ZEB-680 §1: clone the live projection into the spawned task so X's
        // introducee link verifies the Accepted response against revocations.
        let revoked = self.revoked.clone();
        // ZEB-680 §2: clone the live trust doc handle so X's request carries X's
        // own-fleet revocations (built fresh from the snapshot in the driver).
        let self_trust_doc = self.self_trust_doc.clone();
        tokio::spawn(async move {
            match crate::complete_introduction(
                subject,
                reachability,
                endpoint,
                crate::HandshakeDialConfig::from_env(),
                self_owner,
                // self_display: a UX hint only, and not persisted at start_node
                // (mirrors the friend acceptor's production `None`).
                None,
                self_enrollment,
                self_device2,
                self_reachability,
                keytree,
                crdt_state,
                hlc_tracker,
                device_id,
                sync_engine,
                friend_publisher,
                event_sink,
                revoked,
                self_trust_doc,
            )
            .await
            {
                Ok(outcome) => tracing::info!(
                    subject = %hex::encode(subject.0),
                    ?outcome,
                    "ZEB-376: X completed introduction link"
                ),
                Err(e) => tracing::debug!(
                    subject = %hex::encode(subject.0),
                    error = %e,
                    "ZEB-376: X introduction link failed"
                ),
            }
        });
    }

    /// ZEB-376 Task 11 (AskMe): record an introduction-offer in the pending inbox
    /// and emit the existing `friend-request-received` prompt for the user's
    /// explicit accept. On accept, `accept_friend_request` `take_offer`s this and
    /// runs [`crate::complete_introduction`] — the SAME self-dial action an
    /// auto-`Proceed` runs.
    ///
    /// The offer is stored AFTER the arm already verified the introduction
    /// (`verify_introduction` + reachability inner-sig + freshness all ran before
    /// the `decide_introduction` match), so a staged offer is trustworthy. When no
    /// pending store / event sink is wired (tests) each step logs + skips rather
    /// than panicking.
    fn stage_introduction_offer(&self, intro: &crate::friend_intro::Introduction) {
        let Some(pending) = self.pending_requests.as_ref() else {
            tracing::debug!(
                "ZEB-376: AskMe stage: no pending-request store wired (test path); \
                 skipping introduction-offer stage"
            );
            return;
        };
        pending.record_introduction_offer(
            intro.subject,
            // display: a UX hint only; the acceptor has no verified name for the
            // introducee here (mirrors the friend acceptor's production `None`).
            None,
            wall_now_ms(),
            crate::friend_requests::StoredIntroductionOffer {
                voucher: intro.voucher,
                subject: intro.subject,
                reachability: intro.reachability.clone(),
            },
        );
        // Reuse the existing `friend-request-received` event (same one Path A
        // fires) via the acceptor's event sink — the SAME sink that emits
        // `friend-list-changed` on an auto-`Proceed` link.
        match self.event_sink.as_ref() {
            Some(sink) => {
                crate::node_event_sink::emit_ser(sink.as_ref(), "friend-request-received", &())
            }
            None => tracing::debug!(
                "ZEB-376: AskMe stage: no event sink wired (test path); \
                 not emitting friend-request-received"
            ),
        }
    }

    /// Inbound bi-stream handler: read the length-prefixed `CatalogRequest`,
    /// build the signed `ReferralCatalog` via the pure serve-decision, and write
    /// the length-prefixed catalog back. Framing mirrors
    /// `iroh_friend_acceptor::handle_friend_handshake_inbound`
    /// (`accept_bi` under `io_deadline`; `[u32 LE len][body]` with a
    /// `PEX_MAX_PACKET_LEN` bound; `send.finish()`). Any error returns `Err`,
    /// which closes the stream — serving nothing (fail closed).
    async fn serve(&self, conn: &Connection) -> Result<(), String> {
        let (mut send, mut recv) = tokio::time::timeout(self.config.io_deadline, conn.accept_bi())
            .await
            .map_err(|_| "io timeout in accept_bi".to_string())?
            .map_err(|e| format!("accept_bi failed: {e}"))?;

        // Read [u32 LE length-prefix][body].
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut len_buf))
            .await
            .map_err(|_| "io timeout reading length-prefix".to_string())?
            .map_err(|e| format!("read length-prefix: {e}"))?;
        let len = crate::iroh_framing::decode_len_prefix(
            len_buf,
            PEX_MAX_PACKET_LEN,
            crate::iroh_framing::Endian::Le,
            false,
        )
        .map_err(|e| format!("length-prefix out of bounds: len={} max={}", e.len, e.max))?;
        let mut body = vec![0u8; len];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut body))
            .await
            .map_err(|_| "io timeout reading body".to_string())?
            .map_err(|e| format!("read body: {e}"))?;

        // ZEB-376 Task 9: dispatch on the decoded friend-PEX body. A bare 2a
        // `CatalogRequest` (browse) falls back to the Catalog arm — UNCHANGED
        // behavior; the tagged 2b frames route to the introduction arms.
        match decode_pex_frame_or_catalog(&body).map_err(|e| format!("decode pex body: {e}"))? {
            // ── 2a browse (UNCHANGED). ─────────────────────────────────────
            PexDecoded::Catalog(req) => {
                // Authenticate BEFORE bumping the HLC: an unauthenticated /
                // wrong-target peer must not be able to increment the server's
                // `hlc_tracker`. This is fail-closed — an `Err` here early-returns
                // and closes the stream without touching either lock (no HLC bump,
                // no catalog served). The pure `serve_catalog_for_request` below
                // re-runs this same check internally; that redundant second verify
                // is intentional defense-in-depth and keeps the pure fn
                // self-contained.
                // ZEB-378: one expiry-clock sample for BOTH this pre-HLC auth and
                // the serve-side re-auth below, so the two checks can't straddle an
                // expiry boundary (nondeterministic accept/reject near a cert's
                // expiry second).
                let now_secs = crate::iroh_friend_acceptor::wall_now_secs();
                crate::referral_catalog::authenticate_catalog_request(
                    &req,
                    self.self_owner,
                    &self.revoked,
                    now_secs,
                )
                .map_err(|e| format!("{e:?}"))?;

                // Stamp the catalog clock BEFORE taking the crdt lock so the two
                // locks (hlc_tracker, crdt_state) are never nested.
                let at = self.next_hlc().await;

                // Build the catalog under the crdt lock: snapshot the friend graph,
                // run the pure serve-decision, then DROP the guard before any
                // network write. The owner-state lock is never held across IO.
                // Read-only: no mutation.
                let cat = {
                    let state = self.crdt_state.lock().await;
                    serve_catalog_for_request(
                        &state.friend_graph,
                        &req,
                        self.self_owner,
                        self.self_enrollment.clone(),
                        &self.device2_signing_key,
                        at,
                        &self.revoked,
                        now_secs,
                    )
                    .map_err(|e| format!("serve decision: {e}"))?
                }; // guard dropped here — owner-state lock released before the write.

                let resp =
                    encode_referral_catalog(&cat).map_err(|e| format!("encode catalog: {e}"))?;
                let resp_prefix = crate::iroh_framing::encode_len_prefix(
                    resp.len(),
                    PEX_MAX_PACKET_LEN,
                    crate::iroh_framing::Endian::Le,
                    false,
                )
                .map_err(|e| format!("response too large: len={} max={}", e.len, e.max))?;

                // Write [u32 LE length-prefix][catalog CBOR] then finish().
                tokio::time::timeout(self.config.io_deadline, send.write_all(&resp_prefix))
                    .await
                    .map_err(|_| "io timeout writing length-prefix".to_string())?
                    .map_err(|e| format!("write length-prefix: {e}"))?;
                tokio::time::timeout(self.config.io_deadline, send.write_all(&resp))
                    .await
                    .map_err(|_| "io timeout writing body".to_string())?
                    .map_err(|e| format!("write body: {e}"))?;
                // `send.finish()` is sync — no timeout needed.
                send.finish().map_err(|e| format!("send.finish: {e}"))?;
                Ok(())
            }

            // ── 2b: F's broker arm (You → F "introduce me to X"). ───────────
            PexDecoded::Frame(PexFrame::IntroduceRequest(ir)) => {
                // ZEB-694 Tier 1 (pre-auth flood shield): shed a flooding endpoint
                // BEFORE any real work (auth, HLC bump, graph read, F→X dial), keyed
                // on the connecting endpoint's authenticated iroh id — un-spoofable,
                // before any verification. On a shed we LOG ("no silent truncation")
                // and still write the SAME benign ack a normal outcome writes — a
                // shed is network-indistinguishable (no oracle) and never reaches
                // `build_introduction_for_request` or the dial.
                // ZEB-711: limiter timeline = the limiter's own monotonic
                // clock, never wall time (a wall step distorts the window).
                let now = self.intro_rate_limiter.monotonic_now_ms();
                if let Err(reason) = self
                    .intro_rate_limiter
                    .admit_connection(*conn.remote_id().as_bytes(), now)
                {
                    tracing::warn!(reason, "introduction shed by connection shield");
                    return self.write_ack(&mut send).await;
                }
                // Authenticate + authorize (X must be an Active + referrable friend)
                // via the pure broker decision, then relay a signed Introduction to
                // X out-of-band (spawned) and ack the requester. The ack is
                // deliberately benign — it does NOT reveal whether X was referrable
                // (no leak of a non-opted-in friend); the requester learned X from
                // the catalog it already browsed.
                let now_secs = crate::iroh_friend_acceptor::wall_now_secs();
                // Authenticate BEFORE bumping the HLC: an unauthenticated /
                // wrong-target peer must not be able to increment the server's
                // `hlc_tracker` (the SAME invariant the Catalog arm documents above
                // its pre-HLC `authenticate_catalog_request`). On failure we still
                // write the SAME benign ack a normal outcome writes — an auth
                // failure stays network-indistinguishable (no oracle, exactly as
                // today, when this check ran only inside
                // `build_introduction_for_request`) — but WITHOUT the HLC bump, the
                // graph read, or the F→X dial. `build_introduction_for_request`
                // re-runs this exact check internally; that redundant second verify
                // is intentional defense-in-depth (the Catalog arm double-checks
                // too). One `now_secs` sample feeds BOTH checks so they can't
                // straddle a cert-expiry second (ZEB-378, as in the Catalog arm).
                if let Err(e) = crate::friend_intro::authenticate_introduce_request(
                    &ir,
                    self.self_owner,
                    &self.revoked,
                    now_secs,
                ) {
                    tracing::debug!(
                        error = ?e,
                        "ZEB-376: introduce-request pre-HLC auth failed; benign ack, no HLC bump"
                    );
                    return self.write_ack(&mut send).await;
                }
                // ZEB-694 Tier 2 (post-auth): `ir.from_addr` is now authenticated,
                // so the per-requester quota keys on a real owner. A shed still
                // writes the benign ack — no HLC bump, no graph read, no F→X dial.
                if let Err(reason) =
                    self.intro_rate_limiter
                        .admit_requester(ir.from_addr, ir.target, now)
                {
                    tracing::warn!(
                        reason,
                        key = %hex::encode(ir.from_addr.0),
                        "introduction shed by requester quota"
                    );
                    return self.write_ack(&mut send).await;
                }
                // Stamp the Introduction clock BEFORE taking the crdt lock so the
                // two locks (hlc_tracker, crdt_state) are never nested.
                let at = self.next_hlc().await;

                // Under the crdt lock: run the pure broker decision AND snapshot X's
                // sealed rendezvous secret (needed to Case-D resolve + dial X), then
                // DROP the guard before any network IO. Read-only: no mutation.
                let (decision, target_sealed) = {
                    let state = self.crdt_state.lock().await;
                    let decision = crate::friend_intro::build_introduction_for_request(
                        &ir,
                        &state.friend_graph,
                        self.self_owner,
                        self.self_enrollment.clone(),
                        &self.device2_signing_key,
                        at,
                        &self.revoked,
                        now_secs,
                    );
                    let sealed = state
                        .friend_graph
                        .friends
                        .get(&ir.target)
                        .and_then(|e| e.sealed_secret.clone());
                    (decision, sealed)
                }; // guard dropped — owner-state lock released before the dial.

                match decision {
                    Ok(intro) => match (
                        self.pkarr_resolver.clone(),
                        self.owner_keytree.clone(),
                        self.iroh_endpoint.clone(),
                        target_sealed,
                    ) {
                        (Some(resolver), Some(keytree), Some(endpoint), Some(sealed)) => {
                            // Spawn the F→X delivery so `serve()` stays single-shot
                            // and non-blocking (the requester's ack does not wait on
                            // the relay's success).
                            let target = ir.target;
                            tokio::spawn(async move {
                                if let Err(e) = crate::deliver_introduction_to_target(
                                    resolver, keytree, endpoint, target, sealed, intro,
                                )
                                .await
                                {
                                    tracing::debug!(
                                        error = %e,
                                        "ZEB-376: F→X introduction delivery failed"
                                    );
                                }
                            });
                        }
                        _ => tracing::debug!(
                            "ZEB-376: introduce-request broker: dial deps or target \
                             rendezvous secret unavailable; skipping F→X delivery"
                        ),
                    },
                    Err(e) => {
                        tracing::debug!(error = %e, "ZEB-376: introduce-request broker declined")
                    }
                }

                self.write_ack(&mut send).await
            }

            // ── 2b: X's inbound-Introduction arm (F → X vouch). ─────────────
            PexDecoded::Frame(PexFrame::Introduction(intro)) => {
                // X verifies F's vouch, verifies the RELAYED reachability is
                // self-authenticated + fresh, enforces `PeerIntroPolicy`, and on
                // `Proceed` dials the introducee to form the mutual link
                // (`established_via: Introduction`). ALL verification happens BEFORE
                // any dial; a bad vouch / reachability propagates as `Err` (the
                // stream closes, fail-closed — no ack), while a policy `Reject`/
                // `Stage`/`Proceed` all write the benign ack (which never reveals
                // the policy outcome). The owner-state lock is read then DROPPED
                // before the network dial.
                let intro = *intro;

                // ZEB-694 Tier 1: the connecting endpoint here is the DELIVERER
                // (F dialing X). Shed a flooding endpoint BEFORE any real work
                // (verify, reachability checks, policy, dial), keyed on its
                // authenticated iroh id — un-spoofable, before any verification. On
                // a shed we LOG ("no silent truncation") and still write the SAME
                // benign ack a normal outcome writes — a shed is
                // network-indistinguishable (no oracle) and never reaches
                // `verify_introduction` or the dial.
                // ZEB-711: limiter timeline = the limiter's own monotonic
                // clock, never wall time (a wall step distorts the window).
                let now = self.intro_rate_limiter.monotonic_now_ms();
                if let Err(reason) = self
                    .intro_rate_limiter
                    .admit_connection(*conn.remote_id().as_bytes(), now)
                {
                    tracing::warn!(reason, "introduction shed by connection shield");
                    return self.write_ack(&mut send).await;
                }

                let now_secs = crate::iroh_friend_acceptor::wall_now_secs();

                // 1. Is the voucher (F) currently an ACTIVE friend of ours? Read
                //    under the crdt lock; the guard drops at the end of this block,
                //    before any verification / IO.
                let voucher_is_active_friend = {
                    let state = self.crdt_state.lock().await;
                    state
                        .friend_graph
                        .friends
                        .get(&intro.voucher)
                        .map(|e| e.status == FriendStatus::Active)
                        .unwrap_or(false)
                }; // guard dropped — owner-state lock released before the dial.

                // 2. Verify F's vouch (to_addr==us, voucher-match, F's cert+sig,
                //    subject cert binds subject owner).
                //
                //    NOTE: `expected_voucher == intro.voucher` here is INTENTIONALLY
                //    self-referential — X accepts a vouch from *any* authenticated
                //    voucher; trust derives from the voucher-cert binding
                //    (`verify_introduction` proves F's device-#2 signed this exact
                //    Introduction) plus the `voucher_is_active_friend` policy gate
                //    below (`FriendsOfFriends`/`Closed` reject a non-friend voucher).
                //    Do NOT "tighten" this into a fixed expected voucher — there is
                //    no single expected F for an inbound introduction.
                crate::friend_intro::verify_introduction(
                    &intro,
                    intro.voucher,
                    self.self_owner,
                    &self.revoked,
                    now_secs,
                )
                .map_err(|e| format!("introduction verify: {e:?}"))?;

                // ZEB-694 Tier 2 (post-auth): `intro.voucher` is now verified, so
                // the per-voucher quota keys on a real owner. A shed still writes
                // the benign ack and never reaches the reachability checks or dial.
                if let Err(reason) =
                    self.intro_rate_limiter
                        .admit_voucher(intro.voucher, intro.subject, now)
                {
                    tracing::warn!(
                        reason,
                        key = %hex::encode(intro.voucher.0),
                        "introduction shed by voucher quota"
                    );
                    return self.write_ack(&mut send).await;
                }

                // 3. Verify the RELAYED reachability is self-authenticated by the
                //    subject's own device-#2 identity (the same inner check the
                //    Case-B initiator runs on a resolved record) + within the
                //    freshness window. The inner-sig HLC is the fixed CANONICAL
                //    `introduction_reachability_hlc()` — NOT `intro.at` (which is
                //    F's broker clock, unrelated to the subject's signing clock).
                //    The reachability's real HLC never rides the wire, so both the
                //    subject signer (Task 12) and X pin the same constant; freshness
                //    lives in `announced_at_ms`, checked next.
                let subj_vk = crate::dm_signing::device2_verifying_key(&intro.subject_cert)
                    .ok_or_else(|| "introduction subject cert has no device-#2 key".to_string())?;
                crate::reachability_record::verify_inner_signature(
                    &intro.reachability,
                    &intro.subject,
                    &crate::friend_intro::introduction_reachability_hlc(),
                    &subj_vk,
                )
                .map_err(|e| format!("relayed reachability inner-sig: {e:?}"))?;
                crate::reachability_record::reachability_freshness_check(
                    &intro.reachability,
                    wall_now_ms(),
                )?;

                // 4. Enforce policy — read FRESH from the settings file (live-apply,
                //    no restart). A missing path (tests) falls back to the
                //    documented default rather than Open.
                let policy = self
                    .connectivity_settings_path
                    .as_ref()
                    .map(|p| {
                        crate::connectivity_settings::ConnectivitySettings::load_or_default(p)
                            .peer_intro_policy
                    })
                    .unwrap_or(crate::friend_graph::PeerIntroPolicy::FriendsOfFriends);

                // 5. Decide + act. All three branches fall through to the ack.
                match crate::friend_intro::decide_introduction(policy, voucher_is_active_friend) {
                    crate::friend_intro::IntroDecision::Proceed => {
                        // X dials the introducee (spawned; `serve()` stays
                        // single-shot — the ack does not wait on the link).
                        self.spawn_complete_introduction(&intro);
                    }
                    crate::friend_intro::IntroDecision::Stage => {
                        // Task 11 fills this (record an offer + emit a prompt).
                        self.stage_introduction_offer(&intro);
                    }
                    crate::friend_intro::IntroDecision::Reject => {
                        tracing::debug!(
                            voucher = %hex::encode(intro.voucher.0),
                            "ZEB-376: introduction rejected by PeerIntroPolicy"
                        );
                    }
                }

                self.write_ack(&mut send).await
            }
        }
    }

    /// Write a minimal length-prefixed ack (`[u32 LE len][0x01]`) then finish the
    /// send stream. The 2b introduction directions are fire-and-relay: the dialer
    /// only needs confirmation its frame was received, not a payload. Same framing
    /// (bound, endian) as the catalog write-back so the friend-PEX wire stays
    /// uniform; `deliver_introduction_to_target` reads exactly this ack.
    async fn write_ack(&self, send: &mut iroh::endpoint::SendStream) -> Result<(), String> {
        const ACK: [u8; 1] = [0x01];
        let prefix = crate::iroh_framing::encode_len_prefix(
            ACK.len(),
            PEX_MAX_PACKET_LEN,
            crate::iroh_framing::Endian::Le,
            false,
        )
        .map_err(|e| {
            format!(
                "ack length-prefix out of bounds: len={} max={}",
                e.len, e.max
            )
        })?;
        tokio::time::timeout(self.config.io_deadline, send.write_all(&prefix))
            .await
            .map_err(|_| "io timeout writing ack length-prefix".to_string())?
            .map_err(|e| format!("write ack length-prefix: {e}"))?;
        tokio::time::timeout(self.config.io_deadline, send.write_all(&ACK))
            .await
            .map_err(|_| "io timeout writing ack body".to_string())?
            .map_err(|e| format!("write ack body: {e}"))?;
        send.finish().map_err(|e| format!("send.finish: {e}"))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::iroh_invite_acceptor::IrohHandshakeDispatcher for IrohFriendPexAcceptor {
    async fn handle_connection(&self, conn: Connection) {
        if let Err(e) = self.serve(&conn).await {
            tracing::debug!(error = %e, "friend-pex serve ended");
        }
        // Wait for the dialer to drive the close so the response bytes flush
        // before `conn` drops (same race-avoidance as the handshake acceptors).
        let _ = tokio::time::timeout(self.config.io_deadline, conn.closed()).await;
    }
}

/// Wall-clock now in epoch-milliseconds — the same one-syscall pattern
/// `iroh_friend_acceptor::wall_now_ms` uses for HLC stamping. Saturates to `0`
/// if the clock is before the epoch.
fn wall_now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::mint_test_owner;
    use crate::friend_graph::{FriendEntry, FriendOrigin};

    /// ZEB-680: an empty revoked-device projection for verifier call sites that
    /// don't exercise revocation (it revokes nothing).
    fn no_revocations() -> crate::revoked_device_projection::RevokedDeviceProjection {
        crate::revoked_device_projection::RevokedDeviceProjection::new()
    }

    /// Deterministic HLC for fixtures.
    fn hlc(n: u64) -> Hlc {
        Hlc {
            wall_ms: n,
            logical: 0,
            device_id: "test-device".to_string(),
        }
    }

    /// Build a FULL valid `FriendEntry`, varying only the lifecycle/opt-in/label
    /// the serve-decision keys on (mirrors `referral_catalog::tests::entry`).
    fn entry(status: FriendStatus, referrable: bool, display: Option<&str>) -> FriendEntry {
        FriendEntry {
            master_ed25519: [0x11; 32],
            display: display.map(str::to_string),
            status,
            established_via: FriendOrigin::Token,
            referrable,
            learned_at: hlc(1),
            sealed_secret: None,
        }
    }

    #[test]
    fn serves_full_catalog_to_active_friend() {
        let f = mint_test_owner(0x11); // server
        let r = mint_test_owner(0x22); // requester (an active friend of F)

        let mut fg = FriendGraph::default();
        // R is an Active friend of F (but not itself referrable — irrelevant: the
        // gate is on R being a friend, not on R being referrable).
        fg.friends
            .insert(r.owner, entry(FriendStatus::Active, false, Some("r")));
        // F has one referrable friend that should appear in the served catalog.
        fg.friends.insert(
            OwnerAddr([7; 16]),
            entry(FriendStatus::Active, true, Some("g")),
        );

        let req = sign_catalog_request(&r.device_key, r.owner, f.owner, r.cert.clone());
        let cat = serve_catalog_for_request(
            &fg,
            &req,
            f.owner,
            f.cert.clone(),
            &f.device_key,
            hlc(7),
            &no_revocations(),
            0,
        )
        .expect("active friend is served a catalog");

        assert_eq!(
            cat.entries.len(),
            1,
            "the single referrable friend is served"
        );
        assert_eq!(cat.entries[0].peer_owner, OwnerAddr([7; 16]));
        // The catalog is validly signed by F and subject-bound to R.
        assert!(verify_referral_catalog(&cat, f.owner, r.owner, &no_revocations(), 0).is_ok());
    }

    #[test]
    fn serves_empty_catalog_to_non_friend() {
        let f = mint_test_owner(0x11); // server
        let stranger = mint_test_owner(0x33); // authenticated, but NOT F's friend

        let mut fg = FriendGraph::default();
        // F has a referrable friend — but the stranger must NOT see it.
        fg.friends.insert(
            OwnerAddr([7; 16]),
            entry(FriendStatus::Active, true, Some("g")),
        );

        let req = sign_catalog_request(
            &stranger.device_key,
            stranger.owner,
            f.owner,
            stranger.cert.clone(),
        );
        let cat = serve_catalog_for_request(
            &fg,
            &req,
            f.owner,
            f.cert.clone(),
            &f.device_key,
            hlc(7),
            &no_revocations(),
            0,
        )
        .expect("a non-friend still gets a (benign, empty) signed catalog");

        // SECURITY: a non-friend leaks NOTHING about F's referrable friends.
        assert!(
            cat.entries.is_empty(),
            "non-friend must not learn any referrable friends"
        );
        // The empty catalog is still validly signed + subject-bound to the
        // stranger (benign: indistinguishable from "F has no referrables").
        assert!(
            verify_referral_catalog(&cat, f.owner, stranger.owner, &no_revocations(), 0).is_ok()
        );
    }

    #[test]
    fn rejects_request_addressed_to_someone_else() {
        let f = mint_test_owner(0x11); // server
        let r = mint_test_owner(0x22);

        let fg = FriendGraph::default();
        // Request addressed to 0x99, NOT to F → must be rejected before serving.
        let req = sign_catalog_request(
            &r.device_key,
            r.owner,
            OwnerAddr([0x99; 16]),
            r.cert.clone(),
        );
        let res = serve_catalog_for_request(
            &fg,
            &req,
            f.owner,
            f.cert.clone(),
            &f.device_key,
            hlc(7),
            &no_revocations(),
            0,
        );
        assert_eq!(
            res.unwrap_err(),
            ReferralAuthError::WrongTarget,
            "a request addressed to a different owner must be rejected"
        );
    }

    #[test]
    fn serves_empty_catalog_to_pending_requester() {
        let f = mint_test_owner(0x11); // server
        let requester = mint_test_owner(0x44); // present, but only Pending

        let mut fg = FriendGraph::default();
        // The requester is in the graph but NOT yet an Active friend.
        fg.friends.insert(
            requester.owner,
            entry(FriendStatus::Pending, false, Some("p")),
        );
        // F has a referrable friend — a Pending requester must NOT see it.
        fg.friends.insert(
            OwnerAddr([7; 16]),
            entry(FriendStatus::Active, true, Some("g")),
        );

        let req = sign_catalog_request(
            &requester.device_key,
            requester.owner,
            f.owner,
            requester.cert.clone(),
        );
        let cat = serve_catalog_for_request(
            &fg,
            &req,
            f.owner,
            f.cert.clone(),
            &f.device_key,
            hlc(7),
            &no_revocations(),
            0,
        )
        .expect("a Pending requester still gets a (benign, empty) signed catalog");

        // SECURITY: a non-Active friend leaks NOTHING about F's referrables.
        assert!(
            cat.entries.is_empty(),
            "a Pending requester must not learn any referrable friends"
        );
        // Still a validly signed catalog, subject-bound to the requester.
        assert!(
            verify_referral_catalog(&cat, f.owner, requester.owner, &no_revocations(), 0).is_ok()
        );
    }

    #[test]
    fn serves_empty_catalog_to_revoked_requester() {
        let f = mint_test_owner(0x11); // server
        let requester = mint_test_owner(0x44); // present, but Revoked

        let mut fg = FriendGraph::default();
        // The requester was a friend, but has since been Revoked.
        fg.friends.insert(
            requester.owner,
            entry(FriendStatus::Revoked, false, Some("x")),
        );
        // F has a referrable friend — a Revoked requester must NOT see it.
        fg.friends.insert(
            OwnerAddr([7; 16]),
            entry(FriendStatus::Active, true, Some("g")),
        );

        let req = sign_catalog_request(
            &requester.device_key,
            requester.owner,
            f.owner,
            requester.cert.clone(),
        );
        let cat = serve_catalog_for_request(
            &fg,
            &req,
            f.owner,
            f.cert.clone(),
            &f.device_key,
            hlc(7),
            &no_revocations(),
            0,
        )
        .expect("a Revoked requester still gets a (benign, empty) signed catalog");

        // SECURITY: a Revoked friend leaks NOTHING about F's referrables.
        assert!(
            cat.entries.is_empty(),
            "a Revoked requester must not learn any referrable friends"
        );
        // Still a validly signed catalog, subject-bound to the requester.
        assert!(
            verify_referral_catalog(&cat, f.owner, requester.owner, &no_revocations(), 0).is_ok()
        );
    }

    #[test]
    fn revoked_referrable_peer_is_not_served() {
        let f = mint_test_owner(0x11); // server
        let r = mint_test_owner(0x22); // requester (an active friend of F)

        let mut fg = FriendGraph::default();
        // R is an Active friend of F → entitled to F's referrable catalog.
        fg.friends
            .insert(r.owner, entry(FriendStatus::Active, false, Some("r")));
        // An Active + referrable peer — this one SHOULD appear in the catalog.
        let active_peer = OwnerAddr([7; 16]);
        fg.friends
            .insert(active_peer, entry(FriendStatus::Active, true, Some("g")));
        // A Revoked peer that is ALSO flagged referrable — the Revoked status must
        // win: it must NOT be served despite `referrable=true`.
        let revoked_peer = OwnerAddr([8; 16]);
        fg.friends
            .insert(revoked_peer, entry(FriendStatus::Revoked, true, Some("z")));

        let req = sign_catalog_request(&r.device_key, r.owner, f.owner, r.cert.clone());
        let cat = serve_catalog_for_request(
            &fg,
            &req,
            f.owner,
            f.cert.clone(),
            &f.device_key,
            hlc(7),
            &no_revocations(),
            0,
        )
        .expect("active friend is served a catalog");

        // Only the Active+referrable peer is served; the Revoked one is excluded.
        assert_eq!(
            cat.entries.len(),
            1,
            "exactly one peer (the Active+referrable one) is served"
        );
        assert_eq!(cat.entries[0].peer_owner, active_peer);
        assert!(
            !cat.entries.iter().any(|e| e.peer_owner == revoked_peer),
            "a Revoked peer must never be served even if flagged referrable"
        );
        // The catalog is validly signed by F and subject-bound to R.
        assert!(verify_referral_catalog(&cat, f.owner, r.owner, &no_revocations(), 0).is_ok());
    }
}
