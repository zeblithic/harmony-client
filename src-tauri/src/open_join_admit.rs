//! Beacon-side verification + admission for tokenless open-community join.
//!
//! `verify_and_admit_open_join` is the security core of the open-join handshake:
//! a beacon (which holds the community `epoch_key`) calls it to decide whether
//! to admit a tokenless [`OpenJoinRequest`]. It is **pure/synchronous** — no
//! I/O, no engine lock — so it is fully unit-testable. The caller (the iroh
//! accept dispatcher) supplies `current_events` (the beacon engine's signed
//! membership log) and, on success, applies the admitted Join to its engine.
//!
//! Check order (cheap structural → capability → identity → stateful):
//!   1. Community scope (`req.community_id == community_id`).
//!   2. Freshness window (bounded skew on `created_at.wall_ms`).
//!   3. Capability proof (`epoch_auth` recomputed under `epoch_key`).
//!   4. Device-hash binding (`signing_device_hash == H(joiner_identity_pub)`).
//!   5. Joiner identity control (enrollment cert → enrolled device key).
//!   6. Packet-envelope signature (`verify_strict` over the exact signed bytes).
//!   7. Nonce-replay + per-window rate limit.
//!   8. Ban-check (materialized state strictly before the Join HLC).
//!   9. Admit via the shipping `bootstrap_admit_open_publisher` gate.
//!
//! ZEB-846 Task 7 adds one more bound, checked immediately after step 2:
//! `join_event.at.wall_ms` — the Join's OWN wall — must also be within
//! `clock_trust::MAX_FORWARD_SKEW_MS` of the beacon's wall clock. This is
//! separate from `created_at` above (which only bounds the request envelope)
//! because the Join event is what actually lands in the persisted log.

use crate::community_invite::{device_hash_from_identity_pub, OpenJoinRequest};
use crate::community_membership::{
    bootstrap_admit_open_publisher, enrolled_key_from_cert, prior_state_at_hlc, MemberStatus,
    SignedMembershipEvent,
};
use crate::friend_intro::KeyedSlidingWindow;
use crate::open_join_auth::verify_epoch_auth;
use crate::owner_state_types::{EpochKey, OwnerAddr, SpaceId};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Max admissions accepted per rolling window before excess is shed.
pub const OPEN_JOIN_RATE_LIMIT_PER_WINDOW: usize = 20;
/// Rolling rate-limit window, in milliseconds.
pub const OPEN_JOIN_RATE_LIMIT_WINDOW_MS: u64 = 60_000;

/// ZEB-865: node-wide aggregate admissions accepted per
/// [`OPEN_JOIN_RATE_LIMIT_WINDOW_MS`] before excess is shed as
/// [`OpenJoinReject::NodeCapacity`]. 1024 = 51× the per-source budget: far above
/// any realistic single-beacon honest burst (joiners also spread across the
/// butler set and retry on shed), while cutting the uncapped Sybil worst case
/// (`MAX_WINDOW_KEYS × OPEN_JOIN_RATE_LIMIT_PER_WINDOW` ≈ 163,840/60 s) ~160×.
/// Defense-in-depth atop the per-source admission budget and the Tier-1
/// connection shield, so it is sized to favor never locking out honest load.
pub const OPEN_JOIN_GLOBAL_ADMIT_MAX: usize = 1024;

/// ZEB-853 (B7): pre-auth Tier-1 per-connection-endpoint cap over
/// [`OPEN_JOIN_CONN_WINDOW_MS`]. Generous — one iroh endpoint may legitimately
/// retry a join (dial → resolve → re-dial) — but a genuine single-endpoint
/// flood is still shed BEFORE any packet read or pre-consent crypto. Mirrors
/// `friend_intro::FRIEND_HANDSHAKE_PER_CONNECTION_MAX`.
pub const OPEN_JOIN_CONN_MAX: usize = 40;
/// ZEB-853 (B7): sliding window for the Tier-1 connection shield (1h, matching
/// the friend/v1 shield). Distinct from the per-source ADMISSION budget
/// ([`OPEN_JOIN_RATE_LIMIT_WINDOW_MS`], 60s) — the shield paces raw connection
/// attempts before decode, the budget paces successful admissions after crypto.
pub const OPEN_JOIN_CONN_WINDOW_MS: u64 = 60 * 60 * 1000;

/// How old an [`OpenJoinRequest`]'s `created_at` may be (relative to the
/// beacon's wall clock) before it is rejected as `Stale`. Distinct from the
/// rate-limit window: the freshness window bounds the joiner's signed
/// capability/timestamp validity, while the rate-limit window paces admissions.
/// 2 minutes accommodates cross-WAN dial/resolve latency and modest clock skew
/// while keeping the replayable/forge-resistant window short.
pub const OPEN_JOIN_FRESHNESS_WINDOW_MS: u64 = 120_000;

/// Rejection reasons for [`verify_and_admit_open_join`]. Distinct variants so a
/// caller can pick a wire status / log line per failure class without parsing
/// strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenJoinReject {
    /// `epoch_auth` did not recompute under `epoch_key` (no/forged capability).
    BadCapability,
    /// `signing_device_hash` ≠ `H(joiner_identity_pub)` (request self-contradicts).
    DeviceHashMismatch,
    /// The packet envelope signature failed `verify_strict` against the joiner's
    /// enrolled device key.
    BadJoinerSig,
    /// The Join's enrollment cert is missing/invalid (no proof of identity control).
    BadEnrollment,
    /// `created_at` is outside the bounded freshness window (too old or too far future).
    Stale,
    /// `join_event.at.wall_ms` — the Join's OWN wall, distinct from the
    /// envelope's `created_at` — is more than `clock_trust::MAX_FORWARD_SKEW_MS`
    /// ahead of the beacon's wall clock. This is the timestamp that actually
    /// lands in the persisted membership log, so it gets its own bound
    /// (ZEB-846 Task 7 — closes the gap left by Task 3's zenoh-merge-only
    /// `community_membership::verify_event` forward-skew reject).
    JoinEventFutureSkew,
    /// The request nonce was already seen within the replay-cache horizon.
    Replay,
    /// The joiner owner is `Banned` at the Join HLC.
    Banned,
    /// Per-window admission cap exceeded.
    RateLimited,
    /// Node-wide aggregate admission ceiling exceeded (ZEB-865). Distinct from
    /// `RateLimited` (per-source): the source is within its own budget but the
    /// node is at aggregate capacity. Same benign typed-rejection wire behavior
    /// as the other post-decode rejects.
    NodeCapacity,
    /// Wrong community, or `bootstrap_admit_open_publisher` declined (not Joined).
    NotAdmittable,
}

/// Successful admission: the joiner's owner address plus the event snapshot
/// (the beacon's current log with the joiner's Join appended) the caller serves
/// back and applies to its engine.
#[derive(Debug)]
pub struct OpenJoinAdmitOk {
    pub joiner_addr: OwnerAddr,
    pub member_events_snapshot: Vec<SignedMembershipEvent>,
}

/// Per-source admission limiter + nonce-replay cache. The admission budget is
/// keyed PER-SOURCE (ZEB-853, keyed on the connecting `remote_id`) so one
/// flooding source can't exhaust the shared budget; the nonce cache rejects
/// exact-replay within the retention horizon and is bounded by eviction.
pub struct OpenJoinRateLimiter {
    /// ZEB-853 (B7, Half 2): per-source admission windows, keyed on the
    /// connecting `remote_id`. This was a single global `window_start_ms` /
    /// `count_in_window` counter — one source could exhaust the whole 20/60s
    /// budget and lock out every legitimate open-joiner. Each source now gets
    /// its OWN 20/60s window, and the map is bounded against rotating-key floods
    /// by [`KeyedSlidingWindow`]'s `MAX_WINDOW_KEYS` eviction (the same audited
    /// primitive the friend/pex/intro shields use).
    windows: KeyedSlidingWindow<[u8; 32]>,
    /// ZEB-865: node-wide aggregate admission ceiling, checked in ADDITION to
    /// the per-source `windows`. A single unit-key reuse of the audited
    /// sliding-window primitive — exactly one global 60 s window (the
    /// `MAX_WINDOW_KEYS` eviction is a no-op at one key). The per-source budget
    /// alone can't bound a Sybil fan-out (each fake source gets its own window);
    /// this aggregate gate caps the sum.
    global: KeyedSlidingWindow<()>,
    seen_nonces: HashSet<[u8; 16]>,
    nonce_seen_at: HashMap<[u8; 16], u64>,
    /// ZEB-711: monotonic epoch for the production limiter timeline. `allow` /
    /// `is_replay` / `record_nonce` keep taking an explicit `limiter_now_ms`
    /// (the unit-test seam), but the acceptor derives it from
    /// [`Self::monotonic_now_ms`] instead of the beacon wall clock — a wall step
    /// would otherwise distort enforcement (forward: a flood gets a fresh
    /// budget; backward: an honest shed peer stays shed longer, and the nonce
    /// horizon is corrupted). `tokio::time::Instant` also honors the paused test
    /// clock. Wall time stays for the freshness arm only (`created_at.wall_ms`).
    epoch: tokio::time::Instant,
}

impl OpenJoinRateLimiter {
    /// Fresh limiter with its monotonic epoch anchored now. (`tokio::time::Instant`
    /// has no `Default`, so this replaces a derived `Default`.)
    /// Fresh limiter with production caps, its monotonic epoch anchored now.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::with_caps(OPEN_JOIN_RATE_LIMIT_PER_WINDOW, OPEN_JOIN_GLOBAL_ADMIT_MAX)
    }

    /// Test/tuning constructor — deterministic tiny CAPS for the per-source and
    /// aggregate windows. The window itself is NOT a parameter: both windows AND
    /// the nonce-replay horizon in `is_replay` (defined as 4× the window) share
    /// the single protocol constant [`OPEN_JOIN_RATE_LIMIT_WINDOW_MS`]. Exposing
    /// `window_ms` here would let a caller desync replay retention from the
    /// admission window (Qodo, PR #596) — so only the caps vary.
    pub fn with_caps(per_source_max: usize, global_max: usize) -> Self {
        Self {
            windows: KeyedSlidingWindow::new(per_source_max, OPEN_JOIN_RATE_LIMIT_WINDOW_MS),
            global: KeyedSlidingWindow::new(global_max, OPEN_JOIN_RATE_LIMIT_WINDOW_MS),
            seen_nonces: HashSet::new(),
            nonce_seen_at: HashMap::new(),
            epoch: tokio::time::Instant::now(),
        }
    }

    /// ZEB-711: the production timeline for admits on this limiter —
    /// milliseconds since it was constructed, from the monotonic (and
    /// test-pausable) tokio clock. Window state and epoch live and die with the
    /// limiter instance, so the timeline is internally consistent by construction.
    pub fn monotonic_now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Returns true if a request from `source` is allowed within THAT source's
    /// own rolling window; records the admission when allowed. ZEB-853 (B7):
    /// keyed per-source (the connecting `remote_id`) so one flooding source
    /// can't spend another's budget. [`KeyedSlidingWindow::admit`] timestamps
    /// actual requests, so an idle-before-first-request acceptor gets its full
    /// first window with no separate anchor bookkeeping (the wall-vs-monotonic
    /// and idle-anchor guarantees are pinned by the tests below).
    fn allow(&mut self, source: [u8; 32], limiter_now_ms: u64) -> bool {
        self.windows.admit(source, limiter_now_ms)
    }

    /// Returns true if `nonce` was already seen within the retention horizon.
    /// Evicts entries older than the horizon to bound memory. Does NOT record
    /// the nonce — recording is split into [`Self::record_nonce`] so a request
    /// that is shed by the rate limiter (which is checked AFTER replay) does not
    /// leave its nonce persisted, which would wrongly reject a legitimate retry
    /// as a replay.
    fn is_replay(&mut self, nonce: &[u8; 16], limiter_now_ms: u64) -> bool {
        let horizon =
            limiter_now_ms.saturating_sub(OPEN_JOIN_RATE_LIMIT_WINDOW_MS.saturating_mul(4));
        let seen = &mut self.seen_nonces;
        self.nonce_seen_at.retain(|n, t| {
            let keep = *t >= horizon;
            if !keep {
                seen.remove(n);
            }
            keep
        });
        self.seen_nonces.contains(nonce)
    }

    /// Record `nonce` as seen at `now_ms`. Called only AFTER a request has
    /// passed both the replay check and the rate-limit check, so a `RateLimited`
    /// rejection never persists a nonce (a later legitimate retry must not be
    /// rejected as a replay).
    fn record_nonce(&mut self, nonce: &[u8; 16], limiter_now_ms: u64) {
        self.seen_nonces.insert(*nonce);
        self.nonce_seen_at.insert(*nonce, limiter_now_ms);
    }

    /// ZEB-865: node-wide aggregate capacity peek (no record). Composed BEFORE
    /// the per-source `allow` so a ceiling shed charges neither the source's
    /// budget nor its nonce.
    fn global_has_capacity(&self, limiter_now_ms: u64) -> bool {
        self.global.would_admit((), limiter_now_ms)
    }

    /// ZEB-865: commit one aggregate token. Called ONLY after `allow` admits, so
    /// a per-source shed never drains the ceiling (which would let one spammer
    /// re-create single-source lockout).
    fn record_global(&mut self, limiter_now_ms: u64) {
        self.global.admit((), limiter_now_ms);
    }

    /// ZEB-865: the whole rate-limit decision for one open-join request — replay,
    /// then the node-wide aggregate ceiling, then the per-source budget, then the
    /// nonce record — in the one order that keeps a ceiling shed from charging the
    /// source's budget or nonce, and keeps a per-source shed from draining the
    /// aggregate ceiling. `verify_and_admit_open_join` and the unit tests share
    /// this one ordering.
    fn admit_source(
        &mut self,
        source: [u8; 32],
        nonce: &[u8; 16],
        limiter_now_ms: u64,
    ) -> Result<(), OpenJoinReject> {
        if self.is_replay(nonce, limiter_now_ms) {
            return Err(OpenJoinReject::Replay);
        }
        if !self.global_has_capacity(limiter_now_ms) {
            return Err(OpenJoinReject::NodeCapacity);
        }
        if !self.allow(source, limiter_now_ms) {
            return Err(OpenJoinReject::RateLimited);
        }
        self.record_global(limiter_now_ms);
        self.record_nonce(nonce, limiter_now_ms);
        Ok(())
    }
}

/// ZEB-853 (B7): pre-auth Tier-1 connection shield for the open-join / invite
/// handshake ALPN — the ZEB-700 `FriendRateLimiter` posture applied to the
/// `IrohInviteHandshakeAcceptor`. Keyed on the connecting endpoint's
/// authenticated `remote_id` (un-spoofable), it sheds a flooding source BEFORE
/// the acceptor reads or decodes the packet and BEFORE the two ed25519
/// verifications inside [`verify_and_admit_open_join`] — bounding the unbounded
/// pre-consent crypto anyone holding the public open-invite link could otherwise
/// force by opening connections. Its budget is disjoint from the friend / pex /
/// intro shields (its own instance).
///
/// Deliberately NOT an authorization decision — it only sheds volume; the
/// acceptor LOGS every shed and answers with the SAME benign no-response close
/// its existing benign outcomes (countersign-timeout, community-not-found)
/// produce, so a shed is network-indistinguishable (no oracle). Since the gate
/// runs pre-decode (the packet type — invite `0x10` vs open-join `0x11` — is not
/// yet known) there is no single typed reply that fits both arms; a silent
/// close IS the benign-equivalent here (unlike friend/pex, whose single-arm
/// protocols always reply). `admit_connection` never `.await`s, so the
/// `std::sync::Mutex` is never held across a suspension point, and the tracked
/// map is bounded by [`KeyedSlidingWindow`]'s `MAX_WINDOW_KEYS` eviction so a
/// rotating-`remote_id` flood can't turn this shield into a memory-DoS of its
/// own.
pub struct OpenJoinConnLimiter {
    conn: Mutex<KeyedSlidingWindow<[u8; 32]>>,
    /// ZEB-711: monotonic epoch — the admit timeline is the limiter's OWN
    /// monotonic clock, never wall time (a wall step would distort the window:
    /// forward → a flood gets a fresh budget; backward → an honest shed peer
    /// stays shed). `tokio::time::Instant` also honors the paused test clock.
    epoch: tokio::time::Instant,
}

impl OpenJoinConnLimiter {
    /// Production shield with the default caps ([`OPEN_JOIN_CONN_MAX`] over
    /// [`OPEN_JOIN_CONN_WINDOW_MS`]).
    pub fn new() -> Self {
        Self::with_caps(OPEN_JOIN_CONN_MAX, OPEN_JOIN_CONN_WINDOW_MS)
    }

    /// Test/tuning constructor — deterministic tiny caps in unit tests.
    pub fn with_caps(conn_max: usize, window_ms: u64) -> Self {
        Self {
            conn: Mutex::new(KeyedSlidingWindow::new(conn_max, window_ms)),
            epoch: tokio::time::Instant::now(),
        }
    }

    /// ZEB-711: production timeline for this limiter's admit calls — ms since
    /// construction, from the monotonic (and test-pausable) tokio clock.
    pub fn monotonic_now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Poison-tolerant lock: a panic elsewhere must not wedge the acceptor — the
    /// guarded state is plain counters, safe to keep using.
    fn lock(&self) -> std::sync::MutexGuard<'_, KeyedSlidingWindow<[u8; 32]>> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Tier 1 — pre-auth. Key = the connecting endpoint's authenticated
    /// `remote_id`. `Ok(())` admits (and records); `Err` sheds without
    /// recording. Runs BEFORE any read / decode / signature verification.
    pub fn admit_connection(&self, remote_id: [u8; 32], now_ms: u64) -> Result<(), &'static str> {
        if self.lock().admit(remote_id, now_ms) {
            Ok(())
        } else {
            Err("per-connection cap")
        }
    }
}

impl Default for OpenJoinConnLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Verify a tokenless open-join request and, if valid, admit the joiner.
///
/// `signed_bytes` MUST be the exact bytes the joiner signed (= the
/// `signed_bytes` captured by `decode_packet` for the `OpenJoin` variant, which
/// is `canonical_cbor_encode(&req)`); we verify `packet_sig` against THESE bytes
/// rather than re-encoding `req` here, so encoder drift can never desync the
/// verify preimage from the mint preimage.
///
/// `wall_now_ms` is the beacon's wall clock (bounds `created_at` freshness);
/// `limiter_now_ms` is the limiter's OWN monotonic clock (rate-limit window +
/// nonce horizon, ZEB-711 — never the wall clock, so a wall step cannot reset
/// the rate limit); `freshness_window_ms` bounds how old `created_at` may be.
/// `source_id` is the connecting endpoint's un-spoofable transport `remote_id`
/// (ZEB-853, B7) — the admission budget is keyed on it so one flooding source
/// can't exhaust another's window. `current_events` is the beacon engine's
/// signed membership log (assumed already verified — `prior_state_at_hlc` /
/// `materialize` do not re-verify signatures).
#[allow(clippy::too_many_arguments)]
pub fn verify_and_admit_open_join(
    req: &OpenJoinRequest,
    packet_sig: &[u8; 64],
    signed_bytes: &[u8],
    epoch_key: &EpochKey,
    community_id: SpaceId,
    admin_addr: OwnerAddr,
    current_events: &[SignedMembershipEvent],
    wall_now_ms: u64,
    freshness_window_ms: u64,
    limiter_now_ms: u64,
    source_id: [u8; 32],
    limiter: &mut OpenJoinRateLimiter,
) -> Result<OpenJoinAdmitOk, OpenJoinReject> {
    // 1. Community scope.
    if req.community_id != community_id {
        return Err(OpenJoinReject::NotAdmittable);
    }

    // 2. Freshness (bounded window; reject future-dated beyond a small skew).
    let created = req.created_at.wall_ms;
    if wall_now_ms.saturating_sub(created) > freshness_window_ms
        || created > wall_now_ms.saturating_add(60_000)
    {
        return Err(OpenJoinReject::Stale);
    }

    // 2b. ZEB-846 (Task 7): bound the join_event's OWN wall — separate from
    //     `created_at` above, which only bounds the request envelope. This
    //     event is what `bootstrap_admit_open_publisher` (step 9) appends to
    //     the persisted membership log, so an attacker who mints a fresh
    //     envelope around a far-future-walled Join must be rejected here,
    //     not just at the zenoh-merge path's `verify_event` (Task 3).
    if crate::clock_trust::reject_future(
        req.join_event.at.wall_ms,
        wall_now_ms,
        crate::clock_trust::MAX_FORWARD_SKEW_MS,
    ) {
        return Err(OpenJoinReject::JoinEventFutureSkew);
    }

    // 3. Capability proof. `timestamp_ms` is `created_at.wall_ms` — the joiner
    //    mints `epoch_auth` over the same value (Task 10 must match this).
    if !verify_epoch_auth(
        epoch_key,
        &community_id,
        &req.joiner_identity_pub,
        &req.nonce,
        created,
        &req.epoch_auth,
    ) {
        return Err(OpenJoinReject::BadCapability);
    }

    // 4. Device-hash binding. `decode_packet` does NOT enforce this for 0x11, so
    //    reject a request whose advertised device hash doesn't derive from its
    //    own identity pub.
    if req.signing_device_hash.0 != device_hash_from_identity_pub(&req.joiner_identity_pub) {
        return Err(OpenJoinReject::DeviceHashMismatch);
    }

    // 5. Joiner identity control: the enrollment cert binds the join_event's
    //    device key to the joiner owner, verified the same way the merge path
    //    does (cert.verify + Master issuer + owner == actor).
    let enrolled =
        enrolled_key_from_cert(&req.join_event).map_err(|_| OpenJoinReject::BadEnrollment)?;

    // 6. Packet envelope signature: signed by the joiner's enrolled device key
    //    over the EXACT `signed_bytes` (mirrors verify_publisher_sig's
    //    verify_strict posture). We do NOT re-encode `req` here.
    {
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&enrolled.device_ed25519)
            .map_err(|_| OpenJoinReject::BadJoinerSig)?;
        let sig = ed25519_dalek::Signature::from_bytes(packet_sig);
        vk.verify_strict(signed_bytes, &sig)
            .map_err(|_| OpenJoinReject::BadJoinerSig)?;
    }

    // 7. Replay + per-source budget + node-wide aggregate ceiling + nonce record,
    //    all in one ordering owned by `admit_source` (ZEB-865). Runs after the
    //    cheap structural + crypto checks, before the stateful materialization —
    //    so the aggregate ceiling (which only fully-verified requests can reach)
    //    sheds the dominant materialization cost of a Sybil fan-out. A ceiling
    //    shed charges neither the source's budget nor its nonce (cleanly
    //    retryable), and a per-source shed never drains the aggregate ceiling.
    limiter.admit_source(source_id, &req.nonce, limiter_now_ms)?;

    // 8. Ban-check against the materialized state strictly before the joiner's
    //    Join HLC. A Banned owner is rejected even if their fresh Join would
    //    otherwise admit.
    let mat = prior_state_at_hlc(current_events, &req.join_event.at, admin_addr);
    if let Some(ms) = mat.members.get(&enrolled.owner) {
        if ms.status == MemberStatus::Banned {
            return Err(OpenJoinReject::Banned);
        }
    }

    // 9. Admit via the shipping open-admission gate: feed the request's own Join
    //    alongside the current log; bootstrap_admit_open_publisher materializes
    //    the publisher's prefix STRICTLY BEFORE the root HLC and confirms they
    //    are Joined. The production sync path's root HLC (`payload.at`) is always
    //    strictly after the publisher's self-Join; here the Join *is* the only
    //    publisher event, so we derive a root HLC one logical tick past it so the
    //    Join falls inside the pre-root window. (The ban-check above deliberately
    //    uses `req.join_event.at` itself, not this bumped root.)
    let admission_root = crate::owner_state_types::Hlc {
        wall_ms: req.join_event.at.wall_ms,
        logical: req.join_event.at.logical.saturating_add(1),
        device_id: req.join_event.at.device_id.clone(),
    };
    let mut events_with_join = current_events.to_vec();
    events_with_join.push(req.join_event.clone());
    bootstrap_admit_open_publisher(
        &events_with_join,
        enrolled.owner,
        admin_addr,
        community_id,
        &admission_root,
    )
    .ok_or(OpenJoinReject::NotAdmittable)?;

    Ok(OpenJoinAdmitOk {
        joiner_addr: enrolled.owner,
        member_events_snapshot: events_with_join,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_invite::OpenJoinRequest;
    use crate::community_membership::{
        mint_test_owner, sign_event, EventPayload, MembershipEventKind, SignedMembershipEvent,
        TestOwner,
    };
    use crate::open_join_auth::mint_epoch_auth;
    use crate::owner_state_types::{DeviceIdentityHash, Hlc};

    const COMMUNITY: SpaceId = SpaceId([0x42; 16]);
    const FRESHNESS: u64 = 60_000;
    /// ZEB-853: a single fixed source key for the tests that exercise the
    /// per-request checks or the single-source rate window — their behavior is
    /// unchanged by the per-source keying because every call uses the SAME
    /// source. The cross-source isolation is proven by
    /// `open_join_rate_limit_is_per_source` / `open_join_tier1_sheds_one_source_not_others`.
    const TEST_SOURCE: [u8; 32] = [0u8; 32];

    /// A self-contained, real-mint open-join fixture: a community admin, a
    /// joiner with a valid Master EnrollmentCert and a self-signed open Join,
    /// and a faithfully built `OpenJoinRequest` (real `epoch_auth`, real packet
    /// signature) the beacon would receive.
    struct Fixture {
        epoch_key: EpochKey,
        community_id: SpaceId,
        admin_addr: OwnerAddr,
        joiner: TestOwner,
        joiner_identity_pub: [u8; 64],
        joiner_addr: OwnerAddr,
        current_events: Vec<SignedMembershipEvent>,
        now_ms: u64,
        /// Monotonic nonce source so `fresh_request` yields unique nonces.
        next_nonce: std::cell::Cell<u8>,
    }

    impl Fixture {
        fn build(banned: bool) -> Self {
            let epoch_key = EpochKey::new([0x5e; 32]);
            let admin = mint_test_owner(0x21);
            let joiner = mint_test_owner(0x22);

            // 64-byte identity pub for the joiner (any value the request is
            // internally consistent about; epoch_auth + device-hash both bind to
            // exactly these bytes). Derive deterministically from the seed.
            let mut joiner_identity_pub = [0u8; 64];
            joiner_identity_pub[..32]
                .copy_from_slice(&joiner.device_key.verifying_key().to_bytes());
            joiner_identity_pub[32..].copy_from_slice(&[0x22u8; 32]);

            // Admin self-Join (gives the admin a real Joined member entry; the
            // bootstrap rule also grants admin_addr power 100). Carries the
            // admin's EnrollmentCert so its signer resolves.
            let admin_join = {
                let p = EventPayload {
                    id: [0x01; 16],
                    community_id: COMMUNITY,
                    kind: MembershipEventKind::Join,
                    actor: admin.owner,
                    at: Hlc {
                        wall_ms: 10,
                        logical: 0,
                        device_id: "admin".into(),
                    },
                };
                let e = sign_event(&p, &admin.device_key).unwrap();
                SignedMembershipEvent {
                    enrollment: Some(admin.cert.clone()),
                    ..e
                }
            };

            let mut current_events = vec![admin_join];

            if banned {
                // The joiner first Joins early, then the admin Kicks them
                // (→ Banned), both strictly before the fresh re-Join HLC the
                // request carries. materialize() only bans an existing member,
                // so the prior Join is required.
                let prior_join = {
                    let p = EventPayload {
                        id: [0x02; 16],
                        community_id: COMMUNITY,
                        kind: MembershipEventKind::Join,
                        actor: joiner.owner,
                        at: Hlc {
                            wall_ms: 20,
                            logical: 0,
                            device_id: "joiner".into(),
                        },
                    };
                    let e = sign_event(&p, &joiner.device_key).unwrap();
                    SignedMembershipEvent {
                        enrollment: Some(joiner.cert.clone()),
                        ..e
                    }
                };
                let kick = {
                    let p = EventPayload {
                        id: [0x03; 16],
                        community_id: COMMUNITY,
                        kind: MembershipEventKind::Kick {
                            target: joiner.owner,
                            reason: None,
                        },
                        actor: admin.owner,
                        at: Hlc {
                            wall_ms: 30,
                            logical: 0,
                            device_id: "admin".into(),
                        },
                    };
                    sign_event(&p, &admin.device_key).unwrap()
                };
                current_events.push(prior_join);
                current_events.push(kick);
            }

            Fixture {
                epoch_key,
                community_id: COMMUNITY,
                admin_addr: admin.owner,
                joiner_addr: joiner.owner,
                joiner_identity_pub,
                joiner,
                current_events,
                // Far enough past the request's created_at (1000) to be in-window.
                now_ms: 5_000,
                next_nonce: std::cell::Cell::new(0),
            }
        }

        fn new() -> Self {
            Self::build(false)
        }

        fn with_banned_joiner() -> Self {
            Self::build(true)
        }

        /// The joiner's self-signed open Join the request carries, at the
        /// given `at.wall_ms` (a fresh HLC after any prior ban events).
        /// Parameterized so the ZEB-846 join-event forward-skew bound can be
        /// exercised independently of `created_at` (which stays fixed at 1000
        /// in `request_with_nonce_and_join_wall_ms`).
        fn join_event_at(&self, wall_ms: u64) -> SignedMembershipEvent {
            let p = EventPayload {
                id: [0x10; 16],
                community_id: self.community_id,
                kind: MembershipEventKind::Join,
                actor: self.joiner.owner,
                at: Hlc {
                    wall_ms,
                    logical: 0,
                    device_id: "joiner".into(),
                },
            };
            let e = sign_event(&p, &self.joiner.device_key).unwrap();
            SignedMembershipEvent {
                enrollment: Some(self.joiner.cert.clone()),
                ..e
            }
        }

        /// Build a fully-signed request with the given nonce: real `epoch_auth`
        /// over (community, identity_pub, nonce, created_at.wall_ms) and a real
        /// packet signature over canonical CBOR of the request.
        fn request_with_nonce(&self, nonce: [u8; 16]) -> (OpenJoinRequest, [u8; 64], Vec<u8>) {
            self.request_with_nonce_and_join_wall_ms(nonce, 1000)
        }

        /// Like `request_with_nonce`, but lets the caller set the inner
        /// `join_event`'s own `at.wall_ms` (`created_at` stays fixed at 1000,
        /// so it remains fresh regardless) — used to test the ZEB-846
        /// join-event forward-skew bound in isolation from envelope freshness.
        fn request_with_nonce_and_join_wall_ms(
            &self,
            nonce: [u8; 16],
            join_wall_ms: u64,
        ) -> (OpenJoinRequest, [u8; 64], Vec<u8>) {
            let created_at = Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "joiner".into(),
            };
            let epoch_auth = mint_epoch_auth(
                &self.epoch_key,
                &self.community_id,
                &self.joiner_identity_pub,
                &nonce,
                created_at.wall_ms,
            );
            let req = OpenJoinRequest {
                community_id: self.community_id,
                join_event: self.join_event_at(join_wall_ms),
                joiner_identity_pub: self.joiner_identity_pub,
                signing_device_hash: DeviceIdentityHash(device_hash_from_identity_pub(
                    &self.joiner_identity_pub,
                )),
                epoch_auth,
                nonce,
                created_at,
            };
            // Sign with the joiner's ENROLLED DEVICE key — the same key
            // enrolled_key_from_cert resolves — so verify_strict passes.
            let packet = crate::community_invite::build_signed_open_join_packet(
                req.clone(),
                &self.joiner.device_key,
            )
            .expect("build packet");
            match packet {
                crate::community_invite::CommunityInvitePacket::OpenJoin {
                    signature,
                    signed_bytes,
                    ..
                } => (req, signature, signed_bytes),
                other => panic!("expected OpenJoin, got {other:?}"),
            }
        }

        fn valid_request(&self) -> (OpenJoinRequest, [u8; 64], Vec<u8>) {
            self.request_with_nonce([0x07; 16])
        }

        /// A request with a unique nonce each call (for rate-limit testing).
        fn fresh_request(&self) -> (OpenJoinRequest, [u8; 64], Vec<u8>) {
            let n = self.next_nonce.get();
            self.next_nonce.set(n.wrapping_add(1));
            let mut nonce = [0u8; 16];
            nonce[0] = n;
            nonce[1] = 0xAA; // disjoint from valid_request's [0x07;16]
            self.request_with_nonce(nonce)
        }
    }

    #[test]
    fn admits_a_valid_open_join() {
        let f = Fixture::new();
        let (req, sig, signed_bytes) = f.valid_request();
        let mut lim = OpenJoinRateLimiter::new();
        let ok = verify_and_admit_open_join(
            &req,
            &sig,
            &signed_bytes,
            &f.epoch_key,
            f.community_id,
            f.admin_addr,
            &f.current_events,
            f.now_ms,
            FRESHNESS,
            f.now_ms,
            TEST_SOURCE,
            &mut lim,
        )
        .expect("valid open join should be admitted");
        assert_eq!(ok.joiner_addr, f.joiner_addr);
        assert!(
            ok.member_events_snapshot.len() == f.current_events.len() + 1,
            "snapshot must include the joiner's Join"
        );
    }

    #[test]
    fn rejects_wrong_capability() {
        let f = Fixture::new();
        let (mut req, sig, signed_bytes) = f.valid_request();
        req.epoch_auth = [0u8; 32]; // not a valid MAC
        let mut lim = OpenJoinRateLimiter::new();
        assert_eq!(
            verify_and_admit_open_join(
                &req,
                &sig,
                &signed_bytes,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                f.now_ms,
                TEST_SOURCE,
                &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::BadCapability
        );
    }

    #[test]
    fn rejects_device_hash_mismatch() {
        let f = Fixture::new();
        let (mut req, sig, signed_bytes) = f.valid_request();
        // Corrupt the advertised device hash so it no longer derives from the
        // identity pub. epoch_auth still validates (it doesn't cover the hash),
        // so this exercises the dedicated DeviceHashMismatch reject.
        req.signing_device_hash = DeviceIdentityHash([0xFF; 16]);
        let mut lim = OpenJoinRateLimiter::new();
        assert_eq!(
            verify_and_admit_open_join(
                &req,
                &sig,
                &signed_bytes,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                f.now_ms,
                TEST_SOURCE,
                &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::DeviceHashMismatch
        );
    }

    #[test]
    fn rejects_banned_identity() {
        let f = Fixture::with_banned_joiner();
        let (req, sig, signed_bytes) = f.valid_request();
        let mut lim = OpenJoinRateLimiter::new();
        assert_eq!(
            verify_and_admit_open_join(
                &req,
                &sig,
                &signed_bytes,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                f.now_ms,
                TEST_SOURCE,
                &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::Banned
        );
    }

    #[test]
    fn rejects_stale_timestamp() {
        let f = Fixture::new();
        let (req, sig, signed_bytes) = f.valid_request();
        let mut lim = OpenJoinRateLimiter::new();
        // now far beyond created_at + window.
        assert_eq!(
            verify_and_admit_open_join(
                &req,
                &sig,
                &signed_bytes,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms + 10_000_000,
                FRESHNESS,
                f.now_ms + 10_000_000,
                TEST_SOURCE,
                &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::Stale
        );
    }

    /// ZEB-846 Task 7: `req.join_event.at.wall_ms` — the Join's OWN wall —
    /// must be bounded even when `created_at` (the envelope) is fresh. This
    /// is the timestamp that actually lands in the persisted membership log
    /// via `bootstrap_admit_open_publisher`, so a skewed/malicious peer that
    /// mints a fresh envelope around a far-future-walled Join must still be
    /// rejected.
    #[test]
    fn rejects_join_event_future_skew_with_fresh_envelope() {
        let f = Fixture::new();
        let just_outside = f.now_ms + crate::clock_trust::MAX_FORWARD_SKEW_MS + 1;
        let (req, sig, signed_bytes) =
            f.request_with_nonce_and_join_wall_ms([0x09; 16], just_outside);
        let mut lim = OpenJoinRateLimiter::new();
        assert_eq!(
            verify_and_admit_open_join(
                &req,
                &sig,
                &signed_bytes,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                f.now_ms,
                TEST_SOURCE,
                &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::JoinEventFutureSkew,
            "created_at stayed fresh — the rejection must be attributable to the \
             join_event's own forward-skew bound, not envelope freshness (Stale)"
        );
    }

    /// Boundary companion to the above: `join_event.at.wall_ms` exactly at
    /// `wall_now_ms + MAX_FORWARD_SKEW_MS` must still admit — `reject_future`'s
    /// boundary is inclusive.
    #[test]
    fn admits_join_event_exactly_at_skew_boundary() {
        let f = Fixture::new();
        let at_boundary = f.now_ms + crate::clock_trust::MAX_FORWARD_SKEW_MS;
        let (req, sig, signed_bytes) =
            f.request_with_nonce_and_join_wall_ms([0x0a; 16], at_boundary);
        let mut lim = OpenJoinRateLimiter::new();
        let ok = verify_and_admit_open_join(
            &req,
            &sig,
            &signed_bytes,
            &f.epoch_key,
            f.community_id,
            f.admin_addr,
            &f.current_events,
            f.now_ms,
            FRESHNESS,
            f.now_ms,
            TEST_SOURCE,
            &mut lim,
        )
        .expect("join_event.at.wall_ms exactly at the skew boundary must admit");
        assert_eq!(ok.joiner_addr, f.joiner_addr);
    }

    #[test]
    fn rejects_replayed_nonce() {
        let f = Fixture::new();
        let (req, sig, signed_bytes) = f.valid_request();
        let mut lim = OpenJoinRateLimiter::new();
        verify_and_admit_open_join(
            &req,
            &sig,
            &signed_bytes,
            &f.epoch_key,
            f.community_id,
            f.admin_addr,
            &f.current_events,
            f.now_ms,
            FRESHNESS,
            f.now_ms,
            TEST_SOURCE,
            &mut lim,
        )
        .expect("first ok");
        assert_eq!(
            verify_and_admit_open_join(
                &req,
                &sig,
                &signed_bytes,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                f.now_ms,
                TEST_SOURCE,
                &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::Replay
        );
    }

    #[test]
    fn rate_limit_sheds_excess() {
        let f = Fixture::new();
        let mut lim = OpenJoinRateLimiter::new();
        let mut last: Result<(), OpenJoinReject> = Ok(());
        for _ in 0..(OPEN_JOIN_RATE_LIMIT_PER_WINDOW + 1) {
            let (req, sig, signed_bytes) = f.fresh_request(); // unique nonce each time
            last = verify_and_admit_open_join(
                &req,
                &sig,
                &signed_bytes,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                f.now_ms,
                TEST_SOURCE,
                &mut lim,
            )
            .map(|_| ());
        }
        assert_eq!(last.unwrap_err(), OpenJoinReject::RateLimited);
    }

    /// A request shed by the rate limiter must NOT persist its nonce: once the
    /// rate-limit window rolls over, the SAME nonce must be admissible (it was
    /// never actually accepted, so retrying it is not a replay).
    #[test]
    fn rate_limited_request_nonce_is_retryable_after_window() {
        let f = Fixture::new();
        let mut lim = OpenJoinRateLimiter::new();
        // Saturate the window with unique nonces so the next request is shed.
        for _ in 0..OPEN_JOIN_RATE_LIMIT_PER_WINDOW {
            let (req, sig, sb) = f.fresh_request();
            verify_and_admit_open_join(
                &req,
                &sig,
                &sb,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                f.now_ms,
                TEST_SOURCE,
                &mut lim,
            )
            .expect("in-window requests admit");
        }
        // This one is rate-limited; its nonce must NOT be recorded.
        let (shed_req, shed_sig, shed_sb) = f.valid_request();
        assert_eq!(
            verify_and_admit_open_join(
                &shed_req,
                &shed_sig,
                &shed_sb,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                f.now_ms,
                TEST_SOURCE,
                &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::RateLimited
        );
        // After the rate-limit window rolls over, the SAME nonce is admissible
        // (not a replay) because the shed request never persisted it.
        let later = f.now_ms + OPEN_JOIN_RATE_LIMIT_WINDOW_MS + 1;
        verify_and_admit_open_join(
            &shed_req,
            &shed_sig,
            &shed_sb,
            &f.epoch_key,
            f.community_id,
            f.admin_addr,
            &f.current_events,
            later,
            // Widen freshness so the request's created_at is still in-window at
            // the later wall clock (the rate-limit window > default freshness).
            OPEN_JOIN_RATE_LIMIT_WINDOW_MS * 4,
            later,
            TEST_SOURCE,
            &mut lim,
        )
        .expect("a previously rate-limited nonce is admissible after the window");
    }

    #[test]
    fn limiter_window_keys_on_monotonic_clock_not_wall() {
        // ZEB-711 / B1: the rate-limit window rolls on the limiter's OWN monotonic
        // clock (`limiter_now_ms`), never the beacon wall clock (`wall_now_ms`).
        // Hold wall FIXED (freshness constant) and advance only the limiter clock:
        // the window must roll on the limiter clock alone. If the limiter (wrongly)
        // keyed on wall, the post-roll request would still be shed (wall never moved).
        let f = Fixture::new();
        let mut lim = OpenJoinRateLimiter::new();
        let wall = f.now_ms; // fixed, in-freshness of created_at (= 1000)

        // Fill the window at limiter t = 0.
        for _ in 0..OPEN_JOIN_RATE_LIMIT_PER_WINDOW {
            let (req, sig, sb) = f.fresh_request();
            verify_and_admit_open_join(
                &req,
                &sig,
                &sb,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                wall,
                FRESHNESS,
                0,
                TEST_SOURCE,
                &mut lim,
            )
            .expect("in-window requests admit");
        }

        // Same limiter time, wall unchanged → window is full → shed.
        let (shed_req, shed_sig, shed_sb) = f.fresh_request();
        assert_eq!(
            verify_and_admit_open_join(
                &shed_req,
                &shed_sig,
                &shed_sb,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                wall,
                FRESHNESS,
                0,
                TEST_SOURCE,
                &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::RateLimited,
            "window is full at the same limiter time"
        );

        // Advance ONLY the limiter clock past the window; wall stays fixed. The
        // window rolls on the monotonic limiter clock → admits. A wall-keyed
        // window would still be full here (wall never moved) — the discriminator.
        let (next_req, next_sig, next_sb) = f.fresh_request();
        verify_and_admit_open_join(
            &next_req,
            &next_sig,
            &next_sb,
            &f.epoch_key,
            f.community_id,
            f.admin_addr,
            &f.current_events,
            wall,
            FRESHNESS,
            OPEN_JOIN_RATE_LIMIT_WINDOW_MS + 1,
            TEST_SOURCE,
            &mut lim,
        )
        .expect("window rolled on the monotonic limiter clock, so this admits");
    }

    /// ZEB-853 (B7, Half 2): the admission budget is PER-SOURCE, keyed on the
    /// connecting `remote_id`. One source exhausting its own 20/60s window must
    /// NOT shed a different source — the pre-fix global counter let a single
    /// flooder lock out every legitimate open-joiner. Exercises the re-keyed
    /// `allow(source, now)` directly (the behavioral guarantee lives in the
    /// per-key `KeyedSlidingWindow`).
    #[test]
    fn open_join_rate_limit_is_per_source() {
        let mut rl = OpenJoinRateLimiter::new();
        let a = [1u8; 32];
        let b = [2u8; 32];
        let now = 0u64;
        for _ in 0..OPEN_JOIN_RATE_LIMIT_PER_WINDOW {
            assert!(rl.allow(a, now), "A's own window admits up to the cap");
        }
        assert!(!rl.allow(a, now), "A exhausted its own window");
        assert!(
            rl.allow(b, now),
            "B unaffected by A — no shared-budget lockout"
        );
    }

    /// ZEB-853 (B7, Half 1): the pre-auth Tier-1 connection shield is keyed on
    /// the un-spoofable `remote_id`. One flooding source is shed past its
    /// per-connection cap while a fresh source is still admitted — so a single
    /// endpoint can't force unbounded pre-consent crypto NOR lock out others
    /// before any packet is even read. The gate in the acceptor is a thin
    /// wrapper over this; the behavioral guarantee lives in `KeyedSlidingWindow`.
    #[test]
    fn open_join_tier1_sheds_one_source_not_others() {
        let lim = OpenJoinConnLimiter::with_caps(3, 60_000);
        let a = [0xAAu8; 32];
        let b = [0xBBu8; 32];
        let now = 1_000u64;
        for _ in 0..3 {
            assert!(
                lim.admit_connection(a, now).is_ok(),
                "source A admitted up to its per-connection cap"
            );
        }
        assert!(
            lim.admit_connection(a, now).is_err(),
            "source A shed past its per-connection cap"
        );
        assert!(
            lim.admit_connection(b, now).is_ok(),
            "fresh source B still admitted (no cross-source lockout)"
        );
    }

    #[test]
    fn limiter_window_anchors_on_first_request_not_construction() {
        // ZEB-711 / Qodo (PR #580): the monotonic epoch starts at limiter
        // construction, so the window must anchor on the FIRST request, not at
        // t=0 — else an acceptor that idles before the first open-join gets a
        // shortened first window and sheds early.
        let f = Fixture::new();
        let mut lim = OpenJoinRateLimiter::new();
        let first = 30_000; // acceptor idled ~30 s before the first request

        for _ in 0..OPEN_JOIN_RATE_LIMIT_PER_WINDOW {
            let (req, sig, sb) = f.fresh_request();
            verify_and_admit_open_join(
                &req,
                &sig,
                &sb,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                first,
                TEST_SOURCE,
                &mut lim,
            )
            .expect("first-window requests admit");
        }

        // 40 s after the first request — still inside the 60 s window anchored at
        // `first`, so this sheds. A window anchored at construction (t=0) would
        // have rolled at t=60_000 and wrongly admitted.
        let (req, sig, sb) = f.fresh_request();
        assert_eq!(
            verify_and_admit_open_join(
                &req,
                &sig,
                &sb,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                first + 40_000,
                TEST_SOURCE,
                &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::RateLimited,
            "window anchors on the first request, not limiter construction"
        );

        // 60 s+ after the first request → window rolls → admits resume.
        let (req, sig, sb) = f.fresh_request();
        verify_and_admit_open_join(
            &req,
            &sig,
            &sb,
            &f.epoch_key,
            f.community_id,
            f.admin_addr,
            &f.current_events,
            f.now_ms,
            FRESHNESS,
            first + OPEN_JOIN_RATE_LIMIT_WINDOW_MS + 1,
            TEST_SOURCE,
            &mut lim,
        )
        .expect("window rolls 60 s after the first request");
    }

    /// ZEB-865: the aggregate ceiling caps total admissions across DISTINCT
    /// sources, even ones well within their own per-source budget. Three sources
    /// each admit once (global cap 3 fills), a fourth is shed NodeCapacity.
    #[test]
    fn global_ceiling_bounds_aggregate_across_distinct_sources() {
        let mut rl = OpenJoinRateLimiter::with_caps(20, 3);
        let now = 0u64;
        for i in 0..3u8 {
            assert_eq!(
                rl.admit_source([i; 32], &[i; 16], now),
                Ok(()),
                "distinct source {i} within the aggregate ceiling admits"
            );
        }
        assert_eq!(
            rl.admit_source([9u8; 32], &[9u8; 16], now),
            Err(OpenJoinReject::NodeCapacity),
            "aggregate ceiling sheds an under-budget source once the node is full"
        );
    }

    /// ZEB-865: the ceiling must NOT re-create B7's single-source lockout. With a
    /// high global cap, a source exhausting its OWN 20/60 s window is RateLimited
    /// (not NodeCapacity), and a different source still admits.
    #[test]
    fn global_ceiling_does_not_relock_single_source() {
        let mut rl = OpenJoinRateLimiter::with_caps(20, 1024);
        let a = [0xAA; 32];
        let now = 0u64;
        for i in 0..20u8 {
            assert_eq!(rl.admit_source(a, &[i; 16], now), Ok(()));
        }
        assert_eq!(
            rl.admit_source(a, &[0xF0; 16], now),
            Err(OpenJoinReject::RateLimited),
            "A over its own budget is RateLimited, not NodeCapacity"
        );
        assert_eq!(
            rl.admit_source([0xBB; 32], &[0xB0; 16], now),
            Ok(()),
            "a different source is unaffected — no cross-source lockout"
        );
    }

    /// ZEB-865 discriminator: a per-source shed must NOT drain the aggregate
    /// ceiling. with_caps(20, 30): source A makes 25 attempts (20 admit + 5
    /// shed). Only the 20 admits spend global tokens, so exactly 10 further
    /// distinct sources admit and the 11th sheds NodeCapacity. A bug where sheds
    /// drained the ceiling would leave only 5 (30 - 25).
    #[test]
    fn single_source_shed_does_not_drain_global_ceiling() {
        let mut rl = OpenJoinRateLimiter::with_caps(20, 30);
        let a = [0xAA; 32];
        let now = 0u64;
        for i in 0..20u8 {
            assert_eq!(rl.admit_source(a, &[i; 16], now), Ok(()));
        }
        for i in 20..25u8 {
            assert_eq!(
                rl.admit_source(a, &[i; 16], now),
                Err(OpenJoinReject::RateLimited),
                "A's over-budget attempts are per-source shed"
            );
        }
        for i in 0..10u8 {
            assert_eq!(
                rl.admit_source([0x40 + i; 32], &[0x80 + i; 16], now),
                Ok(()),
                "distinct source {i} fits the remaining aggregate headroom (30 - 20)"
            );
        }
        assert_eq!(
            rl.admit_source([0x50; 32], &[0x90; 16], now),
            Err(OpenJoinReject::NodeCapacity),
            "11th further source exceeds the ceiling — the 5 sheds spent no tokens"
        );
    }

    /// ZEB-865: the aggregate window rolls on the limiter's OWN monotonic clock,
    /// like the per-source window. Fill it at t=0, advance past the window, admits
    /// resume.
    #[test]
    fn global_ceiling_keys_on_monotonic_clock() {
        let mut rl = OpenJoinRateLimiter::with_caps(20, 2);
        for i in 0..2u8 {
            assert_eq!(rl.admit_source([i; 32], &[i; 16], 0), Ok(()));
        }
        assert_eq!(
            rl.admit_source([9; 32], &[9; 16], 0),
            Err(OpenJoinReject::NodeCapacity),
            "ceiling full at t=0"
        );
        assert_eq!(
            rl.admit_source([9; 32], &[0x19; 16], OPEN_JOIN_RATE_LIMIT_WINDOW_MS + 1),
            Ok(()),
            "aggregate window rolled on the monotonic limiter clock"
        );
    }

    /// ZEB-865: a request shed by the aggregate ceiling must not persist its
    /// nonce (it was never accepted) — through the real gate, the SAME nonce
    /// admits after the window rolls.
    #[test]
    fn globally_shed_request_nonce_is_retryable_after_window() {
        let f = Fixture::new();
        let mut lim = OpenJoinRateLimiter::with_caps(
            OPEN_JOIN_RATE_LIMIT_PER_WINDOW,
            1, // global cap 1 → the second distinct source is ceiling-shed
        );
        let src_a = [0x01; 32];
        let src_b = [0x02; 32];

        // First request fills the aggregate ceiling (global 1/1).
        let (req0, sig0, sb0) = f.fresh_request();
        verify_and_admit_open_join(
            &req0,
            &sig0,
            &sb0,
            &f.epoch_key,
            f.community_id,
            f.admin_addr,
            &f.current_events,
            f.now_ms,
            FRESHNESS,
            f.now_ms,
            src_a,
            &mut lim,
        )
        .expect("first request admits and fills the ceiling");

        // Second request (different source, fixed nonce [0x07;16]) is ceiling-shed.
        let (req1, sig1, sb1) = f.valid_request();
        assert_eq!(
            verify_and_admit_open_join(
                &req1,
                &sig1,
                &sb1,
                &f.epoch_key,
                f.community_id,
                f.admin_addr,
                &f.current_events,
                f.now_ms,
                FRESHNESS,
                f.now_ms,
                src_b,
                &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::NodeCapacity,
        );

        // After the window rolls, the SAME nonce admits (never persisted).
        let later = f.now_ms + OPEN_JOIN_RATE_LIMIT_WINDOW_MS + 1;
        verify_and_admit_open_join(
            &req1,
            &sig1,
            &sb1,
            &f.epoch_key,
            f.community_id,
            f.admin_addr,
            &f.current_events,
            later,
            OPEN_JOIN_RATE_LIMIT_WINDOW_MS * 4,
            later,
            src_b,
            &mut lim,
        )
        .expect("a ceiling-shed nonce is admissible after the window");
    }
}
