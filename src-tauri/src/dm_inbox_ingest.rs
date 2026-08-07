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
//! handles using the shared `pub(crate)` receive-path helpers
//! (`dm_outbox::verify_cidnotify_admission`,
//! `dm_outbox::decrypt_and_bind_dm_blob`,
//! `dm_outbox::dm_received_event_payload`). This module is the ONLY live
//! DM receive entry point (ZEB-710 deleted the unused direct handler).
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

// Only the test module references `INBOX_TTL_MS` directly now; the production
// TTL comparison moved into `DmInboxDoc::gc_expired` (ZEB-851).
#[cfg(test)]
use crate::butler_deposit::INBOX_TTL_MS;
use crate::dm_inbox_crdt::{DmInboxDoc, DmInboxEntry};
use crate::owner_state_types::{ContentId, Hlc, InboxEntry, OwnerAddr, ReceivedMessage, SpaceId};

/// Debounce window between an `on_applied` nudge and the sweep it triggers:
/// a burst of merges (e.g. initial fan-in right after coming online)
/// coalesces into one sweep.
pub const INGEST_SWEEP_DEBOUNCE_MS: u64 = 250;

/// The verified, decrypted fields ingestion needs from one deposited entry:
/// exactly what `apply_inbox` and the `dm-received` emit consume. Produced
/// by [`DmInboxIngestCtx::verify`] (production: the shared receive-path
/// helpers, `dm_outbox::verify_cidnotify_admission` +
/// `decrypt_and_bind_dm_blob`).
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
    /// Production (Task 7), reusing the shared `pub(crate)` receive-path
    /// helpers:
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

    /// ZEB-505: apply a recovered INVITE-ONLY deposit (`cidnotify_packet` is
    /// `None`) — bootstrap the DM Space from the deposited invite ALONE (no
    /// message). Verifies the invite (signature + `inviter` bound to the
    /// deposit entry's `sender_owner`) and applies it pinned to that sender,
    /// without refreshing the OwnerDeviceCache (deposit-recover semantics). An
    /// `Err` leaves the entry PENDING for retry, like `verify`.
    async fn apply_invite_only(&self, entry: &DmInboxEntry) -> Result<(), String>;

    /// ZEB-691: apply a deposited device-revocation entry. Re-verifies the certs
    /// (never trust the butler — it pre-validated, but a sibling doc merge is
    /// also a trust boundary), applies via `handle_revocation_push`, and marks
    /// owner-state dirty on a genuine insert. Returns `Ok(inserted)`. An `Err`
    /// leaves the entry PENDING for retry, like the message/invite arms.
    async fn apply_revocation(&self, entry: &DmInboxEntry) -> Result<bool, String>;

    /// ZEB-674 (C4): apply a deposited file-share grant entry. Decodes the
    /// opaque `grant_push`, opens the per-device blob sealed to THIS device (if
    /// any), re-seals the recovered DEK under the grantee's shared KeyTree, and
    /// records it on `OwnerState.received_file_grants` — marking owner-state
    /// dirty when a new record lands (`received_file_grants` has no deposit-rung
    /// re-delivery backstop, the entry is GC'd once covered). `Ok(())` on a
    /// recorded grant OR when no blob was sealed to this device (a sibling will
    /// record it and replicate via Flow A — either way THIS device is done, so
    /// the caller marks it ingested). An `Err` leaves the entry PENDING for retry
    /// (the TTL GC is the safety valve), like the message/invite/revocation arms.
    async fn apply_grant_push(&self, entry: &DmInboxEntry) -> Result<(), String>;

    /// ZEB-730: apply a deposited file-grant REVOKE entry (owner→grantee).
    /// Decodes the revoked root ContentId from `grant_revoke`, then — under the
    /// owner-state lock — honors the revoke ONLY when the butler-verified deposit
    /// sender (`OwnerAddr(entry.sender_owner)`, NEVER a payload claim) matches the
    /// granter-of-record on the local received grant (`ingest_grant_revoke`'s
    /// griefing guard), reusing ZEB-727's dismiss tombstone. On a genuine change
    /// it marks owner-state dirty (`received_file_grants`/`dismissed_received_grants`
    /// have no deposit-rung re-delivery backstop — ZEB-709) AND emits
    /// `"shared-with-me-updated"` so the grantee UI drops the entry. An
    /// unauthorized / absent-entry revoke is a silent `Ok(())` no-op (a dropped
    /// revoke is not an error) — mark nothing, emit nothing. An `Err` (malformed
    /// wire bytes) leaves the entry PENDING for retry, like the other arms.
    async fn apply_grant_revoke(&self, entry: &DmInboxEntry) -> Result<(), String>;

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
        // ZEB-691: a PURE device-revocation deposit (no cidnotify, no invite).
        // Apply it before the invite-only branch would otherwise swallow the
        // `cidnotify_packet == None` case. The guard is intentionally a
        // pure-revocation check, NOT a bare `revocation_push.is_some()`:
        // `revocation_push` rides UNCONDITIONALLY on the persisted entry, so a
        // message (cidnotify Some) or invite entry could also carry a
        // stray/malicious `revocation_push` — a bare `is_some()` would route that
        // entry into this arm and `continue`, DROPPING the real message/invite.
        if entry.revocation_push.is_some()
            && entry.cidnotify_packet.is_none()
            && entry.invite_packet.is_none()
            && entry.grant_push.is_none()
            && entry.grant_revoke.is_none()
        {
            match ctx.apply_revocation(entry).await {
                Ok(_) => {
                    entry.ingested_by.insert(self_id.clone());
                    changed = true;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = %key,
                        "ZEB-691: revocation recover failed; leaving entry pending for retry"
                    );
                }
            }
            continue;
        }
        // ZEB-674 (C4): a PURE file-share grant deposit (no cidnotify, no
        // invite, no revocation). Apply it before the invite-only branch would
        // otherwise swallow the `cidnotify_packet == None` case. The guard is a
        // pure-shape check (mirroring the revocation arm): `grant_push` rides
        // on the persisted entry, so requiring the other sub-payloads absent
        // keeps a message/invite/revocation entry that somehow carried a stray
        // grant from being mis-routed here (the butler's pure-shape guards
        // already reject that shape, but a sibling doc merge is a trust
        // boundary too, so re-check).
        if entry.grant_push.is_some()
            && entry.cidnotify_packet.is_none()
            && entry.invite_packet.is_none()
            && entry.revocation_push.is_none()
            && entry.grant_revoke.is_none()
        {
            match ctx.apply_grant_push(entry).await {
                Ok(()) => {
                    entry.ingested_by.insert(self_id.clone());
                    changed = true;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = %key,
                        "ZEB-674: grant recover failed; leaving entry pending for retry"
                    );
                }
            }
            continue;
        }
        // ZEB-730: a PURE file-grant REVOKE deposit (owner→grantee) — no
        // cidnotify, no invite, no revocation, no grant. Apply it before the
        // invite-only branch would otherwise swallow the `cidnotify_packet ==
        // None` case. The guard is a pure-shape check mirroring the grant arm:
        // `grant_revoke` rides on the persisted entry, so requiring every other
        // sub-payload absent keeps a message/invite/revocation/grant entry that
        // somehow carried a stray revoke from being mis-routed here (the butler's
        // pure-shape guards already reject that shape, but a sibling doc merge is
        // a trust boundary too, so re-check). `apply_grant_revoke` authorizes by
        // granter-of-record and stamps ZEB-727's dismiss tombstone.
        if entry.grant_revoke.is_some()
            && entry.cidnotify_packet.is_none()
            && entry.invite_packet.is_none()
            && entry.revocation_push.is_none()
            && entry.grant_push.is_none()
        {
            match ctx.apply_grant_revoke(entry).await {
                Ok(()) => {
                    entry.ingested_by.insert(self_id.clone());
                    changed = true;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = %key,
                        "ZEB-730: grant-revoke recover failed; leaving entry pending for retry"
                    );
                }
            }
            continue;
        }
        // ZEB-505: invite-only entry (no CidNotify) — apply the bootstrap
        // invite ALONE (no blob, no message), then mark ingested. A failure
        // leaves it pending for retry, like the message path below.
        // ZEB-730: `grant_revoke.is_none()` excludes a stray revoke — a PURE
        // grant-revoke (also CidNotify-less) is already claimed by the arm above
        // and `continue`d; a malformed grant_revoke+sibling entry must not be
        // mis-applied as an invite (fail-closed, like the acceptor's guards).
        if entry.cidnotify_packet.is_none() && entry.grant_revoke.is_none() {
            match ctx.apply_invite_only(entry).await {
                Ok(()) => {
                    // Deliberate asymmetry vs. the co-deposit path below: this
                    // invite-only deposit is consumed here (marked ingested)
                    // once staged — the prompt is process-local, so if the
                    // process restarts before the user acts, the ONLY way it
                    // re-surfaces is the sender's next message co-depositing
                    // the invite again. The co-deposit path intentionally does
                    // NOT mark itself ingested this way; it stays pending
                    // (retried every sweep) until the invite is admitted.
                    entry.ingested_by.insert(self_id.clone());
                    changed = true;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = %key,
                        "ZEB-505 ingest: invite-only apply failed; left pending for retry"
                    );
                }
            }
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
    //   (b) TTL: `first_observed_ms[key] + INBOX_TTL_MS < now` (strict,
    //       local receipt — see below).
    //
    // Churn-tolerant model (resurrection-by-merge): a sibling that GC'd
    // later than us can re-merge an old doc and resurrect a removed entry
    // (the insert-once merge sees the missing key as new). The COVERAGE
    // criterion remains a deterministic function of (ingested_by, now):
    // `ingested_by` is grow-only with union merge, so every replica
    // evaluating a resurrected-but-covered entry reaches the same removable
    // verdict and the next sweep removes it again (a resurrected entry
    // arrives already covered, so it IS covered at that sweep's start).
    // Once every replica has GC'd, no copy remains to resurrect, so the
    // fleet converges WITHOUT tombstones.
    //
    // The TTL criterion is NOT fleet-wide deterministic (ZEB-851): it is
    // keyed off this replica's own LOCAL, non-replicated
    // `first_observed_ms`, lazily stamped on first local observation — a
    // per-replica SOFT deadline, not a shared one. A never-covered entry
    // resurrected by a still-holding peer re-stamps `first_observed_ms` on
    // this replica and gets a fresh TTL window, so it may persist beyond a
    // single TTL window in a continuously-merging fleet. This is bounded by
    // the store's caps and is the deliberately-safe direction —
    // over-retaining an undelivered DM beats dropping a live one.
    // Re-ingestion after a lost ig-update is likewise safe: `apply_inbox`
    // is idempotent on (space_id, message_cid), and the dm-received emit is
    // gated on its Inserted outcome.
    //
    // An expired entry that was just ingested above is still removed —
    // delivery beat the deadline; the deposit record's job is done.
    let now_ms = ctx.now_ms();
    // ZEB-851: expire from this replica's OWN first observation, not the
    // butler-minted `deposited_at` (a backdated deposit must not drop a DM as
    // pre-expired). `gc_expired` lazy-stamps on the first sweep that sees each
    // entry and borrows the side-map clone-free (see `DmInboxDoc::gc_expired`).
    changed |= doc.gc_expired(now_ms, &covered_at_start);

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
/// ZEB-862: local-only durable-persist trigger (an `engine.persist_now()`),
/// passed as a boxed-future closure so the sweeper stays decoupled from the
/// concrete engine type (mirroring the `notify_dirty` closure). Invoked when a
/// sweep adds first-observation stamps but removes nothing, so the durable TTL
/// clock reaches disk without a fleet republish.
pub type PersistNowFn = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), crate::fleet_sync::SyncError>> + Send>,
        > + Send
        + Sync,
>;

pub async fn run_dm_inbox_ingest_sweeper(
    doc: Arc<Mutex<DmInboxDoc>>,
    ctx: Arc<dyn DmInboxIngestCtx>,
    mut nudge_rx: mpsc::Receiver<()>,
    notify_dirty: Arc<dyn Fn() + Send + Sync>,
    persist_now: PersistNowFn,
    debounce: Duration,
) {
    // Startup sweep: ingest entries deposited while this device was
    // offline (they arrive via the engine's initial fan-in BEFORE
    // on_applied wiring can observe them, and via the persisted doc).
    sweep_once(&doc, ctx.as_ref(), notify_dirty.as_ref(), &persist_now).await;

    while nudge_rx.recv().await.is_some() {
        // Debounce: let the rest of a merge burst land, then drain any
        // extra nudges so the burst coalesces into one sweep.
        tokio::time::sleep(debounce).await;
        while nudge_rx.try_recv().is_ok() {}
        sweep_once(&doc, ctx.as_ref(), notify_dirty.as_ref(), &persist_now).await;
    }
    // recv() == None: every nudge sender dropped (engine shutdown).
}

/// CodeRabbit F1 (ZEB-482): reverse-resolve the owner an authenticated tunnel
/// peer belongs to. Scans the `OwnerDeviceCache` for the FIRST owner whose entry
/// carries a `DeviceTunnelContact` whose tunnel NodeId
/// (`blake3(pq_dsa_pubkey)`, via [`node_id_from_dsa_pubkey`]) equals the peer's
/// authenticated `peer_node_id`. This is the exact inverse of
/// `iroh_tunnel_dm_transport::resolve_owner_tunnel_targets` (owner → contacts →
/// NodeId), so a legitimate friend whose handshake populated the cache resolves
/// here. Returns `None` when no cached contact matches — the caller must then
/// REJECT the invite (an unbindable peer cannot be trusted to name an inviter).
///
/// The friend handshake (ZEB-473) is what populates each friend's
/// owner → devices → `DeviceTunnelContact`, so a real friend's invite binds; a
/// device we have never handshaked (no contact cached) is unbindable.
fn resolve_owner_for_peer(
    state: &crate::owner_state_crdt::OwnerState,
    peer_node_id: [u8; 32],
) -> Option<crate::owner_state_types::OwnerAddr> {
    state
        .owner_device_cache
        .devices
        .iter()
        .find(|(_owner, entry)| {
            entry
                .device_tunnel_contacts
                .iter()
                .flatten()
                .any(|contact| {
                    crate::tunnel_manager::node_id_from_dsa_pubkey(&contact.pq_dsa_pubkey)
                        == peer_node_id
                })
        })
        .map(|(owner, _entry)| *owner)
}

/// ZEB-473 (DM-over-iroh, Move 1a) Task 9: ingest ONE inbound DM packet
/// delivered live over the PQ tunnel, through the SAME verify → decrypt →
/// `apply_inbox` → `dm-received` sequence a deposit-delivered DM takes.
///
/// The tunnel carries only the sealed+signed CidNotify packet (NOT the
/// storage blob), exactly like the legacy direct receive path: the blob is
/// fetched from CAS by `signed.message_cid`. This is the load-bearing
/// difference from the butler/deposit ingest ([`ingest_pending`]), whose
/// blob is carried inline in the dm-inbox doc entry. The two paths therefore
/// share the *free* receive-path helpers (`verify_cidnotify_admission`,
/// `decrypt_and_bind_dm_blob`, `apply_inbox`, `dm_received_event_payload`)
/// but not a single ingest body — see the Task 9 plan note. Keeping this in
/// terms of those shared helpers is what keeps the trust path identical and
/// non-drifting across the two carriers.
///
/// ZEB-482: a `DmPacket::Invite` on the same tunnel is auto-accepted into the
/// DM Space (via the shared `dm_outbox::apply_invite`) and returns `Ok(false)`
/// with NO `dm-received` emit — it bootstraps the Space so the subsequent
/// `CidNotify` for that Space admits instead of rejecting `SpaceNotFound`. An
/// `Ack` on the tunnel ingest path is rejected (`Err`) — it is not handled
/// here (the read-ack carrier was removed with Reticulum).
///
/// Pipeline (the three-phase locked/unlocked/re-locked receive shape,
/// ZEB-241; deliberately WITHOUT a device-cache refresh or ack fan-out,
/// which the tunnel carrier does not owe — the cache is populated on the
/// friend handshake, Task 5, and the read-ack was removed with Reticulum):
///   1. `dm_envelope::decode_packet` → dispatch on the `DmPacket` variant
///      (`Invite` → `apply_invite`; the CidNotify steps below otherwise);
///   2. under the owner-state lock: `verify_cidnotify_admission` (pubkey
///      lookup, signature, owner resolution + match, Space lookup, ZEB-275
///      SpaceKind gate, membership);
///   3. CAS fetch the storage blob by `signed.message_cid` (500ms timeout,
///      matching the direct path's Phase B);
///   4. `decrypt_and_bind_dm_blob` (AAD, content_key + prior-keys fallback,
///      sender-impersonation defense);
///   5. `apply_inbox` (idempotent on `(space_id, message_cid)` — the tunnel
///      copy dedups against any deposit copy of the same DM);
///   6. emit `dm-received` (the shared event name + payload builder) ONLY on
///      `ApplyOutcome::Inserted`, exactly the direct path's atomic-emit gate.
///
/// Returns `Ok(true)` when a NEW message was applied + emitted, `Ok(false)`
/// when the DM deduped (already in the inbox — direct/deposit arrival or a
/// duplicate tunnel frame; consumed without re-emitting), and `Err(reason)`
/// for any rejection (bad packet, unknown/forged sender, non-member, CAS
/// miss, decrypt failure). The caller (the tunnel drain) logs `Err` at warn
/// and drops the packet — a bad tunnel DM must never crash the drain, and
/// the deposit rung is the durability backstop for anything that should have
/// arrived.
// ZEB-236 (T3) added the `pending_invites` store handle; the receive-identity +
// verified-packet + peer-bind params already fill the arg list. Threading them
// through a struct would not clarify this single production call boundary.
// ZEB-710: `pub` (was `pub(crate)`) so the DM integration tests
// (`tests/dm/dm_cert_identity_integration.rs`, `dm_revocation_cutoff_integration.rs`)
// can drive the LIVE receive path directly — the same reachability the deleted
// `DmOutbox::handle_cidnotify_lifted` had.
#[allow(clippy::too_many_arguments)]
pub async fn ingest_dm_packet(
    crdt_state: &Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
    content_store: &Arc<dyn crate::content_store::ContentStore>,
    sink: &Arc<dyn crate::node_event_sink::NodeEventSink>,
    // ZEB-236 (T3): the process-local staged-invite store (cloned out of
    // `NodeState` by the tunnel drain). `Some` on every live route; `None` only
    // for callers with no store wired (unit tests). A non-friend `Invite` arm
    // stages here and surfaces via the `sink` above.
    pending_invites: Option<Arc<crate::pending_dm_invites::PendingDmInvites>>,
    self_owner: crate::owner_state_types::OwnerAddr,
    device_id: &str,
    // ZEB-482 (CodeRabbit F1): the authenticated tunnel peer's NodeId
    // (`blake3(peer ML-DSA pubkey)`, carried on `InboundDm::peer_node_id`). Used
    // ONLY by the `Invite` arm to bind the payload-controlled `inviter` field to
    // the device the frame actually arrived from — see `resolve_owner_for_peer`.
    peer_node_id: [u8; 32],
    packet_bytes: &[u8],
    // ZEB-580 S2: the shared-community revocation projection — forwarded to
    // `verify_cidnotify_admission` for the CidNotify signer-device cutoff.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    // ZEB-685 (S3): mark the owner-state engine dirty after a RevocationPush
    // inserts a NEW revoked key, so the friend-scoped store is persisted +
    // replicated to sibling devices (it has no deposit-rung backstop; without
    // this it is lost on restart). `None` for callers with no engine wired
    // (unit tests) — the mutation still lands in memory, just isn't flushed.
    notify_owner_state_dirty: Option<&(dyn Fn() + Send + Sync)>,
) -> Result<bool, String> {
    // 1. Decode + dispatch on the DmPacket variant (ZEB-482). The tunnel
    //    carries a discriminated `DmPacket`; today only `Invite` (DM-Space
    //    bootstrap) and `CidNotify` (the encrypted-blob notification) ride it.
    // ZEB-484: an inline blob (from a `CidNotifyWithBlob`) is carried out of the
    // dispatch here and CAS-put AFTER Phase 2 admission (see the 2b block) so a
    // rejected packet never causes a CAS write (Qodo). `None` for every other
    // variant.
    let mut inline_blob: Option<Vec<u8>> = None;
    let (signed, signature, signed_bytes) = match crate::dm_envelope::decode_packet(packet_bytes)
        .map_err(|e| format!("decode_packet: {e}"))?
    {
        // ZEB-482: a DM-Space invite — auto-accept it (write the Space +
        // cache the inviter) via the SAME trust gates the (dormant) outbox
        // `handle_invite` applies. Invites carry no `dm-received`, so this
        // returns `Ok(false)` without emitting. The Space MUST land before
        // the first CidNotify for this Space is admitted (SpaceNotFound
        // otherwise); the tunnel's in-order FIFO guarantees that ordering
        // because the invite is enqueued at Space-creation, the CidNotify
        // only at message-send.
        crate::dm_envelope::DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        } => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            // Capture for the ZEB-639 ignore log (`signed` moves into apply_invite).
            let invite_space_id = signed.space_id;
            // Scope the owner-state lock to the `apply_invite` call: the outcome
            // is owned, so we take it out and DROP the guard before staging +
            // emitting (the store `Mutex` + event sink must not nest inside the
            // held `crdt_state` lock — ZEB-236 T3).
            let outcome = {
                let mut state = crdt_state.lock().await;
                // CodeRabbit F1: resolve the owner the AUTHENTICATED tunnel peer
                // belongs to (reverse lookup over the OwnerDeviceCache populated by
                // the friend handshake) and bind `apply_invite` to it. An invite
                // whose `signed.inviter` claims a DIFFERENT owner than the sending
                // device is rejected before any cache/Space mutation. An invite we
                // CANNOT bind to a known owner (no cached contact matches the peer)
                // is also rejected — an unbindable invite must not be trusted.
                let expected_inviter =
                    resolve_owner_for_peer(&state, peer_node_id).ok_or_else(|| {
                        format!(
                        "apply_invite: unbindable tunnel peer {} (no cached owner contact matches)",
                        hex::encode(peer_node_id)
                    )
                    })?;
                crate::dm_outbox::apply_invite(
                    &mut state,
                    self_owner,
                    device_id,
                    signed,
                    signature,
                    &signed_bytes,
                    now_ms,
                    Some(expected_inviter),
                    // ZEB-483: authenticated tunnel path — refresh the cache.
                    true,
                    revoked,
                )
                .map_err(|e| format!("apply_invite: {e:?}"))?
            };
            match outcome {
                // ZEB-236 (T3): a non-friend invite is staged in the process-local
                // store and surfaced to the UI; an active-friend invite was already
                // auto-accepted (Space written) inside `apply_invite`.
                crate::dm_outbox::ApplyInviteOutcome::Staged(staged) => {
                    crate::pending_dm_invites::stage_and_emit_staged_invite(
                        pending_invites.as_ref(),
                        sink.as_ref(),
                        staged,
                    );
                }
                crate::dm_outbox::ApplyInviteOutcome::Accepted => {
                    // ZEB-709: the auto-accept wrote the DM Space (+ cache
                    // refresh) into owner-state — arm the owner-state flush.
                    // Staged/Ignored return before any owner-state write.
                    if let Some(mark_dirty) = notify_owner_state_dirty {
                        mark_dirty();
                    }
                    // ZEB-640 (1): a friend-tier auto-accept makes any EARLIER
                    // staged (pre-befriend) entry for this space stale — the
                    // Space now exists in nav, so purge the pending row.
                    crate::pending_dm_invites::purge_stale_staged_on_accept(
                        pending_invites.as_ref(),
                        sink.as_ref(),
                        &invite_space_id,
                    );
                }
                // ZEB-639: non-friend invite for a space we already hold.
                // ZEB-642 (1): a staged row for this space is stale by
                // definition once the space exists (the same argument that
                // blessed the co-deposit Ok(None) conflation) — purge it.
                // The helper emits dm-invite-list-changed only on actual
                // removal, so redeliveries stay event-quiet.
                crate::dm_outbox::ApplyInviteOutcome::IgnoredExistingSpace => {
                    tracing::debug!(
                        space_id = ?invite_space_id,
                        "tunnel invite ignored: space already exists locally (non-friend inviter)"
                    );
                    crate::pending_dm_invites::purge_stale_staged_on_accept(
                        pending_invites.as_ref(),
                        sink.as_ref(),
                        &invite_space_id,
                    );
                }
            }
            return Ok(false);
        }
        crate::dm_envelope::DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        } => (signed, signature, signed_bytes),
        crate::dm_envelope::DmPacket::Ack { .. } => {
            return Err(
                "tunnel DM packet is an Ack (not handled on the tunnel ingest path)".into(),
            );
        }
        crate::dm_envelope::DmPacket::RevocationPush {
            revocation,
            enrollment,
        } => {
            // ZEB-685 (S3): a friend-pushed device revocation. Resolve the
            // tunnel peer to its owner (the SAME bind apply_invite uses), then
            // verify + trust-bind + store + feed the projection. A control frame:
            // never delivered as a chat message.
            let mut state = crdt_state.lock().await;
            let expected_owner = resolve_owner_for_peer(&state, peer_node_id).ok_or_else(|| {
                format!(
                    "revocation_push: unbindable tunnel peer {}",
                    hex::encode(peer_node_id)
                )
            })?;
            match crate::dm_outbox::handle_revocation_push(
                &mut state,
                expected_owner,
                &revocation,
                &enrollment,
                revoked,
            ) {
                Ok(inserted) => {
                    // Persist + replicate the friend-scoped store ONLY on a
                    // genuine new insert. Drop the owner-state lock first: the
                    // engine's own publish path takes it (notify_dirty itself is
                    // non-blocking, but keep the ordering obviously deadlock-free).
                    drop(state);
                    if inserted {
                        if let Some(mark_dirty) = notify_owner_state_dirty {
                            mark_dirty();
                        }
                    }
                    tracing::info!(
                        owner = %hex::encode(expected_owner.0),
                        inserted,
                        "ZEB-685: applied friend RevocationPush"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "ZEB-685: rejected RevocationPush")
                }
            }
            return Ok(false);
        }
        crate::dm_envelope::DmPacket::CidNotifyWithBlob {
            signed,
            signature,
            signed_bytes,
            storage_blob,
        } => {
            // ZEB-484 (Move 1c): the live tunnel carried the encrypted blob
            // inline. Carry it past the dispatch; it is CAS-put AFTER Phase 2
            // admission (block 2b) — never here, so a rejected/invalid packet
            // cannot pollute the CAS (Qodo). The rest of the pipeline is the bare
            // CidNotify path verbatim.
            inline_blob = Some(storage_blob);
            (signed, signature, signed_bytes)
        }
        crate::dm_envelope::DmPacket::ReadReceipt {
            signed,
            signature,
            signed_bytes,
        } => {
            // ZEB-214: an opt-in read-receipt watermark. Verify against the
            // CURRENT cache (same admission chain as CidNotify), then emit
            // `dm-read-receipt`. A control frame: never a chat message, so this
            // returns Ok(false) without touching the inbox.
            let resolved = {
                let state = crdt_state.lock().await;
                crate::dm_outbox::verify_read_receipt_admission(
                    &state,
                    &signed,
                    &signature,
                    &signed_bytes,
                    revoked,
                )
            };
            match resolved {
                Ok(resolved_owner) => {
                    crate::node_event_sink::emit_ser(
                        sink.as_ref(),
                        crate::dm_read_receipt::DM_READ_RECEIPT_EVENT,
                        &crate::dm_read_receipt::dm_read_receipt_event_payload(
                            signed.space_id,
                            resolved_owner,
                            &signed.read_up_to,
                            signed.sent_at.wall_ms,
                        ),
                    );
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "ZEB-214: rejected read receipt; dropping");
                }
            }
            return Ok(false);
        }
    };

    // 2. Admission under the owner-state lock — the SAME verification a
    //    direct/deposit arrival runs. We only carry `resolved_owner` past the
    //    lock: the Space snapshot here is intentionally DROPPED rather than
    //    reused, because the slow CAS fetch below opens a TOCTOU window. The
    //    fresh Space (and its current content_key) is re-fetched + re-validated
    //    under a second lock in step 4 (Phase C), mirroring the direct path.
    let resolved_owner = {
        let state = crdt_state.lock().await;
        let (_space, resolved_owner, _identity_pub) = crate::dm_outbox::verify_cidnotify_admission(
            &state,
            &signed,
            &signature,
            &signed_bytes,
            revoked,
        )
        .map_err(|e| format!("verify_cidnotify_admission: {e:?}"))?;
        resolved_owner
    };

    // 2b. ZEB-484 (Move 1c): if the live tunnel carried the blob inline, CAS-put
    //     it NOW — AFTER Phase 2 admission — so an invalid/rejected packet (bad
    //     signature, non-member, unknown device) NEVER causes a CAS write (Qodo:
    //     no cache pollution from unadmitted tunnel traffic). Bind the blob to the
    //     signed `message_cid` by content-addressing BEFORE writing: a mismatch
    //     fails closed with no write. Phase 3's `get(message_cid)` then hits this
    //     local put, exactly like the butler/deposit path it mirrors.
    if let Some(inline_blob) = inline_blob {
        let inline_cid = harmony_content::cid::ContentId::for_book(
            &inline_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .map_err(|e| format!("CidNotifyWithBlob for_book: {e:?}"))?;
        if inline_cid != signed.message_cid {
            return Err("CidNotifyWithBlob: inline blob CID does not match message_cid".into());
        }
        content_store
            .put(signed.message_cid, inline_blob)
            .await
            .map_err(|e| format!("CidNotifyWithBlob CAS put: {e:?}"))?;
    }

    // 3. Read the storage blob from LOCAL CAS by the packet's message_cid. The
    //    blob is already local: either block 2b just put the inline one, or the
    //    deposit/community-relay sweeper populated it. `get_local` (Greptile) —
    //    NOT `get`/`GetOrFetch` — because an encrypted DM CID is never network-
    //    servable anyway (content-serve refuses encrypted CIDs), so a network
    //    fetch could only waste a round-trip + leak the CID; a local miss falls
    //    through to the deposit path. 500ms timeout guards the event-loop hop.
    let blob = match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        content_store.get_local(&signed.message_cid),
    )
    .await
    {
        Ok(Ok(Some(bytes))) => bytes,
        Ok(Ok(None)) => return Err("CAS fetch: blob not found for message_cid".into()),
        Ok(Err(e)) => return Err(format!("CAS fetch: {e:?}")),
        Err(_) => return Err("CAS fetch: 500ms timeout".into()),
    };

    // 3b. Blob ↔ packet binding: the fetched storage blob MUST hash to the
    //     packet's signed `message_cid` under the DM send path's flags. The CAS
    //     `get` is keyed by `message_cid`, but a poisoned local CAS (or a
    //     backend returning mismatched bytes) could hand back a blob whose CID
    //     differs from the signed one — then the inbox entry would be keyed by
    //     the signed CID while the emitted/decrypted body came from a different
    //     blob. Recompute + compare BEFORE decrypt. Mirrors the deposited-ingest
    //     verifier's binding check (CR3, ZEB-473).
    let computed_cid = harmony_content::cid::ContentId::for_book(
        &blob,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .map_err(|e| format!("CAS fetch: for_book: {e:?}"))?;
    if computed_cid != signed.message_cid {
        return Err("CAS fetch: blob CID does not match message_cid".into());
    }

    // 4. Re-lock + TOCTOU re-check + decrypt + apply — Phase C of the
    //    receive shape, held under ONE lock acquisition.
    //    The `space` snapshot taken in step 2 is now stale: during the slow CAS
    //    fetch the Space could have been deleted, the sender could have lost
    //    membership (a GroupDm kick), or the `content_key` could have rotated.
    //    Re-fetch the Space from `OwnerState.spaces` by id and re-verify the
    //    SpaceKind gate + membership before decrypting — otherwise a tunnel DM
    //    from a now-revoked member (or against a deleted/rotated Space) could
    //    still be decrypted + applied, diverging from the direct path's trust.
    //    On a failed re-check we reject (Err → drain logs + drops): the deposit
    //    rung is the durability backstop for anything that should have arrived.
    //    `received_at` is this device's wall clock (the tunnel has no deposit
    //    timestamp); `from` is the resolved sender owner.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let inbox_entry = InboxEntry {
        space_id: signed.space_id,
        message_cid: signed.message_cid,
        from: resolved_owner,
        received_at: Hlc {
            wall_ms: now_ms,
            logical: 0,
            device_id: device_id.to_string(),
        },
    };
    let (payload, newly_inserted) = {
        let mut state = crdt_state.lock().await;

        // TOCTOU re-check: re-fetch the Space (it may have been deleted), then
        // re-verify the SpaceKind gate + membership against the FRESH state —
        // the Phase C re-check of the receive shape.
        let space_c = state
            .spaces
            .get(&signed.space_id)
            .cloned()
            .ok_or_else(|| "TOCTOU re-check: Space deleted during CAS fetch".to_string())?;
        if !matches!(
            space_c.kind,
            crate::owner_state_types::SpaceKind::Dm | crate::owner_state_types::SpaceKind::GroupDm
        ) {
            return Err("TOCTOU re-check: Space kind no longer DM/GroupDm".into());
        }
        if !space_c.members.contains(&resolved_owner) {
            return Err("TOCTOU re-check: sender lost membership during CAS fetch".into());
        }

        // Decrypt with the FRESH space (+ its current content_key / prior keys),
        // so a rotation that landed during the CAS fetch is reflected here.
        let payload = crate::dm_outbox::decrypt_and_bind_dm_blob(&space_c, &blob, resolved_owner)
            .map_err(|e| format!("decrypt_and_bind_dm_blob: {e:?}"))?;

        let newly_inserted = match state.apply_inbox(inbox_entry.clone()) {
            crate::owner_state_crdt::ApplyOutcome::Inserted => true,
            crate::owner_state_crdt::ApplyOutcome::Merged { .. } => false,
            crate::owner_state_crdt::ApplyOutcome::Rejected(reason) => {
                return Err(format!("apply_inbox rejected: {reason:?}"));
            }
        };
        (payload, newly_inserted)
    };

    // ZEB-709: arm the OWNER-STATE flush for the payload write (lock dropped
    // above; notify-on-insert only). Without this, a live-tunnel DM's sender
    // gets its Ack (sender stops re-sending) while the receiver's inbox entry
    // sits un-notified in memory — a crash before an unrelated owner-state
    // flush loses the message with no re-delivery path.
    if newly_inserted {
        if let Some(mark_dirty) = notify_owner_state_dirty {
            mark_dirty();
        }
    }

    // 6. Emit the SAME dm-received event the direct/deposit paths emit, gated
    //    on Inserted so a deduped tunnel copy never re-emits.
    if newly_inserted {
        let rm = ReceivedMessage {
            inbox_entry,
            body: payload.body,
            mime_type: payload.mime_type,
            sent_at: payload.sent_at,
        };
        crate::node_event_sink::emit_ser(
            sink.as_ref(),
            crate::dm_outbox::DM_RECEIVED_EVENT,
            &crate::dm_outbox::dm_received_event_payload(&rm),
        );
    }
    Ok(newly_inserted)
}

/// Production [`DmInboxIngestCtx`] over real `start_node` handles (ZEB-418
/// P1 Task 7). Each method implements its trait doc's production contract
/// verbatim:
///
/// * CAS = the shared `RuntimeContentStore` (`ContentStore::put`,
///   idempotent on content address);
/// * verification = the shared `pub(crate)` receive-path helpers
///   (`dm_outbox::verify_cidnotify_admission` +
///   `dm_outbox::decrypt_and_bind_dm_blob`) under the owner-state lock —
///   the SAME trust path every arrival takes;
/// * the UI emit = the shared `dm_outbox::DM_RECEIVED_EVENT` const +
///   `dm_received_event_payload` builder;
/// * `enrolled` = a start_node-time snapshot of the `harmony_owner`
///   `OwnerState.enrollments` values mapped through
///   `hex::encode(cert.device_pubkeys.classical.ed25519_verify)` (the SP1
///   64-hex device-id form). Enrollment changes require a node restart, so
///   the snapshot tracks the enrolled set for the engine's lifetime.
pub struct ProdDmInboxIngestCtx {
    /// This device's SP1 device id (64-hex of the device ed25519 verify key).
    pub device_id: String,
    /// ZEB-483: this node's own `OwnerAddr` (the deposit recipient). Threaded
    /// into `apply_deposited_invite`'s `apply_invite` recipient-membership gate
    /// when bootstrapping the DM Space from a deposited invite on recover.
    pub self_owner: OwnerAddr,
    /// The runtime owner-state CRDT (`NodeState`'s `crdt_state` Arc).
    pub crdt_state: Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
    /// The shared CAS handle (`RuntimeContentStore` in production).
    pub content_store: Arc<dyn crate::content_store::ContentStore>,
    /// Event sink for the `dm-received` emit (ZEB-445) and the ZEB-236
    /// `dm-invite-received` / `dm-invite-list-changed` emits.
    pub sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    /// ZEB-236 (T3): the process-local staged-invite store (from `NodeState`).
    /// A deposited non-friend invite (invite-only or co-deposited) is staged
    /// here + surfaced via `sink`. `None` only in unit tests with no store.
    pub pending_dm_invites: Option<std::sync::Arc<crate::pending_dm_invites::PendingDmInvites>>,
    /// Enrolled device ids (64-hex), snapshotted at start_node.
    pub enrolled: BTreeSet<String>,
    /// ZEB-580 S2: the shared-community revocation projection — a bare
    /// (Arc-backed, cheap-clone) handle matching `MembershipProjection`'s
    /// by-value style. Forwarded to `verify_cidnotify_sender_binding` for the
    /// CidNotify signer-device cutoff.
    pub revoked: crate::revoked_device_projection::RevokedDeviceProjection,
    /// ZEB-691: owner-state SyncEngine dirty hook. A deposited revocation entry
    /// is eventually GC'd, so unlike CidNotify/invite (which lean on
    /// re-delivery), the recover MUST persist the owner-state mutation itself.
    /// `None` only in unit tests that assert without persistence.
    pub notify_owner_state_dirty: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// ZEB-674 (C4): this device's X25519 private key, derived at start_node via
    /// `dm_signing::ed25519_priv_to_x25519` from the device ed25519 signing key
    /// (the SAME derivation the butler acceptor uses as its seal target — see
    /// `ProdRelayIngestCtx::device_x25519_privs` / `ProdButlerDepositCtx`). Used
    /// by `apply_grant_push` to open the per-device grant blob sealed to us.
    pub device_x25519_priv: zeroize::Zeroizing<[u8; 32]>,
    /// ZEB-674 (C4): the grantee's own shared `KeyTree` (the pinned epoch-0
    /// fleet tree — the SAME one `file_deks` seals under). `apply_grant_push`
    /// re-seals the recovered DEK under it so any of the owner's bound devices
    /// can open the stored grant (Flow A).
    pub owner_keytree: std::sync::Arc<crate::owner_state_crypto::KeyTree>,
}

#[async_trait]
impl DmInboxIngestCtx for ProdDmInboxIngestCtx {
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
        // 1. Decode the deposited packet — must be a CidNotify. (ZEB-505
        //    invite-only entries are applied by `apply_invite_only` and never
        //    reach here.)
        let cidnotify = entry
            .cidnotify_packet
            .as_deref()
            .ok_or("verify called on an invite-only deposit (no CidNotify)")?;
        let packet = crate::dm_envelope::decode_packet(cidnotify)
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
        // ZEB-640 (1): set inside the lock scope when `apply_deposited_invite`
        // consumed a co-deposited invite WITHOUT staging (`Ok(None)`:
        // friend-tier auto-accept, or the Space already exists) — any EARLIER
        // staged (pre-befriend) entry for this space is stale. The purge itself
        // runs AFTER the lock scope (helper caller contract).
        // ZEB-642 (2): dm_inbox's skip-window CLOSES BEFORE blob↔packet binding — the purge at lock-drop precedes step 4, so a binding Err cannot skip it.
        let mut purge_stale_staged = false;
        let (space, resolved_owner) = {
            let mut state = self.crdt_state.lock().await;
            // ZEB-483 (CodeRabbit Critical): verify the CidNotify's signer
            // against the PRISTINE OwnerDeviceCache FIRST, then let any deposited
            // invite bootstrap ONLY the missing DM Space — pinned to that
            // verified sender. Applying the invite first would let it seed the
            // `device → owner → pub` rows the admission then reads, so a forged
            // invite + notify could verify against cache the untrusted invite
            // just wrote (circular trust). On the legitimate offline path the
            // sender is an existing friend whose device is already cached here,
            // so it resolves WITHOUT the invite; the invite supplies only the
            // Space. `?` leaves the entry pending so a later sibling-doc merge or
            // state catch-up retries (fail-closed).
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
            let staged_invite = if let Some(inv) = entry.invite_packet.as_ref() {
                match crate::dm_outbox::apply_deposited_invite(
                    &mut state,
                    self.self_owner,
                    &self.self_device_id(),
                    inv,
                    resolved_owner,
                    signed.space_id,
                    signed.signing_device_hash,
                    identity_pub,
                    self.now_ms(),
                    &self.revoked,
                )? {
                    Some(staged) => Some(staged),
                    // ZEB-640 (1): invite consumed without staging (friend-tier
                    // accept / space exists) → flag the post-lock purge.
                    None => {
                        purge_stale_staged = true;
                        // ZEB-709: the friend-tier accept may have written the
                        // DM Space into owner-state inside this lock — arm the
                        // flush HERE so even the deferred-message error exits
                        // below keep it durable (the entry stays pending, but
                        // the in-memory Space write already happened). A
                        // spurious arm on the space-already-exists no-op is a
                        // harmless debounced persist of unchanged state.
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
            // the invite so the user is prompted, and leave the message deferred
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
                            // re-prompts on THIS SAME message's sweep
                            // redeliveries (this invite stays unacked while
                            // pending, so the deposit sweeper re-delivers it).
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
            (space, resolved_owner)
        };
        // ZEB-640 (1): the co-deposited invite auto-accepted (or the Space
        // already existed) — the lock is dropped now, so purge any stale
        // staged (pre-befriend) entry for this space.
        if purge_stale_staged {
            crate::pending_dm_invites::purge_stale_staged_on_accept(
                self.pending_dm_invites.as_ref(),
                self.sink.as_ref(),
                &signed.space_id,
            );
        }
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

    async fn apply_invite_only(&self, entry: &DmInboxEntry) -> Result<(), String> {
        let invite = entry
            .invite_packet
            .as_deref()
            .ok_or("ZEB-505: invite-only deposit missing invite_packet")?;
        let packet =
            crate::dm_envelope::decode_packet(invite).map_err(|e| format!("decode invite: {e}"))?;
        let crate::dm_envelope::DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        } = packet
        else {
            return Err("ZEB-505: invite-only deposit packet is not an Invite".into());
        };
        // Bind the invite's claimed inviter to the deposit entry's verified
        // sender (mirrors the message path's sender-consistency check).
        if signed.inviter.0 != entry.sender_owner {
            return Err("ZEB-505: invite inviter does not match deposit entry sender".into());
        }
        // Capture for the ZEB-639 ignore log (`signed` moves into apply_invite).
        let invite_space_id = signed.space_id;
        // Scope the lock to `apply_invite`, then DROP the guard before staging +
        // emitting (store `Mutex` + sink must not nest inside the held lock).
        let outcome = {
            let mut state = self.crdt_state.lock().await;
            crate::dm_outbox::apply_invite(
                &mut state,
                self.self_owner,
                &self.self_device_id(),
                signed,
                signature,
                &signed_bytes,
                self.now_ms(),
                Some(crate::owner_state_types::OwnerAddr(entry.sender_owner)),
                // Deposit-recover: never refresh the OwnerDeviceCache from a
                // deposited invite (it would let an untrusted invite seed cache rows).
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
                Ok(())
            }
            crate::dm_outbox::ApplyInviteOutcome::Accepted => {
                // ZEB-709: the auto-accept wrote the DM Space into owner-state
                // — arm the owner-state flush (Staged writes only the
                // process-local pending store; IgnoredExistingSpace writes
                // nothing; cache refresh is disabled on this path).
                if let Some(mark) = &self.notify_owner_state_dirty {
                    mark();
                }
                // ZEB-640 (1): a friend-tier auto-accept makes any EARLIER
                // staged (pre-befriend) entry for this space stale — the Space
                // now exists in nav, so purge the pending row.
                crate::pending_dm_invites::purge_stale_staged_on_accept(
                    self.pending_dm_invites.as_ref(),
                    self.sink.as_ref(),
                    &invite_space_id,
                );
                Ok(())
            }
            // ZEB-639: non-friend invite for a space we already hold.
            // ZEB-642 (1): purge the stale staged row (see tunnel arm).
            crate::dm_outbox::ApplyInviteOutcome::IgnoredExistingSpace => {
                tracing::debug!(
                    space_id = ?invite_space_id,
                    "deposited invite ignored: space already exists locally (non-friend inviter)"
                );
                crate::pending_dm_invites::purge_stale_staged_on_accept(
                    self.pending_dm_invites.as_ref(),
                    self.sink.as_ref(),
                    &invite_space_id,
                );
                Ok(())
            }
        }
    }

    async fn apply_revocation(&self, entry: &DmInboxEntry) -> Result<bool, String> {
        // Never trust the carrier: the butler pre-validated the deposit, but a
        // sibling doc merge is also a trust boundary, so re-run the FULL
        // `handle_revocation_push` verification (master-signed revocation +
        // paired enrollment, trust-bound to the depositing friend).
        let rp = entry
            .revocation_push
            .as_deref()
            .ok_or("apply_revocation: entry has no revocation_push")?;
        let packet = crate::dm_envelope::decode_packet(rp)
            .map_err(|e| format!("decode revocation_push: {e}"))?;
        let crate::dm_envelope::DmPacket::RevocationPush {
            revocation,
            enrollment,
        } = packet
        else {
            return Err("revocation_push is not a RevocationPush packet".into());
        };
        let inserted = {
            let mut state = self.crdt_state.lock().await;
            crate::dm_outbox::handle_revocation_push(
                &mut state,
                OwnerAddr(entry.sender_owner),
                &revocation,
                &enrollment,
                &self.revoked,
            )
            .map_err(|e| format!("handle_revocation_push: {e:?}"))?
        };
        // The revoked-device store lives in the owner-state CRDT and has NO
        // deposit-rung re-delivery backstop (the entry is GC'd once covered), so
        // persistence + sibling replication MUST come from notify_dirty here —
        // ONLY on a genuine new insert (an idempotent re-apply must not churn).
        if inserted {
            if let Some(mark) = &self.notify_owner_state_dirty {
                mark();
            }
        }
        Ok(inserted)
    }

    async fn apply_grant_push(&self, entry: &DmInboxEntry) -> Result<(), String> {
        let gp = entry
            .grant_push
            .as_deref()
            .ok_or("apply_grant_push: entry has no grant_push")?;
        // Open + re-seal + record under the owner-state lock. `granter_owner` is
        // the butler-verified deposit sender (never the payload's claim — the
        // per-device seal does not authenticate the granter).
        let recorded = {
            let mut state = self.crdt_state.lock().await;
            crate::file_sharing::ingest_grant_push(
                &mut state,
                &self.device_x25519_priv,
                &self.owner_keytree,
                OwnerAddr(entry.sender_owner),
                gp,
            )
            .map_err(|e| format!("ingest_grant_push: {e}"))?
        };
        // `received_file_grants` lives in the owner-state CRDT and has NO
        // deposit-rung re-delivery backstop (the entry is GC'd once covered), so
        // persistence + sibling (Flow A) replication MUST come from notify_dirty
        // here — ONLY when a grant was actually recorded (`Some`). A `None` (no
        // blob sealed to this device) mutated nothing; a sibling records it and
        // replicates it back to us, so we do not churn a persist.
        if let Some(cid) = recorded {
            if let Some(mark) = &self.notify_owner_state_dirty {
                mark();
            }
            // ZEB-723: nudge the grantee UI to refresh "Shared with me" + bump
            // its unread badge. Gated on a genuine new record (Some) exactly like
            // notify_dirty — an idempotent re-apply (None) mutated nothing and
            // must not re-emit. Payload mirrors `grants-updated` ({ cid }); the
            // frontend re-queries `list_received_grants` rather than trusting it.
            crate::node_event_sink::emit_ser(
                self.sink.as_ref(),
                "shared-with-me-updated",
                &serde_json::json!({ "cid": hex::encode(cid.to_bytes()) }),
            );
        }
        Ok(())
    }

    async fn apply_grant_revoke(&self, entry: &DmInboxEntry) -> Result<(), String> {
        let gr = entry
            .grant_revoke
            .as_deref()
            .ok_or("apply_grant_revoke: entry has no grant_revoke")?;
        // Decode the revoked root ContentId. The whole payload was frame-sealed to
        // us, so these bytes are authentic-to-transport; AUTHORIZATION (the
        // granter-of-record match) happens inside `ingest_grant_revoke` under the
        // lock — `sender_owner` is the butler-verified frame sender, never a
        // payload claim.
        let cid = crate::butler_deposit::decode_grant_revoke(gr)?;
        let changed = {
            let mut state = self.crdt_state.lock().await;
            crate::file_sharing::ingest_grant_revoke(
                &mut state,
                OwnerAddr(entry.sender_owner),
                cid,
                crate::file_sharing::now_epoch_ms(),
            )
        };
        // `received_file_grants`/`dismissed_received_grants` live in the owner-state
        // CRDT and have NO deposit-rung re-delivery backstop (the entry is GC'd once
        // covered), so persistence + sibling (Flow A) replication MUST come from
        // notify_dirty here — ONLY when the revoke actually changed state
        // (authorized AND a matching active grant existed). An unauthorized /
        // absent revoke mutated nothing and must not churn a persist or a UI
        // refresh (griefing guard).
        if changed {
            if let Some(mark) = &self.notify_owner_state_dirty {
                mark();
            }
            // ZEB-723 parity: nudge the grantee UI to refresh "Shared with me"
            // (the revoked grant disappears). Canonical lowercase-hex cid,
            // mirroring `apply_grant_push` / `grants-updated`.
            crate::node_event_sink::emit_ser(
                self.sink.as_ref(),
                "shared-with-me-updated",
                &serde_json::json!({ "cid": hex::encode(cid) }),
            );
        }
        Ok(())
    }

    async fn apply_inbox(&self, entry: InboxEntry) -> Result<bool, String> {
        let inserted = {
            let mut state = self.crdt_state.lock().await;
            match state.apply_inbox(entry) {
                crate::owner_state_crdt::ApplyOutcome::Inserted => true,
                crate::owner_state_crdt::ApplyOutcome::Merged { .. } => false,
                crate::owner_state_crdt::ApplyOutcome::Rejected(reason) => {
                    return Err(format!("apply_inbox rejected: {reason:?}"));
                }
            }
        };
        // ZEB-709: the payload write must arm the OWNER-STATE flush — the
        // `ingested_by` ack (dm-inbox DATASET engine, notified by the sweep)
        // otherwise persists + replicates while this entry sits un-notified
        // in memory, and a crash in that window loses the DM permanently
        // (restart skips via `ingested_by`; coverage-GC destroys the
        // deposit). Notify-on-insert only, mirroring `apply_revocation`.
        if inserted {
            if let Some(mark) = &self.notify_owner_state_dirty {
                mark();
            }
        }
        Ok(inserted)
    }

    fn emit_dm_received(&self, msg: &ReceivedMessage) {
        crate::node_event_sink::emit_ser(
            self.sink.as_ref(),
            crate::dm_outbox::DM_RECEIVED_EVENT,
            &crate::dm_outbox::dm_received_event_payload(msg),
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
    persist_now: &PersistNowFn,
) {
    let (changed, fo_grew) = {
        let mut guard = doc.lock().await;
        let before = guard.first_observed_ms().len();
        let changed = ingest_pending(&mut guard, ctx).await;
        // When `changed` is false the side-map cannot shrink (no entry was
        // removed, so its live-key prune removes nothing), so a length change
        // means `gc_expired` lazily stamped a newly-seen entry.
        (changed, guard.first_observed_ms().len() != before)
    };
    if changed {
        // `notify_dirty` schedules a debounced publish + persist, which also
        // captures any first-observation stamps added during this sweep.
        notify_dirty();
    } else if fo_grew {
        // ZEB-862: a stamp-only sweep added durable first-observation
        // timestamps but removed nothing. Persist them LOCALLY (no fleet
        // republish; the clock is serde-skip so the wire bytes are unchanged)
        // so the TTL survives restart.
        if let Err(e) = persist_now().await {
            tracing::warn!(error = %e, "ZEB-862: dm-inbox first-observed persist_now failed");
        }
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
    /// ZEB-674: deterministic seeds for the grant-path Prod-ctx test. The device
    /// ed25519 seed drives BOTH the ctx's X25519 private (via
    /// `ed25519_priv_to_x25519`) AND the pubkey a grant is sealed to (via
    /// `ed25519_pub_to_x25519`) — mirroring production's device-key derivation —
    /// and the keytree seed drives the grantee's shared re-seal tree.
    const TEST_DEVICE_ED25519_SEED: [u8; 32] = [0x33; 32];
    const TEST_KEYTREE_SEED: [u8; 32] = [0x44; 32];

    /// The X25519 public key a grant must be sealed to so `prod_ctx_with_dirty`'s
    /// device key opens it (production's `birational(vk)` seal target).
    fn test_device_x25519_pub() -> [u8; 32] {
        let sk = ed25519_dalek::SigningKey::from_bytes(&TEST_DEVICE_ED25519_SEED);
        crate::dm_signing::ed25519_pub_to_x25519(&sk.verifying_key().to_bytes())
            .expect("valid x25519 pub")
    }

    /// The grantee's shared KeyTree matching `prod_ctx_with_dirty`'s
    /// `owner_keytree` — used to open the re-sealed DEK from another "device".
    fn test_owner_keytree() -> crate::owner_state_crypto::KeyTree {
        crate::owner_state_crypto::KeyTree::derive(&TEST_KEYTREE_SEED).expect("keytree")
    }

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
            cidnotify_packet: Some(packet),
            storage_blob: format!("blob-{}", hex::encode(&cid[..4])).into_bytes(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
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
        /// ZEB-674: when true, `apply_grant_push` returns `Err` (entry stays
        /// pending for retry) instead of succeeding.
        grant_fail: bool,
        /// ZEB-730: when true, `apply_grant_revoke` returns `Err` (entry stays
        /// pending for retry) instead of succeeding.
        grant_revoke_fail: bool,
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
                grant_fail: false,
                grant_revoke_fail: false,
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
            let cn = entry
                .cidnotify_packet
                .as_deref()
                .expect("probe message entry has cidnotify_packet");
            let space: [u8; 16] = cn[..16].try_into().expect("probe packet space_id");
            let cid: [u8; 32] = cn[16..48].try_into().expect("probe packet cid");
            Ok(VerifiedDmDeposit {
                space_id: SpaceId(space),
                message_cid: ContentId::from_bytes(cid),
                body: b"hello".to_vec(),
                mime_type: "text/plain".into(),
                sent_at: hlc(42),
            })
        }

        async fn apply_invite_only(&self, _entry: &DmInboxEntry) -> Result<(), String> {
            self.calls.lock().unwrap().push("apply_invite_only".into());
            Ok(())
        }

        async fn apply_revocation(&self, _entry: &DmInboxEntry) -> Result<bool, String> {
            self.calls.lock().unwrap().push("apply_revocation".into());
            Ok(true)
        }

        async fn apply_grant_push(&self, _entry: &DmInboxEntry) -> Result<(), String> {
            self.calls.lock().unwrap().push("apply_grant_push".into());
            if self.grant_fail {
                return Err("simulated grant apply failure".into());
            }
            Ok(())
        }

        async fn apply_grant_revoke(&self, _entry: &DmInboxEntry) -> Result<(), String> {
            self.calls.lock().unwrap().push("apply_grant_revoke".into());
            if self.grant_revoke_fail {
                return Err("simulated grant_revoke apply failure".into());
            }
            Ok(())
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
    async fn ingest_routes_invite_only_entry_to_apply_invite_only_not_verify() {
        // ZEB-505: an entry with `cidnotify_packet: None` is a standalone
        // invite-only deposit. The sweep must route it to `apply_invite_only`
        // (bootstrap the Space from the invite ALONE) and SKIP the message
        // pipeline entirely — no CAS-put, no verify, no apply_inbox, no emit.
        let key = DmInboxDoc::invite_key(&[1; 16]);
        let entry = DmInboxEntry {
            sender_owner: SENDER_OWNER,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: Some(vec![0xAA, 0xBB, 0xCC]),
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
            deposited_at: hlc(500),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "invite-only ingest mutated the doc (ig growth)");

        // Routed to apply_invite_only ONLY — the message pipeline is skipped.
        assert_eq!(ctx.calls(), vec!["apply_invite_only"]);
        assert!(
            ctx.applied().is_empty(),
            "no message apply_inbox on invite-only"
        );
        assert!(
            ctx.emitted().is_empty(),
            "no dm-received emit on invite-only"
        );

        // Self added to the grow-only ig set: the entry is marked ingested.
        assert!(doc.entries[&key].ingested_by.contains(SELF_ID));
    }

    /// ZEB-674 (C4): a PURE grant entry (`grant_push: Some`, all other
    /// sub-payloads `None`) is routed to `apply_grant_push` and SKIPS the
    /// message/invite/revocation pipelines entirely — no CAS-put, no verify, no
    /// apply_inbox, no emit — then is marked ingested.
    #[tokio::test]
    async fn ingest_routes_grant_entry_to_apply_grant_push() {
        let key = DmInboxDoc::grant_key(&SENDER_OWNER, &[0xDE, 0xAD]);
        let entry = DmInboxEntry {
            sender_owner: SENDER_OWNER,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: Some(vec![0xDE, 0xAD]),
            grant_revoke: None,
            deposited_at: hlc(500),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "grant ingest mutated the doc (ig growth)");

        assert_eq!(ctx.calls(), vec!["apply_grant_push"]);
        assert!(
            ctx.applied().is_empty(),
            "no message apply_inbox on a grant"
        );
        assert!(ctx.emitted().is_empty(), "no dm-received emit on a grant");
        assert!(doc.entries[&key].ingested_by.contains(SELF_ID));
    }

    /// Whole-branch review (guard symmetry): an ADVERSARIAL sibling-doc entry
    /// carrying BOTH `revocation_push` AND `grant_push` (cidnotify + invite
    /// `None`) must NOT be claimed by the revocation arm. The revocation arm's
    /// `grant_push.is_none()` guard is symmetric with the grant arm's
    /// `revocation_push.is_none()` guard — each specialized arm fires ONLY for
    /// its exact pure shape. Without the guard, `revocation_push.is_some()`
    /// alone would route this entry into `apply_revocation`, which acks +
    /// consumes the entry and silently DROPS the grant (and would apply an
    /// attacker-chosen revocation riding a mixed payload). A sibling doc merge
    /// is a trust boundary, so this malformed shape — the honest butler never
    /// emits it — must be declined defensively rather than processed.
    #[tokio::test]
    async fn ingest_revocation_plus_grant_entry_not_claimed_by_revocation_arm() {
        let key = DmInboxDoc::grant_key(&SENDER_OWNER, &[0xDE, 0xAD]);
        let entry = DmInboxEntry {
            sender_owner: SENDER_OWNER,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: Some(vec![0xBA, 0xAD]), // stray/adversarial
            grant_push: Some(vec![0xDE, 0xAD]),
            grant_revoke: None,
            deposited_at: hlc(500),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let _ = ingest_pending(&mut doc, &ctx).await;

        assert!(
            !ctx.calls().contains(&"apply_revocation".to_string()),
            "a {{revocation+grant}} entry must not be claimed by the revocation \
             arm (that arm acks the entry and drops the grant); the \
             grant_push.is_none() guard forbids it"
        );
        // Mutually-exclusive pure-shape guards: the grant arm also declines (it
        // requires revocation_push.is_none()), so the entry falls through to the
        // existing cidnotify-None invite-only catch-all — never the revocation path.
        assert_eq!(ctx.calls(), vec!["apply_invite_only"]);
    }

    /// A failed `apply_grant_push` leaves the entry PENDING (not ingested) for
    /// retry, exactly like the message/invite/revocation arms.
    #[tokio::test]
    async fn ingest_grant_apply_failure_leaves_entry_pending() {
        let key = DmInboxDoc::grant_key(&SENDER_OWNER, &[0x01]);
        let entry = DmInboxEntry {
            sender_owner: SENDER_OWNER,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: Some(vec![0x01]),
            grant_revoke: None,
            deposited_at: hlc(500),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);
        let mut ctx = ProbeCtx::new();
        ctx.grant_fail = true;

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(!changed, "a failed grant apply mutates nothing");
        assert_eq!(ctx.calls(), vec!["apply_grant_push"]);
        assert!(
            !doc.entries[&key].ingested_by.contains(SELF_ID),
            "entry left pending for retry"
        );
    }

    /// ZEB-730: a PURE grant-revoke entry (`grant_revoke: Some`, all other
    /// sub-payloads `None`) is routed to `apply_grant_revoke` and SKIPS the
    /// message/invite/revocation/grant pipelines entirely — no CAS-put, no
    /// verify, no apply_inbox, no emit — then is marked ingested.
    #[tokio::test]
    async fn ingest_routes_grant_revoke_entry_to_apply_grant_revoke() {
        let gr = crate::butler_deposit::encode_grant_revoke([0x77; 32]);
        let key = DmInboxDoc::grant_revoke_key(&SENDER_OWNER, &gr);
        let entry = DmInboxEntry {
            sender_owner: SENDER_OWNER,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: Some(gr),
            deposited_at: hlc(500),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "grant-revoke ingest mutated the doc (ig growth)");
        assert_eq!(ctx.calls(), vec!["apply_grant_revoke"]);
        assert!(
            ctx.applied().is_empty(),
            "no message apply_inbox on a grant-revoke"
        );
        assert!(
            ctx.emitted().is_empty(),
            "no dm-received emit on a grant-revoke"
        );
        assert!(doc.entries[&key].ingested_by.contains(SELF_ID));
    }

    /// ZEB-730 (guard completeness): a MESSAGE entry (cidnotify present) that
    /// ALSO carries a stray `grant_revoke` must NOT be claimed by the grant-revoke
    /// arm — that arm's pure-shape guard requires `cidnotify_packet.is_none()`.
    /// The stray `grant_revoke` is inert: `apply_grant_revoke` is only reachable
    /// from the pure arm, so the entry is processed as a normal message and the
    /// revoke never fires (mirrors how a stray grant_push/revocation_push on a
    /// message is inert).
    #[tokio::test]
    async fn ingest_message_with_stray_grant_revoke_not_routed_to_apply_grant_revoke() {
        let (key, mut entry) = make_entry([1; 16], [2; 32], 500, &[]);
        entry.grant_revoke = Some(crate::butler_deposit::encode_grant_revoke([0x77; 32]));
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let _ = ingest_pending(&mut doc, &ctx).await;

        assert!(
            !ctx.calls().contains(&"apply_grant_revoke".to_string()),
            "a message carrying a stray grant_revoke must NOT route to apply_grant_revoke"
        );
        // Processed as a normal message end-to-end (the stray grant_revoke is inert).
        assert_eq!(
            ctx.calls(),
            vec!["cas_put", "verify", "apply_inbox", "emit"]
        );
    }

    /// ZEB-730: a failed `apply_grant_revoke` leaves the entry PENDING (not
    /// ingested) for retry, exactly like the grant/message/invite/revocation arms.
    #[tokio::test]
    async fn ingest_grant_revoke_apply_failure_leaves_entry_pending() {
        let gr = crate::butler_deposit::encode_grant_revoke([0x77; 32]);
        let key = DmInboxDoc::grant_revoke_key(&SENDER_OWNER, &gr);
        let entry = DmInboxEntry {
            sender_owner: SENDER_OWNER,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: Some(gr),
            deposited_at: hlc(500),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);
        let mut ctx = ProbeCtx::new();
        ctx.grant_revoke_fail = true;

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(!changed, "a failed grant-revoke apply mutates nothing");
        assert_eq!(ctx.calls(), vec!["apply_grant_revoke"]);
        assert!(
            !doc.entries[&key].ingested_by.contains(SELF_ID),
            "entry left pending for retry"
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
        // ZEB-862: count stamp-only persist_now calls. Every sweep in this test
        // mutates (ingests), so it takes the notify_dirty path — persist_now must
        // stay at 0.
        let persisted = Arc::new(AtomicUsize::new(0));
        let persist_now: PersistNowFn = {
            let persisted = Arc::clone(&persisted);
            Arc::new(move || {
                let persisted = Arc::clone(&persisted);
                Box::pin(async move {
                    persisted.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
        };

        let handle = tokio::spawn(run_dm_inbox_ingest_sweeper(
            Arc::clone(&doc),
            Arc::clone(&ctx) as Arc<dyn DmInboxIngestCtx>,
            nudge_rx,
            notify_dirty,
            persist_now,
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

        assert_eq!(
            persisted.load(Ordering::SeqCst),
            0,
            "mutating sweeps notify_dirty; the stamp-only persist_now path is not taken here"
        );
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

        let dir = tempfile::tempdir().unwrap();
        let kt = Arc::new(KeyTree::derive(&[0x44u8; 32]).expect("derive kt"));
        let doc = Arc::new(Mutex::new(DmInboxDoc::default()));
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".to_string(),
        )));
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let merger: Merger<DmInboxDoc> = Arc::new(|local, remote| local.merge_from(remote));
        // The ingestion-nudge channel exactly as start_node wires it (the
        // receiver half would feed `run_dm_inbox_ingest_sweeper`; this test
        // proves the ENGINE wiring, so the rx is simply held).
        let (nudge_tx, _nudge_rx) = mpsc::channel::<()>(1);

        let engine = FleetSyncEngine::<DmInboxDoc>::new(FleetSyncConfig {
            keys: crate::owner_state_crypto::FleetKeySet::new(kt),
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
                first_observed_path: dir.path().join("dm_inbox_first_observed.cbor"),
            }),
            lookup_key_tag: b"dm-inbox-v1",
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            publish_seen: true,
            on_applied: Some(ingest_nudge_on_applied(nudge_tx)),
            sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        //     has ingested → removed on the FIRST sweep. Coverage is
        //     unaffected by ZEB-851's local-clock TTL change.
        let (key_covered, entry_covered) =
            make_entry([1; 16], [1; 32], now_ms - 1_000, &[SELF_ID, SIBLING_ID]);
        // (b) backdated `deposited_at` (ms=500, ancient) but
        //     coverage-incomplete (ig carries SELF only). Under local-clock
        //     TTL this entry is first OBSERVED on this very sweep, so it
        //     must NOT pre-expire here — that's the ZEB-851 regression this
        //     test now pins alongside coverage.
        let (key_ttl, entry_ttl) = make_entry([2; 16], [2; 32], 500, &[SELF_ID]);

        let mut doc = DmInboxDoc::default();
        doc.entries
            .insert(key_covered.clone(), entry_covered.clone());
        doc.entries.insert(key_ttl.clone(), entry_ttl);

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "GC removals are doc mutations");
        assert!(
            !doc.entries.contains_key(&key_covered),
            "ig ⊇ enrolled set → coverage GC"
        );
        assert!(
            doc.entries.contains_key(&key_ttl),
            "a backdated deposited_at must not pre-expire a freshly-observed entry"
        );
        assert!(
            ctx.calls().is_empty(),
            "all entries already carried self in ig — GC must not re-ingest"
        );

        // (c) fresh + coverage-incomplete, inserted only NOW — first
        //     observed on the boundary sweep below, so it stays retained
        //     through both subsequent sweeps.
        let (key_fresh, entry_fresh) = make_entry([3; 16], [3; 32], now_ms - 1_000, &[SELF_ID]);
        doc.entries.insert(key_fresh.clone(), entry_fresh);

        // (d) TTL boundary pin: a sweep exactly at
        //     first_observed(key_ttl) + INBOX_TTL_MS is NOT yet expired
        //     (strict `<`) → retained.
        let ctx_boundary = ProbeCtx {
            now_ms: now_ms + INBOX_TTL_MS,
            ..ProbeCtx::new()
        };
        let changed = ingest_pending(&mut doc, &ctx_boundary).await;
        assert!(!changed, "boundary sweep must not mutate anything");
        assert!(
            doc.entries.contains_key(&key_ttl),
            "TTL is strict `<` — boundary retained"
        );
        assert!(
            doc.entries.contains_key(&key_fresh),
            "fresh + uncovered → retained"
        );

        // One ms past the boundary: key_ttl (first-observed at `now_ms`)
        // expires from local receipt; key_fresh (first-observed one sweep
        // later, on ctx_boundary) does not.
        let ctx_expired = ProbeCtx {
            now_ms: now_ms + INBOX_TTL_MS + 1,
            ..ProbeCtx::new()
        };
        let changed = ingest_pending(&mut doc, &ctx_expired).await;
        assert!(changed, "TTL GC removal is a doc mutation");
        assert!(
            !doc.entries.contains_key(&key_ttl),
            "expired from local receipt → GC"
        );
        assert!(
            doc.entries.contains_key(&key_fresh),
            "fresh + uncovered → still retained"
        );

        // Empty-enrolled guard: ig ⊇ ∅ is vacuously true, so an empty
        // provider snapshot must NOT wipe the inbox (TTL still applies; the
        // entry is first-observed on this very sweep, so it isn't expired
        // either).
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
        // (self is already in the resurrected ig): coverage GC is
        // deterministic on (ig, now) regardless of local-clock TTL state,
        // so every replica converges on removal and once all have GC'd no
        // copy remains to resurrect.
        let applied_before = ctx_expired.applied().len();
        let changed = ingest_pending(&mut doc, &ctx_expired).await;
        assert!(changed);
        assert!(
            !doc.entries.contains_key(&key_covered),
            "re-GC after resurrection-by-merge converges"
        );
        assert_eq!(
            ctx_expired.applied().len(),
            applied_before,
            "no duplicate ingestion/emit for a resurrected entry"
        );
    }

    #[tokio::test]
    async fn ingest_gc_uses_local_receipt_not_backdated_deposited_at() {
        let now: u64 = INBOX_TTL_MS + 1_000_000;
        // Backdated deposited_at (deposited_ms = 1); ig carries SELF only, so
        // coverage is INCOMPLETE (ProbeCtx::new enrolls self + sibling) → the
        // only removal path is the TTL GC, and the ingest loop skips it (already
        // self-ingested — same posture as the retained entries in
        // gc_removes_when_ig_covers_enrolled_set_or_ttl).
        let (key, entry) = make_entry([7; 16], [7; 32], 1, &[SELF_ID]);
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);

        // First sweep observes the entry locally at `now`.
        let ctx = ProbeCtx {
            now_ms: now,
            ..ProbeCtx::new()
        };
        let _ = ingest_pending(&mut doc, &ctx).await;
        assert!(
            doc.entries.contains_key(&key),
            "a backdated deposited_at must not pre-expire a freshly-observed inbox entry"
        );

        // A later sweep past first_observed + INBOX_TTL_MS removes it normally.
        let later = ProbeCtx {
            now_ms: now + INBOX_TTL_MS + 1,
            ..ProbeCtx::new()
        };
        let _ = ingest_pending(&mut doc, &later).await;
        assert!(
            !doc.entries.contains_key(&key),
            "expired from local receipt"
        );
    }

    // ── ZEB-691: recipient inbox-sweeper revocation arm ──────────────────────

    /// ZEB-691: a self-contained master-signed revocation scenario (mirrors
    /// `dm_outbox::tests::sample_revocation_case`): a master, a device it
    /// enrolled, and a master-signed `RevocationCert` for that device. Returns
    /// `(owner, revocation, enrollment, revoked_ed25519)`.
    fn sample_revocation() -> (
        OwnerAddr,
        harmony_owner::certs::RevocationCert,
        harmony_owner::certs::EnrollmentCert,
        [u8; 32],
    ) {
        use ed25519_dalek::SigningKey;
        use harmony_owner::certs::{EnrollmentCert, RevocationCert, RevocationReason};
        use harmony_owner::pubkey_bundle::PubKeyBundle;

        let master_sk = SigningKey::from_bytes(&[0x11; 32]);
        let master_bundle = PubKeyBundle::classical_only(master_sk.verifying_key().to_bytes());
        let owner = OwnerAddr(master_bundle.identity_hash());

        let device_sk = SigningKey::from_bytes(&[0x22; 32]);
        let device_bundle = PubKeyBundle::classical_only(device_sk.verifying_key().to_bytes());
        let device_id = device_bundle.identity_hash();
        let revoked_ed = device_bundle.classical.ed25519_verify;

        let now = 1_700_000_000u64;
        let enrollment = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            device_id,
            device_bundle,
            now,
            None,
        )
        .expect("enrollment sign");
        let revocation = RevocationCert::sign_master(
            &master_sk,
            master_bundle,
            device_id,
            now,
            RevocationReason::Compromised,
        )
        .expect("revocation sign");
        (owner, revocation, enrollment, revoked_ed)
    }

    /// Build a `ProdDmInboxIngestCtx` over stub handles with a counting
    /// `notify_owner_state_dirty`, also returning the `RecordingSink` handle
    /// so callers can assert emitted frames. Returns
    /// `(ctx, crdt_state, dirty_counter, revoked, sink_handle)`.
    fn prod_ctx_with_dirty_and_sink() -> (
        ProdDmInboxIngestCtx,
        Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
        Arc<AtomicUsize>,
        crate::revoked_device_projection::RevokedDeviceProjection,
        Arc<crate::node_event_sink::RecordingSink>,
    ) {
        let crdt_state = Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            Arc::new(Arc::clone(&sink_handle));
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let dirty = Arc::new(AtomicUsize::new(0));
        let notify: Arc<dyn Fn() + Send + Sync> = {
            let dirty = Arc::clone(&dirty);
            Arc::new(move || {
                dirty.fetch_add(1, Ordering::SeqCst);
            })
        };
        let ctx = ProdDmInboxIngestCtx {
            device_id: SELF_ID.to_string(),
            self_owner: OwnerAddr([0x01; 16]),
            crdt_state: Arc::clone(&crdt_state),
            content_store,
            sink,
            pending_dm_invites: None,
            enrolled: BTreeSet::new(),
            revoked: revoked.clone(),
            notify_owner_state_dirty: Some(notify),
            device_x25519_priv: crate::dm_signing::ed25519_priv_to_x25519(
                &ed25519_dalek::SigningKey::from_bytes(&TEST_DEVICE_ED25519_SEED),
            ),
            owner_keytree: Arc::new(test_owner_keytree()),
        };
        (ctx, crdt_state, dirty, revoked, sink_handle)
    }

    /// Build a `ProdDmInboxIngestCtx` over stub handles with a counting
    /// `notify_owner_state_dirty`. Returns `(ctx, crdt_state, dirty_counter)`.
    fn prod_ctx_with_dirty() -> (
        ProdDmInboxIngestCtx,
        Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
        Arc<AtomicUsize>,
        crate::revoked_device_projection::RevokedDeviceProjection,
    ) {
        let (ctx, crdt_state, dirty, revoked, _sink) = prod_ctx_with_dirty_and_sink();
        (ctx, crdt_state, dirty, revoked)
    }

    /// Build a PURE revocation deposit entry (no cidnotify, no invite) carrying a
    /// real signed `RevocationPush` frame, keyed on `sender_owner`.
    fn revocation_entry(
        sender_owner: [u8; 16],
        revocation: harmony_owner::certs::RevocationCert,
        enrollment: harmony_owner::certs::EnrollmentCert,
    ) -> DmInboxEntry {
        let packet = crate::dm_envelope::build_revocation_push_packet(revocation, enrollment);
        let bytes = crate::dm_envelope::encode_packet(&packet).expect("encode revocation push");
        DmInboxEntry {
            sender_owner,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: Some(bytes),
            grant_push: None,
            grant_revoke: None,
            deposited_at: hlc(500),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        }
    }

    /// ZEB-674 (C4) sweeper integration: a grant-only entry carrying a REAL
    /// `grant_push` (sealed to this ctx's device key) is swept end-to-end through
    /// the PRODUCTION `apply_grant_push` — it lands on `received_file_grants`,
    /// fires `notify_owner_state_dirty` exactly once, and the stored DEK is
    /// openable BOTH via `open_received_file` (the grantee read path) AND
    /// directly via `open_dek_at_rest` with a freshly-derived KeyTree of the same
    /// material (a DIFFERENT device with the same shared KeyTree — device-
    /// agnostic, mirroring `file_deks`). The granter recorded is the entry's
    /// butler-verified `sender_owner`.
    #[tokio::test]
    async fn sweep_ingests_real_grant_push_via_prod_ctx_device_agnostic() {
        use crate::file_sharing::{
            open_dek_at_rest, open_received_file, seal_grant_for_devices, FileGrantInner,
        };
        let (ctx, crdt_state, dirty, _revoked) = prod_ctx_with_dirty();

        // A real sealed grant, targeted at the ctx device's X25519 pubkey.
        let dek_bytes = [0x5Au8; 32];
        let cid_bytes = [0xC1u8; 32];
        let inner = FileGrantInner {
            cid: cid_bytes,
            file_name: "shared.md".into(),
            file_size: 42,
            mime: "text/markdown".into(),
            dek: dek_bytes,
        };
        let sealed = seal_grant_for_devices(&inner, &[test_device_x25519_pub()]).expect("seal");
        let list: Vec<serde_bytes::ByteBuf> =
            sealed.into_iter().map(serde_bytes::ByteBuf::from).collect();
        let mut grant_push = Vec::new();
        ciborium::into_writer(&list, &mut grant_push).expect("encode grant_push");

        let granter = OwnerAddr([0xB0; 16]);
        let key = DmInboxDoc::grant_key(&granter.0, &grant_push);
        // Deposit "now" (the Prod ctx's `now_ms` is the real wall clock, and an
        // empty enrolled set disables coverage-GC) so the entry survives the
        // sweep's TTL check and we can assert it was marked ingested.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let entry = DmInboxEntry {
            sender_owner: granter.0,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: Some(grant_push),
            grant_revoke: None,
            deposited_at: hlc(now),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "grant sweep mutated the doc (ig growth)");
        assert!(
            doc.entries[&key].ingested_by.contains(SELF_ID),
            "entry marked ingested"
        );
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            1,
            "a recorded grant fires notify_owner_state_dirty exactly once"
        );

        let cid = ContentId::from_bytes(cid_bytes);
        let state = crdt_state.lock().await;
        let rec = state
            .received_file_grants
            .get(&cid_bytes)
            .expect("received_file_grants populated");
        assert_eq!(
            rec.granter_owner, granter,
            "granter is the butler-verified deposit sender"
        );
        assert_ne!(
            rec.sealed_dek.as_slice(),
            dek_bytes.as_slice(),
            "stored blob is the KeyTree-sealed envelope, never the raw DEK"
        );

        // (a) grantee read path recovers the DEK.
        let recovered =
            open_received_file(&state, &test_owner_keytree(), cid).expect("open received file");
        assert_eq!(recovered.as_bytes(), &dek_bytes, "recovered DEK matches");

        // (b) device-agnostic: a FRESH KeyTree of the same shared material (a
        // different device of the same owner) opens the stored blob directly.
        let other_device_tree = test_owner_keytree();
        let via_tree =
            open_dek_at_rest(&other_device_tree, &rec.sealed_dek).expect("open via shared KeyTree");
        assert_eq!(
            via_tree.as_bytes(),
            &dek_bytes,
            "any device with the shared KeyTree opens the re-sealed grant"
        );
    }

    /// ZEB-723: a genuinely-recorded grant (the `Some(cid)` branch of
    /// `apply_grant_push`, same gate as `notify_owner_state_dirty`) must also
    /// emit `shared-with-me-updated` so the grantee's "Shared with me" UI can
    /// refresh and bump its unread badge. Drives a REAL per-device-sealed
    /// `grant_push` through the production sweeper, exactly like
    /// `sweep_ingests_real_grant_push_via_prod_ctx_device_agnostic`, and
    /// asserts the emitted frame via the `RecordingSink` handle.
    #[tokio::test]
    async fn sweep_ingested_grant_emits_shared_with_me_updated() {
        use crate::file_sharing::{seal_grant_for_devices, FileGrantInner};
        let (ctx, _crdt_state, _dirty, _revoked, sink_handle) = prod_ctx_with_dirty_and_sink();

        let cid_bytes = [0xC1u8; 32];
        let inner = FileGrantInner {
            cid: cid_bytes,
            file_name: "shared.md".into(),
            file_size: 42,
            mime: "text/markdown".into(),
            dek: [0x5Au8; 32],
        };
        let sealed = seal_grant_for_devices(&inner, &[test_device_x25519_pub()]).expect("seal");
        let list: Vec<serde_bytes::ByteBuf> =
            sealed.into_iter().map(serde_bytes::ByteBuf::from).collect();
        let mut grant_push = Vec::new();
        ciborium::into_writer(&list, &mut grant_push).expect("encode grant_push");

        let granter = OwnerAddr([0xB0; 16]);
        let key = DmInboxDoc::grant_key(&granter.0, &grant_push);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let entry = DmInboxEntry {
            sender_owner: granter.0,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: Some(grant_push),
            grant_revoke: None,
            deposited_at: hlc(now),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key, entry);

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "grant sweep mutated the doc");

        let frames = sink_handle.frames();
        let matching = frames
            .iter()
            .filter(|(name, payload)| {
                name == "shared-with-me-updated"
                    && payload["cid"] == serde_json::json!(hex::encode(cid_bytes))
            })
            .count();
        assert_eq!(
            matching, 1,
            "exactly one shared-with-me-updated frame for the recorded grant's cid \
             (cardinality + idempotency — a single record must not double-emit); got {frames:?}"
        );
    }

    /// ZEB-730 seed helper: install a `ReceivedFileGrant` on a seeded owner-state
    /// with `granter_owner == granter` so `apply_grant_revoke`'s granter-of-record
    /// authorization can be exercised without a full grant_push ingest.
    async fn seed_received_grant(
        crdt_state: &Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
        cid: [u8; 32],
        granter: OwnerAddr,
    ) {
        let mut state = crdt_state.lock().await;
        state.received_file_grants.insert(
            cid,
            crate::owner_state_types::ReceivedFileGrant {
                granter_owner: granter,
                cid,
                file_name: "doc.pdf".into(),
                file_size: 10,
                mime: "application/pdf".into(),
                sealed_dek: vec![1, 2, 3],
                received_at: 100,
            },
        );
    }

    /// ZEB-730 (prod path): an AUTHORIZED grant-revoke (deposit sender ==
    /// granter-of-record) applied through the PRODUCTION `apply_grant_revoke`
    /// GCs the received-grant entry, stamps ZEB-727's tombstone, fires
    /// `notify_owner_state_dirty` exactly once, and emits exactly one
    /// `shared-with-me-updated` frame carrying the canonical lowercase-hex cid.
    #[tokio::test]
    async fn apply_grant_revoke_authorized_gcs_notifies_and_emits() {
        let (ctx, crdt_state, dirty, _revoked, sink_handle) = prod_ctx_with_dirty_and_sink();

        let granter = OwnerAddr([0xB0; 16]);
        let cid = [0xC1u8; 32];
        seed_received_grant(&crdt_state, cid, granter).await;

        let entry = DmInboxEntry {
            sender_owner: granter.0, // butler-verified sender == granter-of-record
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: Some(crate::butler_deposit::encode_grant_revoke(cid)),
            deposited_at: hlc(500),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };

        ctx.apply_grant_revoke(&entry)
            .await
            .expect("an authorized grant-revoke applies");

        {
            let state = crdt_state.lock().await;
            assert!(
                !state.received_file_grants.contains_key(&cid),
                "authorized revoke GCs the received-grant entry"
            );
            assert!(
                state.dismissed_received_grants.contains_key(&cid),
                "authorized revoke stamps the ZEB-727 dismiss tombstone"
            );
        }
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            1,
            "an authorized revoke fires notify_owner_state_dirty exactly once"
        );
        let frames = sink_handle.frames();
        let matching = frames
            .iter()
            .filter(|(name, payload)| {
                name == "shared-with-me-updated"
                    && payload["cid"] == serde_json::json!(hex::encode(cid))
            })
            .count();
        assert_eq!(
            matching, 1,
            "exactly one shared-with-me-updated frame carrying the canonical \
             lowercase-hex cid; got {frames:?}"
        );
    }

    /// ZEB-730 SECURITY (prod path, griefing guard): a grant-revoke whose
    /// butler-verified deposit sender is NOT the granter-of-record is a silent
    /// no-op — entry intact, no tombstone, no notify, no emit — so no Active
    /// friend can grief a grantee into losing a file they did not share. Returns
    /// `Ok(())` (a dropped revoke is not an error).
    #[tokio::test]
    async fn apply_grant_revoke_unauthorized_is_noop() {
        let (ctx, crdt_state, dirty, _revoked, sink_handle) = prod_ctx_with_dirty_and_sink();

        let granter = OwnerAddr([0xB0; 16]);
        let attacker = OwnerAddr([0x1A; 16]);
        let cid = [0xC1u8; 32];
        seed_received_grant(&crdt_state, cid, granter).await;

        // Deposit sender is the attacker, NOT the granter-of-record.
        let entry = DmInboxEntry {
            sender_owner: attacker.0,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: Some(crate::butler_deposit::encode_grant_revoke(cid)),
            deposited_at: hlc(500),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };

        ctx.apply_grant_revoke(&entry)
            .await
            .expect("a dropped (unauthorized) revoke is not an error");

        {
            let state = crdt_state.lock().await;
            assert!(
                state.received_file_grants.contains_key(&cid),
                "the received grant is intact (griefing guard)"
            );
            assert!(
                state.dismissed_received_grants.is_empty(),
                "no tombstone minted from an unauthorized revoke"
            );
        }
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            0,
            "no notify on an unauthorized revoke"
        );
        assert!(
            sink_handle.frames().is_empty(),
            "no shared-with-me-updated emit on an unauthorized revoke"
        );
    }

    #[tokio::test]
    async fn apply_revocation_applies_and_marks_dirty_once() {
        // ZEB-691 SECURITY: the recipient re-verifies the deposited revocation
        // (never trust the butler) via the FULL `handle_revocation_push`, stores
        // it in the owner-state CRDT, and marks owner-state dirty EXACTLY once —
        // on the genuine insert, NOT the idempotent re-apply. A deposited
        // revocation is eventually GC'd, so persistence has no re-delivery
        // backstop: it MUST come from notify_owner_state_dirty.
        let (owner, revocation, enrollment, revoked_ed) = sample_revocation();
        let (ctx, crdt_state, dirty, revoked) = prod_ctx_with_dirty();
        let entry = revocation_entry(owner.0, revocation, enrollment);

        // First apply: Ok(true) — CRDT gains the revoked key, projection fed,
        // dirty fires once.
        let inserted = ctx
            .apply_revocation(&entry)
            .await
            .expect("a valid master-signed revocation applies");
        assert!(inserted, "a fresh revocation is a genuine insert");
        assert!(
            revoked.is_revoked(&owner, &revoked_ed),
            "the live projection is fed"
        );
        {
            let state = crdt_state.lock().await;
            assert!(
                state
                    .revoked_dm_devices
                    .get(&owner)
                    .expect("owner has a revoked set")
                    .contains(&revoked_ed),
                "owner-state CRDT stored the revoked device key"
            );
        }
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            1,
            "dirty fires exactly once on the genuine insert"
        );

        // Second apply: idempotent Ok(false) — dirty must NOT re-fire.
        let again = ctx
            .apply_revocation(&entry)
            .await
            .expect("idempotent re-apply is not an error");
        assert!(
            !again,
            "re-applying the same revocation is not a new insert"
        );
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            1,
            "no spurious dirty on the idempotent re-apply"
        );
    }

    #[tokio::test]
    async fn apply_revocation_rejects_forged_entry_and_stores_nothing() {
        // ZEB-691 trust boundary: apply_revocation re-runs the FULL verification
        // — it never trusts the carrier. A revocation whose deposit `sender_owner`
        // does not match the cert owner (a relayed third-party revocation the
        // butler failed to catch) is rejected, NOTHING is stored, and dirty never
        // fires.
        let (owner, revocation, enrollment, revoked_ed) = sample_revocation();
        let (ctx, crdt_state, dirty, revoked) = prod_ctx_with_dirty();
        // Claim a DIFFERENT sender than the cert's owner → OwnerFieldMismatch.
        let entry = revocation_entry([0xEE; 16], revocation, enrollment);

        let err = ctx.apply_revocation(&entry).await;
        assert!(
            err.is_err(),
            "a forged/mismatched revocation must be rejected, got {err:?}"
        );
        assert!(
            crdt_state.lock().await.revoked_dm_devices.is_empty(),
            "nothing stored on reject"
        );
        assert!(
            !revoked.is_revoked(&owner, &revoked_ed),
            "projection not fed on reject"
        );
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            0,
            "no dirty notification on reject"
        );
    }

    #[tokio::test]
    async fn ingest_pending_routes_pure_revocation_entry_and_marks_ingested() {
        // ZEB-691: a PURE revocation deposit (no cidnotify, no invite) is applied
        // via `apply_revocation`, marked ingested, and reported as a doc change —
        // never routed through the message or invite-only paths.
        let key = DmInboxDoc::revoke_key(&SENDER_OWNER, &[0x11; 16]);
        let entry = DmInboxEntry {
            sender_owner: SENDER_OWNER,
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: Some(vec![0x05, 0xDE, 0xAD]),
            grant_push: None,
            grant_revoke: None,
            deposited_at: hlc(500),
            deposited_by: "butler-device".into(),
            ingested_by: Default::default(),
        };
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "revocation ingest mutated the doc (ig growth)");
        assert_eq!(
            ctx.calls(),
            vec!["apply_revocation"],
            "a pure revocation routes ONLY to apply_revocation"
        );
        assert!(
            ctx.applied().is_empty(),
            "no message apply_inbox on a revocation"
        );
        assert!(
            ctx.emitted().is_empty(),
            "no dm-received emit on a revocation"
        );
        assert!(
            doc.entries[&key].ingested_by.contains(SELF_ID),
            "self added to the grow-only ig set"
        );
    }

    #[tokio::test]
    async fn apply_inbox_marks_owner_state_dirty_once_zeb709() {
        // ZEB-709: the DM-message insert into owner-state MUST mark the
        // OWNER-STATE engine dirty. Without it, the `ingested_by` ack (a
        // dm-inbox DATASET write, notified by the sweep) persists and
        // replicates within its debounce while the payload sits un-notified
        // in memory — a crash in that window permanently loses the DM: the
        // restarted sweeper skips the entry via `ingested_by`, coverage-GC
        // destroys it, and the deposit clears. The notify does not make the
        // two engines atomic; it flips the failure direction to safe (a
        // persisted payload with a lost ack just re-ingests idempotently).
        // Dirty-once discipline mirrors the revocation arm: fire on
        // Inserted, never on the Merged re-apply.
        let (ctx, crdt_state, dirty, _revoked) = prod_ctx_with_dirty();
        let entry = crate::owner_state_types::InboxEntry {
            space_id: crate::owner_state_types::SpaceId([0x07; 16]),
            message_cid: ContentId::from_bytes([0x11; 32]),
            from: OwnerAddr([0x02; 16]),
            received_at: hlc(500),
        };

        let inserted = ctx
            .apply_inbox(entry.clone())
            .await
            .expect("a fresh inbox entry applies");
        assert!(inserted, "first apply is a genuine insert");
        {
            let state = crdt_state.lock().await;
            assert!(
                state.inbox.contains_key(&entry.key()),
                "owner-state CRDT stored the inbox entry"
            );
        }
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            1,
            "dirty fires exactly once on the genuine insert"
        );

        let again = ctx
            .apply_inbox(entry)
            .await
            .expect("idempotent re-apply is not an error");
        assert!(!again, "re-applying the same entry is not a new insert");
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            1,
            "no spurious dirty on the idempotent re-apply"
        );
    }

    #[tokio::test]
    async fn ingest_pending_revocation_deposit_arms_recipient_cutoff() {
        // ZEB-691 (B7 e2e): a butler-deposited revocation — the EXACT
        // `DmInboxEntry` shape `iroh_butler_acceptor::handle_deposit_core`
        // persists under `revoke_key` (its B4 acceptor test pins
        // `entry.revocation_push == Some(wire)` + a `REVOCATION_DEPOSIT_MARKER`
        // ack) — is recovered by the REAL recipient sweeper. `ingest_pending`
        // re-verifies through `handle_revocation_push` (never trusting the
        // carrier), and arms BOTH cutoff surfaces: the live
        // `RevokedDeviceProjection` and the owner-state `revoked_dm_devices`
        // CRDT, marking owner-state dirty so the recovered revocation persists.
        // This closes the B2-review gap: no prior test drove the SWEEPER (vs.
        // `apply_revocation` directly) over a deposited revocation and proved
        // the cutoff ends up armed.
        let (owner, revocation, enrollment, revoked_ed) = sample_revocation();
        let target = revocation.target;
        let (ctx, crdt_state, dirty, revoked) = prod_ctx_with_dirty();

        // Persist EXACTLY as the acceptor does: under revoke:{sender}:{target},
        // carrying the signed RevocationPush and no message/invite half.
        let key = DmInboxDoc::revoke_key(&owner.0, &target);
        let entry = revocation_entry(owner.0, revocation, enrollment);
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);

        assert!(
            !revoked.is_revoked(&owner, &revoked_ed),
            "projection empty before the sweep (baseline)"
        );

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "the sweep applied the revocation (doc ig grew)");

        // The recipient §5.2 cutoff is now armed on BOTH surfaces.
        assert!(
            revoked.is_revoked(&owner, &revoked_ed),
            "the live RevokedDeviceProjection now rejects the revoked device"
        );
        {
            let state = crdt_state.lock().await;
            assert!(
                state
                    .revoked_dm_devices
                    .get(&owner)
                    .expect("owner has a revoked set")
                    .contains(&revoked_ed),
                "owner-state CRDT stored the revoked device key (cutoff armed)"
            );
        }
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            1,
            "the recovered revocation marks owner-state dirty exactly once"
        );
        // The entry is applied then coverage-GC'd by the prod sweep (a fully
        // ingested revocation has no re-delivery backstop — persistence rode the
        // notify_dirty above); `changed` already confirmed the sweep mutated the
        // doc, so we assert on the durable cutoff state, not the transient entry.
    }

    #[tokio::test]
    async fn stray_revocation_on_message_entry_is_not_hijacked() {
        // ZEB-691 (B4 review): `revocation_push` rides UNCONDITIONALLY on the
        // persisted entry, so a MESSAGE entry (cidnotify Some) carrying a
        // stray/malicious `revocation_push` must STILL take the message path —
        // never the revocation arm (which `continue`s and would DROP the real
        // message). The dispatch guard is a PURE-revocation check, not a bare
        // `revocation_push.is_some()`.
        let (key, mut entry) = make_entry([1; 16], [2; 32], 500, &[]);
        entry.revocation_push = Some(vec![0x05, 0xBA, 0xAD]); // stray garbage
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed);
        // The message path ran end-to-end; the revocation arm was NOT taken.
        assert_eq!(
            ctx.calls(),
            vec!["cas_put", "verify", "apply_inbox", "emit"],
            "a stray revocation_push must not divert a message entry"
        );
        assert!(
            !ctx.calls().contains(&"apply_revocation".to_string()),
            "the revocation arm must never fire for a message entry"
        );
        assert_eq!(ctx.applied().len(), 1, "the real message was applied");
        assert_eq!(ctx.emitted().len(), 1, "the real message was emitted");
        assert!(doc.entries[&key].ingested_by.contains(SELF_ID));
    }

    // ── ZEB-473 Task 9: inbound tunnel DM ingest (`ingest_dm_packet`) ─────────

    use super::test_fixture::build_dm_ingest_fixture;

    /// `ingest_dm_packet` over a known-good real packet runs the full
    /// verify→decrypt→apply_inbox→emit pipeline: the inbox gets the entry and
    /// the SAME `dm-received` event the deposit/direct paths emit fires once.
    #[tokio::test]
    async fn ingest_dm_packet_applies_inbox_and_emits_dm_received() {
        let fx = build_dm_ingest_fixture(b"hello over the tunnel").await;

        let emitted = ingest_dm_packet(
            &fx.crdt_state,
            &fx.content_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            // CidNotify path ignores peer_node_id (only the Invite arm binds).
            [0u8; 32],
            &fx.packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("known-good packet must ingest");
        assert!(emitted, "a newly-applied DM emits dm-received");

        // Inbox CRDT carries the entry, keyed by InboxKey(space_id,
        // message_cid), from Alice.
        let key = crate::owner_state_types::InboxKey {
            space_id: fx.space_id,
            message_cid: fx.message_cid,
        };
        let state = fx.crdt_state.lock().await;
        let entry = state
            .inbox
            .get(&key)
            .expect("inbox must contain the ingested DM");
        assert_eq!(entry.from, fx.alice);
        drop(state);

        // The shared dm-received event fired exactly once with the decrypted body.
        let frames = fx.sink_handle.frames();
        let dm_frames: Vec<_> = frames
            .iter()
            .filter(|(name, _)| name == crate::dm_outbox::DM_RECEIVED_EVENT)
            .collect();
        assert_eq!(dm_frames.len(), 1, "exactly one dm-received");
        assert_eq!(
            dm_frames[0].1["body"],
            serde_json::Value::String(hex::encode(b"hello over the tunnel")),
            "payload carries the decrypted body (hex)"
        );

        // Idempotency: a duplicate tunnel frame (same CID) dedups — Ok(false),
        // no second emit.
        let again = ingest_dm_packet(
            &fx.crdt_state,
            &fx.content_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &fx.packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("duplicate packet is not an error");
        assert!(!again, "a duplicate DM dedups (no re-emit)");
        assert_eq!(
            fx.sink_handle
                .frames()
                .iter()
                .filter(|(n, _)| n == crate::dm_outbox::DM_RECEIVED_EVENT)
                .count(),
            1,
            "duplicate must not re-emit dm-received"
        );
    }

    /// ZEB-482: a `DmPacket::Invite` delivered over the tunnel is auto-accepted
    /// — the DM Space lands in `spaces`, the inviter's devices/identity-pub are
    /// cached, ingest returns `Ok(false)` (no message), and NO `dm-received`
    /// event fires (invites carry no body). This is the receive half of the
    /// Move 1b carrier: the Space bootstraps from the invite so the subsequent
    /// CidNotify admits instead of rejecting `SpaceNotFound`.
    #[tokio::test]
    async fn ingest_dm_packet_applies_a_tunnel_delivered_invite() {
        // Fresh receiver (Bob) state with NO pre-existing DM Space — the invite
        // must bootstrap it.
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x77; 16]);
        let state = std::sync::Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            std::sync::Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(std::sync::Arc::clone(&sink_handle));

        // Alice (the inviter) signs a real DmInvite over the tunnel wire.
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub = private_alice.public_identity();
        let alice_identity_pub = alice_pub.to_public_bytes();
        let alice = OwnerAddr([0xA1; 16]);
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub.address_hash);

        let mut members = vec![alice, bob];
        members.sort();
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id,
            kind: crate::owner_state_types::SpaceKind::Dm,
            members,
            inviter: alice,
            content_key: crate::owner_state_types::DmContentKey::new([0x42; 32]),
            sender_devices: vec![alice_device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            signing_device_hash: alice_device_hash,
            inviter_identity_pub: alice_identity_pub,
            inviter_enrollment: None,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);
        let packet = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        })
        .unwrap();

        // CodeRabbit F1: the tunnel ingest path binds `signed.inviter` to the
        // authenticated peer by reverse-resolving the OwnerDeviceCache. The
        // friend handshake (ZEB-473) populated Alice's owner → device →
        // DeviceTunnelContact, so seed that contact (strictly-OLDER `learned_at`
        // than the invite's wall clock so the invite's cache write preserves it)
        // and dial in with the peer NodeId derived from its PQ DSA pubkey.
        let alice_dsa_pubkey = vec![0x07u8; 1952];
        let peer_node_id = crate::tunnel_manager::node_id_from_dsa_pubkey(&alice_dsa_pubkey);
        {
            let mut st = state.lock().await;
            st.apply_owner_device_update(
                alice,
                vec![alice_device_hash],
                vec![None],
                vec![Some(crate::owner_state_types::DeviceTunnelContact {
                    iroh_node_id: [0x09; 32],
                    home_relay_url: None,
                    pq_dsa_pubkey: alice_dsa_pubkey.clone(),
                    pq_kem_pubkey: vec![0x08u8; 1184],
                })],
                Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "handshake".into(),
                },
            );
            // ZEB-236: the tunnel invite arrives from an ACTIVE friend, so it
            // AUTO-ACCEPTS (bootstraps the Space). A non-friend would be STAGED
            // instead — see `ingest_dm_packet_stages_non_friend_tunnel_invite`.
            st.friend_graph
                .friends
                .insert(alice, crate::friend_graph::active_friend_entry_for_test(1));
        }

        let applied = ingest_dm_packet(
            &state,
            &content_store,
            &sink,
            None,
            bob,
            "bob-device-64hex",
            peer_node_id,
            &packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("a known-good invite from a bound peer must apply");
        assert!(!applied, "an invite never emits dm-received (Ok(false))");

        // The DM Space bootstrapped from the invite.
        let st = state.lock().await;
        let space = st
            .spaces
            .get(&space_id)
            .expect("the invite must write the DM Space");
        assert_eq!(space.kind, crate::owner_state_types::SpaceKind::Dm);
        assert!(space.content_key.is_some(), "Space carries the content_key");
        // The inviter's device + identity-pub are cached.
        let cache = st
            .owner_device_cache
            .devices
            .get(&alice)
            .expect("inviter's devices cached");
        assert_eq!(cache.devices, vec![alice_device_hash]);
        assert_eq!(cache.device_identity_pubs[0], Some(alice_identity_pub));
        drop(st);

        // No dm-received emit for a bare invite.
        assert!(
            sink_handle
                .frames()
                .iter()
                .all(|(n, _)| n != crate::dm_outbox::DM_RECEIVED_EVENT),
            "an invite must not emit dm-received"
        );
    }

    /// ZEB-236 (T3): a tunnel-delivered invite from a NON-friend must NOT
    /// auto-accept — the tier fork STAGES it. The Space is not written; the
    /// invite lands in the process-local `PendingDmInvites` store, and the UI is
    /// prompted via `dm-invite-received` (newly staged) + `dm-invite-list-changed`.
    /// The invite still carries no message, so `dm-received` never fires and
    /// ingest returns `Ok(false)`. (Companion to
    /// `ingest_dm_packet_applies_a_tunnel_delivered_invite`, which seeds an active
    /// friendship and asserts the auto-accept branch instead.)
    #[tokio::test]
    async fn ingest_dm_packet_stages_non_friend_tunnel_invite() {
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x77; 16]);
        let state = std::sync::Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            std::sync::Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(std::sync::Arc::clone(&sink_handle));
        let pending = std::sync::Arc::new(crate::pending_dm_invites::PendingDmInvites::new());

        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub = private_alice.public_identity();
        let alice_identity_pub = alice_pub.to_public_bytes();
        let alice = OwnerAddr([0xA1; 16]);
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub.address_hash);

        let mut members = vec![alice, bob];
        members.sort();
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id,
            kind: crate::owner_state_types::SpaceKind::Dm,
            members,
            inviter: alice,
            content_key: crate::owner_state_types::DmContentKey::new([0x42; 32]),
            sender_devices: vec![alice_device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            signing_device_hash: alice_device_hash,
            inviter_identity_pub: alice_identity_pub,
            inviter_enrollment: None,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);
        let packet = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        })
        .unwrap();

        // Alice's device is cached (so `resolve_owner_for_peer` binds the peer),
        // but she is NOT an active friend — the staging branch.
        let alice_dsa_pubkey = vec![0x07u8; 1952];
        let peer_node_id = crate::tunnel_manager::node_id_from_dsa_pubkey(&alice_dsa_pubkey);
        {
            let mut st = state.lock().await;
            st.apply_owner_device_update(
                alice,
                vec![alice_device_hash],
                vec![None],
                vec![Some(crate::owner_state_types::DeviceTunnelContact {
                    iroh_node_id: [0x09; 32],
                    home_relay_url: None,
                    pq_dsa_pubkey: alice_dsa_pubkey.clone(),
                    pq_kem_pubkey: vec![0x08u8; 1184],
                })],
                Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "handshake".into(),
                },
            );
        }

        let applied = ingest_dm_packet(
            &state,
            &content_store,
            &sink,
            Some(std::sync::Arc::clone(&pending)),
            bob,
            "bob-device-64hex",
            peer_node_id,
            &packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("a non-friend invite stages (not an error)");
        assert!(!applied, "an invite never emits dm-received (Ok(false))");

        // No Space bootstrapped — staging writes nothing to owner-state.
        {
            let st = state.lock().await;
            assert!(
                !st.spaces.contains_key(&space_id),
                "a staged (non-friend) invite must NOT write the DM Space"
            );
        }

        // The invite is parked in the process-local store, keyed by space_id.
        let staged = pending.list();
        assert_eq!(staged.len(), 1, "exactly one invite staged");
        assert_eq!(staged[0].signed.space_id, space_id);
        // Tunnel route is cache-refresh entitled (ZEB-483: tunnel = true).
        assert!(staged[0].refresh_owner_device_cache);

        // The UI is prompted once + told the list changed; no dm-received.
        let frames = sink_handle.frames();
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
        assert!(
            frames
                .iter()
                .all(|(n, _)| n != crate::dm_outbox::DM_RECEIVED_EVENT),
            "an invite must not emit dm-received"
        );
    }

    /// ZEB-685 (S3): the dispatch arm's own logic — resolve the tunnel peer to
    /// its owner, apply, and mark the owner-state engine dirty ONLY on a fresh
    /// insert — verified at the `ingest_dm_packet` level (the handler itself is
    /// unit-tested in `dm_outbox.rs`). A RevocationPush is a control frame:
    /// `Ok(false)`, no `dm-received`.
    #[tokio::test]
    async fn ingest_dm_packet_applies_revocation_push_and_marks_dirty() {
        use harmony_owner::certs::{EnrollmentCert, RevocationCert, RevocationReason};
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let bob = OwnerAddr([0xB0; 16]);
        let state = std::sync::Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            std::sync::Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(std::sync::Arc::clone(&sink_handle));

        // Alice's master + revoked device D — a real master-signed RevocationPush.
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[0x51; 32]);
        let master_bundle = PubKeyBundle::classical_only(master_sk.verifying_key().to_bytes());
        let alice = OwnerAddr(master_bundle.identity_hash());
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[0x52; 32]);
        let device_bundle = PubKeyBundle::classical_only(device_sk.verifying_key().to_bytes());
        let device_id = device_bundle.identity_hash();
        let revoked_ed = device_bundle.classical.ed25519_verify;
        let enrollment = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            device_id,
            device_bundle,
            1_700_000_000,
            None,
        )
        .unwrap();
        let revocation = RevocationCert::sign_master(
            &master_sk,
            master_bundle,
            device_id,
            1_700_000_000,
            RevocationReason::Compromised,
        )
        .unwrap();
        let packet = crate::dm_envelope::encode_packet(
            &crate::dm_envelope::build_revocation_push_packet(revocation, enrollment),
        )
        .unwrap();

        // Seed one of Alice's tunnel contacts so `resolve_owner_for_peer` binds
        // the pushing peer to `alice` (the trust-bind's `expected_owner`).
        let alice_dsa_pubkey = vec![0x17u8; 1952];
        let peer_node_id = crate::tunnel_manager::node_id_from_dsa_pubkey(&alice_dsa_pubkey);
        {
            let mut st = state.lock().await;
            st.apply_owner_device_update(
                alice,
                vec![crate::owner_state_types::DeviceIdentityHash([0x1a; 16])],
                vec![None],
                vec![Some(crate::owner_state_types::DeviceTunnelContact {
                    iroh_node_id: [0x09; 32],
                    home_relay_url: None,
                    pq_dsa_pubkey: alice_dsa_pubkey.clone(),
                    pq_kem_pubkey: vec![0x08u8; 1184],
                })],
                Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "handshake".into(),
                },
            );
        }

        let dirty = std::sync::Arc::new(AtomicUsize::new(0));
        let dirty_cb = {
            let d = std::sync::Arc::clone(&dirty);
            move || {
                d.fetch_add(1, Ordering::SeqCst);
            }
        };

        let applied = ingest_dm_packet(
            &state,
            &content_store,
            &sink,
            None,
            bob,
            "bob-device-64hex",
            peer_node_id,
            &packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            Some(&dirty_cb),
        )
        .await
        .expect("a valid RevocationPush applies (control frame, Ok(false))");
        assert!(!applied, "a RevocationPush never emits dm-received");
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            1,
            "a fresh insert marks the owner-state engine dirty exactly once"
        );
        {
            let st = state.lock().await;
            assert!(
                st.revoked_dm_devices
                    .get(&alice)
                    .unwrap()
                    .contains(&revoked_ed),
                "the revoked device landed in the friend-scoped store"
            );
        }
        assert!(
            sink_handle
                .frames()
                .iter()
                .all(|(n, _)| n != crate::dm_outbox::DM_RECEIVED_EVENT),
            "a control frame emits no dm-received"
        );
    }

    /// ZEB-214: a valid read-receipt frame emits `dm-read-receipt` (with the
    /// watermark + read time) and never writes the inbox (it is not a message).
    #[tokio::test]
    async fn ingest_read_receipt_emits_event_and_no_inbox_write() {
        let me = OwnerAddr([0x01; 16]);
        let alice = OwnerAddr([0xA1; 16]);
        let space_id = crate::owner_state_types::SpaceId([0x5A; 16]);
        let state = std::sync::Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            std::sync::Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(std::sync::Arc::clone(&sink_handle));

        // Alice is a cached signer; a 1:1 DM space [alice, me] exists.
        let priv_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub = priv_alice.public_identity();
        let alice_identity_pub = alice_pub.to_public_bytes();
        let alice_dev = crate::owner_state_types::DeviceIdentityHash(alice_pub.address_hash);
        {
            let mut st = state.lock().await;
            st.apply_owner_device_update(
                alice,
                vec![alice_dev],
                vec![Some(alice_identity_pub)],
                vec![],
                Hlc {
                    wall_ms: 50,
                    logical: 0,
                    device_id: "alice-dev".into(),
                },
            );
            let mut members = vec![alice, me];
            members.sort();
            st.spaces.insert(
                space_id,
                crate::owner_state_types::Space {
                    id: space_id,
                    kind: crate::owner_state_types::SpaceKind::Dm,
                    parent: None,
                    community_id: None,
                    name: "dm".into(),
                    transport: None,
                    members,
                    custom_name: None,
                    notification_pref: None,
                    read_receipt_pref: None,
                    left_at: None,
                    created_at: Hlc {
                        wall_ms: 1,
                        logical: 0,
                        device_id: "d".into(),
                    },
                    updated_at: Hlc {
                        wall_ms: 1,
                        logical: 0,
                        device_id: "d".into(),
                    },
                    content_key: Some(crate::owner_state_types::DmContentKey::new([0x22; 32])),
                    prior_content_keys: vec![],
                    current_epoch: None,
                    current_epoch_key: None,
                    old_epoch_keys: std::collections::BTreeMap::new(),
                    admin_addr: None,
                    is_invite_only: None,
                    shared_in_profile: false,
                    pending_join_at: None,
                },
            );
        }

        let signed = crate::dm_envelope::DmReadReceiptSigned {
            space_id,
            sender_owner_addr: alice,
            signing_device_hash: alice_dev,
            read_up_to: Hlc {
                wall_ms: 1234,
                logical: 0,
                device_id: "d".into(),
            },
            sent_at: Hlc {
                wall_ms: 1600,
                logical: 0,
                device_id: "d".into(),
            },
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let sig = priv_alice.sign(&bytes);
        let wire = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::ReadReceipt {
            signed,
            signature: sig,
            signed_bytes: bytes,
        })
        .unwrap();

        let emitted = ingest_dm_packet(
            &state,
            &content_store,
            &sink,
            None,
            me,
            "me-device",
            [0u8; 32],
            &wire,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("a valid read receipt is admitted (control frame, Ok(false))");

        assert!(!emitted, "a receipt is not a chat message");
        let frames = sink_handle.frames();
        let (_, payload) = frames
            .iter()
            .find(|(n, _)| n == "dm-read-receipt")
            .expect("dm-read-receipt emitted");
        assert_eq!(payload["readUpTo"], 1234);
        assert_eq!(payload["at"], 1600);
        assert_eq!(payload["from"], hex::encode(alice.0));
        assert!(
            state.lock().await.inbox.is_empty(),
            "a read receipt must not write the inbox"
        );
    }

    /// ZEB-685 (S3): a RevocationPush from a tunnel peer we cannot bind to a
    /// known owner is rejected before any store mutation — and the owner-state
    /// engine is NOT marked dirty (no spurious publish on a rejected frame).
    #[tokio::test]
    async fn ingest_dm_packet_rejects_revocation_push_from_unbindable_peer() {
        use harmony_owner::certs::{EnrollmentCert, RevocationCert, RevocationReason};
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let bob = OwnerAddr([0xB0; 16]);
        // EMPTY cache — no device contact matches any peer.
        let state = std::sync::Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            std::sync::Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(std::sync::Arc::clone(&sink_handle));

        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[0x51; 32]);
        let master_bundle = PubKeyBundle::classical_only(master_sk.verifying_key().to_bytes());
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[0x52; 32]);
        let device_bundle = PubKeyBundle::classical_only(device_sk.verifying_key().to_bytes());
        let device_id = device_bundle.identity_hash();
        let enrollment = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            device_id,
            device_bundle,
            1_700_000_000,
            None,
        )
        .unwrap();
        let revocation = RevocationCert::sign_master(
            &master_sk,
            master_bundle,
            device_id,
            1_700_000_000,
            RevocationReason::Compromised,
        )
        .unwrap();
        let packet = crate::dm_envelope::encode_packet(
            &crate::dm_envelope::build_revocation_push_packet(revocation, enrollment),
        )
        .unwrap();

        let dirty = std::sync::Arc::new(AtomicUsize::new(0));
        let dirty_cb = {
            let d = std::sync::Arc::clone(&dirty);
            move || {
                d.fetch_add(1, Ordering::SeqCst);
            }
        };

        let err = ingest_dm_packet(
            &state,
            &content_store,
            &sink,
            None,
            bob,
            "bob-device-64hex",
            [0x33; 32], // a peer_node_id absent from the empty cache
            &packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            Some(&dirty_cb),
        )
        .await
        .expect_err("an unbindable tunnel peer must be rejected");
        assert!(err.contains("unbindable"), "got: {err}");
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            0,
            "a rejected push never marks the engine dirty"
        );
        {
            let st = state.lock().await;
            assert!(st.revoked_dm_devices.is_empty(), "nothing stored on reject");
        }
    }

    /// ZEB-640 (1): befriend-then-redeliver. A non-friend tunnel invite is
    /// STAGED; the user then befriends the inviter; a redelivery of the same
    /// invite now auto-accepts (friend tier) and writes the Space. The
    /// `Accepted` arm must PURGE the stale staged entry — otherwise a pending
    /// toast/row survives for a DM Space that already exists in nav. The purge
    /// leg emits exactly one `dm-invite-list-changed` and NO
    /// `dm-invite-received` (nothing to prompt).
    #[tokio::test]
    async fn ingest_dm_packet_purges_staged_invite_on_friend_tier_accept() {
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x77; 16]);
        let state = std::sync::Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            std::sync::Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(std::sync::Arc::clone(&sink_handle));
        let pending = std::sync::Arc::new(crate::pending_dm_invites::PendingDmInvites::new());

        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub = private_alice.public_identity();
        let alice_identity_pub = alice_pub.to_public_bytes();
        let alice = OwnerAddr([0xA1; 16]);
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub.address_hash);

        let mut members = vec![alice, bob];
        members.sort();
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id,
            kind: crate::owner_state_types::SpaceKind::Dm,
            members,
            inviter: alice,
            content_key: crate::owner_state_types::DmContentKey::new([0x42; 32]),
            sender_devices: vec![alice_device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            signing_device_hash: alice_device_hash,
            inviter_identity_pub: alice_identity_pub,
            inviter_enrollment: None,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);
        let packet = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        })
        .unwrap();

        // Alice's device is cached (so `resolve_owner_for_peer` binds the peer),
        // but she is NOT (yet) an active friend — the first delivery stages.
        let alice_dsa_pubkey = vec![0x07u8; 1952];
        let peer_node_id = crate::tunnel_manager::node_id_from_dsa_pubkey(&alice_dsa_pubkey);
        {
            let mut st = state.lock().await;
            st.apply_owner_device_update(
                alice,
                vec![alice_device_hash],
                vec![None],
                vec![Some(crate::owner_state_types::DeviceTunnelContact {
                    iroh_node_id: [0x09; 32],
                    home_relay_url: None,
                    pq_dsa_pubkey: alice_dsa_pubkey.clone(),
                    pq_kem_pubkey: vec![0x08u8; 1184],
                })],
                Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "handshake".into(),
                },
            );
        }

        // Leg 1: non-friend delivery → staged (Space NOT written).
        ingest_dm_packet(
            &state,
            &content_store,
            &sink,
            Some(std::sync::Arc::clone(&pending)),
            bob,
            "bob-device-64hex",
            peer_node_id,
            &packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("a non-friend invite stages (not an error)");
        assert_eq!(pending.list().len(), 1, "precondition: invite staged");
        let frames_after_staging = sink_handle.frames().len();

        // Befriend Alice AFTER staging — the redelivery below now takes the
        // friend-tier auto-accept fork.
        {
            let mut st = state.lock().await;
            st.friend_graph
                .friends
                .insert(alice, crate::friend_graph::active_friend_entry_for_test(2));
        }

        // Leg 2: redeliver the SAME invite → friend-tier auto-accept writes
        // the Space AND purges the now-stale staged entry.
        ingest_dm_packet(
            &state,
            &content_store,
            &sink,
            Some(std::sync::Arc::clone(&pending)),
            bob,
            "bob-device-64hex",
            peer_node_id,
            &packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("a friend-tier invite redelivery must apply");

        {
            let st = state.lock().await;
            assert!(
                st.spaces.contains_key(&space_id),
                "friend-tier auto-accept must write the DM Space"
            );
        }
        assert!(
            pending.list().is_empty(),
            "the stale staged entry must be purged on friend-tier accept"
        );

        // The purge leg emits EXACTLY one dm-invite-list-changed and nothing
        // else — no dm-invite-received (nothing to prompt), no dm-received
        // (invites carry no message).
        let purge_frames = sink_handle.frames().split_off(frames_after_staging);
        assert_eq!(
            purge_frames
                .iter()
                .filter(|(n, _)| n == "dm-invite-list-changed")
                .count(),
            1,
            "purge emits list-changed exactly once"
        );
        assert!(
            purge_frames.iter().all(|(n, _)| n != "dm-invite-received"),
            "purge must never re-prompt"
        );
        assert_eq!(purge_frames.len(), 1, "no other events from the purge leg");
    }

    /// ZEB-484: a `CidNotifyWithBlob` delivers the DM live — the inline blob is
    /// CAS-put (no zenoh content query) and `dm-received` fires. Proven by using
    /// a FRESH (empty) content store: if the inline path didn't CAS-put the blob,
    /// Phase 3's `get(message_cid)` would miss and no `dm-received` would emit.
    #[tokio::test]
    async fn ingest_dm_packet_cidnotify_with_blob_delivers_live() {
        let fx = build_dm_ingest_fixture(b"hello-over-tunnel").await;

        let blob = fx
            .content_store
            .get(&fx.message_cid)
            .await
            .unwrap()
            .expect("fixture stored the encrypted blob");
        let (signed, signature, signed_bytes) =
            match crate::dm_envelope::decode_packet(&fx.packet).unwrap() {
                crate::dm_envelope::DmPacket::CidNotify {
                    signed,
                    signature,
                    signed_bytes,
                } => (signed, signature, signed_bytes),
                other => panic!("fixture packet must be a bare CidNotify, got {other:?}"),
            };
        let with_blob = crate::dm_envelope::DmPacket::CidNotifyWithBlob {
            signed,
            signature,
            signed_bytes,
            storage_blob: blob,
        };
        let wire = crate::dm_envelope::encode_packet(&with_blob).unwrap();

        let fresh_store: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        assert!(
            fresh_store.get(&fx.message_cid).await.unwrap().is_none(),
            "precondition: the fresh store does not yet hold the blob"
        );

        let applied = ingest_dm_packet(
            &fx.crdt_state,
            &fresh_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &wire,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("a CidNotifyWithBlob from an admitted sender must deliver");
        assert!(applied, "a delivered DM emits dm-received (Ok(true))");

        assert!(
            fresh_store.get(&fx.message_cid).await.unwrap().is_some(),
            "the inline blob must be CAS-put so the recipient can read it"
        );
        assert!(
            fx.sink_handle
                .frames()
                .iter()
                .any(|(n, _)| n == crate::dm_outbox::DM_RECEIVED_EVENT),
            "a CidNotifyWithBlob must emit dm-received"
        );
    }

    /// ZEB-709: a live-tunnel DM insert must mark the OWNER-STATE engine dirty
    /// exactly once (never on the deduped re-delivery). The tunnel sender gets
    /// its Ack and stops re-sending, so an un-notified inbox write has no
    /// re-delivery path — a crash before an unrelated owner-state flush would
    /// lose the message permanently.
    #[tokio::test]
    async fn ingest_dm_packet_message_insert_marks_owner_state_dirty_once_zeb709() {
        let fx = build_dm_ingest_fixture(b"dirty-pin-over-tunnel").await;
        let dirty = std::sync::Arc::new(AtomicUsize::new(0));
        let dirty_cb = {
            let d = std::sync::Arc::clone(&dirty);
            move || {
                d.fetch_add(1, Ordering::SeqCst);
            }
        };

        let applied = ingest_dm_packet(
            &fx.crdt_state,
            &fx.content_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &fx.packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            Some(&dirty_cb),
        )
        .await
        .expect("an admitted CidNotify must deliver");
        assert!(applied, "first delivery is a genuine insert");
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            1,
            "the inbox insert marks the owner-state engine dirty exactly once"
        );

        let re_applied = ingest_dm_packet(
            &fx.crdt_state,
            &fx.content_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &fx.packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            Some(&dirty_cb),
        )
        .await
        .expect("a re-delivered CidNotify dedupes cleanly");
        assert!(!re_applied, "the dedup path is Merged, not a new insert");
        assert_eq!(
            dirty.load(Ordering::SeqCst),
            1,
            "no spurious dirty on the deduped re-delivery"
        );
    }

    /// ZEB-484 (Qodo security): a `CidNotifyWithBlob` whose sender is NOT admitted
    /// (here: no Space exists → Phase-2 admission fails) must be rejected WITHOUT
    /// writing the inline blob to CAS — otherwise invalid tunnel traffic could
    /// pollute the local CAS with arbitrary blobs. The CAS-put happens only AFTER
    /// admission (block 2b), so a fresh store stays empty on rejection.
    #[tokio::test]
    async fn ingest_dm_packet_cidnotify_with_blob_unadmitted_does_not_cas_put() {
        let fx = build_dm_ingest_fixture(b"unadmitted-blob").await;
        let blob = fx
            .content_store
            .get(&fx.message_cid)
            .await
            .unwrap()
            .expect("fixture stored the encrypted blob");
        let (signed, signature, signed_bytes) =
            match crate::dm_envelope::decode_packet(&fx.packet).unwrap() {
                crate::dm_envelope::DmPacket::CidNotify {
                    signed,
                    signature,
                    signed_bytes,
                } => (signed, signature, signed_bytes),
                other => panic!("fixture packet must be a bare CidNotify, got {other:?}"),
            };
        let with_blob = crate::dm_envelope::DmPacket::CidNotifyWithBlob {
            signed,
            signature,
            signed_bytes,
            storage_blob: blob,
        };
        let wire = crate::dm_envelope::encode_packet(&with_blob).unwrap();

        // FRESH state with NO Space → admission fails BEFORE the post-admission
        // CAS-put. Fresh empty store so any write would be observable.
        let empty_state =
            std::sync::Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let fresh_store: Arc<dyn crate::content_store::ContentStore> =
            std::sync::Arc::new(crate::content_store::InMemoryStub::default());

        let err = ingest_dm_packet(
            &empty_state,
            &fresh_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &wire,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("an unadmitted CidNotifyWithBlob must be rejected");
        assert!(!err.is_empty());
        assert!(
            fresh_store.get(&fx.message_cid).await.unwrap().is_none(),
            "a rejected (unadmitted) CidNotifyWithBlob must NOT write its blob to CAS"
        );
    }

    /// ZEB-484 (CodeRabbit): the inline path's block-2b CID binding fails closed —
    /// an ADMITTED `CidNotifyWithBlob` whose inline blob does NOT hash to the
    /// signed `message_cid` is rejected with the block-2b error and no CAS write.
    /// (Complements `ingest_dm_packet_rejects_blob_whose_cid_mismatches_signed_cid`,
    /// which covers the CAS-FETCH path; this covers the INLINE path.)
    #[tokio::test]
    async fn ingest_dm_packet_cidnotify_with_blob_cid_mismatch_rejected() {
        let fx = build_dm_ingest_fixture(b"mismatch-blob").await;
        let (signed, signature, signed_bytes) =
            match crate::dm_envelope::decode_packet(&fx.packet).unwrap() {
                crate::dm_envelope::DmPacket::CidNotify {
                    signed,
                    signature,
                    signed_bytes,
                } => (signed, signature, signed_bytes),
                other => panic!("fixture packet must be a bare CidNotify, got {other:?}"),
            };
        // A blob that does NOT hash to `signed.message_cid`.
        let with_blob = crate::dm_envelope::DmPacket::CidNotifyWithBlob {
            signed,
            signature,
            signed_bytes,
            storage_blob: vec![0xABu8; 64],
        };
        let wire = crate::dm_envelope::encode_packet(&with_blob).unwrap();

        // Admitted sender (fixture state holds the Space) → Phase 2 passes → block
        // 2b's CID binding fires. Fresh empty store to observe no write.
        let fresh_store: Arc<dyn crate::content_store::ContentStore> =
            std::sync::Arc::new(crate::content_store::InMemoryStub::default());
        let err = ingest_dm_packet(
            &fx.crdt_state,
            &fresh_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &wire,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("a CID-mismatched inline blob must be rejected");
        assert!(
            err.contains("inline blob CID does not match message_cid"),
            "expected the block-2b binding error, got: {err}"
        );
        assert!(
            fresh_store.get(&fx.message_cid).await.unwrap().is_none(),
            "a mismatched inline blob must NOT be written to CAS"
        );
    }

    /// CodeRabbit F1 (security): an invite whose `signed.inviter` names a
    /// DIFFERENT owner than the authenticated tunnel peer is REJECTED — the
    /// invite is genuinely signed by Alice's device (signature verifies) but
    /// claims `inviter = Carol`. The peer reverse-resolves to Alice (cached
    /// contact), so the inviter-bind gate fires and NEITHER the OwnerDeviceCache
    /// NOR the Space is mutated, and no event is emitted. Without the bind, a
    /// valid signer could poison the cache (map its device under any owner) +
    /// seed a spoofed DM Space.
    #[tokio::test]
    async fn ingest_dm_packet_rejects_invite_whose_inviter_mismatches_tunnel_peer() {
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x77; 16]);
        let state = std::sync::Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            std::sync::Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(std::sync::Arc::clone(&sink_handle));

        // Alice's device signs the invite (the authenticated tunnel peer).
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub = private_alice.public_identity();
        let alice_identity_pub = alice_pub.to_public_bytes();
        let alice = OwnerAddr([0xA1; 16]);
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub.address_hash);

        // ...but the invite CLAIMS Carol as the inviter (the forgery).
        let carol = OwnerAddr([0xCC; 16]);

        let mut members = vec![carol, bob];
        members.sort();
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id,
            kind: crate::owner_state_types::SpaceKind::Dm,
            members,
            inviter: carol,
            content_key: crate::owner_state_types::DmContentKey::new([0x42; 32]),
            sender_devices: vec![alice_device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            signing_device_hash: alice_device_hash,
            inviter_identity_pub: alice_identity_pub,
            inviter_enrollment: None,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);
        let packet = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        })
        .unwrap();

        // The peer reverse-resolves to ALICE (her contact is cached), not Carol.
        let alice_dsa_pubkey = vec![0x07u8; 1952];
        let peer_node_id = crate::tunnel_manager::node_id_from_dsa_pubkey(&alice_dsa_pubkey);
        {
            let mut st = state.lock().await;
            st.apply_owner_device_update(
                alice,
                vec![alice_device_hash],
                vec![None],
                vec![Some(crate::owner_state_types::DeviceTunnelContact {
                    iroh_node_id: [0x09; 32],
                    home_relay_url: None,
                    pq_dsa_pubkey: alice_dsa_pubkey.clone(),
                    pq_kem_pubkey: vec![0x08u8; 1184],
                })],
                Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "handshake".into(),
                },
            );
        }

        // Snapshot Alice's cache entry + the Space set BEFORE the call so we can
        // prove neither was mutated by the rejected invite.
        let (alice_entry_before, spaces_len_before) = {
            let st = state.lock().await;
            (
                st.owner_device_cache.devices.get(&alice).cloned(),
                st.spaces.len(),
            )
        };

        let err = ingest_dm_packet(
            &state,
            &content_store,
            &sink,
            None,
            bob,
            "bob-device-64hex",
            peer_node_id,
            &packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("an inviter-vs-peer mismatch must be rejected");
        assert!(
            err.contains("InviterMismatch"),
            "rejection must come from the inviter-bind gate (got: {err})"
        );

        let st = state.lock().await;
        // Carol's owner was NEVER cached (no poisoning).
        assert!(
            !st.owner_device_cache.devices.contains_key(&carol),
            "a rejected mismatched invite must not cache the claimed (Carol) owner"
        );
        // Alice's pre-existing entry is byte-for-byte unchanged.
        assert_eq!(
            st.owner_device_cache.devices.get(&alice).cloned(),
            alice_entry_before,
            "the bound peer's own cache entry must be untouched by a rejected invite"
        );
        // No spoofed DM Space.
        assert!(
            !st.spaces.contains_key(&space_id),
            "a rejected mismatched invite must not seed a spoofed DM Space"
        );
        assert_eq!(
            st.spaces.len(),
            spaces_len_before,
            "no Space mutation on a rejected invite"
        );
        drop(st);
        assert!(
            sink_handle.frames().is_empty(),
            "a rejected invite must not emit"
        );
    }

    /// CodeRabbit F1: an invite from a peer that cannot be reverse-resolved to
    /// any cached owner (no handshake contact matches the authenticated NodeId)
    /// is REJECTED — an unbindable invite must not be trusted, even if it is
    /// otherwise well-formed and self-consistently signed.
    #[tokio::test]
    async fn ingest_dm_packet_rejects_invite_from_unbindable_peer() {
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x77; 16]);
        let state = std::sync::Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            std::sync::Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(std::sync::Arc::clone(&sink_handle));

        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub = private_alice.public_identity();
        let alice_identity_pub = alice_pub.to_public_bytes();
        let alice = OwnerAddr([0xA1; 16]);
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub.address_hash);

        let mut members = vec![alice, bob];
        members.sort();
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id,
            kind: crate::owner_state_types::SpaceKind::Dm,
            members,
            inviter: alice,
            content_key: crate::owner_state_types::DmContentKey::new([0x42; 32]),
            sender_devices: vec![alice_device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            signing_device_hash: alice_device_hash,
            inviter_identity_pub: alice_identity_pub,
            inviter_enrollment: None,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);
        let packet = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        })
        .unwrap();

        // NO contact is seeded — the OwnerDeviceCache is empty, so the peer
        // NodeId reverse-lookup finds no owner and the invite is unbindable.
        let unknown_peer_node_id = [0xEE; 32];

        let err = ingest_dm_packet(
            &state,
            &content_store,
            &sink,
            None,
            bob,
            "bob-device-64hex",
            unknown_peer_node_id,
            &packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("an invite from an unbindable peer must be rejected");
        assert!(
            err.contains("unbindable"),
            "rejection must come from the unbindable-peer gate (got: {err})"
        );

        let st = state.lock().await;
        assert!(
            st.owner_device_cache.devices.is_empty(),
            "an unbindable invite must not mutate the cache"
        );
        assert!(
            !st.spaces.contains_key(&space_id),
            "an unbindable invite must not seed a Space"
        );
        drop(st);
        assert!(
            sink_handle.frames().is_empty(),
            "an unbindable invite must not emit"
        );
    }

    /// A bad packet (corrupted bytes / unknown sender) is rejected as `Err`
    /// with NO inbox mutation and NO emit — the drain logs+drops it.
    #[tokio::test]
    async fn ingest_dm_packet_rejects_bad_packet_without_side_effects() {
        let fx = build_dm_ingest_fixture(b"hi").await;

        // Flip a byte inside the signed body so the signature no longer verifies
        // (still a structurally-decodable CidNotify, so it reaches the admission
        // signature check and fails there).
        let mut tampered = fx.packet.clone();
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0xFF;

        let err = ingest_dm_packet(
            &fx.crdt_state,
            &fx.content_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &tampered,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("a tampered packet must be rejected");
        assert!(!err.is_empty());

        assert!(
            fx.crdt_state.lock().await.inbox.is_empty(),
            "a rejected packet must not touch the inbox"
        );
        assert!(
            fx.sink_handle.frames().is_empty(),
            "a rejected packet must not emit"
        );
    }

    /// Greptile P2: a `DmPacket::Ack` arriving on the tunnel ingest path is
    /// explicitly rejected (Acks are not handled here) — the error names the
    /// "Ack" type and NEITHER the inbox NOR the event sink is touched. The
    /// rejection arm is a bare `return Err(...)` on the variant match; this test
    /// pins it so a future change that starts accepting Acks on this path can't
    /// land untested.
    #[tokio::test]
    async fn ingest_dm_packet_rejects_an_ack_packet() {
        let fx = build_dm_ingest_fixture(b"hi").await;

        // A structurally-valid Ack — `decode_packet` requires `signing_device_hash
        // ∈ ack_from_devices`. The ingest Ack arm rejects on the variant match
        // BEFORE any signature verification, so an all-zero signature is fine
        // and the peer_node_id is irrelevant (`[0u8; 32]`).
        let device_hash = crate::owner_state_types::DeviceIdentityHash([0x01; 16]);
        let signed = crate::dm_envelope::DmAckSigned {
            space_id: SpaceId([0x55; 16]),
            message_cid: ContentId::from_bytes([0xab; 32]),
            ack_from_owner_addr: OwnerAddr([0xA1; 16]),
            ack_from_devices: vec![device_hash],
            signing_device_hash: device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let packet = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Ack {
            signed,
            signature: [0u8; 64],
            signed_bytes,
        })
        .expect("a well-formed Ack must encode");

        let err = ingest_dm_packet(
            &fx.crdt_state,
            &fx.content_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("an Ack on the tunnel ingest path must be rejected");
        assert!(
            err.contains("Ack"),
            "the rejection must name the Ack packet type, got: {err}"
        );

        assert!(
            fx.crdt_state.lock().await.inbox.is_empty(),
            "a rejected Ack must not touch the inbox"
        );
        assert!(
            fx.sink_handle.frames().is_empty(),
            "a rejected Ack must not emit"
        );
    }

    /// CodeAnt F1 (TOCTOU): admission is checked under the lock, the lock is
    /// dropped for the slow CAS fetch, and the Space/membership are then
    /// re-checked under a SECOND lock before decrypt + apply. If the sender
    /// loses membership DURING the fetch window, the Phase-C re-check must
    /// REJECT the DM.
    ///
    /// This test lands the revocation precisely in the TOCTOU window: a
    /// content-store wrapper revokes Alice's membership as a side-effect of the
    /// CAS `get()` (i.e. after admission passed, before the re-check runs). The
    /// blob is still returned, so the ONLY thing that can stop the apply is the
    /// Phase-C re-check — proving it exists and fires (not admission).
    #[tokio::test]
    async fn ingest_dm_packet_rejects_when_sender_loses_membership_mid_fetch() {
        use crate::content_store::{ContentStore, ContentStoreError, InMemoryStub};
        use harmony_content::cid::ContentId as Cid;
        use std::sync::Arc as StdArc;

        /// Returns the blob normally, but as a side-effect (simulating the kick
        /// racing in during the slow fetch) removes Alice from the Space's
        /// members. The mutation lands strictly between admission and the
        /// Phase-C re-check.
        struct RevokeOnGetStore {
            inner: InMemoryStub,
            crdt_state: StdArc<Mutex<crate::owner_state_crdt::OwnerState>>,
            space_id: SpaceId,
            victim: OwnerAddr,
        }
        #[async_trait]
        impl ContentStore for RevokeOnGetStore {
            async fn put(&self, cid: Cid, blob: Vec<u8>) -> Result<(), ContentStoreError> {
                self.inner.put(cid, blob).await
            }
            async fn get(&self, cid: &Cid) -> Result<Option<Vec<u8>>, ContentStoreError> {
                // Revoke membership in the fetch window (the TOCTOU race).
                {
                    let mut state = self.crdt_state.lock().await;
                    if let Some(space) = state.spaces.get_mut(&self.space_id) {
                        space.members.retain(|m| *m != self.victim);
                    }
                }
                self.inner.get(cid).await
            }
        }

        let fx = build_dm_ingest_fixture(b"revoked mid-fetch").await;
        // Re-stage the blob into the racing store (admission needs the Space
        // still intact at admission time, which the fixture provides).
        let inner = InMemoryStub::default();
        inner
            .put(
                fx.message_cid,
                fx.content_store
                    .get(&fx.message_cid)
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .await
            .unwrap();
        let racing_store: StdArc<dyn ContentStore> = StdArc::new(RevokeOnGetStore {
            inner,
            crdt_state: fx.crdt_state.clone(),
            space_id: fx.space_id,
            victim: fx.alice,
        });

        let err = ingest_dm_packet(
            &fx.crdt_state,
            &racing_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &fx.packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("a sender kicked mid-fetch must be rejected by the Phase-C re-check");
        assert!(
            err.contains("membership"),
            "rejection must come from the Phase-C membership re-check (got: {err})"
        );

        assert!(
            fx.crdt_state.lock().await.inbox.is_empty(),
            "a re-check-rejected DM must not touch the inbox"
        );
        assert!(
            fx.sink_handle.frames().is_empty(),
            "a re-check-rejected DM must not emit dm-received"
        );
    }

    /// CR3 (ZEB-473): a content store that returns bytes whose CID does NOT
    /// match the signed `message_cid` must cause ingest to Err with a
    /// "CID does not match" message, with NO inbox mutation and NO emit — the
    /// blob↔packet binding check rejects a poisoned/mismatched CAS blob before
    /// it can be keyed under the signed CID and decrypted.
    #[tokio::test]
    async fn ingest_dm_packet_rejects_blob_whose_cid_mismatches_signed_cid() {
        use crate::content_store::{ContentStore, ContentStoreError, InMemoryStub};
        use harmony_content::cid::ContentId as Cid;
        use std::sync::Arc as StdArc;

        /// Returns DIFFERENT bytes than were stored under `message_cid`,
        /// simulating a poisoned local CAS (or a backend serving the wrong
        /// blob). The returned bytes hash to a different CID.
        struct MismatchOnGetStore {
            inner: InMemoryStub,
        }
        #[async_trait]
        impl ContentStore for MismatchOnGetStore {
            async fn put(&self, cid: Cid, blob: Vec<u8>) -> Result<(), ContentStoreError> {
                self.inner.put(cid, blob).await
            }
            async fn get(&self, _cid: &Cid) -> Result<Option<Vec<u8>>, ContentStoreError> {
                // Hand back bytes that do NOT hash to the requested CID.
                Ok(Some(
                    b"poisoned bytes that do not hash to message_cid".to_vec(),
                ))
            }
        }

        let fx = build_dm_ingest_fixture(b"original body").await;
        let poisoned_store: StdArc<dyn ContentStore> = StdArc::new(MismatchOnGetStore {
            inner: InMemoryStub::default(),
        });

        let err = ingest_dm_packet(
            &fx.crdt_state,
            &poisoned_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &fx.packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("a CAS blob whose CID != signed message_cid must be rejected");
        assert!(
            err.contains("CID does not match"),
            "rejection must come from the blob↔CID binding check (got: {err})"
        );

        assert!(
            fx.crdt_state.lock().await.inbox.is_empty(),
            "a CID-mismatch-rejected DM must not touch the inbox"
        );
        assert!(
            fx.sink_handle.frames().is_empty(),
            "a CID-mismatch-rejected DM must not emit dm-received"
        );
    }

    // ── ZEB-710 (D3): ports of the deleted `dm_outbox`
    //    `handle_unicast_*` / `handle_cidnotify_lifted_*` unit tests onto the
    //    LIVE receive entry point `ingest_dm_packet`. The orphaned handler and
    //    `ingest_dm_packet` share the same verify/decrypt helpers
    //    (`verify_cidnotify_admission`, `verify_cidnotify_sender_binding`,
    //    `decrypt_and_bind_dm_blob`), so these pin that the LIVE path enforces
    //    each property. Setup uses `dm_inbox_ingest`'s own fixtures/idioms; the
    //    dm_outbox-private helpers (`build_cidnotify_fixture`,
    //    `run_handle_cidnotify_lifted`, `GatedCasStub`) were deleted with the
    //    handler. Each test asserts the LIVE path's observable outcome (no inbox
    //    write / no dm-received emit / specific rejection), naming its original.

    /// A DM/GroupDm/Channel `Space` literal for the ZEB-710 ports (mirrors
    /// `build_dm_ingest_fixture`'s Space). Callers pass sorted `members`.
    fn zeb710_space(
        space_id: SpaceId,
        kind: crate::owner_state_types::SpaceKind,
        members: Vec<OwnerAddr>,
        content_key: Option<crate::owner_state_types::DmContentKey>,
        prior_content_keys: Vec<crate::owner_state_types::DmContentKey>,
    ) -> crate::owner_state_types::Space {
        crate::owner_state_types::Space {
            id: space_id,
            kind,
            parent: None,
            community_id: None,
            name: "Alice".into(),
            transport: None,
            members,
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
            content_key,
            prior_content_keys,
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        }
    }

    /// Port of `handle_unicast_cidnotify_sender_binding_mismatch_drops`: the
    /// decrypted payload's `sender` names a DIFFERENT owner than the resolved
    /// signer (Alice). The signature verifies and the blob decrypts, but
    /// `decrypt_and_bind_dm_blob`'s sender-impersonation defense rejects — the
    /// LIVE path drops it (Err), with NO inbox write and NO dm-received emit.
    #[tokio::test]
    async fn ingest_dm_packet_sender_binding_mismatch_drops_zeb710() {
        let fx = build_dm_ingest_fixture(b"unused-original-body").await;

        // Re-encrypt a blob whose payload.sender is an attacker (NOT the resolved
        // signer, Alice), under the fixture's content_key + the Space's AAD, then
        // sign a fresh CidNotify for it with Alice's real key.
        let attacker = OwnerAddr([0xFF; 16]);
        let content_key = crate::owner_state_types::DmContentKey::new([0x42u8; 32]); // fixture's key
        let space = fx
            .crdt_state
            .lock()
            .await
            .spaces
            .get(&fx.space_id)
            .cloned()
            .expect("fixture installed the DM Space");
        let aad = crate::dm_crypto::compute_aad(&space).unwrap();
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
        let blob = crate::dm_crypto::encrypt_dm_message(&content_key, &aad, &payload).unwrap();
        let message_cid = harmony_content::cid::ContentId::for_book(
            &blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        fx.content_store.put(message_cid, blob).await.unwrap();

        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_device_hash = crate::owner_state_types::DeviceIdentityHash(
            private_alice.public_identity().address_hash,
        );
        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id: fx.space_id,
            message_cid,
            sender_owner_addr: fx.alice,
            sender_devices: vec![alice_device_hash],
            signing_device_hash: alice_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);
        let wire = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        })
        .unwrap();

        let err = ingest_dm_packet(
            &fx.crdt_state,
            &fx.content_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &wire,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("a payload-sender != resolved-signer DM must be dropped");
        assert!(
            err.contains("SenderImpersonation"),
            "rejection must come from decrypt_and_bind_dm_blob's sender-binding defense (got: {err})"
        );

        assert!(
            fx.crdt_state.lock().await.inbox.is_empty(),
            "InboxEntry MUST NOT be installed on SenderImpersonation"
        );
        assert!(
            fx.sink_handle.frames().is_empty(),
            "no dm-received must fire on SenderImpersonation"
        );
    }

    /// Port of `handle_unicast_cidnotify_owner_field_mismatch_drops_no_cache_update`:
    /// `signed.sender_owner_addr` != the resolved owner. The signature verifies
    /// (re-signed by Alice), but `verify_cidnotify_admission`'s owner-field check
    /// rejects with `OwnerFieldMismatch` — drop, no inbox write, and (the LIVE
    /// tunnel carrier owes no device-cache refresh) no `owner_device_cache` write.
    #[tokio::test]
    async fn ingest_dm_packet_owner_field_mismatch_drops_no_cache_update_zeb710() {
        let fx = build_dm_ingest_fixture(b"owner-field-mismatch").await;

        // Decode the fixture's CidNotify, swap sender_owner_addr to an attacker,
        // and re-sign so the signature still verifies — the owner-field check is
        // the explicit defense, NOT a downstream signature failure.
        let (mut signed, _sig, _bytes) =
            match crate::dm_envelope::decode_packet(&fx.packet).unwrap() {
                crate::dm_envelope::DmPacket::CidNotify {
                    signed,
                    signature,
                    signed_bytes,
                } => (signed, signature, signed_bytes),
                other => panic!("fixture packet must be a bare CidNotify, got {other:?}"),
            };
        let attacker = OwnerAddr([0xFF; 16]);
        signed.sender_owner_addr = attacker;
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let new_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let new_sig = private_alice.sign(&new_bytes);
        let wire = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::CidNotify {
            signed,
            signature: new_sig,
            signed_bytes: new_bytes,
        })
        .unwrap();

        // Snapshot Alice's cache entry to confirm no cache mutation on reject.
        let alice_cache_before = fx
            .crdt_state
            .lock()
            .await
            .owner_device_cache
            .devices
            .get(&fx.alice)
            .cloned()
            .expect("fixture pre-seeded Alice's device cache");

        let err = ingest_dm_packet(
            &fx.crdt_state,
            &fx.content_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &wire,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("an owner-field mismatch must be dropped");
        assert!(
            err.contains("OwnerFieldMismatch"),
            "rejection must come from the admission owner-field check (got: {err})"
        );

        let st = fx.crdt_state.lock().await;
        assert!(
            st.inbox.is_empty(),
            "InboxEntry MUST NOT be installed on OwnerFieldMismatch"
        );
        assert_eq!(
            st.owner_device_cache.devices.get(&fx.alice).cloned(),
            Some(alice_cache_before),
            "Alice's cache entry MUST be untouched (cache-poisoning regression)"
        );
        assert!(
            !st.owner_device_cache.devices.contains_key(&attacker),
            "attacker OwnerAddr MUST NOT be cached"
        );
        drop(st);
        assert!(
            fx.sink_handle.frames().is_empty(),
            "no dm-received must fire on OwnerFieldMismatch"
        );
    }

    /// Port of `handle_unicast_cidnotify_unknown_signing_key_drops`: the
    /// `signing_device_hash` is present in the cache but its identity-pub entry
    /// is `None` (pre-bootstrap: hash known, pub not yet learned), so
    /// `lookup_pubkey_for_device` returns None → `UnknownSigningKey`. Drop, no
    /// inbox write, no emit — admission bails before the CAS fetch.
    #[tokio::test]
    async fn ingest_dm_packet_unknown_signing_key_drops_zeb710() {
        use crate::content_store::{ContentStore, InMemoryStub};

        let alice = OwnerAddr([0xA1; 16]);
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x5A; 16]);

        let mut state = crate::owner_state_crdt::OwnerState::default();
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_device_hash = crate::owner_state_types::DeviceIdentityHash(
            private_alice.public_identity().address_hash,
        );
        // Cache the device hash but with NO identity pub → lookup returns None.
        state.apply_owner_device_update(
            alice,
            vec![alice_device_hash],
            vec![None],
            vec![],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        );
        // Install a DM Space so the failure mode is UnknownSigningKey, not SpaceNotFound.
        let mut members = vec![alice, bob];
        members.sort();
        let space = zeb710_space(
            space_id,
            crate::owner_state_types::SpaceKind::Dm,
            members,
            Some(crate::owner_state_types::DmContentKey::new([0xab; 32])),
            vec![],
        );
        assert!(matches!(
            state.apply_space_with_canonicalization(space),
            crate::owner_state_crdt::ApplyOutcome::Inserted
        ));

        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id,
            message_cid: ContentId::from_bytes([0xEE; 32]),
            sender_owner_addr: alice,
            sender_devices: vec![alice_device_hash],
            signing_device_hash: alice_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);
        let wire = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        })
        .unwrap();

        let crdt_state = Arc::new(Mutex::new(state));
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            Arc::new(Arc::clone(&sink_handle));

        let err = ingest_dm_packet(
            &crdt_state,
            &cas,
            &sink,
            None,
            bob,
            "bob-dev",
            [0u8; 32],
            &wire,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("a CidNotify whose signer has no cached pub must be dropped");
        assert!(
            err.contains("UnknownSigningKey"),
            "rejection must come from the admission pubkey lookup (got: {err})"
        );

        assert!(
            crdt_state.lock().await.inbox.is_empty(),
            "no InboxEntry on UnknownSigningKey"
        );
        assert!(
            sink_handle.frames().is_empty(),
            "no dm-received on UnknownSigningKey"
        );
    }

    /// Port of `handle_unicast_cidnotify_decrypt_failure_uses_prior_keys`: the
    /// Space's current `content_key` is K2 with `prior_content_keys=[K1]`, and
    /// the blob is encrypted under K1. `decrypt_and_bind_dm_blob` tries the
    /// current key (fails) then the prior key (succeeds) → delivered.
    #[tokio::test]
    async fn ingest_dm_packet_decrypt_failure_uses_prior_keys_zeb710() {
        use crate::content_store::{ContentStore, InMemoryStub};

        let alice = OwnerAddr([0xA1; 16]);
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x5A; 16]);
        let k1 = crate::owner_state_types::DmContentKey::new([0x11; 32]);
        let k2 = crate::owner_state_types::DmContentKey::new([0x22; 32]);

        let mut state = crate::owner_state_crdt::OwnerState::default();
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub_id = private_alice.public_identity();
        let alice_identity_pub = alice_pub_id.to_public_bytes();
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub_id.address_hash);
        state.apply_owner_device_update(
            alice,
            vec![alice_device_hash],
            vec![Some(alice_identity_pub)],
            vec![],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        );

        let mut members = vec![alice, bob];
        members.sort();
        // current = K2, prior contains K1 (the key the blob is encrypted under).
        let space = zeb710_space(
            space_id,
            crate::owner_state_types::SpaceKind::Dm,
            members,
            Some(k2.clone()),
            vec![k1.clone()],
        );
        assert!(matches!(
            state.apply_space_with_canonicalization(space.clone()),
            crate::owner_state_crdt::ApplyOutcome::Inserted
        ));

        // Encrypt under K1 (the OLD key) — decrypt MUST fall back through prior.
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
        let wire = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        })
        .unwrap();

        let crdt_state = Arc::new(Mutex::new(state));
        let cas_arc: Arc<dyn ContentStore> = Arc::new(cas);
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            Arc::new(Arc::clone(&sink_handle));

        let applied = ingest_dm_packet(
            &crdt_state,
            &cas_arc,
            &sink,
            None,
            bob,
            "bob-dev",
            [0u8; 32],
            &wire,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("prior-key fallback must decrypt + deliver");
        assert!(
            applied,
            "a newly-applied DM (via prior-key fallback) emits dm-received"
        );

        let key = crate::owner_state_types::InboxKey {
            space_id,
            message_cid,
        };
        assert!(
            crdt_state.lock().await.inbox.contains_key(&key),
            "InboxEntry installed via prior-key fallback decrypt"
        );
        let dm_frames = sink_handle
            .frames()
            .into_iter()
            .filter(|(n, _)| n == crate::dm_outbox::DM_RECEIVED_EVENT)
            .count();
        assert_eq!(
            dm_frames, 1,
            "exactly one dm-received via prior-key fallback"
        );
    }

    /// Port of
    /// `handle_cidnotify_lifted_decrypts_via_prior_keys_when_content_key_rotates_during_lift`:
    /// the TOCTOU rotation case. The Space's `content_key` rotates (K1 → K2, with
    /// K1 recorded as prior) as a side-effect of the slow CAS fetch — i.e.
    /// strictly between Phase-2 admission and the Phase-C re-fetch. The returned
    /// blob is still K1-encrypted, so only the Phase-C prior-keys fallback can
    /// decrypt it. Delivered.
    #[tokio::test]
    async fn ingest_dm_packet_prior_keys_when_content_key_rotates_during_fetch_zeb710() {
        use crate::content_store::{ContentStore, ContentStoreError, InMemoryStub};
        use harmony_content::cid::ContentId as Cid;
        use std::sync::Arc as StdArc;

        /// Rotates the Space's content_key to `new_key` (recording the fixture's
        /// original key as prior) as a side-effect of `get()` — landing the
        /// rotation in the TOCTOU window. The returned blob is unchanged (still
        /// encrypted under the original key), so only the prior-keys fallback
        /// can decrypt it.
        struct RotateOnGetStore {
            inner: InMemoryStub,
            crdt_state: StdArc<Mutex<crate::owner_state_crdt::OwnerState>>,
            space_id: SpaceId,
            new_key: crate::owner_state_types::DmContentKey,
        }
        #[async_trait]
        impl ContentStore for RotateOnGetStore {
            async fn put(&self, cid: Cid, blob: Vec<u8>) -> Result<(), ContentStoreError> {
                self.inner.put(cid, blob).await
            }
            async fn get(&self, cid: &Cid) -> Result<Option<Vec<u8>>, ContentStoreError> {
                {
                    let mut state = self.crdt_state.lock().await;
                    if let Some(space) = state.spaces.get_mut(&self.space_id) {
                        if let Some(old) = space.content_key.take() {
                            space.prior_content_keys.insert(0, old);
                        }
                        space.content_key = Some(self.new_key.clone());
                    }
                }
                self.inner.get(cid).await
            }
        }

        let fx = build_dm_ingest_fixture(b"rotated mid-fetch").await;
        // Re-stage the (K1-encrypted) blob into the racing store; admission needs
        // the Space intact at admission time, which the fixture provides.
        let inner = InMemoryStub::default();
        inner
            .put(
                fx.message_cid,
                fx.content_store
                    .get(&fx.message_cid)
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .await
            .unwrap();
        let racing_store: StdArc<dyn ContentStore> = StdArc::new(RotateOnGetStore {
            inner,
            crdt_state: fx.crdt_state.clone(),
            space_id: fx.space_id,
            new_key: crate::owner_state_types::DmContentKey::new([0x99; 32]),
        });

        let applied = ingest_dm_packet(
            &fx.crdt_state,
            &racing_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &fx.packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("a key rotation mid-fetch must still decrypt via prior-keys fallback");
        assert!(
            applied,
            "the rotated-key DM delivers via the Phase-C prior-keys fallback"
        );

        let key = crate::owner_state_types::InboxKey {
            space_id: fx.space_id,
            message_cid: fx.message_cid,
        };
        assert!(
            fx.crdt_state.lock().await.inbox.contains_key(&key),
            "InboxEntry installed under rotated key via prior-keys fallback"
        );
        assert!(
            fx.sink_handle
                .frames()
                .iter()
                .any(|(n, _)| n == crate::dm_outbox::DM_RECEIVED_EVENT),
            "a delivered DM emits dm-received"
        );
    }

    /// Port of `handle_cidnotify_lifted_drops_when_space_deleted_during_lift`: the
    /// Space is removed as a side-effect of the slow CAS fetch (TOCTOU window).
    /// The blob is still returned, so the ONLY thing that can stop the apply is
    /// the Phase-C Space re-fetch → drop (Err), no inbox write, no emit.
    #[tokio::test]
    async fn ingest_dm_packet_drops_when_space_deleted_during_fetch_zeb710() {
        use crate::content_store::{ContentStore, ContentStoreError, InMemoryStub};
        use harmony_content::cid::ContentId as Cid;
        use std::sync::Arc as StdArc;

        /// Removes the Space entirely as a side-effect of `get()` — landing the
        /// deletion strictly between admission and the Phase-C re-fetch.
        struct DeleteSpaceOnGetStore {
            inner: InMemoryStub,
            crdt_state: StdArc<Mutex<crate::owner_state_crdt::OwnerState>>,
            space_id: SpaceId,
        }
        #[async_trait]
        impl ContentStore for DeleteSpaceOnGetStore {
            async fn put(&self, cid: Cid, blob: Vec<u8>) -> Result<(), ContentStoreError> {
                self.inner.put(cid, blob).await
            }
            async fn get(&self, cid: &Cid) -> Result<Option<Vec<u8>>, ContentStoreError> {
                {
                    let mut state = self.crdt_state.lock().await;
                    state.spaces.remove(&self.space_id);
                }
                self.inner.get(cid).await
            }
        }

        let fx = build_dm_ingest_fixture(b"space deleted mid-fetch").await;
        let inner = InMemoryStub::default();
        inner
            .put(
                fx.message_cid,
                fx.content_store
                    .get(&fx.message_cid)
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .await
            .unwrap();
        let racing_store: StdArc<dyn ContentStore> = StdArc::new(DeleteSpaceOnGetStore {
            inner,
            crdt_state: fx.crdt_state.clone(),
            space_id: fx.space_id,
        });

        let err = ingest_dm_packet(
            &fx.crdt_state,
            &racing_store,
            &fx.sink,
            None,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &fx.packet,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("a Space deleted mid-fetch must be rejected by the Phase-C re-fetch");
        assert!(
            err.contains("Space deleted"),
            "rejection must come from the Phase-C Space re-fetch (got: {err})"
        );

        assert!(
            fx.crdt_state.lock().await.inbox.is_empty(),
            "a re-fetch-rejected DM must not touch the inbox"
        );
        assert!(
            fx.sink_handle.frames().is_empty(),
            "a re-fetch-rejected DM must not emit dm-received"
        );
    }

    /// Port of `handle_cidnotify_lifted_gates_on_spacekind_dm_or_groupdm`
    /// (ZEB-275): a CidNotify whose `space_id` resolves to a non-DM (Channel)
    /// Space — where the resolved sender happens to be a member — must be dropped
    /// by the SpaceKind gate inside admission, BEFORE the CAS fetch. A counting
    /// CAS stub (get_local delegates to get) proves the short-circuit: the
    /// observable end state (empty inbox, no emit) is identical either way, so
    /// the `get_calls == 0` assertion is what makes this test load-bearing.
    ///
    /// (dm_inbox_ingest's existing `deposited_invite_with_non_dm_kind_is_rejected`
    /// covers the INVITE decode gate, not the CidNotify admission gate — so this
    /// property was previously unpinned on the live receive path.)
    #[tokio::test]
    async fn ingest_dm_packet_gates_on_spacekind_dm_or_groupdm_zeb710() {
        use crate::content_store::{ContentStore, ContentStoreError, InMemoryStub};
        use harmony_content::cid::ContentId as Cid;
        use std::sync::Arc as StdArc;

        /// Counts `get()` calls (`get_local` delegates here) so the test can
        /// prove the SpaceKind gate short-circuits admission BEFORE the fetch.
        struct CountingCasStub {
            inner: InMemoryStub,
            get_calls: AtomicUsize,
        }
        #[async_trait]
        impl ContentStore for CountingCasStub {
            async fn put(&self, cid: Cid, blob: Vec<u8>) -> Result<(), ContentStoreError> {
                self.inner.put(cid, blob).await
            }
            async fn get(&self, cid: &Cid) -> Result<Option<Vec<u8>>, ContentStoreError> {
                self.get_calls.fetch_add(1, Ordering::SeqCst);
                self.inner.get(cid).await
            }
        }

        let alice = OwnerAddr([0xA1; 16]);
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x5A; 16]);

        let mut state = crate::owner_state_crdt::OwnerState::default();
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub_id = private_alice.public_identity();
        let alice_identity_pub = alice_pub_id.to_public_bytes();
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub_id.address_hash);
        state.apply_owner_device_update(
            alice,
            vec![alice_device_hash],
            vec![Some(alice_identity_pub)],
            vec![],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        );

        // A Channel Space (non-DM) with Alice in members. Direct-insert bypasses
        // validate_invariants; Channel + content_key=None satisfies the
        // kind-vs-content_key invariant.
        let mut members = vec![alice, bob];
        members.sort();
        let space = zeb710_space(
            space_id,
            crate::owner_state_types::SpaceKind::Channel,
            members,
            None,
            vec![],
        );
        state.spaces.insert(space_id, space);

        // A CidNotify that signature-verifies; its message_cid never resolves in
        // CAS (the gate fires before the fetch).
        let fake_cid = harmony_content::cid::ContentId::for_book(
            b"unused payload",
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id,
            message_cid: fake_cid,
            sender_owner_addr: alice,
            sender_devices: vec![alice_device_hash],
            signing_device_hash: alice_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);
        let wire = crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        })
        .unwrap();

        let crdt_state = Arc::new(Mutex::new(state));
        let cas = StdArc::new(CountingCasStub {
            inner: InMemoryStub::default(),
            get_calls: AtomicUsize::new(0),
        });
        let cas_dyn: Arc<dyn ContentStore> = cas.clone();
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            Arc::new(Arc::clone(&sink_handle));

        let err = ingest_dm_packet(
            &crdt_state,
            &cas_dyn,
            &sink,
            None,
            bob,
            "bob-dev",
            [0u8; 32],
            &wire,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect_err("a CidNotify against a non-DM Space must be dropped");
        assert!(
            err.contains("SpaceKindMismatch"),
            "rejection must come from the SpaceKind gate (got: {err})"
        );

        assert!(
            crdt_state.lock().await.inbox.is_empty(),
            "no InboxEntry for a non-DM Space"
        );
        assert!(
            sink_handle.frames().is_empty(),
            "no dm-received for a non-DM Space"
        );
        // LOAD-BEARING: the SpaceKind gate must short-circuit admission BEFORE the
        // CAS fetch — the counter would be 1 if admission fell through to Phase 3.
        assert_eq!(
            cas.get_calls.load(Ordering::SeqCst),
            0,
            "SpaceKind gate must fire before the CAS fetch (counter would be 1 if bypassed)"
        );
    }

    // The end-to-end tunnel→drain→ingest assertion lives in `tunnel_task`'s
    // test module (`tunnel_delivered_dm_ingests_end_to_end`), which already owns
    // the loopback iroh handshake harness; it reuses `build_dm_ingest_fixture`
    // via the `pub(crate)` re-export below so the receive-side fixture is not
    // duplicated.

    // ── ZEB-483: recover-side deposited-invite bootstrap (butler/fleet rung) ──

    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{DmContentKey, Space, SpaceKind};

    /// A `ProdDmInboxIngestCtx` over a fresh Bob state that does NOT have the DM
    /// Space pre-installed (simulating offline-at-create), plus a `DmInboxEntry`
    /// whose `invite_packet` carries the signed bootstrap invite and whose
    /// `cidnotify_packet`/`storage_blob` are signed/encrypted by the SAME sender
    /// (Alice) under the SAME `content_key` carried in the invite.
    struct RecoverInviteFixture {
        prod_ctx: ProdDmInboxIngestCtx,
        crdt_state: Arc<Mutex<OwnerState>>,
        entry: DmInboxEntry,
        space_id: SpaceId,
        expected_body: Vec<u8>,
        // Inputs needed to forge a mismatched-inviter invite for the negative test.
        bob: OwnerAddr,
        created_at: Hlc,
        // The DM content key carried by the (valid) invite; reused so the forged
        // invite is structurally identical except for its inviter binding.
        content_key: DmContentKey,
    }

    impl RecoverInviteFixture {
        fn crdt_state_for_test(&self) -> &Arc<Mutex<OwnerState>> {
            &self.crdt_state
        }

        /// A clone of the entry whose invite is re-signed with an inviter that is
        /// NOT the CidNotify's sender (a third owner, Charlie). `apply_deposited_invite`
        /// pins every trust-bearing invite field to the VERIFIED CidNotify sender
        /// (Alice) before any mutation, so this fails-closed at the signer-binding
        /// check (`signing_device_hash` mismatch) — strictly before `apply_invite`.
        fn entry_with_mismatched_inviter(&self) -> DmInboxEntry {
            // A third owner whose identity actually signs the forged invite (so
            // the Ed25519 signature itself is valid — only the inviter-binding
            // gate must reject it).
            let private_charlie = harmony_identity::PrivateIdentity::from_seed(&[0xC3; 32]);
            let charlie_pub = private_charlie.public_identity();
            let charlie_identity_pub = charlie_pub.to_public_bytes();
            let charlie = OwnerAddr([0xC3; 16]);
            let charlie_device_hash =
                crate::owner_state_types::DeviceIdentityHash(charlie_pub.address_hash);

            // members must include the (forged) inviter + self so only the
            // inviter-binding gate is exercised, not the membership gates.
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
            let mut entry = self.entry.clone();
            entry.invite_packet = Some(invite_wire);
            entry
        }

        /// A clone of the entry whose invite is re-signed by the SAME verified
        /// sender (Alice) and matches every pinned field, but carries a NON-DM
        /// `kind`. It must be rejected by `apply_deposited_invite`'s up-front
        /// SpaceKind gate — before `apply_invite` builds any Space.
        fn entry_with_non_dm_invite(&self) -> DmInboxEntry {
            let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
            let alice_pub = private_alice.public_identity();
            let alice = OwnerAddr([0xA1; 16]);
            let alice_device_hash =
                crate::owner_state_types::DeviceIdentityHash(alice_pub.address_hash);
            let mut members = vec![alice, self.bob];
            members.sort();
            let signed = crate::dm_envelope::DmInviteSigned {
                space_id: self.space_id,
                kind: SpaceKind::Community, // non-DM → must be rejected up front
                members,
                inviter: alice,
                content_key: self.content_key.clone(),
                sender_devices: vec![alice_device_hash],
                created_at: self.created_at.clone(),
                signing_device_hash: alice_device_hash,
                inviter_identity_pub: alice_pub.to_public_bytes(),
                inviter_enrollment: None,
            };
            let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
            let signature = private_alice.sign(&signed_bytes);
            let invite_wire =
                crate::dm_envelope::encode_packet(&crate::dm_envelope::DmPacket::Invite {
                    signed,
                    signature,
                    signed_bytes,
                })
                .unwrap();
            let mut entry = self.entry.clone();
            entry.invite_packet = Some(invite_wire);
            entry
        }
    }

    /// Build the recover fixture: a deposited entry (CidNotify + blob + invite)
    /// for a DM Space Bob has NOT yet bootstrapped. Alice is the sender/inviter.
    ///
    /// `pre_cache_sender` models the trust precondition (ZEB-483 / CodeRabbit
    /// Critical): on the legitimate offline-DM path Alice is an EXISTING friend,
    /// so Bob's `owner_device_cache` already maps Alice's signing device → owner
    /// → identity pub (from the friend handshake) and ONLY the Space is missing —
    /// the invite bootstraps just that. Pass `false` to model an UNCACHED sender
    /// (a stranger, or a forged claim that a co-member/friend deposits): the
    /// recover path MUST then reject fail-closed at sender-binding rather than
    /// let the untrusted invite seed `device → owner → pub` trust from nothing.
    fn build_dm_ingest_fixture_without_space_with_invite(
        pre_cache_sender: bool,
    ) -> RecoverInviteFixture {
        let alice = OwnerAddr([0xA1; 16]);
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x5A; 16]);
        let content_key = DmContentKey::new([0x42u8; 32]);
        let body = b"recovered over the deposit rung".to_vec();

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
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        };
        let payload = crate::dm_envelope::MessagePayload {
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
            crate::dm_crypto::encrypt_dm_message(&content_key, &aad, &payload).unwrap();
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

        let entry = DmInboxEntry {
            sender_owner: alice.0,
            cidnotify_packet: Some(cidnotify_packet),
            storage_blob,
            invite_packet: Some(invite_wire),
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
            deposited_at: Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "butler-device".into(),
            },
            deposited_by: "butler-device".into(),
            ingested_by: BTreeSet::new(),
        };

        // Bob's state: NO Space (offline-at-create). Alice's signing device is
        // pre-cached IFF `pre_cache_sender` — the legitimate path resolves the
        // signer from this PRISTINE cache BEFORE the invite runs, and the invite
        // then bootstraps only the missing Space. Without the pre-cache,
        // sender-binding rejects (the invite can no longer seed device trust).
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
            // deposited invite AUTO-ACCEPTS (bootstraps the Space). Without an
            // active friendship the tier fork would STAGE it instead of applying.
            bob_state
                .friend_graph
                .friends
                .insert(alice, crate::friend_graph::active_friend_entry_for_test(1));
        }
        let crdt_state = Arc::new(Mutex::new(bob_state));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            Arc::new(Arc::clone(&sink_handle));

        let prod_ctx = ProdDmInboxIngestCtx {
            device_id: "bob-device-64hex".into(),
            self_owner: bob,
            crdt_state: Arc::clone(&crdt_state),
            content_store,
            sink,
            pending_dm_invites: None,
            enrolled: BTreeSet::new(),
            revoked: crate::revoked_device_projection::RevokedDeviceProjection::new(),
            notify_owner_state_dirty: None,
            device_x25519_priv: zeroize::Zeroizing::new([0x33; 32]),
            owner_keytree: Arc::new(
                crate::owner_state_crypto::KeyTree::derive(&[0x44; 32]).expect("keytree"),
            ),
        };

        RecoverInviteFixture {
            prod_ctx,
            crdt_state,
            entry,
            space_id,
            expected_body: body,
            bob,
            created_at,
            content_key,
        }
    }

    #[tokio::test]
    async fn deposited_invite_bootstraps_space_then_cidnotify_admits() {
        // Legitimate offline-DM path: Alice is an existing friend (pre-cached);
        // only the Space is missing, and the deposited invite bootstraps it.
        let fx = build_dm_ingest_fixture_without_space_with_invite(true);

        // Sanity: the Space is absent pre-recover → a plain CidNotify ingest
        // would fail with SpaceNotFound.
        {
            let st = fx.crdt_state_for_test().lock().await;
            assert!(
                !st.spaces.contains_key(&fx.space_id),
                "space absent pre-recover"
            );
        }

        let verified = fx
            .prod_ctx
            .verify(&fx.entry)
            .await
            .expect("invite bootstraps space, notify admits");
        assert_eq!(verified.space_id, fx.space_id);
        assert_eq!(verified.body, fx.expected_body);

        let st = fx.crdt_state_for_test().lock().await;
        assert!(
            st.spaces.contains_key(&fx.space_id),
            "Space bootstrapped from the deposited invite"
        );
    }

    #[tokio::test]
    async fn deposited_invite_with_wrong_inviter_is_rejected_and_space_absent() {
        let fx = build_dm_ingest_fixture_without_space_with_invite(true);
        let entry = fx.entry_with_mismatched_inviter();
        let err = fx
            .prod_ctx
            .verify(&entry)
            .await
            .expect_err("mismatched inviter must fail-closed");
        // Pinned to the verified CidNotify signer, so it rejects at the
        // signer-binding check (or, defense-in-depth, inside apply_invite).
        assert!(
            err.contains("does not match verified CidNotify")
                || err.contains("apply_invite")
                || err.contains("InviterMismatch"),
            "got {err}"
        );
        let st = fx.crdt_state_for_test().lock().await;
        assert!(
            !st.spaces.contains_key(&fx.space_id),
            "no Space bootstrapped on reject"
        );
    }

    /// ZEB-483 CodeRabbit Critical regression: a deposited invite from a sender
    /// whose signing device is NOT already cached (a stranger, or a forged claim
    /// a malicious co-member/friend deposits) MUST be rejected before it can seed
    /// trust. Pre-fix, the invite seeded `device → owner → pub` and the
    /// co-deposited CidNotify then "verified" against that just-written cache
    /// (circular trust) — admitting a spoofed DM AND poisoning the device cache.
    /// Post-fix, sender-binding runs against the pristine cache first and rejects
    /// fail-closed: no Space, no cache mutation.
    #[tokio::test]
    async fn deposited_invite_from_uncached_sender_is_rejected_no_cache_poison() {
        // Same self-consistent invite + CidNotify (both validly signed by the
        // claimed sender's key), but the sender is NOT pre-cached.
        let fx = build_dm_ingest_fixture_without_space_with_invite(false);
        let alice = OwnerAddr([0xA1; 16]);

        let err = fx
            .prod_ctx
            .verify(&fx.entry)
            .await
            .expect_err("uncached sender must fail-closed (no trust bootstrap)");
        assert!(
            err.contains("verify_cidnotify_sender_binding")
                || err.contains("UnknownSigningKey")
                || err.contains("UnknownSigningDevice"),
            "must reject at sender-binding against the pristine cache, got {err}"
        );

        let st = fx.crdt_state_for_test().lock().await;
        assert!(
            !st.spaces.contains_key(&fx.space_id),
            "no Space bootstrapped from an unverified sender's invite"
        );
        assert!(
            !st.owner_device_cache.devices.contains_key(&alice),
            "device cache NOT poisoned with the claimed sender's mapping"
        );
    }

    /// ZEB-483 CodeRabbit (round 3): a deposited invite carrying a NON-DM `kind`
    /// must never bootstrap a Space. The SpaceKind invariant is enforced at the
    /// earliest point — `decode_packet` rejects a DmInvite whose kind isn't
    /// Dm/GroupDm — so the invite never decodes and `apply_invite` is never
    /// reached. This pins that fail-closed guarantee.
    #[tokio::test]
    async fn deposited_invite_with_non_dm_kind_is_rejected() {
        let fx = build_dm_ingest_fixture_without_space_with_invite(true);
        let entry = fx.entry_with_non_dm_invite();
        let err = fx
            .prod_ctx
            .verify(&entry)
            .await
            .expect_err("non-DM deposited invite must be rejected");
        assert!(
            err.contains("kind must be Dm or GroupDm"),
            "must reject at decode (SpaceKind payload invariant), got {err}"
        );
        let st = fx.crdt_state_for_test().lock().await;
        assert!(
            !st.spaces.contains_key(&fx.space_id),
            "no Space bootstrapped from a non-DM invite"
        );
    }

    /// Task-3 review fix regression: the co-deposit staging arm must fire
    /// ONLY on `DmReceiveError::SpaceNotFound` (the genuine space-absent
    /// bootstrap case) — never on `SenderNotInSpaceMembers` /
    /// `SpaceKindMismatch`, which mean the Space EXISTS. Models a kicked
    /// group-DM co-member: Alice's signing device is still cached locally
    /// (from when she was a member, so sender-binding resolves her), but the
    /// local Space no longer lists her in `members` (she was removed) and she
    /// is not an active friend, so her stale redelivered message still
    /// carries a co-deposited invite claiming her as a member. Pre-fix this
    /// would stage + emit the invite, prompting the user to effectively
    /// re-admit a removed member.
    #[tokio::test]
    async fn co_deposit_membership_failure_does_not_stage_rejoin_prompt() {
        let alice = OwnerAddr([0xA1; 16]);
        let bob = OwnerAddr([0xB0; 16]);
        let charlie = OwnerAddr([0xC3; 16]);
        let dave = OwnerAddr([0xDA; 16]);
        let space_id = SpaceId([0x5B; 16]);
        let content_key = DmContentKey::new([0x42u8; 32]);

        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub = private_alice.public_identity();
        let alice_identity_pub = alice_pub.to_public_bytes();
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub.address_hash);

        // The invite Alice originally co-deposited still lists the full
        // (pre-kick) 4-person group-DM roster — a stale redelivery from
        // before she was removed. `SpaceKind::GroupDm` requires 3..=16
        // members, so a 4-person roster keeps both this invite AND the
        // post-kick 3-person local Space below independently valid.
        let mut invite_members = vec![alice, bob, charlie, dave];
        invite_members.sort();
        let created_at = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice-dev".into(),
        };

        // Encrypt the DM blob under the AAD the invite's member set implies.
        // Irrelevant to the outcome (verification fails before decrypt is
        // ever reached) but kept realistic.
        let aad_space = Space {
            id: space_id,
            kind: SpaceKind::GroupDm,
            parent: None,
            community_id: None,
            name: "Alice + Bob + Charlie + Dave".into(),
            transport: None,
            members: invite_members.clone(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: created_at.clone(),
            updated_at: created_at.clone(),
            content_key: Some(content_key.clone()),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        };
        let payload = crate::dm_envelope::MessagePayload {
            body: b"redelivered after being kicked".to_vec(),
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
            crate::dm_crypto::encrypt_dm_message(&content_key, &aad, &payload).unwrap();
        let message_cid = ContentId::for_book(
            &storage_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();

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

        let inv_signed = crate::dm_envelope::DmInviteSigned {
            space_id,
            kind: SpaceKind::GroupDm,
            members: invite_members,
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

        let entry = DmInboxEntry {
            sender_owner: alice.0,
            cidnotify_packet: Some(cidnotify_packet),
            storage_blob,
            invite_packet: Some(invite_wire),
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
            deposited_at: Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "butler-device".into(),
            },
            deposited_by: "butler-device".into(),
            ingested_by: BTreeSet::new(),
        };

        // Bob's local state: the Space EXISTS (Bob already had it) but Alice
        // is no longer in `members` — she was kicked, leaving a valid
        // 3-person GroupDm (bob, charlie, dave). Her device is still cached
        // from when she WAS a member, so sender-binding resolves her. She is
        // NOT an active friend (no friend_graph entry), matching a group-DM
        // co-member rather than a 1:1 friend.
        let mut bob_state = OwnerState::default();
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
        let mut remaining_members = vec![bob, charlie, dave];
        remaining_members.sort();
        bob_state.spaces.insert(
            space_id,
            Space {
                id: space_id,
                kind: SpaceKind::GroupDm,
                parent: None,
                community_id: None,
                name: "Group DM".into(),
                transport: None,
                members: remaining_members, // Alice already kicked
                custom_name: None,
                notification_pref: None,
                left_at: None,
                created_at: created_at.clone(),
                updated_at: created_at.clone(),
                content_key: Some(content_key.clone()),
                prior_content_keys: vec![],
                current_epoch: None,
                current_epoch_key: None,
                old_epoch_keys: std::collections::BTreeMap::new(),
                admin_addr: None,
                is_invite_only: None,
                shared_in_profile: false,
                read_receipt_pref: None,
                pending_join_at: None,
            },
        );

        let crdt_state = Arc::new(Mutex::new(bob_state));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            Arc::new(Arc::clone(&sink_handle));
        let pending = Arc::new(crate::pending_dm_invites::PendingDmInvites::new());

        let prod_ctx = ProdDmInboxIngestCtx {
            device_id: "bob-device-64hex".into(),
            self_owner: bob,
            crdt_state,
            content_store,
            sink,
            pending_dm_invites: Some(Arc::clone(&pending)),
            enrolled: BTreeSet::new(),
            revoked: crate::revoked_device_projection::RevokedDeviceProjection::new(),
            notify_owner_state_dirty: None,
            device_x25519_priv: zeroize::Zeroizing::new([0x33; 32]),
            owner_keytree: Arc::new(
                crate::owner_state_crypto::KeyTree::derive(&[0x44; 32]).expect("keytree"),
            ),
        };

        let err = prod_ctx
            .verify(&entry)
            .await
            .expect_err("a kicked co-member's redelivered message must fail-closed");
        assert!(
            err.contains("SenderNotInSpaceMembers"),
            "must reject at the space-membership gate (Space exists, sender \
             was removed from it), got {err}"
        );

        assert!(
            pending.list().is_empty(),
            "SenderNotInSpaceMembers must NOT stage an invite — the Space \
             already exists and the sender was removed from it; staging here \
             would prompt the user to re-admit a kicked co-member"
        );
        assert!(
            sink_handle
                .frames()
                .iter()
                .all(|(n, _)| n != "dm-invite-received"),
            "no dm-invite-received emit on a membership-failure error"
        );
    }

    // ── ZEB-236 (final review): cid-keyed declined ledger (sweep re-prompt kill) ──

    /// Build a CO-DEPOSIT entry (signed CidNotify + co-deposited invite) from
    /// Alice that STAGES a non-friend invite on Bob. `body` drives the encrypted
    /// blob → `message_cid`, so two bodies model the SAME message (a mechanical
    /// re-run) vs a genuinely NEW message (a new cid). The co-deposited invite is
    /// identical across bodies — it carries no `message_cid`.
    fn build_staging_co_deposit(body: &[u8]) -> (DmInboxEntry, SpaceId, ContentId) {
        let alice = OwnerAddr([0xA1; 16]);
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x5A; 16]);
        let content_key = DmContentKey::new([0x42u8; 32]);

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

        // AAD is keyed on the sorted member set only, so the transient encrypt
        // Space and the invite-bootstrapped Space share it (mirrors
        // `build_dm_ingest_fixture_without_space_with_invite`).
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
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        };
        let payload = crate::dm_envelope::MessagePayload {
            body: body.to_vec(),
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
            crate::dm_crypto::encrypt_dm_message(&content_key, &aad, &payload).unwrap();
        let message_cid = ContentId::for_book(
            &storage_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();

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

        let inv_signed = crate::dm_envelope::DmInviteSigned {
            space_id,
            kind: SpaceKind::Dm,
            members,
            inviter: alice,
            content_key,
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

        let entry = DmInboxEntry {
            sender_owner: alice.0,
            cidnotify_packet: Some(cidnotify_packet),
            storage_blob,
            invite_packet: Some(invite_wire),
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
            deposited_at: Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "butler-device".into(),
            },
            deposited_by: "butler-device".into(),
            ingested_by: BTreeSet::new(),
        };
        (entry, space_id, message_cid)
    }

    /// Bob's ingest ctx for the staging path: Alice's signing device pre-cached
    /// (sender-binding resolves) but she is NOT an active friend and Bob has NO
    /// Space, wired to a REAL pending store + a `RecordingSink`.
    fn build_non_friend_staging_ctx() -> (
        ProdDmInboxIngestCtx,
        Arc<crate::pending_dm_invites::PendingDmInvites>,
        Arc<crate::node_event_sink::RecordingSink>,
    ) {
        let alice = OwnerAddr([0xA1; 16]);
        let bob = OwnerAddr([0xB0; 16]);
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub = private_alice.public_identity();
        let alice_identity_pub = alice_pub.to_public_bytes();
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub.address_hash);

        // Alice cached (sender-binding resolves) but NO friendship (→ stage, not
        // auto-accept) and NO Space (→ SpaceNotFound defers the message).
        let mut bob_state = OwnerState::default();
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

        let crdt_state = Arc::new(Mutex::new(bob_state));
        let content_store: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            Arc::new(Arc::clone(&sink_handle));
        let pending = Arc::new(crate::pending_dm_invites::PendingDmInvites::new());

        let prod_ctx = ProdDmInboxIngestCtx {
            device_id: "bob-device-64hex".into(),
            self_owner: bob,
            crdt_state,
            content_store,
            sink,
            pending_dm_invites: Some(Arc::clone(&pending)),
            enrolled: BTreeSet::new(),
            revoked: crate::revoked_device_projection::RevokedDeviceProjection::new(),
            notify_owner_state_dirty: None,
            device_x25519_priv: zeroize::Zeroizing::new([0x33; 32]),
            owner_keytree: Arc::new(
                crate::owner_state_crypto::KeyTree::derive(&[0x44; 32]).expect("keytree"),
            ),
        };
        (prod_ctx, pending, sink_handle)
    }

    /// FIX 1 test (1): a declined co-deposit invite whose SAME message is
    /// re-delivered by the deposit sweeper must NOT re-stage and must emit ZERO
    /// events (kills both the re-prompt AND the every-sweep list-changed churn).
    #[tokio::test]
    async fn declined_co_deposit_same_cid_redelivery_is_suppressed() {
        let (ctx, pending, sink_handle) = build_non_friend_staging_ctx();
        let (entry, space_id, _cid) = build_staging_co_deposit(b"hello there");

        // First delivery: the co-deposited non-friend invite stages; the message
        // defers with SpaceNotFound (Space absent until an accept).
        let err = ctx
            .verify(&entry)
            .await
            .expect_err("staging path defers the message");
        assert!(err.contains("SpaceNotFound"), "got {err}");
        assert_eq!(pending.list().len(), 1, "invite staged on first delivery");
        assert_eq!(
            sink_handle
                .frames()
                .iter()
                .filter(|(n, _)| n == "dm-invite-received")
                .count(),
            1,
            "newly-staged invite prompts once"
        );

        // User declines → (space_id, source_cid) is recorded in the ledger.
        assert!(
            pending.decline(&space_id).is_some(),
            "decline consumes the staged invite"
        );
        assert!(pending.list().is_empty(), "declined invite removed");

        // The deposit sweeper re-delivers the SAME message (same entry → same
        // cid). It must NOT re-stage and must emit nothing new.
        let frames_before = sink_handle.frames().len();
        let err2 = ctx
            .verify(&entry)
            .await
            .expect_err("redelivery still defers");
        assert!(err2.contains("SpaceNotFound"), "got {err2}");
        assert!(
            pending.list().is_empty(),
            "the declined message's redelivery must NOT re-stage"
        );
        assert_eq!(
            sink_handle.frames().len(),
            frames_before,
            "a suppressed redelivery emits neither dm-invite-received nor \
             dm-invite-list-changed (sweep churn killed)"
        );
    }

    /// FIX 1 test (2): after declining, a genuinely NEW message from the same
    /// inviter (different `message_cid`) re-stages and re-emits both events.
    #[tokio::test]
    async fn declined_then_new_message_different_cid_restages() {
        let (ctx, pending, sink_handle) = build_non_friend_staging_ctx();
        let (entry_a, space_id, cid_a) = build_staging_co_deposit(b"first message");

        ctx.verify(&entry_a)
            .await
            .expect_err("first staging defers");
        assert_eq!(pending.list().len(), 1);
        assert!(pending.decline(&space_id).is_some());

        // Different body → different message_cid → NOT in the declined ledger.
        let (entry_b, _space_id_b, cid_b) = build_staging_co_deposit(b"second message");
        assert_ne!(
            cid_a, cid_b,
            "different body yields a different message_cid"
        );

        let frames_before = sink_handle.frames().len();
        ctx.verify(&entry_b)
            .await
            .expect_err("new message also defers (Space still absent)");
        assert_eq!(
            pending.list().len(),
            1,
            "a genuinely new message re-stages after decline"
        );

        let new_frames: Vec<String> = sink_handle.frames()[frames_before..]
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            new_frames.iter().any(|n| n == "dm-invite-received"),
            "a new-cid message re-prompts: {new_frames:?}"
        );
        assert!(
            new_frames.iter().any(|n| n == "dm-invite-list-changed"),
            "a new-cid message fires list-changed: {new_frames:?}"
        );
    }

    /// ZEB-641 (1): staging wiring pin for the PROD-INGEST invite-only site
    /// (`ProdDmInboxIngestCtx::apply_invite_only`, the deposit sweeper's
    /// ZEB-505 route — the second `ApplyInviteOutcome` match in this file).
    /// A non-friend invite-only deposit through the real site wiring lands in
    /// a REAL `PendingDmInvites` and emits both UI events exactly once; an
    /// identical redelivery (the sweeper re-runs every ~7.5 min while the
    /// entry stays pending) emits NOTHING further (keep-first at site level).
    #[tokio::test]
    async fn apply_invite_only_stages_non_friend_deposited_invite() {
        let (ctx, pending, sink_handle) = build_non_friend_staging_ctx();
        // ZEB-505 invite-only deposit: strip the co-deposit down to the bare
        // invite (mirrors the relay fixture's `invite_only_payload`).
        let (mut entry, space_id, _cid) = build_staging_co_deposit(b"invite only");
        entry.cidnotify_packet = None;
        entry.storage_blob = Vec::new();

        ctx.apply_invite_only(&entry)
            .await
            .expect("a staged non-friend invite-only deposit is Ok, not an error");

        // Staging writes nothing to owner-state (Space appears only on accept).
        {
            let st = ctx.crdt_state.lock().await;
            assert!(
                !st.spaces.contains_key(&space_id),
                "a staged (non-friend) invite must NOT write the DM Space"
            );
        }
        let staged = pending.list();
        assert_eq!(staged.len(), 1, "exactly one invite staged");
        assert_eq!(staged[0].signed.space_id, space_id);
        assert_eq!(
            staged[0].source_cid, None,
            "an invite-only deposit has no notifying message — apply_invite \
             leaves source_cid None and this site never overwrites it"
        );
        assert!(
            !staged[0].refresh_owner_device_cache,
            "deposit-recover route is never cache-refresh entitled (ZEB-483)"
        );

        let frames = sink_handle.frames();
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

        // Second identical delivery (sweep re-run): keep-first — no re-stage,
        // no re-emit.
        let frames_before = sink_handle.frames().len();
        ctx.apply_invite_only(&entry)
            .await
            .expect("an already-pending redelivery is still Ok");
        assert_eq!(
            pending.list().len(),
            1,
            "keep-first: the redelivery must not re-stage or duplicate"
        );
        assert_eq!(
            sink_handle.frames().len(),
            frames_before,
            "an already-pending redelivery emits neither dm-invite-received \
             nor dm-invite-list-changed"
        );
    }
}

/// ZEB-473 Task 9: receive-side test fixture for the inbound-tunnel-DM ingest,
/// reused by the end-to-end loopback test in `tunnel_task`'s test module.
/// `pub(crate)` + `cfg(test)` so it crosses the module boundary without leaking
/// into production builds.
#[cfg(test)]
pub(crate) mod test_fixture {
    use super::*;
    use crate::content_store::{ContentStore, InMemoryStub};
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{DmContentKey, Space, SpaceKind};
    use std::sync::Arc as StdArc;

    /// The receive-side handles `ingest_dm_packet` consumes plus the real signed
    /// packet a sender (Alice) carries over the tunnel.
    pub(crate) struct DmIngestFixture {
        pub crdt_state: StdArc<Mutex<OwnerState>>,
        pub content_store: StdArc<dyn ContentStore>,
        pub sink_handle: StdArc<crate::node_event_sink::RecordingSink>,
        pub sink: StdArc<dyn crate::node_event_sink::NodeEventSink>,
        pub bob_device_id: String,
        pub packet: Vec<u8>,
        pub space_id: SpaceId,
        pub message_cid: ContentId,
        pub alice: OwnerAddr,
        /// The receiver (self) OwnerAddr — threaded into `ingest_dm_packet` as
        /// `self_owner` (ZEB-482, needed by the invite-ingest sanity gates).
        pub bob: OwnerAddr,
    }

    /// Bob (self) shares a DM Space with Alice, has Alice's signing device
    /// cached, and the encrypted DM blob is CAS-put. Mirrors `dm_outbox`'s
    /// `build_cidnotify_fixture` (kept local because that one is private to
    /// `dm_outbox`).
    pub(crate) async fn build_dm_ingest_fixture(body: &[u8]) -> DmIngestFixture {
        let alice = OwnerAddr([0xA1; 16]);
        let bob = OwnerAddr([0xB0; 16]);
        let space_id = SpaceId([0x5A; 16]);
        let content_key = DmContentKey::new([0x42u8; 32]);

        let mut state = OwnerState::default();

        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub_id = private_alice.public_identity();
        let alice_identity_pub = alice_pub_id.to_public_bytes();
        let alice_device_hash =
            crate::owner_state_types::DeviceIdentityHash(alice_pub_id.address_hash);
        state.apply_owner_device_update(
            alice,
            vec![alice_device_hash],
            vec![Some(alice_identity_pub)],
            vec![],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        );

        let mut members = [alice, bob];
        members.sort();
        let space = Space {
            id: space_id,
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "Alice".into(),
            transport: None,
            members: members.to_vec(),
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        };
        assert!(matches!(
            state.apply_space_with_canonicalization(space.clone()),
            crate::owner_state_crdt::ApplyOutcome::Inserted
        ));

        let payload = crate::dm_envelope::MessagePayload {
            body: body.to_vec(),
            mime_type: "text/plain".into(),
            sender: alice,
            sent_at: Hlc {
                wall_ms: 150,
                logical: 0,
                device_id: "alice-dev".into(),
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

        // Sign with Alice's identity (the SAME Ed25519 key whose pubkey is the
        // second half of the cached `identity_pub`), then frame to wire bytes.
        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id,
            message_cid,
            sender_owner_addr: alice,
            sender_devices: vec![alice_device_hash],
            signing_device_hash: alice_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);
        let dm_packet = crate::dm_envelope::DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        };
        let packet = crate::dm_envelope::encode_packet(&dm_packet).unwrap();

        let sink_handle = crate::node_event_sink::RecordingSink::new();
        let sink: StdArc<dyn crate::node_event_sink::NodeEventSink> =
            StdArc::new(StdArc::clone(&sink_handle));

        DmIngestFixture {
            crdt_state: StdArc::new(Mutex::new(state)),
            content_store: StdArc::new(cas),
            sink_handle,
            sink,
            bob_device_id: "bob-device-64hex".into(),
            packet,
            space_id,
            message_cid,
            alice,
            bob,
        }
    }
}
