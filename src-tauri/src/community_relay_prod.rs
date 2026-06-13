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

    async fn held_for(&self, recipient_owner: &[u8; 16]) -> Vec<(String, RelayHeldBlob)> {
        let doc = self.relay_hold_doc.lock().await;
        doc.entries
            .iter()
            .filter(|(_, e)| &e.recipient_owner == recipient_owner)
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
/// `pub(crate)` receive-path helpers extracted from `handle_cidnotify_lifted`
/// (`dm_outbox::verify_cidnotify_admission`,
/// `dm_outbox::decrypt_and_bind_dm_blob`) under the owner-state lock, and the
/// SAME `dm-received` event. There is NO separate `sender_owner` field on a
/// `DepositPayload` (unlike the butler's `DmInboxEntry`), so the sender is
/// derived from the signed CidNotify packet itself by
/// `verify_cidnotify_admission` (`resolved_owner`), exactly as the direct path
/// resolves it — no out-of-band sender claim to cross-check.
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
    /// The runtime owner-state CRDT (`NodeState`'s `crdt_state` Arc) — admission
    /// (`verify_cidnotify_admission`) + `apply_inbox` happen under this lock.
    pub crdt_state: Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    /// The shared CAS handle (`RuntimeContentStore` in production) — the storage
    /// blob is put here so the message body is fetchable like a direct arrival.
    pub content_store: Arc<dyn crate::content_store::ContentStore>,
    /// Event sink for the `dm-received` emit (ZEB-445).
    pub sink: Arc<dyn crate::node_event_sink::NodeEventSink>,
}

#[async_trait]
impl RelayIngestCtx for ProdRelayIngestCtx {
    fn device_x25519_privs(&self) -> Vec<Zeroizing<[u8; 32]>> {
        self.device_x25519_privs.clone()
    }

    async fn ingest_recovered(&self, payload: DepositPayload) -> Result<(), String> {
        // 1. Decode the recovered packet — must be a signed CidNotify.
        let packet = crate::dm_envelope::decode_packet(&payload.cidnotify_packet)
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
        let (resolved_owner, payload_msg, newly_inserted, received_at, space_id, message_cid) = {
            let mut state = self.crdt_state.lock().await;
            let (space, resolved_owner, _identity_pub) =
                crate::dm_outbox::verify_cidnotify_admission(
                    &state,
                    &signed,
                    &signature,
                    &signed_bytes,
                )
                .map_err(|e| format!("verify_cidnotify_admission: {e:?}"))?;

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
    use std::collections::{BTreeSet, HashSet};

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

        let mut got = ctx.held_for(&recipient).await;
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
        let got = ctx.held_for(&[0xAA; 16]).await;
        assert!(got.is_empty());
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

        ctx.mark_pulled(&[key.clone()], "Rdev".into())
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
        ctx.mark_pulled(&[key.clone()], "Rdev".into())
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
}
