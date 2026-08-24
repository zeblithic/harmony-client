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
    pub fn record_inbound(&self, addr: OwnerAddr, display: Option<String>, now_ms: u64) {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        if inner.approved.contains(&addr) {
            return;
        }
        inner.inbound.entry(addr).or_insert(PendingInbound {
            display,
            received_at_ms: now_ms,
            kind: PendingKind::LinkRequest,
        });
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
    inner: TtlPreAuth<OwnerAddr>,
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

/// ZEB-784 / ZEB-783: the local record of plain Path-A friend requests this user
/// sent that came back `Pending`, so they can be **retried automatically** and
/// shown in the UI while they wait.
///
/// ## Why this exists
///
/// `AddFriendOutcome::Pending`'s own documentation names the completing beat:
///
/// > The user re-invokes `add_friend_by_key` later to retry; once the target
/// > accepts, the retry's response is `Accepted`.
///
/// That retry was never automated and never surfaced anywhere, so in practice
/// nobody performed it. The natural mutual flow — A adds, B accepts, B adds,
/// A accepts — ends with two stored approvals, zero friendships, and both users
/// looking at "No friends yet", because each believes accepting was the last
/// step. Recording the request here lets the node perform the documented retry
/// on the user's behalf, and lets the UI answer "did my request go anywhere?"
/// (ZEB-783 — the sender previously had no projection of their own outbound
/// request at all).
///
/// ## Why the key is the identity pub hex, not an `OwnerAddr`
///
/// On a `Pending` reply the dialer receives **no cert and no owner id** — the
/// target is not disclosed until they accept. So an `OwnerAddr` key is simply
/// not available at record time. The identity pub hex *is*: it is the string the
/// user typed. Keying on it also keeps the security story trivial — the only
/// thing this store can ever cause is re-dialling a key the user themselves
/// entered, which is precisely what they asked for. Nothing here grants any
/// peer authority to be accepted; the acceptor's one-shot `approve` /
/// `take_approved` gate is untouched and remains the only thing that can
/// establish a friendship.
#[derive(Default)]
pub struct PendingOutboundLinks {
    inner: TtlPreAuth<String>,
    /// Serializes snapshot+encode+write in [`persist`](Self::persist) so two
    /// concurrent mutators cannot write in non-chronological order. Separate
    /// from the map lock precisely so the map is never held across I/O.
    persist_lock: Mutex<()>,
    /// Where this store persists. `None` = ephemeral (tests, and any caller with
    /// no identity dir), which degrades to pre-ZEB-784 behaviour: the retry
    /// simply does not survive a restart.
    path: Option<std::path::PathBuf>,
    /// ZEB-982: seals the file at rest. Set together with `path` — an
    /// ephemeral store has neither.
    cipher: Option<crate::device_dataset_file::DeviceCipher>,
}

/// File name for the persisted outbound-link records, alongside the other
/// per-identity state under `<identity_dir>/`.
pub const OUTBOUND_LINKS_FILENAME: &str = "outbound_friend_links.cbor";

/// How long an unanswered outbound request keeps being retried.
///
/// Much longer than [`OUTBOUND_INTRO_TTL_MS`], and the difference is the whole
/// point: an introduction pre-auth covers a machine-timescale round trip, but
/// this one has to survive **human accept latency**. The recipient may not open
/// the app until tomorrow. A 10-minute bound would give up long before the user
/// ever taps Accept, reintroducing the exact dead end this exists to remove.
/// Matched to `INTRODUCTION_OFFER_TTL_MS` (7d), already this codebase's answer
/// to "how long may a human sit on a pending friend decision".
pub const OUTBOUND_LINK_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000; // 7d

impl PendingOutboundLinks {
    /// Empty, ephemeral store (no persistence).
    pub fn new() -> Self {
        Self::default()
    }

    /// Load from `path`, binding it so every later mutation is persisted.
    ///
    /// Self-heals rather than bricking the boot: a corrupt file is quarantined
    /// aside as `.corrupt-<ms>` (bytes preserved for diagnosis) and an empty
    /// store returned, matching the `load_doc_or_recover` contract the
    /// relay-opt-in and DM-inbox stores use. Losing these records costs the
    /// automatic retry, not correctness — the user can still re-add manually,
    /// which is exactly the pre-ZEB-784 situation.
    pub fn load_or_recover(
        cipher: crate::device_dataset_file::DeviceCipher,
        path: std::path::PathBuf,
        now_ms: u64,
    ) -> Self {
        // ZEB-982: the v3 envelope sits beneath the recovery contract — an
        // AEAD failure quarantines exactly like corrupt CBOR, and the
        // immediate rewrite below re-seals a legacy plaintext file (the
        // pre-existing prune-rewrite doubles as the eager migration).
        let map = match crate::device_dataset_file::read_image(
            &cipher,
            &path,
            OUTBOUND_LINKS_FILENAME,
        ) {
            Ok(None) => HashMap::new(),
            Err(crate::device_dataset_file::ImageError::Io(e)) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "ZEB-784: outbound-link store unreadable; continuing empty");
                HashMap::new()
            }
            other => {
                // Content-corrupt: either the envelope failed (Crypto) or the
                // inner CBOR does not decode. One quarantine path for both.
                let decode_err = match other {
                    Ok(Some(image)) => match Self::decode(&image.bytes) {
                        Ok(m) => Ok(m),
                        Err(e) => Err(e),
                    },
                    Err(crate::device_dataset_file::ImageError::Crypto(e)) => Err(e),
                    _ => unreachable!("Ok(None) and Io handled above"),
                };
                match decode_err {
                Ok(m) => m,
                Err(e) => {
                    let aside = path.with_extension(format!("corrupt-{now_ms}"));
                    match std::fs::rename(&path, &aside) {
                        Ok(()) => {
                            tracing::warn!(error = %e, quarantined = %aside.display(),
                                "ZEB-784: outbound-link store corrupt; quarantined, continuing empty");
                        }
                        Err(rename_err) => {
                            // The rewrite below would overwrite `path` with the
                            // empty map and destroy the very bytes the
                            // quarantine exists to preserve. Bail out to an
                            // ephemeral store instead: this session degrades to
                            // pre-ZEB-784 behaviour (no durable retry), which is
                            // strictly better than silently shredding the
                            // evidence of whatever corrupted the file.
                            tracing::error!(error = %e, rename_error = %rename_err,
                                path = %path.display(),
                                "ZEB-784: outbound-link store corrupt AND quarantine failed; \
                                 running WITHOUT persistence so the bad bytes survive for diagnosis");
                            return Self::default();
                        }
                    }
                    HashMap::new()
                }
                }
            }
        };
        // Drop anything already past its TTL rather than carrying dead records
        // forward — they would never be retried anyway, and pruning here keeps
        // the file from growing without bound across restarts.
        let live: HashMap<String, u64> = map
            .into_iter()
            .filter(|(_, rec)| TtlPreAuth::<String>::is_live(*rec, now_ms, OUTBOUND_LINK_TTL_MS))
            .collect();
        let store = Self {
            inner: TtlPreAuth {
                inner: Mutex::new(live),
            },
            persist_lock: Mutex::new(()),
            path: Some(path),
            cipher: Some(cipher),
        };
        // Rewrite immediately so a pruned or quarantined file is reflected on
        // disk even if the user never sends another request.
        store.persist();
        store
    }

    /// CBOR `Vec<(String, u64)>` — a sequence of pairs rather than a map,
    /// matching the encoding discipline used elsewhere in this crate.
    fn decode(bytes: &[u8]) -> Result<HashMap<String, u64>, String> {
        let rows: Vec<(String, u64)> =
            ciborium::from_reader(bytes).map_err(|e| format!("cbor decode: {e}"))?;
        Ok(rows.into_iter().collect())
    }

    /// Snapshot and write, serialized against other persists — but never holding
    /// the MAP lock across filesystem I/O.
    ///
    /// The persist lock is taken BEFORE the snapshot, and that ordering is the
    /// whole point. Serializing only the write would still allow two mutators to
    /// snapshot in one order and write in the other, so an older snapshot could
    /// land last. Concretely, that loses cancels: the retry driver calls `record`
    /// on a timer (snapshot contains `k`), the user cancels (snapshot omits `k`),
    /// the cancel's write lands first, and the stale snapshot puts `k` back. The
    /// request then rehydrates on the next boot and is retried for the remaining
    /// TTL — a cancelled request coming back from the dead.
    ///
    /// Taking the persist lock first makes snapshot→encode→write one atomic unit,
    /// so the last writer's bytes always reflect the latest state.
    fn persist(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        // Recover a poisoned persist lock: a panic in a prior persist must not
        // permanently disable durability for the rest of the session.
        let _serialize = self
            .persist_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let rows: Vec<(String, u64)> = {
            let m = self
                .inner
                .inner
                .lock()
                .expect("outbound pre-auth mutex poisoned");
            m.iter().map(|(k, rec)| (k.clone(), *rec)).collect()
        };
        let mut bytes = Vec::new();
        if let Err(e) = ciborium::into_writer(&rows, &mut bytes) {
            tracing::warn!(error = %e, "ZEB-784: outbound-link encode failed; not persisted");
            return;
        }
        let Some(cipher) = self.cipher.as_ref() else {
            return; // path implies cipher; defensive
        };
        if let Err(e) =
            crate::device_dataset_file::write_image(cipher, path, OUTBOUND_LINKS_FILENAME, &bytes)
        {
            // Best-effort: the in-memory store is still correct for this
            // session, so a write failure costs durability across restart, not
            // the live retry.
            tracing::warn!(error = %e, path = %path.display(),
                "ZEB-784: outbound-link persist failed");
        }
    }

    /// Hex is case-insensitive; normalise so a request recorded as `AB…` and a
    /// later `forget`/lookup spelled `ab…` refer to the same entry.
    fn norm(identity_pub_hex: &str) -> String {
        identity_pub_hex.trim().to_ascii_lowercase()
    }

    /// Record an outbound request to `identity_pub_hex` at `now_ms`, keeping the
    /// original timestamp if a live record already exists.
    ///
    /// First-write-wins, not last: `add_friend_by_key_impl` records on EVERY
    /// `Pending`, and the retry driver goes through that same function, so
    /// refreshing here would push the deadline out on every pass and the record
    /// could never expire. It also keeps the timestamp meaning "when the user
    /// asked", which is what the ZEB-783 projection displays. A re-add after the
    /// window lapses starts a fresh record, since the expired one no longer
    /// counts as live.
    ///
    /// Persists only when the map actually changed — otherwise a pending request
    /// would rewrite the file on every retry pass, forever.
    pub fn record(&self, identity_pub_hex: &str, now_ms: u64) {
        if self
            .inner
            .record_if_absent(Self::norm(identity_pub_hex), now_ms, OUTBOUND_LINK_TTL_MS)
        {
            self.persist();
        }
    }

    /// Non-consuming: is there a live (un-expired) outbound request to this key?
    pub fn is_pending(&self, identity_pub_hex: &str, now_ms: u64) -> bool {
        self.inner
            .peek(&Self::norm(identity_pub_hex), now_ms, OUTBOUND_LINK_TTL_MS)
    }

    /// Every live outbound request as `(identity_pub_hex, recorded_at_ms)`.
    /// Expired entries are filtered out but NOT removed (this is a read).
    /// Backs both the retry driver and the sender-side UI projection.
    pub fn list(&self, now_ms: u64) -> Vec<(String, u64)> {
        self.inner.list(now_ms, OUTBOUND_LINK_TTL_MS)
    }

    /// Remove every expired record, returning how many were dropped.
    ///
    /// `list`/`is_pending` FILTER expired entries but do not remove them, because
    /// a read should not mutate. Without an explicit sweep a long-lived node
    /// accumulates dead keys in memory and re-persists them on every write, so
    /// the file only ever shrinks at boot. The retry driver calls this on each
    /// pass, which is the natural place: it already runs on a timer and already
    /// walks the live set. Mirrors `sweep_expired_offers` on the inbound store.
    ///
    /// Persists only when something was actually dropped.
    pub fn prune_expired(&self, now_ms: u64) -> usize {
        let dropped = {
            let mut m = self
                .inner
                .inner
                .lock()
                .expect("outbound pre-auth mutex poisoned");
            let before = m.len();
            m.retain(|_, rec| TtlPreAuth::<String>::is_live(*rec, now_ms, OUTBOUND_LINK_TTL_MS));
            before - m.len()
        };
        if dropped > 0 {
            self.persist();
        }
        dropped
    }

    /// Drop the record — the link completed, or the user cancelled. Idempotent.
    pub fn forget(&self, identity_pub_hex: &str) {
        self.inner.forget(&Self::norm(identity_pub_hex));
        self.persist();
    }
}

/// Shared TTL-bounded record map behind [`PendingOutboundIntroductions`] and
/// [`PendingOutboundLinks`]. Generic over the key because the two wrappers
/// address their targets differently: an introduction pre-auth keys on the
/// authenticated `OwnerAddr` it will match an inbound request against, while an
/// outbound link request keys on the identity pub hex the user typed (no
/// `OwnerAddr` is available at record time — see `PendingOutboundLinks`).
///
/// The TTL is a per-call parameter rather than a field so each constant sits
/// next to the contract that justifies it.
struct TtlPreAuth<K: std::hash::Hash + Eq> {
    inner: Mutex<HashMap<K, u64>>,
}

// Manual `Default` — deriving it would demand `K: Default`, which the key types
// need not (and should not) implement.
impl<K: std::hash::Hash + Eq> Default for TtlPreAuth<K> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: std::hash::Hash + Eq + Clone> TtlPreAuth<K> {
    fn record(&self, target: K, now_ms: u64) {
        self.inner
            .lock()
            .expect("outbound pre-auth mutex poisoned")
            .insert(target, now_ms);
    }

    /// Record `target` only if it has no LIVE record already, preserving the
    /// original timestamp when it does. Returns `true` iff the map changed.
    ///
    /// This is the variant a self-retrying store needs. With plain [`record`],
    /// a caller that re-records on every retry pass refreshes the deadline
    /// every pass, so the TTL is never reached and the record becomes
    /// immortal — a TTL its own consumer keeps resetting is not a TTL. It also
    /// keeps a "requested at" timestamp meaning when the USER asked, not when
    /// the machine last retried, which is what any UI projecting it needs.
    ///
    /// An expired record is replaced (returns `true`), so a manual re-add after
    /// the window lapses correctly starts a fresh one.
    fn record_if_absent(&self, target: K, now_ms: u64, ttl_ms: u64) -> bool {
        let mut m = self.inner.lock().expect("outbound pre-auth mutex poisoned");
        match m.get(&target) {
            Some(rec) if Self::is_live(*rec, now_ms, ttl_ms) => false,
            _ => {
                m.insert(target, now_ms);
                true
            }
        }
    }

    /// `true` iff `rec` is a live record as of `now_ms`. Fails CLOSED on a
    /// BACKWARD clock: `saturating_sub` would yield `0 < ttl` when
    /// `now_ms < rec`, keeping a stale record valid indefinitely, so
    /// `now_ms >= rec` is required explicitly.
    fn is_live(rec: u64, now_ms: u64, ttl_ms: u64) -> bool {
        now_ms >= rec && now_ms - rec < ttl_ms
    }

    /// Consuming take. The entry is removed even when expired (and even on a
    /// backward clock), preserving the one-shot property.
    fn take<Q>(&self, target: &Q, now_ms: u64, ttl_ms: u64) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let mut m = self.inner.lock().expect("outbound pre-auth mutex poisoned");
        match m.remove(target) {
            Some(rec) => Self::is_live(rec, now_ms, ttl_ms),
            None => false,
        }
    }

    /// Non-consuming read.
    fn peek<Q>(&self, target: &Q, now_ms: u64, ttl_ms: u64) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let m = self.inner.lock().expect("outbound pre-auth mutex poisoned");
        m.get(target)
            .is_some_and(|rec| Self::is_live(*rec, now_ms, ttl_ms))
    }

    fn list(&self, now_ms: u64, ttl_ms: u64) -> Vec<(K, u64)> {
        let m = self.inner.lock().expect("outbound pre-auth mutex poisoned");
        m.iter()
            .filter(|(_, rec)| Self::is_live(**rec, now_ms, ttl_ms))
            .map(|(k, rec)| (k.clone(), *rec))
            .collect()
    }

    fn forget<Q>(&self, target: &Q)
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
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
        store.record_inbound(addr(1), Some("link".into()), 1_000);
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
        store.record_inbound(addr(3), Some("bob".into()), 2_000);
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
        store.record_inbound(other, None, 1);
        assert!(
            store.peek_offer(&other).is_none(),
            "a LinkRequest is not an offer"
        );
    }

    #[test]
    fn record_then_list_returns_inbound() {
        let store = PendingFriendRequests::new();
        store.record_inbound(addr(1), Some("alice".into()), 1_000);
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
        store.record_inbound(addr(2), Some("first".into()), 1_000);
        // A re-dial must NOT overwrite the original entry (display + timestamp).
        store.record_inbound(addr(2), Some("second".into()), 9_999);
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
        store.record_inbound(addr(8), Some("bob".into()), 1_000);
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
        store.record_inbound(addr(4), None, 5_000);
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
        store.record_inbound(addr(7), Some("dora".into()), 1_000);
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
        store.record_inbound(addr(6), Some("charlie".into()), 5_000);
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
        store.record_inbound(link, None, 0); // a LinkRequest, never swept

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

    // ── ZEB-784 / ZEB-783: PendingOutboundLinks ──────────────────────────────

    /// A key with an uppercase/whitespace spelling must address the SAME entry
    /// as its canonical form, or a `forget` from the UI would silently miss the
    /// record the dialer wrote and the request would keep retrying after cancel.
    #[test]
    fn outbound_links_normalise_hex_spelling() {
        let store = PendingOutboundLinks::new();
        store.record("  AB12CD  ", 1_000);
        assert!(store.is_pending("ab12cd", 1_000));
        store.forget("Ab12Cd");
        assert!(!store.is_pending("ab12cd", 1_000));
    }

    /// The bug this store would have shipped with: the retry driver goes through
    /// `add_friend_by_key_impl`, which records on every `Pending`. If `record`
    /// refreshed the deadline, each pass would push expiry out by a full TTL and
    /// the entry could never age out — an immortal record retried forever.
    #[test]
    fn outbound_link_record_is_first_write_wins() {
        let store = PendingOutboundLinks::new();
        store.record("aa", 1_000);
        // Simulate many retry passes, each re-recording the same key.
        for pass in 1..=5u64 {
            store.record("aa", 1_000 + pass * 60_000);
        }
        let rows = store.list(1_000 + 5 * 60_000);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].1, 1_000,
            "the timestamp must stay when the USER asked, not when the machine last retried"
        );
        // And the TTL is therefore actually reachable.
        assert!(
            store.list(1_000 + OUTBOUND_LINK_TTL_MS).is_empty(),
            "a re-recorded entry must still expire on the ORIGINAL deadline"
        );
    }

    /// An expired record is replaced rather than resurrected, so a manual re-add
    /// after the window lapses gets a full fresh retry window.
    #[test]
    fn outbound_link_readd_after_expiry_starts_fresh() {
        let store = PendingOutboundLinks::new();
        store.record("bb", 1_000);
        let after = 1_000 + OUTBOUND_LINK_TTL_MS;
        assert!(store.list(after).is_empty(), "lapsed before the re-add");
        store.record("bb", after);
        let rows = store.list(after);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, after, "re-add after expiry restarts the window");
    }

    /// Boundary + backward-clock. `is_live` must fail CLOSED when the clock goes
    /// backwards: a record stamped in the future must not read as live forever.
    #[test]
    fn outbound_link_ttl_boundary_and_backward_clock() {
        let store = PendingOutboundLinks::new();
        store.record("cc", 10_000);
        assert!(store.is_pending("cc", 10_000), "same instant is live");
        assert!(
            store.is_pending("cc", 10_000 + OUTBOUND_LINK_TTL_MS - 1),
            "one ms inside the window is live"
        );
        assert!(
            !store.is_pending("cc", 10_000 + OUTBOUND_LINK_TTL_MS),
            "exactly at the TTL is expired (half-open window)"
        );
        assert!(
            !store.is_pending("cc", 9_999),
            "a backward clock must fail closed, not treat the record as live"
        );
    }

    /// Persistence round-trip: a request survives the restart it exists to
    /// survive. Records are bounded by HUMAN accept latency, so dropping them on
    /// boot would silently abandon a request the user is still waiting on.
    #[test]
    fn outbound_links_persist_and_rehydrate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(OUTBOUND_LINKS_FILENAME);

        let store = PendingOutboundLinks::load_or_recover(crate::device_dataset_file::test_cipher(), path.clone(), 1_000);
        store.record("dd", 1_000);
        store.record("ee", 2_000);
        store.forget("ee");
        drop(store);

        let rehydrated = PendingOutboundLinks::load_or_recover(crate::device_dataset_file::test_cipher(), path, 3_000);
        let rows = rehydrated.list(3_000);
        assert_eq!(rows.len(), 1, "exactly the un-forgotten record survives");
        assert_eq!(rows[0], ("dd".to_string(), 1_000));
    }

    /// Records already past their TTL are pruned at load rather than carried
    /// forward, so the file can't grow without bound across restarts.
    #[test]
    fn outbound_links_prune_expired_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(OUTBOUND_LINKS_FILENAME);

        let store = PendingOutboundLinks::load_or_recover(crate::device_dataset_file::test_cipher(), path.clone(), 1_000);
        store.record("old", 1_000);
        store.record("new", 1_000 + OUTBOUND_LINK_TTL_MS);
        drop(store);

        // Boot far enough ahead that only the second record is still live.
        let boot = 1_000 + OUTBOUND_LINK_TTL_MS + 1;
        let rehydrated = PendingOutboundLinks::load_or_recover(crate::device_dataset_file::test_cipher(), path.clone(), boot);
        let rows = rehydrated.list(boot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "new");
        drop(rehydrated);

        // The prune was written through, not just applied in memory.
        let again = PendingOutboundLinks::load_or_recover(crate::device_dataset_file::test_cipher(), path, boot);
        assert_eq!(again.list(boot).len(), 1, "prune persisted to disk");
    }

    /// A corrupt file must not brick the boot: it is quarantined aside (bytes
    /// preserved for diagnosis) and the store comes up empty. Losing these
    /// records costs the automatic retry, not correctness.
    #[test]
    fn outbound_links_quarantine_corrupt_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(OUTBOUND_LINKS_FILENAME);
        std::fs::write(&path, b"this is not cbor").expect("seed corrupt file");

        let store = PendingOutboundLinks::load_or_recover(crate::device_dataset_file::test_cipher(), path.clone(), 7_777);
        assert!(store.list(7_777).is_empty(), "corrupt file → empty store");

        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("corrupt-"))
            .collect();
        assert_eq!(
            quarantined.len(),
            1,
            "the unreadable bytes must be preserved aside, not deleted: {quarantined:?}"
        );
    }

    /// Runtime pruning: `list` only FILTERS expired entries, so without an
    /// explicit sweep a long-lived node keeps dead keys in memory and
    /// re-persists them on every write. The retry driver calls this each pass.
    #[test]
    fn outbound_links_prune_expired_at_runtime() {
        let store = PendingOutboundLinks::new();
        store.record("old", 1_000);
        store.record("new", 1_000 + OUTBOUND_LINK_TTL_MS);

        let now = 1_000 + OUTBOUND_LINK_TTL_MS + 1;
        // Before the sweep the expired key is filtered from reads but still held.
        assert_eq!(store.list(now).len(), 1, "reads already filter it");
        assert_eq!(store.prune_expired(now), 1, "one expired record dropped");
        assert_eq!(store.prune_expired(now), 0, "sweep is idempotent");
        assert_eq!(store.list(now).len(), 1);
    }

    /// The persisted file must always match the final in-memory state, however
    /// the mutations interleave.
    ///
    /// `persist` takes its lock BEFORE snapshotting for exactly this reason. If
    /// it serialized only the write, two mutators could snapshot in one order
    /// and write in the other — and the case that costs a user something real is
    /// a `record` (from the retry driver's timer) overwriting a concurrent
    /// `forget` (the user's cancel), resurrecting a cancelled request on reboot.
    #[test]
    fn outbound_links_persist_matches_memory_under_concurrency() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(OUTBOUND_LINKS_FILENAME);
        let store = Arc::new(PendingOutboundLinks::load_or_recover(crate::device_dataset_file::test_cipher(), path.clone(), 1_000));

        let mut handles = Vec::new();
        for t in 0..8u64 {
            let s = Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                for i in 0..40u64 {
                    let key = format!("k{}", (t * 40 + i) % 10);
                    if i % 2 == 0 {
                        s.record(&key, 1_000 + i);
                    } else {
                        s.forget(&key);
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("worker panicked");
        }

        // One final mutation so the last write is unambiguously ours, then the
        // file must agree with memory. A torn ordering shows up as a key on disk
        // that memory no longer has (the resurrection case) or vice versa.
        store.forget("k0");
        store.record("sentinel", 2_000);

        let mut in_memory: Vec<String> = store.list(2_000).into_iter().map(|(k, _)| k).collect();
        // ZEB-982: the file is sealed — read the inner image through the
        // envelope with the same test cipher the store was bound to.
        let on_disk_image = crate::device_dataset_file::read_image(
            &crate::device_dataset_file::test_cipher(),
            &path,
            OUTBOUND_LINKS_FILENAME,
        )
        .expect("read")
        .expect("file present");
        let on_disk_map = PendingOutboundLinks::decode(&on_disk_image.bytes).expect("decode");
        let mut on_disk: Vec<String> = on_disk_map.into_keys().collect();
        in_memory.sort();
        on_disk.sort();
        assert_eq!(
            on_disk, in_memory,
            "the persisted file must reflect the final in-memory state"
        );
        assert!(
            !on_disk.contains(&"k0".to_string()),
            "a forgotten key must not be resurrected on disk by a racing record"
        );
    }

    /// When the corrupt file cannot be moved aside, the store must NOT then
    /// overwrite it with an empty map — that would destroy the exact bytes the
    /// quarantine exists to preserve. It degrades to ephemeral instead.
    ///
    /// The rename is made to fail deterministically by pre-creating a DIRECTORY
    /// at the quarantine path (rename of a file onto a directory fails), rather
    /// than by permissions, which vary by platform and by whether tests run as
    /// root.
    #[test]
    fn outbound_links_keep_corrupt_bytes_when_quarantine_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(OUTBOUND_LINKS_FILENAME);
        let corrupt = b"this is not cbor";
        std::fs::write(&path, corrupt).expect("seed corrupt file");

        let now_ms = 4_242;
        let blocked = path.with_extension(format!("corrupt-{now_ms}"));
        std::fs::create_dir(&blocked).expect("occupy the quarantine path");

        let store = PendingOutboundLinks::load_or_recover(crate::device_dataset_file::test_cipher(), path.clone(), now_ms);
        assert!(store.list(now_ms).is_empty(), "comes up empty either way");

        assert_eq!(
            std::fs::read(&path).expect("original still readable"),
            corrupt,
            "the corrupt bytes must survive — losing them costs the diagnosis"
        );

        // And it must be ephemeral: a later write cannot clobber the evidence.
        store.record("aa", now_ms);
        assert_eq!(
            std::fs::read(&path).expect("original still readable"),
            corrupt,
            "a store that failed to quarantine must not persist over the bad file"
        );
    }

    /// A store with no bound path degrades to pre-ZEB-784 behaviour (in-memory
    /// only) instead of panicking — the path every test and any caller without an
    /// identity dir takes.
    #[test]
    fn outbound_links_without_path_are_ephemeral_not_fatal() {
        let store = PendingOutboundLinks::new();
        store.record("ff", 1_000);
        assert!(store.is_pending("ff", 1_000));
        store.forget("ff");
        assert!(!store.is_pending("ff", 1_000));
    }
}
