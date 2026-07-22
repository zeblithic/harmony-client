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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::marker::PhantomData;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::{mpsc, Mutex, RwLock};

use tauri::{AppHandle, Emitter};

use crate::community_dfrost_log_engine::DfrostLogRegistry;
use crate::community_voting_core::{PollEventKindCode, PollId, SignedVotingEvent, Tier};
use crate::community_voting_log::VotingLog;
use crate::community_voting_tier3::{CommitteeOracle, CommitteePublicState};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

// ── ZEB-295 Phase 6 Task 8: production CommitteeOracle ──────────────────────

/// Production `CommitteeOracle` backed by a snapshot of the per-community
/// `DfrostLog` committee state at oracle-install time.
///
/// Phase 6 v1 caching rationale: the `CommitteeOracle` trait is sync (no
/// `async fn` — the apply path is sync), but the underlying dfrost log
/// lives behind a `tokio::sync::Mutex`. Rather than blocking the async
/// runtime via `blocking_lock` at every oracle query, we snapshot the
/// committee state once at install time (typically PollCreate apply,
/// after DKG has finalised) and serve queries from the cache. This is
/// correct as long as the committee doesn't rotate mid-poll — CHURP
/// rotation handling (multi-epoch snapshot tracking) is a v2 concern
/// per spec §5.2/§5.3. The current dfrost wiring stores only the
/// latest committee state in `committee_state` anyway, so multi-epoch
/// recovery is constrained to the snapshot epoch in Phase 6.
///
/// `None` returns (committee not yet active, requested epoch doesn't
/// match snapshot) flow through to the existing "silent drop on
/// missing prerequisite" convention in the apply path.
#[derive(Debug, Clone)]
pub struct DfrostLogCommitteeOracle {
    /// CHURP epoch this snapshot represents.
    pub epoch: u64,
    /// Joint verifying key Y = G · x (compressed Ristretto, 32 bytes).
    pub joint_verifying_key: [u8; 32],
    /// Per-member verifying shares Y_i = G · x_i.
    pub verifying_shares: BTreeMap<OwnerAddr, [u8; 32]>,
    /// FROST `min_signers` (= ElGamal threshold `t`).
    pub threshold: u16,
}

impl DfrostLogCommitteeOracle {
    /// Construct a fresh oracle by snapshotting the current committee state
    /// from a per-community `DfrostLogEngine`. Returns `None` if no DKG has
    /// completed yet (committee inactive).
    pub async fn from_dfrost_engine<R: tauri::Runtime>(
        engine: &crate::community_dfrost_log_engine::DfrostLogEngine<R>,
    ) -> Option<Self> {
        let epoch = engine.latest_committee_epoch().await?;
        let (joint_verifying_key, verifying_shares, threshold) =
            engine.committee_snapshot_at_epoch(epoch).await?;
        Some(Self {
            epoch,
            joint_verifying_key,
            verifying_shares,
            threshold,
        })
    }
}

impl CommitteeOracle for DfrostLogCommitteeOracle {
    fn committee_at_epoch(&self, epoch: u64) -> Option<CommitteePublicState> {
        // Phase 6 v1: snapshot stores ONE epoch. Queries for other epochs
        // return None — the recover_secret_tally path falls through to the
        // next epoch, eventually returning None overall if the requested
        // epoch isn't the snapshot epoch. See spec §5.3 multi-epoch
        // recovery (deferred to v2 with CHURP rotation event log).
        if epoch != self.epoch {
            return None;
        }
        Some(CommitteePublicState {
            epoch,
            joint_verifying_key: self.joint_verifying_key,
            verifying_shares: self.verifying_shares.clone(),
            threshold: self.threshold,
        })
    }
    fn latest_epoch(&self) -> Option<u64> {
        Some(self.epoch)
    }
}

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
/// Exact per-event coordinate: `(actor, device_id, wall_ms, logical)`.
/// Uniquely identifies one event position on a device lane (a device's
/// HLC is monotone, so it never signs two distinct events at the same
/// coordinate). Used by the ZEB-718 backfill apply path.
type VotingEventCoord = (OwnerAddr, String, u64, u32);

#[derive(Debug, Default)]
pub struct VotingReplayTracker {
    seen: HashMap<(OwnerAddr, String), (u64, u32)>,
    /// ZEB-718: every coordinate ever recorded (live or backfilled). The
    /// live path dedups via the per-lane high-water `seen` map (cheap,
    /// correct for monotone live delivery); the **backfill** path dedups
    /// via this exact-coordinate set instead, because a cross-rotation
    /// drop leaves an *in-lane gap* (a peer holds a later event `e2` on a
    /// lane but missed the earlier `e1`) — the high-water mark would
    /// wrongly swallow the backfilled `e1`. Grows one entry per distinct
    /// event; at voting volume (sparse, archive-bounded polls) this is
    /// negligible, and it is deliberately NOT pruned on archive so a
    /// re-broadcast archived event is not resurrected.
    seen_coords: HashSet<VotingEventCoord>,
}

impl VotingReplayTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn ordinal(event: &SignedVotingEvent) -> (u64, u32) {
        (event.hlc.wall_ms, event.hlc.logical)
    }

    fn coord(event: &SignedVotingEvent) -> VotingEventCoord {
        (
            event.actor,
            event.hlc.device_id.clone(),
            event.hlc.wall_ms,
            event.hlc.logical,
        )
    }

    /// Unconditionally bump the high-water mark for an event's lane AND
    /// record its exact coordinate (ZEB-718). Called by `publish_event`
    /// BEFORE the broadcast (the self-loopback fix from ZEB-270) and by
    /// `process_inbound` after a successful apply — so both live paths
    /// populate `seen_coords` for the backfill dedup for free.
    pub fn record(&mut self, event: &SignedVotingEvent) {
        let key = (event.actor, event.hlc.device_id.clone());
        let ord = Self::ordinal(event);
        let entry = self.seen.entry(key).or_insert((0u64, 0u32));
        if ord > *entry {
            *entry = ord;
        }
        self.seen_coords.insert(Self::coord(event));
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

    /// ZEB-718: returns true iff this **exact** event coordinate was
    /// already recorded. Unlike `contains`, this does NOT treat an
    /// older-than-high-water event as seen — so the backfill path
    /// recovers in-lane gaps a cross-rotation drop left behind.
    pub fn seen_coord(&self, event: &SignedVotingEvent) -> bool {
        self.seen_coords.contains(&Self::coord(event))
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
    ///
    /// ZEB-298 Task 8: wrapped in `Mutex<Option<...>>` so we can build
    /// the `Arc<Self>` first and then spawn the receive loop with
    /// `Arc::clone(&engine)`, giving the loop `self`-method access for
    /// the post-apply hooks (`maybe_trigger_engine_auto_orchestration`
    /// et al). Slotted in after spawn completes.
    _receive_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
    /// ZEB-718: install-once `identity_dir` for on-disk persistence.
    /// `None` until installed by the production `ensure_voting_engine_for`;
    /// while `None`, `persist_now` is a no-op (tests / lightweight
    /// harnesses that don't persist). `std::sync::Mutex` because the read
    /// is a quick clone with no `.await` held across the guard.
    persist_dir: std::sync::Mutex<Option<std::path::PathBuf>>,
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

    /// ZEB-718: install the `identity_dir` that `persist_now` writes
    /// under. Install-once from the production `ensure_voting_engine_for`;
    /// tests that don't persist simply never call it.
    pub fn install_persist_dir(&self, identity_dir: std::path::PathBuf) {
        if let Ok(mut g) = self.persist_dir.lock() {
            *g = Some(identity_dir);
        }
    }

    /// ZEB-718: snapshot `{events, policy}` and atomically rewrite
    /// `voting.cbor`. No-op when no `identity_dir` is installed. Never
    /// panics — a write failure is logged and swallowed (the in-memory
    /// log is authoritative and peer-recoverable via backfill). Called
    /// after every log mutation (publish, inbound apply, backfill apply,
    /// archive sweep, policy change).
    pub(crate) async fn persist_now(&self) {
        // Clone the path out from under the std mutex before any await.
        let dir = match self.persist_dir.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        let Some(dir) = dir else {
            return;
        };
        let path = crate::community_voting_persist::voting_path_for(&dir, &self.community_id);
        let community_id = self.community_id;
        // Clone a serde-clean snapshot, then run the blocking CBOR-encode +
        // atomic write on a `spawn_blocking` thread so `std::fs` never parks a
        // Tokio worker (repo persistence pattern — PRs #74/#380/#381).
        //
        // The `voting_log` lock is intentionally held ACROSS the write, not
        // released after the snapshot: `persist_now` runs concurrently from
        // three tasks (IPC publish / inbound receive loop / backfill apply)
        // and the 24h tick archive sweep writes the same `voting.cbor` — all
        // serialize on this one per-community mutex. Releasing before the
        // write would let two writers race on the fixed `.tmp` name and land
        // out of order (a stale snapshot renaming last → lost update). The
        // hold is a sub-ms clone+write on a sparse path, and the blocking I/O
        // is off-worker, so contention is negligible.
        let log = self.voting_log.lock().await;
        let snapshot = crate::community_voting_persist::snapshot_for_persist(&log, &community_id);
        let write_result = tokio::task::spawn_blocking(move || {
            crate::community_voting_persist::write_snapshot(&path, &snapshot)
        })
        .await;
        drop(log);
        let write_err = match write_result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(join_err) => Some(format!("persist task panicked: {join_err}")),
        };
        if let Some(err) = write_err {
            tracing::warn!(
                community_id = ?community_id,
                err = %err,
                "voting persist_now: failed to write voting.cbor \
                 (continuing; log is peer-recoverable via backfill)"
            );
        }
    }

    /// ZEB-718: plaintext `SignedVotingEvent` CBOR frames for the live
    /// (non-archived) voting log — one per event. Consumed by the backfill
    /// responder (in the Zenoh adapter), which re-encrypts each frame under
    /// the community's **current** epoch at serve time so it passes the
    /// requester's current-epoch cut. Archived events are already pruned
    /// from `log.events`, so this is naturally the "live polls only" set.
    pub(crate) async fn read_backfill_frames(&self) -> Vec<Vec<u8>> {
        let log = self.voting_log.lock().await;
        log.events
            .iter()
            .filter_map(|ev| {
                let mut buf = Vec::new();
                ciborium::into_writer(ev, &mut buf).ok().map(|()| buf)
            })
            .collect()
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

        // ZEB-298 Task 8: build the `Arc<Self>` BEFORE spawning the
        // receive loop so we can hand `Arc::clone(&engine)` to the loop.
        // This gives the inbound path `self`-method access (needed by the
        // four post-apply hooks fired by `process_inbound_dispatch`:
        // beacon, engine-auto orchestration, Tier 3 lifecycle emit, and
        // delegate-on-behalf emit). The `_receive_handle` slot stays
        // empty until the spawn completes; storing the handle later
        // closes the lifecycle loop.
        let community_id = params.community_id;
        let mut rx = params.subscriber_rx;
        let engine = Arc::new(Self {
            community_id,
            voting_log: Arc::clone(&params.voting_log),
            tracker: Arc::clone(&tracker),
            publisher_tx: params.publisher_tx,
            _receive_handle: Mutex::new(None),
            dfrost_registry: Mutex::new(None),
            beacon_requester: Mutex::new(None),
            hlc_tracker: params.hlc_tracker,
            device_id: params.device_id,
            app_handle: params.app_handle,
            local_signing: RwLock::new(None),
            identity_resolver: params.identity_resolver,
            membership_resolver: params.membership_resolver,
            persist_dir: std::sync::Mutex::new(None),
            _phantom: PhantomData,
        });

        // Spawn the inbound loop. It takes ownership of subscriber_rx
        // and exits cleanly when the adapter drops its matching Sender.
        // The loop holds an `Arc::clone` of the engine so each packet
        // is dispatched through `self.process_inbound_dispatch(&packet)`
        // which fans out to the post-apply hooks after a successful
        // apply.
        let receive_handle = {
            let me = Arc::clone(&engine);
            tokio::spawn(async move {
                while let Some(packet) = rx.recv().await {
                    if let Err(e) = me.process_inbound_dispatch(&packet).await {
                        tracing::warn!(
                            community_id = ?community_id,
                            err = ?e,
                            "voting engine inbound process failed"
                        );
                    }
                }
            })
        };
        *engine._receive_handle.lock().await = Some(receive_handle);

        engine
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
    async fn maybe_trigger_engine_auto_orchestration(
        self: &Arc<Self>,
        pid: &PollId,
        base_hlc: &Hlc,
    ) {
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
            // ZEB-316: deterministic HLC derived from the triggering event's
            // HLC (poll-derived lane, no wall-clock / device_id / tracker) so
            // every replica reacting to the same base produces a bit-identical
            // kd=sf event_hash.
            let hlc = engine_auto_hlc_from_base(base_hlc, pid, "sf");
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
            // ZEB-316: deterministic HLC derived from the triggering event's
            // HLC so the minted kd=cl is replica-identical — the kd=cl HLC is
            // in `signing_bytes`, so a deterministic HLC yields a byte-identical
            // `close_event_hash` across replicas (acceptance #1).
            //
            // I-1 scope: this is byte-identical across replicas only when the
            // SAME event first satisfies the trigger everywhere (the common
            // case — one kd=ss past the deadline). Under reordering a different
            // event may trip the deadline per replica → same lane, different
            // ordinal → divergent close_event_hash. That is benign: `result` +
            // terminal `stage` still converge via LWW + the terminal-state gate,
            // and close_event_hash is not in any cross-peer state-root. Do NOT
            // treat a peer close_event_hash mismatch as corruption.
            let hlc = engine_auto_hlc_from_base(base_hlc, pid, "cl");
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
        let trigger_kd_rs_result: Option<crate::community_voting_star::StarResult> = {
            let log = self.voting_log.lock().await;
            let state = match log.polls.get(pid) {
                Some(s) => s,
                None => return,
            };
            let t3 = match state.tier_state.as_tier3() {
                Some(t) => t,
                None => return,
            };
            // ZEB-295 Phase 6: this branch handles pu-mode only. se-mode polls
            // route through `maybe_emit_tier3_result_secret` below, which
            // gates on tally-share threshold rather than just close_event_hash.
            if t3.meta.config.privacy_mode != "pu"
                || t3.close_event_hash.is_none()
                || t3.result.is_some()
            {
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

        if let Some(result) = trigger_kd_rs_result {
            // ZEB-316 (qbug1 fix): pu-mode kd=rs mints on a WALL-CLOCK HLC
            // (`reserve_next_local_hlc`), NOT a deterministic close-anchored HLC.
            // This is the SAME revert already applied to se-mode kd=rs (see
            // `try_finalize_secret_tally`).
            //
            // Monotonicity: kd=rs must be minted ABOVE the current receive
            // watermark (`last_received_hlc`) or the apply-time monotonic gate
            // (`community_voting_tier3.rs` → `HlcNotMonotonic`) rejects it. A
            // close-anchored HLC — pinned forever to `(close.wall, close.logical+1)`
            // — can stall BELOW that watermark because the kd=cl→kd=rs cascade is
            // NOT serialized: the `voting_log` mutex is released per-apply, and the
            // post-apply hook re-fires after release with real yields at
            // `persist_now().await` (which holds the log lock across a
            // `spawn_blocking`) and `publisher_tx.send().await`. On the
            // multi-threaded runtime a concurrent IPC ballot-cast or backfill apply
            // can slip a higher-HLC event in between kd=cl and kd=rs, advancing
            // `last_received_hlc` past the close HLC. Because the close-anchored
            // HLC is frozen, EVERY re-mint (and every byte-identical peer copy) is
            // then rejected forever → the poll never finalizes via engine-auto on
            // that replica (per-replica stall + cross-replica divergence). A
            // freshly-reserved HLC is always ≥ the receive watermark, so it always
            // applies. (kd=cl is immune: it re-anchors on the moving close
            // watermark each hook re-fire, so it self-heals.)
            //
            // A deterministic HLC is unnecessary here anyway: the pu-mode RESULT
            // converges bit-identically across replicas via the deterministic
            // `StarResult` payload + the apply-time LWW/terminal-state gate that
            // keeps the first finalizing kd=rs and drops the rest — identical to
            // how se-mode converges.
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

        // ── ZEB-295 Phase 6 Task 8: kd=ts TallyShare orchestration ──────
        //
        // Fires when (privacy_mode == "se", kd=cl applied, we're a committee
        // member at the latest epoch, we haven't yet emitted for this
        // (actor, epoch)). Builds the per-aggregate partial decryption
        // shares + DLEQ proofs against the homomorphic ballot aggregate
        // and publishes the signed envelope so peers can collect ≥t
        // shares and Lagrange-combine the STAR plaintext.
        self.maybe_emit_tally_share(pid, &signing_key, self_owner)
            .await;

        // ── ZEB-295 Phase 6 Task 8: kd=rs (secret-mode) orchestration ───
        //
        // CodeRabbit PR #155 major: a node that is NOT in the committee
        // never publishes kd=ts itself, so the outbound-only invocation
        // (existing behavior) means peers can collect ≥t shares from
        // other nodes' kd=ts but never finalize the poll on their own
        // log. The inbound dispatch path also calls
        // `try_finalize_secret_tally` (see `process_inbound_dispatch`)
        // so any node that crosses the share threshold finalizes
        // independently — Lagrange invariance + canonical share-subset
        // selection guarantees bit-identical kd=rs envelopes across
        // replicas, so the LWW gate in apply cleanly resolves the race.
        self.try_finalize_secret_tally(pid, &signing_key, self_owner)
            .await;
    }

    /// ZEB-295 Phase 6 Task 8: emit a kd=ts TallyShare event after
    /// ratification close if this engine is a committee member at the
    /// latest epoch and hasn't yet emitted.
    ///
    /// Steps (after all gates pass):
    /// 1. Snapshot poll-state under lock (close + privacy_mode +
    ///    aggregate-input ballots + epoch + committee snapshot).
    /// 2. Resolve the local FROST `KeyPackage` from the per-community
    ///    `DfrostLogEngine` and derive the ElGamal decryption scalar
    ///    `x_i = signing_share_as_scalar(kp)`.
    /// 3. For each of the `n + C(n,2)` ballot aggregates: compute the
    ///    partial decryption share `d_i = c1_agg · x_i` and a
    ///    Chaum-Pedersen DLEQ proof `(G ↦ Y_i, c1_agg ↦ d_i)` so peers
    ///    can verify the share against the committee oracle without
    ///    learning `x_i`.
    /// 4. Wrap into `TallySharePayload` and publish via the recursive
    ///    `publish_event` path (mirrors all other engine-auto mints).
    ///
    /// Silent-bail on any missing prerequisite (no committee, no DKG
    /// key locally, aggregate decode fail). Re-runs cheaply on every
    /// apply via the already-emitted guard — late-arriving prerequisites
    /// (e.g. CHURP epoch refresh) get picked up on the next apply tick.
    /// Test seam: invoke `maybe_emit_tally_share` directly from
    /// integration tests + unit tests under `mod tests` below. The
    /// production hook is fired from `maybe_trigger_engine_auto_orchestration`
    /// which is itself only invoked through `publish_event` — driving
    /// the full publish path in a test would require seeding both a
    /// valid prior log and a kd=cl event that passes all apply gates.
    /// This thin pub wrapper preserves the private orchestration hook
    /// while letting tests assert the emission in isolation.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub async fn test_invoke_maybe_emit_tally_share(
        self: &Arc<Self>,
        pid: &PollId,
        signing_key: &Arc<ed25519_dalek::SigningKey>,
        self_owner: OwnerAddr,
    ) {
        self.maybe_emit_tally_share(pid, signing_key, self_owner)
            .await;
    }

    async fn maybe_emit_tally_share(
        self: &Arc<Self>,
        pid: &PollId,
        signing_key: &Arc<ed25519_dalek::SigningKey>,
        self_owner: OwnerAddr,
    ) {
        use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;

        // (1) Quick mode + state gates, plus snapshot of all apply-state
        // we need to compute shares. Drop the lock BEFORE the async
        // crypto work + dfrost engine acquire.
        //
        // Cursor PR #155 review-round-3 (F23): the previous
        // `tally_shares.contains_key(&(self_owner, latest_epoch))` early-
        // bail was wrong — it prevented re-emission when the homomorphic
        // aggregate changed (late-arriving ballots reorder by LWW, which
        // changes `c1_agg`, which invalidates the DLEQ proofs in our
        // previously-published share). C2's LWW upsert in apply means a
        // newer-HLC kd=ts from us OVERWRITES the stale one; so the
        // correct gate is "does our stored share still match what we'd
        // compute against the current aggregate?" We carry the stored
        // entry through the snapshot for post-key-fetch comparison.
        struct Snapshot {
            n: usize,
            aggregates: Vec<crate::community_voting_core::EncCiphertext>,
            committee_epoch: u64,
            y_i_compressed: [u8; 32],
            stored_first_share: Option<[u8; 32]>,
        }
        let snapshot: Option<Snapshot> = {
            let log = self.voting_log.lock().await;
            let state = match log.polls.get(pid) {
                Some(s) => s,
                None => return,
            };
            let t3 = match state.tier_state.as_tier3() {
                Some(t) => t,
                None => return,
            };
            if t3.meta.config.privacy_mode != "se" || t3.close_event_hash.is_none() {
                return;
            }
            let latest_epoch = match t3.committee_oracle.latest_epoch() {
                Some(e) => e,
                None => return,
            };
            // Committee membership + own verifying share.
            let cs = match t3.committee_oracle.committee_at_epoch(latest_epoch) {
                Some(c) => c,
                None => return,
            };
            let y_i_compressed = match cs.verifying_shares.get(&self_owner) {
                Some(b) => *b,
                None => return, // not on committee
            };
            // Capture the first share-entry we previously published at
            // this (actor, epoch), if any — used post-key-fetch to detect
            // unchanged-aggregate idempotency and skip the publish.
            let stored_first_share = t3
                .secret_tally
                .tally_shares
                .get(&(self_owner, latest_epoch))
                .and_then(|rec| rec.entries.first().map(|e| e.share));
            // Derive n via the canonical ratification ordering, matching
            // the apply path's n derivation (C2 fix). Pre-Drafting state
            // is silently skipped — kd=ts can only meaningfully emit
            // after kd=cl has been applied, which requires Ratification.
            let sq = crate::community_voting_tier3::synthesize_status_quo(&t3.meta.poll_id);
            let sq_hash = sq.event_hash;
            let mut all_candidates = t3.candidates.clone();
            all_candidates.push(sq);
            let primary_size = t3.meta.config.sortition_size as usize;
            let advancers = match crate::community_voting_tier3::drafting_advancers(
                &all_candidates,
                primary_size,
                sq_hash,
            ) {
                Some(a) => a,
                None => return,
            };
            let n = crate::community_voting_tier3::ratification_candidates_ordering(
                &advancers, sq_hash,
            )
            .len();
            if n == 0 {
                return;
            }
            let aggregates = match crate::community_voting_tier3::aggregate_se_ballots(
                &t3.ratification_ballots,
                n,
            ) {
                Some(a) => a,
                None => {
                    // No accepted ballots — nothing to decrypt. Future
                    // emit will fire if any ballot lands later (the
                    // apply path resets emit-eligibility on stale-LWW
                    // overwrites; for "no ballots at all" the share
                    // would decrypt to all zeros, but we skip rather
                    // than publish an empty-electorate kd=ts).
                    return;
                }
            };
            Some(Snapshot {
                n,
                aggregates,
                committee_epoch: latest_epoch,
                y_i_compressed,
                stored_first_share,
            })
        };
        let snap = match snapshot {
            Some(s) => s,
            None => return,
        };

        // (2) Acquire the local FROST KeyPackage from the per-community
        // dfrost engine. If unwired or no local key (non-committee
        // member, or DKG didn't finalize locally yet), silent-bail —
        // future applies will retry the gates above.
        let kp = {
            let dr = self.dfrost_registry.lock().await;
            let engine_opt = match dr.as_ref() {
                Some(reg) => reg.get(self.community_id).await,
                None => None,
            };
            match engine_opt {
                Some(engine) => engine.local_key_package().await,
                None => None,
            }
        };
        let kp = match kp {
            Some(k) => k,
            None => {
                tracing::debug!(
                    poll_id = %hex::encode(pid.0),
                    "engine-auto kd=ts: no local FROST KeyPackage — bail"
                );
                return;
            }
        };
        let x_i = crate::community_dfrost_crypto::signing_share_as_scalar(&kp);

        // (2.5) Aggregate-change idempotency check. If we have a stored
        // share at this (actor, epoch) AND its first entry matches what
        // we'd compute for the current aggregate's c1_agg[0], then the
        // homomorphic aggregate hasn't changed since our last emit — no
        // need to re-publish. If they differ, late-arriving ballots have
        // shifted the aggregate; we re-emit so the LWW upsert in apply
        // replaces our stale share with a freshly-bound one (CodeRabbit
        // F23 fix).
        if let Some(stored) = snap.stored_first_share {
            let c1_agg_0 = match crate::community_voting_tier3_crypto::decompress_point(
                &snap.aggregates[0].c1,
            ) {
                Some(p) => p,
                None => return,
            };
            let expected_first =
                crate::community_voting_tier3_crypto::partial_decrypt_share(&c1_agg_0, &x_i);
            let expected_compressed =
                crate::community_voting_tier3_crypto::compress_point(&expected_first);
            if expected_compressed == stored {
                tracing::debug!(
                    poll_id = %hex::encode(pid.0),
                    "engine-auto kd=ts: aggregate unchanged since last emit — skipping"
                );
                return;
            }
        }

        // (3) Decompress own verifying share Y_i once for DLEQ proofs.
        let y_i = match crate::community_voting_tier3_crypto::decompress_point(&snap.y_i_compressed)
        {
            Some(p) => p,
            None => {
                tracing::warn!(
                    poll_id = %hex::encode(pid.0),
                    "engine-auto kd=ts: Y_i failed to decompress — committee snapshot corrupt?"
                );
                return;
            }
        };
        let g = RISTRETTO_BASEPOINT_POINT;

        // (4) Compute one TallyShareEntry per aggregate.
        let mut entries: Vec<crate::community_voting_core::TallyShareEntry> =
            Vec::with_capacity(snap.aggregates.len());
        for agg in &snap.aggregates {
            let c1_agg = match crate::community_voting_tier3_crypto::decompress_point(&agg.c1) {
                Some(p) => p,
                None => {
                    tracing::warn!(
                        poll_id = %hex::encode(pid.0),
                        "engine-auto kd=ts: c1_agg failed to decompress — bail"
                    );
                    return;
                }
            };
            let d_i = crate::community_voting_tier3_crypto::partial_decrypt_share(&c1_agg, &x_i);
            let proof =
                crate::community_voting_tier3_nizk::dleq_prove(&g, &y_i, &c1_agg, &d_i, &x_i);
            entries.push(crate::community_voting_core::TallyShareEntry {
                share: crate::community_voting_tier3_crypto::compress_point(&d_i),
                dleq_proof: proof.to_bytes(),
            });
        }
        debug_assert_eq!(entries.len(), snap.n + snap.n * (snap.n - 1) / 2);

        // (5) Build the signed envelope and publish.
        let hlc = self.reserve_next_local_hlc().await;
        let payload = crate::community_voting_core::TallySharePayload {
            poll_id: *pid,
            committee_epoch: snap.committee_epoch,
            entries,
        };
        let ts_ev = match crate::community_voting_core::build_signed_tally_share(
            signing_key,
            self_owner,
            payload,
            hlc,
        ) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    poll_id = %hex::encode(pid.0),
                    "engine-auto kd=ts build_signed_tally_share failed"
                );
                return;
            }
        };
        if let Err(e) = Box::pin(self.publish_event(ts_ev, None)).await {
            // Race-loss (replay-tracker dedup) is expected when two
            // engines on the same node share signing material. Log at
            // debug to avoid alarm-fatigue.
            tracing::debug!(
                error = %e,
                poll_id = %hex::encode(pid.0),
                "engine-auto kd=ts publish rejected (race loser?)"
            );
        }
    }

    /// ZEB-295 Phase 6 Task 8: finalize a se-mode poll by minting kd=rs
    /// once enough kd=ts have accumulated.
    ///
    /// CodeRabbit PR #155 major: this method is the single source of truth
    /// for the kd=rs (se-mode) emit. It has ONE direct caller — the tail of
    /// `maybe_trigger_engine_auto_orchestration` — which since ZEB-316 fires
    /// from BOTH the local publish path AND the re-enabled inbound dispatch
    /// path (`process_inbound_dispatch`); the former standalone inbound call
    /// was removed as redundant (the cascade tail subsumes it). The inbound
    /// coverage is load-bearing because a node that is NOT in the committee
    /// never publishes kd=ts itself, so its *locally-triggered* cascade never
    /// executes; without the inbound-driven invocation, a non-committee node
    /// would observe ≥t kd=ts events from peers but never finalize its own
    /// log → permanent divergence.
    ///
    /// Convergence: `recover_secret_tally` deterministically picks a
    /// canonical size-`threshold` subset of (i, x_i) shares (Lagrange
    /// invariance) so every replica that crosses the threshold reconstructs
    /// a bit-identical kd=rs *result* (StarResult payload). The kd=rs *HLC*
    /// is a wall-clock reservation and therefore differs per replica (see
    /// the mint site below for why se-mode cannot use a deterministic
    /// close-anchored HLC); the apply-time LWW/terminal-state gate keeps the
    /// first finalizing kd=rs and silently rejects the rest, so the polls
    /// still converge on one terminal result.
    async fn try_finalize_secret_tally(
        self: &Arc<Self>,
        pid: &PollId,
        signing_key: &Arc<ed25519_dalek::SigningKey>,
        self_owner: OwnerAddr,
    ) {
        // (1) Snapshot under lock, compute result, drop lock before
        // recursive publish_event.
        let trigger_result: Option<crate::community_voting_star::StarResult> = {
            let log = self.voting_log.lock().await;
            let state = match log.polls.get(pid) {
                Some(s) => s,
                None => return,
            };
            let t3 = match state.tier_state.as_tier3() {
                Some(t) => t,
                None => return,
            };
            if t3.meta.config.privacy_mode != "se" || t3.result.is_some() {
                None
            } else {
                // Build candidate ordering (mirror kd=rs pu-mode path).
                let sq = crate::community_voting_tier3::synthesize_status_quo(&t3.meta.poll_id);
                let sq_hash = sq.event_hash;
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
                        // Pre-Drafting stage — can't happen given the
                        // close-event invariant in the kd=ts apply path
                        // (kd=ts only stored once Ratification is past).
                        return;
                    }
                };
                let ordered = crate::community_voting_tier3::ratification_candidates_ordering(
                    &advancers, sq_hash,
                );
                crate::community_voting_tier3::recover_secret_tally(t3, &ordered)
            }
        };

        if let Some(result) = trigger_result {
            // ZEB-316 (C1 fix): se-mode kd=rs mints on a WALL-CLOCK HLC
            // (`reserve_next_local_hlc`), NOT a deterministic close-anchored HLC.
            // kd=rs must be minted ABOVE the current receive watermark
            // (`last_received_hlc`) or the apply-time monotonic gate rejects it.
            // In secret mode the committee kd=ts (tally-share) events land AFTER
            // the close carrying per-replica wall-clock HLCs whose wall exceeds
            // the close event's wall, so a close-anchored kd=rs would be
            // non-monotonic and rejected → the poll would never finalize via
            // engine-auto. No deterministic base is also ≥ every applied kd=ts,
            // because the shares are per-replica, not replica-identical.
            //
            // A deterministic HLC is unnecessary here anyway: the kd=rs RESULT
            // already converges bit-identically across replicas via Lagrange
            // invariance in `recover_secret_tally` (a canonical size-`threshold`
            // share subset) + the apply-time LWW/terminal-state gate that keeps
            // the first finalizing kd=rs and drops the rest. (pu-mode kd=rs is
            // wall-clock for the same monotonicity reason — see
            // `maybe_trigger_engine_auto_orchestration`.)
            let hlc = self.reserve_next_local_hlc().await;
            let rs_ev = match crate::community_voting_core::build_signed_poll_result_tier3(
                signing_key,
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
                        "engine-auto kd=rs (secret) build_signed_poll_result_tier3 failed"
                    );
                    return;
                }
            };
            if let Err(e) = Box::pin(self.publish_event(rs_ev, None)).await {
                tracing::debug!(
                    error = %e,
                    poll_id = %hex::encode(pid.0),
                    "engine-auto kd=rs (secret) publish rejected (race loser?)"
                );
            }
        }
    }

    /// ZEB-295 Phase 6 Task 8: install a `DfrostLogCommitteeOracle`
    /// snapshot on the freshly-applied Tier 3 poll. Best-effort — if
    /// the dfrost registry is unwired or no DKG has completed yet, the
    /// poll keeps its default `NullCommitteeOracle` and any se-mode
    /// kd=ts apply silently drops until a real committee exists.
    async fn maybe_install_committee_oracle_for_poll(self: &Arc<Self>, pid: &PollId) {
        // Acquire the dfrost engine; bail if registry/engine unwired.
        let dfrost_engine = {
            let dr = self.dfrost_registry.lock().await;
            match dr.as_ref() {
                Some(reg) => reg.get(self.community_id).await,
                None => None,
            }
        };
        let engine = match dfrost_engine {
            Some(e) => e,
            None => {
                tracing::debug!(
                    poll_id = %hex::encode(pid.0),
                    "engine-auto: dfrost engine unwired; skipping CommitteeOracle install"
                );
                return;
            }
        };
        let oracle = match DfrostLogCommitteeOracle::from_dfrost_engine(engine.as_ref()).await {
            Some(o) => o,
            None => {
                tracing::debug!(
                    poll_id = %hex::encode(pid.0),
                    "engine-auto: no DKG completed yet; skipping CommitteeOracle install"
                );
                return;
            }
        };
        // Re-acquire the voting_log lock briefly to mutate t3.committee_oracle.
        let mut log = self.voting_log.lock().await;
        if let Some(ps) = log.polls.get_mut(pid) {
            if let Some(t3) = ps.tier_state.as_tier3_mut() {
                t3.install_committee_oracle(Arc::new(oracle));
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
                // Non-PollCreate Tier 3 events reference the affected poll
                // via the `{ "pi": ... }` map in the payload — `derive_poll_id`
                // from signing_bytes is correct ONLY for PollCreate. Using
                // it for kd=md/dc/da/rb/cl/rs/sf yields a different (wrong)
                // PollId that misses log.polls, suppressing lifecycle
                // emits (Qodo R1 finding).
                let pid_opt: Option<PollId> =
                    crate::community_voting_log::decode_poll_id_ref(&event.payload);
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

        // ZEB-718: persist the locally-minted event so it survives restart.
        // Any hook-minted follow-up events below persist themselves via
        // their own recursive `publish_event`.
        self.persist_now().await;

        // (3a) After a Tier 3 PollCreate is applied, trigger a VRF beacon
        // request. The stored poll epoch is now authoritative; the beacon
        // request uses it via the poll's meta (not a fresh dfrost query).
        // Fire-and-forget; errors are logged inside maybe_trigger_beacon_for_tier3_create.
        self.maybe_trigger_beacon_for_tier3_create(&event).await;

        // (3b) ZEB-295 Phase 6: install production CommitteeOracle on the
        // freshly-created Tier 3 poll. Snapshot once from the dfrost log;
        // future queries serve from the cache (CHURP rotation handling
        // deferred to v2 per spec §5.2). Failure to install (no DKG yet,
        // registry not wired) is non-fatal — the poll keeps its default
        // NullCommitteeOracle, and any se-mode kd=ts apply will silently
        // drop until a real committee is available.
        if event.tier == Tier::Sortition && event.kind == PollEventKindCode::PollCreate {
            self.maybe_install_committee_oracle_for_poll(&applied_poll_id)
                .await;
        }

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
            // ZEB-316: thread the just-applied event's HLC as the deterministic
            // base for engine-auto kd=sf/kd=cl mints.
            self.maybe_trigger_engine_auto_orchestration(&applied_poll_id, &event.hlc)
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

        // ZEB-295 Phase 6 Task 8: emit voting-tier3-tally-share-applied
        // on every accepted kd=ts so the frontend can render incremental
        // committee-share-count progress in the awaiting-tally se-mode
        // state. Snapshot share_count + threshold at the latest epoch
        // from the post-apply state.
        if applied_event.kind == PollEventKindCode::TallyShare {
            // Decode the applied payload's epoch (we need it for the DTO
            // even though we count shares at the *latest* epoch on the
            // poll — for the simple case they match).
            let epoch_from_payload: Option<u64> = ciborium::de::from_reader::<
                crate::community_voting_core::TallySharePayload,
                _,
            >(&applied_event.payload[..])
            .ok()
            .map(|p| p.committee_epoch);

            // Compute share_count + threshold under a brief re-acquire.
            let counts: Option<(u64, usize, u16)> = {
                let log = self.voting_log.lock().await;
                log.polls
                    .get(pid)
                    .and_then(|ps| ps.tier_state.as_tier3())
                    .and_then(|t3| {
                        let ep =
                            epoch_from_payload.or_else(|| t3.committee_oracle.latest_epoch())?;
                        let share_count = t3
                            .secret_tally
                            .tally_shares
                            .iter()
                            .filter(|((_addr, e), _record)| *e == ep)
                            .count();
                        let threshold = t3
                            .committee_oracle
                            .committee_at_epoch(ep)
                            .map(|cs| cs.threshold)
                            .unwrap_or(0);
                        Some((ep, share_count, threshold))
                    })
            };
            if let Some((epoch, share_count, threshold)) = counts {
                let payload = serde_json::json!({
                    "communityId": community_id_hex,
                    "pollId": pid_hex,
                    "epoch": epoch,
                    "shareCount": share_count,
                    "threshold": threshold,
                });
                if let Err(e) = app_handle.emit("voting-tier3-tally-share-applied", &payload) {
                    tracing::warn!(
                        error = %e,
                        poll_id = %pid_hex,
                        "voting-tier3-tally-share-applied emit failed (non-fatal)"
                    );
                }
            }
        }

        // 5. ZEB-294: deliberation-statement-created (kd=ds applied).
        // Decode the payload, then re-acquire the voting_log lock to confirm
        // the statement actually landed in the projection. Apply rules
        // (stage / mini-public / spam-cap / length) silently drop invalid
        // events; emitting only on real acceptance avoids unnecessary UI
        // refresh churn on dropped traffic.
        if applied_event.kind == PollEventKindCode::DeliberationStatement {
            if let Ok(ds_payload) = ciborium::de::from_reader::<
                crate::community_voting_core::DeliberationStatementPayload,
                _,
            >(&applied_event.payload[..])
            {
                let event_hash = crate::community_voting_tier3::event_hash_of(applied_event);
                let accepted: bool = {
                    let log = self.voting_log.lock().await;
                    log.polls
                        .get(pid)
                        .and_then(|ps| ps.tier_state.as_tier3())
                        .is_some_and(|t3| t3.deliberation.statements.contains_key(&event_hash))
                };
                if accepted {
                    let payload = serde_json::json!({
                        "pollId": hex::encode(ds_payload.poll_id.0),
                        "statementEventHash": hex::encode(event_hash),
                        "author": hex::encode(applied_event.actor.0),
                        "text": ds_payload.text,
                        "createdAtHlcMs": applied_event.hlc.wall_ms,
                    });
                    if let Err(e) =
                        app_handle.emit("voting-tier3-deliberation-statement-created", &payload)
                    {
                        tracing::warn!(
                            error = %e,
                            poll_id = %pid_hex,
                            "voting-tier3-deliberation-statement-created emit failed (non-fatal)"
                        );
                    }
                }
            }
        }

        // 6. ZEB-294: deliberation-vote-cast (kd=dv applied).
        // Only emit when the event actually landed (or refreshed) a vote entry —
        // i.e. the projection's entry for (voter, statement_event_hash) points
        // at this event's hash. LWW-rejected duplicates and apply-time drops
        // would leave a different (or absent) entry.
        if applied_event.kind == PollEventKindCode::DeliberationVote {
            if let Ok(dv_payload) = ciborium::de::from_reader::<
                crate::community_voting_core::DeliberationVotePayload,
                _,
            >(&applied_event.payload[..])
            {
                if let Some(vote_code) =
                    crate::community_voting_core::BridgingVoteCode::from_u8(dv_payload.vote)
                {
                    let event_hash = crate::community_voting_tier3::event_hash_of(applied_event);
                    let accepted: bool = {
                        let log = self.voting_log.lock().await;
                        log.polls
                            .get(pid)
                            .and_then(|ps| ps.tier_state.as_tier3())
                            .and_then(|t3| {
                                t3.deliberation
                                    .votes
                                    .get(&(applied_event.actor, dv_payload.statement_event_hash))
                            })
                            .is_some_and(|entry| entry.last_update_event_hash == event_hash)
                    };
                    if accepted {
                        let payload = serde_json::json!({
                            "pollId": hex::encode(dv_payload.poll_id.0),
                            "statementEventHash": hex::encode(dv_payload.statement_event_hash),
                            "voter": hex::encode(applied_event.actor.0),
                            "vote": vote_code.as_wire_str(),
                        });
                        if let Err(e) =
                            app_handle.emit("voting-tier3-deliberation-vote-cast", &payload)
                        {
                            tracing::warn!(
                                error = %e,
                                poll_id = %pid_hex,
                                "voting-tier3-deliberation-vote-cast emit failed (non-fatal)"
                            );
                        }
                    }
                }
            }
        }

        // 7. ZEB-319: mini-public-decline (kd=md applied). Emit only when
        // the decline actually landed in t3.declines (apply rules drop
        // invalid declines silently — though kd=md currently has no
        // stage guard, keeping the acceptance check future-proofs against
        // apply-rule tightening).
        if applied_event.kind == PollEventKindCode::MiniPublicDecline {
            let accepted: bool = {
                let log = self.voting_log.lock().await;
                log.polls
                    .get(pid)
                    .and_then(|ps| ps.tier_state.as_tier3())
                    .is_some_and(|t3| {
                        t3.declines
                            .iter()
                            .any(|d| d.0 == applied_event.actor && d.1 == applied_event.hlc)
                    })
            };
            if accepted {
                let payload = serde_json::json!({
                    "pollId": pid_hex,
                    "communityId": community_id_hex,
                    "decliner": hex::encode(applied_event.actor.0),
                    "declineHlcMs": applied_event.hlc.wall_ms,
                });
                if let Err(e) = app_handle.emit("voting-tier3-mini-public-decline", &payload) {
                    tracing::warn!(
                        error = %e,
                        poll_id = %pid_hex,
                        "voting-tier3-mini-public-decline emit failed (non-fatal)"
                    );
                }
            }
        }

        // 8. ZEB-319: draft-candidate (kd=dc applied). Emit only when the
        // candidate actually landed in t3.candidates (apply is currently
        // unconditional, but acceptance check future-proofs against
        // stage-gating apply-rule additions).
        if applied_event.kind == PollEventKindCode::DraftCandidate {
            if let Ok(dc_payload) = ciborium::de::from_reader::<
                crate::community_voting_core::DraftCandidatePayload,
                _,
            >(&applied_event.payload[..])
            {
                let event_hash = crate::community_voting_tier3::event_hash_of(applied_event);
                let accepted: bool = {
                    let log = self.voting_log.lock().await;
                    log.polls
                        .get(pid)
                        .and_then(|ps| ps.tier_state.as_tier3())
                        .is_some_and(|t3| t3.candidates.iter().any(|c| c.event_hash == event_hash))
                };
                if accepted {
                    let payload = serde_json::json!({
                        "pollId": pid_hex,
                        "communityId": community_id_hex,
                        "proposer": hex::encode(applied_event.actor.0),
                        "eventHash": hex::encode(event_hash),
                        "candidateText": dc_payload.text,
                    });
                    if let Err(e) = app_handle.emit("voting-tier3-draft-candidate", &payload) {
                        tracing::warn!(
                            error = %e,
                            poll_id = %pid_hex,
                            "voting-tier3-draft-candidate emit failed (non-fatal)"
                        );
                    }
                }
            }
        }

        // 9. ZEB-319: draft-approval (kd=da applied). Emit only when the
        // approval actually landed — i.e. the referenced candidate exists
        // and the actor is now in its approvals set. Silent-drop kd=da
        // events (unknown candidate_event_hash) stay silent.
        if applied_event.kind == PollEventKindCode::DraftApproval {
            if let Ok(da_payload) = ciborium::de::from_reader::<
                crate::community_voting_core::DraftApprovalPayload,
                _,
            >(&applied_event.payload[..])
            {
                let target_hash = da_payload.candidate_event_hash;
                let accepted: bool = {
                    let log = self.voting_log.lock().await;
                    log.polls
                        .get(pid)
                        .and_then(|ps| ps.tier_state.as_tier3())
                        .is_some_and(|t3| {
                            t3.candidates
                                .iter()
                                .find(|c| c.event_hash == target_hash)
                                .is_some_and(|c| c.approvals.contains(&applied_event.actor))
                        })
                };
                if accepted {
                    let payload = serde_json::json!({
                        "pollId": pid_hex,
                        "communityId": community_id_hex,
                        "approver": hex::encode(applied_event.actor.0),
                        "targetEventHash": hex::encode(target_hash),
                    });
                    if let Err(e) = app_handle.emit("voting-tier3-draft-approval", &payload) {
                        tracing::warn!(
                            error = %e,
                            poll_id = %pid_hex,
                            "voting-tier3-draft-approval emit failed (non-fatal)"
                        );
                    }
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
    ) -> Result<Option<(SignedVotingEvent, PollId)>, String> {
        // Decode.
        let event: SignedVotingEvent =
            ciborium::from_reader(packet).map_err(|e| format!("decode: {e}"))?;

        // Dedup gate.
        {
            let tracker = tracker.lock().await;
            if tracker.contains(&event) {
                // Self-loopback or peer redelivery; drop silently.
                return Ok(None);
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
            return Ok(None);
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
        let applied_poll_id: PollId = {
            let mut log = voting_log.lock().await;
            log.apply_with_snapshot(event.clone(), &community_id, Some(snapshot))
                .map_err(|e| format!("apply: {e:?}"))?
        };

        // Record AFTER successful apply on the inbound path: if apply
        // failed (illegal transition, etc.) we don't want to suppress a
        // legitimate retry. On the publish path the ordering is reversed
        // for the self-loopback fix.
        {
            let mut tracker = tracker.lock().await;
            tracker.record(&event);
        }

        Ok(Some((event, applied_poll_id)))
    }

    /// ZEB-718: apply an event received via the backfill pull path.
    ///
    /// Mirrors `process_inbound` (decode → verify@hlc → eligibility →
    /// apply → record) with two deliberate differences:
    /// 1. **Dedup by exact coordinate** (`tracker.seen_coord`), not the
    ///    per-lane high-water mark — so a cross-rotation in-lane gap (a
    ///    later event on the lane was received, the earlier one dropped)
    ///    is recovered rather than swallowed.
    /// 2. **No post-apply orchestration hooks** — backfilled events are
    ///    historical; the inbound dispatch already suppresses the
    ///    orchestration cascade for the same peer-mint-HLC-race reason.
    ///
    /// Returns `Ok(None)` on a duplicate coordinate or when resolvers are
    /// not wired; `Ok(Some(pid))` on a fresh apply.
    pub(crate) async fn apply_backfilled_event(
        &self,
        packet: &[u8],
    ) -> Result<Option<PollId>, String> {
        let event: SignedVotingEvent =
            ciborium::from_reader(packet).map_err(|e| format!("decode: {e}"))?;

        // Coordinate dedup — NOT the high-water gate (see method doc).
        {
            let tracker = self.tracker.lock().await;
            if tracker.seen_coord(&event) {
                return Ok(None);
            }
        }

        let (Some(id_resolver), Some(mem_resolver)) = (
            self.identity_resolver.as_ref(),
            self.membership_resolver.as_ref(),
        ) else {
            tracing::debug!(
                community_id = ?self.community_id,
                "apply_backfilled_event: dropping event — resolvers not wired"
            );
            return Ok(None);
        };

        let snapshot = mem_resolver
            .snapshot_at(self.community_id, &event.hlc)
            .await
            .map_err(|e| format!("snapshot resolve: {e}"))?;

        crate::community_voting_core::verify_voting_event(&event, &snapshot, id_resolver.as_ref())
            .await
            .map_err(|e| format!("verify: {e}"))?;

        inbound_eligibility_check(&event, &snapshot, &self.voting_log).await?;

        let applied_poll_id: PollId = {
            let mut log = self.voting_log.lock().await;
            log.apply_with_snapshot(event.clone(), &self.community_id, Some(snapshot))
                .map_err(|e| format!("apply: {e:?}"))?
        };

        // Record AFTER a successful apply — records both the high-water
        // mark and the exact coordinate (so a re-backfill dedups).
        {
            let mut tracker = self.tracker.lock().await;
            tracker.record(&event);
        }

        // ZEB-718: persist the recovered event so it survives restart.
        self.persist_now().await;

        Ok(Some(applied_poll_id))
    }

    /// ZEB-298 Task 8: inbound dispatch wrapper. Invoked from the receive
    /// loop with `Arc::clone(&engine)` so the four post-apply hooks have
    /// `self`-method access.
    ///
    /// Mirrors `publish_event`'s post-apply hook fan-out so peer replicas
    /// reach an identical post-state to the originating node:
    /// 1. `maybe_trigger_beacon_for_tier3_create` — D-FROST VRF beacon
    ///    request on Tier 3 PollCreate
    /// 2. `maybe_trigger_engine_auto_orchestration` — auto-mint
    ///    kd=sf/cl/rs follow-ups (gated on Tier 3)
    /// 3. `maybe_emit_tier3_lifecycle_events` — Tauri lifecycle events
    ///    for Deliberation→Drafting, Drafting→Ratification, kd=ss /
    ///    kd=rs (gated on Tier 3 + AppHandle)
    /// 4. `maybe_emit_delegate_on_behalf` — ZEB-298 Tier 2 Signal
    ///    notification when the inbound signaler is the local user's
    ///    delegate
    ///
    /// Lock ordering: capture `previous_stage` BEFORE delegating to
    /// `Self::process_inbound` (which acquires the log lock for the
    /// apply step). After the static call returns the log lock is
    /// released; the four hooks each re-acquire it briefly as needed,
    /// avoiding the deadlock that would otherwise occur if we tried to
    /// fire them while holding it.
    ///
    /// Returns `Ok(())` whether the event applied or was dropped (dedup
    /// / resolvers-not-wired / verify failure / apply rejection). The
    /// receive-loop callsite logs `Err` at warn but does not propagate.
    async fn process_inbound_dispatch(self: &Arc<Self>, packet: &[u8]) -> Result<(), String> {
        // Cheap pre-decode of the event header to compute the
        // pre-apply previous_stage snapshot for Tier 3 lifecycle emit
        // (mirrors `publish_event`'s `previous_stage_for_emit`
        // derivation). Failure here is non-fatal — `process_inbound`
        // will hit the same decode error and report it to the caller;
        // we just degrade gracefully to "no previous stage snapshot".
        let previous_stage_for_emit: Option<crate::community_voting_tier3::Stage> =
            if self.app_handle.is_some() {
                match ciborium::from_reader::<SignedVotingEvent, _>(packet) {
                    Ok(pre_event) => {
                        if pre_event.tier == Tier::Sortition
                            && pre_event.kind != PollEventKindCode::PollCreate
                        {
                            // Non-PollCreate Tier 3 events: PollId lives in the
                            // payload's `{ "pi": ... }` map, NOT in signing_bytes
                            // (Qodo R1 — signing-bytes derivation is only correct
                            // for PollCreate; using it elsewhere misses log.polls
                            // and suppresses lifecycle emits).
                            let pid_opt: Option<PollId> =
                                crate::community_voting_log::decode_poll_id_ref(&pre_event.payload);
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
                        }
                    }
                    Err(_) => None,
                }
            } else {
                None
            };

        // Delegate the decode + dedup + verify + eligibility + apply
        // pipeline to the static `process_inbound`. Returns the
        // applied `(event, pid)` on success, `None` on dedup-drop /
        // resolvers-not-wired (silent), or `Err` on verify / apply
        // failures.
        let applied = Self::process_inbound(
            self.community_id,
            &self.voting_log,
            &self.tracker,
            self.identity_resolver.as_ref(),
            self.membership_resolver.as_ref(),
            packet,
        )
        .await?;

        let Some((event, applied_poll_id)) = applied else {
            // Dropped silently (dedup or resolvers absent); no hooks.
            return Ok(());
        };

        // ZEB-718: persist the peer-received event so it survives restart.
        self.persist_now().await;

        // ── Post-apply hooks: same order + same guards as publish_event ──
        //
        // The log lock is released by this point (the apply scope inside
        // `process_inbound` ended). Each hook re-acquires the lock
        // briefly as needed and drops it before any external side
        // effect (Tauri emit, beacon spawn) — so no hook deadlocks with
        // a sibling, even though several touch the log.

        // (1) D-FROST beacon for Tier 3 PollCreate. Internally gated
        // on event.tier + event.kind + beacon_requester presence.
        self.maybe_trigger_beacon_for_tier3_create(&event).await;

        // (1a) ZEB-295 Phase 6: install production CommitteeOracle on a
        // freshly-applied inbound Tier 3 PollCreate. Mirrors the outbound
        // install in `publish_event` so peer-received polls see the same
        // committee snapshot.
        if event.tier == Tier::Sortition && event.kind == PollEventKindCode::PollCreate {
            self.maybe_install_committee_oracle_for_poll(&applied_poll_id)
                .await;
        }

        // (2) Engine-auto orchestration + Tier 3 lifecycle emit. Cheap
        // tier gate avoids touching app_handle for non-Tier-3 traffic.
        //
        // ZEB-316: peer engines holding local_signing now auto-orchestrate
        // from the inbound path too. The mint HLC is derived deterministically
        // from `event.hlc` (this applied trigger, byte-identical on every
        // replica), so independent peer mints are bit-identical → trivial LWW.
        // This mirrors `publish_event`'s ordering (orchestration BEFORE the
        // lifecycle emit) so the emitted stage reflects the post-orchestration
        // end state, and its cascade tail (`maybe_emit_tally_share` +
        // `try_finalize_secret_tally`) subsumes the former standalone se-mode
        // finalize block — a node that crosses the kd=ts threshold via peer
        // inbound now finalizes independently regardless of committee
        // membership.
        if event.tier == Tier::Sortition {
            self.maybe_trigger_engine_auto_orchestration(&applied_poll_id, &event.hlc)
                .await;
            self.maybe_emit_tier3_lifecycle_events(
                &applied_poll_id,
                &event,
                previous_stage_for_emit,
            )
            .await;
        }

        // (3) ZEB-298 Tier 2 delegate-on-behalf notify. Internally
        // gated on (Tier::Conviction, Signal) + policy + delegate
        // edge; non-matching traffic short-circuits cheaply.
        self.maybe_emit_delegate_on_behalf(&event, &applied_poll_id)
            .await;

        Ok(())
    }
}

/// ZEB-316: deterministic, replica-identical HLC for an engine-auto mint.
///
/// Strictly newer than `base`; the `device_id` is a poll-derived lane
/// (`engine-auto-{kind}-{poll_prefix}`) so every replica reacting to the
/// SAME `base` produces a bit-identical HLC → bit-identical signing_bytes →
/// bit-identical event_hash. Unlike `reserve_next_local_hlc`, this reads NO
/// wall-clock, NO `self.device_id`, and does NOT touch the hlc_tracker — all
/// three diverge per replica. `kind` ∈ {"cl","sf","rs"}.
fn engine_auto_hlc_from_base(base: &Hlc, pid: &PollId, kind: &str) -> Hlc {
    let lane = format!("engine-auto-{kind}-{}", hex::encode(&pid.0[..4]));
    // Strictly newer by (wall_ms, logical, device_id): logical+1 at equal wall.
    // Saturation guard (astronomically unlikely — logical resets on wall advance):
    // if logical is maxed, bump wall and reset logical so it stays strictly newer.
    if base.logical == u32::MAX {
        Hlc {
            wall_ms: base.wall_ms.saturating_add(1),
            logical: 0,
            device_id: lane,
        }
    } else {
        Hlc {
            wall_ms: base.wall_ms,
            logical: base.logical + 1,
            device_id: lane,
        }
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

// ── ZEB-718 backfill test seam ───────────────────────────────────────────────

/// Test seam (ZEB-718): build the backfill read + apply closures for
/// `engine`, so integration tests — which see only the public API — can wire
/// the real backfill path into `spawn_voting_log_zenoh_adapter`. Mirrors the
/// closures `ensure_voting_engine_for` builds in production (the underlying
/// `read_backfill_frames` / `apply_backfilled_event` are `pub(crate)`).
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn backfill_closures_for_test<R: tauri::Runtime>(
    engine: &Arc<VotingLogEngine<R>>,
) -> (
    crate::event_loop::VotingBackfillReadFn,
    crate::event_loop::VotingBackfillApplyFn,
) {
    let e_read = Arc::clone(engine);
    let read: crate::event_loop::VotingBackfillReadFn = Arc::new(move || {
        let e = Arc::clone(&e_read);
        Box::pin(async move { e.read_backfill_frames().await })
    });
    let e_apply = Arc::clone(engine);
    let apply: crate::event_loop::VotingBackfillApplyFn = Arc::new(move |frame: Vec<u8>| {
        let e = Arc::clone(&e_apply);
        Box::pin(async move {
            let _ = e.apply_backfilled_event(&frame).await;
        })
    });
    (read, apply)
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
    // ZEB-298 Task 8: the static `process_inbound` now returns
    // `Result<Option<(SignedVotingEvent, PollId)>, String>` so the
    // dispatch wrapper can fire post-apply hooks. The shim collapses
    // the Some/None distinction back to `Result<(), String>` so
    // existing integration tests which assert `is_ok()` / `is_err()`
    // / `unwrap_err()` continue to work without change.
    VotingLogEngine::<tauri::Wry>::process_inbound(
        community_id,
        voting_log,
        tracker,
        identity_resolver,
        membership_resolver,
        packet,
    )
    .await
    .map(|_| ())
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

    // ── ZEB-718 backfill apply helpers ─────────────────────────────────────

    /// Build a Tier-1 PollCreate signed by `key` for `owner` on `device`
    /// at `wall`, so `verify_voting_event` accepts it.
    fn signed_poll_create(
        key: &ed25519_dalek::SigningKey,
        owner: OwnerAddr,
        device: &str,
        wall: u64,
    ) -> SignedVotingEvent {
        use ed25519_dalek::Signer;
        let mut ev = poll_create_event(owner, device, wall);
        let sb = ev.signing_bytes().expect("signing_bytes");
        ev.sig = key.sign(&sb).to_bytes().to_vec();
        ev
    }

    /// Start a minimal production-wired engine whose only member is
    /// `owner` (power 10), sharing `voting_log` with the caller so it can
    /// assert on the materialized state.
    async fn start_backfill_test_engine(
        community_id: SpaceId,
        owner: OwnerAddr,
        pub64: [u8; 64],
        voting_log: Arc<Mutex<VotingLog>>,
    ) -> Arc<VotingLogEngine<tauri::test::MockRuntime>> {
        let mut members = HashMap::new();
        members.insert(
            owner,
            MemberAttrs {
                power: 10,
                vouching_depth: 0,
            },
        );
        let resolvers = Arc::new(FixedTestResolvers {
            identity: HashMap::from([(owner, pub64)]),
            snapshot: MembershipSnapshot { members },
        });
        let id_resolver: Arc<dyn crate::community_voting_core::VotingIdentityResolver> =
            resolvers.clone();
        let mem_resolver: Arc<dyn crate::community_voting_log::MembershipSnapshotResolver> =
            resolvers;
        let (publisher_tx, _publisher_rx) = mpsc::channel::<Vec<u8>>(8);
        let (_subscriber_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(8);
        let app = tauri::test::mock_app();
        VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
            community_id,
            voting_log,
            publisher_tx,
            subscriber_rx,
            hlc_tracker: None,
            device_id: None,
            app_handle: Some(app.handle().clone()),
            identity_resolver: Some(id_resolver),
            membership_resolver: Some(mem_resolver),
        })
        .await
    }

    #[tokio::test]
    async fn apply_backfilled_skips_already_applied_coordinate() {
        let community_id = SpaceId([0x71; 16]);
        let (key, owner, pub64) = fixture_identity_engine(0x71);
        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let engine =
            start_backfill_test_engine(community_id, owner, pub64, Arc::clone(&voting_log)).await;

        let packet = encode_event(&signed_poll_create(&key, owner, "dev", 1_000));

        let first = engine.apply_backfilled_event(&packet).await.unwrap();
        assert!(first.is_some(), "first backfill applies");
        assert_eq!(voting_log.lock().await.polls.len(), 1);

        let second = engine.apply_backfilled_event(&packet).await.unwrap();
        assert!(second.is_none(), "duplicate coordinate is skipped");
        assert_eq!(
            voting_log.lock().await.polls.len(),
            1,
            "no double-apply on re-backfill"
        );
    }

    #[tokio::test]
    async fn apply_backfilled_recovers_in_lane_gap_the_high_water_would_drop() {
        let community_id = SpaceId([0x72; 16]);
        let (key, owner, pub64) = fixture_identity_engine(0x72);
        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let engine =
            start_backfill_test_engine(community_id, owner, pub64, Arc::clone(&voting_log)).await;

        // Two independent polls on the SAME device lane; e1 older than e2.
        let e1 = signed_poll_create(&key, owner, "dev", 1_000);
        let e2 = signed_poll_create(&key, owner, "dev", 2_000);

        // Apply e2 first — advances the high-water for (owner,"dev") past e1.
        let r2 = engine
            .apply_backfilled_event(&encode_event(&e2))
            .await
            .unwrap();
        assert!(r2.is_some());

        // The live high-water gate WOULD now drop e1 (proving the gap);
        // the coordinate gate does not.
        {
            let t = engine.tracker.lock().await;
            assert!(
                t.contains(&e1),
                "high-water gate would wrongly drop the in-lane-gap e1"
            );
            assert!(!t.seen_coord(&e1), "e1's exact coordinate is unseen");
        }

        let r1 = engine
            .apply_backfilled_event(&encode_event(&e1))
            .await
            .unwrap();
        assert!(
            r1.is_some(),
            "in-lane gap e1 must be recovered by coordinate dedup"
        );
        assert_eq!(
            voting_log.lock().await.polls.len(),
            2,
            "both polls present after gap recovery"
        );
    }

    #[tokio::test]
    async fn apply_backfilled_persists_to_disk_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let community_id = SpaceId([0x73; 16]);
        let (key, owner, pub64) = fixture_identity_engine(0x73);
        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let engine =
            start_backfill_test_engine(community_id, owner, pub64, Arc::clone(&voting_log)).await;
        engine.install_persist_dir(dir.path().to_path_buf());

        let ev = signed_poll_create(&key, owner, "dev", 1_000);
        engine
            .apply_backfilled_event(&encode_event(&ev))
            .await
            .unwrap();

        // persist_now fired after apply — the on-disk log round-trips.
        let path = crate::community_voting_persist::voting_path_for(dir.path(), &community_id);
        assert!(
            path.exists(),
            "voting.cbor must exist after a persisted mutation"
        );
        let (events, _policy, poll_restore) =
            crate::community_voting_persist::load_voting_log(&path, &community_id).unwrap();
        assert_eq!(events, vec![ev], "persisted log reloads the applied event");
        // The applied PollCreate materialized one poll, so its restore persists.
        assert_eq!(
            poll_restore.len(),
            1,
            "the applied poll's tick-state overlay is persisted"
        );
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
                tier3_privacy_mode_default: "pu".into(),
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

    /// ZEB-318: Tier 2 events routed through `publish_event` — the IPC
    /// path since the reroute of `voting_signal_tier2` /
    /// `voting_delegate_tier2` / `voting_undelegate_tier2` /
    /// `voting_create_tier2_proposal` — must be broadcast live on the
    /// publisher channel. This is the property the old direct `log.apply`
    /// IPC path lacked: locally-minted Tier 2 events never reached
    /// `publisher_tx`, so peers only learned of them via backfill.
    #[tokio::test]
    async fn publish_event_broadcasts_tier2_signal_packet() {
        let local_owner = OwnerAddr([0x11; 16]);
        let delegate_owner = OwnerAddr([0x22; 16]);
        let mut fix = delegate_on_behalf_fixture(local_owner, delegate_owner, false).await;

        let signal = signal_event_for_emit(delegate_owner, fix.pid, true, 2_000);
        let expected_hlc = signal.hlc.clone();
        fix.engine
            .publish_event(signal, None)
            .await
            .expect("publish_event signal");

        let packet = tokio::time::timeout(Duration::from_secs(1), fix._publisher_rx.recv())
            .await
            .expect("Tier 2 Signal must be broadcast within 1s of publish_event")
            .expect("publisher channel must be open");
        let decoded: SignedVotingEvent =
            ciborium::from_reader(packet.as_slice()).expect("broadcast packet decodes");
        assert_eq!(decoded.kind, PollEventKindCode::Signal);
        assert_eq!(decoded.tier, Tier::Conviction);
        assert_eq!(decoded.actor, delegate_owner);
        assert_eq!(decoded.hlc, expected_hlc);
    }

    // ── ZEB-298 Task 8: process_inbound post-apply hook fan-out ────────────
    //
    // The four hooks fired from `publish_event` must ALSO fire from the
    // inbound (peer-Zenoh-receive) path so peer replicas reach an
    // identical post-state to the originating node. The previous tests
    // cover the `publish_event` callsite of `maybe_emit_delegate_on_behalf`;
    // this one mirrors the assertion but exercises the
    // `process_inbound_dispatch` callsite end-to-end by pushing a real
    // signed Signal event onto the engine's subscriber channel and
    // confirming the Tauri emit fires.

    /// Tier 2 Signal arriving via the inbound (peer-Zenoh) path must
    /// fire `voting-delegate-signaled-on-your-behalf` when the
    /// signaler is the local user's delegate and policy opts in —
    /// proving that `process_inbound_dispatch` invokes the same
    /// `maybe_emit_delegate_on_behalf` hook that `publish_event` does.
    #[tokio::test]
    async fn process_inbound_tier2_signal_fires_delegate_on_behalf() {
        use crate::community_voting_conviction::{
            AutoExecAction, CommunityVotingPolicy, Tier2PollConfig, Q32,
        };
        use tauri::Listener;

        let community_id = SpaceId([0xE3; 16]);

        // Real keypairs so verify_voting_event passes: `local_owner` is
        // who we install on the engine; `delegate_owner` is the actor
        // that signs both the seed PollCreate and the inbound Signal.
        let (_local_key, local_owner, local_pub64) = fixture_identity_engine(0xE1);
        let (delegate_key, delegate_owner, delegate_pub64) = fixture_identity_engine(0xE2);

        // Tier 2 config — eligibility is permissive (min_power = 0) so
        // the inbound Signal isn't rejected by the per-tier eligibility
        // gate. `notify_on_delegate_signal` matters only after apply.
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

        // Snapshot covers both the local user and the delegate so
        // PollCreate's total_supply is non-zero and the Signal's
        // eligibility check sees the delegate as a member.
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
        let snapshot = MembershipSnapshot { members };

        // Pre-seed the log: PollCreate (signed by the delegate so it's
        // valid in case we ever route it through inbound), delegation
        // edge (local → delegate), and policy enabled.
        let voting_log = Arc::new(Mutex::new(VotingLog::new()));
        let pid: PollId = {
            let mut log = voting_log.lock().await;

            let mut create_payload = Vec::new();
            ciborium::into_writer(&cfg, &mut create_payload).expect("encode cfg");
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
                payload: create_payload,
                sig: vec![0u8; 64], // local pre-seed bypasses verify
            };
            let pid = log
                .apply_with_snapshot(create_event, &community_id, Some(snapshot.clone()))
                .expect("apply tier2 create");

            // Delegation edge: local_owner → delegate_owner.
            log.delegation_graph
                .apply_delegate(local_owner, delegate_owner, (500, 0))
                .expect("apply_delegate edge");

            // Community policy enables the notify hook.
            log.set_policy(CommunityVotingPolicy {
                notify_on_delegate_signal: true,
                tier3_privacy_mode_default: "pu".into(),
            });

            pid
        };

        // Production resolvers so process_inbound's verify + apply
        // path activates (otherwise it short-circuits to Ok(None)).
        let resolvers = Arc::new(FixedTestResolvers {
            identity: HashMap::from([(local_owner, local_pub64), (delegate_owner, delegate_pub64)]),
            snapshot,
        });
        let id_resolver: Arc<dyn crate::community_voting_core::VotingIdentityResolver> =
            resolvers.clone();
        let mem_resolver: Arc<dyn crate::community_voting_log::MembershipSnapshotResolver> =
            resolvers;

        let (publisher_tx, _publisher_rx) = mpsc::channel::<Vec<u8>>(8);
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
            identity_resolver: Some(id_resolver),
            membership_resolver: Some(mem_resolver),
        })
        .await;

        // Install local_owner so maybe_emit_delegate_on_behalf can
        // read it. Signing-key bytes are irrelevant — the hook only
        // reads the owner.
        let local_signing = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]));
        engine
            .install_local_signing_key(local_signing, local_owner)
            .await;

        // Listener for the emit under test.
        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_for_listener = Arc::clone(&captured);
        app_handle.listen("voting-delegate-signaled-on-your-behalf", move |evt| {
            captured_for_listener
                .lock()
                .expect("captured lock")
                .push(evt.payload().to_string());
        });

        // Build a real, delegate-signed Tier 2 Signal event and push
        // it onto the subscriber channel — the engine's receive loop
        // dispatches via `process_inbound_dispatch`, which invokes
        // `maybe_emit_delegate_on_behalf` on successful apply.
        let signal_event = {
            use ed25519_dalek::Signer;
            let payload_struct = crate::community_voting_conviction::SignalPayload {
                proposal_id: pid,
                support: true,
            };
            let mut payload = Vec::new();
            ciborium::into_writer(&payload_struct, &mut payload).expect("encode signal");
            let mut ev = SignedVotingEvent {
                tag: 'p',
                version: 1,
                tier: Tier::Conviction,
                kind: PollEventKindCode::Signal,
                hlc: Hlc {
                    wall_ms: 2_000,
                    logical: 0,
                    device_id: "dev-signal".into(),
                },
                actor: delegate_owner,
                payload,
                sig: vec![0u8; 64],
            };
            let sb = ev.signing_bytes().expect("signing_bytes");
            ev.sig = delegate_key.sign(&sb).to_bytes().to_vec();
            ev
        };
        let mut packet = Vec::new();
        ciborium::into_writer(&signal_event, &mut packet).expect("encode signal packet");

        subscriber_tx
            .send(packet)
            .await
            .expect("send signal packet to subscriber channel");

        let payloads = wait_for_emits(Arc::clone(&captured), 1, Duration::from_secs(2)).await;
        assert_eq!(
            payloads.len(),
            1,
            "process_inbound must fire voting-delegate-signaled-on-your-behalf exactly once; \
             got {payloads:?}"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&payloads[0]).expect("payload is JSON");
        assert_eq!(
            parsed["proposalId"].as_str(),
            Some(hex::encode(pid.0).as_str())
        );
        assert_eq!(
            parsed["delegate"].as_str(),
            Some(hex::encode(delegate_owner.0).as_str())
        );
        assert_eq!(parsed["support"].as_bool(), Some(true));

        // Keep the engine + subscriber_tx alive until end-of-scope so
        // the receive loop doesn't exit before the emit lands.
        drop(engine);
        drop(subscriber_tx);
    }

    // ── ZEB-319: Tier 3 kd=md/dc/da emit payload-shape tests ─────────────

    /// Shared fixture: build a VotingLogEngine wired to a mock Tauri
    /// app_handle, with a Tier 3 poll pre-seeded past sortition.
    /// Returns (engine, pid, app_handle, _publisher_rx, _subscriber_tx).
    async fn tier3_past_sortition_fixture() -> (
        Arc<VotingLogEngine<tauri::test::MockRuntime>>,
        PollId,
        tauri::AppHandle<tauri::test::MockRuntime>,
        mpsc::Receiver<Vec<u8>>,
        mpsc::Sender<Vec<u8>>,
    ) {
        use crate::community_voting_core::{
            Eligibility, MemberAttrs, MembershipSnapshot, Tier3PollConfigPayload,
        };

        let community_id = SpaceId([0xF3; 16]);

        // Primary mini-public member (actor for kd=md/dc/da tests).
        let (actor_key, actor_owner, _actor_pub64) = fixture_identity_engine(0xA1);
        // Proposer who creates the poll + kd=ss.
        let (_proposer_key, proposer_owner, _proposer_pub64) = fixture_identity_engine(0xA2);

        let voting_log = Arc::new(Mutex::new(VotingLog::new()));

        // 1. Build + apply a Tier 3 PollCreate directly into the log.
        let sortition_size: u16 = 20; // minimum valid per validate_tier3_poll_config
        let config = Tier3PollConfigPayload {
            proposal_text: "ZEB-319 emit test".into(),
            sortition_size,
            deliberation_window_seconds: 7200,
            drafting_window_seconds: 7200,
            ratification_window_seconds: 7200,
            privacy_mode: "pu".into(),
            incentive_mode: "a".into(),
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            retry_of: None,
        };
        let mut cfg_payload = Vec::new();
        ciborium::into_writer(&config, &mut cfg_payload).expect("encode tier3 cfg");
        let create_event = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::PollCreate,
            hlc: Hlc {
                wall_ms: 1_000_000,
                logical: 0,
                device_id: "dev-cr".into(),
            },
            actor: proposer_owner,
            payload: cfg_payload,
            sig: vec![0u8; 64],
        };

        // Snapshot: need sortition_size * 2 eligible members.
        let electorate: Vec<OwnerAddr> = (0..(sortition_size as usize * 2))
            .map(|i| {
                let mut a = [0u8; 16];
                a[0] = (i & 0xFF) as u8;
                a[1] = 0xF3;
                OwnerAddr(a)
            })
            .collect();
        // Make actor_owner one of the primary members (for mini-public).
        let primary: Vec<OwnerAddr> = {
            let mut p = vec![actor_owner];
            p.extend(electorate.iter().take(sortition_size as usize - 1).copied());
            p
        };

        let snapshot = MembershipSnapshot {
            members: electorate
                .iter()
                .map(|o| {
                    (
                        *o,
                        MemberAttrs {
                            power: 1,
                            vouching_depth: 0,
                        },
                    )
                })
                .chain(std::iter::once((
                    actor_owner,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )))
                .chain(std::iter::once((
                    proposer_owner,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )))
                .collect(),
        };

        let pid = {
            let mut log = voting_log.lock().await;
            log.apply_with_snapshot(create_event.clone(), &community_id, Some(snapshot))
                .expect("apply tier3 PollCreate")
        };

        // 2. Apply a kd=ss SortitionSelection (actor_owner in primary).
        let ss_payload_struct = crate::community_voting_core::SortitionSelectionPayload {
            poll_id: pid,
            primary: primary.clone(),
            backup: vec![],
        };
        let mut ss_payload = Vec::new();
        ciborium::into_writer(&ss_payload_struct, &mut ss_payload).expect("encode ss");
        let ss_event = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::SortitionSelection,
            hlc: Hlc {
                wall_ms: 1_000_001,
                logical: 0,
                device_id: "dev-ss".into(),
            },
            actor: proposer_owner,
            payload: ss_payload,
            sig: vec![0u8; 64],
        };
        {
            let mut log = voting_log.lock().await;
            log.apply_with_snapshot(ss_event, &community_id, None)
                .expect("apply kd=ss");
        }

        // 3. Start engine with mock app_handle.
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

        // Return actor_key in a way that publish_event can sign events.
        // We store it on the engine via install_local_signing_key so that
        // engine-auto paths work; the actual test events are signed externally.
        let signing_key = Arc::new(actor_key);
        engine
            .install_local_signing_key(Arc::clone(&signing_key), actor_owner)
            .await;

        (engine, pid, app_handle, publisher_rx, subscriber_tx)
    }

    /// Build a signed kd=md MiniPublicDecline event for tests.
    fn build_md_event(actor_seed: u8, pid: PollId, wall_ms: u64) -> SignedVotingEvent {
        use ed25519_dalek::Signer;
        let priv_id = harmony_identity::PrivateIdentity::from_seed(&[actor_seed; 32]);
        let owner = OwnerAddr(priv_id.identity.address_hash);
        let private_bytes = priv_id.to_private_bytes();
        let mut ed_secret = [0u8; 32];
        ed_secret.copy_from_slice(&private_bytes[32..64]);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_secret);

        let payload_struct = crate::community_voting_core::MiniPublicDeclinePayload {
            poll_id: pid,
            reason: None,
        };
        let mut payload = Vec::new();
        ciborium::into_writer(&payload_struct, &mut payload).expect("encode md");
        let mut ev = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::MiniPublicDecline,
            hlc: Hlc {
                wall_ms,
                logical: 0,
                device_id: "dev-md".into(),
            },
            actor: owner,
            payload,
            sig: vec![0u8; 64],
        };
        let sb = ev.signing_bytes().expect("signing_bytes");
        ev.sig = signing_key.sign(&sb).to_bytes().to_vec();
        ev
    }

    /// Asserts that `publish_event` of a kd=md MiniPublicDecline fires
    /// exactly one `voting-tier3-mini-public-decline` Tauri event with
    /// the expected payload shape: { pollId, communityId, decliner,
    /// declineHlcMs }.
    #[tokio::test]
    async fn emits_voting_tier3_mini_public_decline_payload_shape() {
        use std::time::Duration;
        use tauri::Listener;

        let (engine, pid, app_handle, _pub_rx, _sub_tx) = tier3_past_sortition_fixture().await;

        // Capture emits.
        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        app_handle.listen("voting-tier3-mini-public-decline", move |evt| {
            captured_clone
                .lock()
                .expect("captured lock")
                .push(evt.payload().to_string());
        });

        // The fixture installs actor seed 0xA1 as local signing key.
        // Build the kd=md event signed by that actor.
        let md_event = build_md_event(0xA1, pid, 1_100_000);
        let actor_owner = md_event.actor;
        let wall_ms = md_event.hlc.wall_ms;

        engine
            .publish_event(md_event, None)
            .await
            .expect("publish kd=md");

        let payloads = wait_for_emits(Arc::clone(&captured), 1, Duration::from_secs(2)).await;
        assert_eq!(
            payloads.len(),
            1,
            "exactly one voting-tier3-mini-public-decline expected; got {payloads:?}"
        );

        let parsed: serde_json::Value =
            serde_json::from_str(&payloads[0]).expect("payload is valid JSON");

        // Assert all four expected keys are present with correct values.
        assert!(parsed["pollId"].is_string(), "pollId present");
        assert!(parsed["communityId"].is_string(), "communityId present");
        assert_eq!(
            parsed["decliner"].as_str(),
            Some(hex::encode(actor_owner.0).as_str()),
            "decliner == hex(actor.0)"
        );
        assert_eq!(
            parsed["declineHlcMs"].as_u64(),
            Some(wall_ms),
            "declineHlcMs == md_event.hlc.wall_ms"
        );

        // Assert no extra keys beyond the four expected.
        let obj = parsed.as_object().expect("payload is an object");
        assert_eq!(
            obj.len(),
            4,
            "payload must have exactly 4 keys (pollId, communityId, decliner, declineHlcMs); \
             got {obj:?}"
        );
    }

    /// Asserts that `publish_event` of a kd=dc DraftCandidate fires
    /// exactly one `voting-tier3-draft-candidate` Tauri event with
    /// the expected payload shape: { pollId, communityId, proposer,
    /// eventHash, candidateText }.
    #[tokio::test]
    async fn emits_voting_tier3_draft_candidate_payload_shape() {
        use ed25519_dalek::Signer;
        use std::time::Duration;
        use tauri::Listener;

        let (engine, pid, app_handle, _pub_rx, _sub_tx) = tier3_past_sortition_fixture().await;

        // Capture emits.
        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        app_handle.listen("voting-tier3-draft-candidate", move |evt| {
            captured_clone
                .lock()
                .expect("captured lock")
                .push(evt.payload().to_string());
        });

        // Build actor (seed 0xA1 = same actor installed in the fixture).
        let priv_id = harmony_identity::PrivateIdentity::from_seed(&[0xA1u8; 32]);
        let actor_owner = OwnerAddr(priv_id.identity.address_hash);
        let private_bytes = priv_id.to_private_bytes();
        let mut ed_secret = [0u8; 32];
        ed_secret.copy_from_slice(&private_bytes[32..64]);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_secret);

        let candidate_text = "Proposal text for ZEB-319 dc test".to_string();
        let dc_payload_struct = crate::community_voting_core::DraftCandidatePayload {
            poll_id: pid,
            text: candidate_text.clone(),
        };
        let mut dc_payload_bytes = Vec::new();
        ciborium::into_writer(&dc_payload_struct, &mut dc_payload_bytes).expect("encode dc");
        let mut dc_event = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::DraftCandidate,
            hlc: Hlc {
                wall_ms: 1_200_000,
                logical: 0,
                device_id: "dev-dc".into(),
            },
            actor: actor_owner,
            payload: dc_payload_bytes,
            sig: vec![0u8; 64],
        };
        let sb = dc_event.signing_bytes().expect("signing_bytes");
        dc_event.sig = signing_key.sign(&sb).to_bytes().to_vec();

        // Compute expected event_hash BEFORE publish_event moves the event.
        let expected_event_hash = crate::community_voting_tier3::event_hash_of(&dc_event);

        engine
            .publish_event(dc_event, None)
            .await
            .expect("publish kd=dc");

        let payloads = wait_for_emits(Arc::clone(&captured), 1, Duration::from_secs(2)).await;
        assert_eq!(
            payloads.len(),
            1,
            "exactly one voting-tier3-draft-candidate expected; got {payloads:?}"
        );

        let parsed: serde_json::Value =
            serde_json::from_str(&payloads[0]).expect("payload is valid JSON");

        assert!(parsed["pollId"].is_string(), "pollId present");
        assert!(parsed["communityId"].is_string(), "communityId present");
        assert_eq!(
            parsed["proposer"].as_str(),
            Some(hex::encode(actor_owner.0).as_str()),
            "proposer == hex(actor.0)"
        );
        assert_eq!(
            parsed["eventHash"].as_str(),
            Some(hex::encode(expected_event_hash).as_str()),
            "eventHash == hex(sha256(signing_bytes(dc_event)))"
        );
        assert_eq!(
            parsed["candidateText"].as_str(),
            Some(candidate_text.as_str()),
            "candidateText == DraftCandidatePayload.text"
        );

        // Assert no extra keys beyond the five expected.
        let obj = parsed.as_object().expect("payload is an object");
        assert_eq!(
            obj.len(),
            5,
            "payload must have exactly 5 keys (pollId, communityId, proposer, eventHash, \
             candidateText); got {obj:?}"
        );
    }

    /// Asserts that `publish_event` of a kd=da DraftApproval fires
    /// exactly one `voting-tier3-draft-approval` Tauri event with
    /// the expected payload shape: { pollId, communityId, approver,
    /// targetEventHash }.
    #[tokio::test]
    async fn emits_voting_tier3_draft_approval_payload_shape() {
        use ed25519_dalek::Signer;
        use std::time::Duration;
        use tauri::Listener;

        let (engine, pid, app_handle, _pub_rx, _sub_tx) = tier3_past_sortition_fixture().await;

        // Capture emits.
        let captured_dc: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_da: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_dc_clone = Arc::clone(&captured_dc);
        let captured_da_clone = Arc::clone(&captured_da);
        app_handle.listen("voting-tier3-draft-candidate", move |evt| {
            captured_dc_clone
                .lock()
                .expect("captured_dc lock")
                .push(evt.payload().to_string());
        });
        app_handle.listen("voting-tier3-draft-approval", move |evt| {
            captured_da_clone
                .lock()
                .expect("captured_da lock")
                .push(evt.payload().to_string());
        });

        // Build actor (seed 0xA1 = installed in fixture).
        let priv_id = harmony_identity::PrivateIdentity::from_seed(&[0xA1u8; 32]);
        let actor_owner = OwnerAddr(priv_id.identity.address_hash);
        let private_bytes = priv_id.to_private_bytes();
        let mut ed_secret = [0u8; 32];
        ed_secret.copy_from_slice(&private_bytes[32..64]);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_secret);

        // Step 1: publish a kd=dc so we have a candidate to approve.
        let dc_payload_struct = crate::community_voting_core::DraftCandidatePayload {
            poll_id: pid,
            text: "Candidate for approval test".into(),
        };
        let mut dc_payload_bytes = Vec::new();
        ciborium::into_writer(&dc_payload_struct, &mut dc_payload_bytes).expect("encode dc");
        let mut dc_event = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::DraftCandidate,
            hlc: Hlc {
                wall_ms: 1_200_000,
                logical: 0,
                device_id: "dev-dc-for-da".into(),
            },
            actor: actor_owner,
            payload: dc_payload_bytes,
            sig: vec![0u8; 64],
        };
        let sb = dc_event.signing_bytes().expect("signing_bytes dc");
        dc_event.sig = signing_key.sign(&sb).to_bytes().to_vec();
        let candidate_event_hash = crate::community_voting_tier3::event_hash_of(&dc_event);

        engine
            .publish_event(dc_event, None)
            .await
            .expect("publish kd=dc for da test");

        // Wait for dc emit to land before publishing da.
        wait_for_emits(Arc::clone(&captured_dc), 1, Duration::from_secs(2)).await;

        // Step 2: publish a kd=da DraftApproval referencing the above candidate.
        // Use a second actor (seed 0xA2 is the proposer in the fixture; use the
        // fixture actor again but at a different HLC to simulate self-approval
        // from another mini-public member — apply is idempotent for same actor).
        let da_payload_struct = crate::community_voting_core::DraftApprovalPayload {
            poll_id: pid,
            candidate_event_hash,
        };
        let mut da_payload_bytes = Vec::new();
        ciborium::into_writer(&da_payload_struct, &mut da_payload_bytes).expect("encode da");
        // Use actor seed 0xA1's key (already in mini-public; self-approval
        // already implicit via dc apply, so this is a no-op on the HashSet
        // but the apply still routes and the emit should still fire).
        let mut da_event = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::DraftApproval,
            hlc: Hlc {
                wall_ms: 1_300_000,
                logical: 0,
                device_id: "dev-da".into(),
            },
            actor: actor_owner,
            payload: da_payload_bytes,
            sig: vec![0u8; 64],
        };
        let sb_da = da_event.signing_bytes().expect("signing_bytes da");
        da_event.sig = signing_key.sign(&sb_da).to_bytes().to_vec();

        engine
            .publish_event(da_event, None)
            .await
            .expect("publish kd=da");

        let payloads = wait_for_emits(Arc::clone(&captured_da), 1, Duration::from_secs(2)).await;
        assert_eq!(
            payloads.len(),
            1,
            "exactly one voting-tier3-draft-approval expected; got {payloads:?}"
        );

        let parsed: serde_json::Value =
            serde_json::from_str(&payloads[0]).expect("payload is valid JSON");

        assert!(parsed["pollId"].is_string(), "pollId present");
        assert!(parsed["communityId"].is_string(), "communityId present");
        assert_eq!(
            parsed["approver"].as_str(),
            Some(hex::encode(actor_owner.0).as_str()),
            "approver == hex(actor.0)"
        );
        assert_eq!(
            parsed["targetEventHash"].as_str(),
            Some(hex::encode(candidate_event_hash).as_str()),
            "targetEventHash == hex(candidate_event_hash)"
        );

        // Assert no extra keys beyond the four expected.
        let obj = parsed.as_object().expect("payload is an object");
        assert_eq!(
            obj.len(),
            4,
            "payload must have exactly 4 keys (pollId, communityId, approver, \
             targetEventHash); got {obj:?}"
        );
    }

    // ── ZEB-316: deterministic engine-auto HLC derivation ──────────────────

    #[test]
    fn engine_auto_hlc_from_base_is_deterministic_and_strictly_newer() {
        let pid = PollId([0xAB; 32]);
        let base = Hlc {
            wall_ms: 1_000,
            logical: 3,
            device_id: "engine".into(),
        };

        let a = engine_auto_hlc_from_base(&base, &pid, "cl");
        let b = engine_auto_hlc_from_base(&base, &pid, "cl");
        // Deterministic: identical (base, pid, kind) → identical HLC.
        assert_eq!(a, b);
        // Strictly newer than base.
        assert!(a.is_strictly_newer_than(&base), "must be strictly newer");
        // Poll-derived lane (first 4 bytes of poll_id hex).
        assert_eq!(a.device_id, "engine-auto-cl-abababab");
        // Same wall, logical+1 in the common case.
        assert_eq!(a.wall_ms, 1_000);
        assert_eq!(a.logical, 4);

        // Distinct kinds → distinct lanes, but both strictly newer than base.
        let rs = engine_auto_hlc_from_base(&base, &pid, "rs");
        assert_eq!(rs.device_id, "engine-auto-rs-abababab");
        assert!(rs.is_strictly_newer_than(&base));

        // Saturation guard: logical at u32::MAX bumps wall instead.
        let maxed = Hlc {
            wall_ms: 5,
            logical: u32::MAX,
            device_id: "x".into(),
        };
        let d = engine_auto_hlc_from_base(&maxed, &pid, "cl");
        assert_eq!(d.wall_ms, 6);
        assert_eq!(d.logical, 0);
        assert!(d.is_strictly_newer_than(&maxed));
    }
}
