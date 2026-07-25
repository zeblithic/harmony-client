//! ZEB-371 Task 12: process-local pending inbound friend-request store.
//!
//! Path A (mutual-key, no token) records an inbound `FriendLinkRequest` from a
//! NEW owner here and replies `FriendLinkResponse::Pending` (writing NO friend)
//! until the user explicitly accepts. The user-facing accept/decline/add IPCs
//! (the NEXT task) reach this store via the `Arc<PendingFriendRequests>` parked
//! on `NodeState`:
//!   * accept → [`PendingFriendRequests::approve`] (atomically removes the
//!     inbound entry from the inbox AND marks the requester approved so their
//!     NEXT dial passes the consent gate via `prior_accept`),
//!   * decline → [`PendingFriendRequests::decline`] (drop the inbound + any
//!     approval),
//!   * the acceptor, on a re-dial it accepts inline, consumes the approval via
//!     [`PendingFriendRequests::take_approved`].
//!
//! This is PROCESS-LOCAL (not owner-state CRDT) and intentionally ephemeral: a
//! pending request that hasn't been accepted by shutdown is simply re-sent by
//! the requester's next dial. No persistence, no replication.

use crate::owner_state_types::OwnerAddr;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// ZEB-376 Task 11: what KIND of pending inbox entry this is. A plain Path-A
/// `LinkRequest` (mutual-key dial) is accepted by marking the requester approved
/// so their next dial links inline; an `IntroductionOffer` (AskMe-staged F→X
/// vouch) is accepted by X actively dialing the introducee via
/// `complete_introduction` — the SAME action an auto-`Proceed` runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingKind {
    /// Path-A mutual-key inbound request (the historical default).
    LinkRequest,
    /// AskMe-staged introduction offer carrying the already-verified vouch +
    /// relayed reachability the accept path needs to dial the introducee. Boxed
    /// so the enum (and `PendingInbound`) stays small for the common
    /// `LinkRequest` case.
    IntroductionOffer(Box<StoredIntroductionOffer>),
}

/// ZEB-376 Task 11: the introduction data staged for an AskMe accept. Recorded
/// by the friend-PEX `Introduction` arm AFTER it has already verified F's vouch,
/// the relayed reachability's inner signature, and its freshness — so a staged
/// offer is trustworthy. On the user's explicit accept, `take_offer` hands this
/// back and X runs `complete_introduction(subject, reachability, …)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredIntroductionOffer {
    /// The voucher (F) who relayed this introduction — surfaced to the UI as
    /// `introducedBy` and used only for display / provenance.
    pub voucher: OwnerAddr,
    /// The introducee (X's prospective friend) X dials on accept.
    pub subject: OwnerAddr,
    /// The subject's relayed, already-verified reachability X synthesizes the
    /// dial target from.
    pub reachability: crate::reachability_record::ReachabilityAnnouncePayload,
}

/// ZEB-694: a staged AskMe offer older than this is treated as dead (its relayed
/// reachability is past the intro/reachability freshness bound anyway) — swept
/// from the inbox and rejected at accept time with an "expired" message.
pub const INTRODUCTION_OFFER_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000; // 7d

pub fn is_offer_expired(received_at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(received_at_ms) >= INTRODUCTION_OFFER_TTL_MS
}

/// ZEB-784: the requester's dial target, captured from the authenticated
/// `FriendLinkRequest` that produced a `Pending` reply, so the user's later
/// Accept can dial them back instead of waiting for a re-dial that nobody
/// triggers.
///
/// Both fields are **signature-bound** into the request's `contact_digest`
/// (ZEB-473 §6.3), and cert + signature verification always runs BEFORE the
/// consent decision (see `decide_consent`'s contract) — so this is verified
/// material, not attacker-steerable routing. That is what makes storing it
/// safe: an active MITM cannot substitute its own node here without failing
/// the upstream authentication that already ran.
///
/// This mirrors [`StoredIntroductionOffer::reachability`], which exists for
/// exactly the same reason on the AskMe path: an accept that must dial needs
/// somewhere to dial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLinkContact {
    /// The requester's iroh `NodeId` — the dial target.
    pub iroh_node_id: [u8; 32],
    /// The requester's home-relay URL, if they advertised one.
    pub home_relay_url: Option<String>,
}

/// One recorded inbound friend request awaiting the user's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInbound {
    /// The requester's advertised display name (UX hint), if any.
    pub display: Option<String>,
    /// Wall-clock epoch-ms the request was first recorded (idempotent: a
    /// re-dial before acceptance does NOT bump this).
    pub received_at_ms: u64,
    /// ZEB-376 Task 11: whether this is a plain Path-A `LinkRequest` or an
    /// AskMe-staged `IntroductionOffer` (which the accept path runs as a
    /// self-dial link rather than a `prior_accept` approval).
    pub kind: PendingKind,
    /// ZEB-784: the signed dial target for a Path-A `LinkRequest`, so Accept can
    /// complete the link itself. `None` for an `IntroductionOffer` (which
    /// carries its own relayed `reachability`) and for pre-ZEB-784 peers whose
    /// request predates the capture.
    pub contact: Option<StoredLinkContact>,
}

#[derive(Default)]
struct Inner {
    inbound: HashMap<OwnerAddr, PendingInbound>,
    approved: HashSet<OwnerAddr>,
    /// ZEB-694: subjects with an introduction accept currently dialing — blocks a
    /// concurrent second accept from double-dialing.
    accepting: HashSet<OwnerAddr>,
}

/// Process-local store of pending inbound friend requests and the set of
/// requesters the user has approved (but whose link hasn't completed yet).
///
/// A single `Mutex<Inner>` makes `approve` (remove inbound + insert approved)
/// atomic with respect to concurrent `record_inbound` calls, preventing a
/// re-dial from resurrecting a just-approved request. The lock is held only
/// for the duration of a single map op — never across an `.await`.
#[derive(Default)]
pub struct PendingFriendRequests {
    inner: Mutex<Inner>,
}

impl PendingFriendRequests {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an inbound request from `addr`. IDEMPOTENT: a request already
    /// recorded (and not yet declined/accepted) keeps its original
    /// `received_at_ms` and display — a re-dial does not reset the entry.
    /// If `addr` has already been approved, the call is a no-op (prevents
    /// a concurrent re-dial from resurrecting the inbox entry).
    ///
    /// ZEB-784: `contact` is the authenticated dial target from the request that
    /// produced this entry, retained so Accept can dial back. Because the entry
    /// is idempotent, a re-dial does NOT refresh the stored contact — the first
    /// one wins, consistent with `received_at_ms`. That is deliberate: a peer
    /// whose routing changed mid-wait is handled by the fallback (the approval
    /// is still recorded, so their own next dial still links), not by letting a
    /// later dial silently rewrite where we will call back to.
    pub fn record_inbound(
        &self,
        addr: OwnerAddr,
        display: Option<String>,
        contact: Option<StoredLinkContact>,
        now_ms: u64,
    ) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        if inner.approved.contains(&addr) {
            return;
        }
        inner.inbound.entry(addr).or_insert(PendingInbound {
            display,
            received_at_ms: now_ms,
            kind: PendingKind::LinkRequest,
            contact,
        });
    }

    /// ZEB-784: non-consuming read of the stored dial target for `addr`.
    ///
    /// Read-only by design: the accept path reads this BEFORE it mutates
    /// anything, so a dial that fails leaves the entry exactly as it was and the
    /// user can Accept again. Returns `None` when `addr` is unknown, is an
    /// `IntroductionOffer` (which dials via its own reachability), or was
    /// recorded by a peer that predates the capture.
    pub fn peek_contact(&self, addr: &OwnerAddr) -> Option<StoredLinkContact> {
        let inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.inbound.get(addr).and_then(|p| p.contact.clone())
    }

    /// ZEB-376 Task 11: stage an AskMe introduction OFFER for `subject` in the
    /// pending inbox. The offer SUPERSEDES any prior inbound entry for `subject`
    /// (an unconditional `insert`, not an `or_insert`): a verified,
    /// F-vouched + reachability-checked offer is strictly more actionable than a
    /// bare Path-A `LinkRequest` staged earlier for the same owner — dropping it
    /// (#6) would strand the user with a request they can't accept-as-introduction.
    /// A re-delivered SAME offer just replaces itself (resetting `received_at_ms`
    /// to `now_ms` — acceptable; it is the same verified offer).
    pub fn record_introduction_offer(
        &self,
        subject: OwnerAddr,
        display: Option<String>,
        now_ms: u64,
        offer: StoredIntroductionOffer,
    ) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.inbound.insert(
            subject,
            PendingInbound {
                display,
                received_at_ms: now_ms,
                kind: PendingKind::IntroductionOffer(Box::new(offer)),
                // ZEB-784: an offer dials via its own verified `reachability`;
                // it never needs the Path-A callback contact.
                contact: None,
            },
        );
    }

    /// ZEB-376 Task 11: remove + return the staged introduction offer for
    /// `subject` (the user tapped Accept on an introduction). Returns `None` when
    /// `subject` has no entry OR its entry is a plain `LinkRequest` — leaving a
    /// `LinkRequest` entry INTACT so the accept-IPC falls through to the Path-A
    /// [`approve`](Self::approve) branch. One-shot: a second call returns `None`.
    pub fn take_offer(&self, subject: &OwnerAddr) -> Option<StoredIntroductionOffer> {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        // Only consume the entry when it is actually an IntroductionOffer.
        match inner.inbound.get(subject) {
            Some(p) if matches!(p.kind, PendingKind::IntroductionOffer(_)) => {
                match inner.inbound.remove(subject).map(|p| p.kind) {
                    Some(PendingKind::IntroductionOffer(offer)) => Some(*offer),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// ZEB-376 Task 11: non-consuming peek — true iff `subject` has a staged
    /// `IntroductionOffer` (NOT a plain `LinkRequest`). Lets the accept path
    /// validate its self-dial handles BEFORE the one-shot [`take_offer`](Self::take_offer)
    /// irreversibly consumes the offer (HARD-RULE: verify before an irreversible
    /// write — a missing handle must leave the offer row intact + recoverable).
    /// Read-only; a subsequent `take_offer` is still the single consuming op.
    pub fn has_offer(&self, subject: &OwnerAddr) -> bool {
        let inner = self.inner.lock().expect("pending inner mutex poisoned");
        matches!(
            inner.inbound.get(subject),
            Some(p) if matches!(p.kind, PendingKind::IntroductionOffer(_))
        )
    }

    /// Non-consuming clone of a staged `IntroductionOffer` plus its `received_at_ms`.
    /// Returns `None` if the entry is absent or a plain `LinkRequest`. The accept
    /// path uses this (instead of `take_offer`) so a failed dial leaves the offer
    /// staged for retry (ZEB-694).
    pub fn peek_offer(&self, subject: &OwnerAddr) -> Option<(StoredIntroductionOffer, u64)> {
        let inner = self.inner.lock().expect("pending inner mutex poisoned");
        match inner.inbound.get(subject) {
            Some(p) => match &p.kind {
                PendingKind::IntroductionOffer(o) => Some(((**o).clone(), p.received_at_ms)),
                PendingKind::LinkRequest => None,
            },
            None => None,
        }
    }

    /// Snapshot the currently-pending inbound requests (for the list IPC).
    pub fn list(&self) -> Vec<(OwnerAddr, PendingInbound)> {
        let inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner
            .inbound
            .iter()
            .map(|(addr, p)| (*addr, p.clone()))
            .collect()
    }

    /// True if the user has approved `addr` (so the requester's next dial can be
    /// accepted inline via the consent decision's `prior_accept`).
    pub fn is_approved(&self, addr: &OwnerAddr) -> bool {
        self.inner
            .lock()
            .expect("pending inner mutex poisoned")
            .approved
            .contains(addr)
    }

    /// Mark `addr` as approved (the user tapped Accept). The approval persists
    /// until the link completes ([`take_approved`](Self::take_approved)) or the
    /// user declines ([`decline`](Self::decline)).
    ///
    /// Prefer [`approve`](Self::approve) for the accept-IPC path — it also
    /// removes the entry from the pending inbox atomically.
    pub fn mark_approved(&self, addr: OwnerAddr) {
        self.inner
            .lock()
            .expect("pending inner mutex poisoned")
            .approved
            .insert(addr);
    }

    /// The user accepted `addr`: drop it from the pending inbox AND record the
    /// approval atomically (so the requester's next dial completes via
    /// `prior_accept`). The inbox no longer shows it; the friend appears in the
    /// friends list once the link completes.
    pub fn approve(&self, addr: OwnerAddr) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.inbound.remove(&addr);
        inner.approved.insert(addr);
    }

    /// Remove + return whether `addr` was approved. Called once the inbound
    /// handshake actually completes the inline accept, so a single Accept tap
    /// authorises exactly one completed link (re-arming requires a fresh tap).
    pub fn take_approved(&self, addr: &OwnerAddr) -> bool {
        self.inner
            .lock()
            .expect("pending inner mutex poisoned")
            .approved
            .remove(addr)
    }

    /// A link with `addr` completed (via token redeem OR inline accept): drop
    /// any lingering pending-inbox entry AND consume any approval, atomically.
    ///
    /// This is the completion-cleanup the acceptor calls on EVERY path that
    /// writes an Active friend. Without it, a requester who earlier received
    /// `Pending` (recorded in the inbox) and then became an active friend
    /// through a *different* path — e.g. redeeming a friend token (`TokenPath`)
    /// or being added by key — would linger as a "ghost" request in
    /// `list_pending_friend_requests` even though they are already a friend.
    /// Idempotent: a no-op when neither set holds `addr`.
    pub fn clear_completed(&self, addr: &OwnerAddr) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.inbound.remove(addr);
        inner.approved.remove(addr);
    }

    /// Decline `addr`: drop any recorded inbound request AND any approval.
    pub fn decline(&self, addr: &OwnerAddr) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.inbound.remove(addr);
        inner.approved.remove(addr);
    }

    /// Test-and-set: `true` and marks in-flight if no accept for `subject` is
    /// already dialing; `false` if one is. Pair with `end_accept` (or the RAII
    /// `AcceptInFlightGuard`).
    #[must_use]
    pub fn try_begin_accept(&self, subject: OwnerAddr) -> bool {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.accepting.insert(subject) // HashSet::insert returns false if already present
    }

    /// Clear the in-flight marker for `subject`.
    pub fn end_accept(&self, subject: &OwnerAddr) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        inner.accepting.remove(subject);
    }

    /// Remove every staged `IntroductionOffer` older than the TTL. Plain
    /// `LinkRequest` entries have their own lifecycle and are left untouched.
    /// Returns the number of offers swept.
    pub fn sweep_expired_offers(&self, now_ms: u64) -> usize {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        let before = inner.inbound.len();
        inner.inbound.retain(|_, p| match p.kind {
            PendingKind::IntroductionOffer(_) => !is_offer_expired(p.received_at_ms, now_ms),
            PendingKind::LinkRequest => true,
        });
        before - inner.inbound.len()
    }
}

/// RAII: clears the in-flight accept marker on drop, so every accept exit path
/// (early return, dial error, panic) releases it.
#[must_use = "dropping this immediately releases the in-flight accept marker"]
pub struct AcceptInFlightGuard {
    store: Arc<PendingFriendRequests>,
    subject: OwnerAddr,
}

impl AcceptInFlightGuard {
    pub fn new(store: Arc<PendingFriendRequests>, subject: OwnerAddr) -> Self {
        Self { store, subject }
    }
}

impl Drop for AcceptInFlightGuard {
    fn drop(&mut self) {
        self.store.end_accept(&self.subject);
    }
}

/// ZEB-376: process-local pre-authorization for introductions the user
/// initiated. When you send an `IntroduceRequest` for target X you `record(X)`;
/// X's inbound introduction-driven `FriendLinkRequest` then auto-accepts because
/// its authenticated sender is `take`-able here. One-shot + TTL-bounded so a
/// stale pre-auth can't silently accept an unrelated later dial. Not persisted
/// (ephemeral, like `PendingFriendRequests`).
#[derive(Default)]
pub struct PendingOutboundIntroductions {
    inner: TtlPreAuth,
}

/// TTL bounding a recorded pre-authorization: a `record`ed target older than
/// this (in wall-clock epoch-ms) never authorizes an inline-introduced accept.
pub const OUTBOUND_INTRO_TTL_MS: u64 = 10 * 60 * 1000; // 10 min

impl PendingOutboundIntroductions {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (idempotent refresh of the deadline for) a pre-authorization for
    /// `target` at `now_ms`. A later `record` bumps the recorded instant.
    pub fn record(&self, target: OwnerAddr, now_ms: u64) {
        self.inner.record(target, now_ms);
    }

    /// Remove + return true iff `target` was recorded AND still within the TTL.
    /// A present-but-expired entry is removed and returns false. One-shot: even
    /// a fresh hit is consumed, so a single pre-auth accepts exactly one dial.
    pub fn take(&self, target: &OwnerAddr, now_ms: u64) -> bool {
        self.inner.take(target, now_ms, OUTBOUND_INTRO_TTL_MS)
    }
}

/// ZEB-784: the same one-shot, TTL-bounded pre-authorization for a **plain
/// Path-A link request** the local user initiated.
///
/// When you `add_friend_by_key(X)` and X's node replies `Pending`, you record X
/// here. X's later reciprocal dial — the one their Accept now performs — is then
/// `take`-able and auto-accepts as [`ConsentDecision::AcceptInline`].
///
/// **Why this is required and not merely nice.** Before ZEB-784 the dialer's
/// `Pending` branch wrote nothing at all, so the requester had no local record
/// of their own outbound request (that gap is ZEB-783). `decide_consent` gates
/// on `known` = "already an Active|Pending friend", which was therefore `false`
/// — meaning a callback dial from the acceptor would have been treated as a
/// brand-new request from a stranger and parked at `Pending`. The deadlock would
/// have *mirrored* rather than resolved, with both sides now showing a pending
/// request. ZEB-783 and ZEB-784 are one defect seen from two ends.
///
/// **Why this is deliberately NOT routed through `known && auto_accept_known`.**
/// That path is opt-out: a user who turns "auto-accept friends I already know"
/// off would silently get the original deadlock back. "I dialed you first" is an
/// explicit, per-target consent decision by this user, so it carries its own
/// accept authority independent of that toggle.
#[derive(Default)]
pub struct PendingOutboundLinks {
    inner: TtlPreAuth,
    /// Where this store persists. `None` = ephemeral (tests, and any caller that
    /// has no identity dir yet), which behaves exactly like the pre-ZEB-784
    /// in-memory store.
    path: Option<std::path::PathBuf>,
}

/// File name for the persisted outbound-link records, alongside the other
/// per-identity state under `<identity_dir>/`.
pub const OUTBOUND_LINKS_FILENAME: &str = "outbound_friend_links.cbor";

/// TTL bounding a recorded outbound link request.
///
/// Much longer than [`OUTBOUND_INTRO_TTL_MS`], and the difference is the whole
/// point: an introduction pre-auth covers a machine-timescale round trip, but
/// this one has to survive **human accept latency**. The recipient may not open
/// the app until tomorrow. A 10-minute bound here would expire the pre-auth long
/// before the user ever taps Accept and reintroduce the exact deadlock this
/// change exists to remove. Matched to `INTRODUCTION_OFFER_TTL_MS` (7d), which
/// is already this codebase's answer to "how long may a human sit on a pending
/// friend decision".
pub const OUTBOUND_LINK_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000; // 7d

impl PendingOutboundLinks {
    /// Empty, ephemeral store (no persistence).
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the store from `path`, binding it so every later mutation is
    /// persisted.
    ///
    /// Self-heals rather than bricking the boot: a corrupt file is quarantined
    /// aside as `.corrupt-<ms>` (bytes preserved for diagnosis) and an empty
    /// store returned, matching the `load_doc_or_recover` contract used by the
    /// relay-opt-in and DM-inbox stores. Losing these records degrades to
    /// pre-ZEB-784 behaviour — the peer's Accept parks at `Pending` — which is
    /// strictly better than refusing to start.
    pub fn load_or_recover(path: std::path::PathBuf, now_ms: u64) -> Self {
        let map = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "ZEB-784: outbound-link store unreadable; continuing empty");
                HashMap::new()
            }
            Ok(bytes) => match Self::decode(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    let aside = path.with_extension(format!("corrupt-{now_ms}"));
                    let _ = std::fs::rename(&path, &aside);
                    tracing::warn!(error = %e, quarantined = %aside.display(),
                        "ZEB-784: outbound-link store corrupt; quarantined, continuing empty");
                    HashMap::new()
                }
            },
        };
        // Drop anything already past its TTL at load time rather than carrying
        // dead records forward — `take` would reject them anyway, and pruning
        // here keeps the file from growing without bound across restarts.
        let live: HashMap<OwnerAddr, u64> = map
            .into_iter()
            .filter(|(_, rec)| TtlPreAuth::is_live(*rec, now_ms, OUTBOUND_LINK_TTL_MS))
            .collect();
        let store = Self {
            inner: TtlPreAuth {
                inner: Mutex::new(live),
            },
            path: Some(path),
        };
        // Rewrite immediately so a pruned/quarantined file is reflected on disk
        // even if the user never sends another request.
        store.persist();
        store
    }

    /// CBOR: `Vec<([u8; 16], u64)>`. A `Vec` of pairs rather than a map because
    /// `OwnerAddr` is a byte array, and CBOR map keys of that shape round-trip
    /// less predictably across serde versions than a plain sequence does.
    fn decode(bytes: &[u8]) -> Result<HashMap<OwnerAddr, u64>, String> {
        let rows: Vec<([u8; 16], u64)> =
            ciborium::from_reader(bytes).map_err(|e| format!("cbor decode: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|(a, rec)| (OwnerAddr(a), rec))
            .collect())
    }

    /// Snapshot under the lock, then write OUTSIDE it — never hold a mutex
    /// across filesystem I/O.
    fn persist(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let rows: Vec<([u8; 16], u64)> = {
            let m = self
                .inner
                .inner
                .lock()
                .expect("outbound pre-auth mutex poisoned");
            m.iter().map(|(addr, rec)| (addr.0, *rec)).collect()
        };
        let mut bytes = Vec::new();
        if let Err(e) = ciborium::into_writer(&rows, &mut bytes) {
            tracing::warn!(error = %e, "ZEB-784: outbound-link encode failed; not persisted");
            return;
        }
        if let Err(e) = crate::owner_state_persist::save_atomically(path, &bytes) {
            // Best-effort: the in-memory store is still correct for this
            // session, so a write failure costs durability across restart, not
            // the live handshake.
            tracing::warn!(error = %e, path = %path.display(),
                "ZEB-784: outbound-link persist failed");
        }
    }

    /// Record (idempotent refresh of the deadline for) an outbound link request
    /// to `target` at `now_ms`.
    pub fn record(&self, target: OwnerAddr, now_ms: u64) {
        self.inner.record(target, now_ms);
        self.persist();
    }

    /// Non-consuming test: is there a live (un-expired) outbound request to
    /// `target`? Used by the sender-side projection (ZEB-783) so the UI can show
    /// "waiting for them" without burning the one-shot pre-auth.
    pub fn is_pending(&self, target: &OwnerAddr, now_ms: u64) -> bool {
        self.inner.peek(target, now_ms, OUTBOUND_LINK_TTL_MS)
    }

    /// Snapshot every live outbound request as `(target, recorded_at_ms)`.
    /// Expired entries are filtered out but NOT removed (this is a read).
    pub fn list(&self, now_ms: u64) -> Vec<(OwnerAddr, u64)> {
        self.inner.list(now_ms, OUTBOUND_LINK_TTL_MS)
    }

    /// Remove + return true iff `target` was recorded AND still within the TTL.
    /// One-shot, same contract as [`PendingOutboundIntroductions::take`].
    pub fn take(&self, target: &OwnerAddr, now_ms: u64) -> bool {
        let hit = self.inner.take(target, now_ms, OUTBOUND_LINK_TTL_MS);
        // Persist unconditionally: `take` removes the entry whether or not it
        // was live, so the on-disk copy is stale either way. Skipping the write
        // on a miss would let an expired record survive a restart and then be
        // re-evaluated against a fresh clock.
        self.persist();
        hit
    }

    /// Drop any record for `target` without consuming it as an authorization —
    /// used when the link completed by some other route, or the user cancelled.
    pub fn forget(&self, target: &OwnerAddr) {
        self.inner.forget(target);
        self.persist();
    }
}

/// Shared one-shot, TTL-bounded pre-authorization map behind
/// [`PendingOutboundIntroductions`] and [`PendingOutboundLinks`]. The TTL is a
/// per-call parameter rather than a field because the two wrappers bound
/// fundamentally different waits (machine round trip vs human decision), and
/// keeping it at the call site puts each constant next to the contract that
/// justifies it.
#[derive(Default)]
struct TtlPreAuth {
    inner: Mutex<HashMap<OwnerAddr, u64>>,
}

impl TtlPreAuth {
    fn record(&self, target: OwnerAddr, now_ms: u64) {
        self.inner
            .lock()
            .expect("outbound pre-auth mutex poisoned")
            .insert(target, now_ms);
    }

    /// `true` iff `rec` is a live record as of `now_ms`. Fails CLOSED on a
    /// BACKWARD clock: `saturating_sub` would yield `0 < ttl` when
    /// `now_ms < rec`, keeping a stale pre-auth valid indefinitely, so
    /// `now_ms >= rec` is required explicitly.
    fn is_live(rec: u64, now_ms: u64, ttl_ms: u64) -> bool {
        now_ms >= rec && now_ms - rec < ttl_ms
    }

    /// Consuming take. The entry is removed even when expired (and even on a
    /// backward clock), preserving the one-shot property.
    fn take(&self, target: &OwnerAddr, now_ms: u64, ttl_ms: u64) -> bool {
        let mut m = self.inner.lock().expect("outbound pre-auth mutex poisoned");
        match m.remove(target) {
            Some(rec) => Self::is_live(rec, now_ms, ttl_ms),
            None => false,
        }
    }

    /// Non-consuming read.
    fn peek(&self, target: &OwnerAddr, now_ms: u64, ttl_ms: u64) -> bool {
        let m = self.inner.lock().expect("outbound pre-auth mutex poisoned");
        m.get(target)
            .is_some_and(|rec| Self::is_live(*rec, now_ms, ttl_ms))
    }

    fn list(&self, now_ms: u64, ttl_ms: u64) -> Vec<(OwnerAddr, u64)> {
        let m = self.inner.lock().expect("outbound pre-auth mutex poisoned");
        m.iter()
            .filter(|(_, rec)| Self::is_live(**rec, now_ms, ttl_ms))
            .map(|(addr, rec)| (*addr, *rec))
            .collect()
    }

    fn forget(&self, target: &OwnerAddr) {
        self.inner
            .lock()
            .expect("outbound pre-auth mutex poisoned")
            .remove(target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> OwnerAddr {
        OwnerAddr([b; 16])
    }

    /// Minimal (unsigned) reachability payload — `take_offer`/`record_*` never
    /// inspect its contents, so a zeroed record suffices for the store tests.
    fn fixture_reach() -> crate::reachability_record::ReachabilityAnnouncePayload {
        crate::reachability_record::ReachabilityAnnouncePayload {
            iroh_node_id: [7u8; 32],
            home_relay_url: String::new(),
            direct_addresses: Vec::new(),
            announced_at_ms: 0,
            identity_signature: [0u8; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        }
    }

    #[test]
    fn introduction_offer_stages_and_take_consumes() {
        let store = PendingFriendRequests::new();
        let offer = StoredIntroductionOffer {
            voucher: addr(2),
            subject: addr(1),
            reachability: fixture_reach(),
        };
        store.record_introduction_offer(addr(1), Some("alice".into()), 1_000, offer);
        // Surfaces in the inbox with its introduced_by voucher.
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert!(
            matches!(&list[0].1.kind, PendingKind::IntroductionOffer(o) if o.voucher == addr(2))
        );
        // take_offer consumes it once.
        assert!(store.take_offer(&addr(1)).is_some());
        assert!(store.take_offer(&addr(1)).is_none());
        assert!(store.list().is_empty());
    }

    #[test]
    fn introduction_offer_supersedes_prior_link_request() {
        // #6 regression: a bare Path-A LinkRequest staged first for owner O must
        // NOT block a later verified IntroductionOffer for O — the offer supersedes
        // the prior inbound entry (unconditional insert, not or_insert). Before the
        // fix, `take_offer(O)` returned None because the LinkRequest still occupied
        // the slot, stranding the user with an unacceptable-as-introduction row.
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(1), Some("link".into()), None, 1_000);
        assert!(matches!(&store.list()[0].1.kind, PendingKind::LinkRequest));
        let offer = StoredIntroductionOffer {
            voucher: addr(2),
            subject: addr(1),
            reachability: fixture_reach(),
        };
        store.record_introduction_offer(addr(1), Some("offer".into()), 2_000, offer);
        // The offer now occupies the slot and is take-able.
        let taken = store.take_offer(&addr(1));
        assert!(
            taken.is_some(),
            "the verified offer must supersede the prior LinkRequest and be take-able"
        );
        assert_eq!(taken.unwrap().voucher, addr(2));
        assert!(
            store.list().is_empty(),
            "take consumed the superseding offer"
        );
    }

    #[test]
    fn take_offer_leaves_link_request_intact() {
        // A plain Path-A LinkRequest must NOT be consumed by the introduction
        // accept path — take_offer returns None and leaves the row so the accept
        // IPC falls through to the `approve` branch.
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(3), Some("bob".into()), None, 2_000);
        assert!(store.take_offer(&addr(3)).is_none());
        assert_eq!(store.list().len(), 1, "LinkRequest row must remain");
        assert!(matches!(&store.list()[0].1.kind, PendingKind::LinkRequest));
    }

    #[test]
    fn peek_offer_clones_without_consuming() {
        let store = PendingFriendRequests::default();
        let subj = OwnerAddr([1; 16]);
        let offer = StoredIntroductionOffer {
            voucher: OwnerAddr([2; 16]),
            subject: subj,
            reachability: fixture_reach(), // existing helper in this test module (:383)
        };
        store.record_introduction_offer(subj, Some("x".into()), 4242, offer.clone());
        let (peeked, received_at) = store.peek_offer(&subj).expect("offer present");
        assert_eq!(peeked, offer);
        assert_eq!(received_at, 4242);
        assert!(store.has_offer(&subj), "peek did NOT consume the offer");
        // a plain LinkRequest yields None
        let other = OwnerAddr([9; 16]);
        store.record_inbound(other, None, None, 1);
        assert!(
            store.peek_offer(&other).is_none(),
            "a LinkRequest is not an offer"
        );
    }

    #[test]
    fn record_then_list_returns_inbound() {
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(1), Some("alice".into()), None, 1_000);
        let list = store.list();
        assert_eq!(list.len(), 1);
        let (a, p) = &list[0];
        assert_eq!(*a, addr(1));
        assert_eq!(p.display.as_deref(), Some("alice"));
        assert_eq!(p.received_at_ms, 1_000);
    }

    #[test]
    fn record_inbound_is_idempotent() {
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(2), Some("first".into()), None, 1_000);
        // A re-dial must NOT overwrite the original entry (display + timestamp).
        store.record_inbound(addr(2), Some("second".into()), None, 9_999);
        let list = store.list();
        assert_eq!(
            list.len(),
            1,
            "duplicate record must not create a 2nd entry"
        );
        let (_, p) = &list[0];
        assert_eq!(p.display.as_deref(), Some("first"));
        assert_eq!(p.received_at_ms, 1_000);
    }

    #[test]
    fn approve_then_take_consumes_once() {
        let store = PendingFriendRequests::new();
        assert!(!store.is_approved(&addr(3)), "unknown addr is not approved");
        store.approve(addr(3));
        assert!(store.is_approved(&addr(3)));
        // take_approved removes + returns true the first time…
        assert!(store.take_approved(&addr(3)));
        // …and false thereafter (consumed).
        assert!(!store.take_approved(&addr(3)));
        assert!(!store.is_approved(&addr(3)));
    }

    #[test]
    fn approve_clears_inbox_and_records_approval() {
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(8), Some("bob".into()), None, 1_000);
        store.approve(addr(8));
        assert!(
            store.list().is_empty(),
            "approved request leaves the pending inbox"
        );
        assert!(
            store.is_approved(&addr(8)),
            "approval recorded for the next dial"
        );
        // one-shot completion still consumes the approval
        assert!(store.take_approved(&addr(8)));
        assert!(!store.take_approved(&addr(8)));
    }

    #[test]
    fn decline_drops_inbound_and_approval() {
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(4), None, None, 5_000);
        store.mark_approved(addr(4));
        store.decline(&addr(4));
        assert!(store.list().is_empty(), "decline drops the inbound request");
        assert!(!store.is_approved(&addr(4)), "decline drops the approval");
        // take_approved is now a no-op (returns false).
        assert!(!store.take_approved(&addr(4)));
    }

    #[test]
    fn take_approved_on_unknown_is_false() {
        let store = PendingFriendRequests::new();
        assert!(!store.take_approved(&addr(5)));
    }

    #[test]
    fn clear_completed_drops_stale_inbox_entry() {
        // Regression (Cursor "Stale pending after link completes"): a requester
        // recorded as Pending who later becomes an active friend via another
        // path (e.g. token redeem) must NOT linger in the pending inbox.
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(7), Some("dora".into()), None, 1_000);
        assert_eq!(store.list().len(), 1, "precondition: recorded as pending");
        store.clear_completed(&addr(7));
        assert!(
            store.list().is_empty(),
            "completed link must clear the stale inbox entry (no ghost request)"
        );
    }

    #[test]
    fn clear_completed_consumes_approval_and_is_idempotent() {
        let store = PendingFriendRequests::new();
        store.approve(addr(9)); // user tapped Accept → approved (inbox already cleared)
        store.clear_completed(&addr(9));
        assert!(
            !store.is_approved(&addr(9)),
            "completion consumes the one-shot approval"
        );
        // Idempotent: a second call on an unknown addr is a harmless no-op.
        store.clear_completed(&addr(9));
        assert!(store.list().is_empty());
    }

    #[test]
    fn record_inbound_skips_already_approved() {
        // A re-dial from an already-approved peer must NOT resurrect the inbox
        // entry — approve+record_inbound must be atomic via the single mutex.
        let store = PendingFriendRequests::new();
        store.approve(addr(6));
        store.record_inbound(addr(6), Some("charlie".into()), None, 5_000);
        assert!(
            store.list().is_empty(),
            "record_inbound must not add to inbox when addr is already approved"
        );
    }

    #[test]
    fn outbound_intro_take_is_one_shot_and_ttl_bounded() {
        let s = PendingOutboundIntroductions::new();
        s.record(addr(1), 1_000);
        // Fresh + present → true, and consumed (one-shot).
        assert!(s.take(&addr(1), 1_500));
        assert!(!s.take(&addr(1), 1_600));
        // Expired records never authorize.
        s.record(addr(2), 1_000);
        assert!(!s.take(&addr(2), 1_000 + OUTBOUND_INTRO_TTL_MS + 1));
        // Unknown target → false.
        assert!(!s.take(&addr(3), 2_000));
    }

    #[test]
    fn outbound_intro_take_fails_closed_on_backward_clock() {
        // #7 (security): a record made at t=1000 must NOT authorize a `take` at
        // t=500 (the wall clock moved backward after recording). `saturating_sub`
        // would have returned 0 < TTL → true, keeping the pre-auth valid. The
        // explicit `now_ms >= rec` guard rejects it — and still consumes the entry
        // (one-shot preserved: a second take returns false regardless).
        let s = PendingOutboundIntroductions::new();
        s.record(addr(1), 1_000);
        assert!(
            !s.take(&addr(1), 500),
            "a backward clock must not authorize a stale pre-auth"
        );
        assert!(
            !s.take(&addr(1), 1_500),
            "the backward-clock take still consumed the entry (one-shot)"
        );
    }

    #[test]
    fn in_flight_guard_blocks_concurrent_accept() {
        use std::sync::Arc;
        let store = Arc::new(PendingFriendRequests::default());
        let subj = OwnerAddr([1; 16]);
        assert!(store.try_begin_accept(subj), "first accept begins");
        assert!(
            !store.try_begin_accept(subj),
            "second concurrent accept is blocked"
        );
        {
            // RAII guard clears the marker on drop.
            let _g = AcceptInFlightGuard::new(Arc::clone(&store), subj);
            // still in flight while the guard lives
            assert!(!store.try_begin_accept(subj));
        } // _g drops here → end_accept
          // NOTE: try_begin_accept above set the flag; the guard's drop cleared it.
        assert!(
            store.try_begin_accept(subj),
            "after the guard drops, a new accept can begin"
        );
    }

    #[test]
    fn sweep_removes_only_expired_offers() {
        let store = PendingFriendRequests::default();
        let fresh = OwnerAddr([1; 16]);
        let stale = OwnerAddr([2; 16]);
        let link = OwnerAddr([3; 16]);
        let mk = |s: OwnerAddr| StoredIntroductionOffer {
            voucher: OwnerAddr([9; 16]),
            subject: s,
            reachability: fixture_reach(), // existing helper in this test module (friend_requests.rs:383)
        };
        let now = 10 * INTRODUCTION_OFFER_TTL_MS;
        store.record_introduction_offer(fresh, None, now, mk(fresh)); // received now → fresh
        store.record_introduction_offer(stale, None, now - INTRODUCTION_OFFER_TTL_MS, mk(stale)); // exactly TTL old → expired
        store.record_inbound(link, None, None, 0); // a LinkRequest, never swept

        assert!(is_offer_expired(now - INTRODUCTION_OFFER_TTL_MS, now));
        assert!(!is_offer_expired(now, now));

        let swept = store.sweep_expired_offers(now);
        assert_eq!(swept, 1, "only the stale offer is swept");
        assert!(store.has_offer(&fresh), "fresh offer retained");
        assert!(!store.has_offer(&stale), "stale offer removed");
        // LinkRequest untouched: the inbound entry must SURVIVE the sweep as a
        // LinkRequest (peek_offer is None for it by design either way, so it can't
        // catch a regression in the `LinkRequest => true` retain arm — check list()).
        let after = store.list();
        assert!(
            after
                .iter()
                .any(|(a, p)| *a == link && matches!(p.kind, PendingKind::LinkRequest)),
            "the LinkRequest entry must survive the sweep"
        );
    }
}
