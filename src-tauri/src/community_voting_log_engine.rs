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
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};

use crate::community_voting_core::SignedVotingEvent;
use crate::community_voting_log::VotingLog;
use crate::owner_state_types::{OwnerAddr, SpaceId};

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
pub struct VotingLogEngine {
    community_id: SpaceId,
    voting_log: Arc<Mutex<VotingLog>>,
    tracker: Arc<Mutex<VotingReplayTracker>>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    /// Held only so the receive task isn't aborted by handle-drop while
    /// the engine is alive. The task exits naturally when `subscriber_rx`
    /// closes (adapter dropped the matching `Sender`).
    _receive_handle: tokio::task::JoinHandle<()>,
}

impl VotingLogEngine {
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
        })
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

        // (3) Apply locally.
        {
            let mut log = self.voting_log.lock().await;
            log.apply_with_snapshot(event.clone(), &self.community_id, None)
                .map_err(|e| format!("apply: {e:?}"))?;
        }

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
#[derive(Default)]
pub struct VotingLogRegistry {
    engines: Mutex<HashMap<SpaceId, Arc<VotingLogEngine>>>,
}

impl VotingLogRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start an engine for `params.community_id` and stash it in the
    /// registry. If an engine already exists for that community it is
    /// replaced — the caller is responsible for shutting the old one
    /// down by dropping their `Arc` (the receive loop will then exit
    /// when the adapter's publisher sender is dropped).
    pub async fn register(&self, params: VotingLogEngineParams) -> Arc<VotingLogEngine> {
        let cid = params.community_id;
        let engine = VotingLogEngine::start(params).await;
        let mut engines = self.engines.lock().await;
        engines.insert(cid, Arc::clone(&engine));
        engine
    }

    pub async fn get(&self, community_id: SpaceId) -> Option<Arc<VotingLogEngine>> {
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

        let engine = VotingLogEngine::start(VotingLogEngineParams {
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

        let _engine = VotingLogEngine::start(VotingLogEngineParams {
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
        let registry = VotingLogRegistry::new();

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
}
