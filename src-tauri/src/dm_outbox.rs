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
    /// Send `entry` to `recipient`'s pre-resolved `destinations`. The
    /// caller (drain) resolves OwnerAddr → device-hash list before
    /// invoking — see `resolve_destinations` below. Empty `destinations`
    /// must be filtered out by the caller (drain treats empty as a
    /// transient resolver miss and bumps backoff without calling send).
    ///
    /// Resolution is split out of the transport (was inside
    /// `RuntimeUnicastTransport::send` via an injected `DestinationResolver`
    /// in the original Phase 3b shape) because production drain holds
    /// `OwnerState`'s mutex via `&mut OwnerState`, and the production
    /// resolver also needed to read `OwnerState` — which deadlocked with
    /// `try_lock` on the same Tokio mutex. Resolving inside drain reads
    /// directly from the held `&OwnerState` reference, no locking
    /// required.
    async fn send(
        &self,
        entry: &OutboxEntry,
        recipient: OwnerAddr,
        destinations: Vec<[u8; 16]>,
    ) -> Result<(), TransportError>;
}

/// Resolve `recipient` → list of 16-byte Reticulum destination hashes
/// from `OwnerDeviceCache`. Each cached `DeviceIdentityHash` maps to its
/// destination via `compute_dm_destination_hash` (Task 10). Empty Vec
/// when no entry is known — drain treats that as a transient miss and
/// bumps backoff so a future tick (after Flow A propagates the missing
/// entry) retries.
///
/// Pure function: no locking, no `&mut`. Drain calls this with the
/// `&OwnerState` it already has from its mutex guard, sidestepping the
/// recursive-lock deadlock that lived in the original Phase 3b shape.
pub fn resolve_destinations(cache: &OwnerDeviceCache, recipient: OwnerAddr) -> Vec<[u8; 16]> {
    cache
        .devices
        .get(&recipient)
        .map(|entry| {
            entry
                .devices
                .iter()
                .map(|d| crate::dm_signing::compute_dm_destination_hash(d.0))
                .collect()
        })
        .unwrap_or_default()
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
//
// Stub ignores `destinations` — its purpose is to record (entry, recipient)
// for unit-test assertions; per-device fan-out is exercised by the
// integration test against `RuntimeUnicastTransport`.
#[async_trait]
impl DmTransport for StubTransport {
    async fn send(
        &self,
        entry: &OutboxEntry,
        recipient: OwnerAddr,
        _destinations: Vec<[u8; 16]>,
    ) -> Result<(), TransportError> {
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

/// Production `DmTransport` adapter (ZEB-227 Phase 3b). Per `send`:
///
/// 1. Build a `DmCidNotifySigned` whose `signing_device_hash` is our
///    device's identity hash (single-device `sender_devices` for Phase
///    3b — cross-device piggyback grows automatically as Flow A
///    propagates more entries; see spec §"Public-key storage on
///    OwnerDeviceCache").
/// 2. Sign + canonical-CBOR-encode via
///    `dm_envelope::build_signed_cidnotify` + `encode_packet`.
/// 3. Push one `UnicastSendRequest` per destination hash into `tx`,
///    which the event-loop drains and forwards to `NodeRuntime`.
///
/// Resolution of `recipient: OwnerAddr` → `destinations: Vec<[u8; 16]>`
/// happens UPSTREAM in `DmOutbox::drain` (which has `&OwnerState` in
/// scope from its mutex guard). Original Phase 3b shape had the
/// transport own a `DestinationResolver` that also wanted to lock
/// `OwnerState` — recursive `try_lock` on the same Tokio mutex always
/// failed → empty Vec → no DMs ever delivered. Splitting resolution out
/// of the transport sidesteps that deadlock; see `resolve_destinations`
/// above.
///
/// `DmInvite` outbound is Phase 4's `add_space` IPC for DM kinds
/// (spec Flow 1). `DmAck` outbound is built directly by the receive-side
/// `handle_cidnotify` (Task 10) — it bypasses `DmTransport::send`
/// because acks are not tied to an `OutboxEntry` retry loop.
pub struct RuntimeUnicastTransport {
    tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
    self_owner: OwnerAddr,
    our_signing_device_hash: DeviceIdentityHash,
    signing_key: Arc<ed25519_dalek::SigningKey>,
}

impl RuntimeUnicastTransport {
    pub fn new(
        tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        signing_key: Arc<ed25519_dalek::SigningKey>,
    ) -> Self {
        Self {
            tx,
            self_owner,
            our_signing_device_hash,
            signing_key,
        }
    }
}

#[async_trait]
impl DmTransport for RuntimeUnicastTransport {
    async fn send(
        &self,
        entry: &OutboxEntry,
        recipient: OwnerAddr,
        destinations: Vec<[u8; 16]>,
    ) -> Result<(), TransportError> {
        // Empty destinations → no known devices for this recipient.
        // Surface as Transient so the outbox backoff drives a future
        // retry once Flow A propagates the missing OwnerDeviceCache
        // entry. (StubTransport ignores `destinations` and returns
        // pre-seeded outcomes — this branch only fires for the real
        // production transport path.)
        if destinations.is_empty() {
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
            // Use try_send (not send().await) because this transport runs
            // inside the event-loop task that ALSO drains
            // `unicast_send_rx`. .await on a full channel would deadlock
            // the event loop on itself. Transient errors flow back into
            // DmOutbox::drain's per-recipient backoff, which retries on
            // the next tick once the channel has drained.
            self.tx
                .try_send(UnicastSendRequest {
                    destination_hash,
                    packet: wire.clone(),
                })
                .map_err(|e| match e {
                    tokio::sync::mpsc::error::TrySendError::Full(_) => {
                        TransportError::Transient("unicast channel full".to_string())
                    }
                    // Closed channel = event-loop receiver dropped (runtime
                    // shutdown / panic). Permanent because retry will never
                    // succeed; the OutboxEntry surfaces failure once instead
                    // of spinning every drain tick.
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                        TransportError::Permanent(format!("event-loop channel closed: {e}"))
                    }
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
    /// Phase 4: ReceivedMessage bundles written by `handle_cidnotify` for
    /// which `apply_inbox` returned `ApplyOutcome::Inserted` (NOT
    /// `Merged`/NoOp). The caller emits `dm-received` IPC events from this
    /// field. Per spec §"Idempotency and drain semantics", the
    /// inserted-vs-already-present discriminant from `apply_inbox` is the
    /// atomic-emit boundary; duplicate notifies hitting the same
    /// `(space_id, message_cid)` key don't re-emit. `drain` (sender-side)
    /// leaves this empty; only `handle_cidnotify` (receiver-side)
    /// populates it.
    ///
    /// Each element wraps the InboxEntry alongside the decrypted body,
    /// mime_type, and sender's HLC `sent_at` — Phase 3b only carried the
    /// InboxEntry pointer, forcing the IPC emit path to re-fetch + re-
    /// decrypt to render the message. Phase 4 widens the carrier so the
    /// already-decrypted MessagePayload fields ride along.
    pub newly_received: Vec<crate::owner_state_types::ReceivedMessage>,
}

/// Phase 4 — outcome of `DmOutbox::delete_dm_outbox_entry`.
///
/// The IPC layer reads this to decide which `dm-deleted` IPC event to
/// emit. All fields are `Option` so a no-op delete (idempotent missing-
/// id call) returns `Default::default()` and the caller knows to emit
/// nothing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeleteDmOutboxOutcome {
    pub deleted_outbox_id: Option<OutboxEntryId>,
    pub deleted_inbox_key: Option<crate::owner_state_types::InboxKey>,
    pub space_id: Option<SpaceId>,
    pub message_cid: Option<crate::owner_state_types::ContentId>,
}

/// Phase 4 — error type for `DmOutbox::delete_dm_outbox_entry`.
///
/// Currently no failure modes (BTreeMap removal is infallible, and a
/// missing entry is the idempotent success case). Kept as an enum
/// purely for forward-extensibility — adding a variant later (e.g.,
/// "cannot delete a Complete entry") is a non-breaking change in
/// signature, whereas widening `Result<T, !>` to `Result<T, E>` would
/// be a breaking change for every caller. Suppress the empty-enum
/// lint accordingly.
#[allow(clippy::empty_enums)]
#[derive(Debug, thiserror::Error)]
pub enum DeleteDmError {}

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
    /// Phase 3b: our device-Identity hash, used as the
    /// `signing_device_hash` in outbound DmAck packets fanned out by
    /// `handle_cidnotify`. Mirrors `RuntimeUnicastTransport`'s field of
    /// the same name; both are populated from the same identity-management
    /// site in production (Task 11 wires `lib.rs::start_node`).
    pub(crate) our_signing_device_hash: DeviceIdentityHash,
    /// Phase 3b: our device's Ed25519 signing key, used to sign outbound
    /// DmAck packets in `handle_cidnotify`. Held via `Arc` so the outbox
    /// can outlive any single owning context — `RuntimeUnicastTransport`
    /// holds it the same way.
    pub(crate) signing_key: Arc<ed25519_dalek::SigningKey>,
    in_flight: HashSet<(OutboxEntryId, OwnerAddr)>,
    backoff: HashMap<(OutboxEntryId, OwnerAddr), AttemptState>,
}

impl DmOutbox {
    pub fn new(
        device_id: String,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        signing_key: Arc<ed25519_dalek::SigningKey>,
    ) -> Self {
        Self {
            device_id,
            self_owner,
            our_signing_device_hash,
            signing_key,
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
            created_at: sent_at.clone(),
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };
        match state.apply_outbox(entry) {
            ApplyOutcome::Inserted | ApplyOutcome::Merged { .. } => {
                // Phase 4: Self-InboxEntry write for self-history persistence.
                //
                // InboxEntry semantics widen here from "received from someone
                // else" to "exists in this Space's history (sender OR
                // recipient)". A paired device receiving the same DmCidNotify
                // writes its own InboxEntry on receipt; this self-write on
                // the sending device matches what the paired device will
                // write, so the InboxEntry table converges naturally without
                // special-casing.
                let self_inbox_entry = crate::owner_state_types::InboxEntry {
                    space_id,
                    message_cid,
                    from: self.self_owner,
                    received_at: sent_at.clone(),
                };
                let _ = state.apply_inbox(self_inbox_entry);
                // Outcome ignored: Inserted is the happy path; Merged{old_id:
                // None} fires if a paired device's CidNotify already wrote
                // this CID first (cross-device race), which is fine — same
                // payload, idempotent.
                Ok(entry_id)
                // Note: ApplyOutcome::Merged would also reach here. It "should
                // not happen" because a fresh ULID can't collide with any
                // existing entry, but we treat it the same as Inserted for
                // safety.
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

    /// Phase 4 — Manual delete of a stuck or expired self-OutboxEntry.
    ///
    /// Removes BOTH the OutboxEntry and the corresponding self-InboxEntry
    /// keyed by `(space_id, message_cid)`. User intent on manual delete
    /// is "make this message go away," so removing both is the expected
    /// UX. The IPC layer reads the returned `DeleteDmOutboxOutcome` to
    /// decide which `dm-deleted` event to emit.
    ///
    /// Also clears any in-flight + backoff cache entries for the deleted
    /// message so a stale entry can't resurface from a future drain tick.
    ///
    /// Idempotent: returns `Default::default()` (all None) if the
    /// OutboxEntry doesn't exist (e.g., already deleted, or the caller
    /// raced a Complete → GC).
    pub fn delete_dm_outbox_entry(
        &mut self,
        state: &mut OwnerState,
        message_id: OutboxEntryId,
    ) -> Result<DeleteDmOutboxOutcome, DeleteDmError> {
        let outbox_entry = match state.outbox.remove(&message_id) {
            Some(e) => e,
            None => return Ok(DeleteDmOutboxOutcome::default()),
        };
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id: outbox_entry.space_id,
            message_cid: outbox_entry.message_cid,
        };
        // Self-InboxEntry may legitimately be absent (e.g., a paired
        // device's CidNotify could have raced ahead and the InboxEntry
        // could have been GC'd). Either way, idempotent removal.
        let _removed_inbox = state.delete_inbox_entry(inbox_key);

        // Clear in-flight + backoff caches across all recipients of this
        // message so a stale entry can't resurface on a future drain.
        self.in_flight.retain(|(eid, _)| *eid != message_id);
        self.backoff.retain(|(eid, _), _| *eid != message_id);

        Ok(DeleteDmOutboxOutcome {
            deleted_outbox_id: Some(message_id),
            deleted_inbox_key: Some(inbox_key),
            space_id: Some(outbox_entry.space_id),
            message_cid: Some(outbox_entry.message_cid),
        })
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
                // Resolve destinations from the in-scope `&OwnerState`
                // (no mutex acquisition: drain holds the state via the
                // caller's mutex guard, no recursive lock needed).
                // Production transport returns `TransportError::Transient`
                // on empty (Flow A may surface the missing
                // OwnerDeviceCache entry on the next sync round); test
                // stubs (StubTransport) ignore `destinations` entirely.
                let destinations = resolve_destinations(&state.owner_device_cache, recipient);
                self.in_flight.insert((entry_id, recipient));
                let result = transport.send(&entry_clone, recipient, destinations).await;
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
    ///        The LWW HLC for this update is built from OUR local
    ///        `wall_now_ms` + `self.device_id`, NOT `signed.created_at`
    ///        (the inviter's claim) — using the remote HLC would let
    ///        an attacker forge a far-future timestamp on a single
    ///        malicious invite and pin the cache, rejecting all future
    ///        legitimate updates from the same owner as `StaleHlc`.
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
        wall_now_ms: u64,
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

        // SECURITY: the OwnerDeviceCache LWW HLC must record when WE
        // learned about these devices, NOT the timestamp the inviter
        // claims they sent the invite. Using `signed.created_at` here
        // would let an attacker forge a far-future HLC (e.g.,
        // wall_ms = u64::MAX / 2) on a single malicious invite,
        // pinning the local cache and rejecting every legitimate
        // future update from the same owner as `StaleHlc` — a
        // denial-of-updates attack. Mirror the pattern that
        // `handle_cidnotify` step 8 already uses (local wall clock
        // + our device_id).
        let learned_at = Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: self.device_id.clone(),
        };
        let cache_outcome = state.apply_owner_device_update(
            signed.inviter,
            signed.sender_devices.clone(),
            device_identity_pubs,
            learned_at,
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
            name: format!("DM with {}", hex::encode(signed.inviter.0)),
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

    /// Inbound `DmCidNotify` handler — Phase 3b receive path.
    ///
    /// Per ZEB-216 spec §"Wire format" Flow 2 steps 7-13:
    ///   7a. Look up signing pubkey via `lookup_pubkey_for_device`. None →
    ///       `UnknownSigningKey` (pre-bootstrap state — drop, telemetry).
    ///       Verify signature via `dm_signing::verify_dm_packet_signature`
    ///       (key-substitution defense + Ed25519 verify).
    ///   7b. `resolve_signed_origin_owner(cache, signing_device_hash)` →
    ///       `resolved_owner` (UnknownSigningDevice / AmbiguousSigningDevice
    ///       drop on degenerate cache states).
    ///   7c. `signed.sender_owner_addr ?= resolved_owner` — drop
    ///       `OwnerFieldMismatch` on cache-poisoning.
    ///   8.  `apply_owner_device_update` with notify.sender_devices and
    ///       a parallel pubs vec (Some at the signer's index, None
    ///       elsewhere). LWW HLC uses our local wall clock — this is OUR
    ///       record of when WE learned about these devices, not the
    ///       sender's HLC. Rejected outcome ignored (our cache may be
    ///       fresher than what just arrived).
    ///   9.  Look up Space (SpaceNotFound drops if we're not a member).
    ///       `cas.get(message_cid)` with 500ms tokio::time::timeout. Three
    ///       drop paths: blob not found, fetch error, timeout — all
    ///       surface as `CasFetchFailed`.
    ///   11. `decrypt_dm_message` with prior-keys fallback (current key
    ///       first, then each prior).
    ///   12. `verify_sender_binding(payload, resolved_owner)` —
    ///       encrypted-payload-layer check that the body's sender field
    ///       matches the wire-layer cryptographically-authenticated source.
    ///   13a. `apply_inbox` + atomic-emit: only `Inserted` (NOT NoOp /
    ///        Merged) populates `DrainOutcome.newly_received`. Duplicate
    ///        notifies stay silent per spec §"Idempotency and drain
    ///        semantics".
    ///   13b. DmAck fan-out: build_signed_ack with our signing key + our
    ///        signing_device_hash; encode_packet; one
    ///        `UnicastSendRequest` per device in signed.sender_devices.
    ///        Failed sends are silent per spec (no retry on the ack
    ///        itself).
    ///
    /// Sanity gates run BEFORE expensive operations (signature verify,
    /// CAS fetch, decrypt). Cheaper checks first.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_cidnotify(
        &mut self,
        state: &mut OwnerState,
        cas: &dyn ContentStore,
        unicast_send_tx: &tokio::sync::mpsc::Sender<UnicastSendRequest>,
        signed: crate::dm_envelope::DmCidNotifySigned,
        signature: [u8; 64],
        signed_bytes: &[u8],
        wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        // Step 7a: look up signing pubkey + verify signature.
        let identity_pub =
            lookup_pubkey_for_device(&state.owner_device_cache, signed.signing_device_hash)
                .ok_or(DmReceiveError::UnknownSigningKey)?;
        crate::dm_signing::verify_dm_packet_signature(
            signed_bytes,
            &signature,
            &identity_pub,
            signed.signing_device_hash,
        )?;

        // Step 7b: resolve signing_device_hash → OwnerAddr.
        let resolved_owner =
            resolve_signed_origin_owner(&state.owner_device_cache, signed.signing_device_hash)?;

        // Step 7c: verify notify.sender_owner_addr matches the resolved
        // owner. Drops cache-poisoning attempts where a peer claims a
        // sender_owner_addr that doesn't agree with the cryptographically-
        // authenticated source.
        if signed.sender_owner_addr != resolved_owner {
            return Err(DmReceiveError::OwnerFieldMismatch);
        }

        // Look up Space for AAD + content_key. SpaceNotFound is the
        // "we are not a member" / "stale notify after we left" drop path.
        let space = state
            .spaces
            .get(&signed.space_id)
            .ok_or(DmReceiveError::SpaceNotFound)?
            .clone();

        // Membership gate. Signature verify proves WHO sent this; this gate
        // proves they're still ALLOWED to send into this space. An ex-member
        // whose identity_pub is still in OwnerDeviceCache (because we haven't
        // expired the entry) would otherwise pass signature verify and land
        // a message in our inbox after their membership was revoked. For DM
        // kinds members never changes; for GroupDm members can shrink, and
        // this gate is what blocks cached-key reuse from a removed member.
        if !space.members.contains(&resolved_owner) {
            return Err(DmReceiveError::SenderNotInSpaceMembers);
        }

        // Step 8: refresh OwnerDeviceCache with notify.sender_devices.
        // Pubs vec: Some(identity_pub) at the signer's index (we just
        // verified that hash → pub binding), None at every other index
        // (Path B post-bootstrap — non-signer devices remain pubs-less
        // until the next signed packet from each surfaces them).
        // HLC uses our local wall clock + device_id (this is OUR record
        // of when WE learned, NOT the sender's HLC).
        let mut updated_pubs: Vec<Option<[u8; 64]>> = vec![None; signed.sender_devices.len()];
        if let Some(idx) = signed
            .sender_devices
            .iter()
            .position(|d| *d == signed.signing_device_hash)
        {
            updated_pubs[idx] = Some(identity_pub);
        }
        // Ignore the apply outcome — Rejected (StaleHlc) is acceptable
        // here, our cache may already be fresher than what just arrived.
        let _ = state.apply_owner_device_update(
            resolved_owner,
            signed.sender_devices.clone(),
            updated_pubs,
            Hlc {
                wall_ms: wall_now_ms,
                logical: 0,
                device_id: self.device_id.clone(),
            },
        );

        // Step 9: fetch storage_blob from CAS with 500ms timeout. The
        // timeout matters in production where cas.get goes through the
        // cas_op channel and may wait on Zenoh DAG-sync; tests using
        // InMemoryStub return immediately so the timeout is never hit.
        let blob = match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            cas.get(&signed.message_cid),
        )
        .await
        {
            Ok(Ok(Some(bytes))) => bytes,
            Ok(Ok(None)) => return Err(DmReceiveError::CasFetchFailed("blob not found".into())),
            Ok(Err(e)) => return Err(DmReceiveError::CasFetchFailed(format!("{e:?}"))),
            Err(_) => return Err(DmReceiveError::CasFetchFailed("500ms fetch timeout".into())),
        };

        // Step 11: decrypt with prior-keys fallback.
        // `space.content_key` is non-None for any DM/group-DM Space that
        // passed `validate_invariants` — the invariant check in
        // `apply_space_with_canonicalization` (which wrote this Space into
        // state) rejects DM/group-DM Spaces with content_key=None.
        let aad = crate::dm_crypto::compute_aad(&space)
            .map_err(|e| DmReceiveError::AadCompute(e.to_string()))?;
        let payload = crate::dm_crypto::decrypt_dm_message(
            space
                .content_key
                .as_ref()
                .expect("DM Space MUST have content_key per validate_invariants"),
            &space.prior_content_keys,
            &aad,
            &blob,
        )
        .map_err(|_| DmReceiveError::DecryptFailed)?;

        // Step 12: sender-binding check (encrypted-payload layer).
        // Wire-layer says the signer's owner is `resolved_owner`; the
        // encrypted body's `sender` field MUST agree.
        crate::dm_crypto::verify_sender_binding(&payload, resolved_owner)
            .map_err(|_| DmReceiveError::SenderImpersonation)?;

        // Step 13a: apply_inbox — atomic-emit semantics.
        // Only `Inserted` (NOT `Merged` — composite-key collision via a
        // duplicate notify) populates newly_received. Caller emits
        // dm-received IPC ONLY for Inserted entries.
        let inbox_entry = crate::owner_state_types::InboxEntry {
            space_id: signed.space_id,
            message_cid: signed.message_cid,
            from: resolved_owner,
            received_at: Hlc {
                wall_ms: wall_now_ms,
                logical: 0,
                device_id: self.device_id.clone(),
            },
        };
        let outcome = state.apply_inbox(inbox_entry.clone());
        let mut drain_outcome = DrainOutcome::default();
        if matches!(outcome, ApplyOutcome::Inserted) {
            drain_outcome
                .newly_received
                .push(crate::owner_state_types::ReceivedMessage {
                    inbox_entry,
                    body: payload.body.clone(),
                    mime_type: payload.mime_type.clone(),
                    sent_at: payload.sent_at.clone(),
                });
        }

        // Step 13b: DmAck fan-out to all sender_devices.
        // Build the ack signed by US (our signing key, our device hash).
        // ack_from_devices = our currently-known devices for self_owner
        // from OwnerDeviceCache, falling back to just us if no entry yet
        // (pre-Flow-A-bootstrap). signing_device_hash MUST be in
        // ack_from_devices (envelope invariant) — fall back to a
        // single-element vec to satisfy that.
        let our_ack_devices = state
            .owner_device_cache
            .devices
            .get(&self.self_owner)
            .map(|e| e.devices.clone())
            .filter(|devs| devs.contains(&self.our_signing_device_hash))
            .unwrap_or_else(|| vec![self.our_signing_device_hash]);
        let ack_signed = crate::dm_envelope::DmAckSigned {
            space_id: signed.space_id,
            message_cid: signed.message_cid,
            ack_from_owner_addr: self.self_owner,
            ack_from_devices: our_ack_devices,
            signing_device_hash: self.our_signing_device_hash,
        };
        let ack_packet = crate::dm_envelope::build_signed_ack(ack_signed, &self.signing_key)
            .map_err(|e| DmReceiveError::Decode(format!("build_signed_ack: {e}")))?;
        let ack_wire = crate::dm_envelope::encode_packet(&ack_packet)
            .map_err(|e| DmReceiveError::Decode(format!("encode_packet ack: {e}")))?;

        // Compute one dest_hash per sender device + push UnicastSendRequest.
        // dest_hash = SHA256(name_hash("harmony.dm") || identity_address_hash)[:16]
        // — same convention the future Task 11 production resolver uses.
        // Failed sends are silent per spec — no retry on the ack itself.
        //
        // Use try_send (not send().await) because handle_cidnotify is invoked
        // from the same event-loop task that drains unicast_send_rx. .await
        // on a full channel would deadlock the event loop on itself. On
        // channel pressure we drop+warn; the sender's drain backoff
        // retransmits the underlying CidNotify, which produces a fresh ack
        // opportunity on the next inbound tick.
        for device in &signed.sender_devices {
            let dest_hash = crate::dm_signing::compute_dm_destination_hash(device.0);
            if let Err(e) = unicast_send_tx.try_send(UnicastSendRequest {
                destination_hash: dest_hash,
                packet: ack_wire.clone(),
            }) {
                tracing::warn!(
                    error = ?e,
                    "ack fan-out dropped due to channel pressure; sender will retransmit CidNotify"
                );
            }
        }

        Ok(drain_outcome)
    }

    /// Inbound `DmAck` handler — Phase 3b receive path for the sender side.
    ///
    /// Per ZEB-216 spec §"Application-signature binding rule" + Flow 3:
    ///   1. Look up signing pubkey via `lookup_pubkey_for_device`. None →
    ///      `UnknownSigningKey` (pre-bootstrap state — drop, telemetry).
    ///      Verify signature via `dm_signing::verify_dm_packet_signature`
    ///      (key-substitution defense + Ed25519 verify).
    ///   2. `resolve_signed_origin_owner(cache, signing_device_hash)` →
    ///      `resolved_owner` (UnknownSigningDevice / AmbiguousSigningDevice
    ///      drop on degenerate cache states).
    ///   3. `signed.ack_from_owner_addr ?= resolved_owner` — drop
    ///      `OwnerFieldMismatch` on cache-poisoning.
    ///   4. Look up the OutboxEntry by `(space_id, message_cid)`. Missing →
    ///      `OutboxEntryNotFound` (stale ack from before app restart, or
    ///      ack for an entry already swept).
    ///   5. Verify `resolved_owner ∈ entry.recipient_owners` —
    ///      `AckFromNonRecipient` (forged-ack regression).
    ///   6. Refresh OwnerDeviceCache with `signed.ack_from_devices` and
    ///      our newly-verified pubkey for the signer at the matching index.
    ///      Rejected outcome ignored — our cache may be fresher.
    ///   7. Call `mark_ack_delivered` to mutate `delivered_to`, recompute
    ///      `delivery_status`, and clear in-flight/backoff. Push into
    ///      `DrainOutcome.newly_delivered` if newly delivered (caller
    ///      emits `dm-delivered` IPC). `mark_ack_delivered` already calls
    ///      the CRDT `apply_outbox` path indirectly via direct mutation
    ///      with status recomputation — no separate apply needed.
    pub async fn handle_ack(
        &mut self,
        state: &mut OwnerState,
        signed: crate::dm_envelope::DmAckSigned,
        signature: [u8; 64],
        signed_bytes: &[u8],
        wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        // Step 1: look up signing pubkey + verify signature.
        let identity_pub =
            lookup_pubkey_for_device(&state.owner_device_cache, signed.signing_device_hash)
                .ok_or(DmReceiveError::UnknownSigningKey)?;
        crate::dm_signing::verify_dm_packet_signature(
            signed_bytes,
            &signature,
            &identity_pub,
            signed.signing_device_hash,
        )?;

        // Step 2: resolve signing_device_hash → OwnerAddr.
        let resolved_owner =
            resolve_signed_origin_owner(&state.owner_device_cache, signed.signing_device_hash)?;

        // Step 3: verify ack_from_owner_addr matches the resolved owner.
        // Drops cache-poisoning attempts where a peer claims an
        // ack_from_owner_addr that doesn't agree with the cryptographically-
        // authenticated source.
        if signed.ack_from_owner_addr != resolved_owner {
            return Err(DmReceiveError::OwnerFieldMismatch);
        }

        // Step 4: find the OutboxEntry by (space_id, message_cid). The
        // outbox is keyed by OutboxEntryId (a fresh ULID minted at send
        // time), so we iterate to locate the match. Missing entry =
        // stale ack from before app restart, or ack for an entry already
        // swept — drop with telemetry.
        let entry_id = state
            .outbox
            .iter()
            .find(|(_, e)| e.space_id == signed.space_id && e.message_cid == signed.message_cid)
            .map(|(id, _)| *id)
            .ok_or(DmReceiveError::OutboxEntryNotFound)?;

        // Step 5: forged-ack defense — resolved_owner MUST be in the
        // entry's recipient_owners. A peer NOT on the recipient list cannot
        // legitimately ack the message; their ack must not advance
        // delivered_to.
        //
        // No `space.members` gate parallel to `handle_cidnotify`'s membership
        // check is needed here. handle_cidnotify gates against the LIVE
        // space.members snapshot to block ex-members from injecting fresh
        // inbox writes. handle_ack instead gates against the OutboxEntry's
        // OWN `recipient_owners` snapshot, which was frozen at send time.
        // That's strictly stronger for the ack flow: a peer who was a member
        // at send time but was removed before acking is still a legitimate
        // recipient of the in-flight message — denying their ack would leak
        // delivery state. AckFromNonRecipient already covers the
        // ex-member-with-cached-key case (they were never in this entry's
        // recipient_owners), so a separate space.members lookup would be
        // redundant.
        let entry_ref = state
            .outbox
            .get(&entry_id)
            .expect("entry_id was just looked up from state.outbox");
        if !entry_ref.recipient_owners.contains(&resolved_owner) {
            return Err(DmReceiveError::AckFromNonRecipient);
        }

        // Step 6: refresh OwnerDeviceCache with ack.ack_from_devices.
        // Pubs vec: Some(identity_pub) at the signer's index, None at
        // every other index (Path B post-bootstrap — non-signer devices
        // remain pubs-less until the next signed packet from each
        // surfaces them). HLC uses our local wall clock + device_id.
        let mut updated_pubs: Vec<Option<[u8; 64]>> = vec![None; signed.ack_from_devices.len()];
        if let Some(idx) = signed
            .ack_from_devices
            .iter()
            .position(|d| *d == signed.signing_device_hash)
        {
            updated_pubs[idx] = Some(identity_pub);
        }
        // Ignore the apply outcome — Rejected (StaleHlc) is acceptable
        // here, our cache may already be fresher than what just arrived.
        let _ = state.apply_owner_device_update(
            resolved_owner,
            signed.ack_from_devices.clone(),
            updated_pubs,
            Hlc {
                wall_ms: wall_now_ms,
                logical: 0,
                device_id: self.device_id.clone(),
            },
        );

        // Step 7: mutate delivered_to + recompute delivery_status via
        // mark_ack_delivered (Phase 2's primitive — handles the
        // delivered_to insert, status recompute (Expired-sticky), and
        // in_flight/backoff cleanup). Returns true iff this was newly
        // delivered (not a duplicate). Caller emits dm-delivered IPC for
        // the entries in newly_delivered.
        let mut drain_outcome = DrainOutcome::default();
        if self.mark_ack_delivered(state, entry_id, resolved_owner) {
            drain_outcome
                .newly_delivered
                .push((entry_id, resolved_owner));
        }
        Ok(drain_outcome)
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
    /// Sender's resolved owner is not in space.members. Defends against
    /// ex-members whose signing key is still cached in OwnerDeviceCache.
    #[error("sender's resolved owner is not in space.members (ex-member with cached key?)")]
    SenderNotInSpaceMembers,
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
/// ascending-lex per its existing invariant (re-established by the
/// struct-level `Deserialize` impl on `OwnerDeviceEntry` on every load —
/// jointly with the parallel `device_identity_pubs` vec — see
/// `owner_state_types.rs:286-307`).
// Task 9 (handle_invite) does NOT call this — invites carry the inviter
// inline so resolution is unnecessary. Task 10 (handle_cidnotify) is the
// first consumer; Task 11 (handle_ack) will be the second.
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
    use crate::owner_state_types::{ContentId, DmContentKey, InboxEntry, Space, TransportBinding};

    /// Test-only helper: build a `DmOutbox` with synthetic signing key +
    /// device hash that don't actually verify against each other. Most
    /// tests in this module are sender-side / state-machine-only paths that
    /// never invoke handle_cidnotify (which is the only consumer of the new
    /// signing fields), so synthetic values suffice. Tests that DO verify
    /// signature bytes (the handle_invite suite, the handle_cidnotify suite
    /// added in Phase 3b Task 10) construct keys + hashes from
    /// `harmony_identity::PrivateIdentity::from_seed` directly.
    fn make_outbox_synthetic(device_id: &str, self_owner: OwnerAddr) -> DmOutbox {
        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]));
        DmOutbox::new(
            device_id.into(),
            self_owner,
            DeviceIdentityHash([0xaa; 16]),
            signing_key,
        )
    }

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
        let res = t.send(&e, r, Vec::new()).await;
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
            let _ = t.send(&e, r, Vec::new()).await;
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
        let o = make_outbox_synthetic("dev", OwnerAddr([0xaa; 16]));
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
        let mut o = make_outbox_synthetic("dev", alice);
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
    async fn send_dm_writes_self_inbox_entry_alongside_outbox_entry() {
        // Phase 4 self-history persistence: send_dm must write a self-InboxEntry
        // alongside the OutboxEntry, so self-sent messages survive past
        // OutboxEntry's lifetime (Complete entries can be GC'd; InboxEntry is
        // the durable scrollback record).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let _msg_id = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"hello".to_vec(),
                "text/plain".into(),
                1_000_000,
                None,
            )
            .await
            .expect("send_dm must succeed");

        // Self-InboxEntry exists at (space_id, message_cid) with from = self_owner.
        let self_inbox: Vec<&InboxEntry> = state
            .inbox
            .values()
            .filter(|e| e.space_id == space_id && e.from == o.self_owner)
            .collect();
        assert_eq!(
            self_inbox.len(),
            1,
            "send_dm must write exactly one self-InboxEntry"
        );
        assert_eq!(
            self_inbox[0].from, o.self_owner,
            "self-InboxEntry from = self_owner"
        );
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
        let mut o = make_outbox_synthetic("dev", alice);
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
        let mut o = make_outbox_synthetic("dev", OwnerAddr([0x01; 16]));
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

        let mut o = make_outbox_synthetic("dev", alice);
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

        let mut o = make_outbox_synthetic("dev", alice);
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
        let mut outbox = make_outbox_synthetic("device", OwnerAddr([0xff; 16]));
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
        let mut o = make_outbox_synthetic("dev", alice);
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
        let mut o = make_outbox_synthetic("dev", alice);
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

        let mut o = make_outbox_synthetic("dev", alice);
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
        let mut o = make_outbox_synthetic("dev", alice);
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
        let mut o = make_outbox_synthetic("dev", alice);
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
        let mut o = make_outbox_synthetic("dev", alice);
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
        let mut o = make_outbox_synthetic("dev", alice);

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
        let mut o = make_outbox_synthetic("dev", alice);

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
        let mut o = make_outbox_synthetic("dev", alice);
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

        let transport = RuntimeUnicastTransport::new(
            tx,
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
            .send(&entry, recipient, vec![dest_hash])
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

    /// Empty `destinations` → `TransportError::Transient` so drain bumps
    /// backoff and a future tick (after Flow A surfaces the missing
    /// `OwnerDeviceCache` entry) retries. Replaces the original
    /// resolver-based variant: resolution moved out of the transport,
    /// but the empty-list contract stayed at the transport boundary so
    /// existing drain unit tests (which exercise drain → StubTransport
    /// without populating OwnerDeviceCache) continue to work — only
    /// `RuntimeUnicastTransport` cares about destinations.
    #[tokio::test]
    async fn runtime_unicast_transport_no_known_devices_is_transient_error() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);
        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]));
        let our_device = DeviceIdentityHash([0xaa; 16]);

        let transport =
            RuntimeUnicastTransport::new(tx, OwnerAddr([0xff; 16]), our_device, signing_key);

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
            .send(&entry, OwnerAddr([1; 16]), Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, TransportError::Transient(_)),
            "empty destinations must surface as Transient (drives backoff retry), got {err:?}"
        );
    }

    #[tokio::test]
    async fn handle_invite_writes_space_and_cache_with_signing_pub() {
        use crate::owner_state_crdt::OwnerState;

        let mut state = OwnerState::default();
        let mut outbox = make_outbox_synthetic("device", OwnerAddr([2; 16]));

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
    async fn handle_invite_uses_local_wall_now_ms_for_cache_lww_not_remote_created_at() {
        // SECURITY regression: handle_invite previously fed
        // `signed.created_at` (attacker-controlled remote HLC) into
        // `apply_owner_device_update` as the LWW timestamp. A forged
        // far-future HLC (e.g., wall_ms = u64::MAX / 2) on a single
        // malicious invite would pin the local cache and reject every
        // legitimate future update from the same owner as `StaleHlc`
        // — a denial-of-updates attack.
        //
        // The fix uses our local `wall_now_ms` + `self.device_id` to
        // build the LWW HLC (mirroring `handle_cidnotify` step 8). The
        // assertion: after handle_invite, the cache entry's
        // `learned_at.wall_ms` MUST be `wall_now_ms`, NOT the remote
        // far-future value.
        use crate::owner_state_crdt::OwnerState;

        let mut state = OwnerState::default();
        let mut outbox = make_outbox_synthetic("local-dev", OwnerAddr([2; 16]));

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        // Forged far-future HLC — under the bug, this would be written
        // into the cache and lock out legitimate updates.
        let attacker_hlc = Hlc {
            wall_ms: u64::MAX / 2,
            logical: 0,
            device_id: "attacker".into(),
        };

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: attacker_hlc.clone(),
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        let local_wall_now_ms: u64 = 12345;
        outbox
            .handle_invite(
                &mut state,
                signed,
                signature,
                &body_bytes,
                local_wall_now_ms,
            )
            .await
            .unwrap();

        let entry = state
            .owner_device_cache
            .devices
            .get(&OwnerAddr([1; 16]))
            .expect("cache entry must exist after handle_invite");
        assert_eq!(
            entry.learned_at.wall_ms, local_wall_now_ms,
            "cache LWW HLC MUST use local wall_now_ms, NOT attacker-controlled created_at"
        );
        assert_ne!(
            entry.learned_at.wall_ms, attacker_hlc.wall_ms,
            "cache LWW HLC MUST NOT echo the remote far-future timestamp"
        );
        assert_eq!(
            entry.learned_at.device_id, "local-dev",
            "cache LWW HLC device_id MUST be OUR device_id"
        );
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
        let mut outbox = make_outbox_synthetic("device", OwnerAddr([2; 16]));

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
        let mut outbox = make_outbox_synthetic("device", OwnerAddr([2; 16]));

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
        let mut outbox = make_outbox_synthetic("device", OwnerAddr([2; 16]));

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
        let mut outbox = make_outbox_synthetic("device", OwnerAddr([99; 16]));

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
        let mut outbox = make_outbox_synthetic("device", OwnerAddr([2; 16]));

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

    // ── Phase 3b Task 10: handle_cidnotify tests ────────────────────────

    /// Build the standard receive-side fixture: a self-owner Bob (the
    /// `outbox`'s self_owner), a peer Alice with a known signing identity
    /// pre-seeded into Bob's
    /// `OwnerDeviceCache.devices[alice].device_identity_pubs`, a DM Space
    /// shared between them, and an InMemoryStub CAS pre-seeded with an
    /// encrypted MessagePayload addressed `from = alice`.
    ///
    /// Async because `cas.put` is on the `ContentStore` async trait — the
    /// in-memory impl never actually yields, but the interface forces an
    /// `await`.
    #[allow(clippy::too_many_arguments)]
    async fn build_cidnotify_fixture(
        space_id: SpaceId,
        space_kind: SpaceKind,
        alice: OwnerAddr,
        alice_seed: u8,
        bob: OwnerAddr,
        message_body: &[u8],
        content_key: DmContentKey,
    ) -> (
        OwnerState,
        InMemoryStub,
        crate::dm_envelope::DmCidNotifySigned,
        [u8; 64],
        Vec<u8>,
        DeviceIdentityHash,
        [u8; 64], // alice's full identity_pub (used by some tests)
        ContentId,
    ) {
        let mut state = OwnerState::default();
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[alice_seed; 32]);
        let alice_pub_id = private_alice.public_identity();
        let alice_identity_pub = alice_pub_id.to_public_bytes();
        let alice_device_hash = DeviceIdentityHash(alice_pub_id.address_hash);

        // Pre-seed Bob's view of Alice in OwnerDeviceCache (post-bootstrap).
        state.apply_owner_device_update(
            alice,
            vec![alice_device_hash],
            vec![Some(alice_identity_pub)],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        );

        // Build + install the shared DM Space (Bob is also a member).
        let mut sorted = [alice, bob];
        sorted.sort();
        let space = Space {
            id: space_id,
            kind: space_kind,
            parent: None,
            community_id: None,
            name: "Alice".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: sorted.to_vec(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            updated_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            content_key: Some(content_key.clone()),
            prior_content_keys: vec![],
        };
        let outcome = state.apply_space_with_canonicalization(space.clone());
        assert!(
            matches!(outcome, ApplyOutcome::Inserted),
            "fixture install failed: {outcome:?}"
        );

        // Encrypt MessagePayload under content_key + space's AAD.
        let payload = crate::dm_envelope::MessagePayload {
            body: message_body.to_vec(),
            mime_type: "text/plain".into(),
            sender: alice, // sender-binding correct
            sent_at: Hlc {
                wall_ms: 150,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        };
        let aad = crate::dm_crypto::compute_aad(&space).unwrap();
        let blob = crate::dm_crypto::encrypt_dm_message(&content_key, &aad, &payload).unwrap();

        // Compute message_cid + write to CAS.
        let message_cid = harmony_content::cid::ContentId::for_book(
            &blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        let cas = InMemoryStub::default();
        cas.put(message_cid, blob).await.unwrap();

        // Build + sign the DmCidNotify packet.
        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id,
            message_cid,
            sender_owner_addr: alice,
            sender_devices: vec![alice_device_hash],
            signing_device_hash: alice_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);

        (
            state,
            cas,
            signed,
            signature,
            signed_bytes,
            alice_device_hash,
            alice_identity_pub,
            message_cid,
        )
    }

    #[tokio::test]
    async fn handle_unicast_cidnotify_triggers_cas_fetch_decrypt_inbox_write() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let content_key = DmContentKey::new([0xab; 32]);
        let (mut state, cas, signed, signature, signed_bytes, _adev, _apub, message_cid) =
            build_cidnotify_fixture(
                space_id,
                SpaceKind::Dm,
                alice,
                0x42,
                bob,
                b"hi bob",
                content_key,
            )
            .await;

        let mut outbox = make_outbox_synthetic("bob-dev", bob);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

        let outcome = outbox
            .handle_cidnotify(
                &mut state,
                &cas,
                &tx,
                signed.clone(),
                signature,
                &signed_bytes,
                500,
            )
            .await
            .expect("happy path returns Ok");

        // InboxEntry written.
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id,
            message_cid,
        };
        assert!(
            state.inbox.contains_key(&inbox_key),
            "InboxEntry must be installed"
        );
        let entry = state.inbox.get(&inbox_key).unwrap();
        assert_eq!(entry.from, alice);

        // newly_received populated for the Inserted outcome.
        assert_eq!(outcome.newly_received.len(), 1);
        assert_eq!(outcome.newly_received[0].inbox_entry.from, alice);
        assert_eq!(outcome.newly_received[0].inbox_entry.space_id, space_id);

        // One UnicastSendRequest emitted per sender device (one device
        // here, so exactly one).
        let req = rx.try_recv().expect("ack must have been pushed");
        assert!(!req.packet.is_empty(), "ack packet bytes must be non-empty");
        assert!(rx.try_recv().is_err(), "exactly one ack expected");
        // Ack packet decodes as DmAck with our signing_device_hash.
        let ack = crate::dm_envelope::decode_packet(&req.packet).unwrap();
        match ack {
            crate::dm_envelope::DmPacket::Ack { signed: ack, .. } => {
                assert_eq!(ack.space_id, space_id);
                assert_eq!(ack.message_cid, message_cid);
                assert_eq!(ack.ack_from_owner_addr, bob);
            }
            other => panic!("expected DmAck, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_unicast_cidnotify_duplicate_no_dm_received_emit() {
        // Atomic-emit semantics: a duplicate notify (same composite key)
        // MUST NOT re-emit dm-received. apply_inbox returns NoOp/Merged
        // on the second call → newly_received stays empty. (The first
        // call's full-ack-fan-out behavior is exercised by the happy-path
        // test above; this test only checks the second-call
        // empty-newly_received contract.)
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let content_key = DmContentKey::new([0xab; 32]);
        let (mut state, cas, signed, signature, signed_bytes, _adev, _apub, _mcid) =
            build_cidnotify_fixture(
                space_id,
                SpaceKind::Dm,
                alice,
                0x42,
                bob,
                b"hi bob",
                content_key,
            )
            .await;

        let mut outbox = make_outbox_synthetic("bob-dev", bob);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

        // First call — happy path, newly_received populated.
        let first = outbox
            .handle_cidnotify(
                &mut state,
                &cas,
                &tx,
                signed.clone(),
                signature,
                &signed_bytes,
                500,
            )
            .await
            .expect("first call ok");
        assert_eq!(
            first.newly_received.len(),
            1,
            "first call must populate newly_received"
        );
        // Drain the ack the first call emitted so we can check the second
        // call's emit count cleanly.
        let _ = rx.try_recv();

        // Second call — same packet bytes; apply_inbox returns Merged
        // (composite key already exists), newly_received stays empty.
        let second = outbox
            .handle_cidnotify(&mut state, &cas, &tx, signed, signature, &signed_bytes, 600)
            .await
            .expect("second call ok (re-applies idempotently)");
        assert!(
            second.newly_received.is_empty(),
            "duplicate notify MUST NOT re-emit dm-received (atomic-emit semantics)"
        );
    }

    #[tokio::test]
    async fn handle_unicast_cidnotify_sender_binding_mismatch_drops() {
        // Set MessagePayload.sender to a DIFFERENT owner than the wire-
        // layer-resolved owner (alice). The signature still verifies
        // (signing_device_hash → alice's identity), the cache update
        // succeeds, but verify_sender_binding rejects with
        // SenderImpersonation. No InboxEntry, no ack.
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let content_key = DmContentKey::new([0xab; 32]);

        // Use the standard fixture but swap the encrypted payload's
        // `sender` field for a different OwnerAddr. We re-do the fixture
        // construction inline so we control the payload.sender mutation.
        let mut state = OwnerState::default();
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let alice_pub_id = private_alice.public_identity();
        let alice_identity_pub = alice_pub_id.to_public_bytes();
        let alice_device_hash = DeviceIdentityHash(alice_pub_id.address_hash);
        state.apply_owner_device_update(
            alice,
            vec![alice_device_hash],
            vec![Some(alice_identity_pub)],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        );

        let mut sorted = [alice, bob];
        sorted.sort();
        let space = Space {
            id: space_id,
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "Alice".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: sorted.to_vec(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            updated_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            content_key: Some(content_key.clone()),
            prior_content_keys: vec![],
        };
        state.apply_space_with_canonicalization(space.clone());

        // Encrypt with payload.sender = an attacker (NOT alice).
        let attacker = OwnerAddr([0xff; 16]);
        let payload = crate::dm_envelope::MessagePayload {
            body: b"forged".to_vec(),
            mime_type: "text/plain".into(),
            sender: attacker,
            sent_at: Hlc {
                wall_ms: 150,
                logical: 0,
                device_id: "attacker-dev".into(),
            },
        };
        let aad = crate::dm_crypto::compute_aad(&space).unwrap();
        let blob = crate::dm_crypto::encrypt_dm_message(&content_key, &aad, &payload).unwrap();
        let message_cid = harmony_content::cid::ContentId::for_book(
            &blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        let cas = InMemoryStub::default();
        cas.put(message_cid, blob).await.unwrap();

        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id,
            message_cid,
            sender_owner_addr: alice,
            sender_devices: vec![alice_device_hash],
            signing_device_hash: alice_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);

        let mut outbox = make_outbox_synthetic("bob-dev", bob);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

        let err = outbox
            .handle_cidnotify(&mut state, &cas, &tx, signed, signature, &signed_bytes, 500)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::SenderImpersonation),
            "expected SenderImpersonation, got {err:?}"
        );
        // No InboxEntry written.
        assert!(state.inbox.is_empty());
        // No ack emitted.
        assert!(rx.try_recv().is_err(), "ack MUST NOT fire on impersonation");
    }

    #[tokio::test]
    async fn handle_unicast_cidnotify_owner_field_mismatch_drops_no_cache_update() {
        // signed.sender_owner_addr ≠ resolved_owner. The signature
        // verifies, but Step 7c fails. Cache-poisoning regression:
        // apply_owner_device_update MUST NOT be called.
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let content_key = DmContentKey::new([0xab; 32]);
        let (mut state, cas, mut signed, _signature, _signed_bytes, _adev, _apub, _mcid) =
            build_cidnotify_fixture(
                space_id,
                SpaceKind::Dm,
                alice,
                0x42,
                bob,
                b"hi bob",
                content_key,
            )
            .await;

        // Swap sender_owner_addr to an attacker. Re-sign the modified
        // body so signature verification still passes — Step 7c is the
        // explicit defense, NOT a downstream signature failure.
        let attacker = OwnerAddr([0xff; 16]);
        signed.sender_owner_addr = attacker;
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let new_signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let new_signature = private_alice.sign(&new_signed_bytes);

        // Snapshot the cache state for alice to confirm Step 8 didn't fire.
        let alice_cache_before = state
            .owner_device_cache
            .devices
            .get(&alice)
            .cloned()
            .expect("fixture pre-seeded alice");

        let mut outbox = make_outbox_synthetic("bob-dev", bob);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

        let err = outbox
            .handle_cidnotify(
                &mut state,
                &cas,
                &tx,
                signed,
                new_signature,
                &new_signed_bytes,
                500,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::OwnerFieldMismatch),
            "expected OwnerFieldMismatch, got {err:?}"
        );
        // No InboxEntry, no ack.
        assert!(state.inbox.is_empty());
        assert!(rx.try_recv().is_err());
        // Cache for alice unchanged (NOT updated by Step 8 — Step 7c
        // returned BEFORE Step 8).
        let alice_cache_after = state.owner_device_cache.devices.get(&alice).unwrap();
        assert_eq!(
            alice_cache_after, &alice_cache_before,
            "cache MUST NOT be touched on OwnerFieldMismatch (cache-poisoning regression)"
        );
        // Attacker MUST NOT have been inserted into the cache.
        assert!(
            !state.owner_device_cache.devices.contains_key(&attacker),
            "attacker OwnerAddr MUST NOT be cached"
        );
    }

    #[tokio::test]
    async fn handle_unicast_cidnotify_unknown_signing_key_drops() {
        // signing_device_hash IS known in the cache, but the
        // device_identity_pubs[idx] entry is None (pre-bootstrap state —
        // we know-the-hash-but-not-the-pub). lookup_pubkey_for_device
        // returns None → UnknownSigningKey drop.
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);

        let mut state = OwnerState::default();
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let alice_pub_id = private_alice.public_identity();
        let alice_device_hash = DeviceIdentityHash(alice_pub_id.address_hash);

        // Pre-seed cache with the device hash but None for the pub.
        state.apply_owner_device_update(
            alice,
            vec![alice_device_hash],
            vec![None], // pre-bootstrap: hash known, pub not yet learned
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        );

        // Install a Space (so SpaceNotFound isn't the failure mode).
        let mut sorted = [alice, bob];
        sorted.sort();
        let space = Space {
            id: space_id,
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "Alice".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: sorted.to_vec(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            updated_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            content_key: Some(DmContentKey::new([0xab; 32])),
            prior_content_keys: vec![],
        };
        state.apply_space_with_canonicalization(space);

        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id,
            message_cid: ContentId::from_bytes([0xee; 32]),
            sender_owner_addr: alice,
            sender_devices: vec![alice_device_hash],
            signing_device_hash: alice_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);

        let cas = InMemoryStub::default();
        let mut outbox = make_outbox_synthetic("bob-dev", bob);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

        let err = outbox
            .handle_cidnotify(&mut state, &cas, &tx, signed, signature, &signed_bytes, 500)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::UnknownSigningKey),
            "expected UnknownSigningKey, got {err:?}"
        );
        // No InboxEntry, no ack — we never got past Step 7a.
        assert!(state.inbox.is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_unicast_cidnotify_decrypt_failure_uses_prior_keys() {
        // Space has prior_content_keys=[K1] and current key=K2. The blob
        // is encrypted under K1. decrypt_dm_message tries current first
        // (fails), then K1 (succeeds). InboxEntry written.
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let k1 = DmContentKey::new([0x11; 32]);
        let k2 = DmContentKey::new([0x22; 32]);

        let mut state = OwnerState::default();
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let alice_pub_id = private_alice.public_identity();
        let alice_identity_pub = alice_pub_id.to_public_bytes();
        let alice_device_hash = DeviceIdentityHash(alice_pub_id.address_hash);
        state.apply_owner_device_update(
            alice,
            vec![alice_device_hash],
            vec![Some(alice_identity_pub)],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        );

        let mut sorted = [alice, bob];
        sorted.sort();
        let space = Space {
            id: space_id,
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "Alice".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: sorted.to_vec(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            updated_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            content_key: Some(k2.clone()),        // current = K2
            prior_content_keys: vec![k1.clone()], // prior contains K1
        };
        state.apply_space_with_canonicalization(space.clone());

        // Encrypt under K1 (the OLD key) — decrypt MUST fall back.
        let payload = crate::dm_envelope::MessagePayload {
            body: b"recovered".to_vec(),
            mime_type: "text/plain".into(),
            sender: alice,
            sent_at: Hlc {
                wall_ms: 150,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        };
        let aad = crate::dm_crypto::compute_aad(&space).unwrap();
        let blob = crate::dm_crypto::encrypt_dm_message(&k1, &aad, &payload).unwrap();
        let message_cid = harmony_content::cid::ContentId::for_book(
            &blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        let cas = InMemoryStub::default();
        cas.put(message_cid, blob).await.unwrap();

        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id,
            message_cid,
            sender_owner_addr: alice,
            sender_devices: vec![alice_device_hash],
            signing_device_hash: alice_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);

        let mut outbox = make_outbox_synthetic("bob-dev", bob);
        let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

        let outcome = outbox
            .handle_cidnotify(&mut state, &cas, &tx, signed, signature, &signed_bytes, 500)
            .await
            .expect("prior-key fallback must succeed");

        assert_eq!(outcome.newly_received.len(), 1);
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id,
            message_cid,
        };
        assert!(state.inbox.contains_key(&inbox_key));
    }

    // ── Phase 3b Task 11: handle_ack tests ──────────────────────────────

    /// Build the standard sender-side ack-receive fixture: self-owner Alice
    /// (the outbox's self_owner) has previously sent a DM to Bob and the
    /// OutboxEntry is still Pending. Bob's signing identity is pre-seeded
    /// into Alice's `OwnerDeviceCache.devices[bob].device_identity_pubs`.
    /// Returns (state, signed_ack, signature, signed_bytes, outbox_entry_id).
    #[allow(clippy::type_complexity)]
    fn build_handle_ack_fixture(
        alice: OwnerAddr,
        bob: OwnerAddr,
        space_id: SpaceId,
        message_cid: ContentId,
    ) -> (
        OwnerState,
        crate::dm_envelope::DmAckSigned,
        [u8; 64],
        Vec<u8>,
        OutboxEntryId,
    ) {
        let mut state = OwnerState::default();
        let private_bob = harmony_identity::PrivateIdentity::from_seed(&[0x77; 32]);
        let bob_pub_id = private_bob.public_identity();
        let bob_identity_pub = bob_pub_id.to_public_bytes();
        let bob_device_hash = DeviceIdentityHash(bob_pub_id.address_hash);

        // Pre-seed Alice's view of Bob in OwnerDeviceCache (post-bootstrap).
        state.apply_owner_device_update(
            bob,
            vec![bob_device_hash],
            vec![Some(bob_identity_pub)],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "bob-dev".into(),
            },
        );

        // Install Alice's pending OutboxEntry — destined to bob.
        let entry_id = OutboxEntryId([0x77; 16]);
        let entry = OutboxEntry {
            id: entry_id,
            space_id,
            recipient_owners: vec![bob],
            message_cid,
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };
        match state.apply_outbox(entry) {
            ApplyOutcome::Inserted => {}
            other => panic!("fixture install failed: {other:?}"),
        }
        let _ = alice; // self_owner is used by callers via outbox, not state

        // Build + sign the DmAck packet.
        let signed = crate::dm_envelope::DmAckSigned {
            space_id,
            message_cid,
            ack_from_owner_addr: bob,
            ack_from_devices: vec![bob_device_hash],
            signing_device_hash: bob_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_bob.sign(&signed_bytes);

        (state, signed, signature, signed_bytes, entry_id)
    }

    #[tokio::test]
    async fn handle_unicast_ack_updates_outbox_delivered_to() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let message_cid = ContentId::from_bytes([0xee; 32]);
        let (mut state, signed, signature, signed_bytes, entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, message_cid);

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let outcome = outbox
            .handle_ack(&mut state, signed, signature, &signed_bytes, 500)
            .await
            .expect("happy path returns Ok");

        assert_eq!(
            outcome.newly_delivered,
            vec![(entry_id, bob)],
            "newly_delivered must contain (entry_id, bob) on first ack"
        );
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(stored.delivered_to.contains(&bob));
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    #[tokio::test]
    async fn handle_unicast_ack_owner_field_mismatch_drops() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let message_cid = ContentId::from_bytes([0xee; 32]);
        let (mut state, mut signed, _sig, _bytes, _entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, message_cid);

        // Swap ack_from_owner_addr to an attacker. Re-sign with bob's key
        // so Step 1 (signature verify) passes — Step 3 is the explicit
        // defense, NOT a downstream signature failure.
        let attacker = OwnerAddr([0xff; 16]);
        signed.ack_from_owner_addr = attacker;
        let private_bob = harmony_identity::PrivateIdentity::from_seed(&[0x77; 32]);
        let new_signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let new_signature = private_bob.sign(&new_signed_bytes);

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let err = outbox
            .handle_ack(&mut state, signed, new_signature, &new_signed_bytes, 500)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::OwnerFieldMismatch),
            "expected OwnerFieldMismatch, got {err:?}"
        );
    }

    #[tokio::test]
    async fn handle_unicast_ack_from_non_recipient_drops() {
        // resolved_owner is in OwnerDeviceCache but NOT in the
        // OutboxEntry's recipient_owners list — forged ack from a peer
        // who wasn't on the recipient list. MUST NOT advance delivered_to.
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let mallory = OwnerAddr([0x03; 16]);
        let space_id = SpaceId([7; 16]);
        let message_cid = ContentId::from_bytes([0xee; 32]);

        // Build the standard fixture (entry's recipient_owners = [bob]).
        let (mut state, _signed_bob, _sig_bob, _bytes_bob, entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, message_cid);

        // Now seed Mallory's identity into the cache too (cache-known but
        // NOT a legitimate recipient of this OutboxEntry).
        let private_mallory = harmony_identity::PrivateIdentity::from_seed(&[0x33; 32]);
        let mallory_pub_id = private_mallory.public_identity();
        let mallory_identity_pub = mallory_pub_id.to_public_bytes();
        let mallory_device_hash = DeviceIdentityHash(mallory_pub_id.address_hash);
        state.apply_owner_device_update(
            mallory,
            vec![mallory_device_hash],
            vec![Some(mallory_identity_pub)],
            Hlc {
                wall_ms: 60,
                logical: 0,
                device_id: "mallory-dev".into(),
            },
        );

        // Mallory crafts an ack and signs it with her own key.
        let signed = crate::dm_envelope::DmAckSigned {
            space_id,
            message_cid,
            ack_from_owner_addr: mallory,
            ack_from_devices: vec![mallory_device_hash],
            signing_device_hash: mallory_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_mallory.sign(&signed_bytes);

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let err = outbox
            .handle_ack(&mut state, signed, signature, &signed_bytes, 500)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::AckFromNonRecipient),
            "expected AckFromNonRecipient, got {err:?}"
        );
        // delivered_to must still be empty.
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(stored.delivered_to.is_empty());
        assert!(matches!(stored.delivery_status, DeliveryStatus::Pending));
    }

    #[tokio::test]
    async fn handle_unicast_ack_signature_invalid_drops() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let message_cid = ContentId::from_bytes([0xee; 32]);
        let (mut state, signed, mut signature, signed_bytes, entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, message_cid);

        // Flip a bit in the signature.
        signature[0] ^= 0xff;

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let err = outbox
            .handle_ack(&mut state, signed, signature, &signed_bytes, 500)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::SignatureVerificationFailed),
            "expected SignatureVerificationFailed, got {err:?}"
        );
        // No mutation to delivered_to.
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(stored.delivered_to.is_empty());
    }

    #[tokio::test]
    async fn handle_unicast_ack_outbox_entry_not_found_drops() {
        // DmAck for (space_id, message_cid) we never sent — no matching
        // OutboxEntry. Drop with OutboxEntryNotFound.
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let real_cid = ContentId::from_bytes([0xee; 32]);
        let (mut state, _signed, _sig, _bytes, _entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, real_cid);

        // Build a DmAck for a DIFFERENT message_cid (one we never sent).
        let unknown_cid = ContentId::from_bytes([0x99; 32]);
        let private_bob = harmony_identity::PrivateIdentity::from_seed(&[0x77; 32]);
        let bob_pub_id = private_bob.public_identity();
        let bob_device_hash = DeviceIdentityHash(bob_pub_id.address_hash);
        let signed = crate::dm_envelope::DmAckSigned {
            space_id,
            message_cid: unknown_cid,
            ack_from_owner_addr: bob,
            ack_from_devices: vec![bob_device_hash],
            signing_device_hash: bob_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_bob.sign(&signed_bytes);

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let err = outbox
            .handle_ack(&mut state, signed, signature, &signed_bytes, 500)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::OutboxEntryNotFound),
            "expected OutboxEntryNotFound, got {err:?}"
        );
    }

    // ── ZEB-227 PR #80 review fix: try_send pressure regressions ─────────

    #[tokio::test]
    async fn runtime_unicast_transport_send_returns_transient_when_channel_full() {
        // RuntimeUnicastTransport::send must NOT .await on a full channel
        // (deadlocks the event loop on itself). Verify it returns
        // Transient — drain's per-recipient backoff drives the retry.
        let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(1);
        // Pre-fill the channel so the first try_send inside `send` hits
        // TrySendError::Full.
        tx.try_send(UnicastSendRequest {
            destination_hash: [0u8; 16],
            packet: vec![],
        })
        .expect("pre-fill must succeed on a fresh channel");

        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));
        let transport = RuntimeUnicastTransport::new(
            tx,
            OwnerAddr([0x01; 16]),
            DeviceIdentityHash([0xaa; 16]),
            signing_key,
        );
        let entry = entry(7);
        // Non-empty destinations so we get past the empty-destinations
        // Transient short-circuit and exercise the channel-full path.
        let res = transport
            .send(&entry, OwnerAddr([0x02; 16]), vec![[0xbb; 16]])
            .await;
        match res {
            Err(TransportError::Transient(msg)) => {
                assert!(
                    msg.contains("unicast channel full"),
                    "expected 'unicast channel full' Transient, got: {msg}"
                );
            }
            other => panic!("expected Transient on full channel, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_cidnotify_ack_fanout_silent_on_channel_full() {
        // Ack fan-out on a saturated channel must not propagate as a
        // DmReceiveError; the CidNotify body still applies (InboxEntry
        // written, newly_received populated). Sender's drain backoff
        // retransmits the CidNotify, which reopens the ack window.
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let content_key = DmContentKey::new([0xab; 32]);
        let (mut state, cas, signed, signature, signed_bytes, _adev, _apub, message_cid) =
            build_cidnotify_fixture(
                space_id,
                SpaceKind::Dm,
                alice,
                0x42,
                bob,
                b"hi bob",
                content_key,
            )
            .await;

        let mut outbox = make_outbox_synthetic("bob-dev", bob);
        // Capacity 1, prefilled, so the ack-fan-out try_send hits Full.
        let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(1);
        tx.try_send(UnicastSendRequest {
            destination_hash: [0u8; 16],
            packet: vec![0xde, 0xad],
        })
        .expect("pre-fill must succeed on a fresh channel");

        let outcome = outbox
            .handle_cidnotify(
                &mut state,
                &cas,
                &tx,
                signed.clone(),
                signature,
                &signed_bytes,
                500,
            )
            .await
            .expect("CidNotify body must still apply even when ack channel is full");

        // InboxEntry was installed.
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id,
            message_cid,
        };
        assert!(
            state.inbox.contains_key(&inbox_key),
            "InboxEntry must still be installed despite dropped ack"
        );
        // newly_received populated for the Inserted outcome — ack drop
        // does NOT suppress dm-received emission.
        assert_eq!(outcome.newly_received.len(), 1);
        assert_eq!(outcome.newly_received[0].inbox_entry.from, alice);
    }

    #[tokio::test]
    async fn runtime_unicast_transport_send_returns_permanent_when_channel_closed() {
        // Channel closed = event-loop receiver dropped (runtime shutdown
        // or panic). retry will never succeed, so `send` must return
        // Permanent — drain converts that to OutboxEntry failure once
        // instead of spinning every drain tick.
        let (tx, rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);
        // Drop the receiver BEFORE calling send → try_send sees Closed.
        drop(rx);

        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));
        let transport = RuntimeUnicastTransport::new(
            tx,
            OwnerAddr([0x01; 16]),
            DeviceIdentityHash([0xaa; 16]),
            signing_key,
        );
        let entry = entry(7);
        // Non-empty destinations so we get past the empty-destinations
        // Transient short-circuit and exercise the channel-closed path.
        let res = transport
            .send(&entry, OwnerAddr([0x02; 16]), vec![[0xbb; 16]])
            .await;
        match res {
            Err(TransportError::Permanent(msg)) => {
                assert!(
                    msg.contains("event-loop channel closed"),
                    "expected 'event-loop channel closed' Permanent, got: {msg}"
                );
            }
            other => panic!("expected Permanent on closed channel, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_cidnotify_drops_when_resolved_owner_not_in_space_members() {
        // Defense-in-depth membership gate: a sender whose identity_pub
        // is still cached in OwnerDeviceCache (e.g., a former group-DM
        // member whose entry hasn't been expired) MUST NOT be able to
        // land a message in our inbox after their membership was
        // revoked. Signature verify and owner-mismatch both pass; the
        // space.members containment check is what stops them.
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let charlie = OwnerAddr([0x03; 16]);
        let space_id = SpaceId([7; 16]);
        let content_key = DmContentKey::new([0xab; 32]);

        // Build a fixture where Alice (the signer) IS still cached but
        // is NOT in space.members. We seed the fixture with Alice as the
        // sender (so her signing key + OwnerDeviceCache binding is in
        // place), then mutate the Space's members to drop Alice — the
        // "cached key from an ex-member" scenario.
        // Build under SpaceKind::Dm (2 members satisfies the DM
        // invariant) so the fixture's apply_space_with_canonicalization
        // accepts. After install, we direct-insert a mutated Space that
        // simulates the post-revocation GroupDm state where bob has
        // expelled alice but bob's local cache still has her key.
        let (mut state, cas, signed, signature, signed_bytes, alice_dev, _apub, message_cid) =
            build_cidnotify_fixture(
                space_id,
                SpaceKind::Dm,
                alice,
                0x42,
                bob,
                b"hi bob",
                content_key,
            )
            .await;

        // Snapshot Alice's cache entry to confirm the membership-gate drop
        // does NOT refresh OwnerDeviceCache (Step 8 must be gated behind
        // the membership check).
        let alice_cache_before = state
            .owner_device_cache
            .devices
            .get(&alice)
            .cloned()
            .expect("fixture pre-seeded alice");

        // Mutate the Space's members to remove Alice (the "ex-member"
        // scenario). Replace with bob + charlie so members is non-empty
        // (DM/group-DM invariant). Direct insertion bypasses
        // canonicalization — we're simulating the post-revocation state
        // where bob's local OwnerDeviceCache hasn't yet expired alice's
        // cached key.
        let mut sp = state.spaces.get(&space_id).cloned().unwrap();
        sp.members = vec![bob, charlie];
        sp.members.sort();
        state.spaces.insert(space_id, sp);

        // Sanity: the OwnerDeviceCache still binds alice's device → key
        // (the "cached key" precondition for this attack scenario).
        assert!(
            state.owner_device_cache.devices.contains_key(&alice),
            "alice's OwnerDeviceCache entry must persist (ex-member with cached key)"
        );

        let mut outbox = make_outbox_synthetic("bob-dev", bob);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);

        let err = outbox
            .handle_cidnotify(&mut state, &cas, &tx, signed, signature, &signed_bytes, 500)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::SenderNotInSpaceMembers),
            "expected SenderNotInSpaceMembers, got {err:?}"
        );

        // Inbox unchanged — no entry written for the rejected packet.
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id,
            message_cid,
        };
        assert!(
            !state.inbox.contains_key(&inbox_key),
            "InboxEntry MUST NOT be installed for an ex-member sender"
        );
        assert!(
            state.inbox.is_empty(),
            "inbox MUST remain empty on membership-gate drop"
        );

        // No ack fan-out — the membership gate fires before Step 13b.
        assert!(
            rx.try_recv().is_err(),
            "no ack must be emitted for an ex-member sender"
        );

        // OwnerDeviceCache unchanged — Step 8 (refresh from
        // notify.sender_devices) MUST NOT fire when the membership gate
        // rejects. Otherwise an ex-member could keep their cache entry
        // alive indefinitely by spamming signed CidNotifies.
        let alice_cache_after = state.owner_device_cache.devices.get(&alice).unwrap();
        assert_eq!(
            alice_cache_after, &alice_cache_before,
            "OwnerDeviceCache MUST NOT be refreshed on membership-gate drop"
        );
        // Sanity: the device hash didn't somehow leak elsewhere.
        let _ = alice_dev; // silence unused var warning if any
    }

    // ── Phase 4: delete_dm_outbox_entry (manual delete) ─────────────────
    //
    // Removes the OutboxEntry + the corresponding self-InboxEntry keyed
    // by `(space_id, message_cid)`, plus clears in_flight/backoff caches
    // so a stuck entry can't resurface.

    #[tokio::test]
    async fn delete_dm_outbox_entry_removes_outbox_and_self_inbox() {
        // Arrange: build a DM Space, send a DM (which writes both
        // OutboxEntry and self-InboxEntry), pre-populate in_flight +
        // backoff entries for that message to verify they're cleared.
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
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

        // Pre-condition: both records exist.
        let message_cid = state
            .outbox
            .get(&msg_id)
            .expect("outbox entry exists")
            .message_cid;
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id,
            message_cid,
        };
        assert!(
            state.inbox.contains_key(&inbox_key),
            "self-InboxEntry must exist before delete"
        );

        // Pre-populate in_flight + backoff so we can verify they're cleared.
        o.in_flight.insert((msg_id, bob));
        o.backoff.insert(
            (msg_id, bob),
            AttemptState {
                last_attempt_wall_ms: 1_000,
                failure_count: 1,
            },
        );

        // Act.
        let outcome = o
            .delete_dm_outbox_entry(&mut state, msg_id)
            .expect("delete_dm_outbox_entry ok");

        // Assert: OutboxEntry gone.
        assert!(
            !state.outbox.contains_key(&msg_id),
            "OutboxEntry must be removed"
        );
        // Self-InboxEntry gone.
        assert!(
            !state.inbox.contains_key(&inbox_key),
            "self-InboxEntry must be removed"
        );
        // in_flight + backoff cleared for this message_id (across all
        // recipients, not just bob).
        assert!(
            !o.in_flight.iter().any(|(eid, _)| *eid == msg_id),
            "in_flight must be cleared for deleted message_id"
        );
        assert!(
            !o.backoff.keys().any(|(eid, _)| *eid == msg_id),
            "backoff must be cleared for deleted message_id"
        );

        // Outcome carries the IPC payload data.
        assert_eq!(outcome.deleted_outbox_id, Some(msg_id));
        assert_eq!(outcome.deleted_inbox_key, Some(inbox_key));
        assert_eq!(outcome.space_id, Some(space_id));
        assert_eq!(outcome.message_cid, Some(message_cid));
    }

    #[tokio::test]
    async fn delete_dm_outbox_entry_idempotent_on_missing() {
        // Arrange: empty state, no OutboxEntry exists.
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let mut o = make_outbox_synthetic("dev", alice);
        let fake_id = OutboxEntryId([0xff; 16]);

        // Act.
        let outcome = o
            .delete_dm_outbox_entry(&mut state, fake_id)
            .expect("idempotent: no error on missing");

        // Assert: all-None outcome, no error.
        assert_eq!(outcome.deleted_outbox_id, None);
        assert_eq!(outcome.deleted_inbox_key, None);
        assert_eq!(outcome.space_id, None);
        assert_eq!(outcome.message_cid, None);
    }
}
