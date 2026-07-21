//! ZEB-458 P4 Phase B: PRODUCTION context impls for the community-relay
//! deposit + pull acceptors.
//!
//! Phase A ([`crate::iroh_community_relay_acceptor`]) defined the Tauri-free
//! pipelines against two injectable context traits — [`RelayDepositCtx`] and
//! [`RelayPullCtx`] — with only test-mock impls. This module supplies their
//! production implementations, backed by the relay's replicated state:
//!
//! - [`ProdRelayDepositCtx`] — admission (opt-in + co-membership) + opaque
//!   persist-with-caps over [`RelayHoldDoc`] (mirrors
//!   [`crate::iroh_butler_acceptor::ProdButlerDepositCtx::persist_entry`]).
//! - [`ProdRelayPullCtx`] — serve/membership gates + recipient-scoped
//!   `held_for` + ack→pulled_by→flush (GC is a separate periodic sweep).
//!
//! Both ctxs answer community-membership questions through a
//! [`CommunityMembershipLookup`] seam rather than reaching into the community
//! state registry directly. This keeps them unit-testable with a fake (no full
//! `CommunitySyncRegistry`); the registry-backed lookup impl is wired in at
//! `start_node` in a later task (T11).
//!
//! The flush seam ([`RelayHoldFlush`]) decouples both ctx structs from the
//! concrete `FleetSyncEngine<RelayHoldDoc>`, making `persist_hold` and
//! `mark_pulled` unit-testable without constructing a real engine. Production
//! wires [`EngineRelayHoldFlush`] at `start_node` (T11).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use zeroize::Zeroizing;

use crate::butler_deposit::DepositPayload;
use crate::community_relay::{RelayHeldBlob, RELAY_HOLD_GLOBAL_CAP, RELAY_HOLD_PER_SENDER_CAP};
use crate::community_relay_hold_crdt::{RelayHoldDoc, RelayHoldEntry};
use crate::community_relay_optin::RelayOptInDoc;
use crate::community_relay_pull::RelayIngestCtx;
use crate::fleet_sync::FleetSyncEngine;
use crate::iroh_community_relay_acceptor::{RelayDepositCtx, RelayPersistVerdict, RelayPullCtx};
use crate::owner_state_types::{Hlc, InboxEntry, ReceivedMessage, SpaceId};

// =====================================================================
// Membership seam
// =====================================================================

/// Answers community-membership questions for the relay ctxs. The prod impl
/// (registry-backed) is wired at start_node (T11); tests use a fake. This seam
/// keeps the ctxs unit-testable without a full `CommunitySyncRegistry`.
#[async_trait]
pub trait CommunityMembershipLookup: Send + Sync {
    /// Is `owner` a Joined member of `community_id` in the relay's replicated
    /// state?
    async fn is_joined(&self, community_id: &SpaceId, owner: &[u8; 16]) -> bool;
}

/// Production [`CommunityMembershipLookup`] backed by the live
/// [`CommunitySyncRegistry`]. Wired at start_node (T11b). Resolves a community's
/// engine, materializes its membership against the engine's configured admin
/// addr, and answers `is_joined` by the member's `Joined` status. A community
/// with no spawned engine (never joined / left + GC'd) answers `false`.
pub struct ProdCommunityMembershipLookup {
    pub registry: Arc<crate::community_state_sync::CommunitySyncRegistry>,
}

#[async_trait]
impl CommunityMembershipLookup for ProdCommunityMembershipLookup {
    async fn is_joined(&self, community_id: &SpaceId, owner: &[u8; 16]) -> bool {
        let Some(engine) = self.registry.engine_arc(community_id).await else {
            return false;
        };
        let admin = engine.admin_addr();
        let state = engine.state();
        let guard = state.lock().await;
        guard
            .materialized(admin)
            .members
            .get(&crate::owner_state_types::OwnerAddr(*owner))
            .map(|m| m.status == crate::community_membership::MemberStatus::Joined)
            .unwrap_or(false)
    }
}

/// Wall-clock epoch SECONDS (for `EnrollmentCert` expiry checks). Mirrors
/// [`crate::iroh_butler_acceptor::ProdButlerDepositCtx::now_secs`].
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// =====================================================================
// RelayHoldFlush seam
// =====================================================================

/// Decouples the relay ctxs from the concrete `FleetSyncEngine<RelayHoldDoc>`
/// so the ctx methods (incl. cap enforcement) are unit-testable with a no-op
/// flush. Production wires `EngineRelayHoldFlush(engine_arc)` at start_node
/// (T11).
#[async_trait]
pub trait RelayHoldFlush: Send + Sync {
    fn notify_dirty(&self);
    async fn flush_now(&self) -> Result<(), String>;
}

/// Production flush seam over the real fleet-sync engine.
pub struct EngineRelayHoldFlush(
    pub Arc<FleetSyncEngine<crate::community_relay_hold_crdt::RelayHoldDoc>>,
);

#[async_trait]
impl RelayHoldFlush for EngineRelayHoldFlush {
    fn notify_dirty(&self) {
        self.0.notify_dirty();
    }
    async fn flush_now(&self) -> Result<(), String> {
        self.0
            .flush_now()
            .await
            .map_err(|e| format!("flush_now: {e}"))
    }
}

// =====================================================================
// ProdRelayDepositCtx
// =====================================================================

/// Production [`RelayDepositCtx`]: admission via the opt-in doc + the
/// membership seam, and an atomic persist-with-caps over the relay's
/// [`RelayHoldDoc`] that durably flushes BEFORE the deposit acks (D7).
///
/// `persist_hold` mirrors
/// [`crate::iroh_butler_acceptor::ProdButlerDepositCtx::persist_entry`]:
/// occupied key → `Duplicate` (caps bypassed, still Ok); vacant within caps →
/// insert + flush → `Inserted`; vacant over caps → `CapExceeded` (nothing
/// inserted / flushed). There is NO local ingest sweeper to nudge on a relay
/// (the relay is never the recipient of the blobs it holds), so that step is
/// omitted.
pub struct ProdRelayDepositCtx {
    /// This relay owner's address bytes (`OwnerAddr.0`), checked for
    /// self-membership in `serves_community`.
    pub self_owner: [u8; 16],
    /// This relay device's id (64-hex of the device ed25519 verify key),
    /// stamped as `held_by`.
    pub relay_device_id: String,
    /// Runtime relay-hold CRDT (same Arc the engine owns).
    pub relay_hold_doc: Arc<tokio::sync::Mutex<RelayHoldDoc>>,
    /// HLC tracker for minting `held_at`.
    pub relay_hold_tracker: Arc<tokio::sync::Mutex<BTreeMap<String, Hlc>>>,
    /// Flush seam (notify_dirty + flush_now over the fleet-sync engine).
    pub flush: Arc<dyn RelayHoldFlush>,
    /// Runtime per-community relay opt-in doc.
    pub optin: Arc<tokio::sync::Mutex<RelayOptInDoc>>,
    /// Community-membership lookup seam (registry-backed at start_node, T11).
    pub membership: Arc<dyn CommunityMembershipLookup>,
}

#[async_trait]
impl RelayDepositCtx for ProdRelayDepositCtx {
    fn relay_device_id(&self) -> String {
        self.relay_device_id.clone()
    }

    async fn serves_community(&self, community_id: &SpaceId) -> bool {
        // Opted in to relay for this community AND a Joined member of it.
        self.optin.lock().await.is_opted_in(community_id)
            && self
                .membership
                .is_joined(community_id, &self.self_owner)
                .await
    }

    async fn both_co_members(
        &self,
        community_id: &SpaceId,
        sender_owner: &[u8; 16],
        recipient_owner: &[u8; 16],
    ) -> bool {
        self.membership.is_joined(community_id, sender_owner).await
            && self
                .membership
                .is_joined(community_id, recipient_owner)
                .await
    }

    fn now_secs(&self) -> u64 {
        now_epoch_secs()
    }

    async fn mint_hlc(&self) -> Hlc {
        crate::fleet_sync::mint_next_hlc(&self.relay_hold_tracker, &self.relay_device_id).await
    }

    async fn persist_hold(
        &self,
        key: String,
        entry: RelayHoldEntry,
    ) -> Result<RelayPersistVerdict, String> {
        // Caps + insert INSIDE the doc-lock critical section: counting and
        // inserting under one lock acquisition means concurrent deposits can
        // never overshoot the quotas. Mirrors
        // `ProdButlerDepositCtx::persist_entry`.
        let verdict = {
            let mut doc = self.relay_hold_doc.lock().await;
            if doc.entries.contains_key(&key) {
                // Occupied key: insert-once leaves it untouched and the caps
                // are BYPASSED — the entry is already stored, so a redelivery
                // after a lost ack re-acks idempotently even at a full hold.
                // Falls through to the flush below (D7).
                RelayPersistVerdict::Duplicate
            } else {
                // Community-scoped per-sender cap (matches count_for_sender's
                // (community_id, sender_owner) filter) + global cap.
                let sender_pending = doc.count_for_sender(&entry.community_id, &entry.sender_owner);
                let total = doc.live_count();
                if sender_pending >= RELAY_HOLD_PER_SENDER_CAP || total >= RELAY_HOLD_GLOBAL_CAP {
                    // Nothing inserted → nothing to flush; the doc is exactly
                    // as it was.
                    return Ok(RelayPersistVerdict::CapExceeded);
                }
                doc.entries.insert(key, entry);
                RelayPersistVerdict::Inserted
            }
        };
        // notify_dirty BEFORE flush_now: if the flush's publish leg fails, the
        // engine's swap-and-restore keeps the dirty latch armed so a later
        // debounce retries the publish.
        self.flush.notify_dirty();
        // Durable persist + publish BEFORE the ack exists (D7). Flushed even
        // for a Duplicate key: if the FIRST deposit's flush failed after the
        // in-memory insert, the retry hits the occupied entry — skipping the
        // flush here would ack an entry that was never made durable.
        self.flush.flush_now().await?;
        // No local ingest sweeper to nudge: a relay is never the recipient of
        // the blobs it holds (unlike the butler, which is itself a recipient
        // device).
        Ok(verdict)
    }
}

// =====================================================================
// ProdRelayPullCtx
// =====================================================================

/// Production [`RelayPullCtx`]: serve/membership gates via the opt-in doc +
/// the membership seam, a recipient-scoped `held_for`, and an ack handler that
/// unions `pulled_by` into the present keys and durably flushes. GC is a
/// SEPARATE periodic sweep — NOT run inline here — so `pulled_by` replicates
/// to the relay's fleet before any replica removes the entry. See
/// `mark_pulled` for the full rationale.
pub struct ProdRelayPullCtx {
    /// This relay owner's address bytes, checked in `serves_community`.
    pub self_owner: [u8; 16],
    /// Runtime relay-hold CRDT (same Arc the engine owns).
    pub relay_hold_doc: Arc<tokio::sync::Mutex<RelayHoldDoc>>,
    /// Flush seam (notify_dirty + flush_now over the fleet-sync engine).
    pub flush: Arc<dyn RelayHoldFlush>,
    /// Runtime per-community relay opt-in doc.
    pub optin: Arc<tokio::sync::Mutex<RelayOptInDoc>>,
    /// Community-membership lookup seam (registry-backed at start_node, T11).
    pub membership: Arc<dyn CommunityMembershipLookup>,
}

#[async_trait]
impl RelayPullCtx for ProdRelayPullCtx {
    async fn serves_community(&self, community_id: &SpaceId) -> bool {
        self.optin.lock().await.is_opted_in(community_id)
            && self
                .membership
                .is_joined(community_id, &self.self_owner)
                .await
    }

    async fn is_joined_member(&self, community_id: &SpaceId, owner: &[u8; 16]) -> bool {
        self.membership.is_joined(community_id, owner).await
    }

    fn now_secs(&self) -> u64 {
        now_epoch_secs()
    }

    async fn held_for(
        &self,
        recipient_owner: &[u8; 16],
        requester_device: &str,
    ) -> Vec<(String, RelayHeldBlob)> {
        let doc = self.relay_hold_doc.lock().await;
        doc.entries
            .iter()
            // Recipient-scoped AND not yet pulled+acked by THIS device, so a
            // device drains its backlog without re-serving acked-but-not-yet-
            // GC'd entries (GC is a later periodic sweep).
            .filter(|(_, e)| {
                &e.recipient_owner == recipient_owner && !e.pulled_by.contains(requester_device)
            })
            .map(|(k, e)| {
                (
                    k.clone(),
                    RelayHeldBlob {
                        sender_owner: e.sender_owner,
                        sealed_blob: e.sealed_blob.clone(),
                    },
                )
            })
            .collect()
    }

    async fn mark_pulled(&self, keys: &[String], requester_device: String) -> Result<(), String> {
        // Union the requester into pulled_by for every key present (missing
        // keys = no-op, so an ack for an already-GC'd blob never errors).
        //
        // GC is a SEPARATE periodic sweep (not inline) so `pulled_by` replicates
        // to the relay's fleet before any replica removes the entry.
        // `RelayHoldDoc::merge_from` is a grow-only union: an early local delete
        // would be resurrected by a sibling relay that has not yet seen the pull
        // (spec D38). The periodic sweep runs AFTER `pulled_by` has propagated;
        // any transient resurrection carries `pulled_by` and is re-removed on the
        // next sweep.
        {
            let mut doc = self.relay_hold_doc.lock().await;
            for k in keys {
                if let Some(e) = doc.entries.get_mut(k) {
                    e.pulled_by.insert(requester_device.clone());
                }
            }
        }
        // Durable persist + publish of the pulled_by union so covered state
        // replicates to siblings before the periodic sweep reclaims storage.
        // notify_dirty BEFORE flush_now (publish-retry latch, as in deposit).
        self.flush.notify_dirty();
        self.flush.flush_now().await?;
        Ok(())
    }
}

// =====================================================================
// ProdRelayIngestCtx
// =====================================================================

/// Production [`RelayIngestCtx`] (ZEB-458 P4 Phase B): the recipient-side ingest
/// for blobs pulled off a community relay. After
/// [`crate::community_relay_pull::open_and_ingest`] opens a sealed blob to one
/// of this owner's device keys and decodes the [`DepositPayload`], this ctx runs
/// it through the SAME normal DM receive path a direct arrival takes — so a
/// relayed message and a directly-delivered one are indistinguishable to the UI
/// and to persistence.
///
/// `ingest_recovered` mirrors [`crate::dm_inbox_ingest::ProdDmInboxIngestCtx`]'s
/// `cas_put` → `verify` → `apply_inbox` → emit sequence, reusing the SAME
/// `pub(crate)` receive-path helpers (`dm_outbox::verify_cidnotify_admission`,
/// `dm_outbox::decrypt_and_bind_dm_blob`) under the owner-state lock, and the
/// SAME `dm-received` event. There is NO separate `sender_owner` field on a
/// `DepositPayload` (unlike the butler's `DmInboxEntry`), so the sender is
/// derived from the signed CidNotify packet itself by
/// `verify_cidnotify_admission` (`resolved_owner`), exactly as every other
/// receive path resolves it — no out-of-band sender claim to cross-check.
///
/// `Err(reason)` from any step leaves the blob unacked (the relay keeps holding
/// it; a later pull retries), so the relay's coverage-GC `ack ⟹ ingested`
/// invariant holds.
pub struct ProdRelayIngestCtx {
    /// This device's SP1 device id (64-hex of the device ed25519 verify key),
    /// stamped as `received_at.device_id` on the inbox entry (mirrors the
    /// normal path's `outbox_g.device_id`).
    pub device_id: String,
    /// X25519 private keys for each enrolled device this node holds (one per
    /// device). `open_and_ingest` tries each against the sealed blob; in
    /// practice a single device holds one key.
    pub device_x25519_privs: Vec<Zeroizing<[u8; 32]>>,
    /// ZEB-483: this node's own `OwnerAddr` (the deposit recipient). Threaded
    /// into `apply_deposited_invite`'s `apply_invite` recipient-membership gate
    /// when bootstrapping the DM Space from a relay-held deposited invite on
    /// recover (mirrors `ProdDmInboxIngestCtx::self_owner`).
    pub self_owner: crate::owner_state_types::OwnerAddr,
    /// The runtime owner-state CRDT (`NodeState`'s `crdt_state` Arc) — admission
    /// (`verify_cidnotify_admission`) + `apply_inbox` happen under this lock.
    pub crdt_state: Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    /// The shared CAS handle (`RuntimeContentStore` in production) — the storage
    /// blob is put here so the message body is fetchable like a direct arrival.
    pub content_store: Arc<dyn crate::content_store::ContentStore>,
    /// Event sink for the `dm-received` emit (ZEB-445) and the ZEB-236
    /// `dm-invite-received` / `dm-invite-list-changed` emits.
    pub sink: Arc<dyn crate::node_event_sink::NodeEventSink>,
    /// ZEB-236 (T3): the process-local staged-invite store (from `NodeState`).
    /// A relay-recovered non-friend invite (invite-only or co-deposited) is
    /// staged here + surfaced via `sink`. `None` only in unit tests.
    pub pending_dm_invites: Option<Arc<crate::pending_dm_invites::PendingDmInvites>>,
    /// ZEB-580 S2: the shared-community revocation projection — forwarded to
    /// `verify_cidnotify_sender_binding` for the CidNotify signer-device
    /// cutoff (bare Arc-backed handle, matches `MembershipProjection`'s
    /// by-value-clone style).
    pub revoked: crate::revoked_device_projection::RevokedDeviceProjection,
    /// ZEB-709: arms the OWNER-STATE `SyncEngine` flush after this ctx writes
    /// into `crdt_state` (inbox insert / invite auto-accept). Without it the
    /// relay ack persists+replicates (relay coverage-GC destroys the held
    /// blob) while the payload sits un-notified in memory — a crash in that
    /// window loses the message permanently. Mirrors
    /// `ProdDmInboxIngestCtx::notify_owner_state_dirty`. `None` only in unit
    /// tests.
    pub notify_owner_state_dirty: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[async_trait]
impl RelayIngestCtx for ProdRelayIngestCtx {
    fn device_x25519_privs(&self) -> Vec<Zeroizing<[u8; 32]>> {
        self.device_x25519_privs.clone()
    }

    async fn ingest_recovered(
        &self,
        sender_owner: [u8; 16],
        payload: DepositPayload,
    ) -> Result<(), String> {
        // ZEB-505: an invite-only recovered deposit (no CidNotify) bootstraps
        // the DM Space from the invite ALONE — no blob, no message, no emit.
        // The inviter is bound to the AUTHENTICATED relay `sender_owner` (the
        // relay verified the deposit frame's signature over it) — NOT the
        // payload-controlled `signed.inviter`. `apply_invite`'s signature check
        // authenticates only the signing DEVICE, so without an authenticated
        // `expected_inviter` an admitted co-member could recover a blob
        // attributing the Space to a different owner (Qodo: the relay pull path
        // carries `RelayHeldBlob.sender_owner`; bind to it, mirroring the butler
        // acceptor's `frame.sender_owner` bind).
        let Some(cidnotify_bytes) = payload.cidnotify_packet.as_deref() else {
            let invite = payload
                .invite_packet
                .as_deref()
                .ok_or("ZEB-505: invite-only deposit missing invite_packet")?;
            let inv_packet = crate::dm_envelope::decode_packet(invite)
                .map_err(|e| format!("decode invite: {e}"))?;
            let crate::dm_envelope::DmPacket::Invite {
                signed,
                signature,
                signed_bytes,
            } = inv_packet
            else {
                return Err("ZEB-505: invite-only deposit packet is not an Invite".into());
            };
            if signed.inviter.0 != sender_owner {
                return Err(
                    "ZEB-505: invite inviter does not match authenticated relay sender".into(),
                );
            }
            // Capture for the ZEB-639 ignore log (`signed` moves into apply_invite).
            let invite_space_id = signed.space_id;
            // Scope the lock to `apply_invite`, then DROP the guard before
            // staging + emitting (store `Mutex` + sink must not nest inside the
            // held `crdt_state` lock — ZEB-236 T3).
            let outcome = {
                let mut state = self.crdt_state.lock().await;
                crate::dm_outbox::apply_invite(
                    &mut state,
                    self.self_owner,
                    &self.device_id,
                    signed,
                    signature,
                    &signed_bytes,
                    now_epoch_ms(),
                    Some(crate::owner_state_types::OwnerAddr(sender_owner)),
                    false,
                    &self.revoked,
                )
                .map_err(|e| format!("apply_invite: {e:?}"))?
            };
            match outcome {
                // ZEB-236 (T3): stage the non-friend invite + surface it to the UI.
                crate::dm_outbox::ApplyInviteOutcome::Staged(staged) => {
                    crate::pending_dm_invites::stage_and_emit_staged_invite(
                        self.pending_dm_invites.as_ref(),
                        self.sink.as_ref(),
                        staged,
                    );
                }
                crate::dm_outbox::ApplyInviteOutcome::Accepted => {
                    // ZEB-709: the auto-accept wrote the DM Space into
                    // owner-state — arm the owner-state flush (Staged/Ignored
                    // return before any owner-state write).
                    if let Some(mark) = &self.notify_owner_state_dirty {
                        mark();
                    }
                    // ZEB-640 (1): a friend-tier auto-accept makes any EARLIER
                    // staged (pre-befriend) entry for this space stale — the
                    // Space now exists in nav, so purge the pending row.
                    crate::pending_dm_invites::purge_stale_staged_on_accept(
                        self.pending_dm_invites.as_ref(),
                        self.sink.as_ref(),
                        &invite_space_id,
                    );
                }
                // ZEB-639: non-friend invite for a space we already hold.
                // ZEB-642 (1): purge the stale staged row (see the
                // dm_inbox_ingest tunnel arm).
                crate::dm_outbox::ApplyInviteOutcome::IgnoredExistingSpace => {
                    tracing::debug!(
                        space_id = ?invite_space_id,
                        "relay invite ignored: space already exists locally (non-friend inviter)"
                    );
                    crate::pending_dm_invites::purge_stale_staged_on_accept(
                        self.pending_dm_invites.as_ref(),
                        self.sink.as_ref(),
                        &invite_space_id,
                    );
                }
            }
            return Ok(());
        };
        // 1. Decode the recovered packet — must be a signed CidNotify.
        let packet = crate::dm_envelope::decode_packet(cidnotify_bytes)
            .map_err(|e| format!("decode_packet: {e}"))?;
        let crate::dm_envelope::DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        } = packet
        else {
            return Err("recovered packet is not a CidNotify".into());
        };

        // 2. CAS-put the storage blob FIRST so the body is fetchable at the
        //    packet's message_cid before anything references the CID — exactly
        //    as the dm-inbox sweeper does (idempotent on content address).
        let computed_cid = harmony_content::cid::ContentId::for_book(
            &payload.storage_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .map_err(|e| format!("for_book: {e:?}"))?;
        self.content_store
            .put(computed_cid, payload.storage_blob.clone())
            .await
            .map_err(|e| format!("cas put: {e}"))?;

        // 3. Normal receive-path admission under the owner-state lock
        //    (signature, owner resolution, Space lookup, SpaceKind gate,
        //    membership). The sender identity (`resolved_owner`) is derived
        //    here from the packet — there is no separate sender claim to bind.
        //    `apply_inbox` runs under the SAME lock acquisition so admission and
        //    the durable insert are atomic, matching the direct path.
        // ZEB-640 (1): set inside the lock scope when `apply_deposited_invite`
        // consumed a co-deposited invite WITHOUT staging (`Ok(None)`:
        // friend-tier auto-accept, or the Space already exists) — any EARLIER
        // staged (pre-befriend) entry for this space is stale. The purge itself
        // runs AFTER the lock scope (helper caller contract).
        // ZEB-642 (2): relay's skip-window EXTENDS THROUGH apply_inbox — the lock scope encloses blob-binding/decrypt/apply_inbox, any of whose Err returns skips the purge until the next delivery.
        let mut purge_stale_staged = false;
        let (resolved_owner, payload_msg, newly_inserted, received_at, space_id, message_cid) = {
            let mut state = self.crdt_state.lock().await;
            // ZEB-483 (CodeRabbit Critical): verify the CidNotify's signer
            // against the PRISTINE OwnerDeviceCache FIRST, then let any deposited
            // invite bootstrap ONLY the missing DM Space — pinned to that
            // verified sender. Applying the invite first would let it seed the
            // `device → owner → pub` rows the admission then reads, so a forged
            // invite + notify could verify against cache the untrusted invite
            // just wrote (circular trust). On the legitimate offline path the
            // sender is an existing co-member/friend whose device is already
            // cached here, so it resolves WITHOUT the invite; the invite supplies
            // only the Space. `?` leaves the blob unacked so the relay drain
            // retries (fail-closed).
            let (resolved_owner, identity_pub) = crate::dm_outbox::verify_cidnotify_sender_binding(
                &state,
                &signed,
                &signature,
                &signed_bytes,
                &self.revoked,
            )
            .map_err(|e| format!("verify_cidnotify_sender_binding: {e:?}"))?;
            // ZEB-236 (T3): a co-deposited invite from a non-friend is STAGED
            // (writes no Space); `apply_deposited_invite` hands it back here.
            let staged_invite = if let Some(inv) = payload.invite_packet.as_ref() {
                match crate::dm_outbox::apply_deposited_invite(
                    &mut state,
                    self.self_owner,
                    &self.device_id,
                    inv,
                    resolved_owner,
                    signed.space_id,
                    signed.signing_device_hash,
                    identity_pub,
                    now_epoch_ms(),
                    &self.revoked,
                )? {
                    Some(staged) => Some(staged),
                    // ZEB-640 (1): invite consumed without staging (friend-tier
                    // accept / space exists) → flag the post-lock purge.
                    None => {
                        purge_stale_staged = true;
                        // ZEB-709: the friend-tier accept may have written the
                        // DM Space into owner-state inside this lock — arm the
                        // flush HERE so even the unacked-Err exits below keep
                        // it durable (mirrors the dm_inbox_ingest co-deposit
                        // arm; a spurious arm on the space-exists no-op is a
                        // harmless debounced persist).
                        if let Some(mark) = &self.notify_owner_state_dirty {
                            mark();
                        }
                        None
                    }
                }
            } else {
                None
            };
            // A staged (non-friend) invite bootstrapped no Space, so this lookup
            // fails UNLESS the Space already exists from a prior accept. On the
            // space-absent (SpaceNotFound) branch, drop the lock, stage + emit
            // the invite so the user is prompted, and leave the message unacked
            // (Err) for a post-accept redelivery. Any OTHER error here means the
            // Space DOES exist (e.g. a kicked co-member's device is still
            // cached and passes sender binding, but fails
            // SenderNotInSpaceMembers/SpaceKindMismatch) — do NOT stage, or a
            // removed member's redelivery would re-prompt the user to re-admit
            // them. When the Space exists and the sender IS a member, the
            // invite is redundant — admit the message and do NOT re-prompt.
            let space =
                match crate::dm_outbox::verify_cidnotify_space(&state, &signed, resolved_owner) {
                    Ok(space) => space,
                    Err(e @ crate::dm_outbox::DmReceiveError::SpaceNotFound) => {
                        drop(state);
                        if let Some(mut staged) = staged_invite {
                            // ZEB-236 (final review): tag with the verified
                            // CidNotify's `message_cid` so a decline suppresses
                            // re-prompts on THIS SAME message's relay-drain
                            // redeliveries (the invite stays unacked while
                            // pending, so the relay re-delivers it).
                            staged.source_cid = Some(signed.message_cid);
                            crate::pending_dm_invites::stage_and_emit_staged_invite(
                                self.pending_dm_invites.as_ref(),
                                self.sink.as_ref(),
                                staged,
                            );
                        }
                        return Err(format!("verify_cidnotify_space: {e:?}"));
                    }
                    Err(e) => {
                        return Err(format!("verify_cidnotify_space: {e:?}"));
                    }
                };

            // 4. Blob ↔ packet binding: the recovered storage blob must hash to
            //    the packet's message_cid under the DM send path's flags.
            if computed_cid != signed.message_cid {
                return Err("storage blob CID does not match packet message_cid".into());
            }

            // 5. Decrypt + sender binding — the normal path's Phase C.
            let payload_msg = crate::dm_outbox::decrypt_and_bind_dm_blob(
                &space,
                &payload.storage_blob,
                resolved_owner,
            )
            .map_err(|e| format!("decrypt_and_bind_dm_blob: {e:?}"))?;

            // 6. apply_inbox — idempotent on (space_id, message_cid). `from` =
            //    the resolved sender, `received_at` = now (this device's id),
            //    mirroring the direct path's `inbox_entry`. A message that ALSO
            //    arrived direct dedupes here (Merged → no re-emit).
            let received_at = Hlc {
                wall_ms: now_epoch_ms(),
                logical: 0,
                device_id: self.device_id.clone(),
            };
            let inbox_entry = InboxEntry {
                space_id: signed.space_id,
                message_cid: signed.message_cid,
                from: resolved_owner,
                received_at: received_at.clone(),
            };
            let newly_inserted = match state.apply_inbox(inbox_entry) {
                crate::owner_state_crdt::ApplyOutcome::Inserted => true,
                crate::owner_state_crdt::ApplyOutcome::Merged { .. } => false,
                crate::owner_state_crdt::ApplyOutcome::Rejected(reason) => {
                    return Err(format!("apply_inbox rejected: {reason:?}"));
                }
            };
            (
                resolved_owner,
                payload_msg,
                newly_inserted,
                received_at,
                signed.space_id,
                signed.message_cid,
            )
        };

        // ZEB-709: arm the OWNER-STATE flush for the payload write (lock
        // dropped above; notify-on-insert only). The relay ack that follows a
        // successful ingest lets coverage-GC destroy the held blob — the
        // payload must not be the only un-persisted copy at that point.
        if newly_inserted {
            if let Some(mark) = &self.notify_owner_state_dirty {
                mark();
            }
        }

        // ZEB-640 (1): the co-deposited invite auto-accepted (or the Space
        // already existed) — the lock is dropped now, so purge any stale
        // staged (pre-befriend) entry for this space.
        if purge_stale_staged {
            crate::pending_dm_invites::purge_stale_staged_on_accept(
                self.pending_dm_invites.as_ref(),
                self.sink.as_ref(),
                &space_id,
            );
        }

        // 7. Emit the SAME dm-received event the normal path emits, gated on
        //    Inserted (a duplicate — direct arrival or an earlier ingest — never
        //    re-emits). Done AFTER the lock is released.
        if newly_inserted {
            let rm = ReceivedMessage {
                inbox_entry: InboxEntry {
                    space_id,
                    message_cid,
                    from: resolved_owner,
                    received_at,
                },
                body: payload_msg.body,
                mime_type: payload_msg.mime_type,
                sent_at: payload_msg.sent_at,
            };
            crate::node_event_sink::emit_ser(
                self.sink.as_ref(),
                crate::dm_outbox::DM_RECEIVED_EVENT,
                &crate::dm_outbox::dm_received_event_payload(&rm),
            );
        }
        Ok(())
    }
}

// =====================================================================
// ProdCommunityRelayDepositClient (sender side — D40 relay rung)
// =====================================================================

/// Resolves the RECIPIENT owner's freshest advertised butler set (the SEAL
/// TARGETS — each is one of R's enrolled device ed25519 verify keys). Factored
/// behind a seam so the decision logic of
/// [`ProdCommunityRelayDepositClient::deposit`] (no butler set → false, etc.)
/// is unit-testable without a real `ReachabilityResolver`/pkarr. Production
/// wires [`ReachabilityButlerSetResolve`] over the live resolver.
#[async_trait]
pub trait ButlerSetResolve: Send + Sync {
    async fn resolve_targets(
        &self,
        recipient_owner: &crate::owner_state_types::OwnerAddr,
        now_ms: u64,
    ) -> Vec<crate::reachability_record::ButlerSetEntry>;
}

/// Production butler-set resolution over the live [`ReachabilityResolver`]
/// (in-memory CRDT cache first, pkarr fallback on miss) +
/// `freshest_butler_set_by_source` (durable-CRDT entries window-exempt,
/// pkarr-live windowed — Task 4) — identical to the butler client's
/// resolution step.
pub struct ReachabilityButlerSetResolve(pub crate::reachability_resolver::ReachabilityResolver);

#[async_trait]
impl ButlerSetResolve for ReachabilityButlerSetResolve {
    async fn resolve_targets(
        &self,
        recipient_owner: &crate::owner_state_types::OwnerAddr,
        now_ms: u64,
    ) -> Vec<crate::reachability_record::ButlerSetEntry> {
        let tagged = self.0.resolve_async_with_source(recipient_owner).await;
        crate::butler_deposit::freshest_butler_set_by_source(&tagged, now_ms)
    }
}

/// Dials ONE community relay and deposits ONE sealed [`RelayDepositFrame`],
/// returning `Ok(())` iff the relay acked (a valid [`RelayDepositAck`] decoded).
/// Factored behind a seam so the deposit DECISION loop (first acking relay
/// wins; none ack → false) is unit-testable without iroh. Production wires
/// [`IrohRelayDepositDial`].
#[async_trait]
pub trait RelayDepositDial: Send + Sync {
    async fn dial_deposit(
        &self,
        relay: &crate::community_relay_announce::CommunityRelayEntry,
        frame: &crate::community_relay::RelayDepositFrame,
    ) -> Result<(), String>;
}

/// Production [`RelayDepositDial`] over iroh — mirrors
/// [`crate::butler_deposit::IrohButlerDepositClient::deposit_to_entry`]: dial
/// the relay's endpoint on [`HARMONY_COMMUNITY_RELAY_DEPOSIT_V1`], `open_bi`,
/// write the length-prefixed frame, `finish`, read + decode the length-prefixed
/// ack, close — all bounded by `io_deadline`. A decoded [`RelayDepositAck`] is
/// the success signal; its `content_id` is informational (the relay derives it
/// over the sealed_blob), so it is not cross-checked.
pub struct IrohRelayDepositDial {
    pub endpoint: Arc<crate::iroh_endpoint::IrohEndpoint>,
    pub io_deadline: std::time::Duration,
}

#[async_trait]
impl RelayDepositDial for IrohRelayDepositDial {
    async fn dial_deposit(
        &self,
        relay: &crate::community_relay_announce::CommunityRelayEntry,
        frame: &crate::community_relay::RelayDepositFrame,
    ) -> Result<(), String> {
        let exchange = async {
            let ep_id = iroh::EndpointId::from_bytes(&relay.iroh_endpoint_id)
                .map_err(|e| format!("relay endpoint id: {e}"))?;
            let mut addr = iroh::EndpointAddr::new(ep_id);
            if !relay.home_relay.is_empty() {
                match relay.home_relay.parse::<iroh::RelayUrl>() {
                    Ok(url) => addr = addr.with_relay_url(url),
                    Err(e) => tracing::trace!(
                        relay = %relay.home_relay,
                        "ZEB-458: skip malformed relay home_relay: {e}"
                    ),
                }
            }
            let conn = self
                .endpoint
                .inner()
                .connect(
                    addr,
                    crate::iroh_endpoint::alpn::HARMONY_COMMUNITY_RELAY_DEPOSIT_V1,
                )
                .await
                .map_err(|e| format!("connect: {e}"))?;
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi: {e}"))?;
            let frame_bytes = crate::community_relay::encode_relay_deposit_frame(frame)
                .map_err(|e| format!("encode frame: {e}"))?;
            crate::butler_deposit::write_length_prefixed(&mut send, &frame_bytes)
                .await
                .map_err(|e| format!("write frame: {e}"))?;
            send.finish().map_err(|e| format!("finish: {e}"))?;
            let ack_bytes = crate::butler_deposit::read_length_prefixed(&mut recv)
                .await
                .map_err(|e| format!("read ack: {e}"))?;
            // A decoded ack is the success signal; a reject closes the stream
            // without an ack (the read above fails). The content_id is derived
            // by the relay over the sealed_blob — informational, not checked.
            crate::community_relay::decode_relay_deposit_ack(&ack_bytes)
                .map_err(|e| format!("decode ack: {e}"))?;
            // Dialer-driven close (the acceptor waits on `conn.closed()` after
            // writing the ack).
            conn.close(0u32.into(), b"");
            Ok(())
        };
        tokio::time::timeout(self.io_deadline, exchange)
            .await
            .map_err(|_| "relay deposit IO timeout".to_string())?
    }
}

/// Production [`CommunityRelayDepositClient`] (ZEB-458 P4 Phase B, D40): the
/// sender's last-resort relay rung. Seals the SAME [`DepositPayload`] the butler
/// rung uses to the RECIPIENT's advertised butler-set device key(s), and
/// deposits to a relay in a community both parties have Joined. Returns `true`
/// iff at least one relay acked ≥1 sealed copy.
///
/// Mirrors [`crate::butler_deposit::IrohButlerDepositClient`] for the
/// resolve-butler-set + fetch-blob + seal-per-target steps; adds the
/// shared-community computation (D40 gate) and the per-community relay fan-out.
/// The two iroh-touching steps (butler-set resolution + the per-relay dial) are
/// behind seams ([`ButlerSetResolve`], [`RelayDepositDial`]) so the decision
/// logic is unit-testable; T12 covers the live iroh path end-to-end.
pub struct ProdCommunityRelayDepositClient {
    /// Resolves the recipient's butler-set seal targets (R's device keys).
    pub butler_resolver: Arc<dyn ButlerSetResolve>,
    /// Resolves the fresh relay advertisers for a given community.
    pub relay_resolver: Arc<crate::community_relay_resolver::CommunityRelayResolver>,
    /// Dials + deposits one frame to one relay (iroh in production).
    pub dial: Arc<dyn RelayDepositDial>,
    /// CAS holding the message storage blob (written by `send_dm` step 5).
    pub cas: Arc<dyn crate::content_store::ContentStore>,
    /// This (sender) owner's address bytes.
    pub sender_owner: [u8; 16],
    /// Canonical CBOR of this device's own `EnrollmentCert` (the relay
    /// strict-decodes + verifies it against the sender owner's master key).
    pub enrollment_cert_bytes: Vec<u8>,
    /// The cert-bound enrolled device signing key — signs the deposit frame.
    pub device_signing_key: Arc<ed25519_dalek::SigningKey>,
    /// Runtime owner-state CRDT — read to enumerate the sender's community
    /// spaces (the candidate gating communities) + their kinds.
    pub crdt_state: Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    /// Community-membership lookup seam (used for BOTH self+recipient
    /// joined-ness in the shared-community computation).
    pub membership: Arc<dyn CommunityMembershipLookup>,
    /// ZEB-524: when this sender is ALSO a relay host for a shared community,
    /// the fan-out resolves its OWN `CommunityRelayAnnounce` and would iroh-dial
    /// itself (which iroh rejects → nothing held). This is the local relay-hold
    /// sink: for a self relay target the recipient's sealed copy is held in our
    /// own rung by running the full acceptor pipeline
    /// ([`crate::iroh_community_relay_acceptor::handle_relay_deposit_core`])
    /// against this ctx instead of dialing — same admission gates as a dialed-in
    /// deposit, byte-identical hold. `None` disables the self-host shortcut,
    /// preserving the pre-ZEB-524 behavior (self relays are skipped, so a
    /// sole-host sender holds nothing).
    pub local_relay_ctx: Option<Arc<dyn crate::iroh_community_relay_acceptor::RelayDepositCtx>>,
}

impl ProdCommunityRelayDepositClient {
    /// Communities where BOTH the sender and the recipient are Joined members
    /// (D40 gate). `CommunityMembershipLookup::is_joined` is async, but
    /// [`crate::community_relay::find_shared_communities`] takes a SYNC closure,
    /// so the joined-ness is MATERIALIZED into a set first: read the sender's
    /// community space ids + kinds from the owner-state CRDT under one lock, then
    /// probe membership for each Community-kind space (self + recipient) and feed
    /// the pure predicate. Returns the communities in `space_ids` order.
    async fn shared_communities(
        &self,
        recipient_owner: &crate::owner_state_types::OwnerAddr,
    ) -> Vec<crate::owner_state_types::SpaceId> {
        use crate::owner_state_types::{OwnerAddr, SpaceId, SpaceKind};

        // 1. Snapshot (space_id, kind) for every space under one lock, then drop
        //    it before any await on the membership seam.
        let spaces: Vec<(SpaceId, SpaceKind)> = {
            let state = self.crdt_state.lock().await;
            state
                .spaces
                .iter()
                .map(|(id, space)| (*id, space.kind))
                .collect()
        };

        let kind_map: BTreeMap<SpaceId, SpaceKind> = spaces.iter().copied().collect();
        let space_ids: Vec<SpaceId> = spaces.iter().map(|(id, _)| *id).collect();
        let self_addr = OwnerAddr(self.sender_owner);

        // 2. Materialize joined-ness for the Community-kind spaces only — the
        //    pure predicate filters to Community kind anyway, so probing the
        //    others would be wasted RPCs.
        let mut joined: std::collections::HashSet<(SpaceId, [u8; 16])> =
            std::collections::HashSet::new();
        for (id, kind) in &spaces {
            if *kind != SpaceKind::Community {
                continue;
            }
            if self.membership.is_joined(id, &self.sender_owner).await {
                joined.insert((*id, self.sender_owner));
            }
            if self.membership.is_joined(id, &recipient_owner.0).await {
                joined.insert((*id, recipient_owner.0));
            }
        }

        crate::community_relay::find_shared_communities(
            &space_ids,
            &self_addr,
            recipient_owner,
            |c| kind_map.get(c).copied().unwrap_or(SpaceKind::Folder),
            |c, o| joined.contains(&(*c, o.0)),
        )
    }

    /// When the recipient advertises no (durable or live) butler-set, fall back
    /// to <=2 of their ENROLLED device ed25519 vks from durable community
    /// membership (`OwnerDeviceCache`). The relay only seals to the vk (it
    /// holds; the recipient pulls), so endpoint/relay fields are unused —
    /// left zero/empty. D35 preserved (seal to device keys, bounded fan-out).
    ///
    /// SECURITY: the vks are sourced ONLY from durable CRDT membership
    /// (`owner_device_cache.device_identity_pubs`), never from transient or
    /// unauthenticated input. The ed25519 verify key is bytes `[32..64]` of
    /// the canonical 64-byte `X25519_pub(32) || Ed25519_pub(32)` identity pub
    /// (the same slice `dm_signing::verify_dm_packet_signature` treats as the
    /// verifying key). `device_id` is the canonical `DeviceIdentityHash`
    /// (`SHA256(X25519 || Ed25519)[:16]`) taken from the parallel `devices`
    /// vec — NOT recomputed from the ed25519 half — so the entry stays
    /// self-consistent with durable membership. (`devices` and
    /// `device_identity_pubs` are index-parallel: re-paired jointly on
    /// deserialize, see `OwnerDeviceEntry`'s manual `Deserialize`.)
    async fn enrolled_device_fallback_targets(
        &self,
        recipient_owner: &crate::owner_state_types::OwnerAddr,
    ) -> Vec<crate::reachability_record::ButlerSetEntry> {
        let st = self.crdt_state.lock().await;
        let Some(entry) = st.owner_device_cache.devices.get(recipient_owner) else {
            return Vec::new();
        };
        entry
            .devices
            .iter()
            .zip(entry.device_identity_pubs.iter())
            .filter_map(|(dev_id, pub_opt)| pub_opt.as_ref().map(|p| (dev_id, p)))
            // Order-dependent selection: `devices` is sorted ascending by
            // hash, so this picks the two lex-smallest-device-hash enrolled
            // devices. Unlike the real butler-set this fallback has no
            // priority/pinning — it is a best-effort last resort.
            .take(crate::butler_deposit::BUTLER_SET_MAX_ENTRIES)
            .map(|(dev_id, identity_pub)| {
                let ed: [u8; 32] = identity_pub[32..64].try_into().expect("64 - 32 == 32");
                crate::reachability_record::ButlerSetEntry {
                    device_id: dev_id.0, // canonical DeviceIdentityHash from membership
                    iroh_endpoint_id: [0u8; 32],
                    device_ed25519_verify: ed,
                    home_relay: String::new(),
                    pinned: false,
                }
            })
            .collect()
    }

    /// ZEB-524 self-deposit: hold a self-authored deposit `frame` in our OWN
    /// relay rung with no dial, by running the FULL acceptor pipeline
    /// ([`crate::iroh_community_relay_acceptor::handle_relay_deposit_core`])
    /// against our own ctx. A self-deposit is thereby treated exactly like a
    /// deposit that arrived from ourselves over the wire: it can never admit
    /// anything a dialed-in deposit couldn't — the opt-in (`serves_community`),
    /// byte-ceiling, co-membership, cert, and signature gates ALL apply, and the
    /// atomic caps + durable flush produce a `RelayHoldEntry` byte-identical to
    /// a dialed-in hold that the recipient's normal relay-pull recover flow
    /// later retrieves. (Re-verifying our own cert/sig is negligible and keeps
    /// the admission policy single-sourced in the acceptor — critically, it
    /// closes the opt-out-with-fresh-ad hole where a self-hold would otherwise
    /// succeed but the pull acceptor would refuse to serve it.)
    async fn local_relay_hold(
        &self,
        frame: &crate::community_relay::RelayDepositFrame,
        ctx: &dyn crate::iroh_community_relay_acceptor::RelayDepositCtx,
    ) -> Result<(), String> {
        crate::iroh_community_relay_acceptor::handle_relay_deposit_core(frame, ctx)
            .await
            .map(|_ack| ())
            .map_err(|e| format!("local relay hold rejected: {e}"))
    }
}

#[async_trait]
impl crate::community_relay::CommunityRelayDepositClient for ProdCommunityRelayDepositClient {
    async fn deposit(&self, req: &crate::butler_deposit::ButlerDepositRequest) -> bool {
        // 1. Communities both parties have Joined (D40 gate). None → no relay
        //    rung is permitted. Checked FIRST (ahead of the seal-target resolve
        //    and the enrolled-device fallback) so a stale cross-community
        //    `OwnerDeviceCache` entry can't drive wasted target derivation — the
        //    fallback's `crdt_state` lock + vk extraction — for a recipient we no
        //    longer share a community with; this membership gate would reject the
        //    deposit regardless (Greptile P2). The gate is membership-only and
        //    never permits a deposit on its own.
        let communities = self.shared_communities(&req.recipient_owner).await;
        if communities.is_empty() {
            return false;
        }

        // 2. Resolve R's fresh butler set — the SEAL TARGETS (R's device keys,
        //    NOT any relay's). Empty → fall back to R's ENROLLED device vks from
        //    durable community membership (the relay only seals to a vk; it
        //    holds the ciphertext, R pulls — no endpoint dial to R). Still empty
        //    → no device to seal to (accepted gap, D35).
        let mut targets = self
            .butler_resolver
            .resolve_targets(&req.recipient_owner, req.now_ms)
            .await;
        if targets.is_empty() {
            targets = self
                .enrolled_device_fallback_targets(&req.recipient_owner)
                .await;
        }
        if targets.is_empty() {
            return false;
        }

        // 3. Fetch the storage blob (only after a target + a shared community
        //    are confirmed — no CAS work for a no-op rung). Miss/err → false.
        //    ZEB-505: an invite-only deposit (no message_cid) carries the invite
        //    alone — there is no blob to fetch.
        let storage_blob = match req.message_cid {
            Some(message_cid) => match self.cas.get(&message_cid).await {
                Ok(Some(blob)) => blob,
                Ok(None) | Err(_) => return false,
            },
            None => Vec::new(),
        };
        let payload = DepositPayload {
            cidnotify_packet: req.cidnotify_packet.clone(),
            storage_blob,
            invite_packet: req.invite_packet.clone(),
            // ZEB-691: revocation pushes route via the butler deposit rung
            // only — this community-relay path never carries them.
            revocation_push: None,
            grant_push: None,
        };

        // 4. Fan out: the FIRST relay that acks ≥1 sealed copy wins. For each
        //    community (in `find_shared_communities` order), for each fresh
        //    relay advertiser, deposit every butler-set device-target copy to
        //    that relay; if it acks any, return true. Depositing all (≤2) device
        //    copies to the one reachable relay means whichever R device returns
        //    can pull its copy.
        //
        // ZEB-524: this node may itself be a fresh relay advertiser for the
        // community — its own `CommunityRelayAnnounce` sits in the resolver with
        // no self-exclusion. A SEPARATE relay host is preferred because it
        // survives this node going offline, so self relays are tried LAST: only
        // if no distinct host acks do we fall back to a LOCAL self-hold (dialing
        // ourselves is impossible — iroh rejects a self-connection). Ordering
        // self last also preserves the pre-fix outcome for the multi-host case
        // (where the self-dial simply failed and the loop moved to a distinct
        // host); the new behavior is purely the added sole-host fallback.
        let self_relay_vk = self.device_signing_key.verifying_key().to_bytes();
        for community in &communities {
            let relays = self
                .relay_resolver
                .relays_for_community(community, req.now_ms);
            let (other_relays, self_relays): (Vec<_>, Vec<_>) = relays
                .iter()
                .partition(|r| r.relay_device_ed25519_verify != self_relay_vk);
            for relay in other_relays.into_iter().chain(self_relays.into_iter()) {
                let is_self = relay.relay_device_ed25519_verify == self_relay_vk;
                let mut relay_acked = false;
                for target in &targets {
                    let frame = match crate::community_relay::build_relay_deposit_frame(
                        req.recipient_owner.0,
                        &target.device_ed25519_verify,
                        self.sender_owner,
                        *community,
                        self.enrollment_cert_bytes.clone(),
                        &self.device_signing_key,
                        &payload,
                    ) {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::debug!(error = %e, "ZEB-458: relay frame build failed; skip target");
                            continue;
                        }
                    };
                    let outcome = if is_self {
                        // ZEB-524 local self-hold: persist into our own rung
                        // instead of dialing ourselves. With no local ctx wired,
                        // skip (degrades to the pre-fix behavior — nothing held).
                        match &self.local_relay_ctx {
                            Some(ctx) => self.local_relay_hold(&frame, ctx.as_ref()).await,
                            None => Err("ZEB-524: self relay target but no local relay ctx wired"
                                .to_string()),
                        }
                    } else {
                        self.dial.dial_deposit(relay, &frame).await
                    };
                    match outcome {
                        Ok(()) => relay_acked = true,
                        Err(e) => {
                            tracing::debug!(
                                relay_device = %hex::encode(relay.relay_device_id),
                                is_self,
                                error = %e,
                                "ZEB-458: relay deposit copy failed"
                            );
                        }
                    }
                }
                if relay_acked {
                    return true;
                }
            }
        }
        false
    }
}

/// Wall-clock now in epoch-MS for `received_at` (the inbox entry's receive
/// stamp). Separate from [`now_epoch_secs`] (cert expiry uses seconds).
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::{mint_test_owner, TestOwner};
    use std::collections::{BTreeSet, HashSet};

    // ---------------------------------------------------------------
    // ZEB-524 self-hold: a mock RelayDepositCtx that records persisted
    // entries and lets a test toggle the opt-in gate (`serves_community`).
    // `now_secs` is pinned to the minted-cert validity window so the FULL
    // acceptor pipeline (cert/sig verify included) admits a self-deposit.
    // ---------------------------------------------------------------
    struct TestSelfRelayCtx {
        /// Whether this node currently serves (is opted in + joined for) the
        /// community — the gate a self-hold must honor.
        serves: bool,
        /// Insert-once record of what was held.
        store: Arc<std::sync::Mutex<BTreeMap<String, RelayHoldEntry>>>,
    }

    #[async_trait]
    impl RelayDepositCtx for TestSelfRelayCtx {
        fn relay_device_id(&self) -> String {
            "self-relay-dev".into()
        }
        async fn serves_community(&self, _c: &SpaceId) -> bool {
            self.serves
        }
        async fn both_co_members(&self, _c: &SpaceId, _s: &[u8; 16], _r: &[u8; 16]) -> bool {
            true
        }
        fn now_secs(&self) -> u64 {
            // Same fixed value the acceptor tests use — within mint_test_owner's
            // cert validity window (real wall-clock would read the cert expired).
            1_700_000_100
        }
        async fn mint_hlc(&self) -> Hlc {
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "self-relay-dev".into(),
            }
        }
        async fn persist_hold(
            &self,
            key: String,
            entry: RelayHoldEntry,
        ) -> Result<RelayPersistVerdict, String> {
            let mut s = self.store.lock().unwrap();
            if s.contains_key(&key) {
                return Ok(RelayPersistVerdict::Duplicate);
            }
            s.insert(key, entry);
            Ok(RelayPersistVerdict::Inserted)
        }
    }

    /// Build a deposit client whose SENDER is a real minted owner (valid
    /// Master-issued cert + enrolled device key) wired with a mock self-relay
    /// ctx (`serves` toggles the opt-in gate). A valid identity is required
    /// because the self-hold runs the full acceptor pipeline. Returns the
    /// client + the record of what the ctx held.
    fn deposit_client_self_minted(
        sender: &TestOwner,
        recipient: [u8; 16],
        relay_resolver: Arc<CommunityRelayResolver>,
        dial: Arc<dyn RelayDepositDial>,
        cas: Arc<dyn crate::content_store::ContentStore>,
        community_byte: u8,
        serves: bool,
    ) -> (
        ProdCommunityRelayDepositClient,
        Arc<std::sync::Mutex<BTreeMap<String, RelayHoldEntry>>>,
    ) {
        let store = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let ctx: Arc<dyn RelayDepositCtx> = Arc::new(TestSelfRelayCtx {
            serves,
            store: Arc::clone(&store),
        });
        let community = SpaceId([community_byte; 16]);
        let client = ProdCommunityRelayDepositClient {
            butler_resolver: Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA)],
            }),
            relay_resolver,
            dial,
            cas,
            sender_owner: sender.owner.0,
            enrollment_cert_bytes: harmony_owner::cbor::to_canonical(&sender.cert)
                .expect("encode sender cert"),
            device_signing_key: Arc::new(sender.device_key.clone()),
            crdt_state: crdt_with_spaces(&[(community_byte, SpaceKind::Community)]),
            membership: fake(&[(community, sender.owner.0), (community, recipient)]),
            local_relay_ctx: Some(ctx),
        };
        (client, store)
    }

    fn space(b: u8) -> SpaceId {
        SpaceId([b; 16])
    }

    // ---------------------------------------------------------------
    // NoopFlush — unit-test flush seam: never errors, no side-effects.
    // ---------------------------------------------------------------
    struct NoopFlush;

    #[async_trait]
    impl RelayHoldFlush for NoopFlush {
        fn notify_dirty(&self) {}
        async fn flush_now(&self) -> Result<(), String> {
            Ok(())
        }
    }

    // ---------------------------------------------------------------
    // FakeMembership — unit-test seam for the membership-dependent logic
    // (serves_community / both_co_members / is_joined_member) without a
    // full CommunitySyncRegistry.
    // ---------------------------------------------------------------
    struct FakeMembership {
        joined: HashSet<(SpaceId, [u8; 16])>,
    }

    #[async_trait]
    impl CommunityMembershipLookup for FakeMembership {
        async fn is_joined(&self, c: &SpaceId, o: &[u8; 16]) -> bool {
            self.joined.contains(&(*c, *o))
        }
    }

    fn fake(pairs: &[(SpaceId, [u8; 16])]) -> Arc<dyn CommunityMembershipLookup> {
        Arc::new(FakeMembership {
            joined: pairs.iter().copied().collect(),
        })
    }

    /// Opt-in doc with `community` opted in iff `opted` (HLC stamp arbitrary).
    fn optin_doc(community: SpaceId, opted: bool) -> Arc<tokio::sync::Mutex<RelayOptInDoc>> {
        let mut d = RelayOptInDoc::default();
        if opted {
            d.set(
                community,
                true,
                Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "relay".into(),
                },
            );
        }
        Arc::new(tokio::sync::Mutex::new(d))
    }

    /// Build a `ProdRelayDepositCtx` wired with a `NoopFlush` and the
    /// provided membership/optin. `self_owner` defaults to `[0x01; 16]`.
    fn deposit_ctx(
        self_owner: [u8; 16],
        relay_hold_doc: Arc<tokio::sync::Mutex<RelayHoldDoc>>,
        optin: Arc<tokio::sync::Mutex<RelayOptInDoc>>,
        membership: Arc<dyn CommunityMembershipLookup>,
    ) -> ProdRelayDepositCtx {
        ProdRelayDepositCtx {
            self_owner,
            relay_device_id: "relay-dev".into(),
            relay_hold_doc,
            relay_hold_tracker: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
            flush: Arc::new(NoopFlush),
            optin,
            membership,
        }
    }

    /// Build a `ProdRelayPullCtx` wired with a `NoopFlush` and the provided
    /// membership/optin. `self_owner` defaults to `[0x01; 16]`.
    fn pull_ctx(
        self_owner: [u8; 16],
        relay_hold_doc: Arc<tokio::sync::Mutex<RelayHoldDoc>>,
        optin: Arc<tokio::sync::Mutex<RelayOptInDoc>>,
        membership: Arc<dyn CommunityMembershipLookup>,
    ) -> ProdRelayPullCtx {
        ProdRelayPullCtx {
            self_owner,
            relay_hold_doc,
            flush: Arc::new(NoopFlush),
            optin,
            membership,
        }
    }

    fn hold_entry(
        recipient: [u8; 16],
        sender: [u8; 16],
        community: SpaceId,
        blob: Vec<u8>,
    ) -> RelayHoldEntry {
        RelayHoldEntry {
            recipient_owner: recipient,
            sender_owner: sender,
            community_id: community,
            sealed_blob: blob,
            held_at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "relay".into(),
            },
            held_by: "relay".into(),
            pulled_by: BTreeSet::new(),
        }
    }

    // ---------------------------------------------------------------
    // serves_community: true iff opted-in AND self is_joined
    // Tests call the REAL ProdRelayDepositCtx / ProdRelayPullCtx methods.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn deposit_ctx_serves_community_true_when_opted_in_and_self_joined() {
        let c = space(0xCC);
        let self_owner = [0x11; 16];
        let doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        let ctx = deposit_ctx(
            self_owner,
            doc,
            optin_doc(c, true),
            fake(&[(c, self_owner)]),
        );
        assert!(ctx.serves_community(&c).await);
    }

    #[tokio::test]
    async fn deposit_ctx_serves_community_false_when_not_opted_in() {
        let c = space(0xCC);
        let self_owner = [0x11; 16];
        let doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        let ctx = deposit_ctx(
            self_owner,
            doc,
            optin_doc(c, false), // NOT opted in
            fake(&[(c, self_owner)]),
        );
        assert!(!ctx.serves_community(&c).await);
    }

    #[tokio::test]
    async fn deposit_ctx_serves_community_false_when_self_not_joined() {
        let c = space(0xCC);
        let self_owner = [0x11; 16];
        let doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        let ctx = deposit_ctx(
            self_owner,
            doc,
            optin_doc(c, true), // opted in
            fake(&[]),          // but NOT a member
        );
        assert!(!ctx.serves_community(&c).await);
    }

    #[tokio::test]
    async fn pull_ctx_serves_community_true_when_opted_in_and_self_joined() {
        let c = space(0xCC);
        let self_owner = [0x11; 16];
        let doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        let ctx = pull_ctx(
            self_owner,
            doc,
            optin_doc(c, true),
            fake(&[(c, self_owner)]),
        );
        assert!(ctx.serves_community(&c).await);
    }

    #[tokio::test]
    async fn pull_ctx_serves_community_false_when_not_opted_in() {
        let c = space(0xCC);
        let self_owner = [0x11; 16];
        let doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        let ctx = pull_ctx(
            self_owner,
            doc,
            optin_doc(c, false),
            fake(&[(c, self_owner)]),
        );
        assert!(!ctx.serves_community(&c).await);
    }

    #[tokio::test]
    async fn pull_ctx_serves_community_false_when_self_not_joined() {
        let c = space(0xCC);
        let self_owner = [0x11; 16];
        let doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        let ctx = pull_ctx(self_owner, doc, optin_doc(c, true), fake(&[]));
        assert!(!ctx.serves_community(&c).await);
    }

    // ---------------------------------------------------------------
    // both_co_members / is_joined_member reflect the fake
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn both_co_members_requires_both_joined() {
        let c = space(0xCC);
        let sender = [0x22; 16];
        let recipient = [0x33; 16];
        let doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));

        // both joined → true
        let ctx = deposit_ctx(
            [0x01; 16],
            doc.clone(),
            optin_doc(c, true),
            fake(&[(c, sender), (c, recipient)]),
        );
        assert!(ctx.both_co_members(&c, &sender, &recipient).await);

        // only sender joined → false
        let ctx = deposit_ctx(
            [0x01; 16],
            doc.clone(),
            optin_doc(c, true),
            fake(&[(c, sender)]),
        );
        assert!(!ctx.both_co_members(&c, &sender, &recipient).await);

        // only recipient joined → false
        let ctx = deposit_ctx(
            [0x01; 16],
            doc.clone(),
            optin_doc(c, true),
            fake(&[(c, recipient)]),
        );
        assert!(!ctx.both_co_members(&c, &sender, &recipient).await);

        // neither joined → false
        let ctx = deposit_ctx([0x01; 16], doc, optin_doc(c, true), fake(&[]));
        assert!(!ctx.both_co_members(&c, &sender, &recipient).await);
    }

    #[tokio::test]
    async fn is_joined_member_reflects_fake() {
        let c = space(0xCC);
        let owner = [0x44; 16];
        let doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        let ctx = pull_ctx([0x01; 16], doc, optin_doc(c, true), fake(&[(c, owner)]));
        assert!(ctx.is_joined_member(&c, &owner).await);
        assert!(!ctx.is_joined_member(&c, &[0x45; 16]).await);
        assert!(!ctx.is_joined_member(&space(0xDD), &owner).await);
    }

    // ---------------------------------------------------------------
    // held_for: exactly the recipient's entries, excludes others'
    // Calls the REAL ProdRelayPullCtx::held_for.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn held_for_returns_only_the_recipients_entries() {
        let c = space(0xCC);
        let recipient = [0xAA; 16];
        let other = [0xBB; 16];
        let sender = [0x22; 16];

        let mut doc = RelayHoldDoc::default();

        // Two entries for our recipient (distinct content → distinct keys).
        let blob_a = vec![1, 2, 3];
        let blob_b = vec![4, 5, 6];
        let key_a = RelayHoldDoc::key(&recipient, &[0x01; 32]);
        let key_b = RelayHoldDoc::key(&recipient, &[0x02; 32]);
        doc.entries.insert(
            key_a.clone(),
            hold_entry(recipient, sender, c, blob_a.clone()),
        );
        doc.entries.insert(
            key_b.clone(),
            hold_entry(recipient, sender, c, blob_b.clone()),
        );

        // One entry for a DIFFERENT recipient — must be excluded.
        let key_other = RelayHoldDoc::key(&other, &[0x03; 32]);
        doc.entries
            .insert(key_other, hold_entry(other, sender, c, vec![9, 9, 9]));

        let doc_arc = Arc::new(tokio::sync::Mutex::new(doc));
        let ctx = pull_ctx(
            [0x01; 16],
            doc_arc,
            optin_doc(c, true),
            fake(&[(c, [0x01; 16])]),
        );

        // Requester device not in any entry's pulled_by → no per-device
        // filtering, so this exercises the plain recipient-scoping.
        let mut got = ctx.held_for(&recipient, "device-never-pulled").await;
        got.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(got.len(), 2, "exactly the recipient's two entries");
        let keys: BTreeSet<String> = got.iter().map(|(k, _)| k.clone()).collect();
        assert!(keys.contains(&key_a));
        assert!(keys.contains(&key_b));
        // Blobs + sender carried through.
        let blobs: BTreeSet<Vec<u8>> = got.iter().map(|(_, b)| b.sealed_blob.clone()).collect();
        assert!(blobs.contains(&blob_a));
        assert!(blobs.contains(&blob_b));
        assert!(got.iter().all(|(_, b)| b.sender_owner == sender));
    }

    #[tokio::test]
    async fn held_for_empty_when_recipient_has_no_entries() {
        let c = space(0xCC);
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            RelayHoldDoc::key(&[0xBB; 16], &[0x01; 32]),
            hold_entry([0xBB; 16], [0x22; 16], c, vec![1]),
        );
        let doc_arc = Arc::new(tokio::sync::Mutex::new(doc));
        let ctx = pull_ctx(
            [0x01; 16],
            doc_arc,
            optin_doc(c, true),
            fake(&[(c, [0x01; 16])]),
        );
        let got = ctx.held_for(&[0xAA; 16], "device-never-pulled").await;
        assert!(got.is_empty());
    }

    /// `held_for` excludes entries this requesting device has already
    /// pulled+acked (its id is in `pulled_by`), so a device drains its backlog
    /// without re-serving acked-but-not-yet-GC'd entries.
    #[tokio::test]
    async fn held_for_excludes_entries_already_pulled_by_this_device() {
        let c = space(0xCC);
        let recipient = [0xAA; 16];
        let sender = [0x22; 16];
        let device = "abc123-requester-device";

        let mut doc = RelayHoldDoc::default();
        let key_undelivered = RelayHoldDoc::key(&recipient, &[0x01; 32]);
        let key_already_acked = RelayHoldDoc::key(&recipient, &[0x02; 32]);
        doc.entries.insert(
            key_undelivered.clone(),
            hold_entry(recipient, sender, c, vec![1, 2, 3]),
        );
        // This entry was already acked by `device` — it lingers (GC is a later
        // sweep) but must NOT be re-served to `device`.
        let mut acked = hold_entry(recipient, sender, c, vec![4, 5, 6]);
        acked.pulled_by.insert(device.to_string());
        doc.entries.insert(key_already_acked.clone(), acked);

        let doc_arc = Arc::new(tokio::sync::Mutex::new(doc));
        let ctx = pull_ctx(
            [0x01; 16],
            doc_arc,
            optin_doc(c, true),
            fake(&[(c, [0x01; 16])]),
        );

        // This device sees only the undelivered entry.
        let got = ctx.held_for(&recipient, device).await;
        assert_eq!(got.len(), 1, "only the un-acked entry is served");
        assert_eq!(got[0].0, key_undelivered);

        // A DIFFERENT device that hasn't acked anything sees both entries.
        let other = ctx.held_for(&recipient, "some-other-device").await;
        assert_eq!(other.len(), 2, "another device still sees both");
    }

    // ---------------------------------------------------------------
    // persist_hold caps — the main new unit-test coverage.
    // Calls the REAL ProdRelayDepositCtx::persist_hold method.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn persist_hold_first_insert_returns_inserted() {
        let c = space(0xCC);
        let sender = [0x22; 16];
        let recipient = [0x33; 16];
        let doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        let ctx = deposit_ctx(
            [0x01; 16],
            doc.clone(),
            optin_doc(c, true),
            fake(&[(c, [0x01; 16])]),
        );

        let key = RelayHoldDoc::key(&recipient, &[0xAA; 32]);
        let entry = hold_entry(recipient, sender, c, vec![1, 2, 3]);
        let verdict = ctx.persist_hold(key.clone(), entry).await.unwrap();
        assert_eq!(verdict, RelayPersistVerdict::Inserted);

        // Entry is actually in the doc.
        assert!(doc.lock().await.entries.contains_key(&key));
    }

    #[tokio::test]
    async fn persist_hold_duplicate_key_returns_duplicate() {
        let c = space(0xCC);
        let sender = [0x22; 16];
        let recipient = [0x33; 16];
        let doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        let ctx = deposit_ctx(
            [0x01; 16],
            doc.clone(),
            optin_doc(c, true),
            fake(&[(c, [0x01; 16])]),
        );

        let key = RelayHoldDoc::key(&recipient, &[0xAA; 32]);
        let entry = hold_entry(recipient, sender, c, vec![1, 2, 3]);

        // First insert → Inserted.
        let v1 = ctx.persist_hold(key.clone(), entry.clone()).await.unwrap();
        assert_eq!(v1, RelayPersistVerdict::Inserted);

        // Second call with same key → Duplicate (caps bypassed).
        let v2 = ctx.persist_hold(key.clone(), entry).await.unwrap();
        assert_eq!(v2, RelayPersistVerdict::Duplicate);

        // Still only one entry in the doc.
        assert_eq!(doc.lock().await.entries.len(), 1);
    }

    #[tokio::test]
    async fn persist_hold_per_sender_cap_exceeded() {
        // Fill a single (community, sender) to RELAY_HOLD_PER_SENDER_CAP
        // distinct keys, then attempt one more distinct key for that sender
        // → CapExceeded, and the doc did NOT grow past the cap.
        let c = space(0xCC);
        let sender = [0x22; 16];
        let recipient = [0x33; 16];

        let mut initial_doc = RelayHoldDoc::default();
        // Pre-populate with exactly RELAY_HOLD_PER_SENDER_CAP entries for
        // (c, sender). We use distinct content_ids [i; 32] for i in 0..cap.
        for i in 0u8..RELAY_HOLD_PER_SENDER_CAP as u8 {
            let content_id = [i; 32];
            let key = RelayHoldDoc::key(&recipient, &content_id);
            initial_doc
                .entries
                .insert(key, hold_entry(recipient, sender, c, vec![i]));
        }
        assert_eq!(initial_doc.entries.len(), RELAY_HOLD_PER_SENDER_CAP);

        let doc = Arc::new(tokio::sync::Mutex::new(initial_doc));
        let ctx = deposit_ctx(
            [0x01; 16],
            doc.clone(),
            optin_doc(c, true),
            fake(&[(c, [0x01; 16])]),
        );

        // One more distinct key for the same (community, sender) → CapExceeded.
        let overflow_key = RelayHoldDoc::key(&recipient, &[0xFF; 32]);
        let overflow_entry = hold_entry(recipient, sender, c, vec![0xFF]);
        let verdict = ctx
            .persist_hold(overflow_key.clone(), overflow_entry)
            .await
            .unwrap();
        assert_eq!(verdict, RelayPersistVerdict::CapExceeded);

        // Doc must NOT have grown.
        assert_eq!(
            doc.lock().await.entries.len(),
            RELAY_HOLD_PER_SENDER_CAP,
            "doc must not grow past the per-sender cap"
        );
        assert!(
            !doc.lock().await.entries.contains_key(&overflow_key),
            "overflow key must not be in the doc"
        );
    }

    #[tokio::test]
    async fn persist_hold_global_cap_exceeded() {
        // Fill the doc to RELAY_HOLD_GLOBAL_CAP entries across different
        // senders (so per-sender cap is not the limiting factor), then assert
        // the global cap triggers CapExceeded.
        //
        // RELAY_HOLD_GLOBAL_CAP = 1024. We build the doc directly and call
        // persist_hold once more to exercise the global-cap branch.
        let c = space(0xCC);
        let recipient = [0x33; 16];

        let mut initial_doc = RelayHoldDoc::default();
        // Use a unique sender per entry so per-sender cap (64) never triggers.
        // 1024 entries across 1024 distinct senders = 1 per sender.
        for i in 0u16..RELAY_HOLD_GLOBAL_CAP as u16 {
            let sender = {
                let mut s = [0u8; 16];
                let bytes = i.to_le_bytes();
                s[0] = bytes[0];
                s[1] = bytes[1];
                s
            };
            // Distinct content_id per entry — encode `i` into the first two bytes.
            let content_id = {
                let mut cid = [0u8; 32];
                let bytes = i.to_le_bytes();
                cid[0] = bytes[0];
                cid[1] = bytes[1];
                cid
            };
            let key = RelayHoldDoc::key(&recipient, &content_id);
            initial_doc
                .entries
                .insert(key, hold_entry(recipient, sender, c, vec![0x01]));
        }
        assert_eq!(initial_doc.entries.len(), RELAY_HOLD_GLOBAL_CAP);

        let doc = Arc::new(tokio::sync::Mutex::new(initial_doc));
        let ctx = deposit_ctx(
            [0x01; 16],
            doc.clone(),
            optin_doc(c, true),
            fake(&[(c, [0x01; 16])]),
        );

        // A fresh sender (count_for_sender = 0 < 64) but global is full.
        let new_sender = [0xFF; 16];
        let overflow_key = RelayHoldDoc::key(&[0xAA; 16], &[0xEE; 32]);
        let overflow_entry = hold_entry([0xAA; 16], new_sender, c, vec![0xEE]);
        let verdict = ctx
            .persist_hold(overflow_key.clone(), overflow_entry)
            .await
            .unwrap();
        assert_eq!(verdict, RelayPersistVerdict::CapExceeded);

        // Doc must NOT have grown past RELAY_HOLD_GLOBAL_CAP.
        assert_eq!(
            doc.lock().await.entries.len(),
            RELAY_HOLD_GLOBAL_CAP,
            "doc must not grow past the global cap"
        );
    }

    // ---------------------------------------------------------------
    // mark_pulled — calls the REAL ProdRelayPullCtx::mark_pulled.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn mark_pulled_sets_pulled_by_and_does_not_remove_inline() {
        // mark_pulled must union pulled_by but MUST NOT call gc() inline.
        // `RelayHoldDoc::merge_from` is a grow-only union: an early local delete
        // would be resurrected by a sibling relay that has not yet seen the pull
        // (spec D38). The periodic sweep (separate from mark_pulled) is what
        // reclaims storage.
        let c = space(0xCC);
        let recipient = [0xAA; 16];
        let sender = [0x22; 16];
        let key = RelayHoldDoc::key(&recipient, &[0x01; 32]);

        let mut doc = RelayHoldDoc::default();
        doc.entries
            .insert(key.clone(), hold_entry(recipient, sender, c, vec![1, 2, 3]));
        let doc_arc = Arc::new(tokio::sync::Mutex::new(doc));
        let ctx = pull_ctx(
            [0x01; 16],
            doc_arc.clone(),
            optin_doc(c, true),
            fake(&[(c, [0x01; 16])]),
        );

        ctx.mark_pulled(std::slice::from_ref(&key), "Rdev".into())
            .await
            .unwrap();

        // The entry must STILL BE PRESENT — mark_pulled does NOT remove it.
        let locked = doc_arc.lock().await;
        assert!(
            locked.entries.contains_key(&key),
            "mark_pulled must not remove the entry inline (grow-only union resurrects early deletes)"
        );
        // pulled_by must now contain "Rdev".
        let pb = &locked.entries[&key].pulled_by;
        assert!(
            pb.contains("Rdev"),
            "pulled_by must contain the requester device after mark_pulled"
        );
    }

    #[tokio::test]
    async fn mark_pulled_missing_key_is_noop() {
        let c = space(0xCC);
        let doc_arc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        let ctx = pull_ctx(
            [0x01; 16],
            doc_arc.clone(),
            optin_doc(c, true),
            fake(&[(c, [0x01; 16])]),
        );

        // A key that does not exist — must not error and doc stays empty.
        ctx.mark_pulled(&["nonexistent-key".to_string()], "devX".into())
            .await
            .unwrap();
        assert!(doc_arc.lock().await.entries.is_empty());
    }

    #[tokio::test]
    async fn separate_gc_sweep_removes_covered_entry() {
        // After mark_pulled has set pulled_by (covered state), a direct call to
        // `doc.gc(now_ms_large)` — simulating the periodic sweep — removes the
        // entry. This confirms that the SWEEP (not mark_pulled inline) is what
        // reclaims storage. The now_ms used here is large enough not to TTL-expire
        // the entry on its own (we use a held_at.wall_ms that is well within TTL
        // at any test-time clock); the removal is by coverage, not TTL.
        let c = space(0xCC);
        let recipient = [0xAA; 16];
        let sender = [0x22; 16];
        let key = RelayHoldDoc::key(&recipient, &[0x01; 32]);

        let mut doc = RelayHoldDoc::default();
        doc.entries
            .insert(key.clone(), hold_entry(recipient, sender, c, vec![1, 2, 3]));
        let doc_arc = Arc::new(tokio::sync::Mutex::new(doc));
        let ctx = pull_ctx(
            [0x01; 16],
            doc_arc.clone(),
            optin_doc(c, true),
            fake(&[(c, [0x01; 16])]),
        );

        // Step 1: mark_pulled sets pulled_by but does NOT remove the entry.
        ctx.mark_pulled(std::slice::from_ref(&key), "Rdev".into())
            .await
            .unwrap();
        assert!(
            doc_arc.lock().await.entries.contains_key(&key),
            "entry must still be present after mark_pulled (no inline GC)"
        );

        // Step 2: the periodic sweep (gc) now sees the entry as covered-at-start
        // (pulled_by is non-empty) and removes it. Use a now_ms that is WITHIN
        // TTL for the held_at.wall_ms=1_000 entry (held_at + TTL ≥ 1_000 +
        // RELAY_HOLD_TTL_MS >> 2_000_000) — the removal is by coverage, not TTL.
        let now_ms: u64 = 2_000_000;
        let removed = doc_arc.lock().await.gc(now_ms);
        assert!(removed, "gc sweep must remove the covered entry");
        assert!(
            doc_arc.lock().await.entries.is_empty(),
            "doc must be empty after the periodic gc sweep"
        );
    }

    // A compile-time anchor: the prod ctx structs implement the Phase A
    // traits. (Behavioral persist/GC coverage is the T12 integration test.)
    #[test]
    fn prod_ctxs_implement_phase_a_traits() {
        fn is_deposit_ctx<T: RelayDepositCtx>() {}
        fn is_pull_ctx<T: RelayPullCtx>() {}
        is_deposit_ctx::<ProdRelayDepositCtx>();
        is_pull_ctx::<ProdRelayPullCtx>();
    }

    // =================================================================
    // ProdCommunityRelayDepositClient (sender side) — decision logic via
    // the ButlerSetResolve + RelayDepositDial seams (iroh-free).
    // =================================================================

    use crate::butler_deposit::ButlerDepositRequest;
    use crate::community_relay::{CommunityRelayDepositClient, RelayDepositFrame};
    use crate::community_relay_announce::{CommunityRelayAnnouncePayload, CommunityRelayEntry};
    use crate::community_relay_resolver::CommunityRelayResolver;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{ContentId, OutboxEntryId, OwnerAddr, Space, SpaceKind};
    use crate::reachability_record::ButlerSetEntry;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Butler-set resolution seam returning a fixed target list (the SEAL
    /// TARGETS — R's device keys).
    struct FakeButlerSetResolve {
        targets: Vec<ButlerSetEntry>,
    }

    #[async_trait]
    impl ButlerSetResolve for FakeButlerSetResolve {
        async fn resolve_targets(&self, _r: &OwnerAddr, _now_ms: u64) -> Vec<ButlerSetEntry> {
            self.targets.clone()
        }
    }

    /// One butler-set device target keyed by its device verify key.
    fn target(vk_seed: u8) -> ButlerSetEntry {
        ButlerSetEntry {
            device_id: [vk_seed; 16],
            iroh_endpoint_id: [vk_seed; 32],
            device_ed25519_verify: [vk_seed; 32],
            home_relay: String::new(),
            pinned: false,
        }
    }

    /// Relay-dial seam mock: a relay (keyed by `relay_device_id`) acks iff it is
    /// in `acking`. Records the (relay_device_id) of every dialed copy.
    struct MockDial {
        acking: HashSet<[u8; 16]>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl RelayDepositDial for MockDial {
        async fn dial_deposit(
            &self,
            relay: &CommunityRelayEntry,
            _frame: &RelayDepositFrame,
        ) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.acking.contains(&relay.relay_device_id) {
                Ok(())
            } else {
                Err("unreachable".into())
            }
        }
    }

    /// A minimal `Space` of the given kind (all non-essential fields empty).
    fn space_of_kind(id: u8, kind: SpaceKind) -> Space {
        Space {
            id: SpaceId([id; 16]),
            kind,
            parent: None,
            community_id: None,
            name: "S".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        }
    }

    /// Owner-state with the given (space_id_byte, kind) spaces inserted.
    fn crdt_with_spaces(spaces: &[(u8, SpaceKind)]) -> Arc<tokio::sync::Mutex<OwnerState>> {
        let mut st = OwnerState::default();
        for (id, kind) in spaces {
            st.spaces
                .insert(SpaceId([*id; 16]), space_of_kind(*id, *kind));
        }
        Arc::new(tokio::sync::Mutex::new(st))
    }

    /// A relay resolver pre-populated with one relay advertiser per
    /// (community_byte, relay_device_id_seed) at `now`.
    fn relay_resolver_with(entries: &[(u8, u8)], now: u64) -> Arc<CommunityRelayResolver> {
        let r = CommunityRelayResolver::new();
        for (community_byte, relay_seed) in entries {
            let payload = CommunityRelayAnnouncePayload {
                relay: CommunityRelayEntry {
                    relay_device_id: [*relay_seed; 16],
                    iroh_endpoint_id: [*relay_seed; 32],
                    relay_device_ed25519_verify: [*relay_seed; 32],
                    home_relay: String::new(),
                },
                ad_at: now,
                identity_signature: [0; 64],
            };
            r.update(
                SpaceId([*community_byte; 16]),
                OwnerAddr([*relay_seed; 16]),
                payload,
                Hlc {
                    wall_ms: now,
                    logical: 0,
                    device_id: "r".into(),
                },
            );
        }
        Arc::new(r)
    }

    const TEST_NOW: u64 = 1_700_000_000_000;

    /// CAS pre-seeded so the message_cid resolves to a stored blob.
    async fn cas_with_blob(
        cid: &ContentId,
        blob: Vec<u8>,
    ) -> Arc<dyn crate::content_store::ContentStore> {
        use crate::content_store::ContentStore as _;
        let cas = InMemoryStub::default();
        cas.put(*cid, blob).await.unwrap();
        Arc::new(cas)
    }

    fn req(recipient: [u8; 16], cid: ContentId) -> ButlerDepositRequest {
        ButlerDepositRequest {
            entry_id: OutboxEntryId([0x77; 16]),
            recipient_owner: OwnerAddr(recipient),
            space_id: SpaceId([0xDD; 16]),
            message_cid: Some(cid),
            cidnotify_packet: Some(vec![0x01, 0x02, 0x03]),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            now_ms: TEST_NOW,
        }
    }

    /// Build a client wired with the given seams. `device_signing_key` is a
    /// throwaway (build_relay_deposit_frame only needs a valid signer).
    fn deposit_client(
        butler_resolver: Arc<dyn ButlerSetResolve>,
        relay_resolver: Arc<CommunityRelayResolver>,
        dial: Arc<dyn RelayDepositDial>,
        cas: Arc<dyn crate::content_store::ContentStore>,
        sender_owner: [u8; 16],
        crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
        membership: Arc<dyn CommunityMembershipLookup>,
    ) -> ProdCommunityRelayDepositClient {
        ProdCommunityRelayDepositClient {
            butler_resolver,
            relay_resolver,
            dial,
            cas,
            sender_owner,
            enrollment_cert_bytes: vec![0xCE, 0x12],
            device_signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
            crdt_state,
            membership,
            // Self-host shortcut disabled by default; the ZEB-524 tests below
            // build a variant with a wired local ctx.
            local_relay_ctx: None,
        }
    }

    /// The ed25519 verify-key bytes of the throwaway signer `deposit_client`
    /// wires as `device_signing_key`. A relay advertiser carrying this as its
    /// `relay_device_ed25519_verify` is recognized as THIS node (self) by the
    /// ZEB-524 fan-out.
    fn self_relay_vk() -> [u8; 32] {
        ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])
            .verifying_key()
            .to_bytes()
    }

    /// A relay resolver holding one advertiser per entry. Each entry is
    /// `(community_byte, relay_device_id_seed, relay_device_ed25519_verify)`,
    /// so a test can plant a SELF advertiser (vk == [`self_relay_vk`]) alongside
    /// distinct-host advertisers. Mirrors [`relay_resolver_with`] but exposes
    /// the verify key (the field the ZEB-524 self-check compares).
    fn relay_resolver_with_vks(
        entries: &[(u8, u8, [u8; 32])],
        now: u64,
    ) -> Arc<CommunityRelayResolver> {
        let r = CommunityRelayResolver::new();
        for (community_byte, relay_seed, vk) in entries {
            let payload = CommunityRelayAnnouncePayload {
                relay: CommunityRelayEntry {
                    relay_device_id: [*relay_seed; 16],
                    iroh_endpoint_id: [*relay_seed; 32],
                    relay_device_ed25519_verify: *vk,
                    home_relay: String::new(),
                },
                ad_at: now,
                identity_signature: [0; 64],
            };
            r.update(
                SpaceId([*community_byte; 16]),
                OwnerAddr([*relay_seed; 16]),
                payload,
                Hlc {
                    wall_ms: now,
                    logical: 0,
                    device_id: "r".into(),
                },
            );
        }
        Arc::new(r)
    }

    /// Build a deposit client with a wired local relay ctx (ZEB-524 self-hold),
    /// returning the client plus the in-memory relay-hold doc the ctx persists
    /// into so a test can assert what was held.
    #[allow(clippy::too_many_arguments)]
    fn deposit_client_with_local(
        butler_resolver: Arc<dyn ButlerSetResolve>,
        relay_resolver: Arc<CommunityRelayResolver>,
        dial: Arc<dyn RelayDepositDial>,
        cas: Arc<dyn crate::content_store::ContentStore>,
        sender_owner: [u8; 16],
        crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
        membership: Arc<dyn CommunityMembershipLookup>,
        community: SpaceId,
    ) -> (
        ProdCommunityRelayDepositClient,
        Arc<tokio::sync::Mutex<RelayHoldDoc>>,
    ) {
        let relay_hold_doc = Arc::new(tokio::sync::Mutex::new(RelayHoldDoc::default()));
        // The local ctx serves `community` and treats sender+recipient as
        // co-members — but the self-hold path skips those admission checks
        // anyway; only `persist_hold`, `mint_hlc`, and `relay_device_id` run.
        let ctx: Arc<dyn RelayDepositCtx> = Arc::new(deposit_ctx(
            sender_owner,
            Arc::clone(&relay_hold_doc),
            optin_doc(community, true),
            Arc::clone(&membership),
        ));
        let mut client = deposit_client(
            butler_resolver,
            relay_resolver,
            dial,
            cas,
            sender_owner,
            crdt_state,
            membership,
        );
        client.local_relay_ctx = Some(ctx);
        (client, relay_hold_doc)
    }

    #[tokio::test]
    async fn deposit_false_when_no_fresh_butler_set() {
        // No seal target → no device to seal to → false (D35 accepted gap),
        // and NO relay dialed.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));

        let client = deposit_client(
            Arc::new(FakeButlerSetResolve { targets: vec![] }), // EMPTY butler set
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::new(),
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_with_spaces(&[(community, SpaceKind::Community)]),
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        assert!(!client.deposit(&req(recipient, cid)).await);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no relay should be dialed");
    }

    #[tokio::test]
    async fn deposit_false_when_no_shared_community() {
        // A fresh butler set exists, but the only shared-kind space is one the
        // recipient has NOT joined → no shared community → false, no dial.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));

        let client = deposit_client(
            Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA)],
            }),
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::from([[0x01; 16]]), // would ack if reached
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_with_spaces(&[(community, SpaceKind::Community)]),
            // Only the SENDER is joined — recipient is not.
            fake(&[(SpaceId([community; 16]), sender)]),
        );

        assert!(!client.deposit(&req(recipient, cid)).await);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no relay should be dialed");
    }

    #[tokio::test]
    async fn deposit_false_when_dm_only_no_community_kind_space() {
        // A DM space the recipient is in is NOT a Community kind → excluded by
        // find_shared_communities → no shared community → false.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let dm = 0xD0;
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));

        let client = deposit_client(
            Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA)],
            }),
            relay_resolver_with(&[(dm, 0x01)], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::from([[0x01; 16]]),
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_with_spaces(&[(dm, SpaceKind::Dm)]),
            fake(&[(SpaceId([dm; 16]), sender), (SpaceId([dm; 16]), recipient)]),
        );

        assert!(!client.deposit(&req(recipient, cid)).await);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn deposit_false_when_cas_miss() {
        // Butler set + shared community both present, but the storage blob is
        // missing from CAS → false.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);

        let client = deposit_client(
            Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA)],
            }),
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::from([[0x01; 16]]),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(InMemoryStub::default()), // EMPTY CAS — get returns None
            sender,
            crdt_with_spaces(&[(community, SpaceKind::Community)]),
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        assert!(!client.deposit(&req(recipient, cid)).await);
    }

    #[tokio::test]
    async fn deposit_true_when_first_relay_acks() {
        // The single relay acks → true.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));

        let client = deposit_client(
            Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA)],
            }),
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::from([[0x01; 16]]),
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_with_spaces(&[(community, SpaceKind::Community)]),
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        assert!(client.deposit(&req(recipient, cid)).await);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one target → one dial");
    }

    #[tokio::test]
    async fn deposit_true_first_acking_relay_wins_no_later_relay_dialed() {
        // Two relays in the community: the FIRST (freshest) acks. With one
        // butler target, exactly one dial happens and the second relay is never
        // tried.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));

        // relay 0x0A advertised LATER (fresher) so it sorts first; both ack.
        let resolver = CommunityRelayResolver::new();
        for (seed, ad_at) in [(0x0Au8, TEST_NOW + 10), (0x0Bu8, TEST_NOW)] {
            resolver.update(
                SpaceId([community; 16]),
                OwnerAddr([seed; 16]),
                CommunityRelayAnnouncePayload {
                    relay: CommunityRelayEntry {
                        relay_device_id: [seed; 16],
                        iroh_endpoint_id: [seed; 32],
                        relay_device_ed25519_verify: [seed; 32],
                        home_relay: String::new(),
                    },
                    ad_at,
                    identity_signature: [0; 64],
                },
                Hlc {
                    wall_ms: ad_at,
                    logical: 0,
                    device_id: "r".into(),
                },
            );
        }

        let client = deposit_client(
            Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA)],
            }),
            Arc::new(resolver),
            Arc::new(MockDial {
                acking: HashSet::from([[0x0A; 16], [0x0B; 16]]),
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_with_spaces(&[(community, SpaceKind::Community)]),
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        assert!(client.deposit(&req(recipient, cid)).await);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "first acking relay wins; the second relay is not dialed"
        );
    }

    #[tokio::test]
    async fn deposit_falls_through_to_second_relay_when_first_unreachable() {
        // First relay (fresher) is unreachable; second acks → true. Two dials.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));

        let resolver = CommunityRelayResolver::new();
        for (seed, ad_at) in [(0x0Au8, TEST_NOW + 10), (0x0Bu8, TEST_NOW)] {
            resolver.update(
                SpaceId([community; 16]),
                OwnerAddr([seed; 16]),
                CommunityRelayAnnouncePayload {
                    relay: CommunityRelayEntry {
                        relay_device_id: [seed; 16],
                        iroh_endpoint_id: [seed; 32],
                        relay_device_ed25519_verify: [seed; 32],
                        home_relay: String::new(),
                    },
                    ad_at,
                    identity_signature: [0; 64],
                },
                Hlc {
                    wall_ms: ad_at,
                    logical: 0,
                    device_id: "r".into(),
                },
            );
        }

        let client = deposit_client(
            Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA)],
            }),
            Arc::new(resolver),
            Arc::new(MockDial {
                // Only the SECOND relay (0x0B) acks.
                acking: HashSet::from([[0x0B; 16]]),
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_with_spaces(&[(community, SpaceKind::Community)]),
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        assert!(client.deposit(&req(recipient, cid)).await);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "first relay unreachable, second acks → two dials"
        );
    }

    // ---------------------------------------------------------------
    // ZEB-524: sender is (also) the community's relay host.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn deposit_self_relay_holds_locally_without_dialing() {
        // The ONLY relay advertiser for the community is THIS node itself
        // (relay vk == our device signing vk). Pre-fix, the fan-out iroh-dialed
        // our own endpoint (rejected) and nothing was held. Now we hold B's
        // sealed copy in our own rung via the full acceptor pipeline with NO
        // dial. deposit → true, zero dials, one entry held for the recipient.
        let sender = mint_test_owner(0x51);
        let recipient = [0x02; 16];
        let community_byte = 0xC1;
        let community = SpaceId([community_byte; 16]);
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));
        let self_vk = sender.device_key.verifying_key().to_bytes();

        let (client, store) = deposit_client_self_minted(
            &sender,
            recipient,
            relay_resolver_with_vks(&[(community_byte, 0x0A, self_vk)], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::new(), // dialing ourselves must never happen
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            community_byte,
            true, // opted in
        );

        assert!(
            client.deposit(&req(recipient, cid)).await,
            "sole-host self relay must hold locally → true"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a self relay is held locally, never dialed"
        );
        let held = store.lock().unwrap();
        assert_eq!(held.len(), 1, "exactly one sealed copy held");
        let entry = held.values().next().unwrap();
        assert_eq!(entry.recipient_owner, recipient);
        assert_eq!(entry.sender_owner, sender.owner.0);
        assert_eq!(entry.community_id, community);
    }

    #[tokio::test]
    async fn deposit_self_hold_rejected_when_opted_out() {
        // ZEB-524 (Qodo): a self relay that is OPTED OUT (`serves_community` =
        // false) must NOT hold — else deposit would report success but our own
        // pull acceptor would refuse to serve it, a silent delivery failure.
        // Because the self-hold runs the full acceptor pipeline, the opt-in gate
        // rejects it exactly as a dialed-in deposit to an opted-out host would.
        let sender = mint_test_owner(0x51);
        let recipient = [0x02; 16];
        let community_byte = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));
        let self_vk = sender.device_key.verifying_key().to_bytes();

        let (client, store) = deposit_client_self_minted(
            &sender,
            recipient,
            relay_resolver_with_vks(&[(community_byte, 0x0A, self_vk)], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::new(),
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            community_byte,
            false, // OPTED OUT — but the self-ad is still fresh in the resolver
        );

        assert!(
            !client.deposit(&req(recipient, cid)).await,
            "opted-out self relay must not hold → false"
        );
        assert!(
            store.lock().unwrap().is_empty(),
            "nothing held when opted out (no silent delivery failure)"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a self relay is never dialed"
        );
    }

    #[tokio::test]
    async fn deposit_prefers_distinct_host_over_self_hold() {
        // Both a DISTINCT relay host (0x0B) and this node itself advertise. A
        // separate host survives our going offline, so it is preferred: the
        // distinct host acks, we return true WITHOUT a local self-hold.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community_byte = 0xC1;
        let community = SpaceId([community_byte; 16]);
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));

        let (client, hold_doc) = deposit_client_with_local(
            Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA)],
            }),
            relay_resolver_with_vks(
                &[
                    (community_byte, 0x0A, self_relay_vk()), // self
                    (community_byte, 0x0B, [0x0B; 32]),      // distinct host
                ],
                TEST_NOW,
            ),
            Arc::new(MockDial {
                acking: HashSet::from([[0x0B; 16]]), // the distinct host acks
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_with_spaces(&[(community_byte, SpaceKind::Community)]),
            fake(&[(community, sender), (community, recipient)]),
            community,
        );

        assert!(client.deposit(&req(recipient, cid)).await);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "distinct host tried first and acked → one dial, self never reached"
        );
        assert!(
            hold_doc.lock().await.entries.is_empty(),
            "a distinct host held the copy; no local self-hold"
        );
    }

    #[tokio::test]
    async fn deposit_falls_back_to_self_hold_when_distinct_host_unreachable() {
        // A distinct host advertises but is unreachable; this node also
        // advertises. The distinct host is tried first (fails), then we fall
        // back to a local self-hold. deposit → true, one (failed) dial, one held.
        let sender = mint_test_owner(0x51);
        let recipient = [0x02; 16];
        let community_byte = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));
        let self_vk = sender.device_key.verifying_key().to_bytes();

        let (client, store) = deposit_client_self_minted(
            &sender,
            recipient,
            relay_resolver_with_vks(
                &[
                    (community_byte, 0x0A, self_vk),    // self
                    (community_byte, 0x0B, [0x0B; 32]), // distinct host
                ],
                TEST_NOW,
            ),
            Arc::new(MockDial {
                acking: HashSet::new(), // the distinct host is unreachable
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            community_byte,
            true, // opted in
        );

        assert!(client.deposit(&req(recipient, cid)).await);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "distinct host dialed once (failed); the self relay is not dialed"
        );
        assert_eq!(
            store.lock().unwrap().len(),
            1,
            "fell back to a local self-hold"
        );
    }

    #[tokio::test]
    async fn deposit_self_relay_skipped_when_no_local_ctx() {
        // Safe degradation: a deposit client with NO local relay ctx wired
        // (local_relay_ctx = None) and only a self relay advertiser holds
        // nothing and — critically — never dials itself. deposit → false.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community_byte = 0xC1;
        let community = SpaceId([community_byte; 16]);
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));

        let client = deposit_client(
            Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA)],
            }),
            relay_resolver_with_vks(&[(community_byte, 0x0A, self_relay_vk())], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::new(),
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_with_spaces(&[(community_byte, SpaceKind::Community)]),
            fake(&[(community, sender), (community, recipient)]),
        );

        assert!(
            !client.deposit(&req(recipient, cid)).await,
            "no local ctx → self relay skipped → nothing held → false"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a self relay is never dialed even when the self-hold is disabled"
        );
    }

    #[tokio::test]
    async fn deposit_false_when_no_relay_acks() {
        // Shared community + butler set + blob all present, but the relay never
        // acks → false (every copy/relay tried).
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));

        let client = deposit_client(
            Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA), target(0xBB)], // two device targets
            }),
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::new(), // nothing acks
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_with_spaces(&[(community, SpaceKind::Community)]),
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        assert!(!client.deposit(&req(recipient, cid)).await);
        // Both device-target copies attempted against the one relay.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn deposit_true_deposits_all_device_copies_to_first_acking_relay() {
        // Two butler-set device targets, one relay that acks: ALL copies are
        // deposited to that first acking relay (2 dials), then true.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);
        let calls = Arc::new(AtomicUsize::new(0));

        let client = deposit_client(
            Arc::new(FakeButlerSetResolve {
                targets: vec![target(0xAA), target(0xBB)],
            }),
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::from([[0x01; 16]]),
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_with_spaces(&[(community, SpaceKind::Community)]),
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        assert!(client.deposit(&req(recipient, cid)).await);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "both device-target copies deposited to the first acking relay"
        );
    }

    // =================================================================
    // Task 5: relay rung enrolled-device fallback (butler-less recipient).
    // When the recipient advertises NO butler-set, the relay deposit falls
    // back to their ENROLLED device ed25519 vks from durable community
    // membership (OwnerDeviceCache). The relay only seals to the vk (it
    // holds; R pulls) so this is safe and D35-bounded (<=2).
    // =================================================================

    /// Relay-dial seam mock that ACKS every dial AND records each frame's
    /// `sealed_blob`, so a test can prove which device key each copy sealed to.
    struct RecordingDial {
        sealed_blobs: Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>,
    }

    #[async_trait]
    impl RelayDepositDial for RecordingDial {
        async fn dial_deposit(
            &self,
            _relay: &CommunityRelayEntry,
            frame: &RelayDepositFrame,
        ) -> Result<(), String> {
            self.sealed_blobs
                .lock()
                .await
                .push(frame.sealed_blob.clone());
            Ok(())
        }
    }

    /// A real ed25519 device identity for the recipient: returns the signing
    /// key (so the test holds the x25519 priv to OPEN the sealed blob) and the
    /// canonical 64-byte `X25519_pub(32) || Ed25519_pub(32)` identity pub the
    /// owner_device_cache stores.
    fn recipient_device_identity(seed: u8) -> (ed25519_dalek::SigningKey, [u8; 64]) {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let ed_pub: [u8; 32] = signing.verifying_key().to_bytes();
        let x_pub = crate::dm_signing::ed25519_pub_to_x25519(&ed_pub)
            .expect("real ed25519 pub converts to x25519");
        let mut identity_pub = [0u8; 64];
        identity_pub[..32].copy_from_slice(&x_pub);
        identity_pub[32..].copy_from_slice(&ed_pub);
        (signing, identity_pub)
    }

    #[tokio::test]
    async fn relay_deposit_enrolled_device_fallback() {
        use crate::dm_signing::{ed25519_priv_to_x25519, open_from_owner_with_info};
        use crate::owner_state_types::{DeviceIdentityHash, OwnerDeviceEntry};

        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);

        // Recipient has two ENROLLED devices in durable membership, but NO
        // butler-set (the resolver returns empty).
        let (dev_a_key, dev_a_pub) = recipient_device_identity(0xA1);
        let (dev_b_key, dev_b_pub) = recipient_device_identity(0xB2);

        // Build the recipient's owner_device_cache entry from those two pubs.
        let crdt_state = crdt_with_spaces(&[(community, SpaceKind::Community)]);
        {
            let mut st = crdt_state.lock().await;
            st.owner_device_cache.devices.insert(
                OwnerAddr(recipient),
                OwnerDeviceEntry {
                    devices: vec![
                        DeviceIdentityHash([0xA1; 16]),
                        DeviceIdentityHash([0xB2; 16]),
                    ],
                    device_identity_pubs: vec![Some(dev_a_pub), Some(dev_b_pub)],
                    learned_at: Hlc {
                        wall_ms: 1,
                        logical: 0,
                        device_id: "t".into(),
                    },
                    device_tunnel_contacts: vec![None, None],
                },
            );
        }

        let sealed_blobs = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let client = deposit_client(
            Arc::new(FakeButlerSetResolve { targets: vec![] }), // EMPTY butler set
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(RecordingDial {
                sealed_blobs: sealed_blobs.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_state,
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        // Deposit succeeds via the enrolled-device fallback.
        assert!(
            client.deposit(&req(recipient, cid)).await,
            "deposit must succeed by sealing to enrolled device vks"
        );

        // Exactly two copies (one per enrolled device, capped at
        // BUTLER_SET_MAX_ENTRIES == 2).
        let blobs = sealed_blobs.lock().await.clone();
        assert_eq!(
            blobs.len(),
            crate::butler_deposit::BUTLER_SET_MAX_ENTRIES,
            "one sealed copy per enrolled device, capped at 2"
        );

        // Each enrolled device's x25519 priv (derived from its ed25519 signing
        // key — the [32..64] half of its identity pub) opens EXACTLY ONE copy.
        // This proves the fallback sealed to the enrolled ed25519 vks.
        for key in [&dev_a_key, &dev_b_key] {
            let x_priv = ed25519_priv_to_x25519(key);
            let opened = blobs
                .iter()
                .filter(|blob| {
                    open_from_owner_with_info(
                        &x_priv,
                        blob,
                        crate::community_relay::COMMUNITY_RELAY_SEAL_INFO,
                    )
                    .is_ok()
                })
                .count();
            assert_eq!(
                opened, 1,
                "each enrolled device key must open exactly one sealed copy"
            );
        }
    }

    #[tokio::test]
    async fn relay_deposit_enrolled_device_fallback_skips_none_pubs() {
        // A recipient whose middle device has a known hash but no propagated
        // identity_pub (`device_identity_pubs = [Some(a), None, Some(b)]`).
        // The `None` is skipped (NOT counted toward the cap), and the two
        // surviving devices (a, b) are the seal targets — proving the
        // filter_map branch is exercised and the right keys win.
        use crate::dm_signing::{ed25519_priv_to_x25519, open_from_owner_with_info};
        use crate::owner_state_types::{DeviceIdentityHash, OwnerDeviceEntry};

        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);

        let (dev_a_key, dev_a_pub) = recipient_device_identity(0xA1);
        let (dev_b_key, dev_b_pub) = recipient_device_identity(0xB2);

        let crdt_state = crdt_with_spaces(&[(community, SpaceKind::Community)]);
        {
            let mut st = crdt_state.lock().await;
            st.owner_device_cache.devices.insert(
                OwnerAddr(recipient),
                OwnerDeviceEntry {
                    // Three parallel devices; the middle pub is unpropagated.
                    devices: vec![
                        DeviceIdentityHash([0xA1; 16]),
                        DeviceIdentityHash([0xCC; 16]),
                        DeviceIdentityHash([0xB2; 16]),
                    ],
                    device_identity_pubs: vec![Some(dev_a_pub), None, Some(dev_b_pub)],
                    learned_at: Hlc {
                        wall_ms: 1,
                        logical: 0,
                        device_id: "t".into(),
                    },
                    device_tunnel_contacts: vec![None, None, None],
                },
            );
        }

        let sealed_blobs = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let client = deposit_client(
            Arc::new(FakeButlerSetResolve { targets: vec![] }), // EMPTY butler set
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(RecordingDial {
                sealed_blobs: sealed_blobs.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_state,
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        assert!(client.deposit(&req(recipient, cid)).await);

        // Exactly two copies — the `None` is skipped, not counted toward the cap.
        let blobs = sealed_blobs.lock().await.clone();
        assert_eq!(
            blobs.len(),
            2,
            "the None-pub device is skipped; exactly the two propagated devices are sealed to"
        );

        // The two surviving devices (a, b) are the seal targets.
        for key in [&dev_a_key, &dev_b_key] {
            let x_priv = ed25519_priv_to_x25519(key);
            let opened = blobs
                .iter()
                .filter(|blob| {
                    open_from_owner_with_info(
                        &x_priv,
                        blob,
                        crate::community_relay::COMMUNITY_RELAY_SEAL_INFO,
                    )
                    .is_ok()
                })
                .count();
            assert_eq!(
                opened, 1,
                "each propagated device key must open exactly one sealed copy"
            );
        }
    }

    #[tokio::test]
    async fn relay_deposit_enrolled_device_fallback_caps_at_two() {
        // Recipient has THREE enrolled devices but no butler-set; the fallback
        // must seal to at most BUTLER_SET_MAX_ENTRIES (2) of them (D35).
        use crate::owner_state_types::{DeviceIdentityHash, OwnerDeviceEntry};

        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);

        let (_a, pub_a) = recipient_device_identity(0xA1);
        let (_b, pub_b) = recipient_device_identity(0xB2);
        let (_c, pub_c) = recipient_device_identity(0xC3);

        let crdt_state = crdt_with_spaces(&[(community, SpaceKind::Community)]);
        {
            let mut st = crdt_state.lock().await;
            st.owner_device_cache.devices.insert(
                OwnerAddr(recipient),
                OwnerDeviceEntry {
                    devices: vec![
                        DeviceIdentityHash([0xA1; 16]),
                        DeviceIdentityHash([0xB2; 16]),
                        DeviceIdentityHash([0xC3; 16]),
                    ],
                    device_identity_pubs: vec![Some(pub_a), Some(pub_b), Some(pub_c)],
                    learned_at: Hlc {
                        wall_ms: 1,
                        logical: 0,
                        device_id: "t".into(),
                    },
                    device_tunnel_contacts: vec![None, None, None],
                },
            );
        }

        let sealed_blobs = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let client = deposit_client(
            Arc::new(FakeButlerSetResolve { targets: vec![] }),
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(RecordingDial {
                sealed_blobs: sealed_blobs.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_state,
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        assert!(client.deposit(&req(recipient, cid)).await);
        assert_eq!(
            sealed_blobs.lock().await.len(),
            crate::butler_deposit::BUTLER_SET_MAX_ENTRIES,
            "fallback fan-out is bounded at BUTLER_SET_MAX_ENTRIES (D35)"
        );
    }

    #[tokio::test]
    async fn relay_deposit_no_fallback_when_recipient_absent_from_cache() {
        // No butler-set AND no owner_device_cache entry for the recipient →
        // nothing to seal to → false, no dial.
        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);

        let sealed_blobs = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let client = deposit_client(
            Arc::new(FakeButlerSetResolve { targets: vec![] }),
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(RecordingDial {
                sealed_blobs: sealed_blobs.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            // owner_device_cache has NO entry for the recipient.
            crdt_with_spaces(&[(community, SpaceKind::Community)]),
            fake(&[
                (SpaceId([community; 16]), sender),
                (SpaceId([community; 16]), recipient),
            ]),
        );

        assert!(!client.deposit(&req(recipient, cid)).await);
        assert!(
            sealed_blobs.lock().await.is_empty(),
            "no enrolled devices and no butler-set → no dial"
        );
    }

    /// Greptile P2 regression: the D40 shared-community gate is checked BEFORE
    /// the enrolled-device fallback, so a STALE `OwnerDeviceCache` entry for a
    /// recipient we no longer share a community with cannot drive a deposit (or
    /// the fallback's target derivation). The recipient HAS a derivable enrolled
    /// device here AND the relay would ack — but with no shared community the
    /// deposit short-circuits to false with no dial.
    #[tokio::test]
    async fn relay_deposit_no_deposit_when_stale_cache_but_no_shared_community() {
        use crate::owner_state_types::{DeviceIdentityHash, OwnerDeviceEntry};

        let sender = [0x01; 16];
        let recipient = [0x02; 16];
        let community = 0xC1;
        let cid = ContentId::from_bytes([0x33; 32]);

        // A derivable enrolled-device entry survives in the cache (e.g. from an
        // old projection before the sender left the shared community).
        let (_dev_key, dev_pub) = recipient_device_identity(0xA1);
        let crdt_state = crdt_with_spaces(&[(community, SpaceKind::Community)]);
        {
            let mut st = crdt_state.lock().await;
            st.owner_device_cache.devices.insert(
                OwnerAddr(recipient),
                OwnerDeviceEntry {
                    devices: vec![DeviceIdentityHash([0xA1; 16])],
                    device_identity_pubs: vec![Some(dev_pub)],
                    learned_at: Hlc {
                        wall_ms: 1,
                        logical: 0,
                        device_id: "t".into(),
                    },
                    device_tunnel_contacts: vec![None],
                },
            );
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let client = deposit_client(
            Arc::new(FakeButlerSetResolve { targets: vec![] }), // no butler-set
            relay_resolver_with(&[(community, 0x01)], TEST_NOW),
            Arc::new(MockDial {
                acking: HashSet::from([[0x01; 16]]), // would ack if reached
                calls: calls.clone(),
            }),
            cas_with_blob(&cid, vec![1, 2, 3]).await,
            sender,
            crdt_state,
            // Only the SENDER is joined — recipient is NOT a co-member.
            fake(&[(SpaceId([community; 16]), sender)]),
        );

        assert!(
            !client.deposit(&req(recipient, cid)).await,
            "no shared community → false even though a stale enrolled-device entry exists"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "D40 gate short-circuits before any relay dial"
        );
    }

    // =================================================================
    // ZEB-483 Task 5: ProdRelayIngestCtx::ingest_recovered applies a
    // piggybacked DmInvite to bootstrap the DM Space BEFORE CidNotify
    // admission, mirroring the butler recover path
    // (dm_inbox_ingest::deposited_invite_bootstraps_space_then_cidnotify_admits).
    // The relay ctx holds the full unsealed DepositPayload, so the invite is
    // `payload.invite_packet` (not an entry field).
    // =================================================================

    /// A built recover fixture: a `DepositPayload` (CidNotify + blob + invite)
    /// for a DM Space the recipient (Bob) has NOT bootstrapped, plus the prod
    /// ingest ctx over Bob's fresh state. Alice is the sender/inviter. The blob
    /// is encrypted under the SAME member set + content_key the invite carries,
    /// so once the invite bootstraps the Space its recomputed AAD + content_key
    /// decrypt it (DM-Space AAD = DedupeKey::SortedMembers, SpaceId-independent).
    struct RelayRecoverInviteFixture {
        prod_ctx: ProdRelayIngestCtx,
        crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
        payload: DepositPayload,
        space_id: SpaceId,
        sink: std::sync::Arc<crate::node_event_sink::RecordingSink>,
        // Inputs needed to forge a mismatched-inviter invite for the negative test.
        bob: OwnerAddr,
        created_at: Hlc,
        content_key: crate::owner_state_types::DmContentKey,
        /// ZEB-709: counts `notify_owner_state_dirty` invocations from the ctx.
        dirty: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RelayRecoverInviteFixture {
        /// Number of `dm-received` frames the recording sink saw.
        fn emitted_dm_received(&self) -> usize {
            self.sink
                .frames()
                .into_iter()
                .filter(|(ev, _)| ev == crate::dm_outbox::DM_RECEIVED_EVENT)
                .count()
        }

        /// A clone of the payload whose invite is re-signed with an inviter that
        /// is NOT the CidNotify's sender (a third, validly-signing owner).
        /// `apply_deposited_invite` binds the invite's `inviter` to the
        /// CidNotify's `sender_owner_addr` (Alice), so this must fail-closed.
        fn payload_with_mismatched_inviter(&self) -> DepositPayload {
            let private_charlie = harmony_identity::PrivateIdentity::from_seed(&[0xC3; 32]);
            let charlie_pub = private_charlie.public_identity();
            let charlie_identity_pub = charlie_pub.to_public_bytes();
            let charlie = OwnerAddr([0xC3; 16]);
            let charlie_device_hash =
                crate::owner_state_types::DeviceIdentityHash(charlie_pub.address_hash);

            let mut members = vec![charlie, self.bob];
            members.sort();
            let signed = crate::dm_envelope::DmInviteSigned {
                space_id: self.space_id,
                kind: SpaceKind::Dm,
                members,
                inviter: charlie,
                content_key: self.content_key.clone(),
                sender_devices: vec![charlie_device_hash],
                created_at: self.created_at.clone(),
                signing_device_hash: charlie_device_hash,
                inviter_identity_pub: charlie_identity_pub,
                inviter_enrollment: None,
            };
            let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
            let signature = private_charlie.sign(&signed_bytes);
            let invite_wire =
                crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Invite {
                    signed,
                    signature,
                    signed_bytes,
                })
                .unwrap();
            let mut payload = self.payload.clone();
            payload.invite_packet = Some(invite_wire);
            payload
        }

        /// A clone of the payload as a ZEB-505 invite-only deposit: the CidNotify
        /// and storage blob are stripped, leaving only Alice's bootstrap invite.
        fn invite_only_payload(&self) -> DepositPayload {
            let mut p = self.payload.clone();
            p.cidnotify_packet = None;
            p.storage_blob = Vec::new();
            p
        }
    }

    fn relay_ingest_fixture_without_space_with_invite(
        pre_cache_sender: bool,
    ) -> RelayRecoverInviteFixture {
        use crate::owner_state_types::{ContentId, DmContentKey, Space};

        let alice = OwnerAddr([0xA1; 16]);
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x5A; 16]);
        let content_key = DmContentKey::new([0x42u8; 32]);
        let body = b"recovered over the relay rung".to_vec();

        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub = private_alice.public_identity();
        let alice_identity_pub = alice_pub.to_public_bytes();
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub.address_hash);

        let mut members = vec![alice, bob];
        members.sort();
        let created_at = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice-dev".into(),
        };

        // (1) Encrypt the DM blob under a transient Space whose members match the
        //     ones the invite carries — `compute_aad` for a DM Space is keyed on
        //     the SORTED member set only (DedupeKey::SortedMembers), so the Space
        //     the invite later bootstraps recomputes the identical AAD and the
        //     same `content_key` decrypts it. SpaceId/name are AAD-irrelevant.
        let aad_space = Space {
            id: space_id,
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "Alice".into(),
            transport: None,
            members: members.clone(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: created_at.clone(),
            updated_at: created_at.clone(),
            content_key: Some(content_key.clone()),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };
        let msg_payload = crate::dm_envelope::MessagePayload {
            body: body.clone(),
            mime_type: "text/plain".into(),
            sender: alice,
            sent_at: Hlc {
                wall_ms: 150,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        };
        let aad = crate::dm_crypto::compute_aad(&aad_space).unwrap();
        let storage_blob =
            crate::dm_crypto::encrypt_dm_message(&content_key, &aad, &msg_payload).unwrap();
        let message_cid = ContentId::for_book(
            &storage_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();

        // (2) Signed CidNotify from Alice for this blob.
        let cn_signed = crate::dm_envelope::DmCidNotifySigned {
            space_id,
            message_cid,
            sender_owner_addr: alice,
            sender_devices: vec![alice_device_hash],
            signing_device_hash: alice_device_hash,
        };
        let cn_signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&cn_signed).unwrap();
        let cn_signature = private_alice.sign(&cn_signed_bytes);
        let cidnotify_packet =
            crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::CidNotify {
                signed: cn_signed,
                signature: cn_signature,
                signed_bytes: cn_signed_bytes,
            })
            .unwrap();

        // (3) Signed DmInvite from Alice carrying the SAME content_key + member
        //     set — this is what bootstraps the Space + caches Alice's device.
        let inv_signed = crate::dm_envelope::DmInviteSigned {
            space_id,
            kind: SpaceKind::Dm,
            members: members.clone(),
            inviter: alice,
            content_key: content_key.clone(),
            sender_devices: vec![alice_device_hash],
            created_at: created_at.clone(),
            signing_device_hash: alice_device_hash,
            inviter_identity_pub: alice_identity_pub,
            inviter_enrollment: None,
        };
        let inv_signed_bytes =
            crate::owner_state_crypto::canonical_cbor_encode(&inv_signed).unwrap();
        let inv_signature = private_alice.sign(&inv_signed_bytes);
        let invite_wire =
            crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Invite {
                signed: inv_signed,
                signature: inv_signature,
                signed_bytes: inv_signed_bytes,
            })
            .unwrap();

        let payload = DepositPayload {
            cidnotify_packet: Some(cidnotify_packet),
            storage_blob,
            invite_packet: Some(invite_wire),
            revocation_push: None,
            grant_push: None,
        };

        // Bob's state: NO Space (offline-at-create). Alice's signing device is
        // pre-cached IFF `pre_cache_sender` — the legitimate path (Alice is an
        // existing co-member/friend) resolves the signer from this PRISTINE cache
        // BEFORE the invite runs, and the invite bootstraps only the missing
        // Space. Without the pre-cache, sender-binding rejects fail-closed
        // (ZEB-483 / CodeRabbit Critical: the invite can no longer seed trust).
        let mut bob_state = OwnerState::default();
        if pre_cache_sender {
            bob_state.apply_owner_device_update(
                alice,
                vec![alice_device_hash],
                vec![Some(alice_identity_pub)],
                vec![None],
                Hlc {
                    wall_ms: 50,
                    logical: 0,
                    device_id: "bob-device-64hex".into(),
                },
            );
            // ZEB-236: the legitimate offline-DM sender is an ACTIVE friend, so a
            // recovered invite AUTO-ACCEPTS (bootstraps the Space). Without an
            // active friendship the tier fork would STAGE it instead of applying.
            bob_state
                .friend_graph
                .friends
                .insert(alice, crate::friend_graph::active_friend_entry_for_test(1));
        }
        let crdt_state = Arc::new(tokio::sync::Mutex::new(bob_state));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            Arc::new(Arc::clone(&sink_handle));

        let dirty = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let prod_ctx = ProdRelayIngestCtx {
            device_id: "bob-device-64hex".into(),
            device_x25519_privs: vec![],
            self_owner: bob,
            crdt_state: Arc::clone(&crdt_state),
            content_store,
            sink,
            pending_dm_invites: None,
            revoked: crate::revoked_device_projection::RevokedDeviceProjection::new(),
            // ZEB-709: count the owner-state dirty arms so tests can pin them.
            notify_owner_state_dirty: {
                let d = std::sync::Arc::clone(&dirty);
                Some(Arc::new(move || {
                    d.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }) as Arc<dyn Fn() + Send + Sync>)
            },
        };

        RelayRecoverInviteFixture {
            prod_ctx,
            crdt_state,
            payload,
            space_id,
            sink: sink_handle,
            bob,
            created_at,
            content_key,
            dirty,
        }
    }

    #[tokio::test]
    async fn ingest_recovered_invite_bootstraps_space_then_delivers() {
        // Legitimate path: Alice is an existing co-member/friend (pre-cached);
        // only the Space is missing, and the relay-held invite bootstraps it.
        let fx = relay_ingest_fixture_without_space_with_invite(true);

        // Sanity: the Space is absent pre-recover → a plain CidNotify ingest
        // would fail with SpaceNotFound.
        {
            let st = fx.crdt_state.lock().await;
            assert!(
                !st.spaces.contains_key(&fx.space_id),
                "space absent pre-recover"
            );
        }

        fx.prod_ctx
            // alice ([0xA1;16]) is the relay-authenticated sender.
            .ingest_recovered([0xA1; 16], fx.payload.clone())
            .await
            .expect("invite bootstraps space, notify admits, blob delivers");

        let st = fx.crdt_state.lock().await;
        assert!(
            st.spaces.contains_key(&fx.space_id),
            "Space bootstrapped from the relay-held invite"
        );
        drop(st);
        assert_eq!(
            fx.emitted_dm_received(),
            1,
            "dm-received emitted exactly once"
        );
    }

    /// ZEB-709: relay recover writes owner-state (Space bootstrap + inbox
    /// insert) and the relay's ack lets coverage-GC destroy the held blob —
    /// both writes must arm the owner-state flush or a crash after the ack
    /// loses the message permanently.
    #[tokio::test]
    async fn ingest_recovered_marks_owner_state_dirty_zeb709() {
        use std::sync::atomic::Ordering;
        let fx = relay_ingest_fixture_without_space_with_invite(true);

        fx.prod_ctx
            .ingest_recovered([0xA1; 16], fx.payload.clone())
            .await
            .expect("invite bootstraps space, notify admits, blob delivers");
        assert_eq!(
            fx.dirty.load(Ordering::SeqCst),
            2,
            "first recover arms owner-state twice: the co-deposit friend-tier \
             accept (Space write) and the inbox insert"
        );

        // Re-recover the same payload: the Space now exists (the co-deposit
        // arm fires its documented harmless spurious arm) and apply_inbox
        // dedupes (Merged → NO insert arm).
        fx.prod_ctx
            .ingest_recovered([0xA1; 16], fx.payload.clone())
            .await
            .expect("re-recover dedupes cleanly");
        assert_eq!(
            fx.dirty.load(Ordering::SeqCst),
            3,
            "re-recover adds only the co-deposit consumed-invite arm (space \
             exists), never a second insert arm"
        );
    }

    #[tokio::test]
    async fn ingest_recovered_invite_wrong_inviter_rejected() {
        let fx = relay_ingest_fixture_without_space_with_invite(true);
        let payload = fx.payload_with_mismatched_inviter();
        let err = fx
            .prod_ctx
            // alice ([0xA1;16]) is the relay-authenticated sender; the message
            // branch is unchanged, so this still rejects via the CidNotify bind.
            .ingest_recovered([0xA1; 16], payload)
            .await
            .expect_err("mismatched inviter must fail-closed");
        // Pinned to the verified CidNotify signer → rejects at the signer-binding
        // check (or, defense-in-depth, inside apply_invite).
        assert!(
            err.contains("does not match verified CidNotify")
                || err.contains("apply_invite")
                || err.contains("InviterMismatch"),
            "got {err}"
        );
        let st = fx.crdt_state.lock().await;
        assert!(
            !st.spaces.contains_key(&fx.space_id),
            "no Space bootstrapped on reject"
        );
        drop(st);
        assert_eq!(fx.emitted_dm_received(), 0, "no delivery on reject");
    }

    /// ZEB-483 CodeRabbit Critical regression (relay rung): a relay-held invite
    /// from a sender whose signing device is NOT already cached must be rejected
    /// before it can seed trust. Pre-fix the invite seeded `device → owner → pub`
    /// and the co-deposited CidNotify "verified" against that just-written cache
    /// (circular trust). Post-fix: sender-binding against the pristine cache
    /// rejects fail-closed — no Space, no cache poison, no delivery.
    #[tokio::test]
    async fn ingest_recovered_invite_from_uncached_sender_rejected_no_poison() {
        let fx = relay_ingest_fixture_without_space_with_invite(false);
        let alice = OwnerAddr([0xA1; 16]);

        let err = fx
            .prod_ctx
            .ingest_recovered(alice.0, fx.payload.clone())
            .await
            .expect_err("uncached sender must fail-closed (no trust bootstrap)");
        assert!(
            err.contains("verify_cidnotify_sender_binding")
                || err.contains("UnknownSigningKey")
                || err.contains("UnknownSigningDevice"),
            "must reject at sender-binding against the pristine cache, got {err}"
        );

        let st = fx.crdt_state.lock().await;
        assert!(
            !st.spaces.contains_key(&fx.space_id),
            "no Space bootstrapped from an unverified sender's invite"
        );
        assert!(
            !st.owner_device_cache.devices.contains_key(&alice),
            "device cache NOT poisoned with the claimed sender's mapping"
        );
        drop(st);
        assert_eq!(fx.emitted_dm_received(), 0, "no delivery on reject");
    }

    /// ZEB-505 / Qodo (Security, High): an invite-only recovered deposit binds
    /// the invite's `expected_inviter` to the RELAY-AUTHENTICATED `sender_owner`,
    /// NOT the payload's own `signed.inviter`. A blob recovered under the wrong
    /// authenticated sender must fail-closed; the matching sender bootstraps the
    /// Space from the invite alone.
    #[tokio::test]
    async fn ingest_recovered_invite_only_binds_to_authenticated_sender() {
        let alice = [0xA1u8; 16];
        let wrong = [0x99u8; 16];

        // (a) Wrong authenticated sender → reject at the sender bind; no Space.
        let fx = relay_ingest_fixture_without_space_with_invite(true);
        let err = fx
            .prod_ctx
            .ingest_recovered(wrong, fx.invite_only_payload())
            .await
            .expect_err("invite-only deposit under a mismatched sender must fail-closed");
        assert!(
            err.contains("authenticated relay sender"),
            "must reject at the sender bind, got {err}"
        );
        {
            let st = fx.crdt_state.lock().await;
            assert!(
                !st.spaces.contains_key(&fx.space_id),
                "no Space bootstrapped under the wrong sender"
            );
        }

        // (b) Correct authenticated sender (alice) → Space bootstrapped from the
        //     invite ALONE; no message, so no dm-received emit.
        let fx2 = relay_ingest_fixture_without_space_with_invite(true);
        fx2.prod_ctx
            .ingest_recovered(alice, fx2.invite_only_payload())
            .await
            .expect("invite-only deposit under the authenticated sender bootstraps the Space");
        let st = fx2.crdt_state.lock().await;
        assert!(
            st.spaces.contains_key(&fx2.space_id),
            "Space bootstrapped from the invite-only deposit"
        );
        drop(st);
        assert_eq!(
            fx2.emitted_dm_received(),
            0,
            "invite-only recover delivers no message"
        );
    }

    /// ZEB-641 (1): fixture variant for the relay STAGING arms — Alice is
    /// pre-cached (sender-binding resolves) but NOT an active friend (the
    /// ZEB-236 tier fork stages instead of auto-accepting), and the ctx is
    /// wired to a REAL `PendingDmInvites` (the base fixture wires `None`).
    async fn relay_non_friend_staging_fixture() -> (
        RelayRecoverInviteFixture,
        Arc<crate::pending_dm_invites::PendingDmInvites>,
    ) {
        let mut fx = relay_ingest_fixture_without_space_with_invite(true);
        {
            let mut st = fx.crdt_state.lock().await;
            st.friend_graph.friends.remove(&OwnerAddr([0xA1; 16]));
        }
        let pending = Arc::new(crate::pending_dm_invites::PendingDmInvites::new());
        fx.prod_ctx.pending_dm_invites = Some(Arc::clone(&pending));
        (fx, pending)
    }

    /// ZEB-641 (1): staging wiring pin for the relay DIRECT (invite-only)
    /// arm of `ProdRelayIngestCtx::ingest_recovered`. A non-friend
    /// invite-only recover through the real site wiring lands in a REAL
    /// `PendingDmInvites` and emits both UI events exactly once; an identical
    /// redelivery (the relay re-delivers while the invite stays unacked)
    /// emits NOTHING further (keep-first at site level).
    #[tokio::test]
    async fn ingest_recovered_invite_only_stages_non_friend_invite() {
        let (fx, pending) = relay_non_friend_staging_fixture().await;

        fx.prod_ctx
            .ingest_recovered([0xA1; 16], fx.invite_only_payload())
            .await
            .expect("a staged non-friend invite-only recover is Ok, not an error");

        // Staging writes nothing to owner-state (Space appears only on accept).
        {
            let st = fx.crdt_state.lock().await;
            assert!(
                !st.spaces.contains_key(&fx.space_id),
                "a staged (non-friend) invite must NOT write the DM Space"
            );
        }
        let staged = pending.list();
        assert_eq!(staged.len(), 1, "exactly one invite staged");
        assert_eq!(staged[0].signed.space_id, fx.space_id);
        assert_eq!(
            staged[0].source_cid, None,
            "the direct invite-only arm has no notifying message — \
             apply_invite leaves source_cid None and this site never \
             overwrites it"
        );
        assert!(
            !staged[0].refresh_owner_device_cache,
            "relay-recover route is never cache-refresh entitled (ZEB-483)"
        );

        let frames = fx.sink.frames();
        assert_eq!(
            frames
                .iter()
                .filter(|(n, _)| n == "dm-invite-received")
                .count(),
            1,
            "newly-staged invite prompts exactly once"
        );
        assert_eq!(
            frames
                .iter()
                .filter(|(n, _)| n == "dm-invite-list-changed")
                .count(),
            1,
            "list-changed fires once after staging"
        );
        assert_eq!(fx.emitted_dm_received(), 0, "no message to deliver");

        // Second identical delivery: keep-first — no re-stage, no re-emit.
        let frames_before = fx.sink.frames().len();
        fx.prod_ctx
            .ingest_recovered([0xA1; 16], fx.invite_only_payload())
            .await
            .expect("an already-pending redelivery is still Ok");
        assert_eq!(
            pending.list().len(),
            1,
            "keep-first: the redelivery must not re-stage or duplicate"
        );
        assert_eq!(
            fx.sink.frames().len(),
            frames_before,
            "an already-pending redelivery emits neither dm-invite-received \
             nor dm-invite-list-changed"
        );
    }

    /// ZEB-641 (1): staging wiring pin for the relay CO-DEPOSIT arm of
    /// `ProdRelayIngestCtx::ingest_recovered` (the `Ok(Some(staged))` branch:
    /// non-friend co-deposited invite + SpaceNotFound on the message). The
    /// invite is staged in a REAL `PendingDmInvites` tagged with the
    /// notifying message's cid, both UI events fire exactly once, and the
    /// message itself is deferred (Err → unacked for post-accept redelivery).
    /// An identical redelivery emits NOTHING further (keep-first at site
    /// level).
    #[tokio::test]
    async fn ingest_recovered_co_deposit_stages_non_friend_invite() {
        use crate::owner_state_types::ContentId;
        let (fx, pending) = relay_non_friend_staging_fixture().await;
        // The site tags the staged invite with the verified CidNotify's
        // message_cid — recompute it from the payload blob the same way.
        let expected_cid = ContentId::for_book(
            &fx.payload.storage_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();

        let err = fx
            .prod_ctx
            .ingest_recovered([0xA1; 16], fx.payload.clone())
            .await
            .expect_err("the staging arm defers the message (unacked for post-accept redelivery)");
        assert!(err.contains("SpaceNotFound"), "got {err}");

        // Staging writes nothing to owner-state (Space appears only on accept).
        {
            let st = fx.crdt_state.lock().await;
            assert!(
                !st.spaces.contains_key(&fx.space_id),
                "a staged (non-friend) invite must NOT write the DM Space"
            );
        }
        let staged = pending.list();
        assert_eq!(staged.len(), 1, "exactly one invite staged");
        assert_eq!(staged[0].signed.space_id, fx.space_id);
        assert_eq!(
            staged[0].source_cid,
            Some(expected_cid),
            "the co-deposit arm overwrites source_cid with the notifying \
             message's cid (decline-ledger key for sweep redeliveries)"
        );
        assert!(
            !staged[0].refresh_owner_device_cache,
            "relay-recover route is never cache-refresh entitled (ZEB-483)"
        );

        let frames = fx.sink.frames();
        assert_eq!(
            frames
                .iter()
                .filter(|(n, _)| n == "dm-invite-received")
                .count(),
            1,
            "newly-staged invite prompts exactly once"
        );
        assert_eq!(
            frames
                .iter()
                .filter(|(n, _)| n == "dm-invite-list-changed")
                .count(),
            1,
            "list-changed fires once after staging"
        );
        assert_eq!(
            fx.emitted_dm_received(),
            0,
            "deferred message never delivers"
        );

        // Second identical delivery (relay drain re-run while unacked):
        // keep-first — still defers, no re-stage, no re-emit.
        let frames_before = fx.sink.frames().len();
        let err2 = fx
            .prod_ctx
            .ingest_recovered([0xA1; 16], fx.payload.clone())
            .await
            .expect_err("redelivery still defers");
        assert!(err2.contains("SpaceNotFound"), "got {err2}");
        assert_eq!(
            pending.list().len(),
            1,
            "keep-first: the redelivery must not re-stage or duplicate"
        );
        assert_eq!(
            fx.sink.frames().len(),
            frames_before,
            "an already-pending redelivery emits neither dm-invite-received \
             nor dm-invite-list-changed"
        );
    }
}
