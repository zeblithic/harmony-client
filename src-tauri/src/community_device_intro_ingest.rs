//! ZEB-495 (ZEB-340 Part 2) Unit 5c: the `community-device-intro` relay sweeper
//! — every already-enrolled device of the owner drains deposited
//! `DeviceAnnounce` intros and `insert_local_event`s each into the matching
//! community engine, then acks its relay into the entry's grow-only
//! `relayed_by` set, then GCs entries whose `relayed_by` already covered the
//! enrolled device set when the sweep began (one-sweep deferral so the covering
//! state replicates first — see `covered_at_start`) or whose TTL expired.
//!
//! This is the Option A relay (design spec §"Unit 5"): the second device cannot
//! self-publish its own introduction (`verify_publisher_sig` rejects a key not
//! yet enrolled), so an enrolled sibling relays it. Once any sibling's
//! state-root publish carries the `DeviceAnnounce`, every peer accepts it via
//! `enrolled_key_from_cert` and the second device's key lands in the owner's
//! `MemberState`; thereafter the second device publishes directly.
//!
//! Mirrors `dm_inbox_ingest` structurally (the `*IngestCtx` trait, the
//! `covered_at_start` GC, the `*_nudge_on_applied` hook, the debounced
//! `run_*_sweeper`, the `Prod*Ctx`) — but the per-entry work is far simpler:
//! decode the `SignedMembershipEvent` and call `insert_local_event` (idempotent
//! — `AlreadyKnown` on a dup). There is no CAS / decrypt: the announce IS the
//! payload. If this device is not a member of the entry's community (no engine
//! spawned), the entry is LEFT PENDING for retry — never relayed, never GC'd.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use crate::community_device_intro_crdt::{CommunityDeviceIntroDoc, CommunityDeviceIntroEntry};

/// TTL safety valve for a deposited intro (30 days, mirrors the dm-inbox
/// `INBOX_TTL_MS`): a never-coverable intro (e.g. a community the owner left, so
/// no enrolled device ever relays it) is eventually GC'd rather than pinned
/// forever.
pub const INTRO_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// Debounce window between an `on_applied` nudge and the sweep it triggers —
/// coalesces a merge burst into one sweep (mirrors `INGEST_SWEEP_DEBOUNCE_MS`).
pub const RELAY_SWEEP_DEBOUNCE_MS: u64 = 250;

/// Upper bound on one inbound peer-fed `community-device-intro` sample before it
/// is copied into an owned Vec (the Zenoh adapter's attacker-driven-allocation
/// guard). Each entry is a signed `DeviceAnnounce` + Master `EnrollmentCert`
/// (a few KB); a fleet replicating many `(community, device)` intros stays well
/// under 1 MiB. Generous bound — oversize samples are dropped with a warn.
pub const COMMUNITY_DEVICE_INTRO_DATASET_MAX_BYTES: usize = 1024 * 1024;

/// Injectable context for [`ingest_pending`]: this device's id, the relay
/// (decode + `insert_local_event` into the community engine), the
/// enrolled-device snapshot for the GC coverage check, and the TTL clock.
/// Production implements this over the `CommunitySyncRegistry`; tests implement
/// it with a probe.
#[async_trait]
pub trait CommunityDeviceIntroIngestCtx: Send + Sync {
    /// This device's SP1 device id (64-hex of the device ed25519 verify key) —
    /// the SAME string form `relayed_by` and [`Self::enrolled_device_ids`] use.
    fn self_device_id(&self) -> String;

    /// Relay one deposited intro into its community engine.
    ///
    /// Production: `registry.engine_arc(&community_id)` —
    ///   * `None` (this device is not a member / no engine spawned for this
    ///     community) ⇒ `Ok(false)` — LEAVE PENDING: not an error, just not
    ///     relayable here yet (another enrolled device, or this device after a
    ///     later join, will relay it). It is NOT marked `relayed_by` and NOT
    ///     GC'd by coverage.
    ///   * `Some(engine)` ⇒ decode the `SignedMembershipEvent` from
    ///     `signed_event` and `engine.insert_local_event(signed)`:
    ///       - `Inserted` / `AlreadyKnown` ⇒ `Ok(true)` — relayed (idempotent);
    ///       - `Rejected(_)` ⇒ `Err(..)` — a bad/forged announce; LEFT PENDING
    ///         (it will never verify, so the TTL GC is its safety valve).
    ///       - `LocalInsertError` / decode failure ⇒ `Err(..)` — LEFT PENDING
    ///         for retry.
    ///
    /// `Ok(true)` = this device relayed it (add self to `relayed_by`).
    /// `Ok(false)` = not relayable here (leave pending, no ack).
    /// `Err(..)` = transient/permanent failure (leave pending; TTL GC).
    async fn relay(&self, entry: &CommunityDeviceIntroEntry) -> Result<bool, String>;

    /// Currently-enrolled device ids for the GC coverage check, in the SAME
    /// 64-hex string form as `relayed_by`. Enrolled — NOT online — devices: a
    /// revoked device's absence must not pin entries forever (revocation
    /// follows the existing device-revocation path). Mirrors
    /// `DmInboxIngestCtx::enrolled_device_ids`.
    async fn enrolled_device_ids(&self) -> BTreeSet<String>;

    /// Wall-clock now in epoch-ms for the TTL check.
    fn now_ms(&self) -> u64;
}

/// One relay + GC sweep over the doc (call under the doc lock). Returns `true`
/// when the doc was mutated (relayed_by growth or GC removal) — the caller must
/// then `notify_dirty()` so the engine replicates our relay ack / removal to
/// siblings.
pub async fn ingest_pending(
    doc: &mut CommunityDeviceIntroDoc,
    ctx: &dyn CommunityDeviceIntroIngestCtx,
) -> bool {
    let self_id = ctx.self_device_id();
    let mut changed = false;

    // Coverage snapshot at sweep START (publish-before-GC, mirrors
    // dm_inbox_ingest's `covered_at_start`): coverage-GC below removes only
    // entries that were ALREADY covered when the sweep began, so the covering
    // `relayed_by ⊇ enrolled` state is published + replicates BEFORE any
    // replica destroys the entry. Coverage is disabled when the enrolled
    // snapshot is empty (`relayed_by ⊇ ∅` is vacuously true — an empty provider
    // snapshot must not wipe the doc).
    let enrolled = ctx.enrolled_device_ids().await;
    let covered_at_start: BTreeSet<String> = doc
        .entries
        .iter()
        .filter(|(_, e)| !enrolled.is_empty() && enrolled.iter().all(|d| e.relayed_by.contains(d)))
        .map(|(k, _)| k.clone())
        .collect();

    for (key, entry) in doc.entries.iter_mut() {
        if entry.relayed_by.contains(&self_id) {
            continue;
        }
        match ctx.relay(entry).await {
            Ok(true) => {
                // Relayed (or idempotently already-known) into the engine.
                entry.relayed_by.insert(self_id.clone());
                changed = true;
            }
            Ok(false) => {
                // Not relayable on this device (not a member of this
                // community / no engine). Leave pending — another enrolled
                // device relays it, or this device does after a later join.
                tracing::trace!(
                    key = %key,
                    "ZEB-495 relay: not a member of this community; intro left pending"
                );
            }
            Err(e) => {
                // Transient (engine busy) or permanent (forged announce
                // rejected by verify). Leave pending — TTL GC is the safety
                // valve for an announce that will never relay.
                tracing::warn!(
                    error = %e,
                    key = %key,
                    "ZEB-495 relay: insert_local_event failed; intro left pending for retry"
                );
            }
        }
    }

    // GC pass — an entry is removable when EITHER:
    //   (a) coverage: every currently-enrolled device id appeared in
    //       `relayed_by` at sweep START (the `covered_at_start` snapshot), or
    //   (b) TTL: `deposited_at.wall_ms + INTRO_TTL_MS < now` (strict).
    //
    // Churn-tolerant model (resurrection-by-merge), identical to dm_inbox: a
    // sibling that GC'd later can re-merge an old doc and resurrect a removed
    // entry; both GC criteria are deterministic functions of
    // (relayed_by, now), and `relayed_by` is grow-only with union merge, so
    // every replica reaches the same removable verdict and the next sweep
    // removes it again. Once every replica has GC'd, no copy remains to
    // resurrect — the fleet converges WITHOUT tombstones. Re-relay after a lost
    // ack is safe: `insert_local_event` is idempotent (`AlreadyKnown`).
    let now_ms = ctx.now_ms();
    let before = doc.entries.len();
    doc.entries.retain(|key, e| {
        let ttl_expired = e.deposited_at.wall_ms.saturating_add(INTRO_TTL_MS) < now_ms;
        !(ttl_expired || covered_at_start.contains(key))
    });
    changed |= doc.entries.len() != before;

    changed
}

/// Adapter for `FleetSyncConfig::on_applied`: nudges the relay sweeper.
/// `try_send` on a capacity-1 channel — the nudge is a level trigger, so a full
/// buffer (a sweep is already scheduled) makes dropping extras correct.
pub fn relay_nudge_on_applied(nudge_tx: mpsc::Sender<()>) -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || {
        let _ = nudge_tx.try_send(());
    })
}

/// The relay-sweeper task: one startup sweep (intros deposited while this
/// device was offline / by a sibling), then one debounced sweep per
/// `on_applied` nudge burst. Exits when every nudge sender is dropped (engine
/// shutdown). Mirrors `run_dm_inbox_ingest_sweeper`.
pub async fn run_community_device_intro_sweeper(
    doc: Arc<Mutex<CommunityDeviceIntroDoc>>,
    ctx: Arc<dyn CommunityDeviceIntroIngestCtx>,
    mut nudge_rx: mpsc::Receiver<()>,
    notify_dirty: Arc<dyn Fn() + Send + Sync>,
    debounce: Duration,
) {
    // Startup sweep: relay intros deposited while this device was offline (they
    // arrive via the engine's initial fan-in BEFORE on_applied wiring observes
    // them, and via the persisted doc).
    sweep_once(&doc, ctx.as_ref(), notify_dirty.as_ref()).await;

    while nudge_rx.recv().await.is_some() {
        // Debounce: let the rest of a merge burst land, then drain extras so
        // the burst coalesces into one sweep.
        tokio::time::sleep(debounce).await;
        while nudge_rx.try_recv().is_ok() {}
        sweep_once(&doc, ctx.as_ref(), notify_dirty.as_ref()).await;
    }
    // recv() == None: every nudge sender dropped (engine shutdown).
}

/// One locked sweep; `notify_dirty` only when the doc actually changed so idle
/// sweeps don't schedule no-op publishes.
async fn sweep_once(
    doc: &Arc<Mutex<CommunityDeviceIntroDoc>>,
    ctx: &dyn CommunityDeviceIntroIngestCtx,
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

/// Production [`CommunityDeviceIntroIngestCtx`] over real `start_node` handles.
///
/// * relay = `registry.engine_arc(&community_id)`; `None` ⇒ `Ok(false)` (not a
///   member here); else decode the signed `DeviceAnnounce` and
///   `insert_local_event` (idempotent — `AlreadyKnown` on dup);
/// * `enrolled` = a start_node-time snapshot of the `harmony_owner`
///   `OwnerState.enrollments` certs mapped through
///   `hex::encode(cert.device_pubkeys.classical.ed25519_verify)` (the SP1
///   64-hex device-id form). Enrollment changes require a node restart, so the
///   snapshot tracks the set for the engine's lifetime (mirrors
///   `ProdDmInboxIngestCtx`).
pub struct ProdCommunityDeviceIntroIngestCtx {
    /// This device's SP1 device id (64-hex of the device ed25519 verify key).
    pub device_id: String,
    /// The community-sync registry (engine lookup by community id).
    pub registry: Arc<crate::community_state_sync::CommunitySyncRegistry>,
    /// Enrolled device ids (64-hex), snapshotted at start_node.
    pub enrolled: BTreeSet<String>,
}

#[async_trait]
impl CommunityDeviceIntroIngestCtx for ProdCommunityDeviceIntroIngestCtx {
    fn self_device_id(&self) -> String {
        self.device_id.clone()
    }

    async fn relay(&self, entry: &CommunityDeviceIntroEntry) -> Result<bool, String> {
        // No engine for this community ⇒ this device isn't a member (or the
        // engine isn't spawned) — leave pending, not an error.
        let Some(engine) = self.registry.engine_arc(&entry.community_id).await else {
            return Ok(false);
        };
        let signed: crate::community_membership::SignedMembershipEvent =
            crate::owner_state_crypto::canonical_cbor_decode(&entry.signed_event)
                .map_err(|e| format!("decode DeviceAnnounce: {e:?}"))?;
        // Defense-in-depth (ZEB-495, Greptile): this dataset's contract is
        // EXCLUSIVELY `DeviceAnnounce` events — the self-introduce deposit is the
        // only writer, and it only ever deposits that kind. Assert it before
        // relaying so a non-`DeviceAnnounce` event smuggled in by a bug, a
        // misbehaving enrolled sibling, or a future refactor that reuses this
        // relay path can never ride into the community engine (where
        // `verify_event` might otherwise accept an enrolled-device-authored
        // `Leave`/`Ban`). Unrelayable by construction ⇒ leave pending (Err),
        // TTL-reaped — exactly like the forged-announce `Rejected` path below.
        if !matches!(
            signed.kind,
            crate::community_membership::MembershipEventKind::DeviceAnnounce
        ) {
            return Err(format!(
                "non-DeviceAnnounce event in community-device-intro dataset: {:?}",
                signed.kind
            ));
        }
        match engine.insert_local_event(signed).await {
            // Inserted OR AlreadyKnown ⇒ relayed (idempotent).
            Ok(crate::community_state_crdt::InsertOutcome::Inserted)
            | Ok(crate::community_state_crdt::InsertOutcome::AlreadyKnown) => Ok(true),
            Ok(crate::community_state_crdt::InsertOutcome::Rejected(reason)) => {
                // A forged / unverifiable announce — it will never relay.
                // Leave pending (Err): the TTL GC is the safety valve.
                Err(format!("insert_local_event rejected: {reason:?}"))
            }
            Err(e) => Err(format!("insert_local_event: {e:?}")),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{Hlc, SpaceId};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    const SELF_ID: &str = "self-device-64hex";
    const SIBLING_ID: &str = "sibling-device-64hex";

    fn hlc(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: "device2".into(),
        }
    }

    fn community(b: u8) -> SpaceId {
        SpaceId([b; 16])
    }

    fn make_entry(
        community_id: SpaceId,
        device_hex: &str,
        deposited_ms: u64,
        relayed: &[&str],
    ) -> (String, CommunityDeviceIntroEntry) {
        let entry = CommunityDeviceIntroEntry {
            signed_event: vec![1, 2, 3, 4],
            community_id,
            deposited_at: hlc(deposited_ms),
            relayed_by: relayed.iter().map(|s| s.to_string()).collect(),
        };
        (
            CommunityDeviceIntroDoc::key(&community_id, device_hex),
            entry,
        )
    }

    /// Probe ctx: records relay calls + the set of communities this device is a
    /// "member" of (engine present). Communities NOT in `member_of` return
    /// `Ok(false)` (leave-pending); `relay_fail` forces an `Err`.
    struct ProbeCtx {
        enrolled: BTreeSet<String>,
        now_ms: u64,
        member_of: BTreeSet<SpaceId>,
        relay_fail: bool,
        relayed_calls: StdMutex<Vec<SpaceId>>,
    }

    impl ProbeCtx {
        fn new() -> Self {
            Self {
                // Self + one sibling so a single-device relay does NOT complete
                // coverage (coverage-GC tests adjust this explicitly).
                enrolled: [SELF_ID.to_string(), SIBLING_ID.to_string()].into(),
                now_ms: 1_000_000,
                member_of: [community(1)].into(),
                relay_fail: false,
                relayed_calls: StdMutex::new(Vec::new()),
            }
        }

        fn relayed_calls(&self) -> Vec<SpaceId> {
            self.relayed_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommunityDeviceIntroIngestCtx for ProbeCtx {
        fn self_device_id(&self) -> String {
            SELF_ID.to_string()
        }

        async fn relay(&self, entry: &CommunityDeviceIntroEntry) -> Result<bool, String> {
            self.relayed_calls.lock().unwrap().push(entry.community_id);
            if !self.member_of.contains(&entry.community_id) {
                return Ok(false); // not a member of this community
            }
            if self.relay_fail {
                return Err("simulated insert_local_event failure".into());
            }
            Ok(true)
        }

        async fn enrolled_device_ids(&self) -> BTreeSet<String> {
            self.enrolled.clone()
        }

        fn now_ms(&self) -> u64 {
            self.now_ms
        }
    }

    #[tokio::test]
    async fn relays_a_pending_entry_and_marks_relayed_by() {
        let (key, entry) = make_entry(community(1), "device2-64hex", 500, &[]);
        let mut doc = CommunityDeviceIntroDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "relaying mutated the doc (relayed_by growth)");
        assert_eq!(ctx.relayed_calls(), vec![community(1)]);
        assert!(
            doc.entries[&key].relayed_by.contains(SELF_ID),
            "self added to relayed_by"
        );
        // Coverage incomplete (sibling hasn't relayed) → retained.
        assert!(doc.entries.contains_key(&key));
    }

    #[tokio::test]
    async fn leaves_pending_when_not_a_member_of_the_community() {
        // community(2) is NOT in member_of → relay returns Ok(false).
        let (key, entry) = make_entry(community(2), "device2-64hex", 500, &[]);
        let mut doc = CommunityDeviceIntroDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(!changed, "non-member relay is a no-op (no ack)");
        assert_eq!(
            ctx.relayed_calls(),
            vec![community(2)],
            "relay was attempted"
        );
        assert!(
            doc.entries[&key].relayed_by.is_empty(),
            "not marked relayed_by"
        );
        assert!(doc.entries.contains_key(&key), "left pending for retry");
    }

    #[tokio::test]
    async fn failed_relay_leaves_entry_pending_for_retry() {
        let (key, entry) = make_entry(community(1), "device2-64hex", 500, &[]);
        let mut doc = CommunityDeviceIntroDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx {
            relay_fail: true,
            ..ProbeCtx::new()
        };
        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(!changed);
        assert!(doc.entries[&key].relayed_by.is_empty());
        assert!(doc.entries.contains_key(&key));

        // Once the failure clears, the SAME entry relays normally.
        let ctx = ProbeCtx::new();
        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed);
        assert!(doc.entries[&key].relayed_by.contains(SELF_ID));
    }

    #[tokio::test]
    async fn already_relayed_entry_is_idempotent_noop() {
        let (key, entry) = make_entry(community(1), "device2-64hex", 500, &[SELF_ID]);
        let mut doc = CommunityDeviceIntroDoc::default();
        doc.entries.insert(key.clone(), entry);
        let ctx = ProbeCtx::new();

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(!changed, "already-relayed entry is a no-op");
        assert!(ctx.relayed_calls().is_empty(), "no relay attempt");
        assert!(doc.entries.contains_key(&key), "retained (no coverage)");
    }

    #[tokio::test]
    async fn gc_removes_on_coverage_or_ttl() {
        let now_ms: u64 = INTRO_TTL_MS + 1_000_000;
        let ctx = ProbeCtx {
            now_ms,
            ..ProbeCtx::new()
        };

        // (a) coverage-complete (self + sibling both relayed) → removed.
        let (key_cov, entry_cov) = make_entry(
            community(1),
            "dev-cov",
            now_ms - 1_000,
            &[SELF_ID, SIBLING_ID],
        );
        // (b) TTL-expired (strictly past) → removed even though uncovered.
        let (key_ttl, entry_ttl) = make_entry(community(1), "dev-ttl", 500, &[SELF_ID]);
        // (c) fresh + uncovered → retained.
        let (key_fresh, entry_fresh) =
            make_entry(community(1), "dev-fresh", now_ms - 1_000, &[SELF_ID]);
        // (d) TTL boundary (deposited_at + TTL == now) is NOT yet expired
        //     (strict `<`) → retained.
        let (key_edge, entry_edge) =
            make_entry(community(1), "dev-edge", now_ms - INTRO_TTL_MS, &[SELF_ID]);

        let mut doc = CommunityDeviceIntroDoc::default();
        doc.entries.insert(key_cov.clone(), entry_cov.clone());
        doc.entries.insert(key_ttl.clone(), entry_ttl);
        doc.entries.insert(key_fresh.clone(), entry_fresh);
        doc.entries.insert(key_edge.clone(), entry_edge);

        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed, "GC removals are doc mutations");
        assert!(
            !doc.entries.contains_key(&key_cov),
            "relayed_by ⊇ enrolled set → coverage GC"
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
            ctx.relayed_calls().is_empty(),
            "all entries already carried self in relayed_by — no re-relay"
        );

        // Empty-enrolled guard: relayed_by ⊇ ∅ is vacuously true, so an empty
        // provider snapshot must NOT wipe the doc (TTL still applies).
        let ctx_empty = ProbeCtx {
            enrolled: BTreeSet::new(),
            now_ms,
            ..ProbeCtx::new()
        };
        let (key_g, entry_g) = make_entry(community(1), "dev-g", now_ms - 1_000, &[SELF_ID]);
        let mut doc_g = CommunityDeviceIntroDoc::default();
        doc_g.entries.insert(key_g.clone(), entry_g);
        let changed = ingest_pending(&mut doc_g, &ctx_empty).await;
        assert!(!changed);
        assert!(
            doc_g.entries.contains_key(&key_g),
            "empty enrolled set must not coverage-GC anything"
        );

        // Resurrection-by-merge converges: a stale sibling re-merges the
        // removed entry, the next sweep re-GCs it (self already in relayed_by).
        let mut stale = CommunityDeviceIntroDoc::default();
        stale.entries.insert(key_cov.clone(), entry_cov);
        let out = doc.merge_from(stale);
        assert!(out.changed, "resurrection is a visible merge change");
        assert!(doc.entries.contains_key(&key_cov));
        let changed = ingest_pending(&mut doc, &ctx).await;
        assert!(changed);
        assert!(
            !doc.entries.contains_key(&key_cov),
            "re-GC after resurrection-by-merge converges"
        );
    }

    /// Poll `cond` until true (paused-clock sleeps auto-advance → logical-time
    /// waiting); panics after a generous cap so a broken sweeper fails fast.
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
    async fn startup_sweep_relays_preexisting_entries_and_nudge_drives_followup() {
        let (key, entry) = make_entry(community(1), "device2-64hex", 500, &[]);
        let mut initial = CommunityDeviceIntroDoc::default();
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

        let handle = tokio::spawn(run_community_device_intro_sweeper(
            Arc::clone(&doc),
            Arc::clone(&ctx) as Arc<dyn CommunityDeviceIntroIngestCtx>,
            nudge_rx,
            notify_dirty,
            Duration::from_millis(RELAY_SWEEP_DEBOUNCE_MS),
        ));

        // Startup sweep relays the pre-existing entry with NO nudge.
        wait_until(|| !ctx.relayed_calls().is_empty()).await;
        assert!(doc.lock().await.entries[&key].relayed_by.contains(SELF_ID));
        assert!(
            dirty.load(Ordering::SeqCst) >= 1,
            "a mutating sweep must notify_dirty so the relay ack replicates"
        );

        // A nudge triggers a debounced follow-up sweep for a newly-merged entry.
        let (key2, entry2) = make_entry(community(1), "device3-64hex", 600, &[]);
        doc.lock().await.entries.insert(key2.clone(), entry2);
        nudge_tx.send(()).await.expect("sweeper alive");
        wait_until(|| {
            doc.try_lock()
                .map(|g| {
                    g.entries
                        .get(&key2)
                        .map(|e| e.relayed_by.contains(SELF_ID))
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        })
        .await;

        // Dropping the last nudge sender shuts the task down cleanly.
        drop(nudge_tx);
        tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("sweeper must exit when nudge senders drop")
            .expect("sweeper must not panic");
    }

    #[tokio::test]
    async fn nudge_on_applied_is_nonblocking_level_trigger() {
        let (tx, mut rx) = mpsc::channel(1);
        let nudge = relay_nudge_on_applied(tx);
        nudge();
        nudge(); // buffer full — dropped, must not block/panic
        assert_eq!(rx.recv().await, Some(()));
        assert!(rx.try_recv().is_err(), "extras coalesced into one nudge");
    }
}
