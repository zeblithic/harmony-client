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
//!   `held_for` + ack→pulled_by→GC.
//!
//! Both ctxs answer community-membership questions through a
//! [`CommunityMembershipLookup`] seam rather than reaching into the community
//! state registry directly. This keeps them unit-testable with a fake (no full
//! `CommunitySyncRegistry`); the registry-backed lookup impl is wired in at
//! `start_node` in a later task (T11).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::community_relay::{RelayHeldBlob, RELAY_HOLD_GLOBAL_CAP, RELAY_HOLD_PER_SENDER_CAP};
use crate::community_relay_hold_crdt::{RelayHoldDoc, RelayHoldEntry};
use crate::community_relay_optin::RelayOptInDoc;
use crate::fleet_sync::FleetSyncEngine;
use crate::iroh_community_relay_acceptor::{RelayDepositCtx, RelayPersistVerdict, RelayPullCtx};
use crate::owner_state_types::{Hlc, SpaceId};

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

/// Wall-clock epoch MILLISECONDS (for relay-hold GC).
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
/// occupied key → `Duplicate` (caps bypassed); vacant within caps → insert +
/// flush → `Inserted`; vacant over caps → `CapExceeded` (nothing inserted /
/// flushed). There is NO local ingest sweeper to nudge on a relay (the relay is
/// never the recipient of the blobs it holds), so that step is omitted.
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
    /// The relay-hold fleet-sync engine (durable flush + publish).
    pub relay_hold_engine: Arc<FleetSyncEngine<RelayHoldDoc>>,
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
        self.relay_hold_engine.notify_dirty();
        // Durable persist + publish BEFORE the ack exists (D7). Flushed even
        // for a Duplicate key: if the FIRST deposit's flush failed after the
        // in-memory insert, the retry hits the occupied entry — skipping the
        // flush here would ack an entry that was never made durable.
        self.relay_hold_engine
            .flush_now()
            .await
            .map_err(|e| format!("flush_now: {e}"))?;
        // No local ingest sweeper to nudge: a relay is never the recipient of
        // the blobs it holds (unlike the butler, which is itself a recipient
        // device).
        Ok(verdict)
    }
}

// =====================================================================
// ProdRelayPullCtx
// =====================================================================

/// Production [`RelayPullCtx`]: serve/membership gates via the opt-in doc + the
/// membership seam, a recipient-scoped `held_for`, and an ack handler that
/// unions `pulled_by`, GCs (the doc's built-in one-sweep deferral keeps a
/// just-acked entry one extra sweep so `pulled_by` replicates first), and
/// durably flushes.
pub struct ProdRelayPullCtx {
    /// This relay owner's address bytes, checked in `serves_community`.
    pub self_owner: [u8; 16],
    /// Runtime relay-hold CRDT (same Arc the engine owns).
    pub relay_hold_doc: Arc<tokio::sync::Mutex<RelayHoldDoc>>,
    /// The relay-hold fleet-sync engine (durable flush + publish).
    pub relay_hold_engine: Arc<FleetSyncEngine<RelayHoldDoc>>,
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
        // keys = no-op, so an ack for an already-GC'd blob never errors), then
        // run GC under the same lock. The doc's gc() snapshots covered_at_start
        // BEFORE mutating, so an entry that becomes covered DURING this sweep
        // survives one extra gc() call — giving pulled_by time to replicate to
        // sibling relays before any replica destroys the entry (one-sweep
        // deferral, built into RelayHoldDoc::gc).
        {
            let mut doc = self.relay_hold_doc.lock().await;
            for k in keys {
                if let Some(e) = doc.entries.get_mut(k) {
                    e.pulled_by.insert(requester_device.clone());
                }
            }
            let now_ms = now_epoch_ms();
            doc.gc(now_ms);
        }
        // Durable persist + publish of the pulled_by union (+ any GC removal).
        // notify_dirty BEFORE flush_now (publish-retry latch, as in deposit).
        self.relay_hold_engine.notify_dirty();
        self.relay_hold_engine
            .flush_now()
            .await
            .map_err(|e| format!("flush_now: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    fn space(b: u8) -> SpaceId {
        SpaceId([b; 16])
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

    // A `FleetSyncEngine` is NOT cheap to construct for a unit test (it owns a
    // background task + transport channels + a typed CAS/merger/persist
    // config), and its persist/flush behavior is integration-tested exactly
    // like `ProdButlerDepositCtx::persist_entry`. These unit tests therefore
    // cover ONLY the engine-free admission/projection logic; the
    // caps / persist / GC durability paths are covered end-to-end by the T12
    // integration test.
    //
    // `serves_community`, `both_co_members`, `is_joined_member`, and `held_for`
    // read ONLY `optin`, `membership`, and `relay_hold_doc` — never the engine
    // — so the helpers below mirror those method bodies 1:1 over the same
    // inputs, exercising the gate logic without instantiating a real engine.
    // The `_assert_impls` anchor pins that the prod structs satisfy the Phase A
    // trait bounds.
    async fn serves(
        optin: &Arc<tokio::sync::Mutex<RelayOptInDoc>>,
        membership: &Arc<dyn CommunityMembershipLookup>,
        self_owner: &[u8; 16],
        community_id: &SpaceId,
    ) -> bool {
        optin.lock().await.is_opted_in(community_id)
            && membership.is_joined(community_id, self_owner).await
    }

    async fn both_co(
        membership: &Arc<dyn CommunityMembershipLookup>,
        community_id: &SpaceId,
        sender: &[u8; 16],
        recipient: &[u8; 16],
    ) -> bool {
        membership.is_joined(community_id, sender).await
            && membership.is_joined(community_id, recipient).await
    }

    fn held_for_doc(
        doc: &RelayHoldDoc,
        recipient_owner: &[u8; 16],
    ) -> Vec<(String, RelayHeldBlob)> {
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
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn serves_community_true_when_opted_in_and_self_joined() {
        let c = space(0xCC);
        let self_owner = [0x11; 16];
        let optin = optin_doc(c, true);
        let membership = fake(&[(c, self_owner)]);
        assert!(serves(&optin, &membership, &self_owner, &c).await);
    }

    #[tokio::test]
    async fn serves_community_false_when_not_opted_in() {
        let c = space(0xCC);
        let self_owner = [0x11; 16];
        let optin = optin_doc(c, false); // NOT opted in
        let membership = fake(&[(c, self_owner)]); // but IS a member
        assert!(!serves(&optin, &membership, &self_owner, &c).await);
    }

    #[tokio::test]
    async fn serves_community_false_when_self_not_joined() {
        let c = space(0xCC);
        let self_owner = [0x11; 16];
        let optin = optin_doc(c, true); // opted in
        let membership = fake(&[]); // but NOT a member
        assert!(!serves(&optin, &membership, &self_owner, &c).await);
    }

    // ---------------------------------------------------------------
    // both_co_members / is_joined_member reflect the fake
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn both_co_members_requires_both_joined() {
        let c = space(0xCC);
        let sender = [0x22; 16];
        let recipient = [0x33; 16];

        // both joined → true
        let m = fake(&[(c, sender), (c, recipient)]);
        assert!(both_co(&m, &c, &sender, &recipient).await);

        // only sender joined → false
        let m = fake(&[(c, sender)]);
        assert!(!both_co(&m, &c, &sender, &recipient).await);

        // only recipient joined → false
        let m = fake(&[(c, recipient)]);
        assert!(!both_co(&m, &c, &sender, &recipient).await);

        // neither joined → false
        let m = fake(&[]);
        assert!(!both_co(&m, &c, &sender, &recipient).await);
    }

    #[tokio::test]
    async fn is_joined_member_reflects_fake() {
        let c = space(0xCC);
        let owner = [0x44; 16];
        let m = fake(&[(c, owner)]);
        assert!(m.is_joined(&c, &owner).await);
        assert!(!m.is_joined(&c, &[0x45; 16]).await);
        assert!(!m.is_joined(&space(0xDD), &owner).await);
    }

    // ---------------------------------------------------------------
    // held_for: exactly the recipient's entries, excludes others'
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

        let mut got = held_for_doc(&doc, &recipient);
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
        let got = held_for_doc(&doc, &[0xAA; 16]);
        assert!(got.is_empty());
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
