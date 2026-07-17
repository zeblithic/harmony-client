//! Owner-state SyncEngine — debounced publishes + Zenoh-agnostic
//! channel surface + replay-protected subscriber merge path
//! (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"Architecture". Channel-based; the Zenoh adapter lives in
//! `event_loop.rs` (Task 19).
//!
//! ZEB-417 SP1: the engine internals (debounce timer, publish path,
//! replay-protected merge path) are now the generic
//! `crate::fleet_sync::FleetSyncEngine<OwnerState>`. This module is a thin
//! owner-state-specific wrapper: it preserves the exact public
//! `SyncEngine::new(...)` signature, supplies the OwnerState merge function
//! (`merge_remote_into_local`), the durability sink (`OwnerStatePersist`),
//! and the fixed root-blob lookup tag (`OWNER_STATE_ROOT_BLOB_TAG`). The
//! on-wire envelope and on-disk format are byte-identical to the pre-417
//! implementation (see `owner_publish_envelope_is_byte_identical_to_legacy`
//! and the construction note on `OwnerStatePersist`).
//!
//! The generic engine uses MINT ordering (apply-before-advance) on the
//! inbound path, replacing the donor's advance-before-apply /
//! `IncomingOutcome::ErrPreMutation`/`ErrPostMutation` retry detail.
//! Owner-state convergence is unchanged; that internal retry ordering is
//! now owned + tested by `fleet_sync` (see its
//! `apply_before_advance_failure_does_not_advance_tracker` and
//! `blob_miss_is_dropped_and_recovered_on_next_publish`).

use crate::content_store::ContentStore;
use crate::fleet_sync::{FleetPersist, FleetSyncConfig, FleetSyncEngine, MergeOutcome, Merger};
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_crypto::FleetKeySet;
#[cfg(test)]
use crate::owner_state_crypto::KeyTree;
use crate::owner_state_types::Hlc;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Re-export the generic engine's error type. ZEB-417 replaced the local
/// `SyncError` enum (which carried `Persist(PersistError)`) with this; the
/// new `Persist` variant carries a `String` (the durability sink is now an
/// injected trait, so its concrete error type is flattened at the
/// boundary). No external code matched on the old `Persist(PersistError)`
/// variant, so this is a non-breaking swap for all callers.
pub use crate::fleet_sync::SyncError;

/// Default debounce window between a `notify_dirty` and the
/// resulting state-root publish. See spec §"Architecture" — small
/// enough to feel near-instant to a human, large enough to collapse
/// keystroke-rate mutations.
///
/// Re-exported from `fleet_sync` (both are 250; pinned equal by
/// `default_debounce_ms_matches_fleet_sync`).
pub use crate::fleet_sync::DEFAULT_DEBOUNCE_MS;

/// Lookup-key tag for the single-blob OwnerState in 3a's simplified CAS
/// layout. See spec §"Root blob shape — Phase 3a simplification". Phase
/// 3b/c restructures into per-entry blobs. Load-bearing for wire identity:
/// the receiver decrypts the root blob with the same `space_lookup_key`
/// tag the publisher used.
pub(crate) const OWNER_STATE_ROOT_BLOB_TAG: &[u8] = b"owner-state-root-blob-v1";

/// Filesystem paths for both new files; assembled at boot from
/// `resolve_identity_dir()` and the spec's filename constants.
#[derive(Debug, Clone)]
pub struct PersistPaths {
    pub crdt: PathBuf,
    pub replay: PathBuf,
}

/// Owner-state durability sink for the generic engine. Writes the CRDT +
/// replay-tracker to disk via the SAME `save_crdt` / `save_replay` calls
/// the pre-417 `persist_both` used, so the on-disk format is byte-identical
/// by construction (no new disk pin needed — see the comment on
/// `owner_publish_envelope_is_byte_identical_to_legacy`).
struct OwnerStatePersist {
    paths: PersistPaths,
}

impl FleetPersist<OwnerState> for OwnerStatePersist {
    fn persist(
        &self,
        state: &OwnerState,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        crate::owner_state_persist::save_crdt(&self.paths.crdt, state)
            .map_err(|e| SyncError::Persist(e.to_string()))?;
        crate::owner_state_persist::save_replay(&self.paths.replay, tracker)
            .map_err(|e| SyncError::Persist(e.to_string()))?;
        Ok(())
    }
}

/// Owner-state sync engine. A thin wrapper over the generic
/// `FleetSyncEngine<OwnerState>` (ZEB-417 SP1). Owns a tokio task that runs
/// the debounce timer + publisher + subscriber + persistence flushes.
/// Construction spawns the task; `shutdown().await` stops it cleanly with
/// one final flush.
pub struct SyncEngine {
    inner: FleetSyncEngine<OwnerState>,
}

impl SyncEngine {
    /// Construct the engine and spawn its internal task.
    ///
    /// `keys` holds the installed fleet KeyTrees (ZEB-668 S5 — publish on
    /// newest, decrypt across the dual-epoch window); `device_id` is the
    /// local device's HLC source; `state` and `tracker` are shared with the
    /// rest of the app via the same `Arc<Mutex<_>>`s.
    ///
    /// (The ZEB-417 "signature preserved byte-for-byte" note is retired:
    /// ZEB-668 S5 deliberately widened `kt: Arc<KeyTree>` to the swappable
    /// key set.)
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        keys: FleetKeySet,
        device_id: String,
        state: Arc<Mutex<OwnerState>>,
        tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
        content_store: Arc<dyn ContentStore>,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
        paths: PersistPaths,
        debounce_ms: u64,
    ) -> Self {
        // The OwnerState merge function. `merge_remote_into_local` folds
        // every sub-CRDT from a decoded remote snapshot into local in the
        // load-bearing order documented on that function. Owner-state's
        // merge does not compute a fine-grained changed-bit (the merge is
        // idempotent and the only inbound side effect is a debounced
        // persist), so we report `changed: true` unconditionally — the
        // engine uses this only to decide whether to fire `on_applied`,
        // which owner-state leaves `None`.
        let merger: Merger<OwnerState> = Arc::new(|local: &mut OwnerState, remote: OwnerState| {
            merge_remote_into_local(local, remote);
            MergeOutcome { changed: true }
        });

        let inner = FleetSyncEngine::new(FleetSyncConfig {
            keys,
            device_id,
            state,
            merger,
            replay_tracker: tracker,
            content_store,
            publisher_tx,
            subscriber_rx,
            persist: Arc::new(OwnerStatePersist { paths }),
            lookup_key_tag: OWNER_STATE_ROOT_BLOB_TAG,
            debounce_ms,
            // CRITICAL: keeps the owner-state wire byte-identical. With
            // `publish_seen = false` the envelope emits an empty `seen`
            // map, which `FleetRootPublish`'s `skip_serializing_if`
            // omits — encoding byte-for-byte the same as the legacy
            // `RootPublishPayload { root_cid, at }`. Owner-state never used
            // the `seen` digest / `synced_device_count` path.
            publish_seen: false,
            // Owner-state emits no inbound UI event (unchanged behavior).
            on_applied: None,
            // Unused while `publish_seen == false`, but the config field is
            // mandatory; a fresh empty map keeps the engine self-contained.
            sibling_acks: Arc::new(Mutex::new(BTreeMap::new())),
        });

        SyncEngine { inner }
    }

    /// Hint that local CRDT state has mutated and a debounced
    /// publish should fire after `debounce_ms`. Non-blocking.
    pub fn notify_dirty(&self) {
        self.inner.notify_dirty();
    }

    /// ZEB-707: expose the inner engine's root-serve channel so the owner-state
    /// query-side zenoh adapter can answer a peer's root PULL (a butler that
    /// missed the live push). The wrapper isn't a `FleetSyncEngine`, so it
    /// delegates like the methods around it. See `FleetSyncEngine::root_serve_tx`.
    pub(crate) fn root_serve_tx(
        &self,
    ) -> tokio::sync::mpsc::Sender<crate::fleet_sync::RootServeReq> {
        self.inner.root_serve_tx()
    }

    /// Force an immediate publish, bypassing the debounce window.
    /// Returns when the publish has been written to the outbound
    /// channel and any persistence flush has completed.
    ///
    /// Unconditionally publishes even when the engine has no pending
    /// dirty state — this differs from the implicit shutdown flush,
    /// which is gated on pending-dirty. The "force publish" semantics are
    /// intentional for callers that need a fence-style sync point (tests,
    /// explicit "sync now" UI, `create_community` / `redeem_invite`
    /// durability fences). On an idle engine the publish carries an
    /// advanced HLC but identical content, so peers see one extra
    /// encrypt/decrypt round-trip — acceptable for the cases that opt in.
    pub async fn flush_now(&self) -> Result<(), SyncError> {
        self.inner.flush_now().await
    }

    /// Durably persist owner-state to disk WITHOUT publishing. See
    /// `FleetSyncEngine::persist_now`. Used by `fence_owner_state_flush` so a
    /// stalled state-root publish can't starve owner-state durability (ZEB-509).
    pub async fn persist_now(&self) -> Result<(), SyncError> {
        self.inner.persist_now().await
    }

    /// Stop the internal task, flushing any pending writes first.
    /// Must be called explicitly during graceful shutdown — `Drop`
    /// is best-effort only.
    ///
    /// Returns `Err(SyncError)` if the final publish or persist pass
    /// failed. Callers should log this rather than swallow it: a
    /// silent failure here means the very last delta a user made
    /// before quitting was not durably persisted.
    ///
    /// If the engine task was already gone (channel closed before our
    /// send landed), returns `Ok(())` — there was nothing to flush.
    pub async fn shutdown(&self) -> Result<(), SyncError> {
        self.inner.shutdown().await
    }
}

/// ZEB-702 T3 (Component B): owner-state is the `friend_graph` carrier — the
/// dataset whose non-convergence is the root of ZEB-702 — so the transport-epoch
/// republish listener must nudge it too. This thin wrapper isn't a
/// `FleetSyncEngine<S>` (it owns one privately), so the generic
/// `RepublishDirty` impl doesn't reach it; delegate through the public
/// `notify_dirty`, exactly as `flush_now`/`persist_now` above delegate. No
/// behavior change — `republish_dirty` schedules the same debounced re-publish
/// of the current root a local mutation would.
impl crate::fleet_sync::RepublishDirty for SyncEngine {
    fn republish_dirty(&self) {
        self.notify_dirty();
    }
}

/// Merge each entry from a decoded remote `OwnerState` snapshot into
/// `local`, in the load-bearing order documented inside (see the
/// "outbox_tombstones → spaces → outbox → inbox → markers →
/// tombstones → owner_device_cache" comment).
///
/// This is the owner-state merge function supplied to the generic
/// `FleetSyncEngine` as its `Merger<OwnerState>`. Kept as a free function
/// so the merge invariants (in particular, that tombstone application
/// clears any matching live Space even if the snapshot carries both) are
/// unit-testable without spinning up the SyncEngine wire harness.
fn merge_remote_into_local(local: &mut OwnerState, remote: OwnerState) {
    // Destructure `remote` up front so each field can be moved through
    // its own loop without partial-move conflicts later.
    let OwnerState {
        spaces,
        outbox,
        inbox,
        markers,
        tombstones,
        owner_device_cache,
        libraries,
        outbox_tombstones,
        friend_graph,
        revoked_dm_devices,
    } = remote;

    // ZEB-243: apply remote outbox tombstones FIRST. LWW per id by HLC;
    // sweep matching local outbox entries whose created_at is strictly
    // older than the merged tombstone HLC. Must precede the outbox merge
    // loop below — without this ordering, a remote entry that is also
    // tombstoned on the remote could re-insert via apply_outbox before the
    // tombstone has a chance to gate it.
    for (id, remote_hlc) in outbox_tombstones {
        // Local tombstone wins iff it is strictly newer than the remote.
        // "strictly newer" is the only safe comparator — Hlc doesn't
        // derive PartialOrd.
        if local
            .outbox_tombstones
            .get(&id)
            .is_none_or(|existing| remote_hlc.is_strictly_newer_than(existing))
        {
            local.outbox_tombstones.insert(id, remote_hlc);
        }
        // Sweep local outbox entry if its created_at HLC is strictly older
        // than the merged (winning) tombstone HLC. Re-borrow after the
        // potential insert above; split-borrow from outbox is fine.
        let merged_hlc: &Hlc = local
            .outbox_tombstones
            .get(&id)
            .expect("just ensured present");
        if local
            .outbox
            .get(&id)
            .is_some_and(|e| merged_hlc.is_strictly_newer_than(&e.created_at))
        {
            local.outbox.remove(&id);
        }
    }

    for (_, space) in spaces {
        local.apply_space_with_canonicalization(space);
    }
    for (_, entry) in outbox {
        local.apply_outbox(entry);
    }
    for (_, entry) in inbox {
        local.apply_inbox(entry);
    }
    for (_, marker) in markers {
        local.apply_marker(marker);
    }
    for tomb in tombstones {
        // Route through tombstone_space (defined in
        // owner_state_crdt.rs) so the live Space is removed in the
        // same step the tombstone is recorded. A naked
        // tombstones.insert leaves the spaces loop above in charge
        // of dropping the live entry — but a malformed (or racing)
        // remote snapshot can carry both spaces[id] AND
        // tombstones[id] for the same id, which would leave local
        // state with the live Space alongside its tombstone.
        // tombstone_space's spaces.remove + tombstones.insert is
        // idempotent and order-independent.
        local.tombstone_space(tomb);
    }
    // Replicate the remote's per-OwnerAddr device cache. Without this
    // loop, OwnerDeviceCache updates on one of the user's devices
    // never propagate to the others (Phase 3b's link-origin resolver
    // would only see entries learned locally), breaking DM unicast
    // addressing convergence across bound devices.
    for (addr, entry) in owner_device_cache.devices {
        local.apply_owner_device_update(
            addr,
            entry.devices,
            entry.device_identity_pubs,
            // ZEB-473: replicate the remote's parallel tunnel-contact vec
            // verbatim (this is a self-device owner-state sync, not a stub).
            entry.device_tunnel_contacts,
            entry.learned_at,
        );
    }
    // ZEB-218 Sub-D Phase 1: per-OwnerAddr LWW merge of library
    // entries. Compare the max(added_at, removed_at) HLC of remote vs
    // local; remote replaces local iff strictly newer. Full
    // CRDT-Apply IPC plumbing lands in later tasks; this loop only
    // closes the snapshot-merge path so the field round-trips correctly
    // across Flow A.
    for (addr, remote_entry) in libraries {
        let remote_max: &Hlc = match &remote_entry.removed_at {
            Some(rm) if rm.is_strictly_newer_than(&remote_entry.added_at) => rm,
            _ => &remote_entry.added_at,
        };
        let should_replace = match local.libraries.get(&addr) {
            None => true,
            Some(existing) => {
                let local_max: &Hlc = match &existing.removed_at {
                    Some(rm) if rm.is_strictly_newer_than(&existing.added_at) => rm,
                    _ => &existing.added_at,
                };
                remote_max.is_strictly_newer_than(local_max)
            }
        };
        if should_replace {
            local.libraries.insert(addr, remote_entry);
        }
    }
    // ZEB-370 Phase 1: per-entry LWW merge of the friend graph. Each
    // `FriendEntry` is keyed by the friend's OwnerAddr and merged on its
    // `learned_at` HLC (newer wins; `Revoked` is a tombstone). Independent
    // of the other sub-CRDTs (keyed by OwnerAddr, not SpaceId), so its
    // merge order doesn't matter — placed last. Closes the snapshot-merge
    // path so the friend graph replicates across the owner's own devices.
    //
    // A synced entry whose `owner_id` does NOT match
    // `identity_hash(master_ed25519)` is rejected as
    // `InvariantFail` by `apply_friend_update` and vanishes; surface it
    // via `tracing::warn!` so a divergent (potentially malicious) remote
    // snapshot leaves a trace rather than silently dropping. `StaleHlc`
    // is normal LWW and `Inserted`/`Merged` are the success paths — only
    // `InvariantFail` is logged.
    for (addr, entry) in friend_graph.friends {
        if let crate::owner_state_crdt::ApplyOutcome::Rejected(
            reason @ crate::owner_state_crdt::RejectionReason::InvariantFail(_),
        ) = local.apply_friend_update(addr, entry)
        {
            tracing::warn!(
                addr = %hex::encode(addr.0),
                reason = %reason,
                "friend-graph merge rejected entry on invariant violation"
            );
        }
    }

    // ZEB-685 (S3): friend-scoped DM revocations are GROW-ONLY — union per owner
    // key (mirrors `apply_outbox`'s `delivered_to.extend`). NOT LWW: two of the
    // owner's own devices each learning a different revocation must both survive.
    // ZEB-692: after the union, re-apply the two convergent bounds so a sibling
    // snapshot cannot re-inflate past them —
    //   (a) cap each touched owner's set to the smallest-N by byte order;
    //   (b) prune the set for any owner whose merged friend status is `Revoked`
    //       (a de-friended contact's DM cutoff is moot). friend_graph is merged
    //       ABOVE this loop, so the status is already converged here.
    for (owner, set) in revoked_dm_devices {
        let local_set = local.revoked_dm_devices.entry(owner).or_default();
        local_set.extend(set);
        while local_set.len() > crate::owner_state_crdt::MAX_REVOKED_DM_DEVICES_PER_OWNER {
            local_set.pop_last();
        }
    }
    // GC-on-de-friend (convergent prune): drop entries whose owner is present in the
    // merged friend graph AS `Revoked`. Runs over the whole store (not just touched
    // keys) so a Revoked tombstone that arrived in THIS merge also cleans a
    // pre-existing local entry.
    local.revoked_dm_devices.retain(|owner, _| {
        !matches!(
            local.friend_graph.friends.get(owner).map(|e| &e.status),
            Some(crate::friend_graph::FriendStatus::Revoked)
        )
    });
}

#[cfg(test)]
mod debounce_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use std::time::Duration;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[0u8; 32]).expect("kt"))
    }

    fn paths() -> (tempfile::TempDir, PersistPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        (dir, paths)
    }

    /// Re-export equality pin: owner-state's `DEFAULT_DEBOUNCE_MS` must
    /// equal `fleet_sync`'s (the two are the same constant via re-export;
    /// this guards against an accidental divergent redefinition in a
    /// future refactor).
    #[test]
    fn default_debounce_ms_matches_fleet_sync() {
        assert_eq!(DEFAULT_DEBOUNCE_MS, crate::fleet_sync::DEFAULT_DEBOUNCE_MS);
        assert_eq!(DEFAULT_DEBOUNCE_MS, 250);
    }

    /// One notify_dirty fires exactly one publish after the debounce.
    #[tokio::test]
    async fn single_notify_dirty_fires_one_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            FleetKeySet::new(make_kt()),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            50, // shorter debounce for tests
        );

        engine.notify_dirty();
        // Should fire within ~50ms; allow 500ms slack.
        let bytes = tokio::time::timeout(Duration::from_millis(500), pub_rx.recv())
            .await
            .expect("publish within timeout")
            .expect("not closed");
        assert!(!bytes.is_empty(), "publish bytes should be non-empty");
        let _ = engine.shutdown().await;
    }

    /// 50 rapid notify_dirty calls within one debounce window
    /// collapse to exactly one publish.
    #[tokio::test]
    async fn rapid_notify_dirty_collapses_to_one_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(64);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            FleetKeySet::new(make_kt()),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            100, // 100ms debounce
        );

        for _ in 0..50 {
            engine.notify_dirty();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Wait long enough for the debounce to fire.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Drain channel and count publishes.
        let mut count = 0;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(50), pub_rx.recv()).await
        {
            count += 1;
        }
        assert_eq!(count, 1, "expected exactly one publish, got {}", count);
        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn flush_now_fires_immediately() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            FleetKeySet::new(make_kt()),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000, // very long debounce — flush_now must beat it
        );

        engine.flush_now().await.unwrap();
        // Must fire within ~50ms — well below the 5000ms debounce.
        let bytes = tokio::time::timeout(Duration::from_millis(200), pub_rx.recv())
            .await
            .expect("publish within timeout")
            .expect("not closed");
        assert!(!bytes.is_empty());
        let _ = engine.shutdown().await;
    }

    // ZEB-393 Fix A contract guard: flush_now must persist owner-state to
    // disk synchronously (without a graceful shutdown). `create_community` /
    // `redeem_invite` call flush_now after committing a new community so a
    // non-graceful exit can't lose the membership. `flush_now_fires_immediately`
    // only asserts the *publish* side; this locks the *persist* side. Expected
    // green on first run (characterization of existing flush_now behaviour).
    #[tokio::test]
    async fn flush_now_persists_owner_state_to_disk_without_shutdown() {
        use crate::owner_state_persist::load_crdt;
        use crate::owner_state_types::{EpochKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind};

        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let crdt_path = paths.crdt.clone(); // capture before `paths` is moved into new()
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            FleetKeySet::new(make_kt()),
            "test-device".into(),
            Arc::clone(&state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000, // long debounce — only flush_now can persist within the test
        );

        // Mutate owner-state in memory: insert a Community Space.
        {
            let h = Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            };
            let space = Space {
                id: SpaceId([42; 16]),
                kind: SpaceKind::Community,
                parent: None,
                community_id: None,
                name: "Durable".into(),
                transport: None,
                members: vec![],
                custom_name: None,
                notification_pref: None,
                left_at: None,
                created_at: h.clone(),
                updated_at: h,
                content_key: None,
                prior_content_keys: vec![],
                current_epoch: Some(0),
                current_epoch_key: Some(EpochKey::new([1u8; 32])),
                old_epoch_keys: BTreeMap::new(),
                admin_addr: Some(OwnerAddr([2u8; 16])),
                is_invite_only: Some(false),
                shared_in_profile: false,
                pending_join_at: None,
            };
            state.lock().await.spaces.insert(space.id, space);
        }

        // Fence to disk WITHOUT a graceful shutdown.
        engine.flush_now().await.unwrap();

        // Reload from disk as boot would — the Space must be present.
        let reloaded = load_crdt(&crdt_path).unwrap();
        assert!(
            reloaded.spaces.contains_key(&SpaceId([42; 16])),
            "flush_now must persist owner-state so a crash after mint can't lose it"
        );

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn flush_now_cancels_pending_wakeup() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            FleetKeySet::new(make_kt()),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            200,
        );

        engine.notify_dirty();
        // Don't wait for the debounce — call flush_now immediately.
        engine.flush_now().await.unwrap();
        // Drain — should see exactly one publish (flush_now's), not two.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut count = 0;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(50), pub_rx.recv()).await
        {
            count += 1;
        }
        assert_eq!(count, 1, "flush_now should cancel pending wakeup");
        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_flushes_pending_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            FleetKeySet::new(make_kt()),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000, // long debounce — shutdown must short-circuit it
        );

        engine.notify_dirty();
        let _ = engine.shutdown().await;
        // After shutdown, the pending publish must already have fired.
        let bytes = pub_rx.try_recv().expect("pending publish flushed");
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn shutdown_without_pending_writes_does_not_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            FleetKeySet::new(make_kt()),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        let _ = engine.shutdown().await;
        // No notify_dirty was called, so nothing to flush.
        assert!(pub_rx.try_recv().is_err());
    }
}

#[cfg(test)]
mod skeleton_tests {
    use super::*;
    use crate::content_store::InMemoryStub;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[0u8; 32]).expect("kt"))
    }

    #[tokio::test]
    async fn construct_and_shutdown_clean() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        let engine = SyncEngine::new(
            FleetKeySet::new(make_kt()),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            DEFAULT_DEBOUNCE_MS,
        );
        let _ = engine.shutdown().await;
        // No assertions beyond "didn't hang or panic."
    }
}

#[cfg(test)]
mod wire_identity_tests {
    //! Wire-identity pin: the OWNER publish path must emit a byte-identical
    //! envelope to the pre-ZEB-417 implementation. `publish_seen = false`
    //! makes `FleetRootPublish` carry an empty `seen` map, which its
    //! `skip_serializing_if` omits — so the canonical-CBOR encoding equals
    //! the legacy `RootPublishPayload { root_cid, at }` byte-for-byte.
    //!
    //! The on-disk format is preserved by construction: `OwnerStatePersist`
    //! calls the same `save_crdt` / `save_replay` (same V2 schema header +
    //! ciborium body) the pre-417 `persist_both` used, so no new disk pin is
    //! needed here. The existing `owner_state_persist` round-trip tests and
    //! `replay_tracker_survives_engine_restart` cover the disk format.

    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::fleet_sync::FleetRootPublish;
    use crate::owner_state_crypto::{
        canonical_cbor_decode, canonical_cbor_encode, decrypt_root_publish,
    };
    use crate::owner_state_types::RootPublishPayload;
    use std::time::Duration;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[0x5au8; 32]).expect("kt"))
    }

    #[tokio::test]
    async fn owner_publish_envelope_is_byte_identical_to_legacy() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        let kt = make_kt();
        // A fixed (non-default) OwnerState so the published root_cid is a
        // real content-addressed value, not the empty-state CID.
        let mut state = OwnerState::default();
        {
            use crate::owner_state_types::{OwnerAddr, Space, SpaceId, SpaceKind};
            let h = Hlc {
                wall_ms: 7,
                logical: 0,
                device_id: "owner-dev".into(),
            };
            state.spaces.insert(
                SpaceId([9; 16]),
                Space {
                    id: SpaceId([9; 16]),
                    kind: SpaceKind::Dm,
                    parent: None,
                    community_id: None,
                    name: "DM".into(),
                    transport: None,
                    members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
                    custom_name: None,
                    notification_pref: None,
                    left_at: None,
                    created_at: h.clone(),
                    updated_at: h,
                    content_key: None,
                    prior_content_keys: vec![],
                    current_epoch: None,
                    current_epoch_key: None,
                    old_epoch_keys: BTreeMap::new(),
                    admin_addr: None,
                    is_invite_only: None,
                    shared_in_profile: false,
                    pending_join_at: None,
                },
            );
        }

        let engine = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "owner-dev".into(),
            Arc::new(Mutex::new(state)),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        engine.flush_now().await.unwrap();

        // Receive the wire frame and decrypt the root publish.
        let wire = tokio::time::timeout(Duration::from_millis(500), pub_rx.recv())
            .await
            .expect("publish within timeout")
            .expect("channel open");
        let payload_bytes = decrypt_root_publish(&kt, &wire).expect("decrypt root publish");
        let fp: FleetRootPublish = canonical_cbor_decode(&payload_bytes).expect("decode envelope");

        // 1. The owner path must NOT emit a `seen` digest.
        assert!(
            fp.seen.is_empty(),
            "owner publish must carry an empty `seen` map (publish_seen=false)"
        );

        // 2. The canonical encoding of the emitted FleetRootPublish must
        //    equal the canonical encoding of the legacy RootPublishPayload
        //    carrying the same root_cid + at — i.e. the owner envelope is
        //    byte-identical to the pre-417 wire type.
        let legacy_bytes = canonical_cbor_encode(&RootPublishPayload {
            root_cid: fp.root_cid,
            at: fp.at.clone(),
        })
        .expect("encode legacy");
        let fleet_bytes = canonical_cbor_encode(&fp).expect("encode fleet");
        assert_eq!(
            fleet_bytes, legacy_bytes,
            "owner-state publish envelope must be byte-identical to legacy RootPublishPayload"
        );

        let _ = engine.shutdown().await;
    }
}

#[cfg(test)]
mod subscriber_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_crypto::{
        canonical_cbor_encode, encrypt_entry, encrypt_root_publish, space_lookup_key,
    };
    use crate::owner_state_types::RootPublishPayload;
    use std::time::Duration;

    /// Per ZEB-259: bounded polling helper for convergence-wait tests.
    async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond().await {
                return true;
            }
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[7u8; 32]).expect("kt"))
    }

    fn paths() -> (tempfile::TempDir, PersistPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        (dir, paths)
    }

    /// Build a wire payload for testing — re-uses the publisher's
    /// encryption path but with a controlled HLC. The `at.device_id` is
    /// the simulated PEER (never the engine's own device), so the
    /// generic engine's echo-suppression doesn't drop it.
    async fn make_wire(
        kt: &Arc<KeyTree>,
        store: &Arc<dyn ContentStore>,
        state: &OwnerState,
        device_id: &str,
        wall_ms: u64,
        logical: u32,
    ) -> Vec<u8> {
        let blob_cleartext = canonical_cbor_encode(state).unwrap();
        let lookup = space_lookup_key(kt, b"owner-state-root-blob-v1");
        let blob_ciphertext = encrypt_entry(kt, &lookup, &blob_cleartext).unwrap();
        let root_cid = harmony_content::cid::ContentId::for_book(
            &blob_ciphertext,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        store.put(root_cid, blob_ciphertext).await.unwrap();
        // Use the legacy RootPublishPayload here on purpose: it proves the
        // generic engine decodes the pre-417 wire shape (empty-seen
        // FleetRootPublish == RootPublishPayload bytes).
        let payload = RootPublishPayload {
            root_cid,
            at: Hlc {
                wall_ms,
                logical,
                device_id: device_id.into(),
            },
        };
        let payload_bytes = canonical_cbor_encode(&payload).unwrap();
        encrypt_root_publish(kt, &payload_bytes).unwrap()
    }

    #[tokio::test]
    async fn subscriber_accepts_strictly_newer_hlc_and_updates_tracker() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "self-device".into(),
            Arc::clone(&state),
            Arc::clone(&tracker),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000, // long debounce — keep self-publishes out of the way
        );

        let wire = make_wire(&kt, &store, &OwnerState::default(), "peer-bob", 1000, 0).await;
        sub_tx.send(wire).await.unwrap();
        let accepted = wait_until(
            || async {
                let t = tracker.lock().await;
                t.get("peer-bob")
                    .is_some_and(|s| s.wall_ms == 1000 && s.logical == 0)
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            accepted,
            "tracker did not record peer-bob wall_ms=1000 within 2s"
        );

        let t = tracker.lock().await;
        let stored = t.get("peer-bob").expect("peer accepted");
        assert_eq!(stored.wall_ms, 1000);
        assert_eq!(stored.logical, 0);
        drop(t);

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn subscriber_rejects_strictly_older_hlc() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "self-device".into(),
            Arc::clone(&state),
            Arc::clone(&tracker),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        // First publish: at=2000.
        sub_tx
            .send(make_wire(&kt, &store, &OwnerState::default(), "peer-bob", 2000, 0).await)
            .await
            .unwrap();
        let recorded = wait_until(
            || async {
                let t = tracker.lock().await;
                t.get("peer-bob").is_some_and(|s| s.wall_ms == 2000)
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            recorded,
            "first publish (wall_ms=2000) not recorded within 2s"
        );

        // Replay: at=1000 (older). Tracker must NOT regress.
        sub_tx
            .send(make_wire(&kt, &store, &OwnerState::default(), "peer-bob", 1000, 0).await)
            .await
            .unwrap();
        // Tier B settle window (per spec §3 negative-assertion rule):
        // we're verifying the tracker stays at wall_ms=2000 AFTER the
        // subscriber processes the older replay. wait_until is the
        // wrong tool here — its predicate is true on entry, so it
        // would exit immediately, before the engine has dequeued the
        // older wire. Bare-sleep gives the subscriber loop time to
        // process; the affirmative assert below then catches a
        // regression. 500ms is 10× the original 50ms for CI headroom.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let t = tracker.lock().await;
        let stored = t.get("peer-bob").expect("still present");
        assert_eq!(stored.wall_ms, 2000, "tracker must not regress");
        drop(t);

        let _ = engine.shutdown().await;
    }

    use crate::owner_state_types::{
        ContentId, DeliveryStatus, OutboxEntry, OutboxEntryId, OwnerAddr, ReadMarker, Space,
        SpaceId, SpaceKind,
    };

    fn folder(id: u8, ts: u64) -> Space {
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "F".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
            updated_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        }
    }

    #[tokio::test]
    async fn subscriber_fetches_and_merges_remote_state() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let local_state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "self-device".into(),
            Arc::clone(&local_state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        // Build a remote OwnerState containing a folder id=42.
        let mut remote = OwnerState::default();
        remote.spaces.insert(SpaceId([42; 16]), folder(42, 100));

        let wire = make_wire(&kt, &store, &remote, "peer-bob", 1000, 0).await;
        sub_tx.send(wire).await.unwrap();
        let converged = wait_until(
            || async {
                let local = local_state.lock().await;
                local.spaces.contains_key(&SpaceId([42; 16]))
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(converged, "local did not merge SpaceId([42; 16]) within 2s");

        let local = local_state.lock().await;
        assert!(
            local.spaces.contains_key(&SpaceId([42; 16])),
            "remote folder must merge into local"
        );
        drop(local);

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn subscriber_merges_friend_graph_entries() {
        use crate::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let local_state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "self-device".into(),
            Arc::clone(&local_state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        // Build a remote OwnerState that has befriended a peer. The friend's
        // OwnerAddr (their owner_id) MUST derive from their master_ed25519
        // (apply_friend_update enforces that correspondence), so use a real
        // seeded master signing key.
        let friend_master = ed25519_dalek::SigningKey::from_bytes(&[0xe5; 32])
            .verifying_key()
            .to_bytes();
        let friend_addr = crate::friend_graph::owner_id_from_master_ed25519(&friend_master);
        let mut remote = OwnerState::default();
        let outcome = remote.apply_friend_update(
            friend_addr,
            FriendEntry {
                master_ed25519: friend_master,
                display: Some("eve".into()),
                status: FriendStatus::Active,
                established_via: FriendOrigin::Token,
                referrable: false,
                learned_at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "peer".into(),
                },
                sealed_secret: None,
            },
        );
        assert!(matches!(
            outcome,
            crate::owner_state_crdt::ApplyOutcome::Inserted
        ));

        let wire = make_wire(&kt, &store, &remote, "peer-bob", 1000, 0).await;
        sub_tx.send(wire).await.unwrap();
        let converged = wait_until(
            || async {
                let local = local_state.lock().await;
                local.friend_graph.friends.contains_key(&friend_addr)
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            converged,
            "subscriber did not merge friend_graph entry within 2s"
        );

        let local = local_state.lock().await;
        let entry = local
            .friend_graph
            .friends
            .get(&friend_addr)
            .expect("friend entry present after merge");
        assert_eq!(entry.status, FriendStatus::Active);
        assert_eq!(entry.established_via, FriendOrigin::Token);
        drop(local);

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn subscriber_merges_outbox_inbox_marker_entries() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let local_state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "self-device".into(),
            Arc::clone(&local_state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        let mut remote = OwnerState::default();
        remote.spaces.insert(SpaceId([1; 16]), folder(1, 100));
        remote.outbox.insert(
            OutboxEntryId([7; 16]),
            OutboxEntry {
                id: OutboxEntryId([7; 16]),
                space_id: SpaceId([1; 16]),
                recipient_owners: vec![OwnerAddr([2; 16])],
                message_cid: Some(ContentId::from_bytes([3; 32])),
                created_at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "peer".into(),
                },
                delivered_to: Default::default(),
                delivery_status: DeliveryStatus::Pending,
            },
        );
        remote.markers.insert(
            SpaceId([1; 16]),
            ReadMarker {
                space_id: SpaceId([1; 16]),
                last_read_at: Hlc {
                    wall_ms: 200,
                    logical: 0,
                    device_id: "peer".into(),
                },
            },
        );

        let wire = make_wire(&kt, &store, &remote, "peer-bob", 1000, 0).await;
        sub_tx.send(wire).await.unwrap();
        let converged = wait_until(
            || async {
                let local = local_state.lock().await;
                local.spaces.contains_key(&SpaceId([1; 16]))
                    && local.outbox.contains_key(&OutboxEntryId([7; 16]))
                    && local.markers.contains_key(&SpaceId([1; 16]))
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            converged,
            "subscriber did not merge spaces/outbox/markers within 2s"
        );

        let local = local_state.lock().await;
        assert!(local.spaces.contains_key(&SpaceId([1; 16])));
        assert!(local.outbox.contains_key(&OutboxEntryId([7; 16])));
        assert!(local.markers.contains_key(&SpaceId([1; 16])));
        drop(local);

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn subscriber_logs_and_skips_when_blob_missing() {
        // Build a wire payload but DON'T put the blob in the store —
        // simulate cross-process / cross-device case where the
        // publisher and subscriber don't share their stubs.
        //
        // NOTE (ZEB-417): under the generic engine's MINT ordering
        // (apply-before-advance), a missing blob now leaves the tracker
        // UN-advanced — the opposite of the donor's advance-before-apply
        // scheme, where the tracker was advanced before the fetch. We
        // therefore assert local state stays empty (the merge was
        // skipped); the tracker-still-records assertion from the donor is
        // intentionally dropped here. That ordering is owned + tested by
        // `fleet_sync`'s `blob_miss_is_dropped_and_recovered_on_next_publish`.
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store_publisher = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let store_subscriber = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let local_state = Arc::new(Mutex::new(OwnerState::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let engine = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "self-device".into(),
            Arc::clone(&local_state),
            Arc::clone(&tracker),
            Arc::clone(&store_subscriber), // subscriber's stub is empty
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        let mut remote = OwnerState::default();
        remote.spaces.insert(SpaceId([42; 16]), folder(42, 100));

        // Publisher puts the blob in its OWN stub; subscriber's
        // stub never receives it.
        let wire = make_wire(&kt, &store_publisher, &remote, "peer-bob", 1000, 0).await;
        sub_tx.send(wire).await.unwrap();

        // Settle window: the subscriber dequeues the wire, passes the
        // read-only replay check, then misses the CAS fetch (Ok(None)) and
        // drops the publish WITHOUT merging or advancing the tracker. A
        // bare sleep is the right tool for this negative assertion (the
        // "spaces stays empty" predicate is true on entry, so wait_until
        // would exit before the engine processes the wire).
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Subscriber must NOT have merged — local stays empty.
        let local = local_state.lock().await;
        assert!(
            local.spaces.is_empty(),
            "subscriber should have skipped the merge for missing blob"
        );
        drop(local);

        // Under MINT ordering the tracker is NOT advanced when the blob is
        // unfetchable, so the next publish from the same peer (newer HLC,
        // hopefully-present root_cid) is retried rather than rejected.
        let t = tracker.lock().await;
        assert!(
            !t.contains_key("peer-bob"),
            "MINT ordering: tracker must NOT advance on a blob-fetch miss"
        );
        drop(t);

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn replay_tracker_survives_engine_restart() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;

        // Round 1: bring up engine, accept a publish, shut down.
        {
            let tracker = Arc::new(Mutex::new(BTreeMap::new()));
            let state = Arc::new(Mutex::new(OwnerState::default()));
            let engine = SyncEngine::new(
                FleetKeySet::new(Arc::clone(&kt)),
                "self-device".into(),
                Arc::clone(&state),
                Arc::clone(&tracker),
                Arc::clone(&store),
                pub_tx.clone(),
                sub_rx,
                paths.clone(),
                5000,
            );
            sub_tx
                .send(make_wire(&kt, &store, &OwnerState::default(), "peer-bob", 5000, 0).await)
                .await
                .unwrap();
            let converged = wait_until(
                || async {
                    let t = tracker.lock().await;
                    t.get("peer-bob").is_some_and(|s| s.wall_ms == 5000)
                },
                Duration::from_secs(2),
            )
            .await;
            assert!(
                converged,
                "tracker did not record peer-bob wall_ms=5000 within 2s"
            );
            let _ = engine.shutdown().await;
        }

        // Round 2: boot a fresh engine, load tracker from disk,
        // verify peer-bob's HLC is 5000. Then send an OLDER publish
        // and confirm rejection.
        let tracker_loaded = crate::owner_state_persist::load_replay(&paths.replay).unwrap();
        assert_eq!(tracker_loaded.get("peer-bob").unwrap().wall_ms, 5000);

        let (_pub_tx2, _pub_rx2) = mpsc::channel(16);
        let (sub_tx2, sub_rx2) = mpsc::channel(16);
        let tracker2 = Arc::new(Mutex::new(tracker_loaded));
        let state2 = Arc::new(Mutex::new(OwnerState::default()));
        let engine2 = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "self-device".into(),
            Arc::clone(&state2),
            Arc::clone(&tracker2),
            Arc::clone(&store),
            _pub_tx2,
            sub_rx2,
            paths.clone(),
            5000,
        );
        // Send an older publish: at=2000 < 5000.
        sub_tx2
            .send(make_wire(&kt, &store, &OwnerState::default(), "peer-bob", 2000, 0).await)
            .await
            .unwrap();
        // Tier B settle window (per spec §3 negative-assertion rule):
        // verifying the post-restart tracker stays at wall_ms=5000
        // AFTER the engine processes the older replay. wait_until's
        // predicate is true on entry, so it would exit before the
        // engine dequeues the wire — a regression after that exit
        // would slip past. Bare-sleep gives the subscriber loop time
        // to process; the affirmative assert_eq! below catches any
        // regression. 500ms is 5× the original 100ms for CI headroom.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let t = tracker2.lock().await;
        assert_eq!(
            t.get("peer-bob").unwrap().wall_ms,
            5000,
            "replay tracker must reject the older HLC across restart"
        );
        drop(t);

        let _ = engine2.shutdown().await;
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_crypto::decrypt_root_publish;
    use crate::owner_state_types::RootPublishPayload;
    use ciborium::from_reader;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[42u8; 32]).expect("kt"))
    }

    fn paths() -> (tempfile::TempDir, PersistPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        (dir, paths)
    }

    #[tokio::test]
    async fn publish_emits_decryptable_payload_with_blob_in_store() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default());
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "alice-device".into(),
            Arc::clone(&state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&store) as Arc<dyn ContentStore>,
            pub_tx,
            sub_rx,
            paths,
            50,
        );

        engine.notify_dirty();
        let wire = tokio::time::timeout(std::time::Duration::from_millis(500), pub_rx.recv())
            .await
            .expect("publish within timeout")
            .expect("channel open");

        // Decrypt the wire payload with Phase-1 helper.
        let payload_bytes = decrypt_root_publish(&kt, &wire).expect("decrypt");
        let payload: RootPublishPayload = from_reader(&payload_bytes[..]).expect("CBOR decode");
        assert_eq!(payload.at.device_id, "alice-device");

        // The root_cid must reference a blob present in the stub.
        let blob = store
            .get(&payload.root_cid)
            .await
            .unwrap()
            .expect("blob present");
        assert!(!blob.is_empty());

        let _ = engine.shutdown().await;
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_types::{OwnerAddr, Space, SpaceId, SpaceKind};
    use std::time::Duration;

    async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond().await {
                return true;
            }
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn make_kt(seed: u8) -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[seed; 32]).expect("kt"))
    }

    fn paths(name: &str, dir: &tempfile::TempDir) -> PersistPaths {
        PersistPaths {
            crdt: dir.path().join(format!("{}_crdt.cbor", name)),
            replay: dir.path().join(format!("{}_replay.cbor", name)),
        }
    }

    fn dm(id: u8, members: Vec<u8>, ts: u64) -> Space {
        use crate::owner_state_types::DmContentKey;
        let mut sorted = members.clone();
        sorted.sort();
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: None,
            members: sorted.into_iter().map(|i| OwnerAddr([i; 16])).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
            updated_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        }
    }

    /// Two SyncEngines share one InMemoryStub. A's publish flows to B
    /// via the cross-wired channels. Senders are stored to keep the
    /// forwarding tasks alive; they're not used directly so the
    /// `_`-prefix silences dead-code warnings.
    struct TwoDevices {
        a_engine: SyncEngine,
        b_engine: SyncEngine,
        a_state: Arc<Mutex<OwnerState>>,
        b_state: Arc<Mutex<OwnerState>>,
        _a_to_b_tx: mpsc::Sender<Vec<u8>>,
        _b_to_a_tx: mpsc::Sender<Vec<u8>>,
        _dir: tempfile::TempDir,
    }

    fn spawn_two_devices(kt_seed: u8) -> TwoDevices {
        let dir = tempfile::tempdir().unwrap();
        let kt = make_kt(kt_seed);
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let a_state = Arc::new(Mutex::new(OwnerState::default()));
        let b_state = Arc::new(Mutex::new(OwnerState::default()));
        let a_tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let b_tracker = Arc::new(Mutex::new(BTreeMap::new()));

        // A publishes → forwards into B's subscriber.
        let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (a_to_b_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        // Forwarding task: drain A's outbox into B's inbox.
        let a_to_b_forwarder = a_to_b_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = a_pub_rx.recv().await {
                let _ = a_to_b_forwarder.send(bytes).await;
            }
        });

        // B publishes → forwards into A's subscriber.
        let (b_pub_tx, mut b_pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (b_to_a_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        let b_to_a_forwarder = b_to_a_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = b_pub_rx.recv().await {
                let _ = b_to_a_forwarder.send(bytes).await;
            }
        });

        let a_engine = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "device-a".into(),
            Arc::clone(&a_state),
            a_tracker,
            Arc::clone(&store),
            a_pub_tx,
            a_sub_rx,
            paths("a", &dir),
            50,
        );
        let b_engine = SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "device-b".into(),
            Arc::clone(&b_state),
            b_tracker,
            Arc::clone(&store),
            b_pub_tx,
            b_sub_rx,
            paths("b", &dir),
            50,
        );

        TwoDevices {
            a_engine,
            b_engine,
            a_state,
            b_state,
            _a_to_b_tx: a_to_b_tx,
            _b_to_a_tx: b_to_a_tx,
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn one_way_convergence() {
        let dev = spawn_two_devices(123);
        // A applies a folder.
        let f = dm(1, vec![1, 2], 100);
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(f.clone());
        }
        dev.a_engine.notify_dirty();
        let converged = wait_until(
            || async {
                let b = dev.b_state.lock().await;
                b.spaces.contains_key(&SpaceId([1; 16]))
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(converged, "B did not see SpaceId([1; 16]) within 2s");

        let b = dev.b_state.lock().await;
        assert!(b.spaces.contains_key(&SpaceId([1; 16])));
        drop(b);

        let _ = dev.a_engine.shutdown().await;
        let _ = dev.b_engine.shutdown().await;
    }

    #[tokio::test]
    async fn bidirectional_convergence() {
        let dev = spawn_two_devices(45);
        let dm_ab = dm(1, vec![1, 2], 100);
        let dm_cd = dm(2, vec![3, 4], 100);
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(dm_ab);
        }
        {
            let mut b = dev.b_state.lock().await;
            b.apply_space_with_canonicalization(dm_cd);
        }
        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        let converged = wait_until(
            || async {
                let a = dev.a_state.lock().await;
                let b = dev.b_state.lock().await;
                a.spaces.contains_key(&SpaceId([1; 16]))
                    && a.spaces.contains_key(&SpaceId([2; 16]))
                    && b.spaces.contains_key(&SpaceId([1; 16]))
                    && b.spaces.contains_key(&SpaceId([2; 16]))
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            converged,
            "A and B did not bidirectionally converge on both DMs within 2s"
        );

        let a = dev.a_state.lock().await;
        let b = dev.b_state.lock().await;
        assert!(a.spaces.contains_key(&SpaceId([1; 16])));
        assert!(a.spaces.contains_key(&SpaceId([2; 16])));
        assert!(b.spaces.contains_key(&SpaceId([1; 16])));
        assert!(b.spaces.contains_key(&SpaceId([2; 16])));
        drop(a);
        drop(b);

        let _ = dev.a_engine.shutdown().await;
        let _ = dev.b_engine.shutdown().await;
    }

    #[tokio::test]
    async fn cross_device_dedupe_through_sync() {
        // A and B independently create the same DM with different
        // ULIDs but the same sorted-members. After sync, both
        // converge on the smaller ULID.
        let dev = spawn_two_devices(7);
        let a_dm = dm(5, vec![1, 2], 100); // larger ULID — loser
        let b_dm = dm(1, vec![1, 2], 100); // smaller ULID — winner
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(a_dm);
        }
        {
            let mut b = dev.b_state.lock().await;
            b.apply_space_with_canonicalization(b_dm);
        }
        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        let converged = wait_until(
            || async {
                let a = dev.a_state.lock().await;
                let b = dev.b_state.lock().await;
                a.spaces.contains_key(&SpaceId([1; 16]))
                    && !a.spaces.contains_key(&SpaceId([5; 16]))
                    && b.spaces.contains_key(&SpaceId([1; 16]))
                    && !b.spaces.contains_key(&SpaceId([5; 16]))
            },
            Duration::from_secs(3),
        )
        .await;
        assert!(
            converged,
            "A and B did not converge on winner SpaceId(1) within 3s"
        );

        let a = dev.a_state.lock().await;
        let b = dev.b_state.lock().await;
        // Both must agree on the winner SpaceId(1) and have lost SpaceId(5).
        assert!(a.spaces.contains_key(&SpaceId([1; 16])));
        assert!(!a.spaces.contains_key(&SpaceId([5; 16])));
        assert!(b.spaces.contains_key(&SpaceId([1; 16])));
        assert!(!b.spaces.contains_key(&SpaceId([5; 16])));
        drop(a);
        drop(b);

        let _ = dev.a_engine.shutdown().await;
        let _ = dev.b_engine.shutdown().await;
    }

    use crate::owner_state_types::{ContentId, DeliveryStatus, OutboxEntry, OutboxEntryId};

    /// Phase 2 round-5 scenario, exercised end-to-end through real
    /// sync: A and B's DMs collapse via dedupe, then a lagging
    /// device C sends an outbox ack still referencing the OLD
    /// (loser) space_id. After canonicalization rewrites A's outbox
    /// to the winner space_id, C's lagging ack must still merge.
    #[tokio::test]
    async fn lagging_peer_ack_after_dedupe_still_merges() {
        let dev = spawn_two_devices(99);

        // A creates DM id=5 (will lose dedupe to B's id=1).
        let a_dm = dm(5, vec![1, 2], 100);
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(a_dm);
            // Plus an OutboxEntry on that DM.
            a.apply_outbox(OutboxEntry {
                id: OutboxEntryId([42; 16]),
                space_id: SpaceId([5; 16]),
                recipient_owners: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
                message_cid: Some(ContentId::from_bytes([7; 32])),
                created_at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "device-a".into(),
                },
                delivered_to: [OwnerAddr([1; 16])].into_iter().collect(),
                delivery_status: DeliveryStatus::Partial,
            });
        }
        // B creates DM id=1 (winner).
        let b_dm = dm(1, vec![1, 2], 100);
        {
            let mut b = dev.b_state.lock().await;
            b.apply_space_with_canonicalization(b_dm);
        }

        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        let converged = wait_until(
            || async {
                let a = dev.a_state.lock().await;
                a.outbox
                    .get(&OutboxEntryId([42; 16]))
                    .is_some_and(|e| e.space_id == SpaceId([1; 16]))
            },
            Duration::from_secs(3),
        )
        .await;
        assert!(
            converged,
            "A's outbox did not canonicalize space_id within 3s"
        );

        // After sync: A's outbox should have been canonicalized to id=1.
        {
            let a = dev.a_state.lock().await;
            let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
            assert_eq!(
                entry.space_id,
                SpaceId([1; 16]),
                "A's outbox must have canonicalized space_id"
            );
        }

        // Now A re-mutates its outbox with the SAME OutboxEntry but
        // still referencing the OLD space_id=5 (simulating a lagging
        // peer). Phase 2 round-5 made apply_outbox accept this.
        {
            let mut a = dev.a_state.lock().await;
            a.apply_outbox(OutboxEntry {
                id: OutboxEntryId([42; 16]),
                space_id: SpaceId([5; 16]), // lagging — old loser id
                recipient_owners: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
                message_cid: Some(ContentId::from_bytes([7; 32])),
                created_at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "device-a".into(),
                },
                delivered_to: [OwnerAddr([2; 16])].into_iter().collect(),
                delivery_status: DeliveryStatus::Partial,
            });
        }
        dev.a_engine.notify_dirty();
        let converged = wait_until(
            || async {
                let a = dev.a_state.lock().await;
                a.outbox.get(&OutboxEntryId([42; 16])).is_some_and(|e| {
                    e.space_id == SpaceId([1; 16])
                        && e.delivered_to.len() == 2
                        && e.delivery_status == DeliveryStatus::Complete
                })
            },
            Duration::from_secs(3),
        )
        .await;
        assert!(
            converged,
            "A's outbox did not reach Complete with 2 acks within 3s"
        );

        // After sync: A's entry still on canonicalized space_id=1,
        // and BOTH acks ({1, 2}) are present → Complete.
        let a = dev.a_state.lock().await;
        let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
        assert_eq!(entry.space_id, SpaceId([1; 16]));
        assert_eq!(entry.delivered_to.len(), 2);
        assert_eq!(entry.delivery_status, DeliveryStatus::Complete);
        drop(a);

        let _ = dev.a_engine.shutdown().await;
        let _ = dev.b_engine.shutdown().await;
    }

    /// OwnerDeviceCache must replicate over Flow A sync. Without the
    /// owner_device_cache loop in `merge_remote_into_local`, an entry
    /// learned on device A would never appear on device B even after
    /// successful state-root sync — Phase 3b's link-origin resolver on
    /// B would fail to bind incoming DM messages from the same OwnerAddr
    /// to the right contact.
    #[tokio::test]
    async fn owner_device_cache_converges_through_sync() {
        use crate::owner_state_types::DeviceIdentityHash;

        let dev = spawn_two_devices(231);

        let owner = OwnerAddr([0x42; 16]);
        let learned = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "device-a".into(),
        };
        // Seed parallel `device_identity_pubs` with two distinct Somes
        // so the sync path actually carries pubs across the wire — an
        // empty-pubs seed would make this test go green even if the
        // CRDT-merge path silently dropped device_identity_pubs.
        //
        // Real (hash, pub) pairs derived from PrivateIdentity so the
        // pub-derives-to-hash invariant in apply_owner_device_update
        // accepts the seed.
        let private_a = harmony_identity::PrivateIdentity::from_seed(&[0xa1; 32]);
        let public_a = private_a.public_identity();
        let pub_a = public_a.to_public_bytes();
        let hash_a = DeviceIdentityHash(public_a.address_hash);
        let private_b = harmony_identity::PrivateIdentity::from_seed(&[0xb2; 32]);
        let public_b = private_b.public_identity();
        let pub_b = public_b.to_public_bytes();
        let hash_b = DeviceIdentityHash(public_b.address_hash);
        // Pre-sort so the post-apply order is deterministic for the
        // assertions below (apply sorts ascending by hash).
        let (devices, pubs) = if hash_a < hash_b {
            (vec![hash_a, hash_b], vec![Some(pub_a), Some(pub_b)])
        } else {
            (vec![hash_b, hash_a], vec![Some(pub_b), Some(pub_a)])
        };

        // A learns a per-OwnerAddr device list.
        {
            let mut a = dev.a_state.lock().await;
            a.apply_owner_device_update(
                owner,
                devices.clone(),
                pubs.clone(),
                Vec::new(),
                learned.clone(),
            );
        }
        dev.a_engine.notify_dirty();
        let converged = wait_until(
            || async {
                let b = dev.b_state.lock().await;
                b.owner_device_cache.devices.contains_key(&owner)
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            converged,
            "B did not replicate OwnerDeviceCache entry from A within 2s"
        );

        // B must see the same entry replicated through state-root sync.
        let b = dev.b_state.lock().await;
        let b_entry = b
            .owner_device_cache
            .devices
            .get(&owner)
            .expect("B should have replicated the OwnerDeviceCache entry from A");
        assert_eq!(
            b_entry.devices, devices,
            "B's replicated devices vec must match A's"
        );
        assert_eq!(
            b_entry.device_identity_pubs, pubs,
            "B's replicated device_identity_pubs must match A's — pins that the CRDT merge \
             path does not silently drop the parallel pubs vec"
        );
        assert_eq!(
            b_entry.learned_at, learned,
            "B's replicated learned_at HLC must match A's"
        );
        drop(b);

        let _ = dev.a_engine.shutdown().await;
        let _ = dev.b_engine.shutdown().await;
    }

    /// Round-5 fix: a remote snapshot containing both `spaces[id]`
    /// AND `tombstones[id]` for the same id (malformed, or a
    /// genuine race between two devices) must NOT leave the local
    /// state holding the live Space alongside its tombstone. The
    /// pre-fix code did `tombstones.insert(id)` directly, so the
    /// spaces loop running first would leave `local.spaces[id]`
    /// behind. The new code routes through `tombstone_space`,
    /// which removes the live entry idempotently regardless of
    /// loop order.
    #[tokio::test]
    async fn remote_snapshot_with_both_space_and_tombstone_clears_live() {
        let mut local = OwnerState::default();

        let space_id = SpaceId([0x77; 16]);
        let live = Space {
            id: space_id,
            ..dm(0x77, vec![1, 2], 1000)
        };

        // Build a remote snapshot whose `spaces` and `tombstones`
        // both reference the same id — the racy/malformed shape
        // the fix defends against.
        let mut remote = OwnerState::default();
        remote.spaces.insert(space_id, live);
        remote.tombstones.insert(space_id);

        super::merge_remote_into_local(&mut local, remote);

        assert!(
            local.tombstones.contains(&space_id),
            "tombstone must be recorded"
        );
        assert!(
            !local.spaces.contains_key(&space_id),
            "live Space must be cleared even when remote snapshot carries both spaces[id] and tombstones[id]"
        );
    }

    /// ZEB-243: a remote tombstone with HLC strictly newer than the
    /// matching local outbox entry must sweep that entry out of
    /// `local.outbox` and record the tombstone's HLC into
    /// `local.outbox_tombstones`.
    #[test]
    fn merge_remote_tombstones_sweep_local_outbox() {
        use crate::owner_state_types::{
            ContentId, DeliveryStatus, OutboxEntry, OutboxEntryId, OwnerAddr,
        };

        let mut local = OwnerState::default();
        let id = OutboxEntryId([0x22; 16]);
        let entry_hlc = Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        };
        let tomb_hlc = Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "b".into(),
        };

        // Seed local with an outbox entry.
        local.outbox.insert(
            id,
            OutboxEntry {
                id,
                space_id: SpaceId([0x01; 16]),
                recipient_owners: vec![OwnerAddr([0x02; 16])],
                message_cid: Some(ContentId::from_bytes([0x03; 32])),
                created_at: entry_hlc.clone(),
                delivered_to: Default::default(),
                delivery_status: DeliveryStatus::Pending,
            },
        );

        // Remote carries only a tombstone for the same id with HLC > entry.
        let mut remote = OwnerState::default();
        remote.outbox_tombstones.insert(id, tomb_hlc.clone());

        super::merge_remote_into_local(&mut local, remote);

        assert!(
            !local.outbox.contains_key(&id),
            "local entry must be swept by the remote tombstone"
        );
        assert_eq!(
            local.outbox_tombstones.get(&id),
            Some(&tomb_hlc),
            "merged tombstone HLC must equal the remote tombstone HLC"
        );
    }

    /// ZEB-685 (S3): the friend-scoped `revoked_dm_devices` set is GROW-ONLY —
    /// two of the owner's own devices each learning a DIFFERENT revocation must
    /// BOTH survive the merge (union), not LWW-clobber. This pins the union
    /// arm in `merge_remote_into_local`.
    #[test]
    fn revoked_dm_devices_merge_is_union_not_lww() {
        use crate::owner_state_types::OwnerAddr;
        let owner = OwnerAddr([7u8; 16]);
        let mut local = crate::owner_state_crdt::OwnerState::default();
        local.apply_revoked_dm_device(owner, [1u8; 32]);
        let mut remote = crate::owner_state_crdt::OwnerState::default();
        remote.apply_revoked_dm_device(owner, [2u8; 32]);
        merge_remote_into_local(&mut local, remote);
        let set = local.revoked_dm_devices.get(&owner).unwrap();
        assert!(
            set.contains(&[1u8; 32]) && set.contains(&[2u8; 32]),
            "merge must UNION concurrent revocations, not clobber: {set:?}"
        );
    }

    /// ZEB-692: the per-owner union in `merge_remote_into_local` must re-apply
    /// the same `MAX_REVOKED_DM_DEVICES_PER_OWNER` cap Task A1 put on
    /// `apply_revoked_dm_device`, else a sibling snapshot can re-inflate a
    /// capped set past the bound via the merge path.
    #[test]
    fn merge_caps_revoked_dm_devices_to_smallest_n_and_converges() {
        use crate::owner_state_crdt::{OwnerState, MAX_REVOKED_DM_DEVICES_PER_OWNER};
        let owner = crate::owner_state_types::OwnerAddr([0x22; 16]);
        let mk = |base: u8| {
            let mut s = OwnerState::default();
            for i in 0..MAX_REVOKED_DM_DEVICES_PER_OWNER {
                let mut ed = [0u8; 32];
                ed[0] = base;
                ed[1] = ((i >> 8) & 0xff) as u8;
                ed[2] = (i & 0xff) as u8;
                s.apply_revoked_dm_device(owner, ed);
            }
            s
        };
        let mut a = mk(0x00);
        let b = mk(0x01); // disjoint key space (base byte differs)
        merge_remote_into_local(&mut a, b.clone());
        assert_eq!(
            a.revoked_dm_devices.get(&owner).unwrap().len(),
            MAX_REVOKED_DM_DEVICES_PER_OWNER,
            "union capped back to N"
        );
        // Convergence: merging b again is a no-op (already the N-smallest of a∪b).
        let before = a.revoked_dm_devices.clone();
        merge_remote_into_local(&mut a, b);
        assert_eq!(a.revoked_dm_devices, before, "re-merge is idempotent");
    }

    /// ZEB-692: a de-friended contact's DM revocation entries are moot — the
    /// merge must prune `revoked_dm_devices[owner]` once the merged friend
    /// graph shows that owner as `Revoked`. The friend_graph merge loop runs
    /// BEFORE the revoked_dm_devices union, so the status is already
    /// converged when the prune runs.
    #[test]
    fn merge_prunes_revoked_dm_devices_for_revoked_friends() {
        use crate::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};
        use crate::owner_state_crdt::OwnerState;
        // A friend we hold a revoked-device entry for, whose friendship the remote
        // snapshot has just tombstoned (Revoked). The merge must drop the entry.
        let friend_master = [7u8; 32];
        let friend_addr = crate::friend_graph::owner_id_from_master_ed25519(&friend_master);
        let mut local = OwnerState::default();
        local.apply_revoked_dm_device(friend_addr, [9u8; 32]);
        // Local still thinks the friend is Active.
        local.apply_friend_update(
            friend_addr,
            FriendEntry {
                master_ed25519: friend_master,
                display: None,
                status: FriendStatus::Active,
                established_via: FriendOrigin::Token,
                referrable: false,
                learned_at: crate::owner_state_types::Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "x".into(),
                },
                sealed_secret: None,
            },
        );
        // Remote snapshot carries a strictly-newer Revoked tombstone.
        let mut remote = OwnerState::default();
        remote.apply_friend_update(
            friend_addr,
            FriendEntry {
                master_ed25519: friend_master,
                display: None,
                status: FriendStatus::Revoked,
                established_via: FriendOrigin::Token,
                referrable: false,
                learned_at: crate::owner_state_types::Hlc {
                    wall_ms: 2,
                    logical: 0,
                    device_id: "x".into(),
                },
                sealed_secret: None,
            },
        );
        merge_remote_into_local(&mut local, remote);
        assert_eq!(
            local.friend_graph.friends[&friend_addr].status,
            FriendStatus::Revoked
        );
        assert!(
            !local.revoked_dm_devices.contains_key(&friend_addr),
            "revoked-device entry pruned for a de-friended owner"
        );
    }

    /// ZEB-370 (Greptile silent-failure fix): a synced `FriendEntry`
    /// whose `owner_id` (map key) does NOT derive from its
    /// `master_ed25519` violates the key↔master-key correspondence
    /// invariant. `apply_friend_update` returns
    /// `Rejected(InvariantFail)`, the merge loop logs a
    /// `tracing::warn!`, and the bad entry must NOT enter the local
    /// CRDT. (The warn is a side effect; we assert the observable
    /// non-insertion + the direct rejection here.)
    #[test]
    fn merge_remote_friend_graph_drops_invariant_violating_entry() {
        use crate::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};
        use crate::owner_state_crdt::{ApplyOutcome, RejectionReason};

        // A real master key whose derived OwnerAddr is `derived_addr`.
        let friend_master = ed25519_dalek::SigningKey::from_bytes(&[0xe5; 32])
            .verifying_key()
            .to_bytes();
        let derived_addr = crate::friend_graph::owner_id_from_master_ed25519(&friend_master);
        // A DIFFERENT addr that the master key does NOT derive to.
        let wrong_addr = crate::owner_state_types::OwnerAddr([0xab; 16]);
        assert_ne!(derived_addr, wrong_addr);

        let bad_entry = FriendEntry {
            master_ed25519: friend_master,
            display: Some("mallory".into()),
            status: FriendStatus::Active,
            established_via: FriendOrigin::Token,
            referrable: false,
            learned_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "peer".into(),
            },
            sealed_secret: None,
        };

        // Direct apply: confirms the exact rejection variant the merge
        // loop matches on (must NOT be a StaleHlc / success outcome).
        let mut probe = OwnerState::default();
        assert!(
            matches!(
                probe.apply_friend_update(wrong_addr, bad_entry.clone()),
                ApplyOutcome::Rejected(RejectionReason::InvariantFail(_))
            ),
            "mismatched addr↔master_ed25519 must be Rejected(InvariantFail)"
        );

        // Build a malicious remote snapshot by inserting the divergent
        // entry directly (bypassing apply_friend_update, which would
        // itself reject it) so the merge loop is the thing under test.
        let mut remote = OwnerState::default();
        remote.friend_graph.friends.insert(wrong_addr, bad_entry);

        let mut local = OwnerState::default();
        super::merge_remote_into_local(&mut local, remote);

        assert!(
            !local.friend_graph.friends.contains_key(&wrong_addr),
            "invariant-violating friend entry must NOT enter the local CRDT"
        );
        assert!(
            local.friend_graph.friends.is_empty(),
            "no friend entry should have been merged"
        );
    }

    /// ZEB-243: full create-sync-delete-sync convergence scenario.
    /// A creates an OutboxEntry; B receives it via sync. B then deletes
    /// it (records a tombstone with HLC > entry.created_at). A merges B's
    /// state — both devices end with empty outbox and matching tombstone.
    /// Idempotent: merging B←A again must produce no change.
    #[test]
    fn merge_remote_into_local_convergence_after_create_and_delete() {
        use crate::owner_state_types::{
            ContentId, DeliveryStatus, OutboxEntry, OutboxEntryId, OwnerAddr,
        };

        let id = OutboxEntryId([0x33; 16]);
        let entry_hlc = Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "device-a".into(),
        };
        // Tombstone HLC is strictly newer than entry.
        let tomb_hlc = Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "device-b".into(),
        };

        let make_entry = || OutboxEntry {
            id,
            space_id: SpaceId([0x0a; 16]),
            recipient_owners: vec![OwnerAddr([0x0b; 16])],
            message_cid: Some(ContentId::from_bytes([0x0c; 32])),
            created_at: entry_hlc.clone(),
            delivered_to: Default::default(),
            delivery_status: DeliveryStatus::Pending,
        };

        // --- Device A creates the entry. ---
        let mut a = OwnerState::default();
        a.outbox.insert(id, make_entry());

        // --- Sync A → B: B receives A's entry. ---
        let mut b = OwnerState::default();
        b.outbox.insert(id, make_entry());

        // Sanity: both now have the entry.
        assert!(a.outbox.contains_key(&id));
        assert!(b.outbox.contains_key(&id));

        // --- B "deletes" it: removes from outbox + records tombstone. ---
        b.outbox.remove(&id);
        b.outbox_tombstones.insert(id, tomb_hlc.clone());

        // --- A merges B's state: tombstone must sweep A's local entry. ---
        let b_snapshot_for_a = b.clone();
        super::merge_remote_into_local(&mut a, b_snapshot_for_a);

        assert!(
            !a.outbox.contains_key(&id),
            "A's outbox entry must be swept after receiving B's tombstone"
        );
        assert_eq!(
            a.outbox_tombstones.get(&id),
            Some(&tomb_hlc),
            "A must record B's tombstone HLC"
        );

        // --- Both states now agree: empty outbox, tombstone present. ---
        assert!(b.outbox.is_empty(), "B's outbox must be empty");
        assert_eq!(
            a.outbox_tombstones, b.outbox_tombstones,
            "tombstones must converge"
        );

        // --- Idempotency: merge B←A again → no change. ---
        let a_snapshot_for_b = a.clone();
        let b_tombstones_before = b.outbox_tombstones.clone();
        super::merge_remote_into_local(&mut b, a_snapshot_for_b);
        assert!(
            b.outbox.is_empty(),
            "B's outbox must remain empty after idempotent merge"
        );
        assert_eq!(
            b.outbox_tombstones, b_tombstones_before,
            "B's tombstones must be unchanged by idempotent merge (A has same tombstone)"
        );
    }

    /// 10 randomized sequences of (mutate-on-A, mutate-on-B,
    /// publish-A, publish-B) operations. After draining, A and B
    /// must hold equal `OwnerState.spaces` maps (the only field this
    /// test mutates — operations are `apply_space_with_canonicalization`
    /// calls). Catches non-determinism in the spaces-merge path that
    /// scripted tests miss.
    ///
    /// ZEB-283: Trial count was reduced from 50 → 10 (2026-05-12) to
    /// cut wall-clock from ~76s to ~15s on every `cargo nextest run`.
    /// The xorshift64 RNG seed is fixed so the SAME 10 sequences
    /// exercise on every run — deterministic regression detection
    /// across those 10 trials. If we ever need 50-trial paranoia for
    /// periodic deep validation, file a follow-up to gate a wider
    /// trial count behind a `nightly` Cargo feature + separate CI
    /// workflow.
    #[tokio::test]
    async fn random_sequence_convergence_10x() {
        // Seedable PRNG — chosen so a regression reproduces.
        let mut rng_state: u64 = 0xdead_beef_cafe_babe;
        fn next(rng: &mut u64) -> u64 {
            // xorshift64
            let mut x = *rng;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *rng = x;
            x
        }

        for trial in 0..10 {
            let dev = spawn_two_devices((trial % 256) as u8);
            // Generate 8-12 random folder mutations split between A and B.
            let n_ops = 8 + (next(&mut rng_state) % 5) as u8;
            for op in 0..n_ops {
                let folder_id = 100 + op;
                let timestamp = 1000 + (next(&mut rng_state) % 10000);
                let to_a = next(&mut rng_state) & 1 == 0;
                let f = dm(
                    folder_id,
                    vec![1, 2 + (op % 3)], // distinct sorted-members per op
                    timestamp,
                );
                if to_a {
                    let mut a = dev.a_state.lock().await;
                    a.apply_space_with_canonicalization(f);
                } else {
                    let mut b = dev.b_state.lock().await;
                    b.apply_space_with_canonicalization(f);
                }
            }
            dev.a_engine.notify_dirty();
            dev.b_engine.notify_dirty();
            // Multiple debounce + sync cycles to let convergence settle.
            tokio::time::sleep(Duration::from_millis(800)).await;

            // Force final flushes both directions and let them propagate.
            dev.a_engine.flush_now().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            dev.b_engine.flush_now().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            dev.a_engine.flush_now().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;

            let a = dev.a_state.lock().await;
            let b = dev.b_state.lock().await;
            assert_eq!(
                a.spaces, b.spaces,
                "trial {}: A and B spaces diverge\nA: {:?}\nB: {:?}",
                trial, a.spaces, b.spaces
            );
            drop(a);
            drop(b);

            let _ = dev.a_engine.shutdown().await;
            let _ = dev.b_engine.shutdown().await;
        }
    }
}

#[cfg(test)]
mod cas_op_protocol_tests {
    //! Phase 3b end-to-end test: exercise the CasOp protocol via a
    //! HashMap-backed stub event loop instead of real Zenoh + StorageTier.
    //! Verifies the publisher PutLocal path, subscriber GetOrFetch cache
    //! hit, and subscriber GetOrFetch cache miss.

    use crate::content_store::{CasOp, ContentStore, RuntimeContentStore};
    use harmony_content::cid::ContentId;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// HashMap-backed simulator of the harmony-runtime event loop's
    /// CasOp arm. Two devices share one `Arc<Mutex<HashMap<...>>>` to
    /// represent the network's collective view; PutLocal inserts,
    /// GetOrFetch reads (no real network).
    fn spawn_stub_event_loop(
        mut cas_op_rx: tokio::sync::mpsc::Receiver<CasOp>,
        store: Arc<Mutex<HashMap<ContentId, Vec<u8>>>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(op) = cas_op_rx.recv().await {
                match op {
                    CasOp::PutLocal {
                        cid, blob, reply, ..
                    } => {
                        store.lock().await.insert(cid, blob);
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    CasOp::GetOrFetch { cid, reply, .. } => {
                        let bytes = store.lock().await.get(&cid).cloned();
                        let _ = reply.send(Ok(bytes));
                    }
                    CasOp::GetLocal { cid, reply } => {
                        let bytes = store.lock().await.get(&cid).cloned();
                        let _ = reply.send(bytes);
                    }
                    CasOp::AllowServeSubtree { reply, .. } => {
                        // Not exercised by this stub.
                        let _ = reply.send(Ok(0));
                    }
                }
            }
        })
    }

    #[tokio::test]
    async fn publisher_put_visible_to_subscriber() {
        let (cas_op_tx, cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = Arc::new(Mutex::new(HashMap::new()));
        let _stub = spawn_stub_event_loop(cas_op_rx, Arc::clone(&store));

        let pub_store =
            RuntimeContentStore::new(cas_op_tx.clone(), std::time::Duration::from_millis(500));
        let sub_store =
            RuntimeContentStore::new(cas_op_tx.clone(), std::time::Duration::from_millis(500));

        // Publisher computes a structured CID for some ciphertext and puts.
        let ciphertext = vec![1, 2, 3, 4, 5];
        let cid = ContentId::for_book(
            &ciphertext,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        pub_store.put(cid, ciphertext.clone()).await.unwrap();

        // Subscriber fetches the same CID — must observe the bytes.
        let observed = sub_store.get(&cid).await.unwrap();
        assert_eq!(observed, Some(ciphertext));
    }

    #[tokio::test]
    async fn subscriber_get_returns_none_for_unknown_cid() {
        let (cas_op_tx, cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = Arc::new(Mutex::new(HashMap::new()));
        let _stub = spawn_stub_event_loop(cas_op_rx, Arc::clone(&store));

        let sub = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));
        let unknown =
            ContentId::for_book(b"nothing", harmony_content::cid::ContentFlags::default()).unwrap();
        let observed = sub.get(&unknown).await.unwrap();
        assert_eq!(observed, None);
    }

    #[tokio::test]
    async fn subscriber_observes_timeout_as_none_and_drops_publish() {
        // Stub that returns Ok(None) for the FIRST GetOrFetch — simulating
        // a network timeout at the event-loop layer. PutLocal still works.
        // Drives a SyncEngine subscriber through a synthetic state-root
        // delivery for a CID the stub doesn't have, asserts the engine
        // continues running and local state stays empty.
        use super::*;
        use crate::owner_state_crdt::OwnerState;
        use crate::owner_state_crypto::{
            canonical_cbor_encode, encrypt_entry, encrypt_root_publish, space_lookup_key, KeyTree,
        };
        use crate::owner_state_types::{Hlc, RootPublishPayload};
        use std::collections::BTreeMap;
        use tokio::sync::mpsc;

        let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(8);

        // Custom stub: GetOrFetch returns Ok(None) on the first call,
        // delegates to a shared HashMap on subsequent calls. PutLocal
        // inserts to the HashMap (so any subsequent calls would see them).
        let store_for_stub = Arc::new(Mutex::new(HashMap::<ContentId, Vec<u8>>::new()));
        let store_ref = Arc::clone(&store_for_stub);
        let _stub = tokio::spawn(async move {
            let mut first_get = true;
            while let Some(op) = cas_op_rx.recv().await {
                match op {
                    CasOp::PutLocal {
                        cid, blob, reply, ..
                    } => {
                        store_ref.lock().await.insert(cid, blob);
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    CasOp::GetOrFetch { cid, reply, .. } => {
                        if first_get {
                            first_get = false;
                            let _ = reply.send(Ok(None)); // simulated timeout
                        } else {
                            let bytes = store_ref.lock().await.get(&cid).cloned();
                            let _ = reply.send(Ok(bytes));
                        }
                    }
                    CasOp::GetLocal { cid, reply } => {
                        let bytes = store_ref.lock().await.get(&cid).cloned();
                        let _ = reply.send(bytes);
                    }
                    CasOp::AllowServeSubtree { reply, .. } => {
                        // Not exercised by this stub.
                        let _ = reply.send(Ok(0));
                    }
                }
            }
        });

        // Set up a SyncEngine subscriber wired through RuntimeContentStore.
        let kt = Arc::new(KeyTree::derive(&[42u8; 32]).unwrap());
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let content_store = Arc::new(RuntimeContentStore::new(
            cas_op_tx.clone(),
            std::time::Duration::from_millis(500),
        )) as Arc<dyn ContentStore>;
        let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);

        let dir = tempfile::tempdir().unwrap();
        let engine = crate::owner_state_sync::SyncEngine::new(
            FleetKeySet::new(Arc::clone(&kt)),
            "device-sub".into(),
            Arc::clone(&state),
            Arc::clone(&tracker),
            Arc::clone(&content_store),
            pub_tx,
            sub_rx,
            crate::owner_state_sync::PersistPaths {
                crdt: dir.path().join("crdt.cbor"),
                replay: dir.path().join("replay.cbor"),
            },
            50,
        );

        // Forge a state-root publish for a CID the stub doesn't have. The
        // first GetOrFetch returns Ok(None), so the subscriber should drop
        // this delivery without mutating local state.
        let lookup = space_lookup_key(&kt, super::OWNER_STATE_ROOT_BLOB_TAG);
        let snapshot = OwnerState::default();
        let cleartext = canonical_cbor_encode(&snapshot).unwrap();
        let ciphertext = encrypt_entry(&kt, &lookup, &cleartext).unwrap();
        let cid_unknown = ContentId::for_book(
            &ciphertext,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        let payload = RootPublishPayload {
            root_cid: cid_unknown,
            at: Hlc {
                wall_ms: 1_000_000,
                logical: 0,
                device_id: "device-pub".into(),
            },
        };
        let payload_bytes = canonical_cbor_encode(&payload).unwrap();
        let wire = encrypt_root_publish(&kt, &payload_bytes).unwrap();

        // Deliver the wire payload — subscriber processes it, hits Ok(None),
        // logs WARN, drops the publish. We assert the engine continues
        // running and local state stays empty.
        sub_tx.send(wire).await.unwrap();

        // Allow the subscriber task to process. The 50ms debounce + a
        // brief safety margin covers the async hop.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        {
            let s = state.lock().await;
            assert!(
                s.spaces.is_empty(),
                "state should remain empty after dropped publish"
            );
        }

        // Engine is alive — shutdown returns cleanly.
        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn subscriber_treats_corrupted_admit_as_miss() {
        // Simulates: peer published valid wire, network served corrupted
        // bytes, our event-loop's PutLocal silently drops them (StorageTier
        // hash-verify rejects on the production path), subsequent cache
        // lookups return Ok(None). Subscriber should treat the corruption
        // case identically to a fetch timeout — both yield Ok(None) at
        // the trait boundary, both fall through to "drop publish, rely on
        // next state-root" recovery.
        use crate::content_store::CasOp;
        use std::collections::HashMap;
        use std::sync::Arc;
        use tokio::sync::Mutex as AsyncMutex;

        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);

        // Stub that ALWAYS returns Ok(None) for GetOrFetch, but accepts
        // PutLocal inserts (simulating the publisher-side admit working,
        // but a peer's corrupted reply being silently dropped on receive).
        let store = Arc::new(AsyncMutex::new(HashMap::new()));
        let store_for_stub = Arc::clone(&store);
        let _stub = tokio::spawn(async move {
            while let Some(op) = cas_op_rx.recv().await {
                match op {
                    CasOp::PutLocal {
                        cid, blob, reply, ..
                    } => {
                        store_for_stub.lock().await.insert(cid, blob);
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    CasOp::GetOrFetch { reply, .. } => {
                        // Always None — simulates StorageTier silently
                        // dropping corrupted bytes from a peer's reply.
                        let _ = reply.send(Ok(None));
                    }
                    CasOp::GetLocal { reply, .. } => {
                        let _ = reply.send(None);
                    }
                    CasOp::AllowServeSubtree { reply, .. } => {
                        // Not exercised by this stub.
                        let _ = reply.send(Ok(0));
                    }
                }
            }
        });

        let store_client = crate::content_store::RuntimeContentStore::new(
            cas_op_tx.clone(),
            std::time::Duration::from_millis(500),
        );

        // GetOrFetch on any CID returns Ok(None) — corrupted-admit collapses
        // onto the same recovery path as a timeout.
        let cid = harmony_content::cid::ContentId::for_book(
            b"anything",
            harmony_content::cid::ContentFlags::default(),
        )
        .unwrap();
        let observed = store_client.get(&cid).await.unwrap();
        assert_eq!(
            observed, None,
            "corrupted-admit must surface as Ok(None) at the get() boundary"
        );
    }
}
