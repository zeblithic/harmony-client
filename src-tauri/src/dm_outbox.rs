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
    DeliveryStatus, DeviceIdentityHash, Hlc, OutboxEntry, OutboxEntryId, OwnerAddr,
    OwnerDeviceCache, SpaceId, SpaceKind,
};
use async_trait::async_trait;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

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

/// Payload pushed by `RuntimeUnicastTransport` into the event-loop's
/// outbound channel. Task 7 wires the receiver into `event_loop` which
/// translates each request into `RuntimeEvent::SendUnicastToDevice` for
/// `NodeRuntime`. Per-destination FIFO + cross-destination best-effort
/// ordering inherits from ZEB-226's runtime.
#[derive(Debug, Clone)]
pub struct UnicastSendRequest {
    pub destination_hash: [u8; 16],
    pub packet: Vec<u8>,
}

/// Strategy for mapping a recipient `OwnerAddr` to the list of 16-byte
/// destination hashes we should fan-out to. Production impl
/// (`OwnerDeviceCacheResolver`, lands in Task 11) reads
/// `OwnerDeviceCache`; tests use `StaticDestResolver` to isolate the
/// transport's mechanics from CRDT state.
///
/// May return an empty Vec — caller treats that as a transient error
/// (no known devices) so the outbox backoff drives a future retry once
/// Flow A propagates the missing OwnerDeviceCache entry.
pub trait DestinationResolver: Send + Sync {
    fn resolve(&self, recipient: OwnerAddr) -> Vec<[u8; 16]>;
}

/// Production `DmTransport` adapter (ZEB-227 Phase 3b). Per `send`:
///
/// 1. Resolve the recipient `OwnerAddr` → list of destination hashes
///    via the injected `DestinationResolver`.
/// 2. Build a `DmCidNotifySigned` whose `signing_device_hash` is our
///    device's identity hash (single-device `sender_devices` for Phase
///    3b — cross-device piggyback grows automatically as Flow A
///    propagates more entries; see spec §"Public-key storage on
///    OwnerDeviceCache").
/// 3. Sign + canonical-CBOR-encode via
///    `dm_envelope::build_signed_cidnotify` + `encode_packet`.
/// 4. Push one `UnicastSendRequest` per destination hash into `tx`,
///    which the event-loop drains and forwards to `NodeRuntime`.
///
/// `DmInvite` outbound is Phase 4's `add_space` IPC for DM kinds
/// (spec Flow 1). `DmAck` outbound is built directly by the receive-side
/// `handle_cidnotify` (Task 10) — it bypasses `DmTransport::send`
/// because acks are not tied to an `OutboxEntry` retry loop.
pub struct RuntimeUnicastTransport {
    tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
    resolver: Arc<dyn DestinationResolver>,
    self_owner: OwnerAddr,
    our_signing_device_hash: DeviceIdentityHash,
    signing_key: Arc<ed25519_dalek::SigningKey>,
}

impl RuntimeUnicastTransport {
    pub fn new(
        tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
        resolver: Arc<dyn DestinationResolver>,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        signing_key: Arc<ed25519_dalek::SigningKey>,
    ) -> Self {
        Self {
            tx,
            resolver,
            self_owner,
            our_signing_device_hash,
            signing_key,
        }
    }
}

#[async_trait]
impl DmTransport for RuntimeUnicastTransport {
    async fn send(&self, entry: &OutboxEntry, recipient: OwnerAddr) -> Result<(), TransportError> {
        let destinations = self.resolver.resolve(recipient);
        if destinations.is_empty() {
            // Empty resolver result is transient: Flow A may surface the
            // recipient's devices on the next OwnerState sync round. The
            // outbox's exponential backoff handles the retry cadence.
            return Err(TransportError::Transient(format!(
                "no known devices for recipient {recipient:?}"
            )));
        }

        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id: entry.space_id,
            message_cid: entry.message_cid,
            sender_owner_addr: self.self_owner,
            // Phase 3b: single-device sender_devices (just the signer).
            // Cross-device piggyback (sender lists ALL bound devices) is
            // a documented follow-up — see spec §"Public-key storage on
            // OwnerDeviceCache".
            sender_devices: vec![self.our_signing_device_hash],
            signing_device_hash: self.our_signing_device_hash,
        };
        let packet = crate::dm_envelope::build_signed_cidnotify(signed, &self.signing_key)
            .map_err(|e| TransportError::Permanent(format!("build_signed_cidnotify: {e}")))?;
        let wire = crate::dm_envelope::encode_packet(&packet)
            .map_err(|e| TransportError::Permanent(format!("encode_packet: {e}")))?;

        for destination_hash in destinations {
            self.tx
                .send(UnicastSendRequest {
                    destination_hash,
                    packet: wire.clone(),
                })
                .await
                .map_err(|e| {
                    TransportError::Transient(format!("event-loop channel closed: {e}"))
                })?;
        }
        Ok(())
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
    ///
    /// Phase 3b note: this is the post-verification delivery-marking
    /// primitive that Task 11's `handle_ack` (the inbound DM packet
    /// dispatcher) calls AFTER signature verification + signed-origin
    /// resolution. Phase 2 callers (drain integration tests) drive it
    /// directly because Phase 2 had no signature layer to verify against.
    pub fn mark_ack_delivered(
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
            // Re-derive status. is_expired=false because this is the
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

    /// Inbound DM packet entry point. Decodes, dispatches by discriminant.
    ///
    /// The signature verification happens INSIDE each per-discriminant
    /// handler (handle_invite uses inline pubkey from invite body;
    /// handle_cidnotify and handle_ack use `lookup_pubkey_for_device`
    /// against `OwnerDeviceCache`). Centralizing verification in
    /// `handle_unicast` would force a generic "first try inline, fallback
    /// to cache" pattern that's less expressive than per-discriminant
    /// handling.
    ///
    /// Per spec §"Application-signature binding rule", every dispatched
    /// arm uses the verified `signing_device_hash` from the packet body
    /// (NOT a payload-controlled owner field) for downstream state
    /// mutations.
    pub async fn handle_unicast(
        &mut self,
        state: &mut OwnerState,
        cas: &dyn ContentStore,
        unicast_send_tx: &tokio::sync::mpsc::Sender<UnicastSendRequest>,
        packet_bytes: Vec<u8>,
        wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        let packet = crate::dm_envelope::decode_packet(&packet_bytes)
            .map_err(|e| DmReceiveError::Decode(e.to_string()))?;

        match packet {
            crate::dm_envelope::DmPacket::Invite {
                signed,
                signature,
                signed_bytes,
            } => {
                self.handle_invite(state, signed, signature, &signed_bytes, wall_now_ms)
                    .await
            }
            crate::dm_envelope::DmPacket::CidNotify {
                signed,
                signature,
                signed_bytes,
            } => {
                self.handle_cidnotify(
                    state,
                    cas,
                    unicast_send_tx,
                    signed,
                    signature,
                    &signed_bytes,
                    wall_now_ms,
                )
                .await
            }
            crate::dm_envelope::DmPacket::Ack {
                signed,
                signature,
                signed_bytes,
            } => {
                self.handle_ack(state, signed, signature, &signed_bytes, wall_now_ms)
                    .await
            }
        }
    }

    /// Inbound `DmInvite` handler — Phase 3b auto-accept.
    ///
    /// Per ZEB-216 spec §"Application-signature binding rule":
    ///   1. Three sanity gates (cheap, run before signature verification):
    ///      - inviter ∈ members
    ///      - signing_device_hash ∈ sender_devices (defense-in-depth;
    ///        decode_packet also enforces this — the gate here catches
    ///        future regressions if decode's invariant is ever loosened)
    ///      - self_owner ∈ members
    ///   2. Verify signature using inline `inviter_identity_pub` (the
    ///      64-byte combined identity pubs — DmInvite is the bootstrap
    ///      exception, the receiver does not yet have an OwnerDeviceCache
    ///      entry for the inviter so the signing pub ships inline).
    ///   3. Auto-accept (Phase 3b ships no UI; user-driven decline UX is
    ///      deferred to Phase 4 with a follow-up Linear ticket filed at
    ///      PR-creation time per the Phase 3b spec):
    ///      - `apply_owner_device_update` with a parallel pubs vec that
    ///        has `Some(inviter_identity_pub)` at the signer's index and
    ///        `None` everywhere else. The receiver knows the inviter's
    ///        identity pub for the device that signed THIS invite, but
    ///        has no pubs for the inviter's other devices yet — they
    ///        remain pre-bootstrap until the next invite-equivalent flow.
    ///      - `apply_space_with_canonicalization` for the new DM Space,
    ///        mirroring what `dm_outbox::send_dm` builds on the outbound
    ///        side (Reticulum transport binding, Phase 1 invariants for
    ///        `content_key` etc. — `validate_invariants` runs inside
    ///        `apply_space_with_canonicalization`, so the Space MUST
    ///        satisfy the DM-kind invariants).
    ///   4. Return `DrainOutcome::default()` — no IPC events from the
    ///      bare invite (`dm-received` events are tied to incoming
    ///      messages, not invites).
    pub async fn handle_invite(
        &mut self,
        state: &mut OwnerState,
        signed: crate::dm_envelope::DmInviteSigned,
        signature: [u8; 64],
        signed_bytes: &[u8],
        _wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        // Sanity gate 1: inviter ∈ members.
        if !signed.members.contains(&signed.inviter) {
            return Err(DmReceiveError::InviterNotInMembers);
        }
        // Sanity gate 2: signing_device_hash ∈ sender_devices.
        // (decode_packet already enforces this — defense-in-depth here.)
        if !signed.sender_devices.contains(&signed.signing_device_hash) {
            return Err(DmReceiveError::SigningDeviceNotInSenderDevices);
        }
        // Sanity gate 3: self_owner ∈ members.
        if !signed.members.contains(&self.self_owner) {
            return Err(DmReceiveError::ReceiverNotInMembers);
        }
        // Verify signature using inline inviter_identity_pub (64-byte combined
        // identity pubs — verify_dm_packet_signature splits + uses Ed25519
        // half for the actual signature verification, X25519 half participates
        // only in the device-hash recomputation that defeats key-substitution).
        crate::dm_signing::verify_dm_packet_signature(
            signed_bytes,
            &signature,
            &signed.inviter_identity_pub,
            signed.signing_device_hash,
        )?;

        // Phase 3b auto-accept: write Space + cache + cached identity pub.
        // (Phase 4 will replace this with a stage-pending-invite + UI prompt
        // path; follow-up ticket filed at PR-creation time per spec.)

        // Build a parallel pubs vec: Some(inviter_identity_pub) at the signer's
        // index, None everywhere else. The receiver knows the inviter's
        // identity pub for the device that signed THIS invite, but has no pubs
        // for the inviter's other devices yet — they remain pre-bootstrap
        // until the next invite-equivalent flow.
        let mut device_identity_pubs: Vec<Option<[u8; 64]>> =
            vec![None; signed.sender_devices.len()];
        let signer_idx = signed
            .sender_devices
            .iter()
            .position(|d| *d == signed.signing_device_hash)
            .expect("sanity gate 2 already verified signing_device_hash ∈ sender_devices");
        device_identity_pubs[signer_idx] = Some(signed.inviter_identity_pub);

        let cache_outcome = state.apply_owner_device_update(
            signed.inviter,
            signed.sender_devices.clone(),
            device_identity_pubs,
            signed.created_at.clone(),
        );
        if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = cache_outcome {
            return Err(DmReceiveError::CrdtRejected(format!("{:?}", reason)));
        }

        // Build the Space from the invite. Mirror what add_space's DM/group-DM
        // handling will produce (Phase 4 will produce these on the SEND side
        // as outbound invites; here we mirror the same shape on the RECEIVE
        // side as inbound invite acceptance). Transport binding is Reticulum
        // (DM kinds always are).
        let space = crate::owner_state_types::Space {
            id: signed.space_id,
            kind: signed.kind,
            parent: None,
            community_id: None,
            name: format!("DM with {:?}", signed.inviter),
            // `participants` is a `Vec<ReticulumDest>` of opaque transport
            // identifiers; we don't have those for the inviter's owners
            // here (only their `OwnerAddr`s — the receiver-side code path
            // resolves OwnerAddr → DeviceIdentityHash via OwnerDeviceCache
            // at send time, not from `Space.transport.participants`). Leave
            // empty; matches the existing test fixture pattern.
            transport: Some(crate::owner_state_types::TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: signed.members,
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: signed.created_at.clone(),
            updated_at: signed.created_at,
            content_key: Some(signed.content_key),
            prior_content_keys: vec![],
        };
        let space_outcome = state.apply_space_with_canonicalization(space);
        if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = space_outcome {
            return Err(DmReceiveError::CrdtRejected(format!("{:?}", reason)));
        }

        Ok(DrainOutcome::default())
    }

    /// STUB — Task 10 implements.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_cidnotify(
        &mut self,
        _state: &mut OwnerState,
        _cas: &dyn ContentStore,
        _unicast_send_tx: &tokio::sync::mpsc::Sender<UnicastSendRequest>,
        _signed: crate::dm_envelope::DmCidNotifySigned,
        _signature: [u8; 64],
        _signed_bytes: &[u8],
        _wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        Err(DmReceiveError::Decode(
            "Task 10 implements handle_cidnotify".into(),
        ))
    }

    /// STUB — Task 11 implements (the Phase 3b version replaces the Phase 2
    /// `mark_ack_delivered` primitive's role as the public ack entry point;
    /// `mark_ack_delivered` is retained as the post-verification delivery-
    /// marking helper that this method will call internally).
    pub async fn handle_ack(
        &mut self,
        _state: &mut OwnerState,
        _signed: crate::dm_envelope::DmAckSigned,
        _signature: [u8; 64],
        _signed_bytes: &[u8],
        _wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        Err(DmReceiveError::Decode(
            "Task 11 implements handle_ack".into(),
        ))
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

/// Inbound-DM packet handling errors. Each variant maps to a "drop +
/// telemetry" decision in handle_unicast per ZEB-216 §"Application-
/// signature binding rule". Distinct from dm_crypto::DmReceiveError
/// which only carries the SenderImpersonation case for the encrypted-
/// payload-layer check.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmReceiveError {
    #[error("signing_device_hash not present in any OwnerDeviceCache entry")]
    UnknownSigningDevice,
    #[error("signing_device_hash claimed by multiple OwnerDeviceCache entries (corrupted state or cache-poisoning attempt)")]
    AmbiguousSigningDevice,
    #[error("no public key cached for signing_device_hash (pre-bootstrap)")]
    UnknownSigningKey,
    #[error("signature does not verify against the provided public key")]
    SignatureVerificationFailed,
    #[error("public key does not match claimed signing_device_hash (key-substitution attempt)")]
    SigningKeyDoesNotMatchDeviceHash,
    #[error("payload owner field does not match signed-origin-resolved owner")]
    OwnerFieldMismatch,
    #[error("DmInvite.inviter must be in DmInvite.members")]
    InviterNotInMembers,
    #[error("signing_device_hash must be in DmInvite.sender_devices")]
    SigningDeviceNotInSenderDevices,
    #[error("self_owner_addr must be in DmInvite.members")]
    ReceiverNotInMembers,
    #[error("ack from owner not in OutboxEntry.recipient_owners")]
    AckFromNonRecipient,
    #[error("OutboxEntry not found for (space_id, message_cid)")]
    OutboxEntryNotFound,
    #[error("Space not found for incoming DmCidNotify (we are not a member?)")]
    SpaceNotFound,
    #[error("CAS fetch failed or timed out: {0}")]
    CasFetchFailed(String),
    #[error("DM blob decryption failed under all candidate keys")]
    DecryptFailed,
    #[error("payload sender does not match resolved owner (impersonation)")]
    SenderImpersonation,
    #[error("packet decode failed: {0}")]
    Decode(String),
    #[error("AAD compute failed: {0}")]
    AadCompute(String),
    #[error("CRDT rejected the apply (invariant violation): {0}")]
    CrdtRejected(String),
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

/// Resolve a verified signing device → owner. MUST match exactly one OwnerAddr.
///
/// Pre-condition: the caller has already verified the signature against
/// the public key for `signing_device_hash`. This function only does the
/// device-hash → OwnerAddr lookup, not signature verification.
///
/// Returns Err on zero matches (UnknownSigningDevice) or multiple matches
/// (AmbiguousSigningDevice). Multi-match is reachable via corrupted state
/// or a malicious cache-poisoning DmInvite that claimed an existing device
/// hash for a different owner; either way the resolution is not trustworthy
/// — drop + telemetry.
///
/// Uses `binary_search` on `OwnerDeviceEntry::devices`, which is sorted-
/// ascending-lex per its existing invariant (re-established by
/// `deserialize_device_identities` on every load — see
/// `owner_state_types.rs:286-307`).
// Task 9 (handle_invite) does NOT call this — invites carry the inviter
// inline so resolution is unnecessary. Tasks 10 (handle_cidnotify) and 11
// (handle_ack) will be the first consumers.
#[allow(dead_code)] // Wired in by Tasks 10/11 (handle_cidnotify/handle_ack).
pub(crate) fn resolve_signed_origin_owner(
    cache: &OwnerDeviceCache,
    signing_device_hash: DeviceIdentityHash,
) -> Result<OwnerAddr, DmReceiveError> {
    let matches: Vec<OwnerAddr> = cache
        .devices
        .iter()
        .filter(|(_, entry)| entry.devices.binary_search(&signing_device_hash).is_ok())
        .map(|(addr, _)| *addr)
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(DmReceiveError::UnknownSigningDevice),
        _ => Err(DmReceiveError::AmbiguousSigningDevice),
    }
}

/// Look up the cached 64-byte combined identity pubs for a known device.
/// Reads from `OwnerDeviceCache` via the parallel-vec correspondence
/// between `devices[i]` and `device_identity_pubs[i]` (Task 4).
///
/// Returns `Some(identity_pub_bytes)` only if the device hash is in the
/// cache AND the cache has a `Some(pub)` at the corresponding index.
/// Returns `None` for any of: device unknown, or device known but
/// `device_identity_pubs[i] == None` (pre-bootstrap state — handler
/// treats as `UnknownSigningKey`).
///
/// Returns the full 64-byte combined pub (X25519 || Ed25519); the caller
/// passes this to `dm_signing::verify_dm_packet_signature`, which splits
/// out the Ed25519 half internally. We must return the full 64 bytes
/// (not just Ed25519) so `verify_dm_packet_signature` can re-derive the
/// `signing_device_hash` and confirm the cached pub actually maps to the
/// hash the body claims (key-substitution defense).
#[allow(dead_code)] // Wired in by Tasks 10/11 (handle_cidnotify/handle_ack).
pub(crate) fn lookup_pubkey_for_device(
    cache: &OwnerDeviceCache,
    signing_device_hash: DeviceIdentityHash,
) -> Option<[u8; 64]> {
    for entry in cache.devices.values() {
        if let Ok(idx) = entry.devices.binary_search(&signing_device_hash) {
            if idx < entry.device_identity_pubs.len() {
                // device_identity_pubs[idx] is Option<[u8; 64]>;
                // Some → return; None → fall through, no pub cached.
                return entry.device_identity_pubs[idx];
            }
            return None; // device present but pubs vec shorter than expected
        }
    }
    None
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
        // accumulate one entry per send call forever (~32 bytes each). Verify:
        //   - count is bounded at STUB_MAX_RECORDED_SENDS
        //   - eviction is FIFO (oldest evicted, not newest) — guards against
        //     a future refactor accidentally using pop_back
        let t = StubTransport::new();
        let r = OwnerAddr([2u8; 16]);
        // Each call uses a unique entry_id (1, 2, ...) so we can verify which
        // entries survived eviction by their byte-pattern.
        let total = 2000u32;
        for i in 1..=total {
            let id = OutboxEntryId([i as u8; 16]);
            let mut e = entry(0);
            e.id = id;
            let _ = t.send(&e, r).await;
        }
        let recorded = t.sends();
        assert_eq!(
            recorded.len(),
            StubTransport::STUB_MAX_RECORDED_SENDS,
            "ring buffer must cap at STUB_MAX_RECORDED_SENDS"
        );
        // FIFO: the oldest survivor is push #(total - cap + 1).
        // total=2000, cap=1024 → first survivor is #977.
        // entry_id is [u8; 16] of (i as u8), which wraps mod 256.
        let first_survivor_index = total - StubTransport::STUB_MAX_RECORDED_SENDS as u32 + 1;
        let expected_first_byte = first_survivor_index as u8;
        assert_eq!(
            recorded[0].0 .0[0], expected_first_byte,
            "FIFO eviction: oldest survivor should be push #{first_survivor_index}, \
             not the newest entry (would indicate pop_back regression)"
        );
        // Last survivor is push #total.
        let expected_last_byte = total as u8;
        assert_eq!(
            recorded[recorded.len() - 1].0 .0[0],
            expected_last_byte,
            "FIFO eviction: newest survivor should be push #{total}"
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
    fn mark_ack_delivered_updates_delivered_to() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = outbox_entry_with_recipients(7, vec![bob]);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let mut o = DmOutbox::new("dev".into(), alice);
        let inserted = o.mark_ack_delivered(&mut state, entry_id, bob);

        assert!(inserted, "first ack inserts");
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(stored.delivered_to.contains(&bob));
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    #[test]
    fn mark_ack_delivered_duplicate_is_idempotent() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = outbox_entry_with_recipients(7, vec![bob]);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let mut o = DmOutbox::new("dev".into(), alice);
        let first = o.mark_ack_delivered(&mut state, entry_id, bob);
        let second = o.mark_ack_delivered(&mut state, entry_id, bob);

        assert!(first);
        assert!(!second, "duplicate ack returns false");
        let stored = state.outbox.get(&entry_id).unwrap();
        assert_eq!(stored.delivered_to.len(), 1);
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    #[test]
    fn resolve_signed_origin_owner_single_match_returns_owner() {
        use crate::owner_state_types::{OwnerDeviceCache, OwnerDeviceEntry};
        let mut cache = OwnerDeviceCache::default();
        cache.devices.insert(
            OwnerAddr([1; 16]),
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0xa1; 16])],
                device_identity_pubs: vec![Some([0x11; 64])],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
            },
        );
        let owner = resolve_signed_origin_owner(&cache, DeviceIdentityHash([0xa1; 16])).unwrap();
        assert_eq!(owner, OwnerAddr([1; 16]));
    }

    #[test]
    fn resolve_signed_origin_owner_no_matches_returns_unknown() {
        use crate::owner_state_types::OwnerDeviceCache;
        let cache = OwnerDeviceCache::default();
        let err = resolve_signed_origin_owner(&cache, DeviceIdentityHash([0xff; 16])).unwrap_err();
        assert!(matches!(err, DmReceiveError::UnknownSigningDevice));
    }

    #[test]
    fn resolve_signed_origin_owner_multi_match_returns_ambiguous() {
        // Two OwnerAddr entries claiming the same DeviceIdentityHash.
        // Reachable only via corrupted state or a malicious DmInvite that
        // asserted an existing device hash for a different owner.
        // Resolution untrustworthy — drop with telemetry.
        use crate::owner_state_types::{OwnerDeviceCache, OwnerDeviceEntry};
        let mut cache = OwnerDeviceCache::default();
        let shared = DeviceIdentityHash([0xa1; 16]);
        cache.devices.insert(
            OwnerAddr([1; 16]),
            OwnerDeviceEntry {
                devices: vec![shared],
                device_identity_pubs: vec![Some([0x11; 64])],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
            },
        );
        cache.devices.insert(
            OwnerAddr([2; 16]),
            OwnerDeviceEntry {
                devices: vec![shared], // same hash claimed by a different owner
                device_identity_pubs: vec![Some([0x22; 64])],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
            },
        );
        let err = resolve_signed_origin_owner(&cache, shared).unwrap_err();
        assert!(matches!(err, DmReceiveError::AmbiguousSigningDevice));
    }

    #[tokio::test]
    async fn handle_unicast_invalid_packet_returns_decode_error() {
        let mut state = OwnerState::default();
        let mut outbox = DmOutbox::new("device".into(), OwnerAddr([0xff; 16]));
        let cas = InMemoryStub::default();
        let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

        // Unknown discriminant 0xff with enough trailing bytes to clear the
        // TooShortForSignature gate (1 body byte + 64 signature bytes). The
        // TooShortForSignature gate has its own dm_envelope test; this test
        // exercises the unknown-discriminant drop path.
        let mut padded = vec![0xff_u8]; // unknown discriminant
        padded.extend(std::iter::repeat_n(0_u8, 65)); // 1 byte body + 64 byte sig

        let err = outbox
            .handle_unicast(&mut state, &cas, &tx, padded, 100)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::Decode(_)),
            "expected Decode error from unknown discriminant, got {err:?}"
        );
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
        // handle_unicast's DmAck arm via mark_ack_delivered; Phase 2 callers
        // do it directly).
        let inserted = o.mark_ack_delivered(&mut state, entry_id, bob);
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

    /// Test-only `DestinationResolver` impl with a fixed lookup table.
    /// Mirrors the production `OwnerDeviceCacheResolver` (Task 11) shape
    /// without depending on CRDT state — keeps the transport unit tests
    /// hermetic.
    struct StaticDestResolver {
        table: HashMap<OwnerAddr, Vec<[u8; 16]>>,
    }

    impl StaticDestResolver {
        fn new(entries: impl IntoIterator<Item = (OwnerAddr, Vec<[u8; 16]>)>) -> Self {
            Self {
                table: entries.into_iter().collect(),
            }
        }
    }

    impl DestinationResolver for StaticDestResolver {
        fn resolve(&self, recipient: OwnerAddr) -> Vec<[u8; 16]> {
            self.table.get(&recipient).cloned().unwrap_or_default()
        }
    }

    #[tokio::test]
    async fn runtime_unicast_transport_send_pushes_signed_event_into_channel() {
        // Synthetic identity_pub trick (per dm_signing.rs's empirical
        // finding that ed25519-dalek doesn't strict-check point membership
        // at construction): all-zero X25519 half + real Ed25519 half.
        // The address_hash for this synthetic input matches what
        // verify_dm_packet_signature will compute, so the
        // SigningKeyDoesNotMatchDeviceHash check passes; the Ed25519
        // signature still verifies under the real verifying key.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);
        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]));
        let signing_pub = signing_key.verifying_key();
        let mut identity_pub = [0u8; 64];
        identity_pub[32..].copy_from_slice(signing_pub.as_bytes());
        let our_device = crate::dm_signing::derive_device_hash_from_identity_pub(&identity_pub)
            .expect("synthetic identity_pub should be valid");

        let recipient = OwnerAddr([1; 16]);
        let dest_hash = [0xd1u8; 16];
        let resolver = std::sync::Arc::new(StaticDestResolver::new([(recipient, vec![dest_hash])]));

        let transport = RuntimeUnicastTransport::new(
            tx,
            resolver,
            OwnerAddr([0xff; 16]),
            our_device,
            signing_key.clone(),
        );

        let entry = OutboxEntry {
            id: OutboxEntryId([0xab; 16]),
            space_id: SpaceId([0xcc; 16]),
            recipient_owners: vec![recipient],
            message_cid: ContentId::from_bytes([0xee; 32]),
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };

        transport
            .send(&entry, recipient)
            .await
            .expect("send must succeed");

        let req = rx.recv().await.expect("channel produced no event");
        assert_eq!(req.destination_hash, dest_hash);

        // Decode wire packet → confirm shape + signature verifies.
        let packet = crate::dm_envelope::decode_packet(&req.packet).unwrap();
        match packet {
            crate::dm_envelope::DmPacket::CidNotify {
                signed,
                signature,
                signed_bytes,
            } => {
                assert_eq!(signed.space_id, SpaceId([0xcc; 16]));
                assert_eq!(signed.message_cid, ContentId::from_bytes([0xee; 32]));
                assert_eq!(signed.sender_owner_addr, OwnerAddr([0xff; 16]));
                assert_eq!(signed.signing_device_hash, our_device);
                assert_eq!(
                    signed.sender_devices,
                    vec![our_device],
                    "Phase 3b ships single-device sender_devices"
                );
                // Signature must verify against our identity_pub +
                // claimed device hash.
                assert!(crate::dm_signing::verify_dm_packet_signature(
                    &signed_bytes,
                    &signature,
                    &identity_pub,
                    our_device,
                )
                .is_ok());
            }
            other => panic!("expected CidNotify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn runtime_unicast_transport_no_known_devices_is_transient_error() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);
        let resolver = std::sync::Arc::new(StaticDestResolver::new(std::iter::empty::<(
            OwnerAddr,
            Vec<[u8; 16]>,
        )>()));
        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]));
        // Arbitrary — resolver returns empty before the device hash is
        // ever consulted on the send path.
        let our_device = DeviceIdentityHash([0xaa; 16]);

        let transport = RuntimeUnicastTransport::new(
            tx,
            resolver,
            OwnerAddr([0xff; 16]),
            our_device,
            signing_key,
        );

        let entry = OutboxEntry {
            id: OutboxEntryId([0xab; 16]),
            space_id: SpaceId([0xcc; 16]),
            recipient_owners: vec![OwnerAddr([1; 16])],
            message_cid: ContentId::from_bytes([0xee; 32]),
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };

        let err = transport
            .send(&entry, OwnerAddr([1; 16]))
            .await
            .unwrap_err();
        assert!(
            matches!(err, TransportError::Transient(_)),
            "empty resolver must surface as Transient (drives backoff retry), got {err:?}"
        );
    }

    #[tokio::test]
    async fn handle_invite_writes_space_and_cache_with_signing_pub() {
        use crate::owner_state_crdt::OwnerState;

        let mut state = OwnerState::default();
        let mut outbox = DmOutbox::new("device".into(), OwnerAddr([2; 16]));

        // Build a real signed DmInvite via PrivateIdentity::from_seed.
        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
        };

        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        outbox
            .handle_invite(&mut state, signed.clone(), signature, &body_bytes, 200)
            .await
            .unwrap();

        // Space written.
        assert!(state.spaces.contains_key(&SpaceId([7; 16])));
        let space = state.spaces.get(&SpaceId([7; 16])).unwrap();
        assert_eq!(space.kind, SpaceKind::Dm);
        assert!(space.content_key.is_some());

        // OwnerDeviceCache updated under invite.inviter.
        let entry = state
            .owner_device_cache
            .devices
            .get(&OwnerAddr([1; 16]))
            .unwrap();
        assert_eq!(entry.devices, vec![device_hash]);
        // Cached pub is at index 0 (the only device, also the signer).
        assert_eq!(entry.device_identity_pubs[0], Some(identity_pub));
    }

    #[tokio::test]
    async fn handle_invite_binds_inviter_field_not_members_zero() {
        // Group-DM where invite.inviter is the lex-LARGEST member (so
        // members[0] is a different OwnerAddr). Cache entry must be created
        // under invite.inviter, NOT members[0]. Regression for the lex-vs-
        // inviter binding bug surfaced in spec §"Application-signature
        // binding rule".
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();
        let mut outbox = DmOutbox::new("device".into(), OwnerAddr([2; 16]));

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let inviter_addr = OwnerAddr([0xff; 16]); // lex-largest
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::GroupDm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16]), inviter_addr], // sorted ascending
            inviter: inviter_addr,
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        outbox
            .handle_invite(&mut state, signed, signature, &body_bytes, 200)
            .await
            .unwrap();

        // Cache entry under inviter_addr, NOT members[0].
        assert!(state.owner_device_cache.devices.contains_key(&inviter_addr));
        assert!(!state
            .owner_device_cache
            .devices
            .contains_key(&OwnerAddr([1; 16])));
    }

    #[tokio::test]
    async fn handle_invite_inviter_not_in_members_drops() {
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();
        let mut outbox = DmOutbox::new("device".into(), OwnerAddr([2; 16]));

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([3; 16]), // NOT in members
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        let err = outbox
            .handle_invite(&mut state, signed, signature, &body_bytes, 200)
            .await
            .unwrap_err();
        assert!(matches!(err, DmReceiveError::InviterNotInMembers));
        assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
        assert!(state.owner_device_cache.devices.is_empty());
    }

    #[tokio::test]
    async fn handle_invite_signing_device_not_in_sender_devices_drops() {
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();
        let mut outbox = DmOutbox::new("device".into(), OwnerAddr([2; 16]));

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        // Construct an invite where signing_device_hash is NOT in
        // sender_devices. The sanity gate must reject before signature
        // verification even runs.
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![DeviceIdentityHash([0xab; 16])], // does NOT include device_hash
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
        };

        // NOTE: decode_packet would reject this packet as
        // DecodeError::Invalid (the same invariant). Because we're calling
        // handle_invite directly with a hand-constructed DmInviteSigned
        // that bypasses decode, this test exercises the defense-in-depth
        // gate inside handle_invite. In production the packet would never
        // reach handle_invite — it'd drop at decode_packet — but the gate
        // catches future regressions if decode_packet's invariant is ever
        // loosened.
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        let err = outbox
            .handle_invite(&mut state, signed, signature, &body_bytes, 200)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DmReceiveError::SigningDeviceNotInSenderDevices
        ));
        assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
    }

    #[tokio::test]
    async fn handle_invite_receiver_not_in_members_drops() {
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();
        // self_owner NOT in invite.members.
        let mut outbox = DmOutbox::new("device".into(), OwnerAddr([99; 16]));

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])], // self_owner not here
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        let err = outbox
            .handle_invite(&mut state, signed, signature, &body_bytes, 200)
            .await
            .unwrap_err();
        assert!(matches!(err, DmReceiveError::ReceiverNotInMembers));
        assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
    }

    #[tokio::test]
    async fn handle_invite_tampered_signature_drops() {
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();
        let mut outbox = DmOutbox::new("device".into(), OwnerAddr([2; 16]));

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let mut signature = private.sign(&body_bytes);
        // Flip a bit in the signature.
        signature[0] ^= 0xff;

        let err = outbox
            .handle_invite(&mut state, signed, signature, &body_bytes, 200)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::SignatureVerificationFailed),
            "expected SignatureVerificationFailed, got {:?}",
            err
        );
        assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
    }
}
