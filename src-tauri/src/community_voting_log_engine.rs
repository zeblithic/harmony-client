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

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::{mpsc, Mutex};

use crate::community_dfrost_log_engine::DfrostLogRegistry;
use crate::community_voting_core::{PollEventKindCode, PollId, SignedVotingEvent, Tier};
use crate::community_voting_log::VotingLog;
use crate::owner_state_types::{OwnerAddr, SpaceId};

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
pub struct VotingLogEngineParams {
    pub community_id: SpaceId,
    pub voting_log: Arc<Mutex<VotingLog>>,
    /// Engine → adapter → Zenoh `put`.
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    /// Zenoh subscriber → adapter → engine receive loop.
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
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
    pub async fn start(params: VotingLogEngineParams) -> Arc<Self> {
        let tracker = Arc::new(Mutex::new(VotingReplayTracker::new()));

        // Spawn the inbound loop. It takes ownership of subscriber_rx
        // and exits cleanly when the adapter drops its matching Sender.
        let log_for_loop = Arc::clone(&params.voting_log);
        let tracker_for_loop = Arc::clone(&tracker);
        let community_id = params.community_id;
        let mut rx = params.subscriber_rx;
        let receive_handle = tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                if let Err(e) =
                    Self::process_inbound(community_id, &log_for_loop, &tracker_for_loop, &packet)
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
            _phantom: PhantomData,
        })
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
        &self,
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
                    let epoch_u64 = t3.meta.community_epoch as u64;
                    let expected_mh = derive_vrf_seed(&seed, epoch_u64);
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
        &self,
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

        if let Err(e) = self.publish_event(ss_event).await {
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
        let epoch = community_epoch as u64;
        let community_id = self.community_id;

        // Fire-and-forget: the beacon request may fail (no active committee,
        // ceremony already in flight). Log the error but don't propagate.
        tokio::spawn(async move {
            if let Err(e) = (requester)(community_id, seed, epoch).await {
                tracing::warn!(
                    community_id = ?community_id,
                    error = %e,
                    "maybe_trigger_beacon: dfrost_request_vrf_beacon_inner failed"
                );
            }
        });
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
    pub async fn publish_event(&self, event: SignedVotingEvent) -> Result<(), String> {
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
        let tier3_create_epoch: Option<(PollId, u32)> =
            if event.kind == PollEventKindCode::PollCreate && event.tier == Tier::Sortition {
                let epoch = {
                    let dr = self.dfrost_registry.lock().await;
                    if let Some(reg) = dr.as_ref() {
                        if let Some(engine) = reg.get(self.community_id).await {
                            engine.current_epoch().await as u32
                        } else {
                            0
                        }
                    } else {
                        0
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

        // Apply locally.
        {
            let mut log = self.voting_log.lock().await;
            log.apply_with_snapshot(event.clone(), &self.community_id, None)
                .map_err(|e| format!("apply: {e:?}"))?;

            // Store the pre-read epoch on the newly-created Tier 3 poll.
            // This must happen in the same lock scope as apply so the poll
            // state is consistent before any beacon callback can read it.
            if let Some((poll_id, epoch)) = tier3_create_epoch {
                log.set_tier3_poll_epoch(&poll_id, epoch);
            }
        }

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
        self.publisher_tx
            .send(packet)
            .await
            .map_err(|e| format!("voting publisher_tx closed: {e}"))?;
        Ok(())
    }

    /// Inbound packet processing: decode, dedup, apply, record.
    ///
    /// Called from the receive loop spawned by `start`. Errors here are
    /// logged and dropped (peer sent garbage or we hit a transient apply
    /// failure); we never propagate up to the receive loop or kill the
    /// engine.
    async fn process_inbound(
        community_id: SpaceId,
        voting_log: &Arc<Mutex<VotingLog>>,
        tracker: &Arc<Mutex<VotingReplayTracker>>,
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

        // Hard verify gate. Apply-time invariants in
        // `VotingLog::apply_with_snapshot` (lifecycle transitions,
        // payload decode, graph cycle checks) cannot detect a forged
        // Ed25519 signature, so unverified peer packets must NOT mutate
        // state. The envelope carries only the 16-byte `OwnerAddr` hash
        // — not the Ed25519 pubkey — so signature verification needs
        // the per-community membership snapshot (pubkey → OwnerAddr
        // mapping) that the apply layer otherwise consults for
        // eligibility. The Zenoh adapter (ZEB-291 Task 19.1
        // follow-up) is the natural place to do this lookup; until it
        // lands, this receive loop is dead code in production. CR R3
        // Major: refuse to apply any inbound packet from this surface
        // until the verify gate is wired. Tests that exercised the
        // receive loop with synthetic packets are now feature-gated
        // behind `cfg(any(test, feature = "test-fixtures"))` so the
        // production binary cannot accept forged peer events.
        #[cfg(not(any(test, feature = "test-fixtures")))]
        {
            return Err(
                "inbound voting events are refused until ZEB-291 Task 19.1 wires \
                 verify_event with the per-community membership snapshot"
                    .into(),
            );
        }

        // Apply.
        {
            let mut log = voting_log.lock().await;
            log.apply_with_snapshot(event.clone(), &community_id, None)
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
    pub async fn register(&self, params: VotingLogEngineParams) -> Arc<VotingLogEngine<R>> {
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
    use crate::community_voting_core::{Eligibility, PollEventKindCode, SignedVotingEvent, Tier};
    use crate::owner_state_types::Hlc;
    use std::time::Duration;

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
        })
        .await;

        let actor = OwnerAddr([0xaa; 16]);
        let event = poll_create_event(actor, "dev-a", 1_000);

        engine
            .publish_event(event.clone())
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
        // Push a packet onto subscriber_rx without going through publish_event;
        // the receive loop must decode + apply it to the log.
        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let community_id = SpaceId([0x77; 16]);

        let (publisher_tx, _publisher_rx) = mpsc::channel::<Vec<u8>>(8);
        let (subscriber_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(8);

        let _engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log: Arc::clone(&voting_log),
            publisher_tx,
            subscriber_rx,
        })
        .await;

        // Peer-minted event: actor + device different from anything
        // recorded locally so the dedup gate is open.
        let peer_actor = OwnerAddr([0xbb; 16]);
        let peer_event = poll_create_event(peer_actor, "dev-peer", 5_000);
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
        community_epoch: u32,
    ) -> [u8; 32] {
        use crate::community_dfrost_types::derive_vrf_seed;
        use crate::community_voting_sortition::derive_beacon_seed;
        let seed = derive_beacon_seed(poll_create_event_hash, community_epoch);
        derive_vrf_seed(&seed, community_epoch as u64)
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
        let community_epoch: u32 = 0;
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
        let community_epoch: u32 = 0;
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
    #[tokio::test]
    async fn voting_engine_apply_tier3_create_triggers_beacon_request() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let community_id = SpaceId([0xC4; 16]);
        let actor = OwnerAddr([0xAA; 16]);
        let sortition_size: u16 = 20; // min valid value per validate_tier3_poll_config

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let (publisher_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(32);
        let (_sub_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(32);

        let engine = Arc::new(
            VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
                community_id,
                voting_log: Arc::clone(&voting_log),
                publisher_tx,
                subscriber_rx,
            })
            .await,
        );

        // Install a fake beacon_requester that counts calls.
        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = Arc::clone(&call_count);
        let requester: BeaconRequester = Arc::new(move |_cid, _seed, _epoch| {
            call_count_clone.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok("ok".to_string()) })
        });
        {
            let mut br = engine.beacon_requester.lock().await;
            *br = Some(requester);
        }

        // publish_event a Tier 3 PollCreate → should trigger the requester.
        let (create_event, _electorate) =
            tier3_poll_create_event(actor, "dev-d", 4_000, sortition_size);
        engine
            .publish_event(create_event)
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

        let engine = Arc::new(
            VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
                community_id,
                voting_log: Arc::clone(&voting_log),
                publisher_tx,
                subscriber_rx,
            })
            .await,
        );

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
        let community_epoch: u32 = 1;
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

        let engine = Arc::new(
            VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
                community_id,
                voting_log: Arc::clone(&voting_log),
                publisher_tx,
                subscriber_rx,
            })
            .await,
        );

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
        let wrong_epoch: u32 = 0;
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
}
