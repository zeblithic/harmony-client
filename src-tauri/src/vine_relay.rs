//! ZEB-811: `harmony/vine-relay/v1` — public-read vine descriptor + video
//! fan-out protocol.
//!
//! A "vine relay" is a member of a creator's pkarr-published relay set (see
//! the `Vines` pkarr case). Any follower — including one with no membership
//! or trust relationship to the creator — dials this ALPN to page the
//! creator's signed descriptor feed and pull the video bytes it names. The
//! protocol is deliberately UNAUTHENTICATED: public vine sharing is the
//! design center (ZEB-811 spec), so the wire has no membership/cert gate.
//! What bounds the resulting DoS/proxy exposure:
//!
//! - **Admission cap** ([`VINE_RELAY_MAX_CONCURRENT_SESSIONS`]): a strict
//!   ceiling on concurrently-live inbound sessions, modeled on
//!   `iroh_tunnel_acceptor`'s `InboundAdmission` — accept-then-close at
//!   capacity, never a silent drop, so a saturated relay still answers the
//!   connection attempt (with "busy") rather than leaving the peer to time
//!   out.
//! - **Frame caps**: an inbound REQUEST frame is capped at
//!   [`VINE_QUERY_MAX_FRAME_BYTES`] (tiny — it's just a cursor/limit or a CID
//!   string), rejected by the codec's bound-check-BEFORE-alloc guard
//!   (`crate::iroh_framing`) so an attacker-chosen length prefix can never
//!   drive an allocation. Outbound RESPONSE frames (descriptor pages,
//!   content meta, content chunks) are capped at
//!   [`VINE_CONTENT_MAX_FRAME_BYTES`].
//! - **Session byte budget** ([`VINE_RELAY_SESSION_BYTE_BUDGET`]): a running
//!   total of every response byte written this session; on breach the
//!   session ends immediately (even mid content-stream) — the follower's
//!   pull cursor / content retry resumes on the next session, so a breach
//!   never loses data, only defers it.
//! - **Content allowlist**: `video_bytes` is served ONLY for a CID the
//!   node's OWN cache already lists as a `video_cid` on a signature-
//!   retaining descriptor ([`VineRelayServeCtx::video_cid_is_served`]) — the
//!   allowlist is resolved from local state, never from the requester's
//!   claim, and bytes are read via `ContentStore::get_local` ONLY (never
//!   `get`) in the production ctx, so an anonymous requester can never make
//!   this node dial the mesh on its behalf. The served bytes are
//!   re-verified against the CID (`ContentId::verify_hash`) before being
//!   handed to the wire.
//! - **Share gate + own-creator scope** (`ProdVineRelayServeCtx`): both
//!   descriptor pages and content are refused unless `share_vines_publicly`
//!   is on (checked live, per request) AND the requested creator is THIS
//!   node's own creator address — a v1 relay never serves another (e.g.
//!   followed) creator's cached vines. Without this, an anonymous peer
//!   could probe an arbitrary creator address against this node and learn
//!   whether it's followed/discovered from an empty-vs-nonempty response —
//!   a zero-auth read of the private follow graph that the sig-presence
//!   filter alone does not prevent.
//!
//! ## Wire shape
//!
//! One bidirectional QUIC stream carries a REPEATED request/response loop
//! (so one session can interleave descriptor-page pulls and content fetches
//! without reconnecting). Every frame is `[u32 LE length][CBOR body]`
//! (content chunks are the one exception — raw bytes, no CBOR envelope,
//! still length-prefixed). See [`VinePullRequest`] / [`VinePullResponse`] /
//! [`VineContentRequest`] / [`VineContentMeta`].
//!
//! The session ends on client EOF (graceful — the follower drained what it
//! wanted), an idle timeout with no request for
//! [`VINE_RELAY_IO_DEADLINE_MS`], an oversize/undecodable request, or the
//! byte budget. See [`run_vine_relay_session`] for the exact loop and
//! [`VineRelayAcceptor::handle_connection`] for how each ending maps to the
//! connection close.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::iroh_framing::{read_len_prefixed, write_len_prefixed, Endian, FramingError};

// =====================================================================
// Constants (ZEB-811 plan §Global Constraints)
// =====================================================================

pub const VINE_RELAY_ALPN: &[u8] = b"harmony/vine-relay/v1";

/// Cap on an inbound REQUEST frame (`VinePullRequest`). Generous for the
/// largest legal request (`VineContentRequest` — one hex CID string) plus
/// slack; small enough that the bound-check-before-alloc guard makes an
/// attacker-chosen length prefix cost nothing.
pub const VINE_QUERY_MAX_FRAME_BYTES: usize = 64 * 1024;
/// Cap on an outbound RESPONSE frame: a descriptor page, a
/// [`VineContentMeta`], or one content chunk (chunks are
/// [`VINE_CONTENT_CHUNK_BYTES`], well under this ceiling).
pub const VINE_CONTENT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Strict ceiling on concurrently-live inbound vine-relay sessions.
pub const VINE_RELAY_MAX_CONCURRENT_SESSIONS: usize = 8;
/// Running total of response bytes a single session may write before the
/// relay ends it — bounds a single peer's cost even across a long-lived,
/// well-formed session. The follower resumes from its cursor next pass.
pub const VINE_RELAY_SESSION_BYTE_BUDGET: u64 = 256 * 1024 * 1024;
/// Per-exchange IO deadline: bounds both "how long we wait for the next
/// request" (idle timeout) and "how long a single read/write may take".
/// Precedent: `DEFAULT_RELAY_IO_DEADLINE_MS`
/// (`iroh_community_relay_acceptor.rs`).
pub const VINE_RELAY_IO_DEADLINE_MS: u64 = 30_000;
/// Server-enforced ceiling on `VinePullQuery::limit`, regardless of what the
/// requester asks for — `VineFeedCache::descriptors_for_creator_page` does
/// NOT clamp this itself (ZEB-811 Task 5), so the wire handler is the one
/// enforcement point.
pub const VINE_PULL_PAGE_LIMIT_MAX: u16 = 256;
/// Content is streamed in fixed-size chunks after the `VineContentMeta`
/// header; the last chunk is `size % VINE_CONTENT_CHUNK_BYTES` (or a full
/// chunk when it divides evenly).
pub const VINE_CONTENT_CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// Hard ceiling on the number of requests a single session may serve,
/// independent of the per-exchange idle deadline and the session byte
/// budget. Without this, an anonymous client whose requests barely advance
/// the byte budget — e.g. querying a non-served creator, or with the share
/// gate off, both of which return an almost-empty page every time — can
/// renew the idle deadline forever with cheap requests and hold one of
/// `VINE_RELAY_MAX_CONCURRENT_SESSIONS` admission slots indefinitely. 64
/// comfortably covers any legitimate pull session: the pull driver pages at
/// up to `VINE_PULL_PAGE_LIMIT_MAX` (256) rows per request, and a video
/// transfer is a single `Content` request regardless of how many chunks its
/// response is split into.
pub const VINE_RELAY_MAX_REQUESTS_PER_SESSION: usize = 64;

/// ZEB-811: emit at most one admission-saturation warning per this many ms —
/// same throttle shape as `iroh_tunnel_acceptor::REJECT_WARN_INTERVAL_MS` (a
/// flood's cheap path must not become an unbounded log-write amplifier).
const REJECT_WARN_INTERVAL_MS: u64 = 30_000;
/// Sentinel meaning "no saturation warning emitted yet" — the first
/// rejection always logs immediately rather than waiting out a window.
const WARN_NEVER: u64 = u64::MAX;

// =====================================================================
// Wire frames
// =====================================================================

/// A descriptor-page cursor query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VinePullQuery {
    #[serde(rename = "ca")]
    pub creator_addr: String,
    #[serde(rename = "at")]
    pub after_created_at: u64,
    #[serde(rename = "ai")]
    pub after_id: String,
    #[serde(rename = "lm")]
    pub limit: u16,
}

/// A content-by-CID fetch request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VineContentRequest {
    #[serde(rename = "cd")]
    pub cid_hex: String,
}

/// One request frame. Externally tagged so the CBOR encoding is a
/// single-key map (`{"q": ...}` or `{"c": ...}`) — a cheap discriminant that
/// lets one session interleave descriptor pages and content fetches without
/// a separate stream per kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VinePullRequest {
    #[serde(rename = "q")]
    Query(VinePullQuery),
    #[serde(rename = "c")]
    Content(VineContentRequest),
}

/// A page of descriptors. Each element is `serde_json::to_vec` of a
/// `VineDescriptorPayload` re-serialized from the relay's cache
/// (`VineFeedCache::descriptors_for_creator_page` returns parsed structs,
/// not the creator's original wire bytes — none are retained anywhere, so
/// re-serializing is the only option). This is verification-safe, not just
/// convenient: `vine_signing::verify_descriptor` checks the signature
/// against `descriptor_canonical_bytes(d)`, a deterministic field-based
/// encoding derived from the struct — NOT the raw wire bytes — and
/// `VineDescriptorPayload`'s serde mapping round-trips every field
/// (including `sig`/`identity_pub`) losslessly. A follower verifying this
/// re-serialized copy is verifying the identical canonical bytes the
/// creator originally signed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VinePullResponse {
    #[serde(rename = "ds")]
    pub descriptors: Vec<serde_bytes::ByteBuf>,
}

/// Header preceding a content response. `ok == false` means refused (CID
/// not allowlisted, or a local read/verify failure) — no chunks follow.
/// `ok == true` is followed by `ceil(size / VINE_CONTENT_CHUNK_BYTES)` raw
/// (non-CBOR) length-prefixed chunk frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VineContentMeta {
    #[serde(rename = "ok")]
    pub ok: bool,
    #[serde(rename = "sz")]
    pub size: u64,
}

/// Codec failure. Callers close the connection without exposing this detail
/// to the wire (anti-oracle); it exists for the pure round-trip tests and
/// local logging only.
#[derive(Debug, thiserror::Error)]
pub enum VineRelayCodecError {
    #[error("cbor encode: {0}")]
    Encode(String),
    #[error("cbor decode: {0}")]
    Decode(String),
    #[error("trailing bytes after cbor decode")]
    TrailingBytes,
}

fn encode_cbor<T: Serialize>(value: &T) -> Result<Vec<u8>, VineRelayCodecError> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)
        .map_err(|e| VineRelayCodecError::Encode(e.to_string()))?;
    Ok(buf)
}

/// Strict canonical-CBOR decode: trailing bytes are rejected (mirrors
/// `iroh_community_relay_acceptor::decode_enrollment_cert_strict`).
fn decode_cbor_strict<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, VineRelayCodecError> {
    let mut cursor = std::io::Cursor::new(bytes);
    let value: T = ciborium::from_reader(&mut cursor)
        .map_err(|e| VineRelayCodecError::Decode(e.to_string()))?;
    if cursor.position() as usize != bytes.len() {
        return Err(VineRelayCodecError::TrailingBytes);
    }
    Ok(value)
}

pub fn encode_vine_pull_request(req: &VinePullRequest) -> Result<Vec<u8>, VineRelayCodecError> {
    encode_cbor(req)
}

pub fn decode_vine_pull_request(bytes: &[u8]) -> Result<VinePullRequest, VineRelayCodecError> {
    decode_cbor_strict(bytes)
}

pub fn encode_vine_pull_response(resp: &VinePullResponse) -> Result<Vec<u8>, VineRelayCodecError> {
    encode_cbor(resp)
}

pub fn decode_vine_pull_response(bytes: &[u8]) -> Result<VinePullResponse, VineRelayCodecError> {
    decode_cbor_strict(bytes)
}

pub fn encode_vine_content_meta(meta: &VineContentMeta) -> Result<Vec<u8>, VineRelayCodecError> {
    encode_cbor(meta)
}

pub fn decode_vine_content_meta(bytes: &[u8]) -> Result<VineContentMeta, VineRelayCodecError> {
    decode_cbor_strict(bytes)
}

// =====================================================================
// Injectable serve context
// =====================================================================

/// Injectable context for the vine-relay session loop. Production
/// (`ProdVineRelayServeCtx`) holds the node's `VineFeedCache` + `ContentStore`;
/// tests implement it with in-memory fixtures.
#[async_trait::async_trait]
pub trait VineRelayServeCtx: Send + Sync {
    /// Ascending `(created_at, id)`-ordered descriptor page for `creator`,
    /// strictly after `after`, at most `limit` rows — JSON bytes,
    /// signature-retaining rows only. The caller (`run_vine_relay_session`)
    /// has already clamped `limit` to [`VINE_PULL_PAGE_LIMIT_MAX`];
    /// implementations must NOT re-widen it.
    fn descriptors_page(&self, creator: &str, after: &(u64, String), limit: usize) -> Vec<Vec<u8>>;

    /// True when `cid_hex` is the `video_cid` of at least one signature-
    /// retaining cached descriptor — the content allowlist, resolved from
    /// this node's OWN store, never from the requester's claim.
    fn video_cid_is_served(&self, cid_hex: &str) -> bool;

    /// Local-only content read (implementations MUST use `get_local`, never
    /// `get` — an anonymous requester must never make this node dial the
    /// mesh) plus a hash re-verify before the bytes are handed to the wire.
    /// `None` on any miss/verify failure.
    async fn video_bytes(&self, cid_hex: &str) -> Option<Vec<u8>>;
}

// =====================================================================
// Production serve context
// =====================================================================

/// Production [`VineRelayServeCtx`]: reads through the node's own
/// `VineFeedCache` + `ContentStore` — the same state the local IPCs and
/// event loop already maintain, so a vine relay serves exactly what it has
/// locally cached (nothing is fetched on a requester's behalf).
///
/// ZEB-811 final review: gated on TWO conditions, both enforced at
/// REQUEST time (not just at acceptor-install time, so a live settings
/// toggle takes effect on the very next request without an acceptor
/// reinstall):
///
/// - `share_gate` OFF → serve nothing (empty descriptor page, refused
///   content) — this is a live mirror of `share_vines_publicly`, the SAME
///   `Arc<AtomicBool>` `PkarrVinesPublisher` toggles via `enable`/`disable`.
/// - `creator != own_creator_addr` → serve nothing. A v1 vine relay serves
///   ONLY its own creator's vines: both the pull driver and the video
///   fallback dial a creator's OWN published relay set, so nothing in this
///   branch needs cross-creator serving. Without this scope, an anonymous
///   peer holding this node's endpoint id could query `descriptors_page`
///   for an arbitrary creator address and learn whether THIS node follows
///   or has discovered them from the empty-vs-nonempty response — a
///   zero-auth probe of the node's private follow graph that the
///   sig-presence-only filter (`VineFeedCache::descriptors_for_creator_page`)
///   does nothing to prevent, since it filters by SIGNATURE, not by
///   creator identity.
pub struct ProdVineRelayServeCtx {
    pub cache: Arc<std::sync::Mutex<crate::vine_feed_cache::VineFeedCache>>,
    pub content_store: Arc<dyn crate::content_store::ContentStore>,
    /// This node's own creator address (`node_addr` hex) — the only
    /// creator this relay will ever serve. See struct doc.
    pub own_creator_addr: String,
    /// Live mirror of `share_vines_publicly`: the SAME `Arc<AtomicBool>`
    /// `PkarrVinesPublisher` reads/writes via `enable`/`disable`, so a
    /// runtime toggle is observed on the very next request.
    pub share_gate: Arc<std::sync::atomic::AtomicBool>,
}

impl ProdVineRelayServeCtx {
    fn gate_open(&self) -> bool {
        self.share_gate.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl VineRelayServeCtx for ProdVineRelayServeCtx {
    fn descriptors_page(&self, creator: &str, after: &(u64, String), limit: usize) -> Vec<Vec<u8>> {
        // Both checks are request-time, not install-time — see struct doc.
        if !self.gate_open() || creator != self.own_creator_addr {
            return Vec::new();
        }
        // A poisoned lock fails closed (empty page) rather than panicking —
        // this ctx serves an anonymous, unauthenticated peer.
        let Ok(cache) = self.cache.lock() else {
            return Vec::new();
        };
        cache
            .descriptors_for_creator_page(creator, after, limit)
            .iter()
            .filter_map(|d| serde_json::to_vec(d).ok())
            .collect()
    }

    fn video_cid_is_served(&self, cid_hex: &str) -> bool {
        if !self.gate_open() {
            return false;
        }
        let Ok(cache) = self.cache.lock() else {
            return false;
        };
        // Scoped to `own_creator_addr` — see struct doc.
        cache.video_cid_is_served(&self.own_creator_addr, cid_hex)
    }

    async fn video_bytes(&self, cid_hex: &str) -> Option<Vec<u8>> {
        // No redundant gate/scope check here: `run_vine_relay_session`'s
        // `serve_content` only ever calls `video_bytes` after
        // `video_cid_is_served` (above) has already passed.
        let cid_bytes = crate::parse_cid_hex(cid_hex).ok()?;
        let cid = crate::owner_state_types::ContentId::from_bytes(cid_bytes);
        // `get_local` ONLY — never `get` — so an anonymous requester can
        // never make this node dial the mesh on its behalf.
        let bytes = self.content_store.get_local(&cid).await.ok().flatten()?;
        // `ContentStore` exposes no size/stat probe cheaper than a full
        // read, so the cap can only be checked AFTER the allocation —
        // residual risk is bounded, not attacker-controlled per request:
        // `get_local` only ever returns content THIS node already locally
        // admitted (via `put`/`put_serveable`), never a network blob sized
        // by a remote claim, so the one-time allocation cost here is capped
        // by what is already sitting in local storage, not by anything an
        // anonymous requester can inflate on demand.
        if bytes.len() as u64 > crate::VINE_VIDEO_MAX_BYTES {
            return None;
        }
        cid.verify_hash(&bytes).then_some(bytes)
    }
}

// =====================================================================
// Admission control (ZEB-811, modeled on iroh_tunnel_acceptor::InboundAdmission)
// =====================================================================

struct VineRelayAdmission {
    sem: Arc<Semaphore>,
    /// Monotonic base for `now_ms` — the throttle runs on this limiter's OWN
    /// monotonic clock, never wall time.
    started: Instant,
    last_warn_ms: AtomicU64,
    rejected_since_warn: AtomicU64,
}

impl VineRelayAdmission {
    fn new(cap: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(cap)),
            started: Instant::now(),
            last_warn_ms: AtomicU64::new(WARN_NEVER),
            rejected_since_warn: AtomicU64::new(0),
        }
    }

    /// Claim one session slot. `Some(permit)` admits the connection — the
    /// caller MUST hold the permit for the session's whole lifetime. `None`
    /// means the live session population is at the cap.
    fn try_admit(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.sem).try_acquire_owned().ok()
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Record one rejected session and decide whether it should log. Mirrors
    /// `iroh_tunnel_acceptor::InboundAdmission::note_rejection` exactly (see
    /// there for the first-rejection-always-logs / throttle-after
    /// rationale).
    fn note_rejection(&self, now_ms: u64) -> Option<u64> {
        self.rejected_since_warn.fetch_add(1, Ordering::Relaxed);
        let last = self.last_warn_ms.load(Ordering::Acquire);
        let due = last == WARN_NEVER || now_ms.saturating_sub(last) >= REJECT_WARN_INTERVAL_MS;
        if !due {
            return None;
        }
        // Concurrent rejections can all see the window as due; only the one
        // that installs its timestamp logs, the rest fold into the next
        // window.
        if self
            .last_warn_ms
            .compare_exchange(last, now_ms, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        Some(self.rejected_since_warn.swap(0, Ordering::Relaxed))
    }
}

// =====================================================================
// Session loop (pure — generic over any AsyncRead/AsyncWrite pair)
// =====================================================================

/// Tunable session bounds. Tests construct this with sub-MiB values;
/// production uses [`Self::default`].
#[derive(Debug, Clone, Copy)]
pub struct VineRelaySessionConfig {
    pub io_deadline: Duration,
    pub session_byte_budget: u64,
}

impl Default for VineRelaySessionConfig {
    fn default() -> Self {
        Self {
            io_deadline: Duration::from_millis(VINE_RELAY_IO_DEADLINE_MS),
            session_byte_budget: VINE_RELAY_SESSION_BYTE_BUDGET,
        }
    }
}

/// Why a session's request/response loop ended. `ClientEof` is the only
/// graceful ending — every other variant is a fault or a bound breach, and
/// [`VineRelayAcceptor::handle_connection`] closes uniformly (no detail) for
/// all of them (anti-oracle posture).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VineRelaySessionEnd {
    /// The client closed its send side (or the stream hit real EOF) while we
    /// were waiting for the next request — a normal, successful end.
    ClientEof,
    /// No request arrived within `io_deadline`, or a write stalled past it.
    IdleTimeout,
    /// The inbound request frame's length prefix exceeded
    /// `VINE_QUERY_MAX_FRAME_BYTES` — rejected before any body byte was
    /// allocated (`iroh_framing`'s bound-check-before-alloc guard).
    OversizeRequest,
    /// The request frame's body failed to CBOR-decode (malformed or
    /// trailing bytes).
    DecodeError,
    /// Writing the next response frame would exceed
    /// `VINE_RELAY_SESSION_BYTE_BUDGET` — the session ends immediately
    /// (possibly mid content-stream); the follower resumes on its next
    /// pass.
    BudgetExceeded,
    /// A response write failed at the transport level.
    WriteFailed,
    /// The session served `VINE_RELAY_MAX_REQUESTS_PER_SESSION` requests —
    /// closed to bound how long a single peer can hold an admission slot,
    /// independent of the per-exchange idle deadline and byte budget (see
    /// that constant's doc comment).
    RequestCapExceeded,
}

/// Outcome of one session: how it ended and the total response bytes
/// written (fed to the serving telemetry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VineRelaySessionResult {
    pub end: VineRelaySessionEnd,
    pub bytes_served: u64,
}

/// The pure vine-relay session loop: read one request, dispatch, write one
/// (or more, for content) response, repeat — generic over any
/// `AsyncRead`/`AsyncWrite` pair so it is directly unit-testable over
/// `tokio::io::duplex` without a real iroh `Connection`. The production
/// shell ([`VineRelayAcceptor::handle_connection`]) drives this over the
/// accepted bi-stream halves.
pub async fn run_vine_relay_session<R, W>(
    recv: &mut R,
    send: &mut W,
    ctx: &dyn VineRelayServeCtx,
    cfg: &VineRelaySessionConfig,
) -> VineRelaySessionResult
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut bytes_served: u64 = 0;
    let mut request_count: usize = 0;
    loop {
        if request_count >= VINE_RELAY_MAX_REQUESTS_PER_SESSION {
            return VineRelaySessionResult {
                end: VineRelaySessionEnd::RequestCapExceeded,
                bytes_served,
            };
        }
        request_count += 1;

        let read_res = tokio::time::timeout(
            cfg.io_deadline,
            read_len_prefixed(recv, VINE_QUERY_MAX_FRAME_BYTES, Endian::Le, false),
        )
        .await;
        let body = match read_res {
            Err(_) => {
                return VineRelaySessionResult {
                    end: VineRelaySessionEnd::IdleTimeout,
                    bytes_served,
                }
            }
            Ok(Err(FramingError::OutOfBounds(_))) => {
                return VineRelaySessionResult {
                    end: VineRelaySessionEnd::OversizeRequest,
                    bytes_served,
                }
            }
            Ok(Err(FramingError::Io(_))) => {
                return VineRelaySessionResult {
                    end: VineRelaySessionEnd::ClientEof,
                    bytes_served,
                }
            }
            Ok(Ok(b)) => b,
        };

        let req = match decode_vine_pull_request(&body) {
            Ok(r) => r,
            Err(_) => {
                return VineRelaySessionResult {
                    end: VineRelaySessionEnd::DecodeError,
                    bytes_served,
                }
            }
        };

        let outcome = match req {
            VinePullRequest::Query(q) => handle_query(send, ctx, q, &mut bytes_served, cfg).await,
            VinePullRequest::Content(c) => {
                serve_content(send, ctx, &c.cid_hex, &mut bytes_served, cfg).await
            }
        };
        if let Err(end) = outcome {
            return VineRelaySessionResult { end, bytes_served };
        }
    }
}

async fn handle_query<W: AsyncWrite + Unpin>(
    send: &mut W,
    ctx: &dyn VineRelayServeCtx,
    q: VinePullQuery,
    bytes_served: &mut u64,
    cfg: &VineRelaySessionConfig,
) -> Result<(), VineRelaySessionEnd> {
    let limit = q.limit.min(VINE_PULL_PAGE_LIMIT_MAX) as usize;
    let after = (q.after_created_at, q.after_id);
    let descriptors = ctx.descriptors_page(&q.creator_addr, &after, limit);
    let resp = VinePullResponse {
        descriptors: descriptors
            .into_iter()
            .map(serde_bytes::ByteBuf::from)
            .collect(),
    };
    let resp_bytes =
        encode_vine_pull_response(&resp).map_err(|_| VineRelaySessionEnd::WriteFailed)?;
    write_response_frame(send, &resp_bytes, bytes_served, cfg).await
}

async fn serve_content<W: AsyncWrite + Unpin>(
    send: &mut W,
    ctx: &dyn VineRelayServeCtx,
    cid_hex: &str,
    bytes_served: &mut u64,
    cfg: &VineRelaySessionConfig,
) -> Result<(), VineRelaySessionEnd> {
    // The allowlist check is resolved from THIS node's own cache — never the
    // requester's claim — and `video_bytes` is only ever consulted after it
    // passes, so an unlisted CID never reaches the content-store read.
    let bytes = if ctx.video_cid_is_served(cid_hex) {
        ctx.video_bytes(cid_hex).await
    } else {
        None
    };

    let Some(bytes) = bytes else {
        let meta_bytes = encode_vine_content_meta(&VineContentMeta { ok: false, size: 0 })
            .map_err(|_| VineRelaySessionEnd::WriteFailed)?;
        return write_response_frame(send, &meta_bytes, bytes_served, cfg).await;
    };

    let meta_bytes = encode_vine_content_meta(&VineContentMeta {
        ok: true,
        size: bytes.len() as u64,
    })
    .map_err(|_| VineRelaySessionEnd::WriteFailed)?;
    write_response_frame(send, &meta_bytes, bytes_served, cfg).await?;

    for chunk in bytes.chunks(VINE_CONTENT_CHUNK_BYTES) {
        write_response_frame(send, chunk, bytes_served, cfg).await?;
    }
    Ok(())
}

/// Write one length-prefixed response frame, first checking it would not
/// breach the session byte budget (counted on the frame BODY only — the
/// 4-byte length prefix is not counted).
async fn write_response_frame<W: AsyncWrite + Unpin>(
    send: &mut W,
    body: &[u8],
    bytes_served: &mut u64,
    cfg: &VineRelaySessionConfig,
) -> Result<(), VineRelaySessionEnd> {
    if bytes_served.saturating_add(body.len() as u64) > cfg.session_byte_budget {
        return Err(VineRelaySessionEnd::BudgetExceeded);
    }
    tokio::time::timeout(
        cfg.io_deadline,
        write_len_prefixed(send, body, VINE_CONTENT_MAX_FRAME_BYTES, Endian::Le, true),
    )
    .await
    .map_err(|_| VineRelaySessionEnd::IdleTimeout)?
    .map_err(|_| VineRelaySessionEnd::WriteFailed)?;
    *bytes_served += body.len() as u64;
    Ok(())
}

// =====================================================================
// Production acceptor shell
// =====================================================================

/// Inbound handler for the `harmony/vine-relay/v1` ALPN. Installed on the
/// iroh accept loop via
/// `IrohZenohLinkManager::install_vine_relay_acceptor`.
pub struct VineRelayAcceptor {
    ctx: Arc<dyn VineRelayServeCtx>,
    config: VineRelaySessionConfig,
    admission: VineRelayAdmission,
    telemetry: Option<Arc<crate::network_health::VineRelayServingTelemetry>>,
    /// ZEB-804: per-peer served-traffic registry, keyed by the FULL 32-byte
    /// iroh endpoint id. `None` (tests, pre-boot) — stamping is then a no-op.
    traffic: Option<Arc<crate::network_health::PeerTrafficRegistry>>,
}

impl VineRelayAcceptor {
    pub fn new(ctx: Arc<dyn VineRelayServeCtx>) -> Self {
        Self::with_config(ctx, VineRelaySessionConfig::default())
    }

    pub fn with_config(ctx: Arc<dyn VineRelayServeCtx>, config: VineRelaySessionConfig) -> Self {
        Self {
            ctx,
            config,
            admission: VineRelayAdmission::new(VINE_RELAY_MAX_CONCURRENT_SESSIONS),
            telemetry: None,
            traffic: None,
        }
    }

    /// ZEB-811: install the health telemetry sink. Builder-style so the boot
    /// wiring can attach it without a second constructor arity (mirrors
    /// `IrohCommunityRelayPullAcceptor::with_telemetry`).
    pub fn with_telemetry(
        mut self,
        telemetry: Arc<crate::network_health::VineRelayServingTelemetry>,
    ) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// ZEB-804: install the per-peer served-traffic registry. Builder-style so
    /// the boot wiring can attach it without a second constructor arity
    /// (mirrors `IrohCommunityRelayPullAcceptor::with_traffic_registry`).
    pub fn with_traffic_registry(
        mut self,
        traffic: Arc<crate::network_health::PeerTrafficRegistry>,
    ) -> Self {
        self.traffic = Some(traffic);
        self
    }

    /// Handle one inbound `harmony/vine-relay/v1` connection: admission,
    /// accept the bi-stream, drive [`run_vine_relay_session`], then close.
    pub async fn handle_connection(&self, conn: iroh::endpoint::Connection) {
        let Some(_permit) = self.admission.try_admit() else {
            if let Some(t) = self.telemetry.as_ref() {
                t.record_rejected();
            }
            if let Some(n) = self.admission.note_rejection(self.admission.now_ms()) {
                tracing::warn!(
                    rejected_since_last_warn = n,
                    cap = VINE_RELAY_MAX_CONCURRENT_SESSIONS,
                    "ZEB-811: vine relay saturated; rejecting inbound session(s)"
                );
            }
            // Accept-then-close-at-capacity: the wire signal costs the
            // saturated relay nothing, and the requester learns "busy"
            // instead of timing out.
            conn.close(1u32.into(), b"busy");
            return;
        };

        let (mut send, mut recv) =
            match tokio::time::timeout(self.config.io_deadline, conn.accept_bi()).await {
                Ok(Ok(streams)) => streams,
                Ok(Err(e)) => {
                    if let Some(t) = self.telemetry.as_ref() {
                        t.record_failed();
                    }
                    tracing::debug!(error = %e, "ZEB-811: vine relay accept_bi failed");
                    conn.close(0u32.into(), b"");
                    return;
                }
                Err(_) => {
                    if let Some(t) = self.telemetry.as_ref() {
                        t.record_failed();
                    }
                    conn.close(0u32.into(), b"");
                    return;
                }
            };

        let result =
            run_vine_relay_session(&mut recv, &mut send, self.ctx.as_ref(), &self.config).await;

        if let Some(t) = self.telemetry.as_ref() {
            t.record_served(result.bytes_served);
        }
        // ZEB-804: stamp the FULL-id served-traffic registry beside the
        // telemetry — a session that RAN on an iroh-authenticated connection is
        // traffic evidence for that peer whatever its ending (mirroring
        // `record_served` above, which also fires for every session end).
        if let Some(reg) = self.traffic.as_ref() {
            reg.record_served(
                *conn.remote_id().as_bytes(),
                crate::network_health::now_ms(),
            );
        }
        tracing::debug!(
            end = ?result.end,
            bytes_served = result.bytes_served,
            remote_id = ?conn.remote_id(),
            "ZEB-811: vine relay session ended"
        );

        match result.end {
            // Graceful: the follower drained what it wanted. Finish our
            // send side and give the peer a moment to close on its own —
            // mirrors `IrohCommunityRelayPullAcceptor`'s success path.
            VineRelaySessionEnd::ClientEof => {
                let _ = send.finish();
                let _ = tokio::time::timeout(self.config.io_deadline, conn.closed()).await;
            }
            // Every other ending is a fault or a bound breach: close
            // uniformly with no detail (anti-oracle posture,
            // `iroh_community_relay_acceptor.rs:963`).
            _ => conn.close(0u32.into(), b""),
        }
    }
}

/// ZEB-548 Stage 2: lets the transport spine hold this acceptor as a
/// generic `Arc<dyn IrohHandshakeDispatcher>` (see
/// `IrohZenohLinkManager::install_vine_relay_acceptor`) instead of naming
/// the concrete type — severing the spine→vine_relay compile edge. The
/// body delegates to the inherent `handle_connection`; in receiver syntax
/// inherent-method resolution wins over the trait method, so this does not
/// recurse.
#[async_trait::async_trait]
impl crate::iroh_endpoint::IrohHandshakeDispatcher for VineRelayAcceptor {
    async fn handle_connection(&self, conn: iroh::endpoint::Connection) {
        self.handle_connection(conn).await
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use tokio::io::{split, AsyncReadExt, AsyncWriteExt};

    /// Mock [`VineRelayServeCtx`]. `pages` holds pre-sorted ascending rows
    /// for the pagination test; `last_limit` records the `limit` the
    /// handler actually passed in (proves the server-side clamp); `served`
    /// + `video` back the content-request tests.
    struct MockCtx {
        pages: Vec<Vec<u8>>,
        last_limit: StdMutex<Option<usize>>,
        served: StdMutex<Option<String>>,
        video: StdMutex<HashMap<String, Vec<u8>>>,
    }

    impl MockCtx {
        fn empty() -> Self {
            Self {
                pages: Vec::new(),
                last_limit: StdMutex::new(None),
                served: StdMutex::new(None),
                video: StdMutex::new(HashMap::new()),
            }
        }

        fn with_rows(n: usize) -> Self {
            let pages = (0..n).map(|i| format!("row-{i:04}").into_bytes()).collect();
            Self {
                pages,
                last_limit: StdMutex::new(None),
                served: StdMutex::new(None),
                video: StdMutex::new(HashMap::new()),
            }
        }

        fn serve_cid(&self, cid_hex: &str, bytes: Vec<u8>) {
            *self.served.lock().unwrap() = Some(cid_hex.to_string());
            self.video
                .lock()
                .unwrap()
                .insert(cid_hex.to_string(), bytes);
        }
    }

    #[async_trait::async_trait]
    impl VineRelayServeCtx for MockCtx {
        fn descriptors_page(
            &self,
            _creator: &str,
            _after: &(u64, String),
            limit: usize,
        ) -> Vec<Vec<u8>> {
            *self.last_limit.lock().unwrap() = Some(limit);
            self.pages.iter().take(limit).cloned().collect()
        }

        fn video_cid_is_served(&self, cid_hex: &str) -> bool {
            self.served.lock().unwrap().as_deref() == Some(cid_hex)
        }

        async fn video_bytes(&self, cid_hex: &str) -> Option<Vec<u8>> {
            self.video.lock().unwrap().get(cid_hex).cloned()
        }
    }

    /// Build a connected duplex pair, split each end into independent
    /// read/write halves (mirrors the two distinct types
    /// `iroh::endpoint::{RecvStream, SendStream}` give the production
    /// shell). Returns `(client_read, client_write, server_read,
    /// server_write)`.
    fn duplex_pair(
        cap: usize,
    ) -> (
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(cap);
        let (client_read, client_write) = split(client);
        let (server_read, server_write) = split(server);
        (client_read, client_write, server_read, server_write)
    }

    fn fast_cfg() -> VineRelaySessionConfig {
        VineRelaySessionConfig {
            io_deadline: Duration::from_millis(500),
            ..VineRelaySessionConfig::default()
        }
    }

    #[test]
    fn frame_round_trip_query_and_content() {
        let q = VinePullRequest::Query(VinePullQuery {
            creator_addr: "alice-addr".to_string(),
            after_created_at: 42,
            after_id: "vine-1".to_string(),
            limit: 10,
        });
        let bytes = encode_vine_pull_request(&q).expect("encode query");
        assert_eq!(decode_vine_pull_request(&bytes).expect("decode query"), q);

        let c = VinePullRequest::Content(VineContentRequest {
            cid_hex: "deadbeef".to_string(),
        });
        let bytes = encode_vine_pull_request(&c).expect("encode content");
        assert_eq!(decode_vine_pull_request(&bytes).expect("decode content"), c);

        let resp = VinePullResponse {
            descriptors: vec![
                serde_bytes::ByteBuf::from(b"one".to_vec()),
                serde_bytes::ByteBuf::from(b"two".to_vec()),
            ],
        };
        let bytes = encode_vine_pull_response(&resp).expect("encode response");
        assert_eq!(
            decode_vine_pull_response(&bytes).expect("decode response"),
            resp
        );

        let meta = VineContentMeta {
            ok: true,
            size: 1234,
        };
        let bytes = encode_vine_content_meta(&meta).expect("encode meta");
        assert_eq!(decode_vine_content_meta(&bytes).expect("decode meta"), meta);
    }

    #[tokio::test]
    async fn oversize_query_frame_closes_without_reply() {
        let (mut client_read, mut client_write, mut server_read, mut server_write) =
            duplex_pair(4096);

        // A bare 4-byte LE length prefix claiming a 128 KiB body, no body
        // follows — the codec must reject before ever trying to read it.
        let oversize_len: u32 = 128 * 1024;
        client_write
            .write_all(&oversize_len.to_le_bytes())
            .await
            .expect("write oversize prefix");

        let ctx = MockCtx::empty();
        let cfg = fast_cfg();
        let result = run_vine_relay_session(&mut server_read, &mut server_write, &ctx, &cfg).await;

        assert_eq!(result.end, VineRelaySessionEnd::OversizeRequest);
        assert_eq!(result.bytes_served, 0);

        // No response frame was ever written: shut down the server's write
        // half (tokio::io::split shares the underlying stream via an Arc, so
        // a bare `drop` does NOT half-close it — `shutdown()` is the actual
        // signal) and confirm the client sees immediate EOF.
        server_write
            .shutdown()
            .await
            .expect("shutdown server_write");
        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_millis(200), client_read.read(&mut buf))
            .await
            .expect("read must not hang")
            .expect("read must not error");
        assert_eq!(
            n, 0,
            "server must not write any reply for an oversize request"
        );
    }

    #[tokio::test]
    async fn page_serves_ascending_tuples_with_limit_clamp() {
        let (mut client_read, mut client_write, mut server_read, mut server_write) =
            duplex_pair(1 << 20);

        let ctx = MockCtx::with_rows(300);
        let cfg = fast_cfg();

        let req = VinePullRequest::Query(VinePullQuery {
            creator_addr: "alice-addr".to_string(),
            after_created_at: 0,
            after_id: String::new(),
            limit: 500, // above VINE_PULL_PAGE_LIMIT_MAX — must be clamped
        });
        let req_bytes = encode_vine_pull_request(&req).expect("encode query");
        write_len_prefixed(
            &mut client_write,
            &req_bytes,
            VINE_QUERY_MAX_FRAME_BYTES,
            Endian::Le,
            false,
        )
        .await
        .expect("write query frame");
        // `tokio::io::split` shares the underlying stream via an Arc (the
        // read half keeps it alive), so a bare `drop` does NOT half-close
        // it — `shutdown()` is the real EOF signal the peer's read sees.
        client_write
            .shutdown()
            .await
            .expect("shutdown client_write");

        let result = run_vine_relay_session(&mut server_read, &mut server_write, &ctx, &cfg).await;
        assert_eq!(result.end, VineRelaySessionEnd::ClientEof);
        drop(server_write);

        assert_eq!(
            *ctx.last_limit.lock().unwrap(),
            Some(VINE_PULL_PAGE_LIMIT_MAX as usize),
            "the handler must clamp limit to VINE_PULL_PAGE_LIMIT_MAX before calling the ctx"
        );

        let resp_bytes = read_len_prefixed(
            &mut client_read,
            VINE_CONTENT_MAX_FRAME_BYTES,
            Endian::Le,
            true,
        )
        .await
        .expect("read response frame");
        let resp = decode_vine_pull_response(&resp_bytes).expect("decode response");
        assert_eq!(resp.descriptors.len(), VINE_PULL_PAGE_LIMIT_MAX as usize);
        for (i, d) in resp.descriptors.iter().enumerate() {
            assert_eq!(
                d.as_slice(),
                format!("row-{i:04}").as_bytes(),
                "rows must be served in ascending order"
            );
        }
    }

    #[tokio::test]
    async fn content_refused_for_unlisted_cid() {
        let (mut client_read, mut client_write, mut server_read, mut server_write) =
            duplex_pair(1 << 16);

        let ctx = MockCtx::empty(); // nothing allowlisted
        let cfg = fast_cfg();

        let req = VinePullRequest::Content(VineContentRequest {
            cid_hex: "deadbeef".to_string(),
        });
        let req_bytes = encode_vine_pull_request(&req).expect("encode content request");
        write_len_prefixed(
            &mut client_write,
            &req_bytes,
            VINE_QUERY_MAX_FRAME_BYTES,
            Endian::Le,
            false,
        )
        .await
        .expect("write content request");
        client_write
            .shutdown()
            .await
            .expect("shutdown client_write");

        let result = run_vine_relay_session(&mut server_read, &mut server_write, &ctx, &cfg).await;
        assert_eq!(result.end, VineRelaySessionEnd::ClientEof);
        drop(server_write);

        let meta_bytes = read_len_prefixed(
            &mut client_read,
            VINE_CONTENT_MAX_FRAME_BYTES,
            Endian::Le,
            true,
        )
        .await
        .expect("read meta frame");
        let meta = decode_vine_content_meta(&meta_bytes).expect("decode meta");
        assert_eq!(meta, VineContentMeta { ok: false, size: 0 });
    }

    #[tokio::test]
    async fn content_streams_chunks_and_counts_budget() {
        // The duplex buffer must be large enough to hold everything the
        // session writes (meta + one 4 MiB chunk before the budget trips)
        // WITHOUT a concurrent reader draining it — the test reads only
        // after the session completes, so an undersized buffer would make
        // the write itself block on backpressure and time out.
        let (mut client_read, mut client_write, mut server_read, mut server_write) =
            duplex_pair(16 * 1024 * 1024);

        let nine_mib = 9 * 1024 * 1024;
        let ctx = MockCtx::empty();
        ctx.serve_cid("cafef00d", vec![7u8; nine_mib]);

        let cfg = VineRelaySessionConfig {
            io_deadline: Duration::from_millis(500),
            session_byte_budget: 8 * 1024 * 1024, // forced small budget
        };

        let req = VinePullRequest::Content(VineContentRequest {
            cid_hex: "cafef00d".to_string(),
        });
        let req_bytes = encode_vine_pull_request(&req).expect("encode content request");
        write_len_prefixed(
            &mut client_write,
            &req_bytes,
            VINE_QUERY_MAX_FRAME_BYTES,
            Endian::Le,
            false,
        )
        .await
        .expect("write content request");
        client_write
            .shutdown()
            .await
            .expect("shutdown client_write");

        let result = run_vine_relay_session(&mut server_read, &mut server_write, &ctx, &cfg).await;
        assert_eq!(result.end, VineRelaySessionEnd::BudgetExceeded);
        server_write
            .shutdown()
            .await
            .expect("shutdown server_write");

        let meta_bytes = read_len_prefixed(
            &mut client_read,
            VINE_CONTENT_MAX_FRAME_BYTES,
            Endian::Le,
            true,
        )
        .await
        .expect("read meta frame");
        let meta = decode_vine_content_meta(&meta_bytes).expect("decode meta");
        assert_eq!(
            meta,
            VineContentMeta {
                ok: true,
                size: nine_mib as u64
            }
        );

        let chunk1 = read_len_prefixed(
            &mut client_read,
            VINE_CONTENT_MAX_FRAME_BYTES,
            Endian::Le,
            true,
        )
        .await
        .expect("read first chunk");
        assert_eq!(chunk1.len(), VINE_CONTENT_CHUNK_BYTES);

        // The 8 MiB budget is exhausted by meta + one 4 MiB chunk before a
        // second 4 MiB chunk would fit — the session must close before the
        // 3rd (of 3) chunk ever arrives.
        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_millis(300), client_read.read(&mut buf))
            .await
            .expect("read must not hang")
            .expect("read must not error");
        assert_eq!(n, 0, "no further chunk after the budget breach");

        assert_eq!(
            result.bytes_served,
            (meta_bytes.len() + chunk1.len()) as u64
        );
    }

    #[tokio::test]
    async fn request_cap_ends_session_cleanly() {
        // An anonymous client whose queries barely advance the byte budget
        // (a non-served creator returns an almost-empty page every time,
        // exactly like `MockCtx::empty()` here) must not hold an admission
        // slot forever by renewing the idle deadline with cheap requests —
        // VINE_RELAY_MAX_REQUESTS_PER_SESSION bounds the total request
        // count independent of bytes served or idle timing.
        let (mut client_read, mut client_write, mut server_read, mut server_write) =
            duplex_pair(1 << 20);

        let ctx = MockCtx::empty(); // every page comes back empty
        let cfg = fast_cfg();

        let req = VinePullRequest::Query(VinePullQuery {
            creator_addr: "alice-addr".to_string(),
            after_created_at: 0,
            after_id: String::new(),
            limit: 10,
        });
        let req_bytes = encode_vine_pull_request(&req).expect("encode query");
        // One more request than the cap — proves the cap itself ends the
        // session, not the client simply running out of requests to send.
        for _ in 0..(VINE_RELAY_MAX_REQUESTS_PER_SESSION + 1) {
            write_len_prefixed(
                &mut client_write,
                &req_bytes,
                VINE_QUERY_MAX_FRAME_BYTES,
                Endian::Le,
                false,
            )
            .await
            .expect("write query frame");
        }
        client_write
            .shutdown()
            .await
            .expect("shutdown client_write");

        let result = run_vine_relay_session(&mut server_read, &mut server_write, &ctx, &cfg).await;
        assert_eq!(result.end, VineRelaySessionEnd::RequestCapExceeded);
        server_write
            .shutdown()
            .await
            .expect("shutdown server_write");

        // Exactly the cap's worth of responses were written — never the
        // client's extra (cap + 1)th request.
        let mut served = 0;
        loop {
            let read_res = tokio::time::timeout(
                Duration::from_millis(500),
                read_len_prefixed(
                    &mut client_read,
                    VINE_CONTENT_MAX_FRAME_BYTES,
                    Endian::Le,
                    true,
                ),
            )
            .await
            .expect("read must not hang");
            match read_res {
                Ok(_) => served += 1,
                Err(_) => break,
            }
        }
        assert_eq!(served, VINE_RELAY_MAX_REQUESTS_PER_SESSION);
    }

    #[test]
    fn admission_cap_rejects_ninth_session() {
        let admission = VineRelayAdmission::new(VINE_RELAY_MAX_CONCURRENT_SESSIONS);
        let mut permits = Vec::new();
        for _ in 0..VINE_RELAY_MAX_CONCURRENT_SESSIONS {
            permits.push(
                admission
                    .try_admit()
                    .expect("a permit must be available under the cap"),
            );
        }
        assert!(
            admission.try_admit().is_none(),
            "the 9th concurrent session must be rejected at the cap"
        );

        // Releasing one permit frees a slot.
        permits.pop();
        assert!(admission.try_admit().is_some());
    }

    // ── ZEB-811 final review: ProdVineRelayServeCtx share gate + own-creator scope ──

    /// Build a signature-retaining descriptor fixture — `sig` is a fake
    /// string, not a real signature; these tests exercise the SERVE-SIDE
    /// gate/scope filters, not signature verification (that's Task 5's own
    /// ingest tests).
    fn signed_descriptor(
        id: &str,
        creator: &str,
        video_cid: &str,
        created_at: u64,
    ) -> crate::VineDescriptorPayload {
        crate::VineDescriptorPayload {
            id: id.to_string(),
            creator_address: creator.to_string(),
            creator_name: "Name".to_string(),
            created_at,
            video_cid: video_cid.to_string(),
            title: None,
            reshare_of: None,
            original_creator_address: None,
            original_creator_name: None,
            identity_pub: Some("fake-identity-pub".to_string()),
            sig: Some("fake-sig".to_string()),
            device_sig: None,
        }
    }

    /// Mint a real `ContentId` for `bytes` (so `ContentId::verify_hash`
    /// actually passes) and its hex string.
    fn make_content(bytes: Vec<u8>) -> (String, Vec<u8>) {
        use harmony_content::cid::{ContentFlags, ContentId};
        let cid = ContentId::for_book(&bytes, ContentFlags::default()).expect("build content id");
        (hex::encode(cid.to_bytes()), bytes)
    }

    #[tokio::test]
    async fn share_gate_off_serves_nothing_even_for_own_creator() {
        let own = "own-creator-addr";
        let (video_cid_hex, video_bytes) = make_content(b"own creator's video bytes".to_vec());

        let mut cache = crate::vine_feed_cache::VineFeedCache::new();
        cache.seed_descriptor_for_test(signed_descriptor("v1", own, &video_cid_hex, 100));

        let content_store: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let cid_bytes = crate::parse_cid_hex(&video_cid_hex).expect("parse cid hex");
        content_store
            .put(
                crate::owner_state_types::ContentId::from_bytes(cid_bytes),
                video_bytes,
            )
            .await
            .expect("seed content store");

        let ctx = ProdVineRelayServeCtx {
            cache: Arc::new(std::sync::Mutex::new(cache)),
            content_store,
            own_creator_addr: own.to_string(),
            share_gate: Arc::new(std::sync::atomic::AtomicBool::new(false)), // OFF
        };

        let page = ctx.descriptors_page(own, &(0, String::new()), 10);
        assert!(
            page.is_empty(),
            "gate OFF must serve no descriptors, even for the own creator"
        );
        assert!(
            !ctx.video_cid_is_served(&video_cid_hex),
            "gate OFF must refuse content too"
        );
    }

    #[tokio::test]
    async fn serves_only_own_creator_not_a_followed_other() {
        let own = "own-creator-addr";
        let other = "followed-other-addr";
        let (own_cid_hex, own_bytes) = make_content(b"own creator's video".to_vec());
        let (other_cid_hex, other_bytes) = make_content(b"other creator's video".to_vec());

        let mut cache = crate::vine_feed_cache::VineFeedCache::new();
        cache.seed_descriptor_for_test(signed_descriptor("own-v1", own, &own_cid_hex, 100));
        cache.seed_descriptor_for_test(signed_descriptor("other-v1", other, &other_cid_hex, 100));

        let content_store: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        content_store
            .put(
                crate::owner_state_types::ContentId::from_bytes(
                    crate::parse_cid_hex(&own_cid_hex).expect("parse own cid hex"),
                ),
                own_bytes.clone(),
            )
            .await
            .expect("seed own content");
        content_store
            .put(
                crate::owner_state_types::ContentId::from_bytes(
                    crate::parse_cid_hex(&other_cid_hex).expect("parse other cid hex"),
                ),
                other_bytes,
            )
            .await
            .expect("seed other content");

        let ctx = ProdVineRelayServeCtx {
            cache: Arc::new(std::sync::Mutex::new(cache)),
            content_store,
            own_creator_addr: own.to_string(),
            share_gate: Arc::new(std::sync::atomic::AtomicBool::new(true)), // ON
        };

        // Own creator: served.
        let own_page = ctx.descriptors_page(own, &(0, String::new()), 10);
        assert_eq!(
            own_page.len(),
            1,
            "own creator's descriptors must be served"
        );
        assert!(
            ctx.video_cid_is_served(&own_cid_hex),
            "own creator's video cid must be allowlisted"
        );
        assert_eq!(ctx.video_bytes(&own_cid_hex).await, Some(own_bytes));

        // Other (followed) creator: refused — a v1 relay serves only its
        // own creator, and this is exactly what defeats the follow-graph
        // probe the final review flagged.
        let other_page = ctx.descriptors_page(other, &(0, String::new()), 10);
        assert!(
            other_page.is_empty(),
            "a followed OTHER creator's descriptors must not be served"
        );
        assert!(
            !ctx.video_cid_is_served(&other_cid_hex),
            "a followed OTHER creator's video cid must not be allowlisted"
        );
    }

    #[tokio::test]
    async fn video_bytes_refuses_oversize_cached_content() {
        // `ContentStore` has no size/stat probe, so the cap is enforced
        // AFTER `get_local`'s full read — this proves it's enforced at all,
        // even for content the node's own cache legitimately allowlists.
        // The cid here is a synthetic 32-byte identifier rather than one
        // minted via `make_content`'s `ContentId::for_book` (which itself
        // caps a payload at 1 MiB, well under the 100 MiB size this test
        // needs) — `verify_hash` is never reached because the size cap
        // returns `None` first, so the cid need not actually hash the seeded
        // bytes.
        let own = "own-creator-addr";
        let video_cid_hex = hex::encode([0xABu8; 32]);
        let oversize_len = (crate::VINE_VIDEO_MAX_BYTES + 1) as usize;
        let video_bytes = vec![9u8; oversize_len];

        let mut cache = crate::vine_feed_cache::VineFeedCache::new();
        cache.seed_descriptor_for_test(signed_descriptor("v1", own, &video_cid_hex, 100));

        let content_store: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let cid_bytes = crate::parse_cid_hex(&video_cid_hex).expect("parse cid hex");
        content_store
            .put(
                crate::owner_state_types::ContentId::from_bytes(cid_bytes),
                video_bytes,
            )
            .await
            .expect("seed content store");

        let ctx = ProdVineRelayServeCtx {
            cache: Arc::new(std::sync::Mutex::new(cache)),
            content_store,
            own_creator_addr: own.to_string(),
            share_gate: Arc::new(std::sync::atomic::AtomicBool::new(true)), // ON
        };

        assert!(
            ctx.video_cid_is_served(&video_cid_hex),
            "the cid is legitimately allowlisted via the served descriptor"
        );
        assert!(
            ctx.video_bytes(&video_cid_hex).await.is_none(),
            "content over VINE_VIDEO_MAX_BYTES must never be served, even locally cached"
        );
    }
}
