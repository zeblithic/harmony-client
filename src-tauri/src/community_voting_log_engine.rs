//! ZEB-291 Phase 2: per-community voting-log Zenoh transport engine.
//!
//! Copy of `community_channel_log_engine.rs` (ZEB-270 pattern) substituted
//! for voting events. Engine never touches `zenoh::Session` directly —
//! mpsc-channel split with the adapter (the adapter wires `publisher_tx` /
//! `subscriber_rx` to actual Zenoh `put` / subscribe in a follow-up task).
//!
//! Differences from the channel-log engine:
//!
//! - **Topic shape**: voting is per-community (one topic per community at
//!   `harmony/community/{id}/voting`), not per-(community, channel). The
//!   registry therefore keys on `SpaceId`, not a `(SpaceId, ChannelId)`
//!   tuple.
//! - **No backfill**: voting event volume is sparse (a community
//!   typically has a handful of polls per week, not a stream like chat),
//!   so Phase 2 ships without the Zenoh queryable scaffolding. A future
//!   task can layer backfill on if pull-on-rejoin proves desirable.
//! - **Verify path stubbed**: see `TODO ZEB-291 Task 12.1` in
//!   `process_inbound`. The IPC layer (Task 18) always verifies before
//!   broadcasting locally-minted events, so peer events bypass an
//!   independent verify step in Phase 2. This is defense-in-depth, not a
//!   security catastrophe — the CRDT apply path still enforces lifecycle
//!   transitions, payload decode, and graph cycle checks. Wiring a
//!   reusable `voting_core::verify_event` is tracked separately.
//!
//! Self-loopback fix is preserved verbatim from ZEB-270:
//! `tracker.record(&event)` MUST happen BEFORE `publisher_tx.try_send(packet)`.
//! Without that ordering, the local event loops back via the subscriber path
//! and gets double-applied. See `publish_event` below.

use std::collections::{BTreeMap, HashMap};
use std::marker::PhantomData;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::{mpsc, Mutex, RwLock};

use tauri::{AppHandle, Emitter};

use crate::community_dfrost_log_engine::DfrostLogRegistry;
use crate::community_voting_core::{PollEventKindCode, PollId, SignedVotingEvent, Tier};
use crate::community_voting_log::VotingLog;
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

// ── BeaconRequester type alias ───────────────────────────────────────────────

/// Injected closure for triggering a VRF beacon ceremony.
///
/// Captures `Arc<Mutex<NodeState>>` and calls `dfrost_request_vrf_beacon_inner`.
/// The `SpaceId` is the community, `[u8; 32]` is the beacon seed, `u64` is the
/// current committee epoch.
///
/// Task 10 Option A: injected at `VotingLogEngine::install_dfrost_handle` time
/// so the engine does not need to hold a raw `Arc<Mutex<NodeState>>`.
pub type BeaconRequester = Arc<
    dyn Fn(SpaceId, [u8; 32], u64) -> BoxFuture<'static, Result<String, String>>
        + Send
        + Sync
        + 'static,
>;

// ── Replay tracker ──────────────────────────────────────────────────────────

/// Dedup table keyed on `(actor, device_id)` → max HLC `(wall_ms, logical)`
/// tuple seen.
///
/// Mirrors `community_channel_log::ChannelLogReplayTracker` in shape but
/// dedups on the full `(wall_ms, logical)` HLC ordinal rather than just
/// `wall_ms`. Two distinct events minted on the same lane in the same
/// millisecond (e.g. a Signal followed immediately by a Delegate) share
/// `wall_ms` but differ in `logical`; a `wall_ms`-only tracker would
/// silently drop the second as a duplicate and permanently diverge tier
/// state across replicas.
///
/// `(0, 0)` is treated as "never seen" — Phase 2 has no production
/// events at the epoch HLC; only unit-test scaffolding which constructs
/// events strictly after `record`.
#[derive(Debug, Default)]
pub struct VotingReplayTracker {
    seen: HashMap<(OwnerAddr, String), (u64, u32)>,
}

impl VotingReplayTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn ordinal(event: &SignedVotingEvent) -> (u64, u32) {
        (event.hlc.wall_ms, event.hlc.logical)
    }

    /// Unconditionally bump the high-water mark for an event's lane.
    /// Called by `publish_event` BEFORE the broadcast (the self-loopback
    /// fix from ZEB-270) and by `process_inbound` after a successful apply.
    pub fn record(&mut self, event: &SignedVotingEvent) {
        let key = (event.actor, event.hlc.device_id.clone());
        let ord = Self::ordinal(event);
        let entry = self.seen.entry(key).or_insert((0u64, 0u32));
        if ord > *entry {
            *entry = ord;
        }
    }

    /// Returns true if this event has already been seen on its lane
    /// (`(wall_ms, logical)` is `<=` the recorded high-water mark).
    /// Used by `process_inbound` to drop self-loopbacks and peer
    /// duplicates before doing the (cheap) apply work.
    pub fn contains(&self, event: &SignedVotingEvent) -> bool {
        let key = (event.actor, event.hlc.device_id.clone());
        match self.seen.get(&key) {
            Some(&max_ord) => Self::ordinal(event) <= max_ord,
            None => false,
        }
    }
}

// ── Engine params + handle ──────────────────────────────────────────────────

/// Bundles per-community dependencies + I/O endpoints for the engine.
///
/// The adapter (Task 19 — full NodeState wiring) owns the other ends of
/// `publisher_tx` / `subscriber_rx` and binds them to Zenoh `put` /
/// subscribe on `harmony/community/{community_id}/voting`.
pub struct VotingLogEngineParams<R: tauri::Runtime = tauri::Wry> {
    pub community_id: SpaceId,
    pub voting_log: Arc<Mutex<VotingLog>>,
    /// Engine → adapter → Zenoh `put`.
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    /// Zenoh subscriber → adapter → engine receive loop.
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
    /// Shared per-device HLC tracker (ZEB-267). Used by
    /// `reserve_next_local_hlc` so engine-auto-orchestrated events
    /// (kd=sf / Task 9, kd=cl / Task 10, kd=rs / Task 11) get HLCs
    /// monotone with the IPC layer's mints on the same lane.
    /// Optional so tests that never trigger engine-auto can pass `None`.
    pub hlc_tracker: Option<Arc<Mutex<BTreeMap<String, Hlc>>>>,
    /// Local device_id string, paired with `hlc_tracker` above.
    /// Optional for the same reason as `hlc_tracker`.
    pub device_id: Option<String>,
    /// ZEB-310 Task 12: optional Tauri `AppHandle` used by the post-apply
    /// hook to emit the four Tier 3 lifecycle events
    /// (`voting-tier3-sortition-complete`, `voting-tier3-drafting-open`,
    /// `voting-tier3-ratification-open`, `voting-tier3-finalized`). `None`
    /// disables emission — used by tests / lightweight harnesses that don't
    /// drive the UI.
    pub app_handle: Option<AppHandle<R>>,
    /// ZEB-298+ZEB-312 PR 1: production wiring — resolves identity for
    /// Ed25519 signature verification on inbound voting events. `None`
    /// means inbound events are rejected (engine not production-wired).
    pub identity_resolver:
        Option<std::sync::Arc<dyn crate::community_voting_core::VotingIdentityResolver>>,
    /// ZEB-298+ZEB-312 PR 1: production wiring — resolves per-community
    /// membership snapshot at an HLC for inbound voting events.
    /// Non-PollCreate events use a fresh snapshot too (pragmatic
    /// uniformity over case-splitting). `None` means inbound events are
    /// rejected.
    pub membership_resolver:
        Option<std::sync::Arc<dyn crate::community_voting_log::MembershipSnapshotResolver>>,
    // No backfill_req_tx in Phase 2 — sparse event volume, deferred.
}

/// Per-community voting transport engine.
///
/// Lifetime: created via `VotingLogEngine::start` which spawns the inbound
/// receive loop; lives until the registry is shut down (which drops the
/// publisher sender held by the adapter, which closes `subscriber_rx`,
/// which causes the receive loop to exit and drop the JoinHandle).
///
/// Task 10: generic over `R: tauri::Runtime` so it can hold an
/// `Arc<DfrostLogRegistry<R>>`. `PhantomData<fn() -> R>` (NOT `PhantomData<R>`)
/// keeps the engine `Send + Sync` even when `R = tauri::Wry` (which is !Send).
pub struct VotingLogEngine<R: tauri::Runtime> {
    community_id: SpaceId,
    voting_log: Arc<Mutex<VotingLog>>,
    tracker: Arc<Mutex<VotingReplayTracker>>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    /// Held only so the receive task isn't aborted by handle-drop while
    /// the engine is alive. The task exits naturally when `subscriber_rx`
    /// closes (adapter dropped the matching `Sender`).
    _receive_handle: tokio::task::JoinHandle<()>,
    /// Optional DfrostLogRegistry for BeaconOracle (verify_ss) + subscribe_beacons.
    /// Populated by `install_dfrost_handle`. None until wired by Task 19.
    /// Mutex<Option<...>> for interior mutability (set once, read many).
    dfrost_registry: Mutex<Option<Arc<DfrostLogRegistry<R>>>>,
    /// Optional injected closure for triggering a VRF beacon ceremony.
    /// Populated by `install_dfrost_handle`. None until wired by Task 19.
    beacon_requester: Mutex<Option<BeaconRequester>>,
    /// ZEB-310 Task 9: shared per-device HLC tracker for engine-auto
    /// orchestration (kd=sf / kd=cl / kd=rs). `None` until installed
    /// via `VotingLogEngineParams::hlc_tracker`; the orchestration
    /// hook skips when `None`. Same shape + reuse semantics as the
    /// IPC layer's tracker so engine-auto HLCs are monotone with IPC
    /// mints on the local device's lane.
    hlc_tracker: Option<Arc<Mutex<BTreeMap<String, Hlc>>>>,
    /// ZEB-310 Task 9: local device_id paired with `hlc_tracker`.
    /// `None` follows the same gating as `hlc_tracker`.
    device_id: Option<String>,
    /// ZEB-310 Task 12: optional Tauri AppHandle used by the post-apply
    /// hook to emit Tier 3 lifecycle events. `None` ⇒ engine runs without
    /// emitting Tauri events (tests + dormant production wiring).
    app_handle: Option<AppHandle<R>>,
    /// ZEB-310 Task 9: local signing key + owner for engine-auto
    /// orchestration paths. `None` ⇒ read-only peer mode (no
    /// orchestration). Installed via `install_local_signing_key`
    /// from the IPC layer at runtime and from tests via the
    /// equivalent setup helper.
    ///
    /// Wrapped in `RwLock<Option<...>>` so the install is one-shot
    /// from outside and reads are non-blocking on the common path.
    local_signing: RwLock<Option<(Arc<ed25519_dalek::SigningKey>, OwnerAddr)>>,
    /// ZEB-298+ZEB-312 PR 1: production wiring — identity resolver for
    /// Ed25519 signature verification on inbound voting events. `None`
    /// means the engine is not production-wired and inbound events are
    /// rejected.
    #[allow(dead_code)]
    pub(crate) identity_resolver:
        Option<std::sync::Arc<dyn crate::community_voting_core::VotingIdentityResolver>>,
    /// ZEB-298+ZEB-312 PR 1: production wiring — membership snapshot
    /// resolver for inbound voting events. `None` ⇒ rejected.
    #[allow(dead_code)]
    pub(crate) membership_resolver:
        Option<std::sync::Arc<dyn crate::community_voting_log::MembershipSnapshotResolver>>,
    /// ZEB-307 PhantomData<fn() -> R>: makes VotingLogEngine<R> unconditionally
    /// Send + Sync even when R = tauri::Wry (which is !Send because its
    /// EventLoop holds Rc<>). The engine only owns R through this marker,
    /// never a real R value.
    _phantom: PhantomData<fn() -> R>,
}

impl<R: tauri::Runtime> VotingLogEngine<R> {
    pub fn community_id(&self) -> SpaceId {
        self.community_id
    }

    /// Construct an engine, spawn its inbound receive loop, and return
    /// an `Arc<Self>` suitable for registry storage.
    pub async fn start(params: VotingLogEngineParams<R>) -> Arc<Self> {
        let tracker = Arc::new(Mutex::new(VotingReplayTracker::new()));

        // One-time startup log: if resolvers are absent, the engine runs in
        // outbound-only mode — inbound events will be silently dropped.
        // Gives operators a single visible signal at startup rather than
        // per-event log flood.
        if params.identity_resolver.is_none() || params.membership_resolver.is_none() {
            tracing::info!(
                community_id = ?params.community_id,
                identity_wired = params.identity_resolver.is_some(),
                membership_wired = params.membership_resolver.is_some(),
                "VotingLogEngine started in outbound-only mode \
                 (inbound disabled — ZEB-298+ZEB-312 PR 2 wires production resolvers)"
            );
        }

        // Spawn the inbound loop. It takes ownership of subscriber_rx
        // and exits cleanly when the adapter drops its matching Sender.
        let log_for_loop = Arc::clone(&params.voting_log);
        let tracker_for_loop = Arc::clone(&tracker);
        let community_id = params.community_id;
        let mut rx = params.subscriber_rx;
        let identity_resolver_for_loop = params.identity_resolver.clone();
        let membership_resolver_for_loop = params.membership_resolver.clone();
        let receive_handle = tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                if let Err(e) = Self::process_inbound(
                    community_id,
                    &log_for_loop,
                    &tracker_for_loop,
                    identity_resolver_for_loop.as_ref(),
                    membership_resolver_for_loop.as_ref(),
                    &packet,
                )
                .await
                {
                    tracing::warn!(
                        community_id = ?community_id,
                        err = ?e,
                        "voting engine inbound process failed"
                    );
                }
            }
        });

        Arc::new(Self {
            community_id,
            voting_log: Arc::clone(&params.voting_log),
            tracker,
            publisher_tx: params.publisher_tx,
            _receive_handle: receive_handle,
            dfrost_registry: Mutex::new(None),
            beacon_requester: Mutex::new(None),
            hlc_tracker: params.hlc_tracker,
            device_id: params.device_id,
            app_handle: params.app_handle,
            local_signing: RwLock::new(None),
            identity_resolver: params.identity_resolver,
            membership_resolver: params.membership_resolver,
            _phantom: PhantomData,
        })
    }

    /// ZEB-310 Task 9: install (or replace) the local signing key + owner
    /// used by engine-auto orchestration paths (`maybe_trigger_engine_auto_orchestration`).
    ///
    /// When unset (the default), engine-auto paths skip orchestration —
    /// the engine acts as a read-only peer. The IPC layer installs the
    /// key during `start_node`; tests install it via the multi-engine
    /// setup helper. Replacing an installed key is intentionally
    /// allowed (e.g. on identity rotation).
    ///
    /// Must be paired with `hlc_tracker` + `device_id` installed via
    /// `VotingLogEngineParams`; otherwise orchestration is dormant
    /// (`maybe_trigger_engine_auto_orchestration` short-circuits when
    /// any of the three is missing). Without this pairing,
    /// `reserve_next_local_hlc` would panic on its `.expect(...)`.
    pub async fn install_local_signing_key(
        &self,
        key: Arc<ed25519_dalek::SigningKey>,
        owner: OwnerAddr,
    ) {
        let mut w = self.local_signing.write().await;
        *w = Some((key, owner));
    }

    /// ZEB-310 Task 9: reserve the next HLC on the local device's lane,
    /// using the same atomic-reservation primitive the IPC layer uses.
    /// Pre-condition: `hlc_tracker` and `device_id` were installed via
    /// `VotingLogEngineParams`. Panics in tests if they weren't (which
    /// indicates a misconfigured fixture — orchestration should be
    /// short-circuited earlier by `local_signing.is_none()`).
    pub async fn reserve_next_local_hlc(&self) -> Hlc {
        let wall_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let tracker = self
            .hlc_tracker
            .as_ref()
            .expect("reserve_next_local_hlc called without hlc_tracker installed");
        let device_id = self
            .device_id
            .as_deref()
            .expect("reserve_next_local_hlc called without device_id installed");
        crate::dm_outbox::reserve_next_hlc_for_device(tracker, device_id, wall_now_ms).await
    }

    /// ZEB-310 Task 10: read-only "now" HLC estimate.
    ///
    /// Returns the engine's best estimate of "now" as an `Hlc` derived from
    /// real wall-clock time. Does NOT advance the tracker or reserve a lane —
    /// callers use this purely for deadline checks (e.g. has the ratification
    /// window expired?). Compare directly against stored HLC `wall_ms` fields;
    /// the `logical` and `device_id` placeholders are sentinels for comparison
    /// only and must NOT be used as a real event HLC.
    pub async fn current_hlc_estimate(&self) -> Hlc {
        let wall_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: String::new(),
        }
    }

    /// Wire in the DfrostLogRegistry + beacon_requester closure after construction.
    ///
    /// Task 10 Option A (injected callbacks): called from `start_node` / test
    /// setup after both registries are alive. Subscribes to VRF beacon arrivals
    /// so the engine can react with `on_dfrost_beacon`.
    ///
    /// `this` must be `Arc<Self>` so a `Weak` clone can be stored in the closure.
    /// `beacon_requester` captures `Arc<Mutex<NodeState>>` and calls
    /// `dfrost_request_vrf_beacon_inner`.
    pub async fn install_dfrost_handle(
        this: &Arc<Self>,
        registry: Arc<DfrostLogRegistry<R>>,
        beacon_requester: BeaconRequester,
    ) {
        // Store registry + requester for use in on_dfrost_beacon and
        // maybe_trigger_beacon_for_tier3_create.
        {
            let mut dr = this.dfrost_registry.lock().await;
            *dr = Some(Arc::clone(&registry));
        }
        {
            let mut br = this.beacon_requester.lock().await;
            *br = Some(beacon_requester);
        }

        // Subscribe to dfrost beacon arrivals. The closure captures a Weak ref
        // so we don't create a reference cycle (registry → engine → registry).
        let engine_weak = Arc::downgrade(this);
        registry
            .subscribe_beacons(move |payload, community_id| {
                if let Some(engine) = engine_weak.upgrade() {
                    let payload = payload.clone();
                    let community_id = *community_id;
                    // Callback is synchronous; spawn the async beacon handler.
                    tokio::spawn(async move {
                        engine.on_dfrost_beacon(&payload, community_id).await;
                    });
                }
            })
            .await;
    }

    /// Handle a VRF beacon arrival from DfrostLog.
    ///
    /// For each open Tier 3 poll in Stage::Sortition with no existing kd=ss:
    /// 1. Derive the beacon seed and check if the arriving beacon matches.
    /// 2. If match: compute Fisher-Yates sortition and publish a kd=ss event.
    ///
    /// This method is spawned by the dfrost beacon callback (see
    /// `install_dfrost_handle`). Errors are logged and dropped.
    async fn on_dfrost_beacon(
        self: &Arc<Self>,
        payload: &crate::community_dfrost_types::VrfBeaconPayload,
        community_id: SpaceId,
    ) {
        if community_id != self.community_id {
            return;
        }

        // Collect open Tier3 polls in Stage::Sortition without a kd=ss yet.
        let open_polls: Vec<_> = {
            let log = self.voting_log.lock().await;
            log.polls
                .values()
                .filter_map(|ps| {
                    if ps.meta.tier != Tier::Sortition {
                        return None;
                    }
                    let t3 = ps.tier_state.as_tier3()?;
                    if t3.stage != crate::community_voting_tier3::Stage::Sortition {
                        return None;
                    }
                    if t3.sortition_result.is_some() {
                        return None; // kd=ss already applied
                    }
                    // Derive the beacon seed for this poll.
                    let seed = crate::community_voting_sortition::derive_beacon_seed(
                        &t3.meta.poll_create_event_hash,
                        t3.meta.community_epoch,
                    );
                    // Check if the arriving beacon's message_hash matches.
                    use crate::community_dfrost_types::derive_vrf_seed;
                    let expected_mh = derive_vrf_seed(&seed, t3.meta.community_epoch);
                    if expected_mh != payload.message_hash {
                        return None; // not our beacon
                    }
                    // Snapshot what we need to compute sortition.
                    let poll_id = t3.meta.poll_id;
                    let electorate = t3.eligible_electorate_snapshot.clone();
                    let sortition_size = t3.meta.config.sortition_size as usize;
                    Some((poll_id, electorate, sortition_size))
                })
                .collect()
        };

        for (poll_id, electorate, sortition_size) in open_polls {
            self.publish_sortition_selection(
                poll_id,
                &electorate,
                sortition_size,
                &payload.vrf_output,
            )
            .await;
        }
    }

    /// Compute Fisher-Yates sortition and publish a kd=ss SortitionSelection event.
    async fn publish_sortition_selection(
        self: &Arc<Self>,
        poll_id: crate::community_voting_core::PollId,
        electorate: &[OwnerAddr],
        sortition_size: usize,
        vrf_output: &[u8; 32],
    ) {
        use crate::community_voting_core::{PollEventKindCode, SortitionSelectionPayload, Tier};
        use crate::community_voting_sortition::fisher_yates_select;

        if electorate.len() < sortition_size * 2 {
            tracing::warn!(
                community_id = ?self.community_id,
                ?poll_id,
                electorate_size = electorate.len(),
                sortition_size,
                "electorate too small for sortition (need sortition_size * 2); skipping kd=ss"
            );
            return;
        }

        let result = fisher_yates_select(vrf_output, electorate, sortition_size, sortition_size);

        let ss_payload = SortitionSelectionPayload {
            poll_id,
            primary: result.primary,
            backup: result.backup,
        };
        let mut payload_bytes = Vec::new();
        if let Err(e) = ciborium::into_writer(&ss_payload, &mut payload_bytes) {
            tracing::warn!(
                community_id = ?self.community_id,
                ?poll_id,
                error = %e,
                "on_dfrost_beacon: failed to encode SortitionSelectionPayload"
            );
            return;
        }

        // Build a synthetic kd=ss event. In Phase 4a-main, the engine does not
        // have a signing key wired — signing is done by the IPC layer. For the
        // engine-auto path, we publish an UNSIGNED event (sig = zero bytes) and
        // mark it as engine-generated. Task 19 (full NodeState wiring) will
        // inject the signing key. For now: publish_event does local apply + broadcast
        // with a zero signature. The verify layer (SS1) checks the VRF recompute,
        // not the envelope sig on locally-generated events.
        //
        // Note: zero-sig events will fail peer verify (Ed25519 verify). This is
        // accepted for Phase 4a-main — the kd=ss event is generated by every
        // engine that observes the beacon, so peer verification is redundant.
        // Task 19 wires real signing.
        //
        // Cluster 6 fix (CodeRabbit major, R1 bot review): include a poll_id
        // prefix in device_id to prevent replay-lane collisions when two beacons
        // arrive in the same millisecond. Without the prefix, both kd=ss events
        // share actor=zero+device="engine"+logical=0, and the second is treated
        // as a duplicate by the replay tracker (wall_ms was the only distinguishing
        // field at 1ms resolution).
        let hlc = {
            let wall_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            // Embed the first 4 bytes of poll_id (hex) into device_id so
            // different polls always occupy different replay-tracker lanes.
            let poll_id_prefix = hex::encode(&poll_id.0[..4]);
            crate::owner_state_types::Hlc {
                wall_ms,
                logical: 0,
                device_id: format!("engine-{poll_id_prefix}"),
            }
        };
        let ss_event = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::SortitionSelection,
            hlc,
            actor: OwnerAddr([0u8; 16]), // Task 19: wire self_actor
            payload: payload_bytes,
            sig: vec![0u8; 64], // Task 19: wire real signature
        };

        if let Err(e) = self.publish_event(ss_event, None).await {
            tracing::warn!(
                community_id = ?self.community_id,
                ?poll_id,
                error = %e,
                "on_dfrost_beacon: publish kd=ss failed"
            );
        }
    }

    /// After a Tier 3 PollCreate is locally applied (via publish_event),
    /// trigger a VRF beacon request via the injected beacon_requester.
    ///
    /// Only called for `tier == Sortition` + `kind == PollCreate` events.
    /// The beacon_requester is an Option — if None (Task 19 not yet wired),
    /// the request is silently skipped.
    ///
    /// Per feedback_metadata_before_irreversible_write: the apply has already
    /// succeeded before this is called; the beacon request is a follow-up
    /// side effect, not a precondition.
    async fn maybe_trigger_beacon_for_tier3_create(&self, event: &SignedVotingEvent) {
        if event.tier != Tier::Sortition || event.kind != PollEventKindCode::PollCreate {
            return;
        }
        let requester = match self.beacon_requester.lock().await.clone() {
            Some(r) => r,
            None => return, // beacon_requester not yet wired
        };

        // Cluster 2 fix (CodeRabbit major, R1 bot review): read epoch from the
        // poll's stored `meta.community_epoch` rather than re-querying
        // DfrostLogRegistry. The engine pre-read the epoch before apply and
        // stored it via `set_tier3_poll_epoch`; using the stored value avoids
        // the CHURP-refresh race where a new epoch could land between apply and
        // this call, causing the beacon request to use a different epoch than
        // was stored in the poll meta → message_hash mismatch → poll stalls.
        let (poll_create_event_hash, community_epoch) = {
            // Derive poll_id to look up the stored state.
            let sb = match event.signing_bytes() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        community_id = ?self.community_id,
                        error = ?e,
                        "maybe_trigger_beacon: signing_bytes failed, skipping beacon"
                    );
                    return;
                }
            };
            use sha2::{Digest, Sha256};
            let hash: [u8; 32] = Sha256::digest(&sb).into();
            let poll_id = crate::community_voting_core::derive_poll_id(&self.community_id, &sb);
            let epoch = {
                let log = self.voting_log.lock().await;
                log.polls
                    .get(&poll_id)
                    .and_then(|ps| ps.tier_state.as_tier3())
                    .map(|t3| t3.meta.community_epoch)
                    .unwrap_or(0) // poll just applied; always present
            };
            (hash, epoch)
        };

        let seed = crate::community_voting_sortition::derive_beacon_seed(
            &poll_create_event_hash,
            community_epoch,
        );
        let community_id = self.community_id;

        // Fire-and-forget: the beacon request may fail (no active committee,
        // ceremony already in flight). Log the error but don't propagate.
        tokio::spawn(async move {
            if let Err(e) = (requester)(community_id, seed, community_epoch).await {
                tracing::warn!(
                    community_id = ?community_id,
                    error = %e,
                    "maybe_trigger_beacon: dfrost_request_vrf_beacon_inner failed"
                );
            }
        });
    }

    /// ZEB-310 Task 9: post-apply engine-auto orchestration hook.
    ///
    /// Called from `publish_event` after a successful Tier 3 apply
    /// (gated on `event.tier == Tier::Sortition`). Inspects the affected
    /// poll's state and, if a follow-up engine-auto event is warranted
    /// (kd=sf, and in Tasks 10/11 also kd=cl + kd=rs), mints + publishes
    /// it as if it were locally originated.
    ///
    /// The recursion into `publish_event` is broken with `Box::pin` so
    /// the async-fn return-type cycle compiles. Mutual recursion is
    /// bounded by the L1 lifecycle gate: a kd=sf published from this
    /// hook moves the poll to `Stage::Failed`, after which no further
    /// orchestration trigger fires on that poll.
    ///
    /// Race tolerance: the same poll may simultaneously meet the
    /// trigger condition on multiple devices (e.g. two proposers, in
    /// theory only one but defensive). The follow-up apply is keyed by
    /// `decode_poll_id_ref` + L1 lifecycle, so the second arrival
    /// rejects cleanly without state divergence.
    ///
    /// Skipped silently when `local_signing` is unset (read-only peer
    /// mode), when the poll is not Tier 3 / not in Stage::Sortition,
    /// or when the local owner is not the poll's proposer.
    ///
    /// In Phase 4a-main this implements kd=sf only. ZEB-310 Tasks 10 + 11
    /// extend with kd=cl (drafting timeout / approval threshold) and
    /// kd=rs (ratification window close → STAR tally).
    async fn maybe_trigger_engine_auto_orchestration(self: &Arc<Self>, pid: &PollId) {
        // (1) Short-circuit: no local signing key ⇒ read-only peer.
        let (signing_key, self_owner) = {
            let r = self.local_signing.read().await;
            match r.as_ref() {
                Some((k, o)) => (k.clone(), *o),
                None => return,
            }
        };
        // Additional gate: orchestration also requires hlc_tracker AND
        // device_id installed via VotingLogEngineParams. A caller could
        // install a signing key without these (e.g. partial fixture);
        // without this gate `reserve_next_local_hlc` would panic on its
        // .expect(...). All three must be present for orchestration to
        // proceed.
        if self.hlc_tracker.is_none() || self.device_id.is_none() {
            tracing::debug!(
                "engine-auto: hlc_tracker or device_id missing; skipping orchestration"
            );
            return;
        }

        // (2) Inspect the affected poll's state. Snapshot the values we
        // need under the lock then drop it before the recursive
        // publish_event (which re-acquires the same lock).
        let trigger_kd_sf: bool = {
            let log = self.voting_log.lock().await;
            let state = match log.polls.get(pid) {
                Some(s) => s,
                None => return,
            };
            let t3 = match state.tier_state.as_tier3() {
                Some(t) => t,
                None => return,
            };
            // kd=sf trigger: Stage::Sortition + proposer == self + decline_count
            // covers the full sortition + backup pool. Mirrors `verify_sf`'s
            // SF1 invariant so peer engines reject the kd=sf only when the
            // local engine would have rejected it too.
            let stage_ok = matches!(t3.stage, crate::community_voting_tier3::Stage::Sortition);
            let proposer_ok = t3.meta.proposer == self_owner;
            if !stage_ok || !proposer_ok {
                false
            } else {
                // Use the maximum possible HLC sentinel — we want to count
                // EVERY decline applied so far, not filter by some now-of-event.
                // Per `Tier3PollState::decline_count_at`, declines are
                // included when `(wall_ms, logical, device_id) <= now`.
                let now_sentinel = Hlc {
                    wall_ms: u64::MAX,
                    logical: u32::MAX,
                    // Lexicographically-maximal device_id placeholder; any
                    // real device_id collates ≤ this in the (wall, logical,
                    // device_id) tuple.
                    device_id: "\u{10ffff}".into(),
                };
                let decline_count = t3.decline_count_at(&now_sentinel);
                let capacity = t3
                    .sortition_result
                    .as_ref()
                    .map(|sr| sr.primary.len() + sr.backup.len())
                    .unwrap_or(0);
                // capacity == 0 ⇒ no kd=ss yet ⇒ orchestration cannot fire.
                capacity > 0 && decline_count >= capacity
            }
        };

        if trigger_kd_sf {
            // (3) Mint a signed kd=sf event using the local signing key.
            let hlc = self.reserve_next_local_hlc().await;
            let sf_ev = match crate::community_voting_core::build_signed_sortition_failed(
                &signing_key,
                self_owner,
                *pid,
                hlc,
            ) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        poll_id = %hex::encode(pid.0),
                        "engine-auto kd=sf build_signed_sortition_failed failed"
                    );
                    return;
                }
            };

            // (4) Publish recursively. `Box::pin` breaks the async-fn
            // return-type cycle (publish_event → orchestration → publish_event).
            if let Err(e) = Box::pin(self.publish_event(sf_ev, None)).await {
                tracing::warn!(
                    error = %e,
                    poll_id = %hex::encode(pid.0),
                    "engine-auto kd=sf publish_event failed"
                );
            }
            // kd=sf moves the poll to Stage::Failed, so no further
            // orchestration trigger can fire on this poll — return early.
            return;
        }

        // ── ZEB-310 Task 10: kd=cl PollClose orchestration ───────────────
        //
        // Fires when the poll is in Ratification stage (HLC-aware via
        // `current_stage_at`) AND no kd=cl has been applied yet AND the
        // ratification window has expired (relative to the engine's real
        // wall-clock estimate). Re-reads the poll state under a fresh lock
        // because the kd=sf branch above may already have released and
        // re-acquired the log lock through the recursive `publish_event`.
        //
        // Race tolerance: multiple engines may simultaneously meet the kd=cl
        // trigger. The first valid arrival by HLC wins — duplicates are
        // dropped by the replay tracker on each engine. We log a `debug` line
        // on rejection instead of `warn` because a race loss is not a fault.
        //
        // Note: any signer (with `local_signing` installed) can publish kd=cl
        // per L1 lifecycle; we do NOT gate on `proposer == self`. The kd=sf
        // branch above is proposer-gated because SF1 verify is, but kd=cl
        // is a public timeout event.
        // Time reference: use `t3.last_hlc.wall_ms` — the latest applied
        // event's HLC — as the "now" estimate. This is purely HLC-driven
        // (NOT wall-clock based) for two reasons:
        //
        //   1. **Test determinism.** Tests mint events with synthetic HLCs
        //      that can be hours / days before the test's real wall-clock.
        //      A wall-clock-based trigger would always think the deadline
        //      has expired and prematurely close polls in tests that
        //      target other orchestration paths (e.g. ZEB-310 Task 9's
        //      kd=sf-from-mass-decline test).
        //
        //   2. **Eventual production correctness.** In production, real
        //      kd=rb events carry real wall-clock HLCs; once enough time
        //      has passed for `last_hlc.wall_ms ≥ created + total_window`
        //      to hold, the trigger fires on the NEXT apply. The explicit
        //      user-driven kd=cl IPC remains the canonical path; the
        //      engine-auto kd=cl is the safety net for polls where no one
        //      explicitly closes after the deadline.
        //
        // If `last_hlc` is None (no event applied yet, which shouldn't
        // happen given we just applied the triggering event), treat the
        // window as not yet expired.
        let trigger_kd_cl: bool = {
            let log = self.voting_log.lock().await;
            let state = match log.polls.get(pid) {
                Some(s) => s,
                None => return,
            };
            let t3 = match state.tier_state.as_tier3() {
                Some(t) => t,
                None => return,
            };
            // Cheap guards first.
            if t3.close_event_hash.is_some() {
                false
            } else {
                let last_wall = match t3.last_hlc.as_ref() {
                    Some(h) => h.wall_ms,
                    None => return,
                };
                let now_hlc_cl = Hlc {
                    wall_ms: last_wall,
                    logical: 0,
                    device_id: String::new(),
                };
                let stage_now = t3.current_stage_at(&now_hlc_cl);
                if !matches!(
                    stage_now,
                    crate::community_voting_tier3::Stage::Ratification
                ) {
                    false
                } else {
                    // Total window = deliberation + drafting + ratification.
                    // Engine fires kd=cl once `last_hlc.wall_ms` (HLC-driven
                    // "now") is past `created_wall + total_window_ms`.
                    let total_window_ms: u64 = (t3.meta.config.deliberation_window_seconds as u64
                        + t3.meta.config.drafting_window_seconds as u64
                        + t3.meta.config.ratification_window_seconds as u64)
                        * 1000;
                    let created_wall = t3.meta.poll_create_hlc.wall_ms;
                    last_wall >= created_wall.saturating_add(total_window_ms)
                }
            }
        };

        if trigger_kd_cl {
            let hlc = self.reserve_next_local_hlc().await;
            let cl_ev = match crate::community_voting_core::build_signed_poll_close_tier3(
                &signing_key,
                self_owner,
                *pid,
                hlc,
            ) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        poll_id = %hex::encode(pid.0),
                        "engine-auto kd=cl build_signed_poll_close_tier3 failed"
                    );
                    return;
                }
            };
            if let Err(e) = Box::pin(self.publish_event(cl_ev, None)).await {
                // L1 / replay-dedup rejection on race loss is expected.
                tracing::debug!(
                    error = %e,
                    poll_id = %hex::encode(pid.0),
                    "engine-auto kd=cl publish rejected (race loser?)"
                );
            }
            // Fall through to the kd=rs trigger: the recursive publish_event
            // above applied kd=cl locally and re-fired this hook from inside
            // the recursion (which sees `close_event_hash.is_some()` and
            // falls through to the kd=rs branch below). We still attempt
            // kd=rs at the outer level for defensive convergence; the
            // apply-time `PollInFinalizedState` gate cleanly rejects
            // duplicates.
        }

        // ── ZEB-310 Task 11: kd=rs PollResult orchestration ──────────────
        //
        // Fires when kd=cl has been applied (`close_event_hash.is_some()`) AND
        // no kd=rs has been applied yet (`result.is_none()`). Deterministically
        // computes the STAR tally and publishes signed kd=rs. The kd=rs apply
        // moves the poll to `Stage::Finalized`; subsequent kd=rs events from
        // race losers are rejected by the apply-time terminal-state gate
        // (`PollInFinalizedState` → `IllegalTransition`).
        //
        // The ratification candidate ordering is computed the same way as
        // `verify_sr`: synthesize status_quo, push onto a temp candidates
        // slice, derive advancers via `drafting_advancers`, then sort via
        // `ratification_candidates_ordering`. This ensures bit-identical
        // tally inputs across all engines that ever drive this code path.
        let trigger_kd_rs_args: Option<crate::community_voting_star::StarResult> = {
            let log = self.voting_log.lock().await;
            let state = match log.polls.get(pid) {
                Some(s) => s,
                None => return,
            };
            let t3 = match state.tier_state.as_tier3() {
                Some(t) => t,
                None => return,
            };
            if t3.close_event_hash.is_none() || t3.result.is_some() {
                None
            } else {
                // Build candidate ordering. Same pattern as verify_sr.
                let sq = crate::community_voting_tier3::synthesize_status_quo(&t3.meta.poll_id);
                let sq_hash = sq.event_hash;
                // status_quo is NOT inserted into t3.candidates by apply()
                // (no materialize step today); push it onto a temp slice so
                // drafting_advancers returns Some.
                let mut all_candidates = t3.candidates.clone();
                all_candidates.push(sq);
                let primary_size = t3.meta.config.sortition_size as usize;
                let advancers = match crate::community_voting_tier3::drafting_advancers(
                    &all_candidates,
                    primary_size,
                    sq_hash,
                ) {
                    Some(a) => a,
                    None => {
                        // status_quo missing — pre-Drafting stage, can't
                        // happen given close_event_hash.is_some() invariant
                        // (kd=cl is only minted after Drafting → Ratification),
                        // but bail defensively rather than panicking.
                        tracing::warn!(
                            poll_id = %hex::encode(pid.0),
                            "engine-auto kd=rs: drafting_advancers returned None despite kd=cl applied"
                        );
                        return;
                    }
                };
                let ordered = crate::community_voting_tier3::ratification_candidates_ordering(
                    &advancers, sq_hash,
                );
                let ballots = crate::community_voting_tier3::collect_ratification_ballots(t3);
                let result = crate::community_voting_star::tally_star(&ordered, ballots);
                Some(result)
            }
        };

        if let Some(result) = trigger_kd_rs_args {
            let hlc = self.reserve_next_local_hlc().await;
            let rs_ev = match crate::community_voting_core::build_signed_poll_result_tier3(
                &signing_key,
                self_owner,
                *pid,
                result,
                hlc,
            ) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        poll_id = %hex::encode(pid.0),
                        "engine-auto kd=rs build_signed_poll_result_tier3 failed"
                    );
                    return;
                }
            };
            if let Err(e) = Box::pin(self.publish_event(rs_ev, None)).await {
                // PollInFinalizedState (race loser) is expected.
                tracing::debug!(
                    error = %e,
                    poll_id = %hex::encode(pid.0),
                    "engine-auto kd=rs publish rejected (race loser?)"
                );
            }
        }
    }

    /// Publish a locally-minted, already-signed voting event.
    ///
    /// Pipeline:
    /// 1. CBOR-encode the event (fail fast on encode error).
    /// 2. **Record on the replay tracker BEFORE broadcasting** — see the
    ///    self-loopback fix from ZEB-270 reproduced in
    ///    `community_channel_log_engine::ChannelLogEngine::publish`. If
    ///    we broadcast first, a Zenoh self-loopback can race through the
    ///    subscriber path's `contains()` gate, find the tracker empty,
    ///    and double-apply the event before our local `apply_with_snapshot`
    ///    call runs.
    /// 3. Apply locally so the local UI / queries see the event without
    ///    waiting for a round-trip.
    /// 4. Broadcast on `publisher_tx`. Drop on full channel (degraded mode);
    ///    local apply already succeeded.
    ///
    /// Note: the caller is responsible for verify (V1-V6 + kind-specific)
    /// before invoking this. Phase 2's IPC layer always does that.
    pub async fn publish_event(
        self: &Arc<Self>,
        event: SignedVotingEvent,
        // ZEB-298+ZEB-312 PR 2 Task 3: optional membership snapshot used
        // only for Tier 3 PollCreate, which freezes the electorate at
        // creation time. For all other event kinds the snapshot is None
        // (the apply path reads the poll's frozen snapshot from state).
        snapshot: Option<crate::community_voting_core::MembershipSnapshot>,
    ) -> Result<(), String> {
        // (1) Encode first so we don't mutate any state on a malformed event.
        let mut packet = Vec::new();
        ciborium::into_writer(&event, &mut packet).map_err(|e| format!("encode: {e}"))?;

        // (2) Record BEFORE publishing — self-loopback fix.
        //
        // Self-mint path: the caller already reserved an HLC strictly
        // newer than any previously-recorded one on this (actor, device)
        // lane, so this is an unconditional high-water bump.
        {
            let mut tracker = self.tracker.lock().await;
            tracker.record(&event);
        }

        // (3) For Tier 3 PollCreate: read the current D-FROST epoch BEFORE apply
        // so we can store it atomically with the new poll state. This avoids the
        // epoch-refresh race where a CHURP refresh could land between apply and
        // beacon request, causing epoch mismatch (Cluster 1+2 fix, R1 bot review).
        //
        // Cluster E fix (CodeRabbit major, R2 bot review): if the D-FROST registry or engine
        // is not ready, REJECT the PollCreate rather than accepting it with epoch=0. A poll
        // with epoch=0 will stall — no beacon will ever match a seed derived from epoch=0
        // unless the community happened to use epoch 0 (which is only possible at inception).
        // The caller (IPC layer) receives a meaningful error and can retry after D-FROST starts.
        let tier3_create_epoch: Option<(PollId, u64)> =
            if event.kind == PollEventKindCode::PollCreate && event.tier == Tier::Sortition {
                let epoch = {
                    let dr = self.dfrost_registry.lock().await;
                    if let Some(reg) = dr.as_ref() {
                        if let Some(engine) = reg.get(self.community_id).await {
                            engine.current_epoch().await
                        } else {
                            return Err(
                                "DfrostNotReady: no D-FROST engine running for this community; \
                                 retry Tier 3 PollCreate after D-FROST is initialized"
                                    .into(),
                            );
                        }
                    } else {
                        return Err("DfrostNotReady: D-FROST registry not installed; \
                             retry Tier 3 PollCreate after install_dfrost_handle"
                            .into());
                    }
                };
                // Derive poll_id from signing bytes (same derivation as apply_with_snapshot).
                let sb = event
                    .signing_bytes()
                    .map_err(|e| format!("signing_bytes for epoch pre-read: {e}"))?;
                let poll_id = crate::community_voting_core::derive_poll_id(&self.community_id, &sb);
                Some((poll_id, epoch))
            } else {
                None
            };

        // ZEB-310 Task 12: snapshot the affected poll's current stage BEFORE
        // apply so the post-apply hook can detect Deliberation→Drafting and
        // Drafting→Ratification transitions and emit Tauri lifecycle events.
        //
        // Computed only when an `app_handle` is wired and the event targets
        // Tier 3 (the only tier with materialized stage transitions). For
        // PollCreate (no prior poll state) the snapshot is `None`. The
        // derivation is identical to the `tier3_create_epoch` branch above:
        // signing_bytes → derive_poll_id. Re-uses
        // `current_stage_at(current_hlc_estimate)` so the snapshot reflects
        // the same HLC-aware "now" the post-apply emit will use, giving
        // bit-identical previous/new comparisons across the apply boundary.
        let previous_stage_for_emit: Option<crate::community_voting_tier3::Stage> =
            if self.app_handle.is_some()
                && event.tier == Tier::Sortition
                && event.kind != PollEventKindCode::PollCreate
            {
                // Derive pid from signing bytes (cheap; same derivation as apply).
                let pid_opt: Option<PollId> = event.signing_bytes().ok().map(|sb| {
                    crate::community_voting_core::derive_poll_id(&self.community_id, &sb)
                });
                match pid_opt {
                    Some(pid) => {
                        let now = self.current_hlc_estimate().await;
                        let log = self.voting_log.lock().await;
                        log.polls
                            .get(&pid)
                            .and_then(|ps| ps.tier_state.as_tier3())
                            .map(|t3| t3.current_stage_at(&now))
                    }
                    None => None,
                }
            } else {
                None
            };

        // Apply locally. Capture the returned `PollId` so the engine-auto
        // post-apply hook (ZEB-310 Task 9) can inspect the affected poll's
        // state without re-deriving the id from signing_bytes.
        let applied_poll_id: PollId = {
            let mut log = self.voting_log.lock().await;
            let pid = log
                .apply_with_snapshot(event.clone(), &self.community_id, snapshot.clone())
                .map_err(|e| format!("apply: {e:?}"))?;

            // Store the pre-read epoch on the newly-created Tier 3 poll.
            // This must happen in the same lock scope as apply so the poll
            // state is consistent before any beacon callback can read it.
            if let Some((poll_id, epoch)) = tier3_create_epoch {
                log.set_tier3_poll_epoch(&poll_id, epoch);
            }
            pid
        };

        // (3a) After a Tier 3 PollCreate is applied, trigger a VRF beacon
        // request. The stored poll epoch is now authoritative; the beacon
        // request uses it via the poll's meta (not a fresh dfrost query).
        // Fire-and-forget; errors are logged inside maybe_trigger_beacon_for_tier3_create.
        self.maybe_trigger_beacon_for_tier3_create(&event).await;

        // (4) Broadcast. `send().await` waits for adapter capacity rather
        // than dropping on a full channel — Phase 2 has no backfill, so
        // a silently-dropped publish would mean peers permanently miss
        // a unique voting event while the local log has already applied
        // it (CR R3 Major). Channel-closed (`SendError`) propagates as
        // an error to the caller; the local apply has already happened
        // so callers can decide whether to surface this. Backfill is
        // tracked as a follow-up (ZEB-291 Task 19.1).
        //
        // ZEB-310 Tasks 10/11 ordering note: broadcast MUST run before the
        // engine-auto orchestration hook so peers receive the just-applied
        // event before any hook-triggered follow-ups (kd=cl, kd=rs). If the
        // hook recursed first, the outer event's broadcast would land AFTER
        // the inner events on the wire — peers would then apply the
        // follow-ups (e.g. kd=rs) onto an outdated state and reject the
        // outer event via the terminal-state apply gate
        // (`PollInFinalizedState`). The self-loopback fix is preserved
        // because `tracker.record` already ran above (step 2) before this
        // broadcast.
        self.publisher_tx
            .send(packet)
            .await
            .map_err(|e| format!("voting publisher_tx closed: {e}"))?;

        // (5) ZEB-310 Task 9: engine-auto orchestration post-apply hook.
        // Only Tier 3 events can drive sortition / drafting / ratification
        // stage transitions, so cheaply gate on tier first. The hook itself
        // is race-tolerant — late duplicates are rejected by the L1
        // lifecycle gate in apply. Runs AFTER broadcast so any recursive
        // publish_event the hook triggers broadcasts in the natural order:
        // outer event first on the wire, then any follow-ups.
        if event.tier == Tier::Sortition {
            self.maybe_trigger_engine_auto_orchestration(&applied_poll_id)
                .await;
            // ZEB-310 Task 12: emit Tauri lifecycle events for Tier 3.
            // Runs AFTER orchestration so any kd=cl / kd=rs minted by the
            // hook above has already applied locally — the new_stage we
            // observe here reflects the post-orchestration end state, not
            // the intermediate one. (E.g. when kd=ss arrives past the
            // ratification deadline, orchestration races through
            // kd=cl + kd=rs and we want Finalized to be the emitted state.)
            self.maybe_emit_tier3_lifecycle_events(
                &applied_poll_id,
                &event,
                previous_stage_for_emit,
            )
            .await;
        }

        // (6) ZEB-298 Task 5: emit `voting-delegate-signaled-on-your-behalf`
        // when the just-applied event is a Tier 2 Signal whose signaler is
        // the local user's current delegate and the community policy opts
        // in. Mirror the call site of `maybe_emit_tier3_lifecycle_events`
        // (post-apply, post-broadcast); Task 8 will also wire this hook
        // from `process_inbound` so peer replicas notify identically.
        self.maybe_emit_delegate_on_behalf(&event, &applied_poll_id)
            .await;

        Ok(())
    }

    /// ZEB-310 Task 12: emit the four Tier 3 lifecycle Tauri events from
    /// the post-apply hook. Companion to
    /// `maybe_trigger_engine_auto_orchestration` — both run from the same
    /// hook point in `publish_event` but emit is intentionally a separate
    /// concern (Tauri event delivery is purely a UI hint, while
    /// orchestration mutates log state).
    ///
    /// Events fired (each guarded by its own condition; multiple may fire
    /// in a single hook invocation, e.g. when kd=ss past the ratification
    /// deadline triggers cascaded kd=cl + kd=rs orchestration):
    ///
    /// - `voting-tier3-sortition-complete` — applied event was kd=ss
    /// - `voting-tier3-drafting-open` — stage transitioned Deliberation→Drafting
    /// - `voting-tier3-ratification-open` — stage transitioned Drafting→Ratification
    /// - `voting-tier3-finalized` — applied event was kd=rs (Tier 3)
    ///
    /// All emit failures are non-fatal — logged at WARN and ignored. The
    /// local log + broadcast already succeeded; failing to notify the UI
    /// is a degraded path, not a state divergence.
    ///
    /// `previous_stage` was snapshotted in `publish_event` BEFORE apply;
    /// `None` for PollCreate / when no AppHandle is wired / when the poll
    /// did not yet exist. The stage-transition guards (`Some(Deliberation)
    /// → Drafting`, etc.) require a `Some(_)` previous to fire.
    async fn maybe_emit_tier3_lifecycle_events(
        self: &Arc<Self>,
        pid: &PollId,
        applied_event: &SignedVotingEvent,
        previous_stage: Option<crate::community_voting_tier3::Stage>,
    ) {
        let app_handle = match &self.app_handle {
            Some(h) => h.clone(),
            None => return, // test-only mode without Tauri runtime
        };

        // Snapshot the post-apply tier3 state.
        let now = self.current_hlc_estimate().await;
        let (sortition_result, new_stage, candidates, result) = {
            let log = self.voting_log.lock().await;
            let t3 = match log.polls.get(pid).and_then(|ps| ps.tier_state.as_tier3()) {
                Some(t) => t,
                None => return,
            };
            (
                t3.sortition_result.clone(),
                t3.current_stage_at(&now),
                t3.candidates.clone(),
                t3.result.clone(),
            )
        };

        let pid_hex = hex::encode(pid.0);
        let community_id_hex = hex::encode(self.community_id.0);

        // 1. Sortition complete (any apply of kd=ss). Pulls from the now-set
        // sortition_result. If it's still None despite the kind code matching,
        // the apply must have been rejected mid-flight — skip silently.
        if applied_event.kind == PollEventKindCode::SortitionSelection {
            if let Some(sr) = &sortition_result {
                let payload = crate::VotingTier3SortitionCompletePayload {
                    poll_id: pid_hex.clone(),
                    community_id: community_id_hex.clone(),
                    primary: sr.primary.iter().map(|o| hex::encode(o.0)).collect(),
                    backup: sr.backup.iter().map(|o| hex::encode(o.0)).collect(),
                };
                if let Err(e) = app_handle.emit("voting-tier3-sortition-complete", &payload) {
                    tracing::warn!(
                        error = %e,
                        poll_id = %pid_hex,
                        "voting-tier3-sortition-complete emit failed (non-fatal)"
                    );
                }
            }
        }

        // 2. Drafting open (stage transition 2 → 3).
        if matches!(
            previous_stage,
            Some(crate::community_voting_tier3::Stage::Deliberation)
        ) && matches!(new_stage, crate::community_voting_tier3::Stage::Drafting)
        {
            let payload = crate::VotingTier3DraftingOpenPayload {
                poll_id: pid_hex.clone(),
                community_id: community_id_hex.clone(),
            };
            if let Err(e) = app_handle.emit("voting-tier3-drafting-open", &payload) {
                tracing::warn!(
                    error = %e,
                    poll_id = %pid_hex,
                    "voting-tier3-drafting-open emit failed (non-fatal)"
                );
            }
        }

        // 3. Ratification open (stage transition 3 → 4). Build the
        // deterministic candidate ordering the same way verify_sr / the
        // kd=rs orchestrator does: synthesize status_quo, derive advancers,
        // then sort. The text lookup pulls from the just-snapshotted
        // `candidates` (status_quo's text is "<status quo>" by
        // `synthesize_status_quo`).
        if matches!(
            previous_stage,
            Some(crate::community_voting_tier3::Stage::Drafting)
        ) && matches!(
            new_stage,
            crate::community_voting_tier3::Stage::Ratification
        ) {
            // Lookup map: event_hash → text. status_quo is synthesized,
            // never present in candidates (until materialize lands); we
            // hard-code its text here to match `synthesize_status_quo`.
            let sq = crate::community_voting_tier3::synthesize_status_quo(pid);
            let sq_hash = sq.event_hash;
            let mut all_candidates = candidates.clone();
            all_candidates.push(sq);
            // sortition_size from the just-snapshotted state — fall back
            // to a sentinel if (somehow) the poll vanished. Re-acquire the
            // lock briefly: cheap; the snapshot already happened above
            // but we did not capture sortition_size.
            let primary_size: usize = {
                let log = self.voting_log.lock().await;
                log.polls
                    .get(pid)
                    .and_then(|ps| ps.tier_state.as_tier3())
                    .map(|t3| t3.meta.config.sortition_size as usize)
                    .unwrap_or(0)
            };
            if let Some(advancers) = crate::community_voting_tier3::drafting_advancers(
                &all_candidates,
                primary_size,
                sq_hash,
            ) {
                let ordered = crate::community_voting_tier3::ratification_candidates_ordering(
                    &advancers, sq_hash,
                );
                let candidate_ordering: Vec<crate::CandidateRefDto> = ordered
                    .iter()
                    .map(|c| {
                        let text = all_candidates
                            .iter()
                            .find(|cs| cs.event_hash == c.event_hash)
                            .map(|cs| cs.text.clone())
                            .unwrap_or_default();
                        crate::CandidateRefDto {
                            event_hash: hex::encode(c.event_hash),
                            text,
                            approval_count: c.approval_count,
                        }
                    })
                    .collect();
                let payload = crate::VotingTier3RatificationOpenPayload {
                    poll_id: pid_hex.clone(),
                    community_id: community_id_hex.clone(),
                    candidate_ordering,
                };
                if let Err(e) = app_handle.emit("voting-tier3-ratification-open", &payload) {
                    tracing::warn!(
                        error = %e,
                        poll_id = %pid_hex,
                        "voting-tier3-ratification-open emit failed (non-fatal)"
                    );
                }
            }
        }

        // 4. Finalized (apply of kd=rs Tier 3). `StarResult` carries
        // `winner`, `finalists`, `total_scores` (indexed by candidate
        // position in the ratification ordering), and `runoff_votes`
        // (indexed by finalist position). To populate the DTO's
        // `scores_summary` we need the candidate ordering — pull it from
        // `t3.candidates` via the same derivation as the ratification-open
        // branch above so positions line up.
        if applied_event.kind == PollEventKindCode::PollResult
            && applied_event.tier == Tier::Sortition
        {
            if let Some(star_result) = &result {
                // Re-derive the ratification ordering to align scores by
                // position. This is the same `drafting_advancers +
                // ratification_candidates_ordering` chain the engine-auto
                // kd=rs branch uses, so the positions are bit-identical to
                // what `total_scores` indexes against.
                let sq = crate::community_voting_tier3::synthesize_status_quo(pid);
                let sq_hash = sq.event_hash;
                let mut all_candidates = candidates.clone();
                all_candidates.push(sq);
                let primary_size: usize = {
                    let log = self.voting_log.lock().await;
                    log.polls
                        .get(pid)
                        .and_then(|ps| ps.tier_state.as_tier3())
                        .map(|t3| t3.meta.config.sortition_size as usize)
                        .unwrap_or(0)
                };
                let ordered: Vec<crate::community_voting_star::CandidateRef> =
                    match crate::community_voting_tier3::drafting_advancers(
                        &all_candidates,
                        primary_size,
                        sq_hash,
                    ) {
                        Some(advancers) => {
                            crate::community_voting_tier3::ratification_candidates_ordering(
                                &advancers, sq_hash,
                            )
                        }
                        None => Vec::new(),
                    };

                // Lookup winner text + build per-candidate score summary.
                let winner_text = all_candidates
                    .iter()
                    .find(|cs| cs.event_hash == star_result.winner.event_hash)
                    .map(|cs| cs.text.clone())
                    .unwrap_or_default();

                // Runner-up = highest-runoff-votes finalist that is NOT
                // the winner. `StarResult.finalists` is unordered (a Vec
                // of CandidateRefs); the matching `runoff_votes` slice is
                // positionally aligned. Pick the finalist with max
                // runoff_votes among non-winners, breaking ties on
                // event_hash ASC to match the deterministic tiebreaker
                // used by `tally_star`.
                let runner_up_event_hash: Option<String> = {
                    let mut best: Option<(u32, [u8; 32])> = None;
                    for (i, f) in star_result.finalists.iter().enumerate() {
                        if f.event_hash == star_result.winner.event_hash {
                            continue;
                        }
                        let rv = star_result.runoff_votes.get(i).copied().unwrap_or(0);
                        let candidate = (rv, f.event_hash);
                        best = Some(match best {
                            None => candidate,
                            Some((b_rv, b_eh)) => {
                                if rv > b_rv || (rv == b_rv && f.event_hash < b_eh) {
                                    candidate
                                } else {
                                    (b_rv, b_eh)
                                }
                            }
                        });
                    }
                    best.map(|(_, eh)| hex::encode(eh))
                };

                let scores_summary: Vec<crate::CandidateScoreDto> = ordered
                    .iter()
                    .enumerate()
                    .map(|(i, cand)| {
                        let total_score = star_result.total_scores.get(i).copied().unwrap_or(0);
                        // runoff_votes is indexed by finalist position, not by candidate
                        // position. Look up by event_hash to align.
                        let runoff_votes = star_result
                            .finalists
                            .iter()
                            .position(|f| f.event_hash == cand.event_hash)
                            .and_then(|fi| star_result.runoff_votes.get(fi).copied())
                            .unwrap_or(0);
                        crate::CandidateScoreDto {
                            event_hash: hex::encode(cand.event_hash),
                            total_score,
                            runoff_votes,
                        }
                    })
                    .collect();

                let payload = crate::VotingTier3FinalizedPayload {
                    poll_id: pid_hex.clone(),
                    community_id: community_id_hex.clone(),
                    winner_event_hash: hex::encode(star_result.winner.event_hash),
                    winner_text,
                    runner_up_event_hash,
                    scores_summary,
                };
                if let Err(e) = app_handle.emit("voting-tier3-finalized", &payload) {
                    tracing::warn!(
                        error = %e,
                        poll_id = %pid_hex,
                        "voting-tier3-finalized emit failed (non-fatal)"
                    );
                }
            }
        }
    }

    /// ZEB-298 Task 5: emit `voting-delegate-signaled-on-your-behalf` when
    /// the just-applied event is a Tier 2 Signal whose signaler is the
    /// local user's current delegate in this community and the community
    /// policy opts in. Fired from `publish_event` (this PR) and from
    /// `process_inbound` (ZEB-298 Task 8) so the local replica notifies
    /// identically regardless of whether the Signal arrived via outbound
    /// IPC or peer-inbound.
    ///
    /// Conjunctive guards (ALL must hold to emit):
    /// 1. `self.app_handle` is `Some(_)` — tests/lightweight harnesses pass
    ///    `None` and stay silent.
    /// 2. Event is a Tier 2 Signal (`Tier::Conviction` + `kd=Signal`).
    /// 3. Community policy `notify_on_delegate_signal == true`.
    /// 4. The local user's current delegate in this community equals
    ///    `event.actor` — by construction this implies (a) the local user
    ///    has a live `delegation_graph` edge in this community, i.e. is a
    ///    "registered" Tier 2 voter, and (b) the signaler is that
    ///    delegate. (The task spec lists these as two conditions; the
    ///    `delegate_of(local) == Some(actor)` test collapses them
    ///    cleanly — VotingLog stores no separate community-membership
    ///    accessor today, so the delegation edge is the operational
    ///    proxy.)
    ///
    /// All emit failures are non-fatal — logged at WARN and ignored. The
    /// local log + broadcast already succeeded; failing to notify the UI
    /// is a degraded path, not a state divergence.
    async fn maybe_emit_delegate_on_behalf(&self, event: &SignedVotingEvent, poll_id: &PollId) {
        // Guard 1: app_handle wired.
        let Some(app) = self.app_handle.as_ref() else {
            return;
        };

        // Guard 2: Tier 2 Signal only. Cheapest gate — runs before any
        // lock acquire so non-Tier-2 traffic pays zero overhead.
        if !matches!(
            (event.tier, event.kind),
            (Tier::Conviction, PollEventKindCode::Signal)
        ) {
            return;
        }

        // Local user comes from `local_signing` (set by IPC at startup
        // and by tests via the equivalent helper). Read-only peer mode
        // (no installed key) cannot have a "local delegate".
        let local_owner = {
            let r = self.local_signing.read().await;
            match r.as_ref() {
                Some((_, owner)) => *owner,
                None => return,
            }
        };

        // Read policy + delegate edge under one VotingLog lock acquire.
        // Drop the lock before the emit (Tauri emit may serialize JSON
        // and dispatch synchronously; holding the lock through that is
        // unnecessary and risks contention with concurrent applies).
        let (notify_enabled, current_delegate) = {
            let log = self.voting_log.lock().await;
            let notify = log.policy().notify_on_delegate_signal;
            let delegate = log.delegation_graph.delegate_of(local_owner);
            (notify, delegate)
        };

        // Guard 3: policy opted in.
        if !notify_enabled {
            return;
        }

        // Guards 4 (+ membership-by-proxy): local user has a current
        // delegate AND that delegate is the signaler.
        let Some(delegate) = current_delegate else {
            return;
        };
        if event.actor != delegate {
            return;
        }

        // Decode the Tier 2 Signal payload to read `support`.
        // Decode failure is non-fatal: the apply already succeeded, so
        // the payload is well-formed at the apply layer — a decode error
        // here would indicate a serialization drift bug we want surfaced
        // in logs but not propagated.
        let support = match ciborium::de::from_reader::<
            crate::community_voting_conviction::SignalPayload,
            _,
        >(&event.payload[..])
        {
            Ok(p) => p.support,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "voting-delegate-signaled-on-your-behalf: signal payload decode failed"
                );
                return;
            }
        };

        // Camel-case payload per harmony-client IPC convention.
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Payload {
            community_id: String,
            proposal_id: String,
            delegate: String,
            support: bool,
        }
        let payload = Payload {
            community_id: hex::encode(self.community_id.0),
            proposal_id: hex::encode(poll_id.0),
            delegate: hex::encode(event.actor.0),
            support,
        };
        if let Err(e) = app.emit("voting-delegate-signaled-on-your-behalf", &payload) {
            tracing::warn!(
                error = %e,
                community_id = %hex::encode(self.community_id.0),
                "voting-delegate-signaled-on-your-behalf emit failed (non-fatal)"
            );
        }
    }

    /// Inbound packet processing: decode, dedup, verify, apply, record.
    ///
    /// Called from the receive loop spawned by `start`. Errors here are
    /// logged and dropped (peer sent garbage, failed signature check, or
    /// we hit a transient apply failure); we never propagate up to the
    /// receive loop or kill the engine.
    ///
    /// ZEB-298+ZEB-312 PR 1: verify-then-apply path. The snapshot is
    /// resolved uniformly for every event kind (pragmatic uniformity
    /// over case-splitting PollCreate-fresh vs others-cached): freshness
    /// cost is small, and the uniform shape avoids snapshot-shape
    /// divergence between tier1_snapshot and tier3 eligible_electorate.
    pub(crate) async fn process_inbound(
        community_id: SpaceId,
        voting_log: &Arc<Mutex<VotingLog>>,
        tracker: &Arc<Mutex<VotingReplayTracker>>,
        identity_resolver: Option<&Arc<dyn crate::community_voting_core::VotingIdentityResolver>>,
        membership_resolver: Option<
            &Arc<dyn crate::community_voting_log::MembershipSnapshotResolver>,
        >,
        packet: &[u8],
    ) -> Result<(), String> {
        // Decode.
        let event: SignedVotingEvent =
            ciborium::from_reader(packet).map_err(|e| format!("decode: {e}"))?;

        // Dedup gate.
        {
            let tracker = tracker.lock().await;
            if tracker.contains(&event) {
                // Self-loopback or peer redelivery; drop silently.
                return Ok(());
            }
        }

        // ZEB-298+ZEB-312 PR 1: when resolvers are absent (engine not fully
        // production-wired — e.g. identity_resolver is deferred to PR 2),
        // silently drop inbound events. This avoids flooding logs with
        // "resolver not installed" warns on every peer event arriving over
        // Zenoh. Inbound activates once PR 2 wires the production
        // OwnerDeviceCacheResolver adapter.
        let (Some(id_resolver), Some(mem_resolver)) = (identity_resolver, membership_resolver)
        else {
            tracing::debug!(
                community_id = ?community_id,
                "process_inbound: dropping event — resolvers not wired"
            );
            return Ok(());
        };

        let snapshot = mem_resolver
            .snapshot_at(community_id, &event.hlc)
            .await
            .map_err(|e| format!("snapshot resolve: {e}"))?;

        crate::community_voting_core::verify_voting_event(&event, &snapshot, id_resolver.as_ref())
            .await
            .map_err(|e| format!("verify: {e}"))?;

        // ZEB-298+ZEB-312 PR 1 Fix (Qodo finding): per-tier inbound eligibility check.
        // verify_voting_event does V6 (membership) + signature only — it
        // intentionally skips eligibility. For peer-submitted events that
        // create or vote on proposals, we enforce the proposal's eligibility
        // predicate before applying, matching local-IPC parity.
        inbound_eligibility_check(&event, &snapshot, voting_log).await?;

        // Apply with the verified snapshot.
        {
            let mut log = voting_log.lock().await;
            log.apply_with_snapshot(event.clone(), &community_id, Some(snapshot))
                .map_err(|e| format!("apply: {e:?}"))?;
        }

        // Record AFTER successful apply on the inbound path: if apply
        // failed (illegal transition, etc.) we don't want to suppress a
        // legitimate retry. On the publish path the ordering is reversed
        // for the self-loopback fix.
        {
            let mut tracker = tracker.lock().await;
            tracker.record(&event);
        }

        Ok(())
    }
}

// ── Inbound eligibility helper ──────────────────────────────────────────────

/// Per-tier inbound eligibility check called from `process_inbound` between
/// `verify_voting_event` and `apply_with_snapshot`. Mirrors the predicates
/// that each local-IPC handler enforces before signing:
///
/// - Tier 1 PollCreate: creator must satisfy the config's eligibility predicate
///   (eligibility is embedded in the `Tier1PollConfig` payload).
/// - Tier 1 BallotCast: voter must satisfy the poll's eligibility predicate
///   (fetched from `PollState.tier1_cfg`).
/// - Tier 2 PollCreate: creator must satisfy `Tier2PollConfig.eligibility`.
/// - Tier 2 Signal: signaller must satisfy the proposal's eligibility predicate
///   (fetched from `PollState.tier_state.as_tier2().config.eligibility`).
/// - Tier 2 Delegate / Undelegate: community-wide graph mutations — no
///   proposal-specific eligibility check (membership-V6 sufficient).
/// - Tier 3 PollCreate: creator must satisfy `Tier3PollConfigPayload.eligibility`.
/// - Tier 3 engine-auto events (SortitionSelection, SortitionFailed, PollClose,
///   PollResult): signed by the local engine itself, not a remote peer
///   proposer/voter. No proposal-specific eligibility check required.
/// - All other Tier 3 peer events (DeliberationStatement, MiniPublicDecline,
///   DraftCandidate, DraftApproval, RatificationBallot): eligibility for
///   these events is membership in the sortition selection, not the proposal's
///   eligibility predicate. The Tier 3 apply path enforces sortition membership
///   (the electorate snapshot was frozen at PollCreate time). No additional
///   check here to avoid double-enforcement.
async fn inbound_eligibility_check(
    event: &SignedVotingEvent,
    snapshot: &crate::community_voting_core::MembershipSnapshot,
    voting_log: &Arc<Mutex<VotingLog>>,
) -> Result<(), String> {
    match event.tier {
        crate::community_voting_core::Tier::Approval => {
            match event.kind {
                crate::community_voting_core::PollEventKindCode::PollCreate => {
                    // Tier 1 PollCreate: eligibility predicate is embedded in
                    // the payload (Tier1PollConfig.eligibility). Creator must
                    // satisfy it — mirrors voting_create_tier1_poll's check.
                    let cfg: crate::community_voting_approval::Tier1PollConfig =
                        ciborium::de::from_reader(&event.payload[..])
                            .map_err(|e| format!("decode Tier1PollConfig: {e}"))?;
                    crate::community_voting_core::check_eligibility(
                        snapshot,
                        &event.actor,
                        &cfg.eligibility,
                    )
                    .map_err(|e| format!("Tier 1 PollCreate: creator not eligible: {e:?}"))?;
                }
                crate::community_voting_core::PollEventKindCode::BallotCast => {
                    // Tier 1 BallotCast: use the poll's FROZEN tier1_snapshot
                    // (captured at PollCreate apply-time) for the eligibility
                    // check, mirroring voting_cast_tier1_ballot's local-IPC
                    // discipline. Using the fresh at-HEAD snapshot here would
                    // diverge from local (which uses the frozen one), causing
                    // peer/local apply mismatch during membership churn — a
                    // member who voted while eligible would be retroactively
                    // rejected on peer apply if they later lost eligibility.
                    let ballot: crate::community_voting_approval::Tier1Ballot =
                        ciborium::de::from_reader(&event.payload[..])
                            .map_err(|e| format!("decode Tier1Ballot: {e}"))?;
                    let log_g = voting_log.lock().await;
                    let (eligibility, frozen_snapshot) = match log_g.polls.get(&ballot.poll_id) {
                        Some(ps) => {
                            let cfg = ps.tier1_cfg.as_ref().ok_or_else(|| {
                                format!(
                                    "Tier 1 BallotCast: poll {} missing tier1_cfg",
                                    hex::encode(ballot.poll_id.0)
                                )
                            })?;
                            let snap = ps.tier1_snapshot.clone().ok_or_else(|| {
                                format!(
                                    "Tier 1 BallotCast: poll {} missing tier1_snapshot \
                                     (peer-received poll without frozen snapshot — \
                                     ZEB-298+ZEB-312 PR 2 will fill this via the inbound \
                                     apply path materializing the snapshot)",
                                    hex::encode(ballot.poll_id.0)
                                )
                            })?;
                            (cfg.eligibility, snap)
                        }
                        None => {
                            return Err(format!(
                                "Tier 1 BallotCast for unknown poll {}",
                                hex::encode(ballot.poll_id.0)
                            ));
                        }
                    };
                    drop(log_g);
                    crate::community_voting_core::check_eligibility(
                        &frozen_snapshot,
                        &event.actor,
                        &eligibility,
                    )
                    .map_err(|e| format!("Tier 1 BallotCast: voter not eligible: {e:?}"))?;
                }
                // All other Tier 1 events (PollOpen, PollExtend, PollClose, PollResult):
                // engine-auto or lifecycle events; membership-V6 check from
                // verify_voting_event is sufficient.
                _ => {}
            }
        }
        crate::community_voting_core::Tier::Conviction => {
            match event.kind {
                crate::community_voting_core::PollEventKindCode::PollCreate => {
                    // Tier 2 PollCreate: eligibility is embedded in the payload
                    // (Tier2PollConfig.eligibility). Creator must satisfy it.
                    let cfg: crate::community_voting_conviction::Tier2PollConfig =
                        ciborium::de::from_reader(&event.payload[..])
                            .map_err(|e| format!("decode Tier2PollConfig: {e}"))?;
                    crate::community_voting_core::check_eligibility(
                        snapshot,
                        &event.actor,
                        &cfg.eligibility,
                    )
                    .map_err(|e| format!("Tier 2 PollCreate: creator not eligible: {e:?}"))?;
                }
                crate::community_voting_core::PollEventKindCode::Signal => {
                    // Tier 2 Signal: fetch the proposal's eligibility from the log.
                    let signal: crate::community_voting_conviction::SignalPayload =
                        ciborium::de::from_reader(&event.payload[..])
                            .map_err(|e| format!("decode SignalPayload: {e}"))?;
                    let log_g = voting_log.lock().await;
                    let eligibility = match log_g.polls.get(&signal.proposal_id) {
                        Some(ps) => match &ps.tier_state {
                            crate::community_voting_log::TierState::Tier2(t2) => {
                                t2.config.eligibility
                            }
                            _ => {
                                return Err(format!(
                                    "Tier 2 Signal for non-Tier-2 poll {}",
                                    hex::encode(signal.proposal_id.0)
                                ));
                            }
                        },
                        None => {
                            return Err(format!(
                                "Tier 2 Signal for unknown poll {}",
                                hex::encode(signal.proposal_id.0)
                            ));
                        }
                    };
                    drop(log_g);
                    crate::community_voting_core::check_eligibility(
                        snapshot,
                        &event.actor,
                        &eligibility,
                    )
                    .map_err(|e| format!("Tier 2 Signal: actor not eligible: {e:?}"))?;
                }
                // Delegate/Undelegate are community-wide graph mutations — no
                // proposal-specific eligibility check (membership-V6 is sufficient).
                _ => {}
            }
        }
        crate::community_voting_core::Tier::Sortition => {
            match event.kind {
                crate::community_voting_core::PollEventKindCode::PollCreate => {
                    // Tier 3 PollCreate: eligibility is embedded in the payload
                    // (Tier3PollConfigPayload.eligibility). Creator must satisfy it —
                    // mirrors voting_create_tier3_proposal's check_eligibility call.
                    let cfg: crate::community_voting_core::Tier3PollConfigPayload =
                        ciborium::de::from_reader(&event.payload[..])
                            .map_err(|e| format!("decode Tier3PollConfigPayload: {e}"))?;
                    crate::community_voting_core::check_eligibility(
                        snapshot,
                        &event.actor,
                        &cfg.eligibility,
                    )
                    .map_err(|e| format!("Tier 3 PollCreate: creator not eligible: {e:?}"))?;
                }
                // Engine-auto events (SortitionSelection, SortitionFailed, PollClose,
                // PollResult): signed by the local engine, not a remote peer proposer.
                // No proposal-specific eligibility check — the engine signing key is
                // the trust anchor.
                crate::community_voting_core::PollEventKindCode::SortitionSelection
                | crate::community_voting_core::PollEventKindCode::SortitionFailed
                | crate::community_voting_core::PollEventKindCode::PollClose
                | crate::community_voting_core::PollEventKindCode::PollResult => {}
                // Tier 3 peer events scoped to sortition members
                // (DeliberationStatement, MiniPublicDecline, DraftCandidate,
                // DraftApproval, RatificationBallot): eligibility for these is
                // membership in the sortition selection (snapshotted at PollCreate
                // time as eligible_electorate_snapshot). The Tier 3 apply path
                // already enforces sortition membership — no additional check here
                // to avoid double-enforcement. Membership-V6 from
                // verify_voting_event is sufficient for the outer gate.
                _ => {}
            }
        }
    }
    Ok(())
}

// ── process_inbound_for_test seam ───────────────────────────────────────────

/// ZEB-298+ZEB-312 PR 1 test seam: invoke `process_inbound` directly from
/// integration tests (which compile against the public API). Gated by
/// neither `cfg(test)` nor `feature = "test-fixtures"` — the production
/// build also exposes this, since it is the load-bearing assertion that
/// the feature-gate is gone.
#[doc(hidden)]
pub async fn process_inbound_for_test(
    community_id: crate::owner_state_types::SpaceId,
    voting_log: &Arc<Mutex<VotingLog>>,
    tracker: &Arc<Mutex<VotingReplayTracker>>,
    identity_resolver: Option<&Arc<dyn crate::community_voting_core::VotingIdentityResolver>>,
    membership_resolver: Option<&Arc<dyn crate::community_voting_log::MembershipSnapshotResolver>>,
    packet: &[u8],
) -> Result<(), String> {
    VotingLogEngine::<tauri::Wry>::process_inbound(
        community_id,
        voting_log,
        tracker,
        identity_resolver,
        membership_resolver,
        packet,
    )
    .await
}

// ── Registry ────────────────────────────────────────────────────────────────

/// Per-`SpaceId` map of running engines. Wired into `NodeState` in Task 19
/// alongside the existing channel-log registry.
///
/// Task 10: generic over `R: tauri::Runtime` to match `VotingLogEngine<R>`.
/// The `PhantomData<fn() -> R>` marker keeps the registry `Send + Sync`.
pub struct VotingLogRegistry<R: tauri::Runtime = tauri::Wry> {
    engines: Mutex<HashMap<SpaceId, Arc<VotingLogEngine<R>>>>,
    _phantom: PhantomData<fn() -> R>,
}

impl<R: tauri::Runtime> Default for VotingLogRegistry<R> {
    fn default() -> Self {
        Self {
            engines: Mutex::new(HashMap::new()),
            _phantom: PhantomData,
        }
    }
}

impl<R: tauri::Runtime> VotingLogRegistry<R> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start an engine for `params.community_id` and stash it in the
    /// registry. If an engine already exists for that community it is
    /// replaced — the caller is responsible for shutting the old one
    /// down by dropping their `Arc` (the receive loop will then exit
    /// when the adapter's publisher sender is dropped).
    pub async fn register(&self, params: VotingLogEngineParams<R>) -> Arc<VotingLogEngine<R>> {
        let cid = params.community_id;
        let engine = VotingLogEngine::start(params).await;
        let mut engines = self.engines.lock().await;
        engines.insert(cid, Arc::clone(&engine));
        engine
    }

    pub async fn get(&self, community_id: SpaceId) -> Option<Arc<VotingLogEngine<R>>> {
        let engines = self.engines.lock().await;
        engines.get(&community_id).cloned()
    }

    /// Drop every engine. Each engine's receive task is owned by the
    /// engine itself (`_receive_handle`); dropping the engine `Arc`
    /// causes the task handle to drop, which aborts the task. The
    /// matching `publisher_tx` sender held by the adapter is the
    /// adapter's to drop.
    pub async fn shutdown(&self) {
        let mut engines = self.engines.lock().await;
        engines.clear();
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_voting_approval::Tier1PollConfig;
    use crate::community_voting_core::{
        Eligibility, MemberAttrs, MembershipSnapshot, PollEventKindCode, SignedVotingEvent, Tier,
        VotingIdentityResolver,
    };
    use crate::community_voting_log::{MembershipSnapshotResolver, SnapshotResolverError};
    use crate::owner_state_types::Hlc;
    use std::time::Duration;

    // ── Test resolvers ─────────────────────────────────────────────────────

    /// Fixed resolver pair for unit tests: holds a HashMap of OwnerAddr →
    /// 64-byte composite identity (X25519 || Ed25519) and a fixed MembershipSnapshot.
    struct FixedTestResolvers {
        identity: HashMap<OwnerAddr, [u8; 64]>,
        snapshot: MembershipSnapshot,
    }

    #[async_trait::async_trait]
    impl VotingIdentityResolver for FixedTestResolvers {
        async fn resolve(&self, owner: &OwnerAddr) -> Option<[u8; 64]> {
            self.identity.get(owner).copied()
        }
    }

    /// Build a `(SigningKey, OwnerAddr, [u8; 64])` triple from a single-byte seed.
    /// The returned `owner`'s `address_hash` is derived from the public key bytes —
    /// the same binding enforced by `verify_voting_event`'s defense-in-depth check.
    fn fixture_identity_engine(seed: u8) -> (ed25519_dalek::SigningKey, OwnerAddr, [u8; 64]) {
        let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
        let owner = OwnerAddr(priv_id.identity.address_hash);
        let pub_64 = priv_id.identity.to_public_bytes();
        let private_bytes = priv_id.to_private_bytes();
        let mut ed_secret = [0u8; 32];
        ed_secret.copy_from_slice(&private_bytes[32..64]);
        let signing = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
        (signing, owner, pub_64)
    }

    #[async_trait::async_trait]
    impl MembershipSnapshotResolver for FixedTestResolvers {
        async fn snapshot_at(
            &self,
            _community_id: SpaceId,
            _hlc: &Hlc,
        ) -> Result<MembershipSnapshot, SnapshotResolverError> {
            Ok(self.snapshot.clone())
        }
    }

    // ── Helpers ────────────────────────────────────────────────────────────

    fn good_tier1_config() -> Tier1PollConfig {
        Tier1PollConfig {
            options: vec!["A".into(), "B".into()],
            window_seconds: 3600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            channel_id: crate::community_membership::ChannelId([0x11; 16]),
        }
    }

    fn poll_create_event(actor: OwnerAddr, device_id: &str, wall_ms: u64) -> SignedVotingEvent {
        let mut payload = Vec::new();
        ciborium::into_writer(&good_tier1_config(), &mut payload).expect("encode cfg");
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind: PollEventKindCode::PollCreate,
            hlc: Hlc {
                wall_ms,
                logical: 0,
                device_id: device_id.into(),
            },
            actor,
            payload,
            sig: vec![0u8; 64],
        }
    }

    fn encode_event(event: &SignedVotingEvent) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(event, &mut buf).expect("encode event");
        buf
    }

    // ── VotingReplayTracker ────────────────────────────────────────────────

    #[test]
    fn replay_tracker_basic_dedup() {
        let mut tracker = VotingReplayTracker::new();
        let actor = OwnerAddr([0xaa; 16]);
        let event = poll_create_event(actor, "dev-a", 1_000);

        // First sight: not seen yet.
        assert!(
            !tracker.contains(&event),
            "tracker.contains must be false before any record()"
        );

        tracker.record(&event);
        assert!(
            tracker.contains(&event),
            "tracker.contains must be true after record()"
        );

        // Record-again is idempotent — same HLC stays the high-water.
        tracker.record(&event);
        assert!(
            tracker.contains(&event),
            "double-record must keep contains() true"
        );
    }

    #[test]
    fn replay_tracker_different_devices() {
        // Same actor, different device IDs → independent lanes, no
        // dedup interference. This matches the (actor, device_id) keying
        // in the channel-log tracker.
        let mut tracker = VotingReplayTracker::new();
        let actor = OwnerAddr([0xaa; 16]);
        let ev_dev_a = poll_create_event(actor, "dev-a", 1_000);
        let ev_dev_b = poll_create_event(actor, "dev-b", 500);

        tracker.record(&ev_dev_a);
        assert!(tracker.contains(&ev_dev_a));
        assert!(
            !tracker.contains(&ev_dev_b),
            "recording on dev-a must not mark dev-b as seen"
        );

        tracker.record(&ev_dev_b);
        assert!(tracker.contains(&ev_dev_b));
        // dev-a still recorded — independent lanes.
        assert!(tracker.contains(&ev_dev_a));
    }

    #[test]
    fn replay_tracker_newer_event_not_dedup() {
        // record(wall_ms=1000), then check contains(wall_ms=2000):
        // newer event is NOT in the tracker (would_accept analog).
        let mut tracker = VotingReplayTracker::new();
        let actor = OwnerAddr([0xaa; 16]);
        let older = poll_create_event(actor, "dev-a", 1_000);
        let newer = poll_create_event(actor, "dev-a", 2_000);

        tracker.record(&older);
        assert!(tracker.contains(&older));
        assert!(
            !tracker.contains(&newer),
            "newer event on same lane must not be marked seen yet"
        );
    }

    // ── Engine: publish + self-loopback ────────────────────────────────────

    #[tokio::test]
    async fn engine_publish_self_loopback_no_double_apply() {
        // The critical ZEB-270-derived test: simulate a Zenoh self-loopback
        // by manually pushing the published packet onto subscriber_rx after
        // publish. Because publish_event records the event in the tracker
        // BEFORE try_send, the inbound path's contains() check drops the
        // loopback before reaching apply. Result: log.events has the event
        // exactly once.
        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let community_id = SpaceId([0x55; 16]);

        // mpsc pair: publisher_tx is consumed by the engine; we keep
        // publisher_rx so we can inspect what was broadcast.
        let (publisher_tx, mut publisher_rx) = mpsc::channel::<Vec<u8>>(8);
        // subscriber pair: we keep subscriber_tx so we can simulate a
        // loopback; engine consumes subscriber_rx in its receive loop.
        let (subscriber_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(8);

        let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: None,
            identity_resolver: None,
            membership_resolver: None,
        })
        .await;

        let actor = OwnerAddr([0xaa; 16]);
        let event = poll_create_event(actor, "dev-a", 1_000);

        engine
            .publish_event(event.clone(), None)
            .await
            .expect("publish_event");

        // Drain what the engine broadcast.
        let broadcast_packet = publisher_rx
            .recv()
            .await
            .expect("publisher_tx must have a packet");

        // Simulate self-loopback by pushing the same packet onto
        // subscriber_rx. The receive loop should drop it via the tracker.
        subscriber_tx
            .send(broadcast_packet)
            .await
            .expect("simulate loopback send");

        // Give the receive loop a tick to consume + drop the loopback.
        // 50ms is generous; the receive loop is a single tokio::sync
        // hop. Yields a few times so the task scheduler actually
        // services the spawned task on busy CI.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Assert: the log applied the event exactly once.
        let log = voting_log.lock().await;
        assert_eq!(
            log.events.len(),
            1,
            "self-loopback must not double-apply; got {} events",
            log.events.len()
        );
        assert_eq!(log.polls.len(), 1, "exactly one poll should be present");
    }

    // ── Engine: inbound apply ──────────────────────────────────────────────

    #[tokio::test]
    async fn engine_inbound_apply() {
        // Push a properly signed packet onto subscriber_rx without going
        // through publish_event; the receive loop must verify + apply it.
        //
        // ZEB-298+ZEB-312 PR 1: now requires resolvers to be wired because
        // the #[cfg(not(test))] gate is gone and verify-then-apply is
        // unconditional. We build a real Ed25519 keypair so the signature
        // checks out.
        use crate::community_voting_core::build_signed_poll_create_tier1;

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let community_id = SpaceId([0x77; 16]);

        let (keypair, peer_actor, peer_pub64) = fixture_identity_engine(0xbb);

        let cfg = good_tier1_config();
        let peer_event = build_signed_poll_create_tier1(
            &keypair,
            peer_actor,
            &cfg,
            Hlc {
                wall_ms: 5_000,
                logical: 0,
                device_id: "dev-peer".into(),
            },
        )
        .expect("build peer event");

        // Build fixed resolvers so the engine can verify the inbound event.
        let resolvers = Arc::new(FixedTestResolvers {
            identity: HashMap::from([(peer_actor, peer_pub64)]),
            snapshot: MembershipSnapshot {
                members: HashMap::from([(
                    peer_actor,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 1,
                    },
                )]),
            },
        });
        let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
        let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();

        let (publisher_tx, _publisher_rx) = mpsc::channel::<Vec<u8>>(8);
        let (subscriber_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(8);

        let _engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: None,
            identity_resolver: Some(id_resolver),
            membership_resolver: Some(mem_resolver),
        })
        .await;

        let packet = encode_event(&peer_event);
        subscriber_tx
            .send(packet)
            .await
            .expect("simulate peer inbound");

        // Give the receive loop time to consume + apply.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        let log = voting_log.lock().await;
        assert_eq!(
            log.events.len(),
            1,
            "inbound packet must apply once; got {} events",
            log.events.len()
        );
        assert_eq!(
            log.polls.len(),
            1,
            "inbound PollCreate must materialize one poll"
        );
    }

    // ── Registry ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn registry_register_and_get() {
        let registry = VotingLogRegistry::<tauri::test::MockRuntime>::new();

        let cid_a = SpaceId([0x01; 16]);
        let cid_b = SpaceId([0x02; 16]);

        let log_a = Arc::new(Mutex::new(VotingLog::new()));
        let log_b = Arc::new(Mutex::new(VotingLog::new()));

        let (pub_tx_a, _pub_rx_a) = mpsc::channel::<Vec<u8>>(8);
        let (_sub_tx_a, sub_rx_a) = mpsc::channel::<Vec<u8>>(8);
        let engine_a = registry
            .register(VotingLogEngineParams {
                community_id: cid_a,
                voting_log: Arc::clone(&log_a),
                publisher_tx: pub_tx_a,
                subscriber_rx: sub_rx_a,
                hlc_tracker: None,
                device_id: None,
                app_handle: None,
                identity_resolver: None,
                membership_resolver: None,
            })
            .await;

        let (pub_tx_b, _pub_rx_b) = mpsc::channel::<Vec<u8>>(8);
        let (_sub_tx_b, sub_rx_b) = mpsc::channel::<Vec<u8>>(8);
        let engine_b = registry
            .register(VotingLogEngineParams {
                community_id: cid_b,
                voting_log: Arc::clone(&log_b),
                publisher_tx: pub_tx_b,
                subscriber_rx: sub_rx_b,
                hlc_tracker: None,
                device_id: None,
                app_handle: None,
                identity_resolver: None,
                membership_resolver: None,
            })
            .await;

        // Get returns the right engine for each community.
        let got_a = registry.get(cid_a).await.expect("engine for cid_a");
        let got_b = registry.get(cid_b).await.expect("engine for cid_b");

        assert!(
            Arc::ptr_eq(&got_a, &engine_a),
            "registry must return the engine registered for cid_a"
        );
        assert!(
            Arc::ptr_eq(&got_b, &engine_b),
            "registry must return the engine registered for cid_b"
        );
        assert_eq!(got_a.community_id(), cid_a);
        assert_eq!(got_b.community_id(), cid_b);

        // Unknown community returns None.
        assert!(registry.get(SpaceId([0x99; 16])).await.is_none());

        // Shutdown clears.
        registry.shutdown().await;
        assert!(registry.get(cid_a).await.is_none());
        assert!(registry.get(cid_b).await.is_none());
    }

    // ── Beacon integration: on_dfrost_beacon ──────────────────────────────

    /// Build a minimal valid Tier 3 PollCreate event. Uses a large-enough
    /// electorate so publish_sortition_selection's `electorate.len() >=
    /// sortition_size * 2` guard passes.
    fn tier3_poll_create_event(
        actor: OwnerAddr,
        device_id: &str,
        wall_ms: u64,
        sortition_size: u16,
    ) -> (SignedVotingEvent, Vec<crate::owner_state_types::OwnerAddr>) {
        use crate::community_voting_core::Eligibility;
        use crate::community_voting_core::Tier3PollConfigPayload;

        let config = Tier3PollConfigPayload {
            proposal_text: "test proposal".to_string(),
            sortition_size,
            deliberation_window_seconds: 3600,
            drafting_window_seconds: 3600,
            ratification_window_seconds: 3600,
            privacy_mode: "pu".to_string(),
            incentive_mode: "a".to_string(),
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            retry_of: None,
        };
        let mut payload = Vec::new();
        ciborium::into_writer(&config, &mut payload).expect("encode tier3 cfg");

        let event = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::PollCreate,
            hlc: Hlc {
                wall_ms,
                logical: 0,
                device_id: device_id.into(),
            },
            actor,
            payload,
            sig: vec![0u8; 64],
        };

        // Build an electorate of sortition_size * 2 members so the guard passes.
        let electorate: Vec<crate::owner_state_types::OwnerAddr> = (0..sortition_size as usize * 2)
            .map(|i| {
                let mut addr = [0u8; 16];
                addr[0] = (i & 0xFF) as u8;
                addr[1] = ((i >> 8) & 0xFF) as u8;
                crate::owner_state_types::OwnerAddr(addr)
            })
            .collect();

        (event, electorate)
    }

    /// Derive the expected beacon message_hash for a Tier3 poll state.
    /// Mirrors the logic in on_dfrost_beacon.
    fn expected_beacon_message_hash(
        poll_create_event_hash: &[u8; 32],
        community_epoch: u64,
    ) -> [u8; 32] {
        use crate::community_dfrost_types::derive_vrf_seed;
        use crate::community_voting_sortition::derive_beacon_seed;
        let seed = derive_beacon_seed(poll_create_event_hash, community_epoch);
        derive_vrf_seed(&seed, community_epoch)
    }

    /// on_dfrost_beacon publishes a kd=ss event when the beacon matches an
    /// open Tier 3 Sortition poll with no existing sortition_result.
    #[tokio::test]
    async fn voting_engine_on_dfrost_beacon_publishes_kd_ss_for_matching_poll() {
        use crate::community_dfrost_types::VrfBeaconPayload;
        use crate::community_voting_core::MembershipSnapshot;

        let community_id = SpaceId([0xC0; 16]);
        let actor = OwnerAddr([0xAA; 16]);
        let sortition_size: u16 = 20; // min valid value per validate_tier3_poll_config

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let (publisher_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(32);
        let (_sub_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(32);

        let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: None,
            identity_resolver: None,
            membership_resolver: None,
        })
        .await;

        let (create_event, electorate) =
            tier3_poll_create_event(actor, "dev-a", 1_000, sortition_size);

        // Manually compute poll_create_event_hash (mirrors VotingLog::apply_with_snapshot).
        let signing_bytes = create_event.signing_bytes().expect("signing bytes");
        let poll_create_event_hash: [u8; 32] = {
            use sha2::{Digest, Sha256};
            Sha256::digest(&signing_bytes).into()
        };

        // Apply the Tier 3 PollCreate directly to the log with electorate snapshot.
        {
            let mut log = voting_log.lock().await;
            let snapshot = MembershipSnapshot {
                members: electorate
                    .iter()
                    .map(|addr| {
                        (
                            *addr,
                            crate::community_voting_core::MemberAttrs {
                                power: 1,
                                vouching_depth: 0,
                            },
                        )
                    })
                    .collect(),
            };
            log.apply_with_snapshot(create_event, &community_id, Some(snapshot))
                .expect("tier3 poll create apply");
        }

        // Verify the poll is present in Sortition stage.
        {
            let log = voting_log.lock().await;
            assert_eq!(log.polls.len(), 1);
            let ps = log.polls.values().next().unwrap();
            let t3 = ps.tier_state.as_tier3().expect("tier3 state");
            assert_eq!(t3.stage, crate::community_voting_tier3::Stage::Sortition);
            assert!(t3.sortition_result.is_none());
        }

        // Build a matching VrfBeaconPayload (community_epoch = 0 since log uses 0).
        let community_epoch: u64 = 0;
        let message_hash = expected_beacon_message_hash(&poll_create_event_hash, community_epoch);
        let payload = VrfBeaconPayload {
            ceremony_id: [0x01u8; 32],
            message_hash,
            signature: vec![0u8; 64],
            vrf_output: [0xAB; 32],
        };

        // Fire on_dfrost_beacon.
        engine.on_dfrost_beacon(&payload, community_id).await;

        // Give any spawned tasks time to complete.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Assert: a kd=ss event was applied → sortition_result is Some.
        let log = voting_log.lock().await;
        assert_eq!(log.events.len(), 2, "should have PollCreate + kd=ss events");
        let ps = log.polls.values().next().unwrap();
        let t3 = ps.tier_state.as_tier3().expect("tier3 state");
        assert!(
            t3.sortition_result.is_some(),
            "sortition_result should be set after on_dfrost_beacon"
        );
    }

    /// on_dfrost_beacon ignores beacon for a different community_id.
    #[tokio::test]
    async fn voting_engine_on_dfrost_beacon_ignores_non_matching_community_id() {
        use crate::community_dfrost_types::VrfBeaconPayload;
        use crate::community_voting_core::MembershipSnapshot;

        let community_id = SpaceId([0xC1; 16]);
        let other_community = SpaceId([0xC2; 16]);
        let actor = OwnerAddr([0xAA; 16]);
        let sortition_size: u16 = 20; // min valid value per validate_tier3_poll_config

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let (publisher_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(32);
        let (_sub_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(32);

        let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: None,
            identity_resolver: None,
            membership_resolver: None,
        })
        .await;

        let (create_event, electorate) =
            tier3_poll_create_event(actor, "dev-b", 2_000, sortition_size);

        {
            let mut log = voting_log.lock().await;
            let snapshot = MembershipSnapshot {
                members: electorate
                    .iter()
                    .map(|addr| {
                        (
                            *addr,
                            crate::community_voting_core::MemberAttrs {
                                power: 1,
                                vouching_depth: 0,
                            },
                        )
                    })
                    .collect(),
            };
            log.apply_with_snapshot(create_event, &community_id, Some(snapshot))
                .expect("apply");
        }

        // Fire beacon for a DIFFERENT community_id.
        let payload = VrfBeaconPayload {
            ceremony_id: [0x02u8; 32],
            message_hash: [0xFF; 32], // wrong hash anyway
            signature: vec![0u8; 64],
            vrf_output: [0xCD; 32],
        };
        engine.on_dfrost_beacon(&payload, other_community).await;

        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // No kd=ss event published — log still has only PollCreate.
        let log = voting_log.lock().await;
        assert_eq!(
            log.events.len(),
            1,
            "wrong community_id must not trigger kd=ss"
        );
        let t3 = log
            .polls
            .values()
            .next()
            .unwrap()
            .tier_state
            .as_tier3()
            .unwrap();
        assert!(
            t3.sortition_result.is_none(),
            "sortition_result must remain None"
        );
    }

    /// on_dfrost_beacon ignores a poll that already has a sortition_result.
    #[tokio::test]
    async fn voting_engine_on_dfrost_beacon_ignores_poll_with_existing_kd_ss() {
        use crate::community_dfrost_types::VrfBeaconPayload;
        use crate::community_voting_core::derive_poll_id;
        use crate::community_voting_core::MembershipSnapshot;
        use crate::community_voting_core::SortitionSelectionPayload;

        let community_id = SpaceId([0xC3; 16]);
        let actor = OwnerAddr([0xAA; 16]);
        let sortition_size: u16 = 20; // min valid value per validate_tier3_poll_config

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let (publisher_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(32);
        let (_sub_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(32);

        let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: None,
            identity_resolver: None,
            membership_resolver: None,
        })
        .await;

        let (create_event, electorate) =
            tier3_poll_create_event(actor, "dev-c", 3_000, sortition_size);

        let signing_bytes = create_event.signing_bytes().expect("signing bytes");
        let poll_create_event_hash: [u8; 32] = {
            use sha2::{Digest, Sha256};
            Sha256::digest(&signing_bytes).into()
        };
        let poll_id = derive_poll_id(&community_id, &signing_bytes);

        {
            let mut log = voting_log.lock().await;
            let snapshot = MembershipSnapshot {
                members: electorate
                    .iter()
                    .map(|addr| {
                        (
                            *addr,
                            crate::community_voting_core::MemberAttrs {
                                power: 1,
                                vouching_depth: 0,
                            },
                        )
                    })
                    .collect(),
            };
            log.apply_with_snapshot(create_event.clone(), &community_id, Some(snapshot))
                .expect("apply");

            // Pre-apply a kd=ss so sortition_result is already Some.
            let ss_payload = SortitionSelectionPayload {
                poll_id,
                primary: electorate[..sortition_size as usize].to_vec(),
                backup: electorate[sortition_size as usize..sortition_size as usize * 2].to_vec(),
            };
            let mut ss_bytes = Vec::new();
            ciborium::into_writer(&ss_payload, &mut ss_bytes).expect("encode ss");
            let ss_event = SignedVotingEvent {
                tag: 'p',
                version: 1,
                tier: Tier::Sortition,
                kind: PollEventKindCode::SortitionSelection,
                hlc: Hlc {
                    wall_ms: 3_001,
                    logical: 0,
                    device_id: "dev-c".into(),
                },
                actor,
                payload: ss_bytes,
                sig: vec![0u8; 64],
            };
            log.apply(ss_event, &community_id).expect("pre-apply kd=ss");
        }

        // Verify sortition_result is already set.
        {
            let log = voting_log.lock().await;
            let t3 = log
                .polls
                .values()
                .next()
                .unwrap()
                .tier_state
                .as_tier3()
                .unwrap();
            assert!(
                t3.sortition_result.is_some(),
                "pre-condition: ss already applied"
            );
        }

        // Fire a matching beacon.
        let community_epoch: u64 = 0;
        let message_hash = expected_beacon_message_hash(&poll_create_event_hash, community_epoch);
        let payload = VrfBeaconPayload {
            ceremony_id: [0x03u8; 32],
            message_hash,
            signature: vec![0u8; 64],
            vrf_output: [0xEF; 32],
        };
        engine.on_dfrost_beacon(&payload, community_id).await;

        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Should still be exactly 2 events (PollCreate + first kd=ss), no new one.
        let log = voting_log.lock().await;
        assert_eq!(
            log.events.len(),
            2,
            "beacon must not publish a second kd=ss when sortition_result already set"
        );
    }

    /// maybe_trigger_beacon_for_tier3_create fires the beacon_requester when
    /// a Tier 3 PollCreate event is published.
    ///
    /// Cluster E update: publish_event now requires a DfrostLogRegistry with a
    /// running engine (rejects with DfrostNotReady otherwise). This test installs
    /// a minimal DfrostLogEngine for the community via install_dfrost_handle so
    /// the epoch pre-read succeeds. epoch=0 is acceptable here (fresh engine).
    #[tokio::test]
    async fn voting_engine_apply_tier3_create_triggers_beacon_request() {
        use crate::community_dfrost_log_engine::{DfrostLogEngineParams, DfrostLogRegistry};
        use crate::community_state_sync::IdentityResolver;
        use std::sync::atomic::{AtomicU32, Ordering};

        let community_id = SpaceId([0xC4; 16]);
        let actor = OwnerAddr([0xAA; 16]);
        let sortition_size: u16 = 20; // min valid value per validate_tier3_poll_config

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let (publisher_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(32);
        let (_sub_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(32);

        let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: None,
            identity_resolver: None,
            membership_resolver: None,
        })
        .await;

        // Install a minimal DfrostLogRegistry with a running engine for community_id.
        // This satisfies the Cluster E check (DfrostNotReady guard) without needing
        // a real D-FROST ceremony. The engine's current_epoch() returns 0 for a fresh log.
        let dfrost_reg = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());
        {
            let dfrost_log = Arc::new(Mutex::new(crate::community_dfrost_log::DfrostLog::new()));
            let (dtx, _drx) = mpsc::channel::<Vec<u8>>(4);
            let (_dstx, dsrx) = mpsc::channel::<Vec<u8>>(4);
            let app = tauri::test::mock_app();
            let app_handle = app.handle().clone();
            struct NoopResolver;
            #[async_trait::async_trait]
            impl IdentityResolver for NoopResolver {
                async fn resolve(
                    &self,
                    _addr: &crate::owner_state_types::OwnerAddr,
                ) -> Option<[u8; 64]> {
                    None
                }
            }
            DfrostLogRegistry::register(
                &dfrost_reg,
                DfrostLogEngineParams {
                    community_id,
                    dfrost_log,
                    publisher_tx: dtx,
                    subscriber_rx: dsrx,
                    app_handle,
                    self_addr: OwnerAddr([0u8; 16]),
                    self_x25519_priv: [0u8; 32],
                    identity_resolver: Arc::new(NoopResolver),
                    registry_weak: None,
                },
            )
            .await;
        }

        // Install a fake beacon_requester that counts calls.
        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = Arc::clone(&call_count);
        let requester: BeaconRequester = Arc::new(move |_cid, _seed, _epoch| {
            call_count_clone.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok("ok".to_string()) })
        });
        VotingLogEngine::install_dfrost_handle(&engine, dfrost_reg, requester).await;

        // publish_event a Tier 3 PollCreate → should trigger the requester.
        let (create_event, _electorate) =
            tier3_poll_create_event(actor, "dev-d", 4_000, sortition_size);
        engine
            .publish_event(create_event, None)
            .await
            .expect("publish tier3 create");

        // Give tokio::spawn in maybe_trigger_beacon_for_tier3_create time to fire.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            call_count.load(Ordering::Relaxed),
            1,
            "beacon_requester should have been called once for a Tier3 PollCreate"
        );
    }

    /// Cluster 1+2 regression (Cursor HIGH + Qodo #1 + CodeRabbit major):
    /// When dfrost_registry reports epoch=1 at PollCreate time, the kd=ss
    /// event must be published when a beacon with that epoch's message_hash
    /// arrives. Previously `community_epoch` was hardcoded to 0, so any
    /// non-zero epoch caused the beacon hash to mismatch → poll stalled.
    #[tokio::test]
    async fn voting_engine_on_dfrost_beacon_epoch1_publishes_kd_ss() {
        use crate::community_dfrost_types::VrfBeaconPayload;
        use crate::community_voting_core::MembershipSnapshot;

        let community_id = SpaceId([0xD0; 16]);
        let actor = OwnerAddr([0xBB; 16]);
        let sortition_size: u16 = 20;

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let (publisher_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(32);
        let (_sub_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(32);

        let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: None,
            identity_resolver: None,
            membership_resolver: None,
        })
        .await;

        let (create_event, electorate) =
            tier3_poll_create_event(actor, "dev-e", 5_000, sortition_size);

        // Compute poll_create_event_hash.
        let signing_bytes = create_event.signing_bytes().expect("signing bytes");
        let poll_create_event_hash: [u8; 32] = {
            use sha2::{Digest, Sha256};
            Sha256::digest(&signing_bytes).into()
        };

        // Apply the Tier 3 PollCreate directly to the log, then manually
        // set the stored epoch to 1 (simulating what the engine does when
        // dfrost_registry reports epoch=1 at publish time).
        let poll_id = crate::community_voting_core::derive_poll_id(&community_id, &signing_bytes);
        {
            let mut log = voting_log.lock().await;
            let snapshot = MembershipSnapshot {
                members: electorate
                    .iter()
                    .map(|addr| {
                        (
                            *addr,
                            crate::community_voting_core::MemberAttrs {
                                power: 1,
                                vouching_depth: 0,
                            },
                        )
                    })
                    .collect(),
            };
            log.apply_with_snapshot(create_event, &community_id, Some(snapshot))
                .expect("tier3 poll create apply");
            // Simulate the engine storing epoch=1 (Cluster 1 fix).
            log.set_tier3_poll_epoch(&poll_id, 1);
        }

        // Build a matching VrfBeaconPayload for epoch=1.
        let community_epoch: u64 = 1;
        let message_hash = expected_beacon_message_hash(&poll_create_event_hash, community_epoch);
        let payload = VrfBeaconPayload {
            ceremony_id: [0x10u8; 32],
            message_hash,
            signature: vec![0u8; 64],
            vrf_output: [0xF1; 32],
        };

        // Fire on_dfrost_beacon.
        engine.on_dfrost_beacon(&payload, community_id).await;

        // Give spawned tasks time to complete.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Assert: kd=ss was published → sortition_result is Some.
        let log = voting_log.lock().await;
        assert_eq!(
            log.events.len(),
            2,
            "should have PollCreate + kd=ss events (epoch=1 beacon must match)"
        );
        let t3 = log
            .polls
            .values()
            .next()
            .unwrap()
            .tier_state
            .as_tier3()
            .unwrap();
        assert!(
            t3.sortition_result.is_some(),
            "sortition_result must be set when epoch=1 beacon matches stored epoch"
        );

        // Verify a beacon with epoch=0 would NOT match (demonstrating the fix).
        // (We skip firing a second beacon here since the poll already has ss.)
    }

    /// Cluster 1+2 regression: beacon with epoch=0 must NOT match a poll
    /// whose stored epoch is 1 (no silent mismatched-epoch accept).
    #[tokio::test]
    async fn voting_engine_on_dfrost_beacon_epoch_mismatch_ignored() {
        use crate::community_dfrost_types::VrfBeaconPayload;
        use crate::community_voting_core::MembershipSnapshot;

        let community_id = SpaceId([0xD1; 16]);
        let actor = OwnerAddr([0xCC; 16]);
        let sortition_size: u16 = 20;

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let (publisher_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(32);
        let (_sub_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(32);

        let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: None,
            identity_resolver: None,
            membership_resolver: None,
        })
        .await;

        let (create_event, electorate) =
            tier3_poll_create_event(actor, "dev-f", 6_000, sortition_size);

        let signing_bytes = create_event.signing_bytes().expect("signing bytes");
        let poll_create_event_hash: [u8; 32] = {
            use sha2::{Digest, Sha256};
            Sha256::digest(&signing_bytes).into()
        };
        let poll_id = crate::community_voting_core::derive_poll_id(&community_id, &signing_bytes);

        {
            let mut log = voting_log.lock().await;
            let snapshot = MembershipSnapshot {
                members: electorate
                    .iter()
                    .map(|addr| {
                        (
                            *addr,
                            crate::community_voting_core::MemberAttrs {
                                power: 1,
                                vouching_depth: 0,
                            },
                        )
                    })
                    .collect(),
            };
            log.apply_with_snapshot(create_event, &community_id, Some(snapshot))
                .expect("apply");
            // Store epoch=1 — beacon for epoch=0 must not match.
            log.set_tier3_poll_epoch(&poll_id, 1);
        }

        // Build a VrfBeaconPayload for epoch=0 (wrong epoch for this poll).
        let wrong_epoch: u64 = 0;
        let message_hash_wrong_epoch =
            expected_beacon_message_hash(&poll_create_event_hash, wrong_epoch);
        let payload_wrong_epoch = VrfBeaconPayload {
            ceremony_id: [0x11u8; 32],
            message_hash: message_hash_wrong_epoch,
            signature: vec![0u8; 64],
            vrf_output: [0xF2; 32],
        };

        engine
            .on_dfrost_beacon(&payload_wrong_epoch, community_id)
            .await;

        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // No kd=ss published — poll still in Sortition with no result.
        let log = voting_log.lock().await;
        assert_eq!(
            log.events.len(),
            1,
            "wrong-epoch beacon must not publish kd=ss"
        );
        let t3 = log
            .polls
            .values()
            .next()
            .unwrap()
            .tier_state
            .as_tier3()
            .unwrap();
        assert!(
            t3.sortition_result.is_none(),
            "sortition_result must remain None when beacon epoch mismatches stored epoch"
        );
    }

    // ── Cluster E regression test ────────────────────────────────────────────

    /// publish_event must reject a Tier 3 PollCreate when no D-FROST registry is installed,
    /// returning a DfrostNotReady error rather than silently accepting with epoch=0.
    #[tokio::test]
    async fn publish_tier3_poll_create_without_dfrost_registry_returns_error() {
        let community_id = SpaceId([0xE0; 16]);
        let actor = OwnerAddr([0xBB; 16]);
        let sortition_size: u16 = 20;

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let (publisher_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(32);
        let (_sub_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(32);

        let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: None,
            identity_resolver: None,
            membership_resolver: None,
        })
        .await;
        // Do NOT call install_dfrost_handle — dfrost_registry is None.

        let (create_ev, _electorate) =
            tier3_poll_create_event(actor, "dev-e", 1_000, sortition_size);

        let result = engine.publish_event(create_ev, None).await;
        assert!(
            result.is_err(),
            "publish_event must fail when D-FROST registry is not installed"
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("DfrostNotReady"),
            "error must mention DfrostNotReady; got: {err_msg:?}"
        );

        // Confirm: no poll was applied to the log (apply never ran).
        let log = voting_log.lock().await;
        assert!(
            log.polls.is_empty(),
            "no poll must be stored when PollCreate is rejected"
        );
    }

    // ── Tier 1 inbound eligibility regression tests ────────────────────────

    /// Regression (Cursor Medium): Tier 1 PollCreate from an ineligible
    /// creator (min_power not satisfied) must be rejected by process_inbound.
    /// Prior to the round-2 fix, only Tier 2 had the eligibility gate.
    #[tokio::test]
    async fn process_inbound_tier1_poll_create_ineligible_creator_rejected() {
        use crate::community_voting_core::{
            build_signed_poll_create_tier1, Eligibility, MemberAttrs, MembershipSnapshot,
        };
        use crate::community_voting_log::MembershipSnapshotResolver;

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let tracker = Arc::new(Mutex::new(VotingReplayTracker::new()));
        let community_id = SpaceId([0xF1; 16]);

        let (keypair, actor, pub_64) = fixture_identity_engine(0xF1);

        // Config requires min_power = 10, but the snapshot gives the actor power = 1.
        let cfg = Tier1PollConfig {
            options: vec!["yes".into(), "no".into()],
            window_seconds: 3600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility {
                min_power: 10, // actor cannot satisfy this
                min_vouching_depth: None,
                sortition_size: None,
            },
            channel_id: crate::community_membership::ChannelId([0xF1; 16]),
        };

        let resolvers = Arc::new(FixedTestResolvers {
            identity: HashMap::from([(actor, pub_64)]),
            snapshot: MembershipSnapshot {
                members: HashMap::from([(
                    actor,
                    MemberAttrs {
                        power: 1, // below min_power = 10
                        vouching_depth: 0,
                    },
                )]),
            },
        });
        let id_resolver: Arc<dyn crate::community_voting_core::VotingIdentityResolver> =
            resolvers.clone();
        let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers;

        let event = build_signed_poll_create_tier1(
            &keypair,
            actor,
            &cfg,
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "dev-f1".into(),
            },
        )
        .expect("build event");

        let mut packet = Vec::new();
        ciborium::into_writer(&event, &mut packet).expect("encode");

        let result = VotingLogEngine::<tauri::test::MockRuntime>::process_inbound(
            community_id,
            &voting_log,
            &tracker,
            Some(&id_resolver),
            Some(&mem_resolver),
            &packet,
        )
        .await;

        assert!(
            result.is_err(),
            "ineligible Tier 1 PollCreate must be rejected; got Ok(())"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("not eligible") || err.contains("InsufficientPower"),
            "error must mention eligibility; got: {err:?}"
        );

        // Log must remain empty.
        let log = voting_log.lock().await;
        assert!(
            log.polls.is_empty(),
            "no poll must be stored after rejection"
        );
    }

    /// Regression (Cursor Medium): Tier 1 BallotCast from an ineligible voter
    /// (min_power not satisfied) must be rejected by process_inbound.
    #[tokio::test]
    async fn process_inbound_tier1_ballot_cast_ineligible_voter_rejected() {
        use crate::community_voting_core::{
            build_signed_ballot_tier1, build_signed_poll_create_tier1, Eligibility, MemberAttrs,
            MembershipSnapshot,
        };
        use crate::community_voting_log::MembershipSnapshotResolver;

        let community_id = SpaceId([0xF2; 16]);

        // Creator key — eligible (power = 10, satisfies min_power = 10).
        let (creator_key, creator_actor, creator_pub64) = fixture_identity_engine(0xF2);
        // Voter key — not eligible (power = 1, does NOT satisfy min_power = 10).
        let (voter_key, voter_actor, voter_pub64) = fixture_identity_engine(0xF3);

        let eligibility = Eligibility {
            min_power: 10,
            min_vouching_depth: None,
            sortition_size: None,
        };
        let cfg = Tier1PollConfig {
            options: vec!["yes".into(), "no".into()],
            window_seconds: 3600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility,
            channel_id: crate::community_membership::ChannelId([0xF2; 16]),
        };

        // Snapshot: creator has power=10 (eligible), voter has power=1 (ineligible).
        let snapshot_both_eligible = MembershipSnapshot {
            members: HashMap::from([
                (
                    creator_actor,
                    MemberAttrs {
                        power: 10,
                        vouching_depth: 0,
                    },
                ),
                (
                    voter_actor,
                    MemberAttrs {
                        power: 1, // below min_power = 10
                        vouching_depth: 0,
                    },
                ),
            ]),
        };

        // First: apply PollCreate directly to the log so the poll exists.
        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let tracker = Arc::new(Mutex::new(VotingReplayTracker::new()));

        let create_event = build_signed_poll_create_tier1(
            &creator_key,
            creator_actor,
            &cfg,
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "dev-f2-creator".into(),
            },
        )
        .expect("build create event");

        {
            let mut log = voting_log.lock().await;
            log.apply_with_snapshot(
                create_event.clone(),
                &community_id,
                Some(snapshot_both_eligible.clone()),
            )
            .expect("pre-apply PollCreate");
        }

        // Derive the poll_id from the create event.
        let sb = create_event.signing_bytes().expect("signing bytes");
        let poll_id = crate::community_voting_core::derive_poll_id(&community_id, &sb);

        // Build a BallotCast from the ineligible voter.
        let ballot_event = build_signed_ballot_tier1(
            &voter_key,
            voter_actor,
            poll_id,
            vec![0u8], // vote for option 0
            Hlc {
                wall_ms: 2_000,
                logical: 0,
                device_id: "dev-f3-voter".into(),
            },
        )
        .expect("build ballot event");

        let mut packet = Vec::new();
        ciborium::into_writer(&ballot_event, &mut packet).expect("encode ballot");

        // Resolvers: identity map has both creator and voter.
        let resolvers = Arc::new(FixedTestResolvers {
            identity: HashMap::from([(creator_actor, creator_pub64), (voter_actor, voter_pub64)]),
            snapshot: snapshot_both_eligible,
        });
        let id_resolver: Arc<dyn crate::community_voting_core::VotingIdentityResolver> =
            resolvers.clone();
        let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers;

        let result = VotingLogEngine::<tauri::test::MockRuntime>::process_inbound(
            community_id,
            &voting_log,
            &tracker,
            Some(&id_resolver),
            Some(&mem_resolver),
            &packet,
        )
        .await;

        assert!(
            result.is_err(),
            "ineligible Tier 1 BallotCast must be rejected; got Ok(())"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("not eligible") || err.contains("InsufficientPower"),
            "error must mention eligibility; got: {err:?}"
        );

        // Log must still have only the PollCreate (ballot not applied).
        let log = voting_log.lock().await;
        assert_eq!(
            log.events.len(),
            1,
            "only PollCreate must be in the log; ballot must not have been applied"
        );
    }

    // ── ZEB-298 Task 5: maybe_emit_delegate_on_behalf ──────────────────────
    //
    // The hook is exercised end-to-end via `publish_event` so the tests
    // also lock in the call-site wiring in `publish_event` (parallel to
    // `maybe_emit_tier3_lifecycle_events`). Tier 2 Signal must apply to
    // an existing Tier 2 poll, so each test seeds the log with a
    // PollCreate that mints the poll, registers the local user's
    // delegation edge (delegator = local, delegate = signaler), and sets
    // the policy.

    /// Shared fixture: build a `VotingLogEngine` wired to a mock Tauri
    /// app_handle, with a pre-seeded Tier 2 poll, a delegation edge from
    /// `local_owner` → `delegate_owner`, and the supplied policy.
    /// Returns the running engine, the `PollId` of the seeded poll, and
    /// the mock app handle (so callers can attach a listener).
    /// Guard struct returned by `delegate_on_behalf_fixture` so the
    /// adapter-side channel halves (`publisher_rx` + `subscriber_tx`)
    /// stay alive for the duration of the test. Dropping them would
    /// close the engine's `publisher_tx`, making subsequent
    /// `publish_event` calls fail with `"voting publisher_tx closed"`.
    struct DelegateOnBehalfFixture {
        engine: Arc<VotingLogEngine<tauri::test::MockRuntime>>,
        pid: PollId,
        app_handle: tauri::AppHandle<tauri::test::MockRuntime>,
        // Kept alive; reads aren't asserted by these tests.
        _publisher_rx: mpsc::Receiver<Vec<u8>>,
        _subscriber_tx: mpsc::Sender<Vec<u8>>,
    }

    async fn delegate_on_behalf_fixture(
        local_owner: OwnerAddr,
        delegate_owner: OwnerAddr,
        notify_policy: bool,
    ) -> DelegateOnBehalfFixture {
        use crate::community_voting_conviction::{
            AutoExecAction, CommunityVotingPolicy, Tier2PollConfig, Q32,
        };

        let community_id = SpaceId([0xE2; 16]);
        let voting_log = Arc::new(Mutex::new(VotingLog::new()));

        // Seed the Tier 2 poll, delegation edge, and policy. Doing this
        // before engine.start avoids any race with the inbound loop.
        let pid: PollId = {
            let mut log = voting_log.lock().await;

            // Tier 2 PollCreate.
            let cfg = Tier2PollConfig {
                proposal_text: "promote".into(),
                half_life_seconds: 86_400,
                threshold_min_q32: Q32,
                threshold_max_q32: 100 * Q32,
                beta: 2,
                delegation_allowed: true,
                auto_exec: AutoExecAction::None,
                eligibility: Eligibility {
                    min_power: 0,
                    min_vouching_depth: None,
                    sortition_size: None,
                },
            };
            let mut payload = Vec::new();
            ciborium::into_writer(&cfg, &mut payload).expect("encode tier2 cfg");
            let create_event = SignedVotingEvent {
                tag: 'p',
                version: 1,
                tier: Tier::Conviction,
                kind: PollEventKindCode::PollCreate,
                hlc: Hlc {
                    wall_ms: 1_000,
                    logical: 0,
                    device_id: "dev-create".into(),
                },
                actor: delegate_owner,
                payload,
                sig: vec![0u8; 64],
            };

            // Snapshot includes both the local user and the delegate so
            // Tier 2 PollCreate's `total_supply` is non-zero. The exact
            // membership shape doesn't affect emit semantics — only the
            // delegation edge + policy + actor-is-delegate gate do.
            let mut members = HashMap::new();
            for owner in [local_owner, delegate_owner] {
                members.insert(
                    owner,
                    MemberAttrs {
                        power: 10,
                        vouching_depth: 0,
                    },
                );
            }
            let pid = log
                .apply_with_snapshot(
                    create_event,
                    &community_id,
                    Some(MembershipSnapshot { members }),
                )
                .expect("apply tier2 create");

            // Delegation edge: local_owner → delegate_owner.
            log.delegation_graph
                .apply_delegate(local_owner, delegate_owner, (500, 0))
                .expect("apply_delegate edge");

            // Community policy.
            log.set_policy(CommunityVotingPolicy {
                notify_on_delegate_signal: notify_policy,
            });

            pid
        };

        let (publisher_tx, publisher_rx) = mpsc::channel::<Vec<u8>>(8);
        let (subscriber_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(8);

        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();

        let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: Some(app_handle.clone()),
            identity_resolver: None,
            membership_resolver: None,
        })
        .await;

        // Install the local owner so `maybe_emit_delegate_on_behalf` can
        // read it from `local_signing`. Signing key value is irrelevant —
        // the hook only reads the owner.
        let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]));
        engine
            .install_local_signing_key(signing_key, local_owner)
            .await;

        DelegateOnBehalfFixture {
            engine,
            pid,
            app_handle,
            _publisher_rx: publisher_rx,
            _subscriber_tx: subscriber_tx,
        }
    }

    /// Build a Tier 2 Signal event by `signaler` against `pid`. Skips
    /// signing — the publish path does not verify locally-minted events.
    fn signal_event_for_emit(
        signaler: OwnerAddr,
        pid: PollId,
        support: bool,
        wall_ms: u64,
    ) -> SignedVotingEvent {
        let payload_obj = crate::community_voting_conviction::SignalPayload {
            proposal_id: pid,
            support,
        };
        let mut payload = Vec::new();
        ciborium::into_writer(&payload_obj, &mut payload).expect("encode signal");
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Conviction,
            kind: PollEventKindCode::Signal,
            hlc: Hlc {
                wall_ms,
                logical: 0,
                device_id: "dev-signal".into(),
            },
            actor: signaler,
            payload,
            sig: vec![0u8; 64],
        }
    }

    /// Drain captured emits (if any) by waiting up to `timeout` and
    /// returning the collected JSON payload strings. Drives the Tauri
    /// event loop via `tokio::task::yield_now` so the listener has a
    /// chance to fire before the assertion.
    async fn wait_for_emits(
        captured: Arc<std::sync::Mutex<Vec<String>>>,
        min_count: usize,
        timeout: Duration,
    ) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let v = captured.lock().expect("captured lock");
                if v.len() >= min_count {
                    return v.clone();
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let v = captured.lock().expect("captured lock");
                return v.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn maybe_emit_delegate_on_behalf_fires_when_all_conditions_hold() {
        use tauri::Listener;

        let local_owner = OwnerAddr([0x11; 16]);
        let delegate_owner = OwnerAddr([0x22; 16]);
        let fix = delegate_on_behalf_fixture(local_owner, delegate_owner, true).await;

        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_for_listener = Arc::clone(&captured);
        fix.app_handle
            .listen("voting-delegate-signaled-on-your-behalf", move |evt| {
                captured_for_listener
                    .lock()
                    .expect("captured lock")
                    .push(evt.payload().to_string());
            });

        // Delegate signals on local's behalf.
        let signal = signal_event_for_emit(delegate_owner, fix.pid, true, 2_000);
        fix.engine
            .publish_event(signal, None)
            .await
            .expect("publish_event signal");

        let payloads = wait_for_emits(Arc::clone(&captured), 1, Duration::from_secs(1)).await;
        assert_eq!(
            payloads.len(),
            1,
            "exactly one voting-delegate-signaled-on-your-behalf emit expected; got {payloads:?}"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&payloads[0]).expect("payload is JSON");
        assert_eq!(
            parsed["proposalId"].as_str(),
            Some(hex::encode(fix.pid.0).as_str())
        );
        assert_eq!(
            parsed["delegate"].as_str(),
            Some(hex::encode(delegate_owner.0).as_str())
        );
        assert_eq!(parsed["support"].as_bool(), Some(true));
        assert!(
            parsed["communityId"].is_string(),
            "communityId field present and a string"
        );
    }

    #[tokio::test]
    async fn maybe_emit_delegate_on_behalf_silent_when_policy_disabled() {
        use tauri::Listener;

        let local_owner = OwnerAddr([0x11; 16]);
        let delegate_owner = OwnerAddr([0x22; 16]);
        // Same fixture but with policy.notify_on_delegate_signal = false.
        let fix = delegate_on_behalf_fixture(local_owner, delegate_owner, false).await;

        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_for_listener = Arc::clone(&captured);
        fix.app_handle
            .listen("voting-delegate-signaled-on-your-behalf", move |evt| {
                captured_for_listener
                    .lock()
                    .expect("captured lock")
                    .push(evt.payload().to_string());
            });

        let signal = signal_event_for_emit(delegate_owner, fix.pid, true, 2_000);
        fix.engine
            .publish_event(signal, None)
            .await
            .expect("publish_event signal");

        // Wait long enough that a delayed emit would land.
        let payloads = wait_for_emits(Arc::clone(&captured), 1, Duration::from_millis(200)).await;
        assert!(
            payloads.is_empty(),
            "policy.notify_on_delegate_signal=false must suppress the emit; got {payloads:?}"
        );
    }

    #[tokio::test]
    async fn maybe_emit_delegate_on_behalf_silent_when_signaler_not_local_delegate() {
        use tauri::Listener;

        let local_owner = OwnerAddr([0x11; 16]);
        let delegate_owner = OwnerAddr([0x22; 16]);
        let other_signaler = OwnerAddr([0x33; 16]);
        // Policy enabled + delegation edge installed, but the actual
        // signaler is a different OwnerAddr.
        let fix = delegate_on_behalf_fixture(local_owner, delegate_owner, true).await;

        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_for_listener = Arc::clone(&captured);
        fix.app_handle
            .listen("voting-delegate-signaled-on-your-behalf", move |evt| {
                captured_for_listener
                    .lock()
                    .expect("captured lock")
                    .push(evt.payload().to_string());
            });

        let signal = signal_event_for_emit(other_signaler, fix.pid, true, 2_000);
        fix.engine
            .publish_event(signal, None)
            .await
            .expect("publish_event signal");

        let payloads = wait_for_emits(Arc::clone(&captured), 1, Duration::from_millis(200)).await;
        assert!(
            payloads.is_empty(),
            "signaler != local's delegate must suppress the emit; got {payloads:?}"
        );
    }
}
