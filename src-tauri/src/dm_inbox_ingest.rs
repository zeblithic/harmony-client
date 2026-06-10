//! ZEB-418 SP2 Phase 1 Task 6: dm-inbox ingestion — every device of the
//! recipient owner runs deposited-but-not-yet-ingested entries through the
//! NORMAL DM receive path (CAS availability, receive-path verification,
//! `apply_inbox`, the same `dm-received` UI event), then acks its ingestion
//! into the entry's grow-only `ingested_by` set, then GCs entries whose
//! `ingested_by` already covered the enrolled device set when the sweep
//! began (one-sweep deferral so the covering state replicates first — see
//! `covered_at_start` in [`ingest_pending`]) or whose TTL expired.
//!
//! See `docs/specs/2026-06-09-zeb-418-sp2-butler-design.md` §5 (normative
//! for ingestion + GC semantics).
//!
//! ## Tauri-free core
//!
//! [`ingest_pending`] takes everything it needs through the injectable
//! [`DmInboxIngestCtx`] trait (mirroring `iroh_butler_acceptor`'s
//! `ButlerDepositCtx`), so the unit tests run with probes and no Tauri/iroh
//! runtime. Production (Task 7) implements the trait over `NodeState`
//! handles using the `pub(crate)` receive-path helpers extracted from
//! `handle_cidnotify_lifted` (`dm_outbox::verify_cidnotify_admission`,
//! `dm_outbox::decrypt_and_bind_dm_blob`,
//! `dm_outbox::dm_received_event_payload`).
//!
//! ## Trigger model
//!
//! The dm-inbox `FleetSyncEngine`'s `on_applied` callback (fires after an
//! inbound merge that changed local state — new entries OR ig growth) sends
//! a nudge on a capacity-1 mpsc via [`ingest_nudge_on_applied`];
//! [`run_dm_inbox_ingest_sweeper`] debounces nudge bursts and runs one
//! sweep per burst, plus one sweep at startup for entries deposited while
//! this device was offline.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use crate::butler_deposit::INBOX_TTL_MS;
use crate::dm_inbox_crdt::{DmInboxDoc, DmInboxEntry};
use crate::owner_state_types::{ContentId, Hlc, InboxEntry, OwnerAddr, ReceivedMessage, SpaceId};

/// Debounce window between an `on_applied` nudge and the sweep it triggers:
/// a burst of merges (e.g. initial fan-in right after coming online)
/// coalesces into one sweep.
pub const INGEST_SWEEP_DEBOUNCE_MS: u64 = 250;

/// The verified, decrypted fields ingestion needs from one deposited entry:
/// exactly what `apply_inbox` and the `dm-received` emit consume. Produced
/// by [`DmInboxIngestCtx::verify`] (production: the receive-path helpers
/// extracted from `handle_cidnotify_lifted`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedDmDeposit {
    pub space_id: SpaceId,
    pub message_cid: ContentId,
    /// Decrypted message body (plaintext bytes).
    pub body: Vec<u8>,
    pub mime_type: String,
    pub sent_at: Hlc,
}

/// Injectable context for [`ingest_pending`]: CAS put, receive-path
/// verification, `apply_inbox`, the UI emit, the enrolled-device snapshot,
/// and the TTL clock. Production (ZEB-418 P1 Task 7) implements this over
/// `NodeState`'s owner-state + CAS + AppHandle; tests implement it with
/// probes that record call order.
#[async_trait]
pub trait DmInboxIngestCtx: Send + Sync {
    /// This device's SP1 device id: 64-hex (lowercase) of the device
    /// ed25519 verify key — the SAME string form `ingested_by` carries and
    /// [`Self::enrolled_device_ids`] returns.
    fn self_device_id(&self) -> String;

    /// Make the deposited storage blob locally available in CAS so the
    /// message body can be (re-)fetched exactly like a direct arrival
    /// (production: `ContentStore::put` — idempotent on content address).
    async fn cas_put(&self, storage_blob: &[u8]) -> Result<(), String>;

    /// Run the entry through the NORMAL DM receive-path verification.
    /// Production (Task 7), reusing the `pub(crate)` helpers extracted
    /// from `handle_cidnotify_lifted`:
    ///   1. `dm_envelope::decode_packet(&entry.cidnotify_packet)` →
    ///      `DmPacket::CidNotify { signed, signature, signed_bytes }`;
    ///   2. `signed.sender_owner_addr.0 == entry.sender_owner` (mirror the
    ///      acceptor's step-6 sender-consistency check);
    ///   3. under the owner-state lock:
    ///      `dm_outbox::verify_cidnotify_admission(&state, &signed,
    ///      &signature, &signed_bytes)` → `(space, resolved_owner, _)`;
    ///   4. `ContentId::for_book(&entry.storage_blob, encrypted: true)`
    ///      must equal `signed.message_cid` (blob ↔ packet binding);
    ///   5. `dm_outbox::decrypt_and_bind_dm_blob(&space,
    ///      &entry.storage_blob, resolved_owner)` → `MessagePayload`.
    ///
    /// An `Err` leaves the entry PENDING (not marked ingested): it is
    /// retried on every sweep until it verifies or the TTL GC removes it.
    async fn verify(&self, entry: &DmInboxEntry) -> Result<VerifiedDmDeposit, String>;

    /// `OwnerState::apply_inbox` under the owner-state lock (idempotent on
    /// `(space_id, message_cid)`). Returns `true` iff the entry was NEWLY
    /// inserted (`ApplyOutcome::Inserted`) — the `dm-received` emit gate,
    /// exactly the normal path's atomic-emit boundary. `false` = the
    /// message is already in the DM store (direct arrival, or an earlier
    /// ingest whose ig-update was lost) — consumed without re-emitting.
    async fn apply_inbox(&self, entry: InboxEntry) -> Result<bool, String>;

    /// Emit the SAME UI event the normal receive path emits — production:
    /// `app.emit(dm_outbox::DM_RECEIVED_EVENT,
    /// dm_outbox::dm_received_event_payload(&msg))`.
    fn emit_dm_received(&self, msg: &ReceivedMessage);

    /// Currently-enrolled device ids for the GC coverage check, in the
    /// SAME 64-hex string form as `ingested_by`: production maps the
    /// `harmony_owner` `OwnerState.enrollments` values through
    /// `hex::encode(cert.device_pubkeys.classical.ed25519_verify)`.
    /// Enrolled — NOT online — devices (spec §5): a revoked device's
    /// absence must not pin entries forever (revocation-pruning follows
    /// the existing device-revocation path).
    async fn enrolled_device_ids(&self) -> BTreeSet<String>;

    /// Wall-clock now in epoch-ms for the TTL check.
    fn now_ms(&self) -> u64;
}

/// One ingestion + GC sweep over the dm-inbox doc (call under the doc
/// lock). Returns `true` when the doc was mutated (ig growth or GC
/// removal) — the caller must then `notify_dirty()` so the engine
/// replicates our ingestion ack / removal to siblings.
pub async fn ingest_pending(doc: &mut DmInboxDoc, ctx: &dyn DmInboxIngestCtx) -> bool {
    let self_id = ctx.self_device_id();
    let mut changed = false;

    // Coverage snapshot at sweep START (publish-before-GC, ZEB-418 P1
    // Task 9): coverage-GC below removes only entries that were ALREADY
    // covered when the sweep began. An entry whose final ig ack lands
    // DURING this sweep is retained for one sweep so the covering
    // `ig ⊇ enrolled` state is published (the caller's notify_dirty) and
    // replicates to siblings BEFORE any replica destroys it. Without this,
    // the last-ingesting device would add itself to `ig` and remove the
    // entry in the same pass — the covering state would never escape, and
    // every OTHER replica (whose own ig view is missing the last ack)
    // would pin the entry until the 30-day TTL. Caught by
    // `tests/butler_deposit_integration.rs`. Coverage is disabled when the
    // enrolled snapshot is empty: `ig ⊇ ∅` is vacuously true, and an empty
    // provider snapshot (state not yet loaded) must not wipe the inbox.
    let enrolled = ctx.enrolled_device_ids().await;
    let covered_at_start: BTreeSet<String> = doc
        .entries
        .iter()
        .filter(|(_, e)| !enrolled.is_empty() && enrolled.iter().all(|d| e.ingested_by.contains(d)))
        .map(|(k, _)| k.clone())
        .collect();

    for (key, entry) in doc.entries.iter_mut() {
        if entry.ingested_by.contains(&self_id) {
            continue;
        }
        // 1. CAS-put FIRST so the message blob is fetchable exactly like a
        //    direct arrival before anything references its CID.
        if let Err(e) = ctx.cas_put(&entry.storage_blob).await {
            tracing::warn!(
                error = %e,
                key = %key,
                "ZEB-418 ingest: CAS put failed; entry left pending for retry"
            );
            continue;
        }
        // 2. Normal receive-path verification (see the trait docs for the
        //    production pipeline). A failure leaves the entry PENDING — it
        //    is retried on every sweep until it verifies (e.g. the sender's
        //    device cache entry arrives) or the TTL GC removes it.
        let verified = match ctx.verify(entry).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    key = %key,
                    "ZEB-418 ingest: verification failed; entry left pending \
                     for retry (TTL GC is the safety valve)"
                );
                continue;
            }
        };
        // 3. apply_inbox — idempotent on (space_id, message_cid): a message
        //    that ALSO arrived direct dedupes here (spec §5 both-paths
        //    race). Plan-pinned shape: from = the deposit's sender_owner,
        //    received_at = the butler's deposit time.
        let inbox_entry = InboxEntry {
            space_id: verified.space_id,
            message_cid: verified.message_cid,
            from: OwnerAddr(entry.sender_owner),
            received_at: entry.deposited_at.clone(),
        };
        let newly_inserted = match ctx.apply_inbox(inbox_entry.clone()).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    key = %key,
                    "ZEB-418 ingest: apply_inbox failed; entry left pending for retry"
                );
                continue;
            }
        };
        // 4. Emit the SAME dm-received event the normal path emits, gated
        //    on Inserted exactly like the normal path's atomic-emit
        //    boundary (duplicate keys never re-emit).
        if newly_inserted {
            ctx.emit_dm_received(&ReceivedMessage {
                inbox_entry,
                body: verified.body,
                mime_type: verified.mime_type,
                sent_at: verified.sent_at,
            });
        }
        // 5. Ack our ingestion into the grow-only ig set (replicated to
        //    siblings via the caller's notify_dirty).
        entry.ingested_by.insert(self_id.clone());
        changed = true;
    }

    // GC pass — an entry is removable when EITHER:
    //   (a) coverage: every currently-enrolled device id appeared in
    //       `ingested_by` at sweep START (compared in the shared 64-hex
    //       device-id string form — see the `covered_at_start` snapshot
    //       above and `DmInboxIngestCtx::enrolled_device_ids`), or
    //   (b) TTL: `deposited_at.wall_ms + INBOX_TTL_MS < now` (strict).
    //
    // Churn-tolerant model (resurrection-by-merge): a sibling that GC'd
    // later than us can re-merge an old doc and resurrect a removed entry
    // (the insert-once merge sees the missing key as new). That is FINE:
    // both GC criteria are deterministic functions of (ingested_by, now),
    // and `ingested_by` is grow-only with union merge — so every replica
    // evaluating the resurrected entry reaches the same removable verdict
    // and the next sweep removes it again (a resurrected entry arrives
    // already covered, so it IS covered at that sweep's start). Once every
    // replica has GC'd, no copy remains to resurrect, so the fleet
    // converges WITHOUT tombstones. Re-ingestion after a lost ig-update is
    // likewise safe: `apply_inbox` is idempotent on (space_id,
    // message_cid), and the dm-received emit is gated on its Inserted
    // outcome.
    //
    // An expired entry that was just ingested above is still removed —
    // delivery beat the deadline; the deposit record's job is done.
    let now_ms = ctx.now_ms();
    let before = doc.entries.len();
    doc.entries.retain(|key, e| {
        let ttl_expired = e.deposited_at.wall_ms.saturating_add(INBOX_TTL_MS) < now_ms;
        !(ttl_expired || covered_at_start.contains(key))
    });
    changed |= doc.entries.len() != before;

    changed
}

/// Adapter for `FleetSyncConfig::on_applied`: nudges the ingest sweeper.
/// `try_send` on a capacity-1 channel — the nudge is a level trigger, so a
/// full buffer (a sweep is already scheduled) makes dropping extras
/// correct.
pub fn ingest_nudge_on_applied(nudge_tx: mpsc::Sender<()>) -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || {
        let _ = nudge_tx.try_send(());
    })
}

/// The ingest-sweeper task: one startup sweep (entries deposited while this
/// device was offline), then one debounced sweep per `on_applied` nudge
/// burst. Exits when every nudge sender is dropped (engine shutdown).
///
/// ZEB-418 P1 Task 7: wire this in `start_node` alongside the dm-inbox
/// `FleetSyncEngine` construction (nothing in `lib.rs` references it yet):
///
/// ```text
/// let (nudge_tx, nudge_rx) = tokio::sync::mpsc::channel(1);
/// // FleetSyncConfig { on_applied: Some(ingest_nudge_on_applied(nudge_tx)), .. }
/// tokio::spawn(run_dm_inbox_ingest_sweeper(
///     Arc::clone(&dm_inbox_doc),   // the engine's `state` Arc
///     prod_ctx,                    // DmInboxIngestCtx over NodeState handles
///     nudge_rx,
///     Arc::new(move || engine.notify_dirty()),
///     Duration::from_millis(INGEST_SWEEP_DEBOUNCE_MS),
/// ));
/// // Shutdown: drop nudge_tx (stop_inner) → recv() yields None → task exits.
/// ```
pub async fn run_dm_inbox_ingest_sweeper(
    doc: Arc<Mutex<DmInboxDoc>>,
    ctx: Arc<dyn DmInboxIngestCtx>,
    mut nudge_rx: mpsc::Receiver<()>,
    notify_dirty: Arc<dyn Fn() + Send + Sync>,
    debounce: Duration,
) {
    // Startup sweep: ingest entries deposited while this device was
    // offline (they arrive via the engine's initial fan-in BEFORE
    // on_applied wiring can observe them, and via the persisted doc).
    sweep_once(&doc, ctx.as_ref(), notify_dirty.as_ref()).await;

    while nudge_rx.recv().await.is_some() {
        // Debounce: let the rest of a merge burst land, then drain any
        // extra nudges so the burst coalesces into one sweep.
        tokio::time::sleep(debounce).await;
        while nudge_rx.try_recv().is_ok() {}
        sweep_once(&doc, ctx.as_ref(), notify_dirty.as_ref()).await;
    }
    // recv() == None: every nudge sender dropped (engine shutdown).
}

/// Production [`DmInboxIngestCtx`] over real `start_node` handles (ZEB-418
/// P1 Task 7). Each method implements its trait doc's production contract
/// verbatim:
///
/// * CAS = the shared `RuntimeContentStore` (`ContentStore::put`,
///   idempotent on content address);
/// * verification = the `pub(crate)` receive-path helpers extracted from
///   `handle_cidnotify_lifted` (`dm_outbox::verify_cidnotify_admission` +
///   `dm_outbox::decrypt_and_bind_dm_blob`) under the owner-state lock —
///   the SAME trust path a direct arrival takes;
/// * the UI emit = the shared `dm_outbox::DM_RECEIVED_EVENT` const +
///   `dm_received_event_payload` builder;
/// * `enrolled` = a start_node-time snapshot of the `harmony_owner`
///   `OwnerState.enrollments` values mapped through
///   `hex::encode(cert.device_pubkeys.classical.ed25519_verify)` (the SP1
///   64-hex device-id form). Enrollment changes require a node restart, so
///   the snapshot tracks the enrolled set for the engine's lifetime.
pub struct ProdDmInboxIngestCtx<R: tauri::Runtime> {
    /// This device's SP1 device id (64-hex of the device ed25519 verify key).
    pub device_id: String,
    /// The runtime owner-state CRDT (`NodeState`'s `crdt_state` Arc).
    pub crdt_state: Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
    /// The shared CAS handle (`RuntimeContentStore` in production).
    pub content_store: Arc<dyn crate::content_store::ContentStore>,
    /// AppHandle for the `dm-received` emit.
    pub app: tauri::AppHandle<R>,
    /// Enrolled device ids (64-hex), snapshotted at start_node.
    pub enrolled: BTreeSet<String>,
}

#[async_trait]
impl<R: tauri::Runtime> DmInboxIngestCtx for ProdDmInboxIngestCtx<R> {
    fn self_device_id(&self) -> String {
        self.device_id.clone()
    }

    async fn cas_put(&self, storage_blob: &[u8]) -> Result<(), String> {
        // The CID is recomputed under the DM send path's exact for_book
        // flags so the stored blob is fetchable at the packet's
        // message_cid (the acceptor already verified the binding; verify()
        // re-checks it before anything references the CID).
        let cid = harmony_content::cid::ContentId::for_book(
            storage_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .map_err(|e| format!("for_book: {e:?}"))?;
        self.content_store
            .put(cid, storage_blob.to_vec())
            .await
            .map_err(|e| format!("cas put: {e}"))
    }

    async fn verify(&self, entry: &DmInboxEntry) -> Result<VerifiedDmDeposit, String> {
        // 1. Decode the deposited packet — must be a CidNotify.
        let packet = crate::dm_envelope::decode_packet(&entry.cidnotify_packet)
            .map_err(|e| format!("decode_packet: {e}"))?;
        let crate::dm_envelope::DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        } = packet
        else {
            return Err("deposited packet is not a CidNotify".into());
        };
        // 2. Sender consistency: the packet must claim the deposit entry's
        //    sender (mirrors the acceptor's step-6 check; re-checked here
        //    because a sibling's doc merge is also a trust boundary).
        if signed.sender_owner_addr.0 != entry.sender_owner {
            return Err("packet sender_owner does not match deposit entry".into());
        }
        // 3. The NORMAL receive-path admission pipeline, under the
        //    owner-state lock (signature, owner resolution, Space lookup,
        //    SpaceKind gate, membership).
        let (space, resolved_owner) = {
            let state = self.crdt_state.lock().await;
            let (space, resolved_owner, _identity_pub) =
                crate::dm_outbox::verify_cidnotify_admission(
                    &state,
                    &signed,
                    &signature,
                    &signed_bytes,
                )
                .map_err(|e| format!("verify_cidnotify_admission: {e:?}"))?;
            (space, resolved_owner)
        };
        // 4. Blob ↔ packet binding: the deposited storage blob must hash to
        //    the packet's message_cid under the DM send path's flags.
        let computed_cid = harmony_content::cid::ContentId::for_book(
            &entry.storage_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .map_err(|e| format!("for_book: {e:?}"))?;
        if computed_cid != signed.message_cid {
            return Err("storage blob CID does not match packet message_cid".into());
        }
        // 5. Decrypt + sender binding — the normal path's Phase C.
        let payload =
            crate::dm_outbox::decrypt_and_bind_dm_blob(&space, &entry.storage_blob, resolved_owner)
                .map_err(|e| format!("decrypt_and_bind_dm_blob: {e:?}"))?;
        Ok(VerifiedDmDeposit {
            space_id: signed.space_id,
            message_cid: signed.message_cid,
            body: payload.body,
            mime_type: payload.mime_type,
            sent_at: payload.sent_at,
        })
    }

    async fn apply_inbox(&self, entry: InboxEntry) -> Result<bool, String> {
        let mut state = self.crdt_state.lock().await;
        match state.apply_inbox(entry) {
            crate::owner_state_crdt::ApplyOutcome::Inserted => Ok(true),
            crate::owner_state_crdt::ApplyOutcome::Merged { .. } => Ok(false),
            crate::owner_state_crdt::ApplyOutcome::Rejected(reason) => {
                Err(format!("apply_inbox rejected: {reason:?}"))
            }
        }
    }

    fn emit_dm_received(&self, msg: &ReceivedMessage) {
        use tauri::Emitter;
        let _ = self.app.emit(
            crate::dm_outbox::DM_RECEIVED_EVENT,
            crate::dm_outbox::dm_received_event_payload(msg),
        );
    }

    async fn enrolled_device_ids(&self) -> BTreeSet<String> {
        self.enrolled.clone()
    }

    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

/// One locked sweep; `notify_dirty` only when the doc actually changed so
/// idle sweeps don't schedule no-op publishes.
async fn sweep_once(
    doc: &Arc<Mutex<DmInboxDoc>>,
    ctx: &dyn DmInboxIngestCtx,
    notify_dirty: &(dyn Fn() + Send + Sync),
) {
    let changed = {
        let mut guard = doc.lock().await;
        ingest_pending(&mut guard, ctx).await
    };
    if changed {
        notify_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    const SELF_ID: &str = "self-device-64hex";
    const SIBLING_ID: &str = "sibling-device-64hex";
    const SENDER_OWNER: [u8; 16] = [0xAA; 16];

    fn hlc(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: "butler-device".into(),
        }
    }

    /// A doc entry whose `cidnotify_packet` is probe-decodable:
    /// `[space_id(16) ‖ message_cid(32)]` (the probe ctx's `verify` parses
    /// it the way production decodes the real packet).
    fn make_entry(
        space: [u8; 16],
        cid: [u8; 32],
        deposited_ms: u64,
        ig: &[&str],
    ) -> (String, DmInboxEntry) {
        let mut packet = Vec::with_capacity(48);
        packet.extend_from_slice(&space);
        packet.extend_from_slice(&cid);
        let entry = DmInboxEntry {
            sender_owner: SENDER_OWNER,
            cidnotify_packet: packet,
            storage_blob: format!("blob-{}", hex::encode(&cid[..4])).into_bytes(),
            deposited_at: hlc(deposited_ms),
            deposited_by: "butler-device".into(),
            ingested_by: ig.iter().map(|s| s.to_string()).collect(),
        };
        (DmInboxDoc::key(&space, &cid), entry)
    }

    /// Probe ctx: records call ORDER plus everything written through each
    /// seam, so tests can assert "CAS contains the blob", "apply_inbox
    /// recorded", and "emit gated on Inserted" without Tauri.
    struct ProbeCtx {
        enrolled: BTreeSet<String>,
        now_ms: u64,
        cas_fail: bool,
        verify_fail: bool,
        /// What `apply_inbox` reports: `true` = newly inserted.
        apply_inserted: bool,
        calls: StdMutex<Vec<String>>,
        cas_blobs: StdMutex<Vec<Vec<u8>>>,
        applied: StdMutex<Vec<InboxEntry>>,
        emitted: StdMutex<Vec<ReceivedMessage>>,
    }

    impl ProbeCtx {
        fn new() -> Self {
            Self {
                // Self + one sibling so a single-device ingest does NOT
                // complete coverage (tests that want coverage-GC shrink
                // or grow this set explicitly).
                enrolled: [SELF_ID.to_string(), SIBLING_ID.to_string()].into(),
                now_ms: 1_000_000,
                cas_fail: false,
                verify_fail: false,
                apply_inserted: true,
                calls: StdMutex::new(Vec::new()),
                cas_blobs: StdMutex::new(Vec::new()),
                applied: StdMutex::new(Vec::new()),
                emitted: StdMutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn applied(&self) -> Vec<InboxEntry> {
            self.applied.lock().unwrap().clone()
        }

        fn emitted(&self) -> Vec<ReceivedMessage> {
            self.emitted.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DmInboxIngestCtx for ProbeCtx {
        fn self_device_id(&self) -> String {
            SELF_ID.to_string()
        }

        async fn cas_put(&self, storage_blob: &[u8]) -> Result<(), String> {
            self.calls.lock().unwrap().push("cas_put".into());
            if self.cas_fail {
                return Err("simulated CAS failure".into());
            }
            self.cas_blobs.lock().unwrap().push(storage_blob.to_vec());
            Ok(())
        }

        async fn verify(&self, entry: &DmInboxEntry) -> Result<VerifiedDmDeposit, String> {
            self.calls.lock().unwrap().push("verify".into());
            if self.verify_fail {
                return Err("simulated verification failure".into());
            }
            // Parse the probe packet layout: [space_id(16) ‖ cid(32)].
            let space: [u8; 16] = entry.cidnotify_packet[..16]
                .try_into()
                .expect("probe packet space_id");
            let cid: [u8; 32] = entry.cidnotify_packet[16..48]
                .try_into()
                .expect("probe packet cid");
            Ok(VerifiedDmDeposit {
                space_id: SpaceId(space),
                message_cid: ContentId::from_bytes(cid),
                body: b"hello".to_vec(),
                mime_type: "text/plain".into(),
                sent_at: hlc(42),
            })
        }

        async fn apply_inbox(&self, entry: InboxEntry) -> Result<bool, String> {
            self.calls.lock().unwrap().push("apply_inbox".into());
            self.applied.lock().unwrap().push(entry);
            Ok(self.apply_inserted)
        }

        fn emit_dm_received(&self, msg: &ReceivedMessage) {
            self.calls.lock().unwrap().push("emit".into());
            self.emitted.lock().unwrap().push(msg.clone());
        }

        async fn enrolled_device_ids(&self) -> BTreeSet<String> {
            self.enrolled.clone()
        }

        fn now_ms(&self) -> u64 {
            self.now_ms
        }
    }

    #[tokio::test]
    async fn ingest_puts_blob_verifies_and_applies_inbox() {
        let (key, entry) = make_entry([1; 16], [2; 32], 500, &[]);
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry.clone());
        let ctx = ProbeCtx::new();

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "ingestion mutated the doc (ig growth)");

        // CAS contains the deposited storage blob.
        assert_eq!(
            *ctx.cas_blobs.lock().unwrap(),
            vec![entry.storage_blob.clone()],
            "the deposited storage blob must be CAS-put"
        );

        // apply_inbox recorded with EXACTLY the plan-pinned InboxEntry:
        // from = sender_owner, received_at = deposited_at.
        let expected_inbox = InboxEntry {
            space_id: SpaceId([1; 16]),
            message_cid: ContentId::from_bytes([2; 32]),
            from: OwnerAddr(SENDER_OWNER),
            received_at: hlc(500),
        };
        assert_eq!(ctx.applied(), vec![expected_inbox.clone()]);

        // The same UI event the normal path emits, with the decrypted body.
        let emitted = ctx.emitted();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].inbox_entry, expected_inbox);
        assert_eq!(emitted[0].body, b"hello".to_vec());
        assert_eq!(emitted[0].mime_type, "text/plain");
        assert_eq!(emitted[0].sent_at, hlc(42));

        // Self added to the grow-only ig set; entry retained (coverage
        // incomplete — the sibling hasn't ingested).
        assert!(doc.entries[&key].ingested_by.contains(SELF_ID));

        // Order probe: CAS availability → verification → apply → emit.
        assert_eq!(
            ctx.calls(),
            vec!["cas_put", "verify", "apply_inbox", "emit"]
        );
    }

    #[tokio::test]
    async fn ingest_is_idempotent_for_already_ingested() {
        // (a) Entry whose ig already contains self: untouched, zero calls.
        let (key, entry) = make_entry([1; 16], [2; 32], 500, &[SELF_ID]);
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(!changed, "already-ingested entry is a no-op");
        assert!(ctx.calls().is_empty(), "no seam may be touched");
        assert!(
            doc.entries.contains_key(&key),
            "entry retained (no coverage, fresh)"
        );

        // (b) Pending entry whose message ALREADY reached the DM store
        // (direct arrival, or an earlier ingest whose ig-update was lost):
        // apply_inbox reports Merged (false) → ingestion still consumes the
        // deposit (self added to ig) but does NOT re-emit dm-received —
        // apply_inbox's (space_id, message_cid) idempotency makes lost-ig
        // re-ingestion safe end-to-end.
        let (key_b, entry_b) = make_entry([3; 16], [4; 32], 600, &[]);
        let mut doc_b = DmInboxDoc::default();
        doc_b.entries.insert(key_b.clone(), entry_b);
        let ctx_b = ProbeCtx {
            apply_inserted: false,
            ..ProbeCtx::new()
        };

        let changed = ingest_pending(&mut doc_b, &ctx_b).await;
        assert!(changed, "ig growth is a doc mutation");
        assert_eq!(
            ctx_b.calls(),
            vec!["cas_put", "verify", "apply_inbox"],
            "Merged apply must NOT re-emit dm-received"
        );
        assert!(ctx_b.emitted().is_empty());
        assert!(doc_b.entries[&key_b].ingested_by.contains(SELF_ID));

        // (c) Second pass over the same doc: fully idempotent.
        let before_calls = ctx_b.calls().len();
        let changed = ingest_pending(&mut doc_b, &ctx_b).await;
        assert!(!changed, "second pass is a no-op");
        assert_eq!(ctx_b.calls().len(), before_calls);
    }

    #[tokio::test]
    async fn failed_cas_or_verify_leaves_entry_pending_for_retry() {
        // CAS failure: entry left pending (no ig add), retried next sweep.
        let (key, entry) = make_entry([1; 16], [2; 32], 500, &[]);
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry.clone());
        let ctx = ProbeCtx {
            cas_fail: true,
            ..ProbeCtx::new()
        };
        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(!changed);
        assert_eq!(
            ctx.calls(),
            vec!["cas_put"],
            "verify must not run on CAS failure"
        );
        assert!(doc.entries[&key].ingested_by.is_empty());

        // Verification failure: pending too (TTL GC is the safety valve).
        let ctx = ProbeCtx {
            verify_fail: true,
            ..ProbeCtx::new()
        };
        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(!changed);
        assert_eq!(ctx.calls(), vec!["cas_put", "verify"]);
        assert!(doc.entries[&key].ingested_by.is_empty());
        assert!(ctx.applied().is_empty());

        // Once the failure clears, the SAME entry ingests normally.
        let ctx = ProbeCtx::new();
        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed);
        assert_eq!(ctx.applied().len(), 1);
        assert!(doc.entries[&key].ingested_by.contains(SELF_ID));
    }

    /// Poll `cond` until true (paused-clock sleeps auto-advance, so this
    /// is logical-time waiting, not wall-clock); panics after a generous
    /// logical-time cap so a broken sweeper fails fast instead of hanging.
    async fn wait_until(cond: impl Fn() -> bool) {
        for _ in 0..10_000 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("wait_until: condition not met within the logical-time cap");
    }

    #[tokio::test(start_paused = true)]
    async fn startup_sweep_ingests_preexisting_entries() {
        let (key, entry) = make_entry([1; 16], [2; 32], 500, &[]);
        let mut initial = DmInboxDoc::default();
        initial.entries.insert(key.clone(), entry);
        let doc = Arc::new(Mutex::new(initial));
        let ctx = Arc::new(ProbeCtx::new());
        let dirty = Arc::new(AtomicUsize::new(0));
        let notify_dirty: Arc<dyn Fn() + Send + Sync> = {
            let dirty = Arc::clone(&dirty);
            Arc::new(move || {
                dirty.fetch_add(1, Ordering::SeqCst);
            })
        };
        let (nudge_tx, nudge_rx) = mpsc::channel(1);

        let handle = tokio::spawn(run_dm_inbox_ingest_sweeper(
            Arc::clone(&doc),
            Arc::clone(&ctx) as Arc<dyn DmInboxIngestCtx>,
            nudge_rx,
            notify_dirty,
            Duration::from_millis(INGEST_SWEEP_DEBOUNCE_MS),
        ));

        // The startup sweep ingests the pre-existing entry with NO nudge.
        wait_until(|| ctx.applied().len() == 1).await;
        assert!(doc.lock().await.entries[&key].ingested_by.contains(SELF_ID));
        assert!(
            dirty.load(Ordering::SeqCst) >= 1,
            "a mutating sweep must notify_dirty so the ig ack replicates"
        );

        // A nudge (the on_applied path) triggers a debounced follow-up
        // sweep that picks up an entry merged in after startup.
        let (key2, entry2) = make_entry([3; 16], [4; 32], 600, &[]);
        doc.lock().await.entries.insert(key2.clone(), entry2);
        nudge_tx.send(()).await.expect("sweeper alive");
        wait_until(|| ctx.applied().len() == 2).await;
        assert!(doc.lock().await.entries[&key2]
            .ingested_by
            .contains(SELF_ID));

        // Dropping the last nudge sender shuts the task down cleanly.
        drop(nudge_tx);
        tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("sweeper must exit when nudge senders drop")
            .expect("sweeper must not panic");
    }

    /// The on_applied adapter nudges the channel; a full buffer (sweep
    /// already pending) drops extras without blocking or panicking.
    #[tokio::test]
    async fn ingest_nudge_on_applied_is_nonblocking_level_trigger() {
        let (tx, mut rx) = mpsc::channel(1);
        let nudge = ingest_nudge_on_applied(tx);
        nudge();
        nudge(); // buffer full — dropped, must not block/panic
        assert_eq!(rx.recv().await, Some(()));
        assert!(rx.try_recv().is_err(), "extras coalesced into one nudge");
    }

    /// End-to-end engine-wiring proof (ZEB-418 P1 Task 7): a real
    /// `FleetSyncEngine<DmInboxDoc>` configured exactly as `start_node`
    /// configures it (DmInboxPersist sink, `merge_from` merger,
    /// `publish_seen: true` — GC depends on sibling ig-acks propagating,
    /// spec §5 — lookup tag `b"dm-inbox-v1"`, `on_applied` = the ingestion
    /// nudge) must emit an outbound wire frame on the publisher channel when
    /// a local deposit write is followed by `notify_dirty` + `flush_now`.
    /// Mirrors `notes_commands::notes_engine_publishes_on_local_write`
    /// site-for-site.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dm_inbox_engine_publishes_on_local_write() {
        use crate::content_store::{ContentStore, InMemoryStub};
        use crate::dm_inbox_persist::DmInboxPersist;
        use crate::fleet_sync::{FleetSyncConfig, FleetSyncEngine, Merger, DEFAULT_DEBOUNCE_MS};
        use crate::owner_state_crypto::KeyTree;
        use std::collections::BTreeMap;

        let dir = tempfile::tempdir().unwrap();
        let kt = Arc::new(KeyTree::derive(&[0x44u8; 32]).expect("derive kt"));
        let doc = Arc::new(Mutex::new(DmInboxDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let merger: Merger<DmInboxDoc> = Arc::new(|local, remote| local.merge_from(remote));
        // The ingestion-nudge channel exactly as start_node wires it (the
        // receiver half would feed `run_dm_inbox_ingest_sweeper`; this test
        // proves the ENGINE wiring, so the rx is simply held).
        let (nudge_tx, _nudge_rx) = mpsc::channel::<()>(1);

        let engine = FleetSyncEngine::<DmInboxDoc>::new(FleetSyncConfig {
            kt,
            device_id: "dev-A".to_string(),
            state: Arc::clone(&doc),
            merger,
            replay_tracker: Arc::clone(&tracker),
            content_store: cas,
            publisher_tx: out_tx,
            subscriber_rx: in_rx,
            persist: Arc::new(DmInboxPersist {
                doc_path: dir.path().join("dm_inbox.cbor"),
                replay_path: dir.path().join("dm_inbox_replay.cbor"),
            }),
            lookup_key_tag: b"dm-inbox-v1",
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            publish_seen: true,
            on_applied: Some(ingest_nudge_on_applied(nudge_tx)),
            sibling_acks: Arc::new(Mutex::new(BTreeMap::new())),
        });

        // A local deposit write (what the production butler persist path
        // does under the doc lock), then force the engine to publish.
        {
            let (key, entry) = make_entry([9; 16], [8; 32], 700, &[]);
            doc.lock().await.entries.insert(key, entry);
        }
        engine.notify_dirty();
        engine.flush_now().await.unwrap();

        // The local write must have driven a (non-empty) publish frame onto
        // the outbound channel.
        let frame = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("publish frame produced within 5s")
            .expect("publisher channel yielded Some(frame)");
        assert!(!frame.is_empty(), "published wire frame must be non-empty");

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn gc_removes_when_ig_covers_enrolled_set_or_ttl() {
        let now_ms: u64 = INBOX_TTL_MS + 1_000_000;
        let ctx = ProbeCtx {
            now_ms,
            ..ProbeCtx::new()
        };

        // (a) coverage-complete: every enrolled device (self + sibling)
        //     has ingested → removed.
        let (key_covered, entry_covered) =
            make_entry([1; 16], [1; 32], now_ms - 1_000, &[SELF_ID, SIBLING_ID]);
        // (b) TTL-expired (strictly past the deadline) → removed even
        //     though coverage is incomplete.
        let (key_ttl, entry_ttl) = make_entry([2; 16], [2; 32], 500, &[SELF_ID]);
        // (c) fresh + coverage-incomplete → retained.
        let (key_fresh, entry_fresh) = make_entry([3; 16], [3; 32], now_ms - 1_000, &[SELF_ID]);
        // (d) TTL boundary pin: deposited_at + TTL == now is NOT yet
        //     expired (strict `<`) → retained.
        let (key_edge, entry_edge) =
            make_entry([4; 16], [4; 32], now_ms - INBOX_TTL_MS, &[SELF_ID]);

        let mut doc = DmInboxDoc::default();
        doc.entries
            .insert(key_covered.clone(), entry_covered.clone());
        doc.entries.insert(key_ttl.clone(), entry_ttl);
        doc.entries.insert(key_fresh.clone(), entry_fresh);
        doc.entries.insert(key_edge.clone(), entry_edge);

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "GC removals are doc mutations");
        assert!(
            !doc.entries.contains_key(&key_covered),
            "ig ⊇ enrolled set → coverage GC"
        );
        assert!(!doc.entries.contains_key(&key_ttl), "past TTL → GC");
        assert!(
            doc.entries.contains_key(&key_fresh),
            "fresh + uncovered → retained"
        );
        assert!(
            doc.entries.contains_key(&key_edge),
            "TTL is strict `<` — boundary retained"
        );
        assert!(
            ctx.calls().is_empty(),
            "all entries already carried self in ig — GC must not re-ingest"
        );

        // Empty-enrolled guard: ig ⊇ ∅ is vacuously true, so an empty
        // provider snapshot must NOT wipe the inbox (TTL still applies).
        let ctx_empty = ProbeCtx {
            enrolled: BTreeSet::new(),
            now_ms,
            ..ProbeCtx::new()
        };
        let (key_g, entry_g) = make_entry([5; 16], [5; 32], now_ms - 1_000, &[SELF_ID]);
        let mut doc_g = DmInboxDoc::default();
        doc_g.entries.insert(key_g.clone(), entry_g);
        let changed = ingest_pending(&mut doc_g, &ctx_empty).await;
        assert!(!changed);
        assert!(
            doc_g.entries.contains_key(&key_g),
            "empty enrolled set must not coverage-GC anything"
        );

        // Resurrection-by-merge (the churn-tolerant model): a sibling that
        // GC'd LATER re-merges its old doc — the removed entry reappears
        // (insert-once sees the missing key as new)...
        let mut stale = DmInboxDoc::default();
        stale.entries.insert(key_covered.clone(), entry_covered);
        let out = doc.merge_from(stale);
        assert!(out.changed, "resurrection is a visible merge change");
        assert!(doc.entries.contains_key(&key_covered));

        // ...and the NEXT sweep removes it again without re-ingesting
        // (self is already in the resurrected ig): GC criteria are
        // deterministic on (ig, now), so every replica converges on
        // removal and once all have GC'd no copy remains to resurrect.
        let applied_before = ctx.applied().len();
        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed);
        assert!(
            !doc.entries.contains_key(&key_covered),
            "re-GC after resurrection-by-merge converges"
        );
        assert_eq!(
            ctx.applied().len(),
            applied_before,
            "no duplicate ingestion/emit for a resurrected entry"
        );
    }
}
