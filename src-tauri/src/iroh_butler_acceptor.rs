//! ZEB-418 SP2 Phase 1 Task 5: inbound butler-deposit acceptor for the
//! `harmony/butler-deposit/v1` ALPN.
//!
//! A sender that cannot reach the recipient's active device directly deposits
//! a sealed DM with an online sibling device of the recipient (the "butler").
//! This module is the butler side: [`handle_deposit_core`] runs the spec §4
//! verification pipeline (admission BEFORE any decryption, persist BEFORE
//! ack — D7), and [`IrohButlerDepositAcceptor`] is the thin iroh shell around
//! it (length-prefixed frame in, length-prefixed ack out).
//!
//! See `docs/specs/2026-06-09-zeb-418-sp2-butler-design.md` §4 (deposit
//! protocol — the verification ORDER there is normative) and §5 (`dm-inbox-v1`
//! dataset).
//!
//! ## Verification order (spec §4 as amended PR #221 round 1, plan Task 5)
//!
//! 0. recipient bind: `frame.recipient_owner` must be THIS owner;
//! 1. admission: `frame.sender_owner` must be an `Active` friend (yields the
//!    friend's pinned `master_ed25519`);
//! 2. decode + verify the sender's `EnrollmentCert` against that master key,
//!    extract the cert-bound device verify key;
//! 3. verify `frame.sig` over `BUTLER_DEPOSIT_SIG_DOMAIN ‖ ro ‖ sealed_blob`
//!    against that device key;
//! 4. decrypt `sealed_blob` (the FIRST crypto-on-content step — steps 0–3 run
//!    before any decryption, so the butler never decrypts unauthenticated
//!    bytes);
//! 5. decode the inner [`crate::butler_deposit::DepositPayload`]; resolve the
//!    signing DEVICE to its owner and require it to equal
//!    `frame.sender_owner` (device→owner binding — the same binding the
//!    normal receive path enforces); verify the CidNotify packet signature +
//!    sender/space/CID consistency with the storage blob;
//! 6. atomic persist-with-caps into [`DmInboxDoc`] (per-sender + global
//!    quotas enforced INSIDE the persist critical section; an
//!    already-present key bypasses the caps — idempotent redelivery; an ack
//!    never lies — D7);
//! 7. ack.
//!
//! Caps moved from a standalone pre-decrypt step into the persist critical
//! section (PR #221 round 1): a snapshot-then-insert quota raced under
//! concurrent connections, and a redelivery of an already-stored entry at a
//! full inbox was wrongly rejected instead of idempotently re-acked.
//! Idempotent redelivery + atomicity beat the old caps-before-crypto
//! ordering; steps 1–3 still gate all content crypto behind cert+sig.
//!
//! ## Tauri-free core
//!
//! [`handle_deposit_core`] takes everything it needs through the injectable
//! [`ButlerDepositCtx`] trait (mirroring how `notes_commands` separates pure
//! cores from Tauri shells), so the unit tests can probe CALL ORDER — e.g.
//! that a non-friend deposit never reaches the decrypt step, and that the
//! persist sink completes before the ack value exists.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::{Signature, VerifyingKey};
use harmony_content::cid::{ContentFlags, ContentId};
use harmony_owner::certs::EnrollmentCert;
use iroh::endpoint::Connection;

use crate::butler_deposit::{
    decode_deposit_frame, decode_deposit_payload, deposit_sig_payload, encode_deposit_ack,
    read_length_prefixed, write_length_prefixed, DepositAck, DepositFrame, INBOX_GLOBAL_CAP,
    INBOX_PER_SENDER_CAP,
};
use crate::dm_envelope::{decode_packet, DmPacket};
use crate::dm_inbox_crdt::{DmInboxDoc, DmInboxEntry};
use crate::dm_signing::verify_dm_packet_signature;
use crate::friend_graph::FriendStatus;
use crate::owner_state_types::{DeviceIdentityHash, Hlc};

/// Why a deposit was rejected. Spec §4: the wire NEVER carries a detailed
/// error back to the sender (no oracle for probing the friend graph) — the
/// shell closes the stream uniformly on any reject and this enum is for
/// local logging/counters/tests only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DepositReject {
    /// `frame.recipient_owner` is not this device's owner.
    #[error("deposit addressed to a different recipient owner")]
    WrongRecipient,
    /// `frame.sender_owner` is neither an `Active` friend nor a live
    /// group-DM co-member (ZEB-424). All non-authorized senders collapse
    /// here — the wire close is uniform, so this distinction is only for
    /// counters/tests. (Formerly `NotFriend`.)
    #[error("sender is not authorized to deposit (not an active friend or co-member)")]
    NotAuthorized,
    /// The sender WAS admitted (active friend or live group-DM co-member) but
    /// failed a narrower post-admission scope check: a co-member deposit
    /// bound to a space they don't live-share (message/invite paths), or a
    /// co-member attempting a friend-scoped revocation deposit. Split from
    /// [`NotAuthorized`] (ZEB-702, PR #481 review): only a ROSTER miss is the
    /// roster-sync signal — counting these as `rejected_unauthorized` would
    /// fire the ZEB-702 WARN for senders the roster already admitted. The
    /// wire close is uniform with every other reject (no oracle).
    #[error("sender is not authorized for this deposit's scope")]
    NotAuthorizedForScope,
    /// The embedded `EnrollmentCert` failed to decode, failed verification,
    /// is not Master-issued, has `owner_id != frame.sender_owner`, or its
    /// issuing master fails the admission-path master binding: the friend
    /// path pins the cert master byte-for-byte against the friend-graph's
    /// stored master; the co-member path (ZEB-424 D29.1) requires the
    /// owner-id-derived anchor `owner_id_from_master_ed25519(master) ==
    /// sender_owner`.
    #[error("sender enrollment cert invalid")]
    BadCert,
    /// `frame.sig` is malformed or does not verify over
    /// `BUTLER_DEPOSIT_SIG_DOMAIN ‖ ro ‖ sealed_blob` against the cert-bound
    /// device key.
    #[error("deposit frame signature invalid")]
    BadSig,
    /// Inserting a NEW inbox key would exceed [`INBOX_PER_SENDER_CAP`] or
    /// [`INBOX_GLOBAL_CAP`]. Enforced atomically inside `persist_entry`'s
    /// critical section; a redelivery of an already-stored key is exempt
    /// (it re-acks idempotently even at a full inbox).
    #[error("inbox cap exceeded")]
    CapExceeded,
    /// `sealed_blob` did not open under this device's X25519 key + the
    /// butler-deposit HKDF info string.
    #[error("sealed blob decryption failed")]
    DecryptFailed,
    /// The decrypted plaintext is not a valid
    /// [`crate::butler_deposit::DepositPayload`] / CidNotify packet
    /// (codec/shape failures).
    #[error("deposit payload malformed")]
    BadPayload,
    /// The inner CidNotify packet failed verification: signature,
    /// device→owner binding (signing device unknown, ambiguous, or owned by
    /// a different owner than `frame.sender_owner`), sender consistency, or
    /// space/CID consistency with the storage blob.
    #[error("inner CidNotify verification failed")]
    InnerVerifyFailed,
    /// The dm-inbox write or its durable flush failed — NO ack may be
    /// produced (an ack never lies, D7). The sender retries; the redelivery
    /// is absorbed by the insert-once key dedupe.
    #[error("dm-inbox persist failed: {0}")]
    PersistFailed(String),
}

/// Outcome of the atomic persist step. Cap enforcement lives INSIDE the
/// doc-lock critical section (snapshot-then-insert raced under concurrent
/// connections), and an occupied key bypasses the caps entirely — a
/// redelivery after a lost ack must re-ack idempotently even at a full
/// inbox (the entry is already stored; rejecting it would strand the
/// sender undelivered for a message the butler holds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepositPersistVerdict {
    Inserted,
    /// Key already present — flushed again (D7: a redelivery after a failed
    /// first flush must not ack non-durable state) and acked.
    Duplicate,
    /// Inserting a NEW key would exceed INBOX_PER_SENDER_CAP or
    /// INBOX_GLOBAL_CAP. Nothing inserted, nothing flushed.
    CapExceeded,
}

/// Injectable context for [`handle_deposit_core`]: friend lookup, decrypt,
/// sender-device resolution, and the persist sink (which also enforces the
/// inbox caps atomically). Production (Task 7) implements this over
/// `NodeState`'s owner-state + dm-inbox engine handles; tests implement it
/// with probes that record call order.
#[async_trait]
pub trait ButlerDepositCtx: Send + Sync {
    /// This device's owner address bytes (the only recipient we accept for).
    fn self_owner(&self) -> [u8; 16];

    /// Step 1 admission lookup: `Some((master_ed25519, status))` when
    /// `sender_owner` is in the friend graph. Production reads
    /// `OwnerState.friend_graph` under the CRDT lock.
    async fn lookup_friend(&self, sender_owner: &[u8; 16]) -> Option<([u8; 32], FriendStatus)>;

    /// ZEB-424 (D27): admission fallback when `lookup_friend` is not
    /// Active — `true` iff `sender_owner` shares a live `GroupDm` space
    /// with this owner. Production reads `OwnerState.spaces` under the
    /// CRDT lock via [`shares_live_group_dm_in`].
    async fn shares_live_group_dm(&self, sender_owner: &[u8; 16]) -> bool;

    /// ZEB-424 (D28.1, security follow-up): post-decrypt authoritative bind
    /// for the co-member admission path — `true` iff `space_id` is a live
    /// `GroupDm` in `OwnerState.spaces` whose members contain BOTH this owner
    /// and `sender_owner`. Step 1's [`shares_live_group_dm`] can only prove
    /// the sender shares SOME live group DM (the deposit's `space_id` is
    /// sealed until decrypt), so the co-member path binds the deposit's
    /// ACTUAL space here, before persist/ack — a co-member of one group must
    /// not get a deposit for an unrelated space persisted+acked (it would be
    /// un-ingestible and pin an inbox slot until TTL). Production reads
    /// `OwnerState.spaces` under the CRDT lock via
    /// [`space_is_live_group_dm_co_member_in`].
    async fn space_live_group_dm_co_member(
        &self,
        space_id: &[u8; 16],
        sender_owner: &[u8; 16],
    ) -> bool;

    /// Wall-clock now in epoch-SECONDS for `EnrollmentCert` expiry checks
    /// (cert timestamps are Unix seconds — ZEB-378).
    fn now_secs(&self) -> u64;

    /// Step 4: open the sealed blob. Production:
    /// `open_from_owner_with_info(ed25519_priv_to_x25519(own_device_sk),
    /// sealed, BUTLER_DEPOSIT_SEAL_INFO)`. MUST NOT be reached for a deposit
    /// that failed steps 0–3 (the tests probe exactly this).
    fn decrypt(&self, sealed_blob: &[u8]) -> Result<Vec<u8>, String>;

    /// Resolve the sender DEVICE to its (owner id, identity pub) from the SAME
    /// snapshot the normal receive path uses (`owner_device_cache`:
    /// `resolve_signed_origin_owner` + `lookup_pubkey_for_device`, one lock).
    /// `None` = unknown device OR ambiguous owner — reject; a deposit the
    /// normal path would refuse must never be persisted+acked (the ack would
    /// lie: ingestion reuses the normal path and would reject it until TTL).
    async fn resolve_sender_device(
        &self,
        device_hash: DeviceIdentityHash,
    ) -> Option<([u8; 16], [u8; 64])>;

    /// This device's SP1 device id (64-hex), stamped as `deposited_by`.
    fn device_id(&self) -> String;

    /// Mint a fresh monotone HLC for `deposited_at`.
    async fn mint_hlc(&self) -> Hlc;

    /// Step 6: atomic persist-with-caps. Under ONE doc-lock critical
    /// section: an occupied `key` is left untouched (insert-once — see
    /// [`DepositPersistVerdict::Duplicate`]); a vacant `key` is admitted
    /// only if inserting it keeps the sender under
    /// [`INBOX_PER_SENDER_CAP`] and the doc under [`INBOX_GLOBAL_CAP`]
    /// (else [`DepositPersistVerdict::CapExceeded`] — nothing inserted,
    /// nothing flushed). On `Inserted`/`Duplicate` the doc is durably
    /// flushed (`engine.flush_now().await`) BEFORE returning — the ack is
    /// only produced after this resolves (D7). `Err` = nothing durable may
    /// be assumed — the caller rejects without acking.
    async fn persist_entry(
        &self,
        key: String,
        entry: DmInboxEntry,
    ) -> Result<DepositPersistVerdict, String>;
}

/// Production [`ButlerDepositCtx`] over real `start_node` handles (ZEB-418
/// P1 Task 7). Each method implements its trait doc's production contract
/// verbatim:
///
/// * admission reads `OwnerState.friend_graph` under the CRDT lock;
/// * decrypt = `open_from_owner_with_info` with this device's X25519
///   private (the birational twin of the cert-bound device ed25519 key —
///   the SAME key the butler-set advertisement publishes as `vk`, so a
///   sender sealing to `birational(vk)` lands here);
/// * `resolve_sender_device` sources owner + identity pub from
///   `owner_device_cache` via `dm_outbox::resolve_signed_origin_owner` +
///   `lookup_pubkey_for_device` under ONE lock — the SAME trust source and
///   the SAME resolution the normal receive path's
///   `verify_cidnotify_admission` uses (a deposit from a never-seen,
///   ambiguous, or differently-owned sender device rejects at inner-verify,
///   exactly as the normal path would drop it);
/// * `mint_hlc` mints from the dm-inbox engine's HLC tracker, mirroring
///   how the notes IPC cores mint from the notes tracker;
/// * `persist_entry` = atomic persist-with-caps under the doc lock
///   (occupied key → caps bypassed, idempotent redelivery; vacant key →
///   per-sender + global LIVE-entry quotas enforced in the same critical
///   section — counting live doc entries keeps the quota a real bound on
///   butler storage rather than a flood-resettable counter), then
///   `notify_dirty` + `flush_now().await` (durable publish + persist)
///   BEFORE returning — persist-before-ack, D7 — then a best-effort nudge
///   of the local ingest sweeper (the butler is itself a recipient device:
///   without the nudge its own UI delivery would wait for a sibling's
///   ig-ack merge, which never comes when the rest of the fleet stays
///   offline). A `CapExceeded` verdict inserts and flushes NOTHING.
pub struct ProdButlerDepositCtx {
    /// This owner's address bytes (`OwnerAddr.0`).
    pub self_owner: [u8; 16],
    /// This device's SP1 device id (64-hex of the device ed25519 verify key).
    pub device_id: String,
    /// The runtime owner-state CRDT (`NodeState`'s `crdt_state` Arc) —
    /// friend graph + owner device cache live here.
    pub crdt_state: Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    /// X25519 private scalar derived once at start_node via
    /// `dm_signing::ed25519_priv_to_x25519(device_signing_key)`.
    pub device_x25519_priv: zeroize::Zeroizing<[u8; 32]>,
    /// The dm-inbox dataset handles (same Arcs the engine owns).
    pub dm_inbox_doc: Arc<tokio::sync::Mutex<DmInboxDoc>>,
    pub dm_inbox_tracker:
        Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>>,
    pub dm_inbox_engine: Arc<crate::fleet_sync::FleetSyncEngine<DmInboxDoc>>,
    /// Weak nudge sender into the dm-inbox ingest sweeper. Weak so the
    /// acceptor (whose lifetime is tied to the iroh link manager, not the
    /// engine) can never keep the sweeper alive past engine shutdown —
    /// the engine's `on_applied` closure holds the only strong sender.
    pub ingest_nudge: tokio::sync::mpsc::WeakSender<()>,
}

#[async_trait]
impl ButlerDepositCtx for ProdButlerDepositCtx {
    fn self_owner(&self) -> [u8; 16] {
        self.self_owner
    }

    async fn lookup_friend(&self, sender_owner: &[u8; 16]) -> Option<([u8; 32], FriendStatus)> {
        let state = self.crdt_state.lock().await;
        state
            .friend_graph
            .friends
            .get(&crate::owner_state_types::OwnerAddr(*sender_owner))
            .map(|e| (e.master_ed25519, e.status))
    }

    async fn shares_live_group_dm(&self, sender_owner: &[u8; 16]) -> bool {
        let state = self.crdt_state.lock().await;
        shares_live_group_dm_in(&state, &self.self_owner, sender_owner)
    }

    async fn space_live_group_dm_co_member(
        &self,
        space_id: &[u8; 16],
        sender_owner: &[u8; 16],
    ) -> bool {
        let state = self.crdt_state.lock().await;
        space_is_live_group_dm_co_member_in(&state, &self.self_owner, sender_owner, space_id)
    }

    fn now_secs(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn decrypt(&self, sealed_blob: &[u8]) -> Result<Vec<u8>, String> {
        crate::dm_signing::open_from_owner_with_info(
            &self.device_x25519_priv,
            sealed_blob,
            crate::butler_deposit::BUTLER_DEPOSIT_SEAL_INFO,
        )
        .map_err(|e| format!("{e:?}"))
    }

    async fn resolve_sender_device(
        &self,
        device_hash: DeviceIdentityHash,
    ) -> Option<([u8; 16], [u8; 64])> {
        // ONE lock acquisition so the owner resolution and the pub lookup
        // read the SAME `owner_device_cache` snapshot — mirroring the
        // normal receive path's `verify_cidnotify_admission`, which calls
        // these two primitives under its single OwnerState borrow.
        let state = self.crdt_state.lock().await;
        // Unknown AND Ambiguous both collapse to None: a multi-owner match
        // (corrupted state or cache-poisoning) is not a trustworthy
        // resolution, exactly as the normal path drops it.
        let owner =
            crate::dm_outbox::resolve_signed_origin_owner(&state.owner_device_cache, device_hash)
                .ok()?;
        let identity_pub =
            crate::dm_outbox::lookup_pubkey_for_device(&state.owner_device_cache, device_hash)?;
        Some((owner.0, identity_pub))
    }

    fn device_id(&self) -> String {
        self.device_id.clone()
    }

    async fn mint_hlc(&self) -> Hlc {
        crate::fleet_sync::mint_next_hlc(&self.dm_inbox_tracker, &self.device_id).await
    }

    async fn persist_entry(
        &self,
        key: String,
        entry: DmInboxEntry,
    ) -> Result<DepositPersistVerdict, String> {
        let verdict = {
            let mut doc = self.dm_inbox_doc.lock().await;
            if doc.entries.contains_key(&key) {
                // Occupied key: insert-once leaves it untouched, and the
                // caps are BYPASSED — the entry is already stored, so a
                // redelivery after a lost ack re-acks idempotently even at
                // a full inbox. Falls through to the flush below (D7).
                //
                // ZEB-483 (CodeAnt): ONE exception to insert-once — heal a
                // stored entry that lacks the bootstrap invite. A pre-ZEB-483
                // entry (or any deposit that landed before the sender attached
                // the invite) carries `invite_packet: None`; a later redelivery
                // that DOES carry the invite must upgrade it, or recovery stays
                // un-bootstrappable (`SpaceNotFound`) forever. Promote `None →
                // Some` only (never overwrite an existing invite); the flush
                // below makes the upgrade durable + republishes it to the fleet.
                if entry.invite_packet.is_some() {
                    if let Some(stored) = doc.entries.get_mut(&key) {
                        if stored.invite_packet.is_none() {
                            stored.invite_packet = entry.invite_packet.clone();
                        }
                    }
                }
                DepositPersistVerdict::Duplicate
            } else {
                // Caps INSIDE the doc-lock critical section: counting and
                // inserting under one lock acquisition means concurrent
                // deposits can never overshoot the quotas (the old
                // snapshot-then-insert raced). "Live" = every entry in the
                // doc — the ingest sweep's GC removes entries once
                // `ingested_by` covers the enrolled device set (one sweep
                // after the final ig ack lands) or the TTL expires, so doc
                // occupancy keeps the quota a real bound on butler storage
                // rather than a flood-resettable counter.
                let sender_pending = doc
                    .entries
                    .values()
                    .filter(|e| e.sender_owner == entry.sender_owner)
                    .count();
                let total = doc.entries.len();
                if sender_pending >= INBOX_PER_SENDER_CAP || total >= INBOX_GLOBAL_CAP {
                    // Nothing inserted → nothing to flush; the doc is
                    // exactly as it was.
                    return Ok(DepositPersistVerdict::CapExceeded);
                }
                doc.entries.insert(key, entry);
                DepositPersistVerdict::Inserted
            }
        };
        // notify_dirty BEFORE flush_now: if the flush's publish leg fails,
        // the engine's swap-and-restore keeps the dirty latch armed so a
        // later debounce retries the publish (fleet_sync.rs flush arm).
        self.dm_inbox_engine.notify_dirty();
        // Durable persist + publish BEFORE the ack exists (D7). Flushed
        // even for a duplicate key: if the FIRST deposit's flush failed
        // after the in-memory insert, the retry hits the occupied entry —
        // skipping the flush here would ack an entry that was never made
        // durable.
        self.dm_inbox_engine
            .flush_now()
            .await
            .map_err(|e| format!("flush_now: {e}"))?;
        // Wake the local ingest sweeper (level trigger; full buffer = a
        // sweep is already scheduled; None = engine already shut down).
        if let Some(tx) = self.ingest_nudge.upgrade() {
            let _ = tx.try_send(());
        }
        Ok(verdict)
    }
}

/// Strict canonical-CBOR decode of the embedded [`EnrollmentCert`] (trailing
/// bytes rejected — mirrors `iroh_friend_acceptor::decode_strict`).
fn decode_enrollment_cert_strict(bytes: &[u8]) -> Result<EnrollmentCert, DepositReject> {
    let mut cursor = std::io::Cursor::new(bytes);
    let cert: EnrollmentCert =
        ciborium::from_reader(&mut cursor).map_err(|_| DepositReject::BadCert)?;
    if cursor.position() as usize != bytes.len() {
        return Err(DepositReject::BadCert);
    }
    Ok(cert)
}

/// ZEB-677: strict decode of the frame's `signer_certs_cbor` bundle (canonical
/// CBOR of `Vec<EnrollmentCert>`; empty bytes ⇒ empty bundle — the wire omits
/// the key for Master-issued certs). Same trailing-byte discipline as
/// [`decode_enrollment_cert_strict`].
fn decode_signer_certs_strict(bytes: &[u8]) -> Result<Vec<EnrollmentCert>, DepositReject> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut cursor = std::io::Cursor::new(bytes);
    let certs: Vec<EnrollmentCert> =
        ciborium::from_reader(&mut cursor).map_err(|_| DepositReject::BadCert)?;
    if cursor.position() as usize != bytes.len() {
        return Err(DepositReject::BadCert);
    }
    Ok(certs)
}

/// ZEB-424 (D27): does the butler share a LIVE group-DM space with
/// `sender_owner`? Pure scan over the replicated `OwnerState.spaces` — the
/// same state step-1 admission already reads the friend graph from. A match
/// requires a `GroupDm` space that has not been left, with BOTH this owner
/// and the sender in `members`. Spaces count is small (tens), so a linear
/// scan needs no index (a derived index would add CRDT-merge invalidation
/// hazards for zero measured win).
pub(crate) fn shares_live_group_dm_in(
    state: &crate::owner_state_crdt::OwnerState,
    self_owner: &[u8; 16],
    sender_owner: &[u8; 16],
) -> bool {
    use crate::owner_state_types::{OwnerAddr, SpaceKind};
    let self_addr = OwnerAddr(*self_owner);
    let sender_addr = OwnerAddr(*sender_owner);
    state.spaces.values().any(|s| {
        s.kind == SpaceKind::GroupDm
            && s.left_at.is_none()
            && s.members.contains(&self_addr)
            && s.members.contains(&sender_addr)
    })
}

/// ZEB-424 (D28.1, security follow-up): is the SPECIFIC space `space_id` a
/// LIVE `GroupDm` whose `members` contain BOTH `self_owner` and
/// `sender_owner`? The post-decrypt counterpart to [`shares_live_group_dm_in`]:
/// pre-decrypt admission only proves the sender shares SOME live group DM (the
/// deposit's `space_id` is sealed until decrypt), so the co-member path binds
/// the deposit's own `space_id` to membership here, before persist/ack. This
/// matches the receive-path / ingestion check
/// (`dm_outbox::verify_cidnotify_admission`), so a co-member can never get a
/// deposit for a space they are not a live member of persisted+acked.
pub(crate) fn space_is_live_group_dm_co_member_in(
    state: &crate::owner_state_crdt::OwnerState,
    self_owner: &[u8; 16],
    sender_owner: &[u8; 16],
    space_id: &[u8; 16],
) -> bool {
    use crate::owner_state_types::{OwnerAddr, SpaceId, SpaceKind};
    let self_addr = OwnerAddr(*self_owner);
    let sender_addr = OwnerAddr(*sender_owner);
    state.spaces.get(&SpaceId(*space_id)).is_some_and(|s| {
        s.kind == SpaceKind::GroupDm
            && s.left_at.is_none()
            && s.members.contains(&self_addr)
            && s.members.contains(&sender_addr)
    })
}

/// The Tauri-free deposit pipeline (spec §4 order — see the module docs).
/// Returns the ack to write on success; any reject means the shell closes
/// the stream without detail.
pub async fn handle_deposit_core(
    frame: &DepositFrame,
    ctx: &dyn ButlerDepositCtx,
) -> Result<DepositAck, DepositReject> {
    // Step 0 — recipient bind: this deposit must be for THIS owner's inbox.
    // Cheapest local check, before any lookup or crypto.
    if frame.recipient_owner != ctx.self_owner() {
        return Err(DepositReject::WrongRecipient);
    }

    // Step 1 — admission (spec §4 D5 as amended by ZEB-424 D27/D29.1): the
    // sender must be either an Active friend (pinned-master trust) OR a live
    // group-DM co-member (owner-id-derived trust). Friend status is checked
    // first; a non-Active result (Pending/Revoked/None) falls through to the
    // co-membership check — group membership is independent of friend status.
    #[derive(Clone, Copy)]
    enum Admission {
        /// Active friend: step 2 pins the cert master against this stored key.
        Friend([u8; 32]),
        /// Live group-DM co-member: step 2 derives the anchor from the owner id,
        /// and step 5.5 binds the deposit's own space to membership.
        CoMember,
    }
    let admission = match ctx.lookup_friend(&frame.sender_owner).await {
        Some((friend_master, FriendStatus::Active)) => Admission::Friend(friend_master),
        _ => {
            if ctx.shares_live_group_dm(&frame.sender_owner).await {
                Admission::CoMember
            } else {
                return Err(DepositReject::NotAuthorized);
            }
        }
    };

    // Step 2 — decode + verify the sender device's EnrollmentCert and bind
    // its issuing master to the admitted identity: internally valid (Master
    // self-contained, or Quorum against the presented signer bundle,
    // ZEB-677), owner id == frame.sender_owner, and the master anchor
    // satisfies the admission-path binding below (friend path → byte-for-byte
    // pin against the friend graph's stored master; co-member path → the
    // owner-id-derived anchor, D29.1 — both are the trust anchor for their
    // path).
    let cert = decode_enrollment_cert_strict(&frame.sender_enrollment_cert)?;
    let signer_certs = decode_signer_certs_strict(&frame.signer_certs_cbor)?;
    // ZEB-677: chokepoint verification — Master certs self-contained; Quorum
    // certs against the frame's signer-cert bundle (depth-1). Returns the
    // enrolled device key + the master anchor (from the bundle for quorum).
    let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
        &cert,
        &signer_certs,
        Some(&frame.sender_owner),
        ctx.now_secs(),
    )
    .map_err(|_| DepositReject::BadCert)?;
    let cert_master = verified.master_ed25519;
    // Master binding (D29.1): the friend path keeps its byte-for-byte pin
    // against the stored master; the co-member path derives the anchor from
    // the owner id (the owner id IS the hash of the master bundle —
    // `owner_id_from_master_ed25519`; the invariant
    // `iroh_friend_acceptor::verified_master_anchor_matches_owner_id` pins it).
    //
    // Both branches are defense-in-depth: the chokepoint verification above
    // already rejects `hash(master) != owner_id` (per signer cert for the
    // quorum path), and binds `cert.owner_id == sender_owner`, so any cert
    // reaching here necessarily satisfies the derived check. We keep an
    // EXPLICIT per-variant pin anyway — the friend branch as the
    // long-standing trust anchor, the co-member branch to make that anchor
    // self-evident and resilient if the internal owner_id↔master binding
    // ever moves. Neither is expected to be a live rejection path for a
    // well-formed cert; that's why a forged-master unit test is
    // intentionally omitted (such a cert can't pass verification).
    match admission {
        Admission::Friend(friend_master) => {
            if cert_master != friend_master {
                return Err(DepositReject::BadCert);
            }
        }
        Admission::CoMember => {
            if crate::friend_graph::owner_id_from_master_ed25519(&cert_master)
                != crate::owner_state_types::OwnerAddr(frame.sender_owner)
            {
                return Err(DepositReject::BadCert);
            }
        }
    }
    let device_vk_bytes = verified.device_ed25519;

    // Step 3 — verify the frame signature over
    // `BUTLER_DEPOSIT_SIG_DOMAIN ‖ recipient_owner ‖ sealed_blob` against
    // the cert-bound device key.
    let sig_bytes: [u8; 64] = frame
        .sig
        .as_slice()
        .try_into()
        .map_err(|_| DepositReject::BadSig)?;
    let device_vk =
        VerifyingKey::from_bytes(&device_vk_bytes).map_err(|_| DepositReject::BadCert)?;
    device_vk
        .verify_strict(
            &deposit_sig_payload(&frame.recipient_owner, &frame.sealed_blob),
            &Signature::from_bytes(&sig_bytes),
        )
        .map_err(|_| DepositReject::BadSig)?;

    // Step 4 — decrypt. The FIRST crypto-on-content step: everything above
    // ran on authenticated-but-sealed bytes.
    let plaintext = ctx
        .decrypt(&frame.sealed_blob)
        .map_err(|_| DepositReject::DecryptFailed)?;

    // Step 5 — decode the payload and verify the inner CidNotify with the
    // existing receive-path primitives: device→owner binding (the signing
    // DEVICE must resolve to exactly `frame.sender_owner` — the same
    // binding the normal receive path enforces via
    // `resolve_signed_origin_owner` + the owner-field match; without it
    // the butler would persist+ack a deposit that ingestion, which reuses
    // the normal path, rejects forever — the ack would lie, D7), packet
    // signature, sender consistency with the admission-checked frame
    // sender, and space/CID consistency (the deposited storage blob must
    // hash to the packet's message_cid under the DM send path's exact
    // for_book flags).
    let payload = decode_deposit_payload(&plaintext).map_err(|_| DepositReject::BadPayload)?;
    // ZEB-483 — size-bound the piggybacked DmInvite before any further work. The
    // butler treats it opaquely (sealed end-to-end, applied+verified on recover);
    // this cap bars a malicious sender from inflating butler storage via the
    // invite field. The CidNotify + blob validation below is unchanged.
    if let Some(inv) = payload.invite_packet.as_ref() {
        if inv.len() > crate::butler_deposit::MAX_DEPOSIT_INVITE_BYTES {
            return Err(DepositReject::BadPayload);
        }
    }
    // ZEB-505: a deposit is either a MESSAGE (Some CidNotify + storage blob) or
    // a standalone durable INVITE (None CidNotify — the invite IS the payload).
    // Verify whichever half is present with the same receive-path primitives —
    // device→owner binding + packet signature + the co-member space bind — then
    // hand back the persist key + the ack identifier. The invite-only branch has
    // no storage blob, so it skips the `for_book` cid check (there is no cid).
    let (deposit_space_id, key, ack_message_cid): ([u8; 16], String, Vec<u8>) = match payload
        .cidnotify_packet
        .as_deref()
    {
        Some(cidnotify_bytes) => {
            // CodeRabbit: a message deposit carries no revocation; reject
            // stray bytes fail-closed, mirroring the storage_blob/invite
            // waste guards in the other two arms.
            if payload.revocation_push.is_some() {
                return Err(DepositReject::BadPayload);
            }
            // ZEB-674 (C4): a file-share grant is a PURE deposit shape (its own
            // `grant_key`d entry). Reject a `grant_push` riding alongside a
            // message fail-closed — the recipient's grant dispatch guard
            // (`grant_push.is_some() && cidnotify.is_none() && ..`) would
            // otherwise fail to match and silently drop the grant (same
            // pure-shape reasoning the ZEB-691 revocation guards enforce).
            if payload.grant_push.is_some() {
                return Err(DepositReject::BadPayload);
            }
            let packet = decode_packet(cidnotify_bytes).map_err(|_| DepositReject::BadPayload)?;
            let (signed, signature, signed_bytes) = match packet {
                DmPacket::CidNotify {
                    signed,
                    signature,
                    signed_bytes,
                } => (signed, signature, signed_bytes),
                _ => return Err(DepositReject::BadPayload),
            };
            if signed.sender_owner_addr.0 != frame.sender_owner {
                return Err(DepositReject::InnerVerifyFailed);
            }
            let (resolved_owner, identity_pub) = ctx
                .resolve_sender_device(signed.signing_device_hash)
                .await
                .ok_or(DepositReject::InnerVerifyFailed)?;
            if resolved_owner != frame.sender_owner {
                return Err(DepositReject::InnerVerifyFailed);
            }
            verify_dm_packet_signature(
                &signed_bytes,
                &signature,
                &identity_pub,
                signed.signing_device_hash,
            )
            .map_err(|_| DepositReject::InnerVerifyFailed)?;
            let computed_cid = ContentId::for_book(
                &payload.storage_blob,
                ContentFlags {
                    encrypted: true,
                    ..Default::default()
                },
            )
            .map_err(|_| DepositReject::BadPayload)?;
            if computed_cid != signed.message_cid {
                return Err(DepositReject::InnerVerifyFailed);
            }

            // Step 5.5 (ZEB-424 D28.1, security follow-up) — bind co-member
            // admission to the DEPOSITED space. Step 1 only proved the sender
            // shares SOME live group DM (the `space_id` was still sealed); now
            // that the inner packet is open and its signing device is bound to
            // `frame.sender_owner`, require that the deposit's own
            // `signed.space_id` is a live `GroupDm` containing BOTH this owner
            // and the sender. Without it, a co-member of group A could get a
            // deposit for an unrelated space B persisted+acked, only for
            // ingestion to reject it until TTL — an inbox-slot-pinning DoS
            // plus a lying ack. The friend path is intentionally NOT
            // space-bound here: friendship authorizes 1:1 DM deposits.
            if matches!(admission, Admission::CoMember)
                && !ctx
                    .space_live_group_dm_co_member(&signed.space_id.0, &frame.sender_owner)
                    .await
            {
                return Err(DepositReject::NotAuthorizedForScope);
            }
            let key = DmInboxDoc::key(&signed.space_id.0, &signed.message_cid.to_bytes());
            (
                signed.space_id.0,
                key,
                signed.message_cid.to_bytes().to_vec(),
            )
        }
        // ZEB-505 invite-only deposit: the invite is the sole payload and MUST
        // be present. Verify it with the same device→owner binding + signature
        // primitives the message path uses (so the butler never persists+acks
        // a forged invite — D7), bind co-member admission to the invite's
        // space, then persist keyed by the invite (one standalone invite per
        // space).
        None => {
            // ZEB-691: a device-revocation deposit — no message, no invite,
            // a signed RevocationPush. Pre-validate the certs (D7: never
            // persist+ack a forgery) with the SAME authority the recipient
            // uses on recover, binding the revocation to the AUTHENTICATED
            // depositing friend (`frame.sender_owner`), and key by the
            // revoked device.
            if let Some(rp_bytes) = payload.revocation_push.as_deref() {
                // ZEB-691 converge (Qodo, security): revocations are
                // FRIEND-scoped by design — the send side
                // (`push_revocation_to_friends`) only deposits to ACTIVE
                // friends, and this persists into the friend-scoped
                // `revoked_dm_devices` CRDT (`owner_state_crdt.rs`). A live
                // group-DM co-member is admitted for message/invite
                // deposits above, but is NOT authorized to deposit a
                // revocation: doing so would let a mere co-member write
                // into another owner's friend-scoped revocation set.
                if !matches!(admission, Admission::Friend(_)) {
                    return Err(DepositReject::NotAuthorizedForScope);
                }
                if rp_bytes.len() > crate::butler_deposit::MAX_DEPOSIT_INVITE_BYTES {
                    return Err(DepositReject::BadPayload);
                }
                // The revocation is the SOLE payload: it carries no message,
                // so any storage_blob is unused bytes an admitted sender could
                // attach to waste inbox storage. Reject fail-closed (mirrors
                // the invite-only branch below).
                if !payload.storage_blob.is_empty() {
                    return Err(DepositReject::BadPayload);
                }
                // Symmetric with the blob check above: a pure revocation also
                // carries no invite. Beyond wasting inbox storage, an
                // invite_packet here would make the persisted entry match BOTH
                // `revocation_push.is_some()` and `invite_packet.is_some()`,
                // so the recipient's pure-revocation dispatch guard
                // (`revocation_push.is_some() && cidnotify_packet.is_none() &&
                // invite_packet.is_none()`) would fail to match and silently
                // mis-route/drop the revocation. Reject fail-closed to keep
                // the deposit shape pure.
                if payload.invite_packet.is_some() {
                    return Err(DepositReject::BadPayload);
                }
                // ZEB-674 (C4): symmetric with the invite guard above — a pure
                // revocation carries no grant either. A stray grant_push would
                // make the persisted entry match BOTH dispatch guards, so the
                // recipient's pure-revocation guard would fail to match and drop
                // the revocation. Reject fail-closed to keep the shape pure.
                if payload.grant_push.is_some() {
                    return Err(DepositReject::BadPayload);
                }
                let packet = decode_packet(rp_bytes).map_err(|_| DepositReject::BadPayload)?;
                let DmPacket::RevocationPush {
                    revocation,
                    enrollment,
                } = packet
                else {
                    return Err(DepositReject::BadPayload);
                };
                // PRE-VALIDATE the certs with the EXACT authority the recipient
                // re-applies on recover (Task B5): master-signed revocation +
                // enrollment, BOTH trust-bound to the AUTHENTICATED depositing
                // friend (`frame.sender_owner`). A friend may only revoke THEIR
                // OWN devices, so this fails closed on a relayed third-party
                // revocation — the butler never persists+acks a forgery (D7).
                crate::dm_outbox::verify_revocation_push(
                    crate::owner_state_types::OwnerAddr(frame.sender_owner),
                    &revocation,
                    &enrollment,
                )
                .map_err(|_| DepositReject::InnerVerifyFailed)?;
                // Keyed by the revoking friend + the revoked device: one entry
                // per revoked device, idempotent on redelivery. No space, no
                // message CID — the ack binds to the revocation marker.
                let key = DmInboxDoc::revoke_key(&frame.sender_owner, &revocation.target);
                (
                    [0u8; 16],
                    key,
                    crate::butler_deposit::REVOCATION_DEPOSIT_MARKER.to_vec(),
                )
            } else if let Some(gp_bytes) = payload.grant_push.as_deref() {
                // ZEB-674 (C4): a standalone file-share grant deposit — no
                // message, no invite, no revocation, just the opaque
                // `grant_push` (per-device sealed FileGrantInner blobs). The
                // butler treats it OPAQUELY (each inner blob is sealed
                // end-to-end to a grantee device's X25519 key — see
                // `butler_cannot_open_grant_push`); the recipient decodes +
                // fans it out on recover via `file_sharing::ingest_grant_push`.
                // ZEB-674 converge (Qodo, security): file-share grants are
                // FRIEND-scoped by design — the send side (`grant_read_impl` /
                // `build_grant_push`) only deposits to ACTIVE friends, and the
                // recipient persists into the friend-scoped `received_file_grants`
                // CRDT on recover. A live group-DM co-member is admitted for
                // message/invite deposits above, but — exactly like the
                // revocation branch — is NOT authorized to inject a grant into
                // another owner's friend-scoped received-grants set. Gate on
                // Friend admission; reject any other scope fail-closed.
                if !matches!(admission, Admission::Friend(_)) {
                    return Err(DepositReject::NotAuthorizedForScope);
                }
                // The admission gate authenticated the depositing friend
                // (`frame.sender_owner`); the grant blobs are sealed end-to-end
                // to the grantee's device X25519 keys, so no further inner-packet
                // verification is possible or needed (the butler cannot open the
                // seals — see `butler_cannot_open_grant_push`).
                //
                // The grant is the SOLE payload: reject a non-empty storage_blob
                // or a stray invite fail-closed (mirrors the invite-only /
                // revocation-only waste + pure-shape guards; `revocation_push`
                // is already `None` on this branch).
                if !payload.storage_blob.is_empty() {
                    return Err(DepositReject::BadPayload);
                }
                if payload.invite_packet.is_some() {
                    return Err(DepositReject::BadPayload);
                }
                // Size-bound the opaque grant before persisting (defence against
                // a malicious sender inflating butler storage).
                if gp_bytes.len() > crate::butler_deposit::MAX_DEPOSIT_GRANT_BYTES {
                    return Err(DepositReject::BadPayload);
                }
                // Keyed by the granting friend + a hash of the opaque grant, so
                // a redelivery of the SAME grant is idempotent (one entry per
                // distinct grant payload). No space, no message CID — the ack
                // binds to the grant marker.
                let key = DmInboxDoc::grant_key(&frame.sender_owner, gp_bytes);
                (
                    [0u8; 16],
                    key,
                    crate::butler_deposit::GRANT_DEPOSIT_MARKER.to_vec(),
                )
            } else {
                // CodeRabbit (Stability, Major): the invite is the SOLE payload —
                // an invite-only deposit carries no message, so any storage_blob
                // is unused bytes an admitted sender could attach to waste inbox
                // storage until TTL. Reject a non-empty blob fail-closed.
                if !payload.storage_blob.is_empty() {
                    return Err(DepositReject::BadPayload);
                }
                // CodeRabbit: an invite deposit carries no revocation either;
                // reject stray bytes fail-closed, mirroring the storage_blob
                // waste guard above.
                if payload.revocation_push.is_some() {
                    return Err(DepositReject::BadPayload);
                }
                let invite_bytes = payload
                    .invite_packet
                    .as_deref()
                    .ok_or(DepositReject::BadPayload)?;
                let packet = decode_packet(invite_bytes).map_err(|_| DepositReject::BadPayload)?;
                let (signed, signature, signed_bytes) = match packet {
                    DmPacket::Invite {
                        signed,
                        signature,
                        signed_bytes,
                    } => (signed, signature, signed_bytes),
                    _ => return Err(DepositReject::BadPayload),
                };
                if signed.inviter.0 != frame.sender_owner {
                    return Err(DepositReject::InnerVerifyFailed);
                }
                let (resolved_owner, identity_pub) = ctx
                    .resolve_sender_device(signed.signing_device_hash)
                    .await
                    .ok_or(DepositReject::InnerVerifyFailed)?;
                if resolved_owner != frame.sender_owner {
                    return Err(DepositReject::InnerVerifyFailed);
                }
                // CodeRabbit (Data Integrity, Major): complete invite validation
                // BEFORE persist+ack so the butler never acks an invite that
                // `apply_invite` (run at ingestion) would reject — which would
                // otherwise leave the entry pending until TTL. Mirror the two
                // remaining `apply_invite` gates: (a) the invite's inline
                // `inviter_identity_pub` must match the resolved signing-device
                // pubkey, and (b) the deposit's recipient must actually be a
                // member of the invited Space.
                if signed.inviter_identity_pub != identity_pub {
                    return Err(DepositReject::InnerVerifyFailed);
                }
                if !signed
                    .members
                    .contains(&crate::owner_state_types::OwnerAddr(frame.recipient_owner))
                {
                    return Err(DepositReject::InnerVerifyFailed);
                }
                verify_dm_packet_signature(
                    &signed_bytes,
                    &signature,
                    &identity_pub,
                    signed.signing_device_hash,
                )
                .map_err(|_| DepositReject::InnerVerifyFailed)?;
                if matches!(admission, Admission::CoMember)
                    && !ctx
                        .space_live_group_dm_co_member(&signed.space_id.0, &frame.sender_owner)
                        .await
                {
                    return Err(DepositReject::NotAuthorizedForScope);
                }
                let key = DmInboxDoc::invite_key(&signed.space_id.0);
                (
                    signed.space_id.0,
                    key,
                    crate::butler_deposit::INVITE_ONLY_DEPOSIT_MARKER.to_vec(),
                )
            }
        }
    };

    // Step 6 — atomic persist-with-caps + durable flush BEFORE the ack
    // exists (D7: an ack never lies). Insert-once on the key with the
    // per-sender + global quotas enforced inside the persist critical
    // section; an occupied key bypasses the caps, so a redelivery after a
    // lost ack is absorbed and still acked even at a full inbox.
    let entry = DmInboxEntry {
        sender_owner: frame.sender_owner,
        cidnotify_packet: payload.cidnotify_packet,
        storage_blob: payload.storage_blob,
        invite_packet: payload.invite_packet,
        // ZEB-691 Task B4: carry the decoded RevocationPush wire through. The
        // butler pre-validated it above, but the recipient RE-verifies it on
        // recover (Task B5) — never trust the carrier. `None` for message/invite
        // deposits.
        revocation_push: payload.revocation_push,
        // ZEB-674 (C4): carry the opaque grant_push through to the recipient's
        // ingest sweeper (`file_sharing::ingest_grant_push`). The butler cannot
        // open the per-device seals; `Some` only on a pure grant deposit (the
        // grant-only arm above), `None` otherwise (the pure-shape guards reject
        // a grant riding alongside another sub-payload).
        grant_push: payload.grant_push,
        deposited_at: ctx.mint_hlc().await,
        deposited_by: ctx.device_id(),
        ingested_by: BTreeSet::new(),
    };
    match ctx.persist_entry(key, entry).await {
        Ok(DepositPersistVerdict::Inserted) | Ok(DepositPersistVerdict::Duplicate) => {}
        Ok(DepositPersistVerdict::CapExceeded) => return Err(DepositReject::CapExceeded),
        Err(e) => return Err(DepositReject::PersistFailed(e)),
    }

    // Step 7 — ack.
    Ok(DepositAck {
        space_id: deposit_space_id,
        message_cid: ack_message_cid,
    })
}

// =====================================================================
// ZEB-702: local-only butler-deposit decision observability
// =====================================================================

/// ZEB-702: minimum interval between unauthorized-reject WARN emissions. A
/// butler that fail-closed rejects EVERY deposit (its `friend_graph` never
/// replicated — the ZEB-702 roster-sync failure) is otherwise indistinguishable
/// from transport failure at default log level; one WARN per window surfaces it
/// while keeping probing traffic from spamming the log.
const BUTLER_REJECT_WARN_MIN_INTERVAL_MS: u64 = 60_000;

/// Default clock for [`ButlerDepositStats`]: MONOTONIC milliseconds since
/// process start (Greptile, PR #481: a wall clock would let a forward system
/// clock step re-open the WARN window early — `Instant` is immune to clock
/// corrections, which is the right source for a process-local rate gate).
/// `.max(1)` because `last_warn_ms == 0` encodes "never warned": the first
/// millisecond of process life must not alias it (this also retires the
/// injected-clock-of-0 edge recorded in the T4 review). Injectable via
/// [`ButlerDepositStats::with_clock`] in tests so the rate-limit window is
/// deterministic (the WARN gate reads THIS clock, not tokio's, so paused
/// tokio time would not control it).
fn butler_stats_default_clock() -> Arc<dyn Fn() -> u64 + Send + Sync> {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = *START.get_or_init(std::time::Instant::now);
    Arc::new(move || (start.elapsed().as_millis() as u64).max(1))
}

/// Process-lifetime butler-deposit decision counts. Snapshot of
/// [`ButlerDepositStats`]; Task 5 serializes it into `network_health_snapshot`
/// (serde camelCase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ButlerDepositCounts {
    pub accepted: u64,
    pub rejected_unauthorized: u64,
    pub rejected_other: u64,
}

/// ZEB-702 (Component D): process-lifetime accept/reject counters for the
/// butler-deposit acceptor, plus the rate-limited unauthorized-reject WARN.
/// `Arc`-shared between the acceptor shell (writer) and
/// `network_health_snapshot` (reader), mirroring the dial-outcome counters
/// (`network_health.rs` `DialTelemetry`). LOCAL-ONLY: nothing here touches the
/// wire — the reject stays byte-identical, close-without-detail (spec §4, no
/// oracle).
pub struct ButlerDepositStats {
    accepted: AtomicU64,
    rejected_unauthorized: AtomicU64,
    rejected_other: AtomicU64,
    /// Unix-epoch ms of the last unauthorized-reject WARN; `0` = never warned
    /// (production wall-clock ms is never 0, so the first reject always warns).
    /// Gates the WARN to ≤1 per [`BUTLER_REJECT_WARN_MIN_INTERVAL_MS`] via
    /// compare-exchange.
    last_warn_ms: AtomicU64,
    /// Count of WARNs actually emitted (observability + test hook for the
    /// rate-limit assertions). Distinct from `rejected_unauthorized`, which
    /// counts every reject regardless of whether it warned.
    warns_emitted: AtomicU64,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl ButlerDepositStats {
    pub fn new() -> Self {
        Self {
            accepted: AtomicU64::new(0),
            rejected_unauthorized: AtomicU64::new(0),
            rejected_other: AtomicU64::new(0),
            last_warn_ms: AtomicU64::new(0),
            warns_emitted: AtomicU64::new(0),
            now_ms: butler_stats_default_clock(),
        }
    }

    /// Test seam: inject a controllable wall clock (Unix-epoch ms) so the
    /// rate-limit window is deterministic.
    #[cfg(test)]
    fn with_clock(now_ms: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            now_ms,
            ..Self::new()
        }
    }

    /// Current decision counts.
    pub fn snapshot(&self) -> ButlerDepositCounts {
        ButlerDepositCounts {
            accepted: self.accepted.load(Ordering::Relaxed),
            rejected_unauthorized: self.rejected_unauthorized.load(Ordering::Relaxed),
            rejected_other: self.rejected_other.load(Ordering::Relaxed),
        }
    }

    /// Total WARNs emitted (rate-limited), for observability.
    pub fn warn_emissions(&self) -> u64 {
        self.warns_emitted.load(Ordering::Relaxed)
    }

    /// Count an accepted deposit (ack delivered).
    pub fn record_accepted(&self) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a rejected deposit. A ROSTER-MISS reject (`NotAuthorized`) also
    /// fires a rate-limited WARN (≤1 per [`BUTLER_REJECT_WARN_MIN_INTERVAL_MS`])
    /// naming ZEB-702 and the remedy hint. Every other reject reason (bad
    /// cert/sig, cap, malformed payload, inner-verify, persist, and the
    /// post-admission `NotAuthorizedForScope` class — an admitted sender
    /// failing a space/operation binding) is a `rejected_other` — already
    /// covered by the shell's per-reject `debug!` and never warned (only a
    /// persistently-rejecting roster is the ZEB-702 signal).
    pub fn record_rejected(&self, reject: &DepositReject) {
        match reject {
            DepositReject::NotAuthorized => {
                let n = self.rejected_unauthorized.fetch_add(1, Ordering::Relaxed) + 1;
                if self.arm_warn((self.now_ms)()) {
                    self.warns_emitted.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        rejected_unauthorized = n,
                        "ZEB-702: butler deposit rejected — sender not in this butler's \
                         roster; if persistent, owner-state sync to this sibling is not \
                         converging"
                    );
                }
            }
            _ => {
                self.rejected_other.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// ≤1-per-window WARN gate. Returns `true` (and arms the window at `now_ms`)
    /// iff the caller should emit now. The compare-exchange makes concurrent
    /// unauthorized rejects emit at most one WARN per window (the losers see the
    /// updated `last_warn_ms` and back off).
    fn arm_warn(&self, now_ms: u64) -> bool {
        let last = self.last_warn_ms.load(Ordering::Relaxed);
        if last != 0 && now_ms.saturating_sub(last) < BUTLER_REJECT_WARN_MIN_INTERVAL_MS {
            return false;
        }
        self.last_warn_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

impl Default for ButlerDepositStats {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ButlerDepositStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let c = self.snapshot();
        f.debug_struct("ButlerDepositStats")
            .field("accepted", &c.accepted)
            .field("rejected_unauthorized", &c.rejected_unauthorized)
            .field("rejected_other", &c.rejected_other)
            .field("warns_emitted", &self.warn_emissions())
            .finish_non_exhaustive()
    }
}

// =====================================================================
// Thin iroh shell — length-prefixed frame in, length-prefixed ack out
// =====================================================================

/// Default per-await IO deadline for the inbound deposit exchange. Mirrors
/// `iroh_friend_acceptor::DEFAULT_FRIEND_IO_DEADLINE_MS`.
pub const DEFAULT_BUTLER_IO_DEADLINE_MS: u64 = 30_000;

/// Tunable timeouts for the deposit shell. Tests construct this directly
/// with sub-second values; production uses [`Self::default`].
#[derive(Debug, Clone, Copy)]
pub struct ButlerAcceptorConfig {
    /// Per-await IO timeout bounding `accept_bi`, the frame read, the ack
    /// write, and the post-ack `conn.closed()` wait.
    pub io_deadline: Duration,
}

impl Default for ButlerAcceptorConfig {
    fn default() -> Self {
        Self {
            io_deadline: Duration::from_millis(DEFAULT_BUTLER_IO_DEADLINE_MS),
        }
    }
}

/// Errors that short-circuit one inbound deposit connection. Internal to the
/// shell — NOTHING from here reaches the sender (spec §4: rejects close the
/// stream without detail).
#[derive(Debug, thiserror::Error)]
enum DepositConnError {
    #[error("accept_bi: {0}")]
    AcceptBi(String),
    #[error("read frame: {0}")]
    Read(String),
    #[error("decode frame: {0}")]
    Decode(String),
    #[error("rejected: {0}")]
    Reject(DepositReject),
    #[error("encode ack: {0}")]
    EncodeAck(String),
    #[error("write ack: {0}")]
    Write(String),
    #[error("send.finish: {0}")]
    Finish(String),
    #[error("IO timeout in {step}")]
    IoTimeout { step: &'static str },
}

/// Inbound handler for the `harmony/butler-deposit/v1` ALPN. Installed on
/// the iroh accept loop via
/// `IrohZenohLinkManager::install_butler_deposit_acceptor` (Task 7 builds
/// the production [`ButlerDepositCtx`] once `NodeState`'s dm-inbox engine
/// handles exist and installs it there).
pub struct IrohButlerDepositAcceptor {
    ctx: Arc<dyn ButlerDepositCtx>,
    config: ButlerAcceptorConfig,
    /// Spec §4: rejected deposits are "unlogged beyond a counter" — this is
    /// that counter (rejects additionally surface at `debug!`, never `warn!`,
    /// so probing traffic can't spam production logs).
    rejected_deposits: AtomicU64,
    /// ZEB-702 (Component D): accept/reject decision counters + the
    /// rate-limited unauthorized-reject WARN. `Arc`-shared so the install site
    /// can hand a clone to `network_health_snapshot` (Task 5). Defaults fresh,
    /// so existing callers are unchanged.
    stats: Arc<ButlerDepositStats>,
}

impl IrohButlerDepositAcceptor {
    pub fn new(ctx: Arc<dyn ButlerDepositCtx>) -> Self {
        Self::with_config(ctx, ButlerAcceptorConfig::default())
    }

    pub fn with_config(ctx: Arc<dyn ButlerDepositCtx>, config: ButlerAcceptorConfig) -> Self {
        Self {
            ctx,
            config,
            rejected_deposits: AtomicU64::new(0),
            stats: Arc::new(ButlerDepositStats::new()),
        }
    }

    /// ZEB-702: install a shared [`ButlerDepositStats`] handle (default is a
    /// fresh one). Fluent so the install site can construct the stats first,
    /// keep a clone for `network_health_snapshot`, and pass it in here.
    pub fn with_deposit_stats(mut self, stats: Arc<ButlerDepositStats>) -> Self {
        self.stats = stats;
        self
    }

    /// ZEB-702: shared handle to the accept/reject decision counters (an
    /// `Arc` clone; the install site threads it into `network_health_snapshot`).
    pub fn deposit_stats(&self) -> Arc<ButlerDepositStats> {
        Arc::clone(&self.stats)
    }

    /// Total rejected deposits since construction (spec §4 counter).
    pub fn rejected_deposit_count(&self) -> u64 {
        self.rejected_deposits.load(Ordering::Relaxed)
    }

    /// Handle one inbound deposit connection: read the length-prefixed
    /// [`DepositFrame`] (cap enforced BEFORE the body allocation), run
    /// [`handle_deposit_core`], write the length-prefixed ack, finish. On
    /// ANY failure the stream is closed uniformly with no detail (spec §4 —
    /// no oracle for probing the friend graph).
    pub async fn handle_connection(&self, conn: Connection) {
        match self.handle_deposit_inbound(&conn).await {
            Ok(()) => {
                self.stats.record_accepted();
                tracing::info!(
                    remote_id = ?conn.remote_id(),
                    "ZEB-418: butler deposit accepted (ack delivered)"
                );
                // Wait for the dialer to drive the close so the ack bytes
                // flush before `conn` drops (same race-avoidance as
                // iroh_friend_acceptor / iroh_invite_acceptor).
                let _ = tokio::time::timeout(self.config.io_deadline, conn.closed()).await;
            }
            Err(DepositConnError::Reject(reject)) => {
                self.rejected_deposits.fetch_add(1, Ordering::Relaxed);
                // ZEB-702: classify the reject into the decision counters and,
                // for an unauthorized reject, fire the rate-limited WARN. Both
                // are local-only (atomics + an occasional local log) — the wire
                // reject below stays byte-identical: same close-without-detail.
                self.stats.record_rejected(&reject);
                tracing::debug!(
                    reject = %reject,
                    remote_id = ?conn.remote_id(),
                    "ZEB-418: butler deposit rejected (closing without detail)"
                );
                conn.close(0u32.into(), b"");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    remote_id = ?conn.remote_id(),
                    "ZEB-418: butler deposit connection failed"
                );
                conn.close(0u32.into(), b"");
            }
        }
    }

    async fn handle_deposit_inbound(&self, conn: &Connection) -> Result<(), DepositConnError> {
        let (mut send, mut recv) = tokio::time::timeout(self.config.io_deadline, conn.accept_bi())
            .await
            .map_err(|_| DepositConnError::IoTimeout { step: "accept_bi" })?
            .map_err(|e| DepositConnError::AcceptBi(e.to_string()))?;

        // Length-prefixed frame read — `read_length_prefixed` rejects a
        // zero/oversize prefix BEFORE reading or allocating the body.
        let body = tokio::time::timeout(self.config.io_deadline, read_length_prefixed(&mut recv))
            .await
            .map_err(|_| DepositConnError::IoTimeout { step: "read frame" })?
            .map_err(|e| DepositConnError::Read(e.to_string()))?;
        let frame =
            decode_deposit_frame(&body).map_err(|e| DepositConnError::Decode(e.to_string()))?;

        let ack = handle_deposit_core(&frame, self.ctx.as_ref())
            .await
            .map_err(DepositConnError::Reject)?;

        let ack_bytes =
            encode_deposit_ack(&ack).map_err(|e| DepositConnError::EncodeAck(e.to_string()))?;
        tokio::time::timeout(
            self.config.io_deadline,
            write_length_prefixed(&mut send, &ack_bytes),
        )
        .await
        .map_err(|_| DepositConnError::IoTimeout { step: "write ack" })?
        .map_err(|e| DepositConnError::Write(e.to_string()))?;
        // `send.finish()` is sync — no timeout needed.
        send.finish()
            .map_err(|e| DepositConnError::Finish(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler_deposit::{
        encode_deposit_payload, DepositPayload, BUTLER_DEPOSIT_SEAL_INFO, GRANT_DEPOSIT_MARKER,
        MAX_DEPOSIT_GRANT_BYTES, MAX_DEPOSIT_INVITE_BYTES, REVOCATION_DEPOSIT_MARKER,
    };
    use crate::community_membership::{mint_test_owner, TestOwner};
    use crate::dm_envelope::{build_signed_cidnotify, encode_packet, DmCidNotifySigned};
    use crate::dm_signing::{
        derive_device_hash_from_identity_pub, ed25519_pub_to_x25519, seal_to_owner_with_info,
    };
    use crate::owner_state_types::{OwnerAddr, SpaceId};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    /// The butler's (recipient's) owner address.
    const BUTLER_OWNER: [u8; 16] = [0x88; 16];

    /// The butler device's ed25519 signing key — its X25519 birational twin
    /// is the seal target (exactly the production derivation).
    fn butler_device_sk() -> SigningKey {
        SigningKey::from_bytes(&[0x61; 32])
    }

    /// The sender's owner identity: master + enrolled deposit device + cert.
    fn sender() -> TestOwner {
        mint_test_owner(0x51)
    }

    fn master_from_cert(cert: &EnrollmentCert) -> [u8; 32] {
        use harmony_owner::certs::EnrollmentIssuer;
        match &cert.issuer {
            EnrollmentIssuer::Master { master_pubkey } => master_pubkey.classical.ed25519_verify,
            other => panic!("test certs are Master-issued, got {other:?}"),
        }
    }

    /// The sender's DM-transport device identity (the key that signs the
    /// inner CidNotify packet — distinct from the cert-bound deposit device
    /// key). Synthetic identity_pub trick per `dm_outbox`'s tests: all-zero
    /// X25519 half + real Ed25519 half.
    fn dm_identity() -> (SigningKey, [u8; 64], DeviceIdentityHash) {
        let sk = SigningKey::from_bytes(&[0x53; 32]);
        let mut identity_pub = [0u8; 64];
        identity_pub[32..].copy_from_slice(sk.verifying_key().as_bytes());
        let hash = derive_device_hash_from_identity_pub(&identity_pub)
            .expect("synthetic identity_pub is valid");
        (sk, identity_pub, hash)
    }

    /// Build a signed CidNotify packet (wire bytes) for the fixture.
    fn build_cidnotify(
        sender_owner: OwnerAddr,
        space_id: SpaceId,
        message_cid: ContentId,
    ) -> (Vec<u8>, [u8; 64], DeviceIdentityHash) {
        let (dm_sk, identity_pub, hash) = dm_identity();
        let signed = DmCidNotifySigned {
            space_id,
            message_cid,
            sender_owner_addr: sender_owner,
            sender_devices: vec![hash],
            signing_device_hash: hash,
        };
        let packet = build_signed_cidnotify(signed, &dm_sk).expect("build cidnotify");
        let bytes = encode_packet(&packet).expect("encode cidnotify packet");
        (bytes, identity_pub, hash)
    }

    /// Seal `payload` to the butler device's birational X25519 (production
    /// construction: `seal_to_owner_with_info(birational(vk), …)`).
    fn seal_payload_bytes(payload_bytes: &[u8]) -> Vec<u8> {
        let butler_x_pub = ed25519_pub_to_x25519(&butler_device_sk().verifying_key().to_bytes())
            .expect("butler key birational");
        seal_to_owner_with_info(&butler_x_pub, payload_bytes, BUTLER_DEPOSIT_SEAL_INFO)
            .expect("seal payload")
    }

    /// Sign the deposit frame the way the sender will (Task 8).
    fn sign_frame(ro: &[u8; 16], sealed: &[u8], device_key: &SigningKey) -> Vec<u8> {
        device_key
            .sign(&deposit_sig_payload(ro, sealed))
            .to_bytes()
            .to_vec()
    }

    struct Fixture {
        frame: DepositFrame,
        sender_owner: [u8; 16],
        sender_master: [u8; 32],
        space_id: SpaceId,
        message_cid: ContentId,
        cidnotify_packet: Vec<u8>,
        storage_blob: Vec<u8>,
        dm_device_hash: DeviceIdentityHash,
        identity_pub: [u8; 64],
    }

    /// A fully valid deposit frame from an Active friend.
    fn valid_fixture() -> Fixture {
        let so = sender();
        let space_id = SpaceId([0x77; 16]);
        let storage_blob = b"encrypted-dm-storage-blob-bytes".to_vec();
        let message_cid = ContentId::for_book(
            &storage_blob,
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("cid for blob");
        let (cidnotify_packet, identity_pub, dm_device_hash) =
            build_cidnotify(so.owner, space_id, message_cid);
        let payload = DepositPayload {
            cidnotify_packet: Some(cidnotify_packet.clone()),
            storage_blob: storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        };
        let payload_bytes = encode_deposit_payload(&payload).expect("encode payload");
        let sealed = seal_payload_bytes(&payload_bytes);
        let sig = sign_frame(&BUTLER_OWNER, &sealed, &so.device_key);
        let cert_bytes = harmony_owner::cbor::to_canonical(&so.cert).expect("encode cert");
        Fixture {
            frame: DepositFrame {
                signer_certs_cbor: Vec::new(),
                recipient_owner: BUTLER_OWNER,
                sender_owner: so.owner.0,
                sender_enrollment_cert: cert_bytes,
                sig,
                sealed_blob: sealed,
            },
            sender_owner: so.owner.0,
            sender_master: master_from_cert(&so.cert),
            space_id,
            message_cid,
            cidnotify_packet,
            storage_blob,
            dm_device_hash,
            identity_pub,
        }
    }

    /// Same as [`valid_fixture`] but the sealed `DepositPayload` carries an
    /// `invite_packet` (ZEB-483). The CidNotify + storage blob are identical, so
    /// the acceptor's existing validation path is exercised unchanged.
    fn valid_fixture_with_invite(invite_packet: Option<Vec<u8>>) -> Fixture {
        let so = sender();
        let space_id = SpaceId([0x77; 16]);
        let storage_blob = b"encrypted-dm-storage-blob-bytes".to_vec();
        let message_cid = ContentId::for_book(
            &storage_blob,
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("cid for blob");
        let (cidnotify_packet, identity_pub, dm_device_hash) =
            build_cidnotify(so.owner, space_id, message_cid);
        let payload = DepositPayload {
            cidnotify_packet: Some(cidnotify_packet.clone()),
            storage_blob: storage_blob.clone(),
            invite_packet,
            revocation_push: None,
            grant_push: None,
        };
        let payload_bytes = encode_deposit_payload(&payload).expect("encode payload");
        let sealed = seal_payload_bytes(&payload_bytes);
        let sig = sign_frame(&BUTLER_OWNER, &sealed, &so.device_key);
        let cert_bytes = harmony_owner::cbor::to_canonical(&so.cert).expect("encode cert");
        Fixture {
            frame: DepositFrame {
                signer_certs_cbor: Vec::new(),
                recipient_owner: BUTLER_OWNER,
                sender_owner: so.owner.0,
                sender_enrollment_cert: cert_bytes,
                sig,
                sealed_blob: sealed,
            },
            sender_owner: so.owner.0,
            sender_master: master_from_cert(&so.cert),
            space_id,
            message_cid,
            cidnotify_packet,
            storage_blob,
            dm_device_hash,
            identity_pub,
        }
    }

    /// Probe ctx: records the order of friend-lookup / decrypt / resolve /
    /// persist calls, and backs persist with an insert-once map (with the
    /// production atomic-cap logic) so tests can assert exactly what was
    /// durably written before the ack existed.
    struct TestCtx {
        self_owner: [u8; 16],
        friends: BTreeMap<[u8; 16], ([u8; 32], FriendStatus)>,
        /// ZEB-424: owners that share a live group-DM with self (the
        /// `shares_live_group_dm` source). Empty by default.
        group_co_members: std::collections::BTreeSet<[u8; 16]>,
        /// ZEB-424 (D28.1): `(space_id, sender_owner)` pairs the
        /// `space_live_group_dm_co_member` post-decrypt bind treats as a live
        /// `GroupDm` with both members. Empty by default.
        group_co_member_spaces: std::collections::BTreeSet<([u8; 16], [u8; 16])>,
        /// Sender DEVICE → (owner id, identity pub) — the
        /// `resolve_sender_device` source (mirrors the production
        /// `owner_device_cache` resolution).
        device_owners: BTreeMap<DeviceIdentityHash, ([u8; 16], [u8; 64])>,
        butler_sk: SigningKey,
        persist_fail: bool,
        store: StdMutex<BTreeMap<String, DmInboxEntry>>,
        events: StdMutex<Vec<String>>,
    }

    impl TestCtx {
        /// Ctx where the fixture's sender is an Active friend and its DM
        /// device identity is cached under the sender's owner.
        fn for_fixture(f: &Fixture) -> Self {
            let mut friends = BTreeMap::new();
            friends.insert(f.sender_owner, (f.sender_master, FriendStatus::Active));
            let mut device_owners = BTreeMap::new();
            device_owners.insert(f.dm_device_hash, (f.sender_owner, f.identity_pub));
            Self {
                self_owner: BUTLER_OWNER,
                friends,
                group_co_members: std::collections::BTreeSet::new(),
                group_co_member_spaces: std::collections::BTreeSet::new(),
                device_owners,
                butler_sk: butler_device_sk(),
                persist_fail: false,
                store: StdMutex::new(BTreeMap::new()),
                events: StdMutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        fn push_event(&self, e: impl Into<String>) {
            self.events.lock().unwrap().push(e.into());
        }
    }

    #[async_trait]
    impl ButlerDepositCtx for TestCtx {
        fn self_owner(&self) -> [u8; 16] {
            self.self_owner
        }

        async fn lookup_friend(&self, sender_owner: &[u8; 16]) -> Option<([u8; 32], FriendStatus)> {
            self.push_event("friend_lookup");
            self.friends.get(sender_owner).copied()
        }

        async fn shares_live_group_dm(&self, sender_owner: &[u8; 16]) -> bool {
            self.push_event("group_lookup");
            self.group_co_members.contains(sender_owner)
        }

        async fn space_live_group_dm_co_member(
            &self,
            space_id: &[u8; 16],
            sender_owner: &[u8; 16],
        ) -> bool {
            self.push_event("group_space_lookup");
            self.group_co_member_spaces
                .contains(&(*space_id, *sender_owner))
        }

        fn now_secs(&self) -> u64 {
            1_700_000_100
        }

        fn decrypt(&self, sealed_blob: &[u8]) -> Result<Vec<u8>, String> {
            self.push_event("decrypt");
            crate::dm_signing::open_from_owner_with_info(
                &crate::dm_signing::ed25519_priv_to_x25519(&self.butler_sk),
                sealed_blob,
                BUTLER_DEPOSIT_SEAL_INFO,
            )
            .map_err(|e| format!("{e:?}"))
        }

        async fn resolve_sender_device(
            &self,
            device_hash: DeviceIdentityHash,
        ) -> Option<([u8; 16], [u8; 64])> {
            self.push_event("resolve");
            self.device_owners.get(&device_hash).copied()
        }

        fn device_id(&self) -> String {
            "butler-device-64hex".into()
        }

        async fn mint_hlc(&self) -> Hlc {
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: self.device_id(),
            }
        }

        /// Production atomic-cap logic over the test store: occupied key →
        /// Duplicate (caps bypassed); vacant key → quota check, then
        /// insert. A CapExceeded verdict writes nothing and records no
        /// "persist:" event (nothing was made durable).
        async fn persist_entry(
            &self,
            key: String,
            entry: DmInboxEntry,
        ) -> Result<DepositPersistVerdict, String> {
            if self.persist_fail {
                return Err("simulated flush failure".into());
            }
            let mut store = self.store.lock().unwrap();
            if store.contains_key(&key) {
                // Mirror the production None→Some invite upgrade (ZEB-483
                // CodeAnt): a redeposit that carries the bootstrap invite heals a
                // stored entry that lacked it; never overwrite an existing one.
                if entry.invite_packet.is_some() {
                    if let Some(stored) = store.get_mut(&key) {
                        if stored.invite_packet.is_none() {
                            stored.invite_packet = entry.invite_packet.clone();
                        }
                    }
                }
                self.push_event(format!("persist:{key}"));
                return Ok(DepositPersistVerdict::Duplicate);
            }
            let sender_pending = store
                .values()
                .filter(|e| e.sender_owner == entry.sender_owner)
                .count();
            if sender_pending >= INBOX_PER_SENDER_CAP || store.len() >= INBOX_GLOBAL_CAP {
                return Ok(DepositPersistVerdict::CapExceeded);
            }
            store.insert(key.clone(), entry);
            // Record AFTER the write so "persist:<key>" in the event log
            // means the entry is durably in the store.
            self.push_event(format!("persist:{key}"));
            Ok(DepositPersistVerdict::Inserted)
        }
    }

    /// Minimal store filler for the cap tests — only ever counted by the
    /// persist-level quota logic, never decoded.
    fn filler_entry(sender_owner: [u8; 16]) -> DmInboxEntry {
        DmInboxEntry {
            sender_owner,
            cidnotify_packet: Some(Vec::new()),
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            deposited_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "filler".into(),
            },
            deposited_by: "filler".into(),
            ingested_by: BTreeSet::new(),
        }
    }

    #[tokio::test]
    async fn deposit_carries_invite_packet_into_persisted_entry() {
        let invite = vec![0xABu8; 200];
        let f = valid_fixture_with_invite(Some(invite.clone()));
        let ctx = TestCtx::for_fixture(&f);

        handle_deposit_core(&f.frame, &ctx).await.expect("accepted");

        let key = DmInboxDoc::key(&f.space_id.0, &f.message_cid.to_bytes());
        let store = ctx.store.lock().unwrap();
        let entry = store.get(&key).expect("persisted");
        assert_eq!(
            entry.invite_packet,
            Some(invite),
            "invite carried through verbatim"
        );
        assert_eq!(
            entry.cidnotify_packet,
            Some(f.cidnotify_packet),
            "CidNotify validation/persist unchanged"
        );
    }

    #[tokio::test]
    async fn deposit_with_oversized_invite_is_rejected() {
        let f = valid_fixture_with_invite(Some(vec![0u8; MAX_DEPOSIT_INVITE_BYTES + 1]));
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("must reject");
        assert!(
            matches!(err, DepositReject::BadPayload),
            "oversized invite => BadPayload, got {err:?}"
        );
    }

    #[tokio::test]
    async fn deposit_from_active_friend_is_accepted_persisted_then_acked() {
        let f = valid_fixture();
        let ctx = TestCtx::for_fixture(&f);

        let ack = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("valid deposit from active friend must be accepted");

        // Ack carries the inner packet's space + CID.
        assert_eq!(ack.space_id, f.space_id.0);
        assert_eq!(ack.message_cid, f.message_cid.to_bytes().to_vec());

        // The dataset entry exists (it was inserted BEFORE the ack value
        // could exist — the persist probe records post-write, and the core
        // only returns Ok after persist resolves).
        let key = DmInboxDoc::key(&f.space_id.0, &f.message_cid.to_bytes());
        {
            let store = ctx.store.lock().unwrap();
            let entry = store.get(&key).expect("entry persisted under inbox key");
            assert_eq!(entry.sender_owner, f.sender_owner);
            assert_eq!(entry.cidnotify_packet, Some(f.cidnotify_packet.clone()));
            assert_eq!(entry.storage_blob, f.storage_blob);
            assert_eq!(entry.deposited_by, ctx.device_id());
            assert!(entry.ingested_by.is_empty());
        }

        // Order probe: admission → decrypt → device-owner resolve →
        // persist, with persist the LAST event before the ack was produced
        // (caps now live INSIDE persist, so there is no standalone caps
        // step).
        assert_eq!(
            ctx.events(),
            vec![
                "friend_lookup".to_string(),
                "decrypt".to_string(),
                "resolve".to_string(),
                format!("persist:{key}"),
            ]
        );

        // D7 (persist-before-ack, the contrapositive): if the persist sink
        // fails, NO ack may be produced — even for an otherwise fully valid
        // deposit.
        let failing = TestCtx {
            persist_fail: true,
            ..TestCtx::for_fixture(&f)
        };
        let err = handle_deposit_core(&f.frame, &failing)
            .await
            .expect_err("a failed persist must never be acked");
        assert!(
            matches!(err, DepositReject::PersistFailed(_)),
            "expected PersistFailed, got {err:?}"
        );
        assert!(failing.store.lock().unwrap().is_empty());
    }

    /// ZEB-677: a deposit whose sender cert is QUORUM-issued is accepted when
    /// the frame carries the Master-issued signer-cert bundle (the master
    /// anchor for the friend-graph pin comes from the verified bundle);
    /// with the bundle stripped it fails closed as BadCert.
    #[tokio::test]
    async fn deposit_with_quorum_cert_requires_bundle() {
        use crate::enrollment_verify::quorum_fixtures::mint_quorum_world;
        let world = mint_quorum_world(0xB0);
        let sender_owner = crate::owner_state_types::OwnerAddr(world.owner_id);
        let space_id = SpaceId([0x77; 16]);
        let storage_blob = b"encrypted-dm-storage-blob-bytes".to_vec();
        let message_cid = ContentId::for_book(
            &storage_blob,
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("cid for blob");
        let (cidnotify_packet, identity_pub, dm_device_hash) =
            build_cidnotify(sender_owner, space_id, message_cid);
        let payload = DepositPayload {
            cidnotify_packet: Some(cidnotify_packet.clone()),
            storage_blob: storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        };
        let payload_bytes = encode_deposit_payload(&payload).expect("encode payload");
        let sealed = seal_payload_bytes(&payload_bytes);
        // Device C (quorum-enrolled) signs the deposit.
        let sig = sign_frame(&BUTLER_OWNER, &sealed, &world.c_sk);
        let cert_bytes =
            harmony_owner::cbor::to_canonical(&world.c_quorum_cert).expect("encode cert");
        let bundle_bytes = harmony_owner::cbor::to_canonical(&world.bundle).expect("encode bundle");
        let f = Fixture {
            frame: DepositFrame {
                recipient_owner: BUTLER_OWNER,
                sender_owner: world.owner_id,
                sender_enrollment_cert: cert_bytes,
                sig,
                sealed_blob: sealed,
                signer_certs_cbor: bundle_bytes,
            },
            sender_owner: world.owner_id,
            // The friend-graph pin: the owner's master anchor, which the
            // acceptor must recover from the signer-cert bundle.
            sender_master: world.master_ed25519,
            space_id,
            message_cid,
            cidnotify_packet,
            storage_blob,
            dm_device_hash,
            identity_pub,
        };
        let ctx = TestCtx::for_fixture(&f);
        handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("quorum-certed deposit with bundle must be accepted");

        // Bundle stripped → the quorum cert cannot be verified → BadCert.
        let mut stripped = f.frame.clone();
        stripped.signer_certs_cbor = Vec::new();
        let ctx2 = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&stripped, &ctx2)
            .await
            .expect_err("quorum cert without bundle must be rejected");
        assert!(matches!(err, DepositReject::BadCert), "got {err:?}");
    }

    /// ZEB-424 (D27/D29.1/D28.1): a sender who is NOT a friend at all but
    /// shares a live group-DM with the butler AND deposits into that very
    /// space is admitted via the co-member fallback. The friend lookup is
    /// consulted BEFORE the (pre-decrypt) group lookup (friend status is the
    /// primary, cheaper trust anchor), and the post-decrypt space bind runs
    /// AFTER decrypt (the deposit's space_id is sealed until then).
    #[tokio::test]
    async fn deposit_from_non_friend_group_co_member_is_accepted() {
        let f = valid_fixture();
        let mut ctx = TestCtx::for_fixture(&f);
        ctx.friends.clear();
        ctx.group_co_members.insert(f.sender_owner);
        // The deposit's own space (f.space_id) is a live GroupDm with both
        // members → the D28.1 post-decrypt bind passes.
        ctx.group_co_member_spaces
            .insert((f.space_id.0, f.sender_owner));
        handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("co-member admitted");
        let ev = ctx.events();
        let fp = ev.iter().position(|e| e == "friend_lookup").unwrap();
        let gp = ev.iter().position(|e| e == "group_lookup").unwrap();
        let dp = ev.iter().position(|e| e == "decrypt").unwrap();
        let sp = ev.iter().position(|e| e == "group_space_lookup").unwrap();
        assert!(fp < gp, "friend lookup precedes group lookup: {ev:?}");
        assert!(
            gp < dp && dp < sp,
            "pre-decrypt group gate, then decrypt, then post-decrypt space bind: {ev:?}"
        );
    }

    /// ZEB-424 D28.1 security follow-up: a sender who shares SOME live group
    /// DM (so step-1 admission passes) but whose deposit names a space they
    /// are NOT a live member of is rejected `NotAuthorizedForScope` (an
    /// ADMITTED sender failing the space bind — deliberately NOT the ZEB-702
    /// roster-miss signal) AFTER decrypt but
    /// BEFORE persist/ack — closing the "deposit for an unrelated space"
    /// inbox-slot-pinning / lying-ack vector. The bind is on the inner
    /// packet's own space_id, not merely "shares any group".
    #[tokio::test]
    async fn co_member_deposit_for_non_member_space_rejected_before_persist() {
        let f = valid_fixture();
        let mut ctx = TestCtx::for_fixture(&f);
        ctx.friends.clear();
        // Shares SOME live group → step 1 admits…
        ctx.group_co_members.insert(f.sender_owner);
        // …but the deposit's own space (f.space_id) is NOT registered as a
        // live GroupDm with both members → the post-decrypt bind rejects.
        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("co-member depositing into a non-member space is rejected");
        assert!(
            matches!(err, DepositReject::NotAuthorizedForScope),
            "got {err:?}"
        );
        let ev = ctx.events();
        // The reject is POST-decrypt (the space_id is sealed until then)…
        assert!(
            ev.iter().any(|e| e == "decrypt") && ev.iter().any(|e| e == "group_space_lookup"),
            "space bind runs post-decrypt: {ev:?}"
        );
        // …and crucially nothing was persisted or acked.
        assert!(
            !ev.iter().any(|e| e.starts_with("persist:")),
            "no persist on a space-bind reject: {ev:?}"
        );
        assert!(ctx.store.lock().unwrap().is_empty());
    }

    /// ZEB-424: the friend path is untouched — an Active friend with NO group
    /// co-membership is still admitted exactly as before (the group fallback
    /// is never even consulted for an Active friend).
    #[tokio::test]
    async fn deposit_from_active_friend_still_accepted_without_group() {
        let f = valid_fixture();
        let ctx = TestCtx::for_fixture(&f); // friend-Active, no group
        handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("friend still admitted");
        assert!(
            !ctx.events().iter().any(|e| e == "group_lookup"),
            "an Active friend must not trigger the group fallback: {:?}",
            ctx.events()
        );
    }

    /// ZEB-424: a sender who is neither an Active friend nor a live group-DM
    /// co-member is rejected with `NotAuthorized` BEFORE any decryption.
    #[tokio::test]
    async fn deposit_from_neither_friend_nor_co_member_rejected_before_decrypt() {
        let f = valid_fixture();
        let mut ctx = TestCtx::for_fixture(&f);
        ctx.friends.clear(); // group_co_members already empty → neither
        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("rejected");
        assert!(matches!(err, DepositReject::NotAuthorized));
        assert!(
            !ctx.events().iter().any(|e| e == "decrypt"),
            "no decrypt on reject: {:?}",
            ctx.events()
        );
    }

    /// ZEB-424 D29.1 regression guard: the friend path STILL pins the cert
    /// master byte-for-byte against the friend-graph's stored master. An
    /// Active friend whose pinned master differs from the cert master is
    /// rejected `BadCert` via the UNCHANGED friend branch — the co-member
    /// derived-anchor relaxation must not leak into the friend path.
    #[tokio::test]
    async fn friend_path_still_pins_master_mismatch_rejected() {
        let f = valid_fixture();
        let mut ctx = TestCtx::for_fixture(&f);
        // Wrong pinned master for an otherwise-Active friend.
        ctx.friends
            .insert(f.sender_owner, ([0xAAu8; 32], FriendStatus::Active));
        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("pinned mismatch rejected");
        assert!(matches!(err, DepositReject::BadCert));
    }

    /// ZEB-418 P1 Task 8 cross-check: a frame produced by the SENDER-side
    /// `butler_deposit::build_deposit_frame` (the exact construction
    /// `IrohButlerDepositClient` ships) passes the FULL acceptor pipeline —
    /// cert strict-decode + Master-issuer + owner/master binding, frame sig
    /// over `domain ‖ ro ‖ sealed_blob`, seal opening under the butler
    /// device's birational X25519 + butler info string, inner CidNotify
    /// signature/sender consistency, and `ContentId::for_book` CID
    /// consistency with the storage blob. Pins sender↔butler wire
    /// compatibility end to end.
    #[tokio::test]
    async fn sender_built_frame_passes_acceptor_pipeline() {
        let f = valid_fixture();
        let so = sender();
        let payload = DepositPayload {
            cidnotify_packet: Some(f.cidnotify_packet.clone()),
            storage_blob: f.storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        };
        let butler_vk = butler_device_sk().verifying_key().to_bytes();
        let cert_bytes = harmony_owner::cbor::to_canonical(&so.cert).expect("encode cert");
        let frame = crate::butler_deposit::build_deposit_frame(
            &BUTLER_OWNER,
            &so.owner.0,
            &cert_bytes,
            &so.device_key,
            &butler_vk,
            &payload,
        )
        .expect("sender-side frame build must succeed");

        let ctx = TestCtx::for_fixture(&f);
        let ack = handle_deposit_core(&frame, &ctx)
            .await
            .expect("sender-built frame must pass every acceptor check");
        assert_eq!(ack.space_id, f.space_id.0);
        assert_eq!(ack.message_cid, f.message_cid.to_bytes().to_vec());

        // The entry the butler persisted carries exactly the payload the
        // sender sealed.
        let key = DmInboxDoc::key(&f.space_id.0, &f.message_cid.to_bytes());
        let store = ctx.store.lock().unwrap();
        let entry = store.get(&key).expect("entry persisted");
        assert_eq!(entry.cidnotify_packet, Some(f.cidnotify_packet));
        assert_eq!(entry.storage_blob, f.storage_blob);
    }

    #[tokio::test]
    async fn deposit_from_non_friend_rejected_before_any_crypto() {
        let f = valid_fixture();

        // Unknown sender with NO group co-membership: the friend lookup
        // misses, the group fallback (ZEB-424) is consulted and also misses,
        // then the deposit is rejected — the decrypt probe (and everything
        // after it) is never reached.
        let ctx = TestCtx {
            friends: BTreeMap::new(),
            ..TestCtx::for_fixture(&f)
        };
        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("non-friend deposit must be rejected");
        assert_eq!(err, DepositReject::NotAuthorized);
        assert_eq!(
            ctx.events(),
            vec!["friend_lookup".to_string(), "group_lookup".to_string()],
            "no crypto step (decrypt) and no persist may run for a non-authorized sender"
        );
        assert!(ctx.store.lock().unwrap().is_empty());

        // Pending / Revoked friends are NOT admitted by the friend path
        // (must be Active); with no group co-membership they fall through the
        // group fallback and reject.
        for status in [FriendStatus::Pending, FriendStatus::Revoked] {
            let mut friends = BTreeMap::new();
            friends.insert(f.sender_owner, (f.sender_master, status));
            let ctx = TestCtx {
                friends,
                ..TestCtx::for_fixture(&f)
            };
            let err = handle_deposit_core(&f.frame, &ctx)
                .await
                .expect_err("non-Active friend must be rejected");
            assert_eq!(err, DepositReject::NotAuthorized);
            assert_eq!(
                ctx.events(),
                vec!["friend_lookup".to_string(), "group_lookup".to_string()]
            );
        }

        // Wrong recipient: rejected before even the friend lookup (cheapest
        // local check first — also pre-crypto by construction).
        let mut wrong_recipient = f.frame.clone();
        wrong_recipient.recipient_owner = [0x99; 16];
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&wrong_recipient, &ctx)
            .await
            .expect_err("deposit for another owner must be rejected");
        assert_eq!(err, DepositReject::WrongRecipient);
        assert!(ctx.events().is_empty());
    }

    #[tokio::test]
    async fn deposit_with_bad_cert_or_bad_sig_rejected() {
        let f = valid_fixture();

        // (a) Garbage cert bytes → BadCert.
        let mut garbage_cert = f.frame.clone();
        garbage_cert.sender_enrollment_cert = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&garbage_cert, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::BadCert);
        assert!(
            !ctx.events().contains(&"decrypt".to_string()),
            "bad cert must reject before decrypt"
        );

        // (b) Someone ELSE's valid cert (wrong owner binding) → BadCert.
        let other = mint_test_owner(0x52);
        let mut wrong_owner_cert = f.frame.clone();
        wrong_owner_cert.sender_enrollment_cert =
            harmony_owner::cbor::to_canonical(&other.cert).expect("encode other cert");
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&wrong_owner_cert, &ctx)
            .await
            .unwrap_err();
        assert_eq!(err, DepositReject::BadCert);

        // (c) Cert is internally valid but its master key does not match the
        // friend graph's pinned master for this owner → BadCert.
        let mut friends = BTreeMap::new();
        friends.insert(f.sender_owner, ([0xAB; 32], FriendStatus::Active));
        let ctx = TestCtx {
            friends,
            ..TestCtx::for_fixture(&f)
        };
        let err = handle_deposit_core(&f.frame, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::BadCert);

        // (d) Tampered frame signature → BadSig.
        let mut tampered_sig = f.frame.clone();
        tampered_sig.sig[10] ^= 0xFF;
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&tampered_sig, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::BadSig);
        assert!(
            !ctx.events().contains(&"decrypt".to_string()),
            "bad sig must reject before decrypt"
        );

        // (e) Wrong-length signature → BadSig.
        let mut short_sig = f.frame.clone();
        short_sig.sig = vec![0x01; 12];
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&short_sig, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::BadSig);

        // (f) Signature over a DIFFERENT sealed blob (valid key, wrong
        // payload binding) → BadSig.
        let mut swapped_blob = f.frame.clone();
        swapped_blob.sealed_blob = seal_payload_bytes(b"some other plaintext entirely");
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&swapped_blob, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::BadSig);
    }

    /// ZEB-483 (CodeAnt): a redeposit upgrades a stored entry's MISSING bootstrap
    /// invite (`None → Some`) — healing a pre-ZEB-483 entry whose recovery would
    /// otherwise stay stuck at `SpaceNotFound` — but NEVER overwrites an invite
    /// already present (the insert-once exception).
    #[tokio::test]
    async fn redeposit_upgrades_missing_invite_but_never_overwrites() {
        let f = valid_fixture();
        let ctx = TestCtx::for_fixture(&f);
        let key = "space:cid".to_string();

        // 1. First deposit lacks the invite (pre-ZEB-483 shape).
        let e0 = filler_entry(f.sender_owner);
        assert!(e0.invite_packet.is_none());
        assert_eq!(
            ctx.persist_entry(key.clone(), e0).await.unwrap(),
            DepositPersistVerdict::Inserted
        );
        assert!(ctx.store.lock().unwrap()[&key].invite_packet.is_none());

        // 2. Redeposit carries the invite → Duplicate, stored entry healed.
        let mut e1 = filler_entry(f.sender_owner);
        e1.invite_packet = Some(vec![0x11, 0x22]);
        assert_eq!(
            ctx.persist_entry(key.clone(), e1).await.unwrap(),
            DepositPersistVerdict::Duplicate
        );
        assert_eq!(
            ctx.store.lock().unwrap()[&key].invite_packet.as_deref(),
            Some(&[0x11, 0x22][..]),
            "missing invite upgraded on redeposit"
        );

        // 3. A later redeposit with a DIFFERENT invite must NOT overwrite.
        let mut e2 = filler_entry(f.sender_owner);
        e2.invite_packet = Some(vec![0x99]);
        assert_eq!(
            ctx.persist_entry(key.clone(), e2).await.unwrap(),
            DepositPersistVerdict::Duplicate
        );
        assert_eq!(
            ctx.store.lock().unwrap()[&key].invite_packet.as_deref(),
            Some(&[0x11, 0x22][..]),
            "existing invite never overwritten"
        );
    }

    /// PR #221 round 1: caps are enforced at PERSIST level, atomically
    /// inside the store critical section — a NEW key at a full inbox gets
    /// `CapExceeded` and nothing is persisted.
    #[tokio::test]
    async fn per_sender_and_global_caps_enforced() {
        let f = valid_fixture();
        let key = DmInboxDoc::key(&f.space_id.0, &f.message_cid.to_bytes());

        // Per-sender cap: the store already holds CAP live entries from
        // this sender → CapExceeded, nothing persisted.
        let ctx = TestCtx::for_fixture(&f);
        {
            let mut store = ctx.store.lock().unwrap();
            for i in 0..INBOX_PER_SENDER_CAP {
                store.insert(format!("prefill-sender-{i}"), filler_entry(f.sender_owner));
            }
        }
        let err = handle_deposit_core(&f.frame, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::CapExceeded);
        {
            let store = ctx.store.lock().unwrap();
            assert_eq!(
                store.len(),
                INBOX_PER_SENDER_CAP,
                "CapExceeded must persist nothing"
            );
            assert!(!store.contains_key(&key));
        }

        // Global cap: CAP live entries from OTHER senders → CapExceeded
        // even though this sender has zero pending.
        let ctx = TestCtx::for_fixture(&f);
        {
            let mut store = ctx.store.lock().unwrap();
            for i in 0..INBOX_GLOBAL_CAP {
                store.insert(format!("prefill-other-{i}"), filler_entry([0xEE; 16]));
            }
        }
        let err = handle_deposit_core(&f.frame, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::CapExceeded);
        {
            let store = ctx.store.lock().unwrap();
            assert_eq!(store.len(), INBOX_GLOBAL_CAP);
            assert!(!store.contains_key(&key));
        }

        // One below the per-sender cap → accepted (boundary pin: the cap is
        // a strict "already at" check, not "would exceed by 2").
        let ctx = TestCtx::for_fixture(&f);
        {
            let mut store = ctx.store.lock().unwrap();
            for i in 0..INBOX_PER_SENDER_CAP - 1 {
                store.insert(format!("prefill-sender-{i}"), filler_entry(f.sender_owner));
            }
        }
        handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("one-below-cap deposit must be accepted");
        let store = ctx.store.lock().unwrap();
        assert_eq!(store.len(), INBOX_PER_SENDER_CAP);
        assert!(store.contains_key(&key));
    }

    /// PR #221 round 1: an occupied key BYPASSES the caps — a redelivery
    /// after a lost ack must re-ack idempotently even at a full inbox (the
    /// entry is already stored; rejecting would strand the sender
    /// undelivered for a message the butler holds).
    #[tokio::test]
    async fn duplicate_deposit_at_full_inbox_still_acks() {
        let f = valid_fixture();
        let ctx = TestCtx::for_fixture(&f);

        // First delivery lands normally...
        let ack1 = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("first deposit accepted");

        // ...then the inbox fills to the GLOBAL cap with other senders'
        // entries.
        {
            let mut store = ctx.store.lock().unwrap();
            let mut i = 0;
            while store.len() < INBOX_GLOBAL_CAP {
                store.insert(format!("prefill-other-{i}"), filler_entry([0xEE; 16]));
                i += 1;
            }
        }

        // Redelivery of the EXISTING key (lost-ack retry) at the full
        // inbox: Duplicate verdict path → still acked, no growth.
        let ack2 = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("redelivered deposit at a full inbox must still be acked");
        assert_eq!(ack1, ack2, "duplicate ack must be identical");
        let store = ctx.store.lock().unwrap();
        assert_eq!(store.len(), INBOX_GLOBAL_CAP, "no growth at the cap");
        let key = DmInboxDoc::key(&f.space_id.0, &f.message_cid.to_bytes());
        assert!(store.contains_key(&key));
    }

    /// PR #221 round 1 (device→owner binding): the inner packet's signing
    /// device resolves to a DIFFERENT owner than `frame.sender_owner` —
    /// the deposit must be rejected and never persisted, because ingestion
    /// (which reuses the normal receive path's owner-field match) would
    /// reject it forever: persisting+acking would tell the sender
    /// "delivered" for a message the recipient never sees.
    #[tokio::test]
    async fn deposit_from_device_of_wrong_owner_rejected() {
        let f = valid_fixture();
        let mut device_owners = BTreeMap::new();
        // Same device hash + identity pub, but owned by another owner.
        device_owners.insert(f.dm_device_hash, ([0xEE; 16], f.identity_pub));
        let ctx = TestCtx {
            device_owners,
            ..TestCtx::for_fixture(&f)
        };

        let err = handle_deposit_core(&f.frame, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::InnerVerifyFailed);
        assert!(ctx.store.lock().unwrap().is_empty(), "must not persist");
        assert!(
            !ctx.events().iter().any(|e| e.starts_with("persist")),
            "persist must never be called for a wrong-owner device"
        );
    }

    #[tokio::test]
    async fn duplicate_deposit_same_key_acks_without_second_entry() {
        let f = valid_fixture();
        let ctx = TestCtx::for_fixture(&f);

        let ack1 = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("first deposit accepted");
        let ack2 = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("redelivered deposit must still be acked (lost-ack retry)");
        assert_eq!(ack1, ack2, "duplicate ack must be identical");

        let store = ctx.store.lock().unwrap();
        assert_eq!(store.len(), 1, "insert-once: no second entry for the key");
        let key = DmInboxDoc::key(&f.space_id.0, &f.message_cid.to_bytes());
        assert!(store.contains_key(&key));
    }

    #[tokio::test]
    async fn inner_cidnotify_failing_verification_rejected_not_persisted() {
        let f = valid_fixture();

        // Helper: re-frame the fixture with a replacement payload (re-seal +
        // re-sign so the OUTER frame stays fully valid — only the inner
        // verification may reject).
        let reframe = |payload: &DepositPayload| -> DepositFrame {
            let so = sender();
            let payload_bytes = encode_deposit_payload(payload).expect("encode payload");
            let sealed = seal_payload_bytes(&payload_bytes);
            let sig = sign_frame(&BUTLER_OWNER, &sealed, &so.device_key);
            DepositFrame {
                sealed_blob: sealed,
                sig,
                ..f.frame.clone()
            }
        };

        // (a) Tampered inner packet signature (flip a byte in the 64-byte
        // sig tail) → InnerVerifyFailed.
        let mut tampered_packet = f.cidnotify_packet.clone();
        let last = tampered_packet.len() - 1;
        tampered_packet[last] ^= 0xFF;
        let frame = reframe(&DepositPayload {
            cidnotify_packet: Some(tampered_packet),
            storage_blob: f.storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        });
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&frame, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::InnerVerifyFailed);
        assert!(ctx.store.lock().unwrap().is_empty(), "must not persist");

        // (b) CID mismatch: inner packet's message_cid does not match the
        // deposited storage blob's for_book CID → InnerVerifyFailed.
        let other_cid = ContentId::for_book(
            b"a different blob",
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("cid");
        let (mismatch_packet, _, _) =
            build_cidnotify(OwnerAddr(f.sender_owner), f.space_id, other_cid);
        let frame = reframe(&DepositPayload {
            cidnotify_packet: Some(mismatch_packet),
            storage_blob: f.storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        });
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&frame, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::InnerVerifyFailed);
        assert!(ctx.store.lock().unwrap().is_empty());

        // (c) Unknown signing device (no cached device→owner binding) →
        // InnerVerifyFailed.
        let ctx = TestCtx {
            device_owners: BTreeMap::new(),
            ..TestCtx::for_fixture(&f)
        };
        let err = handle_deposit_core(&f.frame, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::InnerVerifyFailed);
        assert!(ctx.store.lock().unwrap().is_empty());

        // (d) Inner packet claims a DIFFERENT sender owner than the
        // admission-checked frame sender → InnerVerifyFailed.
        let (foreign_packet, _, _) =
            build_cidnotify(OwnerAddr([0xEE; 16]), f.space_id, f.message_cid);
        let frame = reframe(&DepositPayload {
            cidnotify_packet: Some(foreign_packet),
            storage_blob: f.storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        });
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&frame, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::InnerVerifyFailed);
        assert!(ctx.store.lock().unwrap().is_empty());

        // (e) Sealed plaintext is not a DepositPayload at all → BadPayload.
        let so = sender();
        let sealed = seal_payload_bytes(b"\xff\xff not cbor at all");
        let sig = sign_frame(&BUTLER_OWNER, &sealed, &so.device_key);
        let frame = DepositFrame {
            sealed_blob: sealed,
            sig,
            ..f.frame.clone()
        };
        let ctx = TestCtx::for_fixture(&f);
        let err = handle_deposit_core(&frame, &ctx).await.unwrap_err();
        assert_eq!(err, DepositReject::BadPayload);
        assert!(ctx.store.lock().unwrap().is_empty());
    }

    #[test]
    fn shares_live_group_dm_in_matches_only_live_group_with_both_members() {
        use crate::owner_state_crdt::OwnerState;
        use crate::owner_state_types::{Hlc, OwnerAddr, Space, SpaceId, SpaceKind};

        let me = [0x11u8; 16];
        let peer = [0x22u8; 16];
        let stranger = [0x33u8; 16];

        let hlc = Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "t".into(),
        };
        let mk_space = |id: u8, kind: SpaceKind, members: Vec<[u8; 16]>, left: Option<Hlc>| Space {
            id: SpaceId([id; 16]),
            kind,
            parent: None,
            community_id: None,
            name: "g".into(),
            transport: None,
            members: members.into_iter().map(OwnerAddr).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: left,
            created_at: hlc.clone(),
            updated_at: hlc.clone(),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };

        let mut state = OwnerState::default();
        let s_live = mk_space(0x01, SpaceKind::GroupDm, vec![me, peer], None);
        state.spaces.insert(s_live.id, s_live);
        assert!(
            shares_live_group_dm_in(&state, &me, &peer),
            "live group, both members"
        );
        assert!(
            !shares_live_group_dm_in(&state, &me, &stranger),
            "stranger not a member"
        );

        let mut state_left = OwnerState::default();
        let s_left = mk_space(0x02, SpaceKind::GroupDm, vec![me, peer], Some(hlc.clone()));
        state_left.spaces.insert(s_left.id, s_left);
        assert!(
            !shares_live_group_dm_in(&state_left, &me, &peer),
            "left group does not match"
        );

        let mut state_dm = OwnerState::default();
        let s_dm = mk_space(0x03, SpaceKind::Dm, vec![me, peer], None);
        state_dm.spaces.insert(s_dm.id, s_dm);
        assert!(
            !shares_live_group_dm_in(&state_dm, &me, &peer),
            "Dm kind does not match"
        );

        let mut state_noself = OwnerState::default();
        let s_noself = mk_space(0x04, SpaceKind::GroupDm, vec![peer, stranger], None);
        state_noself.spaces.insert(s_noself.id, s_noself);
        assert!(
            !shares_live_group_dm_in(&state_noself, &me, &peer),
            "self must be a member too"
        );
    }

    /// ZEB-424 D28.1: the post-decrypt bind matches ONLY the named space when
    /// it is a live `GroupDm` with both owners — and, crucially, rejects a
    /// DIFFERENT space even when the sender DOES share some other live group
    /// DM (the exact-space property the "shares any" pre-decrypt gate lacks).
    #[test]
    fn space_is_live_group_dm_co_member_in_binds_the_named_space_only() {
        use crate::owner_state_crdt::OwnerState;
        use crate::owner_state_types::{Hlc, OwnerAddr, Space, SpaceId, SpaceKind};

        let me = [0x11u8; 16];
        let peer = [0x22u8; 16];
        let stranger = [0x33u8; 16];
        let hlc = Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "t".into(),
        };
        let mk_space = |id: u8, kind: SpaceKind, members: Vec<[u8; 16]>, left: Option<Hlc>| Space {
            id: SpaceId([id; 16]),
            kind,
            parent: None,
            community_id: None,
            name: "g".into(),
            transport: None,
            members: members.into_iter().map(OwnerAddr).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: left,
            created_at: hlc.clone(),
            updated_at: hlc.clone(),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };

        // A live shared GroupDm (id 0x01) AND a second live GroupDm the sender
        // is NOT in (id 0x05) — proving "shares any" is insufficient.
        let mut state = OwnerState::default();
        for s in [
            mk_space(0x01, SpaceKind::GroupDm, vec![me, peer], None),
            mk_space(0x05, SpaceKind::GroupDm, vec![me, stranger], None),
            mk_space(0x02, SpaceKind::GroupDm, vec![me, peer], Some(hlc.clone())), // left
            mk_space(0x03, SpaceKind::Dm, vec![me, peer], None),                   // Dm kind
        ] {
            state.spaces.insert(s.id, s);
        }

        // Named live GroupDm with both members → admitted.
        assert!(space_is_live_group_dm_co_member_in(
            &state,
            &me,
            &peer,
            &[0x01; 16]
        ));
        // The sender shares 0x01, but the deposit names 0x05 (sender absent) →
        // rejected. This is the exact-space bind the DoS fix turns on.
        assert!(!space_is_live_group_dm_co_member_in(
            &state,
            &me,
            &peer,
            &[0x05; 16]
        ));
        // Left group → rejected.
        assert!(!space_is_live_group_dm_co_member_in(
            &state,
            &me,
            &peer,
            &[0x02; 16]
        ));
        // Dm kind → rejected.
        assert!(!space_is_live_group_dm_co_member_in(
            &state,
            &me,
            &peer,
            &[0x03; 16]
        ));
        // Unknown space id → rejected.
        assert!(!space_is_live_group_dm_co_member_in(
            &state,
            &me,
            &peer,
            &[0xAB; 16]
        ));
    }

    // ── ZEB-691 (Task B4): revocation-deposit acceptor tests ────────────────

    /// Build a master-signed `RevocationPush` wire packet revoking one of the
    /// `master_seed` owner's devices. `master_seed == 0x51` matches the fixture
    /// `sender()` (`mint_test_owner(0x51)`) master, so the revocation binds to
    /// `frame.sender_owner`; any OTHER seed forges a THIRD-PARTY revocation whose
    /// `owner_id != frame.sender_owner` (the relay-a-stranger's-revocation
    /// attack the trust-bind must reject). Returns the wire bytes + the revoked
    /// device's `target` id (the second half of `revoke_key`). Mirrors
    /// `dm_outbox::tests::sample_revocation_case` — no hand-rolled cert crypto.
    fn revocation_wire(master_seed: u8, target_device_seed: u8) -> (Vec<u8>, [u8; 16]) {
        use crate::dm_envelope::build_revocation_push_packet;
        use harmony_owner::certs::{EnrollmentCert, RevocationCert, RevocationReason};
        use harmony_owner::pubkey_bundle::PubKeyBundle;

        let master_sk = SigningKey::from_bytes(&[master_seed; 32]);
        let master_bundle = PubKeyBundle::classical_only(master_sk.verifying_key().to_bytes());
        let target_sk = SigningKey::from_bytes(&[target_device_seed; 32]);
        let target_bundle = PubKeyBundle::classical_only(target_sk.verifying_key().to_bytes());
        let target_device_id = target_bundle.identity_hash();
        let now = 1_700_000_000u64;
        let enrollment = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            target_device_id,
            target_bundle,
            now,
            None,
        )
        .expect("enrollment sign");
        let revocation = RevocationCert::sign_master(
            &master_sk,
            master_bundle,
            target_device_id,
            now,
            RevocationReason::Compromised,
        )
        .expect("revocation sign");
        let packet = build_revocation_push_packet(revocation, enrollment);
        let wire = encode_packet(&packet).expect("encode revocation push");
        (wire, target_device_id)
    }

    /// A deposit frame carrying ONLY a signed `RevocationPush` — no CidNotify, no
    /// invite. The OUTER frame is a normal, fully valid `sender()` deposit (so
    /// steps 0–4 pass unchanged); `storage_blob` lets a test inject an illegal
    /// non-empty blob to exercise the fail-closed blob check, and `invite_packet`
    /// lets a test inject an illegal non-empty invite to exercise the fail-closed
    /// invite check.
    fn revocation_fixture(
        rp_wire: Vec<u8>,
        storage_blob: Vec<u8>,
        invite_packet: Option<Vec<u8>>,
    ) -> Fixture {
        let so = sender();
        // space_id / message_cid are unused by the revocation path, but the
        // shared `Fixture` shape carries them; keep them well-formed.
        let space_id = SpaceId([0x77; 16]);
        let message_cid = ContentId::for_book(
            b"unused-by-revocation",
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("cid");
        let (_dm_sk, identity_pub, dm_device_hash) = dm_identity();
        let payload = DepositPayload {
            cidnotify_packet: None,
            storage_blob: storage_blob.clone(),
            invite_packet,
            revocation_push: Some(rp_wire),
            grant_push: None,
        };
        let payload_bytes = encode_deposit_payload(&payload).expect("encode payload");
        let sealed = seal_payload_bytes(&payload_bytes);
        let sig = sign_frame(&BUTLER_OWNER, &sealed, &so.device_key);
        let cert_bytes = harmony_owner::cbor::to_canonical(&so.cert).expect("encode cert");
        Fixture {
            frame: DepositFrame {
                signer_certs_cbor: Vec::new(),
                recipient_owner: BUTLER_OWNER,
                sender_owner: so.owner.0,
                sender_enrollment_cert: cert_bytes,
                sig,
                sealed_blob: sealed,
            },
            sender_owner: so.owner.0,
            sender_master: master_from_cert(&so.cert),
            space_id,
            message_cid,
            cidnotify_packet: Vec::new(),
            storage_blob,
            dm_device_hash,
            identity_pub,
        }
    }

    /// A revocation-only deposit from an Active friend is persisted under
    /// `revoke:{sender_owner_hex}:{target_hex}` and acked with the
    /// `REVOCATION_DEPOSIT_MARKER` (no space, no message CID).
    #[tokio::test]
    async fn handle_deposit_core_persists_revocation_under_revoke_key() {
        // master 0x51 == sender()'s master → revocation binds to frame.sender_owner.
        let (rp_wire, target) = revocation_wire(0x51, 0x71);
        let f = revocation_fixture(rp_wire.clone(), Vec::new(), None);
        let ctx = TestCtx::for_fixture(&f);

        let ack = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("valid revocation deposit from active friend must be accepted");

        // Ack: zero space + the revocation marker.
        assert_eq!(ack.space_id, [0u8; 16]);
        assert_eq!(ack.message_cid, REVOCATION_DEPOSIT_MARKER.to_vec());

        // Persisted under EXACTLY revoke:{sender_hex}:{target_hex}.
        let key = DmInboxDoc::revoke_key(&f.sender_owner, &target);
        let store = ctx.store.lock().unwrap();
        let entry = store.get(&key).expect("entry persisted under revoke_key");
        assert_eq!(entry.sender_owner, f.sender_owner);
        assert_eq!(
            entry.revocation_push,
            Some(rp_wire),
            "the signed RevocationPush is carried into the persisted entry verbatim"
        );
        assert!(entry.cidnotify_packet.is_none(), "no message half");
        assert!(entry.invite_packet.is_none(), "no invite half");
        assert!(entry.storage_blob.is_empty(), "no storage blob");
    }

    /// D7 trust boundary: an AUTHENTICATED friend deposits a revocation signed by
    /// a DIFFERENT master (`revocation.owner_id != frame.sender_owner`) — relaying
    /// a third party's revocation. The butler PRE-VALIDATES and fails closed with
    /// `InnerVerifyFailed`, and — crucially — NOTHING is persisted (never ack a
    /// forgery).
    #[tokio::test]
    async fn handle_deposit_core_rejects_forged_revocation() {
        // master 0x71 != sender()'s master (0x51) → owner-field mismatch inside
        // verify_revocation_push (the revocation is validly master-signed, just
        // not by the depositing friend's master).
        let (rp_wire, _target) = revocation_wire(0x71, 0x33);
        let f = revocation_fixture(rp_wire, Vec::new(), None);
        let ctx = TestCtx::for_fixture(&f);

        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("a revocation the depositing friend does not own must be rejected");
        assert_eq!(err, DepositReject::InnerVerifyFailed);

        // Nothing persisted, and persist was never even reached (the mock records
        // "persist:<key>" only AFTER a durable write).
        assert!(
            ctx.store.lock().unwrap().is_empty(),
            "a forged revocation must never be persisted"
        );
        assert!(
            !ctx.events().iter().any(|e| e.starts_with("persist")),
            "persist must never run for a forged revocation: {:?}",
            ctx.events()
        );
    }

    /// A revocation deposit carries no message, so any `storage_blob` is unused
    /// bytes an admitted sender could attach to waste inbox storage — rejected
    /// fail-closed (`BadPayload`) and nothing persisted. Uses a well-formed,
    /// sender-owned revocation so the ONLY defect is the blob.
    #[tokio::test]
    async fn handle_deposit_core_rejects_revocation_with_blob() {
        let (rp_wire, _target) = revocation_wire(0x51, 0x71);
        let f = revocation_fixture(rp_wire, b"unexpected-storage-blob".to_vec(), None);
        let ctx = TestCtx::for_fixture(&f);

        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("a revocation deposit with a non-empty storage blob must be rejected");
        assert_eq!(err, DepositReject::BadPayload);
        assert!(
            ctx.store.lock().unwrap().is_empty(),
            "nothing persisted on a blob-carrying revocation"
        );
    }

    /// ZEB-691 whole-branch-review finding: a pure revocation deposit carries no
    /// invite either — an `invite_packet` alongside `revocation_push` is unused
    /// bytes an admitted sender could attach, AND (worse than mere waste) it
    /// would make the persisted entry match BOTH `revocation_push.is_some()` and
    /// `invite_packet.is_some()`, so the recipient's pure-revocation dispatch
    /// guard (`revocation_push.is_some() && cidnotify_packet.is_none() &&
    /// invite_packet.is_none()`) fails to match and the revocation is silently
    /// mis-routed/dropped. Reject fail-closed at the butler, symmetric with the
    /// blob check above, and confirm nothing is persisted.
    #[tokio::test]
    async fn handle_deposit_core_rejects_revocation_with_invite() {
        let (rp_wire, _target) = revocation_wire(0x51, 0x71);
        let f = revocation_fixture(
            rp_wire,
            Vec::new(),
            Some(b"unexpected-invite-bytes".to_vec()),
        );
        let ctx = TestCtx::for_fixture(&f);

        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("a revocation deposit with a non-empty invite_packet must be rejected");
        assert_eq!(err, DepositReject::BadPayload);
        assert!(
            ctx.store.lock().unwrap().is_empty(),
            "nothing persisted on an invite-carrying revocation"
        );
    }

    /// ZEB-691 converge (Qodo, security): revocations are FRIEND-scoped by
    /// design — the send side (`push_revocation_to_friends`) only deposits
    /// to ACTIVE friends, and this persists into the friend-scoped
    /// `revoked_dm_devices` CRDT. A well-formed, sender-owned revocation
    /// admitted only as a live group-DM `CoMember` (NOT an Active friend)
    /// must be rejected `NotAuthorizedForScope` (admitted sender, friend-scoped
    /// operation — not the ZEB-702 roster-miss signal), and nothing persisted — mirrors
    /// `deposit_from_non_friend_group_co_member_is_accepted`'s admission
    /// setup, but for the revocation arm where co-member admission must NOT
    /// be sufficient.
    #[tokio::test]
    async fn handle_deposit_core_rejects_comember_revocation() {
        let (rp_wire, _target) = revocation_wire(0x51, 0x71);
        let f = revocation_fixture(rp_wire, Vec::new(), None);
        let mut ctx = TestCtx::for_fixture(&f);
        ctx.friends.clear();
        ctx.group_co_members.insert(f.sender_owner);

        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("a co-member (non-friend) must not be able to deposit a revocation");
        assert_eq!(err, DepositReject::NotAuthorizedForScope);
        assert!(
            ctx.store.lock().unwrap().is_empty(),
            "nothing persisted for a co-member-admitted revocation"
        );
    }

    /// CodeRabbit: mirrors `deposit_with_oversized_invite_is_rejected` — a
    /// revocation-only deposit whose `revocation_push` exceeds
    /// `MAX_DEPOSIT_INVITE_BYTES` is rejected `BadPayload`, and nothing is
    /// persisted. Uses an Active-friend admission (the default) so the ONLY
    /// defect under test is the oversized payload.
    #[tokio::test]
    async fn handle_deposit_core_rejects_oversized_revocation() {
        let f = revocation_fixture(vec![0u8; MAX_DEPOSIT_INVITE_BYTES + 1], Vec::new(), None);
        let ctx = TestCtx::for_fixture(&f);

        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("oversized revocation_push must be rejected");
        assert_eq!(err, DepositReject::BadPayload);
        assert!(
            ctx.store.lock().unwrap().is_empty(),
            "nothing persisted on an oversized revocation"
        );
    }

    // ── ZEB-674 (C4): file-share grant-deposit acceptor tests ────────────────

    /// A deposit frame carrying ONLY an opaque `grant_push` — no CidNotify, no
    /// invite, no revocation. The OUTER frame is a normal, fully valid `sender()`
    /// deposit (steps 0–4 pass unchanged); `storage_blob` / `invite_packet` let a
    /// test inject illegal extra bytes to exercise the fail-closed pure-shape
    /// guards.
    fn grant_fixture(
        grant_push: Vec<u8>,
        storage_blob: Vec<u8>,
        invite_packet: Option<Vec<u8>>,
    ) -> Fixture {
        let so = sender();
        let space_id = SpaceId([0x77; 16]);
        let message_cid = ContentId::for_book(
            b"unused-by-grant",
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("cid");
        let (_dm_sk, identity_pub, dm_device_hash) = dm_identity();
        let payload = DepositPayload {
            cidnotify_packet: None,
            storage_blob: storage_blob.clone(),
            invite_packet,
            revocation_push: None,
            grant_push: Some(grant_push),
        };
        let payload_bytes = encode_deposit_payload(&payload).expect("encode payload");
        let sealed = seal_payload_bytes(&payload_bytes);
        let sig = sign_frame(&BUTLER_OWNER, &sealed, &so.device_key);
        let cert_bytes = harmony_owner::cbor::to_canonical(&so.cert).expect("encode cert");
        Fixture {
            frame: DepositFrame {
                signer_certs_cbor: Vec::new(),
                recipient_owner: BUTLER_OWNER,
                sender_owner: so.owner.0,
                sender_enrollment_cert: cert_bytes,
                sig,
                sealed_blob: sealed,
            },
            sender_owner: so.owner.0,
            sender_master: master_from_cert(&so.cert),
            space_id,
            message_cid,
            cidnotify_packet: Vec::new(),
            storage_blob,
            dm_device_hash,
            identity_pub,
        }
    }

    /// A grant-only deposit from an Active friend is ACCEPTED (not `BadPayload`,
    /// the pre-ZEB-674 verdict for a no-cidnotify/no-invite/no-revocation
    /// deposit), persisted under `grant:{sender_hex}:{hash_hex}`, projects
    /// `grant_push` onto the entry verbatim, preserves the butler-verified
    /// `sender_owner` (the authenticated granter), and acks with the grant marker.
    #[tokio::test]
    async fn handle_deposit_core_accepts_grant_and_projects_grant_push() {
        let grant_push = b"opaque-per-device-sealed-grant-blobs".to_vec();
        let f = grant_fixture(grant_push.clone(), Vec::new(), None);
        let ctx = TestCtx::for_fixture(&f);

        let ack = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect("valid grant deposit from active friend must be accepted");

        // Ack: zero space + the grant marker (no message CID).
        assert_eq!(ack.space_id, [0u8; 16]);
        assert_eq!(ack.message_cid, GRANT_DEPOSIT_MARKER.to_vec());

        // Persisted under EXACTLY grant:{sender_hex}:{hash_hex}, with grant_push
        // projected and the authenticated sender preserved.
        let key = DmInboxDoc::grant_key(&f.sender_owner, &grant_push);
        let store = ctx.store.lock().unwrap();
        let entry = store.get(&key).expect("entry persisted under grant_key");
        assert_eq!(
            entry.sender_owner, f.sender_owner,
            "the butler-verified granter is preserved as the entry sender"
        );
        assert_eq!(
            entry.grant_push,
            Some(grant_push),
            "grant_push carried into the persisted entry verbatim"
        );
        assert!(entry.cidnotify_packet.is_none(), "no message half");
        assert!(entry.invite_packet.is_none(), "no invite half");
        assert!(entry.revocation_push.is_none(), "no revocation half");
        assert!(entry.storage_blob.is_empty(), "no storage blob");
    }

    /// A grant deposit carries no message — a non-empty `storage_blob` is unused
    /// bytes an admitted sender could attach; rejected fail-closed and nothing
    /// persisted (mirrors the revocation/invite blob guards).
    #[tokio::test]
    async fn handle_deposit_core_rejects_grant_with_blob() {
        let f = grant_fixture(b"grant".to_vec(), b"unexpected-blob".to_vec(), None);
        let ctx = TestCtx::for_fixture(&f);

        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("a grant deposit with a non-empty storage blob must be rejected");
        assert_eq!(err, DepositReject::BadPayload);
        assert!(ctx.store.lock().unwrap().is_empty(), "nothing persisted");
    }

    /// A grant riding alongside an invite would make the persisted entry match
    /// two dispatch guards; reject fail-closed to keep the grant shape pure
    /// (mirrors the revocation invite guard).
    #[tokio::test]
    async fn handle_deposit_core_rejects_grant_with_invite() {
        let f = grant_fixture(
            b"grant".to_vec(),
            Vec::new(),
            Some(b"stray-invite".to_vec()),
        );
        let ctx = TestCtx::for_fixture(&f);

        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("a grant deposit carrying an invite must be rejected");
        assert_eq!(err, DepositReject::BadPayload);
        assert!(ctx.store.lock().unwrap().is_empty(), "nothing persisted");
    }

    /// A grant riding alongside a MESSAGE (CidNotify present) is rejected
    /// fail-closed — the grant is a pure shape, so it cannot piggyback on a
    /// message deposit (the recipient's grant guard requires cidnotify absent).
    #[tokio::test]
    async fn handle_deposit_core_rejects_message_with_grant() {
        let f = valid_fixture();
        // Re-seal the fixture's payload with a stray grant_push added.
        let so = sender();
        let payload = DepositPayload {
            cidnotify_packet: Some(f.cidnotify_packet.clone()),
            storage_blob: f.storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: Some(b"stray-grant".to_vec()),
        };
        let payload_bytes = encode_deposit_payload(&payload).expect("encode");
        let sealed = seal_payload_bytes(&payload_bytes);
        let sig = sign_frame(&BUTLER_OWNER, &sealed, &so.device_key);
        let mut frame = f.frame.clone();
        frame.sealed_blob = sealed;
        frame.sig = sig;
        let ctx = TestCtx::for_fixture(&f);

        let err = handle_deposit_core(&frame, &ctx)
            .await
            .expect_err("a message deposit carrying a stray grant_push must be rejected");
        assert_eq!(err, DepositReject::BadPayload);
        assert!(ctx.store.lock().unwrap().is_empty(), "nothing persisted");
    }

    /// A grant whose `grant_push` exceeds `MAX_DEPOSIT_GRANT_BYTES` is rejected
    /// `BadPayload`, nothing persisted (mirrors the oversized-invite guard).
    #[tokio::test]
    async fn handle_deposit_core_rejects_oversized_grant() {
        let f = grant_fixture(vec![0u8; MAX_DEPOSIT_GRANT_BYTES + 1], Vec::new(), None);
        let ctx = TestCtx::for_fixture(&f);

        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("oversized grant_push must be rejected");
        assert_eq!(err, DepositReject::BadPayload);
        assert!(ctx.store.lock().unwrap().is_empty(), "nothing persisted");
    }

    /// ZEB-674 converge (Qodo, security): a grant-only deposit from a live
    /// group-DM CO-MEMBER who is NOT a friend is rejected
    /// `NotAuthorizedForScope`, nothing persisted. File-share grants are
    /// friend-scoped — the send side only deposits to active friends and the
    /// recipient persists into the friend-scoped `received_file_grants` — so a
    /// mere co-member (admitted for message/invite deposits) must not be able to
    /// inject a grant into another owner's received-grants set. Mirrors the
    /// revocation branch's Friend-only guard.
    #[tokio::test]
    async fn handle_deposit_core_rejects_grant_from_co_member() {
        let f = grant_fixture(b"opaque-per-device-sealed-grant".to_vec(), Vec::new(), None);
        let mut ctx = TestCtx::for_fixture(&f);
        // NOT a friend, but a live group-DM co-member of the deposit's own space,
        // so admission SUCCEEDS via the co-member fallback — the rejection is
        // purely the grant scope guard, not a failed admission.
        ctx.friends.clear();
        ctx.group_co_members.insert(f.sender_owner);
        ctx.group_co_member_spaces
            .insert((f.space_id.0, f.sender_owner));

        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("a grant deposit from a non-friend co-member must be rejected");
        assert_eq!(err, DepositReject::NotAuthorizedForScope);
        assert!(ctx.store.lock().unwrap().is_empty(), "nothing persisted");
    }

    // ==================================================================
    // ZEB-702 Task 4: butler-deposit decision counters + rate-limited WARN
    //
    // The shell (`IrohButlerDepositAcceptor::handle_connection`) classifies a
    // `handle_deposit_core` outcome into `ButlerDepositStats`. The shell needs
    // a live iroh `Connection` (not unit-testable here), so these tests drive
    // the SAME classification the shell performs: run the real pipeline to get
    // the outcome, then feed it to the stats recorder.
    // ==================================================================

    /// (a)+(b): an unauthorized deposit increments `rejected_unauthorized` and
    /// fires exactly one WARN; a second unauthorized reject inside the window
    /// bumps the counter to 2 but emits NO second WARN; once the window elapses
    /// the next unauthorized reject warns again. The clock is injected (mirrors
    /// `reachability_resolver`'s swappable clock) so the window is deterministic.
    #[tokio::test]
    async fn unauthorized_deposit_counts_and_warns_rate_limited() {
        // A real unauthorized outcome from the pipeline: the fixture's sender is
        // neither an Active friend nor a live group-DM co-member.
        let f = valid_fixture();
        let mut ctx = TestCtx::for_fixture(&f);
        ctx.friends.clear();
        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("unauthorized rejected");
        assert!(matches!(err, DepositReject::NotAuthorized), "got {err:?}");

        const T0: u64 = 1_700_000_000_000;
        let clock = Arc::new(AtomicU64::new(T0));
        let c2 = Arc::clone(&clock);
        let stats = ButlerDepositStats::with_clock(Arc::new(move || c2.load(Ordering::SeqCst)));

        // (a) first unauthorized reject → counter 1, one WARN.
        stats.record_rejected(&err);
        assert_eq!(stats.snapshot().rejected_unauthorized, 1);
        assert_eq!(stats.warn_emissions(), 1, "first unauthorized reject warns");

        // (b) second within the window (Δ = 30 s < 60 s) → counter 2, no 2nd WARN.
        clock.store(T0 + 30_000, Ordering::SeqCst);
        stats.record_rejected(&err);
        assert_eq!(stats.snapshot().rejected_unauthorized, 2);
        assert_eq!(
            stats.warn_emissions(),
            1,
            "no second WARN inside the window"
        );

        // window elapsed (Δ = 61 s > 60 s) → warns again.
        clock.store(T0 + 61_000, Ordering::SeqCst);
        stats.record_rejected(&err);
        assert_eq!(stats.snapshot().rejected_unauthorized, 3);
        assert_eq!(stats.warn_emissions(), 2, "window reopened → WARN again");

        // The reject-only path never touched accepted / rejected_other.
        let s = stats.snapshot();
        assert_eq!(s.accepted, 0);
        assert_eq!(s.rejected_other, 0);
    }

    /// ZEB-702 (PR #481 review): a post-admission scope reject — an ADMITTED
    /// co-member failing the space bind — counts as `rejected_other` and never
    /// fires the roster-divergence WARN. Splitting this class out of
    /// `rejected_unauthorized` keeps the ZEB-702 signal meaningful: only true
    /// roster misses indicate owner-state sync failure.
    #[tokio::test]
    async fn scope_reject_counts_other_never_warns() {
        // A real scope outcome from the pipeline: admitted via a shared live
        // group, but the deposit's own space is not live-shared.
        let f = valid_fixture();
        let mut ctx = TestCtx::for_fixture(&f);
        ctx.friends.clear();
        ctx.group_co_members.insert(f.sender_owner);
        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("scope-bound reject");
        assert!(
            matches!(err, DepositReject::NotAuthorizedForScope),
            "got {err:?}"
        );

        let stats = ButlerDepositStats::new();
        stats.record_rejected(&err);
        let s = stats.snapshot();
        assert_eq!(s.rejected_other, 1, "scope reject is rejected_other");
        assert_eq!(
            s.rejected_unauthorized, 0,
            "scope reject is NOT the roster signal"
        );
        assert_eq!(stats.warn_emissions(), 0, "scope reject never warns");
    }

    /// (c): an accepted deposit increments `accepted` only — no reject counter,
    /// no WARN.
    #[tokio::test]
    async fn accepted_deposit_counts_accepted_only() {
        let f = valid_fixture();
        let ctx = TestCtx::for_fixture(&f);
        handle_deposit_core(&f.frame, &ctx).await.expect("accepted");

        let stats = ButlerDepositStats::new();
        stats.record_accepted();

        let s = stats.snapshot();
        assert_eq!(s.accepted, 1);
        assert_eq!(s.rejected_unauthorized, 0);
        assert_eq!(s.rejected_other, 0);
        assert_eq!(stats.warn_emissions(), 0, "the accept path never warns");
    }

    /// A non-authorization reject (bad cert, cap, malformed, …) lands in
    /// `rejected_other` and never warns — the WARN is reserved for the
    /// roster-sync signal (`NotAuthorized`).
    #[test]
    fn other_reject_counts_rejected_other_no_warn() {
        let stats = ButlerDepositStats::new();
        stats.record_rejected(&DepositReject::BadCert);
        stats.record_rejected(&DepositReject::CapExceeded);
        let s = stats.snapshot();
        assert_eq!(s.rejected_other, 2);
        assert_eq!(s.rejected_unauthorized, 0);
        assert_eq!(s.accepted, 0);
        assert_eq!(stats.warn_emissions(), 0);
    }
}
