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
use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};
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
    /// `frame.sender_owner` is not an `Active` friend (unknown, `Pending`,
    /// or `Revoked` — all collapse to the same reject).
    #[error("sender is not an active friend")]
    NotFriend,
    /// The embedded `EnrollmentCert` failed to decode, failed verification,
    /// is not Master-issued, or does not bind to the friend-graph identity
    /// (owner id / pinned master key mismatch).
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

    // Step 1 — admission (spec §4 D5): sender must be an Active friend. This
    // runs FIRST among the identity checks because the cert in step 2 can
    // only be trusted against the friend graph's pinned master key. No
    // crypto has touched attacker-controlled bytes yet.
    let (friend_master, status) = ctx
        .lookup_friend(&frame.sender_owner)
        .await
        .ok_or(DepositReject::NotFriend)?;
    if status != FriendStatus::Active {
        return Err(DepositReject::NotFriend);
    }

    // Step 2 — decode + verify the sender device's EnrollmentCert and bind
    // it to the friend-graph identity: internally valid, Master-issued,
    // owner id == frame.sender_owner, and the issuing master key == the
    // friend graph's pinned master (the actual trust anchor).
    let cert = decode_enrollment_cert_strict(&frame.sender_enrollment_cert)?;
    cert.verify(ctx.now_secs())
        .map_err(|_| DepositReject::BadCert)?;
    let cert_master = match &cert.issuer {
        EnrollmentIssuer::Master { master_pubkey } => master_pubkey.classical.ed25519_verify,
        // cert.verify() only structurally checks Quorum certs (it cannot
        // verify quorum signatures without an OwnerState walk-back), so a
        // non-Master issuer is rejected outright — mirrors
        // `iroh_friend_acceptor::verify_enrolled_device`.
        _ => return Err(DepositReject::BadCert),
    };
    if cert.owner_id != frame.sender_owner || cert_master != friend_master {
        return Err(DepositReject::BadCert);
    }
    let device_vk_bytes = cert.device_pubkeys.classical.ed25519_verify;

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
    let packet = decode_packet(&payload.cidnotify_packet).map_err(|_| DepositReject::BadPayload)?;
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

    // Step 6 — atomic persist-with-caps + durable flush BEFORE the ack
    // exists (D7: an ack never lies). Insert-once on the key with the
    // per-sender + global quotas enforced inside the persist critical
    // section; an occupied key bypasses the caps, so a redelivery after a
    // lost ack is absorbed and still acked even at a full inbox.
    let key = DmInboxDoc::key(&signed.space_id.0, &signed.message_cid.to_bytes());
    let entry = DmInboxEntry {
        sender_owner: frame.sender_owner,
        cidnotify_packet: payload.cidnotify_packet,
        storage_blob: payload.storage_blob,
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
        space_id: signed.space_id.0,
        message_cid: signed.message_cid.to_bytes().to_vec(),
    })
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
        }
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
    use crate::butler_deposit::{encode_deposit_payload, DepositPayload, BUTLER_DEPOSIT_SEAL_INFO};
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
            cidnotify_packet: cidnotify_packet.clone(),
            storage_blob: storage_blob.clone(),
        };
        let payload_bytes = encode_deposit_payload(&payload).expect("encode payload");
        let sealed = seal_payload_bytes(&payload_bytes);
        let sig = sign_frame(&BUTLER_OWNER, &sealed, &so.device_key);
        let cert_bytes = harmony_owner::cbor::to_canonical(&so.cert).expect("encode cert");
        Fixture {
            frame: DepositFrame {
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
            cidnotify_packet: Vec::new(),
            storage_blob: Vec::new(),
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
            assert_eq!(entry.cidnotify_packet, f.cidnotify_packet);
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
            cidnotify_packet: f.cidnotify_packet.clone(),
            storage_blob: f.storage_blob.clone(),
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
        assert_eq!(entry.cidnotify_packet, f.cidnotify_packet);
        assert_eq!(entry.storage_blob, f.storage_blob);
    }

    #[tokio::test]
    async fn deposit_from_non_friend_rejected_before_any_crypto() {
        let f = valid_fixture();

        // Unknown sender: rejected at the friend lookup — the decrypt probe
        // (and everything after it) is never reached.
        let ctx = TestCtx {
            friends: BTreeMap::new(),
            ..TestCtx::for_fixture(&f)
        };
        let err = handle_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("non-friend deposit must be rejected");
        assert_eq!(err, DepositReject::NotFriend);
        assert_eq!(
            ctx.events(),
            vec!["friend_lookup".to_string()],
            "no crypto step (decrypt) and no persist may run for a non-friend"
        );
        assert!(ctx.store.lock().unwrap().is_empty());

        // Pending / Revoked friends are NOT admitted either (must be Active).
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
            assert_eq!(err, DepositReject::NotFriend);
            assert_eq!(ctx.events(), vec!["friend_lookup".to_string()]);
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
            cidnotify_packet: tampered_packet,
            storage_blob: f.storage_blob.clone(),
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
            cidnotify_packet: mismatch_packet,
            storage_blob: f.storage_blob.clone(),
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
            cidnotify_packet: foreign_packet,
            storage_blob: f.storage_blob.clone(),
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
}
