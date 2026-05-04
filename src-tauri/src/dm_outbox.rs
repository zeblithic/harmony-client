//! DM/group-DM outbox orchestrator (ZEB-216 Sub-B Phase 2).
//!
//! Implements the spec at
//! `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md`
//! §"Module structure / dm_outbox.rs".
//!
//! Phase 2 ships:
//!   - `DmTransport` trait with an in-process `StubTransport` for tests.
//!   - `DmOutbox` orchestrator: `send_dm`, `drain`, `handle_ack`.
//!   - Wall-clock-driven 30-day expiration + per-recipient exponential backoff.
//!
//! Phase 3b will:
//!   - Replace `StubTransport` with a real harmony-runtime adapter that
//!     emits `RuntimeAction::SendUnicastToDevice` per resolved device hash.
//!   - Add `handle_unicast` for inbound `DmInvite`/`DmCidNotify`/`DmAck`
//!     demux (which routes `DmAck` packets through `handle_ack`).

use crate::content_store::{ContentStore, ContentStoreError};
use crate::dm_crypto::{compute_aad, encrypt_dm_message, DmEncryptError};
use crate::dm_envelope::MessagePayload;
use crate::owner_state_crdt::{ApplyOutcome, OwnerState, RejectionReason};
use crate::owner_state_types::{
    DeliveryStatus, Hlc, OutboxEntry, OutboxEntryId, OwnerAddr, SpaceId, SpaceKind,
};
use async_trait::async_trait;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Mutex;

pub type MessageId = OutboxEntryId;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("transport temporarily unavailable: {0}")]
    Transient(String),
    #[error("transport permanently failed: {0}")]
    Permanent(String),
}

#[async_trait]
pub trait DmTransport: Send + Sync {
    async fn send(&self, entry: &OutboxEntry, recipient: OwnerAddr) -> Result<(), TransportError>;
}

/// In-process transport for Phase 2 tests + the in-process Tauri integration
/// test harness. Records every send call so tests can assert on them, and lets
/// the test pre-seed an outcome (Ok or Transient/Permanent error) per
/// (entry_id, recipient) pair.
#[derive(Default)]
pub struct StubTransport {
    inner: Mutex<StubInner>,
}

#[derive(Default)]
struct StubInner {
    /// Bounded ring buffer of recorded sends. `StubTransport` is wired into
    /// `start_node` as the production Phase 2 transport, so a long-lived node
    /// would otherwise accumulate one entry per send call forever. Capped at
    /// `STUB_MAX_RECORDED_SENDS` (~32KB worst case at 32B/entry × 1024); on
    /// overflow the oldest entry is `pop_front`ed before `push_back`. No test
    /// asserts a `sends` count above ~10, so the cap is non-disruptive.
    sends: VecDeque<(OutboxEntryId, OwnerAddr)>,
    /// Pre-seeded outcomes; if absent, default = Ok(()).
    outcomes: HashMap<(OutboxEntryId, OwnerAddr), Result<(), TransportError>>,
}

impl StubTransport {
    /// FIFO cap on `StubInner::sends` to keep the production stub bounded.
    const STUB_MAX_RECORDED_SENDS: usize = 1024;

    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed the outcome for the next `send(entry_id, recipient)` call.
    pub fn set_outcome(
        &self,
        entry_id: OutboxEntryId,
        recipient: OwnerAddr,
        outcome: Result<(), TransportError>,
    ) {
        self.inner
            .lock()
            .expect("StubTransport poisoned")
            .outcomes
            .insert((entry_id, recipient), outcome);
    }

    /// Snapshot all recorded sends (in call order, oldest first).
    pub fn sends(&self) -> Vec<(OutboxEntryId, OwnerAddr)> {
        self.inner
            .lock()
            .expect("StubTransport poisoned")
            .sends
            .iter()
            .copied()
            .collect()
    }
}

// `TransportError` is not Clone (thiserror + io-style errors rarely are).
// `remove` instead of `get/clone` so each pre-seeded outcome fires once;
// repeat calls without re-seeding fall through to the default Ok(()).
#[async_trait]
impl DmTransport for StubTransport {
    async fn send(&self, entry: &OutboxEntry, recipient: OwnerAddr) -> Result<(), TransportError> {
        let mut inner = self.inner.lock().expect("StubTransport poisoned");
        if inner.sends.len() >= Self::STUB_MAX_RECORDED_SENDS {
            inner.sends.pop_front();
        }
        inner.sends.push_back((entry.id, recipient));
        inner
            .outcomes
            .remove(&(entry.id, recipient))
            .unwrap_or(Ok(()))
    }
}

#[derive(Debug, Clone, Copy)]
struct AttemptState {
    last_attempt_wall_ms: u64,
    failure_count: u32,
}

const BACKOFF_BASE_MS: u64 = 5_000; // 5s
const BACKOFF_MULTIPLIER: u64 = 2;
const BACKOFF_CAP_MS: u64 = 5 * 60 * 1_000; // 5 min
const BACKOFF_MAX_EXPONENT: u32 = 8; // 5s * 2^8 = 1280s -> capped at 5min
pub const EXPIRATION_MS: u64 = 30 * 24 * 60 * 60 * 1_000; // 30 days

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    /// (entry_id, recipient) pairs whose `delivered_to` was just set this tick.
    /// Phase 2 stub never produces these (acks come via separate handle_ack
    /// calls); Phase 3b will populate when handle_unicast's DmAck arm dispatches
    /// through the same path. Caller emits `dm-delivered` IPC events.
    pub newly_delivered: Vec<(OutboxEntryId, OwnerAddr)>,
    /// Entries that transitioned to Expired this tick.
    pub newly_expired: Vec<OutboxEntryId>,
}

/// Per-process DM-outbox state. One instance per running node, shared between
/// the IPC handler (writes via `send_dm`) and the event-loop drain tick.
///
/// `OwnerState` is held in a separate `Arc<tokio::sync::Mutex<OwnerState>>`
/// (constructed in `start_node`) and passed in by callers that have just
/// acquired its lock. This `DmOutbox` owns only ephemeral per-process state
/// (in-flight set, backoff timestamps); CRDT state lives in `OwnerState`.
pub struct DmOutbox {
    pub(crate) device_id: String,
    pub(crate) self_owner: OwnerAddr,
    in_flight: HashSet<(OutboxEntryId, OwnerAddr)>,
    backoff: HashMap<(OutboxEntryId, OwnerAddr), AttemptState>,
}

impl DmOutbox {
    pub fn new(device_id: String, self_owner: OwnerAddr) -> Self {
        Self {
            device_id,
            self_owner,
            in_flight: HashSet::new(),
            backoff: HashMap::new(),
        }
    }

    /// Encrypt `content` under `Space.content_key`, write the storage blob to
    /// CAS, mint a fresh OutboxEntry, install it. Returns the new MessageId.
    /// Drain (next tick) attempts delivery; this call returns immediately.
    ///
    /// `wall_now_ms` and `prev_hlc` are passed in (not derived) so tests can
    /// drive deterministic HLCs and so the IPC handler can keep the per-device
    /// HLC monotone via the existing SyncEngine HLC tracker.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_dm(
        &mut self,
        state: &mut OwnerState,
        cas: &dyn ContentStore,
        space_id: SpaceId,
        content: Vec<u8>,
        mime_type: String,
        wall_now_ms: u64,
        prev_hlc: Option<&Hlc>,
    ) -> Result<MessageId, SendDmError> {
        // 1. Look up Space, check kind + content_key.
        let space = state
            .spaces
            .get(&space_id)
            .ok_or(SendDmError::UnknownSpace(space_id))?;
        match space.kind {
            SpaceKind::Dm | SpaceKind::GroupDm => {}
            SpaceKind::Folder => return Err(SendDmError::InvalidSpaceKind(space_id, "Folder")),
            SpaceKind::Community => {
                return Err(SendDmError::InvalidSpaceKind(space_id, "Community"))
            }
            SpaceKind::Channel => return Err(SendDmError::InvalidSpaceKind(space_id, "Channel")),
            SpaceKind::PublicChannel => {
                return Err(SendDmError::InvalidSpaceKind(space_id, "PublicChannel"))
            }
        }

        let content_key = space
            .content_key
            .as_ref()
            .ok_or(SendDmError::MissingContentKey(space_id))?;

        // 2. Derive recipient_owners — exclude self, dedup, sort.
        let recipients = derive_recipients(&space.members, &self.self_owner);
        // Reject self-only DMs up front. Without this we'd mint an
        // OutboxEntry with `recipient_owners: vec![]`, which drain() never
        // sends to anyone AND which the expiration sweep would mark
        // Complete (vacuous all-acked truth) instead of Expired — so the
        // entry sits forever doing nothing.
        if recipients.is_empty() {
            return Err(SendDmError::NoRecipients(space_id));
        }

        // 3. Build MessagePayload + HLC stamp.
        let sent_at = next_hlc(prev_hlc, wall_now_ms, &self.device_id);
        let payload = MessagePayload {
            body: content,
            mime_type,
            sender: self.self_owner,
            sent_at: sent_at.clone(),
        };

        // 4. Encrypt under (content_key, AAD = canonical_cbor(dedupe_key)).
        let aad =
            compute_aad(space).map_err(|e| SendDmError::Encode(format!("compute_aad: {e}")))?;
        let storage_blob = encrypt_dm_message(content_key, &aad, &payload)?;

        // 5. Compute message_cid + write to CAS. Mirror publish_root_now's
        //    EncryptedDurable flag pair: encrypted=true, ephemeral=false
        //    (default). DM bodies should never auto-burn from the
        //    StorageTier — they're chat history.
        let message_cid = harmony_content::cid::ContentId::for_book(
            &storage_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .map_err(|e| SendDmError::Encode(format!("ContentId::for_book: {e}")))?;
        cas.put(message_cid, storage_blob).await?;

        // 6. Mint OutboxEntry, install via apply_outbox.
        let entry_id = OutboxEntryId(ulid::Ulid::new().to_bytes());
        let entry = OutboxEntry {
            id: entry_id,
            space_id,
            recipient_owners: recipients,
            message_cid,
            created_at: sent_at,
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };
        match state.apply_outbox(entry) {
            ApplyOutcome::Inserted => Ok(entry_id),
            ApplyOutcome::Merged { .. } => {
                // Should not happen — fresh ULID can't collide with any existing entry.
                Ok(entry_id)
            }
            ApplyOutcome::Rejected(r) => Err(SendDmError::CrdtRejected(r)),
        }
    }

    /// Mark `recipient` as delivered for `entry_id`. Idempotent.
    /// Returns true iff this call mutated `delivered_to` (i.e., recipient
    /// was not already present). Caller emits `dm-delivered` IPC event
    /// only on `true`.
    ///
    /// Drops with telemetry on:
    ///   - unknown entry_id (likely stale ack from before app restart)
    ///   - recipient not in entry.recipient_owners (forged ack)
    ///
    /// Both mismatches log at warn level; neither mutates state.
    pub fn handle_ack(
        &mut self,
        state: &mut OwnerState,
        entry_id: OutboxEntryId,
        recipient: OwnerAddr,
    ) -> bool {
        let Some(entry) = state.outbox.get_mut(&entry_id) else {
            tracing::warn!(?entry_id, ?recipient, "DmAck dropped: unknown entry");
            return false;
        };
        if !entry.recipient_owners.contains(&recipient) {
            tracing::warn!(
                ?entry_id,
                ?recipient,
                "DmAck dropped: recipient not in entry.recipient_owners (forged ack)"
            );
            return false;
        }
        let inserted = entry.delivered_to.insert(recipient);
        if inserted {
            // Re-derive status. is_expired=false because handle_ack is the
            // happy-path mutation; expiration is owned by drain's wall-clock
            // sweep. If drain has already marked Expired, compute_status
            // will preserve Expired only when (a) is_expired is passed true
            // — so we must check the current state to keep Expired sticky.
            let was_expired = matches!(entry.delivery_status, DeliveryStatus::Expired);
            entry.delivery_status = entry.compute_status(was_expired);
            // Clear in-flight + backoff for this (entry, recipient) so a
            // subsequent drain doesn't re-attempt a now-completed delivery.
            self.in_flight.remove(&(entry_id, recipient));
            self.backoff.remove(&(entry_id, recipient));
        }
        inserted
    }

    /// Single drain pass. Walks every Pending/Partial entry; per outstanding
    /// recipient (in `recipient_owners` ∖ `delivered_to`):
    ///   - skip if in `in_flight` set already
    ///   - skip if backoff says next attempt is in the future
    ///   - else mark in-flight, call transport.send().
    ///     - Ok(()): clear in-flight, install AttemptState{failure_count: 1}
    ///       so the next attempt waits the base backoff (5s) for an ack
    ///       before re-sending. handle_ack clears the entry on real ack;
    ///       drain's epilogue clears it on Complete-via-CRDT-merge.
    ///     - Err(_): clear in-flight, bump backoff failure_count + record
    ///       last_attempt_wall_ms (exponential escalation up to 5min cap).
    ///
    /// Then sweep for expiration: any Pending/Partial entry where
    /// `wall_now_ms - created_at.wall_ms >= EXPIRATION_MS` and not all
    /// recipients in delivered_to → mark Expired, record in newly_expired.
    ///
    /// Epilogue: drop backoff/in_flight entries for any OutboxEntry that's
    /// no longer Pending/Partial — covers Complete via local handle_ack,
    /// Complete via CRDT-merge replication of a peer's ack, and Expired.
    pub async fn drain(
        &mut self,
        state: &mut OwnerState,
        transport: &dyn DmTransport,
        wall_now_ms: u64,
    ) -> DrainOutcome {
        let mut outcome = DrainOutcome::default();

        // 1. Collect work units up-front to avoid holding a borrow on `state`
        //    across the await boundary. Entries already past EXPIRATION_MS are
        //    skipped here — they get marked Expired in the sweep below without
        //    a final wasted transport.send attempt.
        let work: Vec<(OutboxEntryId, OutboxEntry, Vec<OwnerAddr>)> = state
            .outbox
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e.delivery_status,
                    DeliveryStatus::Pending | DeliveryStatus::Partial
                )
            })
            .filter(|(_, e)| wall_now_ms.saturating_sub(e.created_at.wall_ms) < EXPIRATION_MS)
            .map(|(id, e)| {
                let outstanding: Vec<OwnerAddr> = e
                    .recipient_owners
                    .iter()
                    .copied()
                    .filter(|r| !e.delivered_to.contains(r))
                    .collect();
                (*id, e.clone(), outstanding)
            })
            .collect();

        // 2. Per-(entry, recipient) attempt.
        for (entry_id, entry_clone, outstanding) in work {
            for recipient in outstanding {
                if self.in_flight.contains(&(entry_id, recipient)) {
                    continue;
                }
                if !self.is_due(entry_id, recipient, wall_now_ms) {
                    continue;
                }
                self.in_flight.insert((entry_id, recipient));
                let result = transport.send(&entry_clone, recipient).await;
                self.in_flight.remove(&(entry_id, recipient));
                match result {
                    Ok(()) => {
                        // Throttle post-Ok retries until the ack arrives.
                        // Without this, `is_due` returns true on the very next
                        // 250ms tick (no backoff entry → first attempt),
                        // producing tick-rate retry until handle_ack fires —
                        // ~4 sends/sec/recipient against the production
                        // StubTransport (which always returns Ok and has an
                        // unbounded sends Vec). Treat "sent but ack pending"
                        // as failure_count=1 so the existing exponential
                        // backoff applies (5s base × 2^(n-1), 5min cap).
                        // First post-Ok retry waits 5s; if still no ack the
                        // next waits 10s, etc. The 30-day expiration sweep
                        // is the eventual terminator.
                        self.backoff.insert(
                            (entry_id, recipient),
                            AttemptState {
                                last_attempt_wall_ms: wall_now_ms,
                                failure_count: 1,
                            },
                        );
                    }
                    Err(e) => {
                        tracing::warn!(?entry_id, ?recipient, error = %e, "transport.send failed; bumping backoff");
                        let st =
                            self.backoff
                                .entry((entry_id, recipient))
                                .or_insert(AttemptState {
                                    last_attempt_wall_ms: 0,
                                    failure_count: 0,
                                });
                        st.last_attempt_wall_ms = wall_now_ms;
                        st.failure_count = st.failure_count.saturating_add(1);
                    }
                }
            }
        }

        // 3. Expiration sweep.
        let mut expired: Vec<OutboxEntryId> = Vec::new();
        for (id, entry) in state.outbox.iter_mut() {
            if !matches!(
                entry.delivery_status,
                DeliveryStatus::Pending | DeliveryStatus::Partial
            ) {
                continue;
            }
            let age = wall_now_ms.saturating_sub(entry.created_at.wall_ms);
            if age >= EXPIRATION_MS {
                let recipient_set: BTreeSet<&OwnerAddr> = entry.recipient_owners.iter().collect();
                let all_acked = recipient_set
                    .iter()
                    .all(|r| entry.delivered_to.contains(*r));
                if !all_acked {
                    entry.delivery_status = DeliveryStatus::Expired;
                    expired.push(*id);
                }
            }
        }
        // 4. Cleanup backoff/in_flight for entries no longer Pending/Partial.
        // Covers expired (just marked above), Complete via local handle_ack
        // (already cleaned in handle_ack but defensive double-cleanup is
        // cheap), AND Complete via CRDT merge (a peer device's ack
        // replicated through owner-state sync — handle_ack never fires for
        // that path so the previous narrow expired-only sweep leaked
        // forever). Entries whose underlying OutboxEntry is gone
        // (shouldn't happen in Phase 2; defensive) are also cleaned.
        self.backoff.retain(|(entry_id, _), _| {
            state
                .outbox
                .get(entry_id)
                .map(|e| {
                    matches!(
                        e.delivery_status,
                        DeliveryStatus::Pending | DeliveryStatus::Partial
                    )
                })
                .unwrap_or(false)
        });
        self.in_flight.retain(|(entry_id, _)| {
            state
                .outbox
                .get(entry_id)
                .map(|e| {
                    matches!(
                        e.delivery_status,
                        DeliveryStatus::Pending | DeliveryStatus::Partial
                    )
                })
                .unwrap_or(false)
        });
        outcome.newly_expired = expired;
        outcome
    }

    fn is_due(&self, entry_id: OutboxEntryId, recipient: OwnerAddr, wall_now_ms: u64) -> bool {
        match self.backoff.get(&(entry_id, recipient)) {
            None => true, // first attempt
            Some(st) => {
                let exponent = st.failure_count.saturating_sub(1).min(BACKOFF_MAX_EXPONENT);
                let raw =
                    BACKOFF_BASE_MS.saturating_mul(BACKOFF_MULTIPLIER.saturating_pow(exponent));
                let delay = raw.min(BACKOFF_CAP_MS);
                wall_now_ms >= st.last_attempt_wall_ms.saturating_add(delay)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn backoff_len(&self) -> usize {
        self.backoff.len()
    }

    #[cfg(test)]
    pub(crate) fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendDmError {
    #[error("space {0:?} not found")]
    UnknownSpace(SpaceId),
    #[error("space {0:?} kind {1:?} is not Dm or GroupDm")]
    InvalidSpaceKind(SpaceId, &'static str),
    #[error("space {0:?} has no content_key (DM/group-dm invariant violated)")]
    MissingContentKey(SpaceId),
    #[error("space {0:?} has no remote recipients (members contains only self)")]
    NoRecipients(SpaceId),
    #[error("encryption failed: {0}")]
    Encrypt(#[from] DmEncryptError),
    #[error("CAS write failed: {0}")]
    Cas(#[from] ContentStoreError),
    #[error("CRDT rejected outbox entry: {0:?}")]
    CrdtRejected(RejectionReason),
    #[error("encoding failed: {0}")]
    Encode(String),
}

fn derive_recipients(members: &[OwnerAddr], self_addr: &OwnerAddr) -> Vec<OwnerAddr> {
    let mut set: BTreeSet<OwnerAddr> = members.iter().copied().collect();
    set.remove(self_addr);
    set.into_iter().collect() // BTreeSet → ascending lex order, deduped
}

// Mirrors `owner_state_sync.rs:452`'s `next_hlc` helper but is duplicated
// rather than re-exported because the SyncEngine's version reaches into
// its private `tracker: BTreeMap<String, Hlc>` and we don't want
// `dm_outbox` coupling to that internal. Phase 2 acceptable; Task 6
// (IPC wiring) will pass the SyncEngine's tracker entry as `prev` to
// keep production HLCs monotone with state-root publishes. (A future
// cleanup could promote this to a shared module — out of Phase 2 scope.)
fn next_hlc(prev: Option<&Hlc>, wall_now_ms: u64, device_id: &str) -> Hlc {
    let (logical, base_wall) = match prev {
        Some(p) if p.wall_ms == wall_now_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) if p.wall_ms > wall_now_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) => (0, p.wall_ms),
        None => (0, 0),
    };
    let effective_wall = std::cmp::max(wall_now_ms, base_wall);
    Hlc {
        wall_ms: effective_wall,
        logical,
        device_id: device_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_types::{ContentId, DmContentKey, Space, TransportBinding};

    fn entry(id: u8) -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([id; 16]),
            space_id: SpaceId([1u8; 16]),
            recipient_owners: vec![OwnerAddr([2u8; 16])],
            message_cid: ContentId::from_bytes([3u8; 32]),
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "test".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    /// Build a minimal-but-valid DM Space. Members must be sorted ascending
    /// (DM invariant), transport must be Reticulum (DM invariant), content_key
    /// must be Some (DM invariant). Tests that want a different kind must reset
    /// these fields after calling.
    fn make_dm_space(id_byte: u8, members: Vec<OwnerAddr>) -> Space {
        Space {
            id: SpaceId([id_byte; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "Bob".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members,
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "dev".into(),
            },
            updated_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "dev".into(),
            },
            content_key: Some(DmContentKey::new([0x42u8; 32])),
            prior_content_keys: vec![],
        }
    }

    fn install_space(state: &mut OwnerState, sp: Space) {
        let outcome = state.apply_space_with_canonicalization(sp);
        assert!(
            matches!(outcome, ApplyOutcome::Inserted),
            "fixture install must succeed, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn stub_records_sends_and_returns_default_ok() {
        let t = StubTransport::new();
        let e = entry(1);
        let r = OwnerAddr([2u8; 16]);
        let res = t.send(&e, r).await;
        assert!(res.is_ok(), "default outcome is Ok: {res:?}");
        assert_eq!(t.sends(), vec![(e.id, r)]);
    }

    #[tokio::test]
    async fn stub_transport_caps_recorded_sends_at_max() {
        // StubTransport is wired into start_node as the production Phase 2
        // transport. Without the FIFO cap on `sends`, a long-lived node would
        // accumulate one entry per send call forever (~32 bytes each). Verify
        // the bound holds: push 2000, expect exactly STUB_MAX_RECORDED_SENDS
        // retained, oldest evicted (FIFO).
        let t = StubTransport::new();
        let e = entry(1);
        let r = OwnerAddr([2u8; 16]);
        for _ in 0..2000 {
            let _ = t.send(&e, r).await;
        }
        assert_eq!(
            t.sends().len(),
            StubTransport::STUB_MAX_RECORDED_SENDS,
            "ring buffer must cap at STUB_MAX_RECORDED_SENDS"
        );
    }

    #[test]
    fn dm_outbox_constructs_with_empty_state() {
        let o = DmOutbox::new("dev".into(), OwnerAddr([0xaa; 16]));
        assert_eq!(o.device_id, "dev");
        assert_eq!(o.self_owner, OwnerAddr([0xaa; 16]));
        assert!(o.in_flight.is_empty());
        assert!(o.backoff.is_empty());
    }

    #[tokio::test]
    async fn send_dm_creates_outbox_entry() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = DmOutbox::new("dev".into(), alice);
        let msg_id = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"hello".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .expect("send_dm ok");

        let stored = state.outbox.get(&msg_id).expect("entry installed");
        assert_eq!(stored.space_id, space_id);
        assert_eq!(stored.recipient_owners, vec![bob], "Alice excluded");
        assert!(stored.delivered_to.is_empty());
        assert!(matches!(stored.delivery_status, DeliveryStatus::Pending));
    }

    #[tokio::test]
    async fn send_dm_invalid_space_kind_rejects() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let mut sp = make_dm_space(7, vec![alice, OwnerAddr([0x02; 16])]);
        // Mutate to a Folder Space — this is the kind that send_dm must reject.
        // Folder invariant requires transport=None, members=[], content_key=None
        // (and no prior_content_keys). Reset all four together so the fixture
        // installs cleanly.
        sp.kind = SpaceKind::Folder;
        sp.transport = None;
        sp.content_key = None;
        sp.members = vec![];
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = DmOutbox::new("dev".into(), alice);
        let err = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"x".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SendDmError::InvalidSpaceKind(_, "Folder")));
    }

    #[tokio::test]
    async fn send_dm_unknown_space_rejects() {
        let mut state = OwnerState::default();
        let cas = InMemoryStub::default();
        let mut o = DmOutbox::new("dev".into(), OwnerAddr([0x01; 16]));
        let err = o
            .send_dm(
                &mut state,
                &cas,
                SpaceId([0x99; 16]),
                b"x".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SendDmError::UnknownSpace(_)));
    }

    fn install_outbox_entry(state: &mut OwnerState, entry: OutboxEntry) {
        match state.apply_outbox(entry) {
            ApplyOutcome::Inserted => {}
            other => panic!("expected Inserted, got {other:?}"),
        }
    }

    fn outbox_entry_with_recipients(id: u8, recipients: Vec<OwnerAddr>) -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([id; 16]),
            space_id: SpaceId([1u8; 16]),
            recipient_owners: recipients,
            message_cid: ContentId::from_bytes([3u8; 32]),
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "dev".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    #[test]
    fn handle_ack_updates_delivered_to() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = outbox_entry_with_recipients(7, vec![bob]);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let mut o = DmOutbox::new("dev".into(), alice);
        let inserted = o.handle_ack(&mut state, entry_id, bob);

        assert!(inserted, "first ack inserts");
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(stored.delivered_to.contains(&bob));
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    #[test]
    fn handle_ack_duplicate_is_idempotent() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = outbox_entry_with_recipients(7, vec![bob]);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let mut o = DmOutbox::new("dev".into(), alice);
        let first = o.handle_ack(&mut state, entry_id, bob);
        let second = o.handle_ack(&mut state, entry_id, bob);

        assert!(first);
        assert!(!second, "duplicate ack returns false");
        let stored = state.outbox.get(&entry_id).unwrap();
        assert_eq!(stored.delivered_to.len(), 1);
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    fn entry_with_age(id: u8, recipients: Vec<OwnerAddr>, created_wall_ms: u64) -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([id; 16]),
            space_id: SpaceId([1u8; 16]),
            recipient_owners: recipients,
            message_cid: ContentId::from_bytes([3u8; 32]),
            created_at: Hlc {
                wall_ms: created_wall_ms,
                logical: 0,
                device_id: "dev".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    #[tokio::test]
    async fn drain_advances_pending_to_complete_on_stub_success() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = DmOutbox::new("dev".into(), alice);
        let outcome = o.drain(&mut state, &transport, 2_000).await;

        assert!(
            outcome.newly_delivered.is_empty(),
            "stub send is Ok but ack hasn't arrived; status stays Pending"
        );
        assert_eq!(transport.sends(), vec![(entry_id, bob)]);
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(matches!(stored.delivery_status, DeliveryStatus::Pending));

        // Now simulate the ack arriving (Phase 3b will route this from
        // handle_unicast's DmAck arm; Phase 2 callers do it directly).
        let inserted = o.handle_ack(&mut state, entry_id, bob);
        assert!(inserted);
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    #[tokio::test]
    async fn drain_partial_state_some_recipients_acked() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let carol = OwnerAddr([0xcc; 16]);
        let dave = OwnerAddr([0xdd; 16]);
        let mut entry = entry_with_age(7, vec![bob, carol, dave], 1_000);
        entry.delivered_to.insert(bob);
        entry.delivered_to.insert(carol);
        entry.delivery_status = DeliveryStatus::Partial;
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = DmOutbox::new("dev".into(), alice);
        let _ = o.drain(&mut state, &transport, 2_000).await;

        // Only dave is outstanding.
        assert_eq!(transport.sends(), vec![(entry_id, dave)]);
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(matches!(stored.delivery_status, DeliveryStatus::Partial));
    }

    #[tokio::test]
    async fn drain_respects_backoff_skipping_recently_attempted() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        // Pre-seed the first send to fail Transient so backoff is engaged.
        transport.set_outcome(
            entry_id,
            bob,
            Err(TransportError::Transient("net down".into())),
        );

        let mut o = DmOutbox::new("dev".into(), alice);
        let _ = o.drain(&mut state, &transport, 10_000).await;
        assert_eq!(
            transport.sends(),
            vec![(entry_id, bob)],
            "first attempt fired"
        );

        // Tick again 1s later — should be skipped (backoff = 5s base).
        let _ = o.drain(&mut state, &transport, 11_000).await;
        assert_eq!(
            transport.sends().len(),
            1,
            "second attempt skipped by backoff"
        );

        // Tick at 16s — past 5s base; should fire.
        let _ = o.drain(&mut state, &transport, 16_000).await;
        assert_eq!(
            transport.sends().len(),
            2,
            "third attempt fired after backoff"
        );
    }

    #[tokio::test]
    async fn drain_expires_30day_old_entry() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = DmOutbox::new("dev".into(), alice);
        // wall_now = created + 30 days + 1s
        let wall_now = 1_000 + EXPIRATION_MS + 1_000;
        let outcome = o.drain(&mut state, &transport, wall_now).await;

        assert_eq!(outcome.newly_expired, vec![entry_id]);
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(matches!(stored.delivery_status, DeliveryStatus::Expired));
        assert!(
            transport.sends().is_empty(),
            "expired entry should not be re-attempted"
        );
    }

    #[tokio::test]
    async fn drain_complete_entry_is_no_op() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let mut entry = entry_with_age(7, vec![bob], 1_000);
        entry.delivered_to.insert(bob);
        entry.delivery_status = DeliveryStatus::Complete;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = DmOutbox::new("dev".into(), alice);
        let outcome = o.drain(&mut state, &transport, 2_000).await;

        assert!(outcome.newly_delivered.is_empty());
        assert!(outcome.newly_expired.is_empty());
        assert!(transport.sends().is_empty());
    }

    #[tokio::test]
    async fn drain_in_flight_set_prevents_duplicate_send_within_tick() {
        // Repeat-call drain in a tight pair: first call records the entry as
        // in-flight (the stub's Ok response normally flushes in_flight before
        // returning, but we hold an outstanding fake "no-result-yet" by
        // pre-seeding two recipients on one entry and inspecting the stub
        // sends() vector for duplicates — i.e., one drain call must not send
        // the same (entry, recipient) twice).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let carol = OwnerAddr([0xcc; 16]);
        let entry = entry_with_age(7, vec![bob, carol], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = DmOutbox::new("dev".into(), alice);
        let _ = o.drain(&mut state, &transport, 2_000).await;

        let sends = transport.sends();
        let unique: HashSet<(OutboxEntryId, OwnerAddr)> = sends.iter().copied().collect();
        assert_eq!(
            sends.len(),
            unique.len(),
            "no duplicate (entry, recipient) sends in one tick"
        );
        assert_eq!(unique.len(), 2, "exactly one send per recipient");
        let _ = entry_id;
    }

    #[tokio::test]
    async fn drain_throttles_post_ok_send_until_backoff_elapses() {
        // Fix A regression: the prior `Ok(()) => self.backoff.remove(...)`
        // branch let `is_due` return true on the very next 250ms tick,
        // producing tick-rate retry until handle_ack arrived. Verify the
        // post-Ok throttle: install entry, drain at t=0 (1 send), drain
        // 1s later (no new send — under 5s base), drain 6s later (one
        // more send — past 5s base).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 0);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = DmOutbox::new("dev".into(), alice);

        let _ = o.drain(&mut state, &transport, 0).await;
        assert_eq!(transport.sends().len(), 1, "first attempt fires at t=0");

        let _ = o.drain(&mut state, &transport, 1_000).await;
        assert_eq!(
            transport.sends().len(),
            1,
            "second attempt at t=1s skipped — under 5s base backoff"
        );

        let _ = o.drain(&mut state, &transport, 6_000).await;
        assert_eq!(
            transport.sends().len(),
            2,
            "third attempt at t=6s fires — past 5s base backoff"
        );
        let _ = entry_id;
    }

    #[tokio::test]
    async fn drain_cleans_backoff_for_complete_via_crdt_merge() {
        // Fix C regression: an entry can transition Pending → Complete
        // via CRDT replication (another device acks, owner-state sync
        // merges the OutboxEntry with delivered_to populated). In that
        // path handle_ack is never called locally, so the prior
        // expired-only cleanup leaked the (entry, recipient) backoff
        // and in_flight entries forever. Verify the broader sweep cleans
        // them after a CRDT-merge completion.
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = DmOutbox::new("dev".into(), alice);

        let _ = o.drain(&mut state, &transport, 2_000).await;
        assert_eq!(transport.sends().len(), 1);
        assert_eq!(
            o.backoff_len(),
            1,
            "post-Ok throttle inserted backoff entry (Fix A)"
        );

        // Simulate a peer device's ack replicating through CRDT merge:
        // mutate delivered_to + delivery_status directly (NOT via
        // handle_ack — that path already cleans up).
        {
            let stored = state.outbox.get_mut(&entry_id).unwrap();
            stored.delivered_to.insert(bob);
            stored.delivery_status = DeliveryStatus::Complete;
        }

        let _ = o.drain(&mut state, &transport, 10_000).await;
        assert_eq!(
            o.backoff_len(),
            0,
            "drain cleaned backoff for Complete-via-CRDT entry"
        );
        assert_eq!(
            o.in_flight_len(),
            0,
            "drain cleaned in_flight for Complete-via-CRDT entry"
        );
        assert_eq!(
            transport.sends().len(),
            1,
            "no further sends — entry is Complete"
        );
    }

    #[tokio::test]
    async fn send_dm_self_only_dm_rejects() {
        // Fix D regression: a Space whose members reduces (via
        // `derive_recipients`'s self-exclusion) to an empty list would
        // have minted an OutboxEntry with `recipient_owners: []`, which
        // drain never sent and the expiration sweep would mark Complete
        // via vacuous all-acked truth (`all(|r| ...)` over empty set).
        //
        // The DM invariant in `Space::canonical_invariants` forbids
        // single-member spaces, so we bypass canonicalization by
        // inserting directly into `state.spaces`. This mirrors the
        // shape of a Space that's been corrupted or where `self_owner`
        // is the only remaining valid member (defensive fallback).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let mut sp = make_dm_space(7, vec![alice, OwnerAddr([0x02; 16])]);
        // Mutate to single-member after construction; insert directly to
        // skip apply_space_with_canonicalization's invariant check.
        sp.members = vec![alice];
        let space_id = sp.id;
        state.spaces.insert(space_id, sp);

        let cas = InMemoryStub::default();
        let mut o = DmOutbox::new("dev".into(), alice);
        let err = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"x".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, SendDmError::NoRecipients(id) if id == space_id),
            "expected NoRecipients, got {err:?}"
        );
    }
}
