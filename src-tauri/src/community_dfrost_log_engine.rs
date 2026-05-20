//! D-FROST per-community signed-event log engine. Mirrors the
//! `community_voting_log_engine.rs` pattern: one topic per community
//! at `harmony/community/{community_id}/dfrost`; mpsc-based publisher
//! and subscriber channels bridged to Zenoh by the event-loop adapter
//! (deferred — out of scope for ZEB-307; this ticket ships the engine,
//! a follow-up ships the adapter).

use crate::community_dfrost_log::{verify_signed_committee_event, DfrostLog};
use crate::community_dfrost_types::{
    DfrostEventKind, DkgRoundPayload, RefreshRoundPayload, SignedCommitteeEvent, VrfBeaconPayload,
};
use crate::community_state_sync::IdentityResolver;
use crate::owner_state_types::{OwnerAddr, SpaceId};
use crate::{DfrostBeaconReadyPayload, DfrostDkgProgressPayload, DfrostRefreshProgressPayload};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::{mpsc, Mutex};

/// Replay-defense tracker keyed on `(actor, device_id)`. Records the
/// max-observed `(wall_ms, logical)` HLC per signer; any inbound event
/// whose HLC is at-or-below the recorded max is considered a replay /
/// loopback and silently dropped.
#[derive(Default)]
pub struct DfrostReplayTracker {
    seen_max: HashMap<(OwnerAddr, String), (u64, u32)>,
}

impl DfrostReplayTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, event: &SignedCommitteeEvent) -> bool {
        match self
            .seen_max
            .get(&(event.actor, event.hlc.device_id.clone()))
        {
            Some((w, l)) => (event.hlc.wall_ms, event.hlc.logical) <= (*w, *l),
            None => false,
        }
    }

    pub fn record(&mut self, event: &SignedCommitteeEvent) {
        let key = (event.actor, event.hlc.device_id.clone());
        let new_hlc = (event.hlc.wall_ms, event.hlc.logical);
        self.seen_max
            .entry(key)
            .and_modify(|cur| {
                if new_hlc > *cur {
                    *cur = new_hlc;
                }
            })
            .or_insert(new_hlc);
    }
}

/// Parameters bundle for `DfrostLogEngine::start`. Tauri-runtime-generic so
/// tests can pass `tauri::test::MockRuntime` and production can pass the
/// default Wry runtime.
pub struct DfrostLogEngineParams<R: tauri::Runtime> {
    pub community_id: SpaceId,
    pub dfrost_log: Arc<Mutex<DfrostLog>>,
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
    pub app_handle: tauri::AppHandle<R>,
    pub self_addr: OwnerAddr,
    pub self_x25519_priv: [u8; 32],
    /// OwnerAddr → 64-byte identity composite (X25519 || Ed25519). Same
    /// trait shape as `community_state_sync::IdentityResolver`; in
    /// production the engine passes the existing `OwnerDeviceCacheResolver`
    /// directly so no second identity cache is introduced.
    pub identity_resolver: Arc<dyn IdentityResolver + Send + Sync>,
}

/// Per-community D-FROST signed-event engine. Owns the inbound receive loop
/// and a handle to the publish side; both wire up to a Zenoh topic adapter
/// in a follow-up ticket.
pub struct DfrostLogEngine<R: tauri::Runtime> {
    community_id: SpaceId,
    // `dfrost_log` is wired into IPC commands in Tasks 6+. Holding it on
    // the engine now keeps the public construction shape stable across
    // the task sequence. `tracker` + `publisher_tx` are both load-bearing
    // for `publish_event` (record-before-send).
    #[allow(dead_code)]
    dfrost_log: Arc<Mutex<DfrostLog>>,
    tracker: Arc<Mutex<DfrostReplayTracker>>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    // JoinHandle held to abort-on-drop the receive task. Read in Task 6 when
    // we add a shutdown path; for now the implicit Drop is sufficient.
    #[allow(dead_code)]
    _receive_handle: tokio::task::JoinHandle<()>,
    _phantom: std::marker::PhantomData<R>,
}

/// Full inbound chain: decode → sig-verify → dedup → apply → record.
/// Every gate-failure logs + drops the packet silently. NEVER propagates
/// up — a malicious peer publishing garbage MUST NOT terminate the
/// receive loop for the whole community.
///
/// Step ordering rationale:
/// 1. **Decode first**: cheapest gate, kills the bulk of malformed traffic
///    before any async resolver I/O.
/// 2. **Sig verify next**: O(1) Ed25519 verify; resolves async but the
///    resolver is a HashMap lookup in practice. MUST precede apply —
///    unverified bytes never touch the materialised log.
/// 3. **Dedup before apply**: dedup is cheap (BTreeMap lookup); avoids
///    the apply-then-rollback cost on self-loopback (common case).
/// 4. **Apply (irreversible)**: only reached on verified, non-replay
///    events. Failure here means apply-level invariant violation
///    (unknown ceremony, payload decode of inner CBOR, etc.) — log
///    and drop, but do NOT advance the replay tracker (so a peer who
///    later sends a valid follow-up event is not blocked by the
///    failed-apply'd ancestor).
/// 5. **Record AFTER apply**: same stash-after-apply pattern as PR
///    #143 R6 — only events that successfully landed in the log
///    advance the replay tracker.
#[allow(clippy::too_many_arguments)]
async fn process_inbound<R: tauri::Runtime>(
    community_id: SpaceId,
    dfrost_log: &Arc<Mutex<DfrostLog>>,
    tracker: &Arc<Mutex<DfrostReplayTracker>>,
    app_handle: &tauri::AppHandle<R>,
    self_addr: &OwnerAddr,
    self_x25519_priv: &[u8; 32],
    identity_resolver: &Arc<dyn IdentityResolver + Send + Sync>,
    packet: &[u8],
) {
    // 1. Decode.
    let event: SignedCommitteeEvent = match ciborium::de::from_reader(packet) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                community_id = %hex::encode(community_id.0),
                error = %e,
                "dfrost inbound: CBOR decode failed",
            );
            return;
        }
    };

    // 2. Verify envelope sig (defence-in-depth: every byte we apply has
    //    been signed by an identity whose address_hash binds to event.actor).
    if let Err(e) = verify_signed_committee_event(&event, identity_resolver.as_ref()).await {
        tracing::warn!(
            community_id = %hex::encode(community_id.0),
            actor = ?event.actor,
            error = %e,
            "dfrost inbound: signature verify failed",
        );
        return;
    }

    // 3. Dedup. Read-only check — does NOT advance the tracker. We
    //    only `record` after successful apply (step 5), so a verified
    //    event that fails apply (unknown ceremony, etc.) does not
    //    permanently block a same-HLC replay from re-applying after
    //    the precondition lands. Lock + immediate drop — no awaits
    //    across the guard.
    {
        let t = tracker.lock().await;
        if t.contains(&event) {
            // Silent: self-loopback is the common case and not worth
            // a log entry per inbound packet.
            return;
        }
    }

    // 4. Apply. Hold the log lock only across the apply call itself.
    let apply_result = {
        let mut log = dfrost_log.lock().await;
        log.apply_with_identity(event.clone(), self_addr, self_x25519_priv)
    };
    if let Err(e) = apply_result {
        tracing::warn!(
            community_id = %hex::encode(community_id.0),
            actor = ?event.actor,
            kind = ?event.kind,
            error = ?e,
            "dfrost inbound: apply failed",
        );
        return;
    }

    // 5. Record. Replay tracker advances ONLY after a successful apply.
    {
        let mut t = tracker.lock().await;
        t.record(&event);
    }

    // 6. Emit Tauri event to mirror local-driven progress emitted by the
    //    IPC layer (`dfrost_initiate_dkg`, `dfrost_contribute_dkg_round`,
    //    `dfrost_contribute_threshold_sign`, `dfrost_propose_refresh`).
    //    Inner CBOR-payload decode failures here are non-fatal: the
    //    outer event already applied successfully, so any decode error
    //    indicates a producer / apply invariant mismatch worth a warn
    //    rather than a silent drop. Off-by-one between local and peer
    //    `participants_so_far` counts is acceptable — both reflect the
    //    pending_dkg map AFTER the just-applied event, so they
    //    converge once both sides have observed the same prefix.
    match event.kind {
        DfrostEventKind::DkgRound => {
            match ciborium::de::from_reader::<DkgRoundPayload, _>(&event.payload[..]) {
                Ok(payload) => {
                    let participants_so_far: u8 = {
                        let log = dfrost_log.lock().await;
                        let count = log
                            .committee_state
                            .pending_dkg
                            .as_ref()
                            .map(|p| match payload.round_num {
                                1 => p.round1_packages.len(),
                                2 => p.round2_packages.len(),
                                _ => p.dk_confirmations.len(),
                            })
                            .unwrap_or(0);
                        u8::try_from(count).unwrap_or(u8::MAX)
                    };
                    let evt = DfrostDkgProgressPayload {
                        ceremony_id: hex::encode(payload.ceremony_id),
                        round_num: payload.round_num,
                        participants_so_far,
                    };
                    if let Err(e) = app_handle.emit("dfrost-dkg-progress", &evt) {
                        tracing::warn!(
                            community_id = %hex::encode(community_id.0),
                            error = %e,
                            "dfrost-dkg-progress emit failed (inbound)",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        community_id = %hex::encode(community_id.0),
                        error = %e,
                        "dfrost inbound: DkgRound payload decode failed post-apply",
                    );
                }
            }
        }
        DfrostEventKind::VrfBeacon => {
            match ciborium::de::from_reader::<VrfBeaconPayload, _>(&event.payload[..]) {
                Ok(payload) => {
                    let evt = DfrostBeaconReadyPayload {
                        ceremony_id: hex::encode(payload.ceremony_id),
                        vrf_output: hex::encode(payload.vrf_output),
                    };
                    if let Err(e) = app_handle.emit("dfrost-beacon-ready", &evt) {
                        tracing::warn!(
                            community_id = %hex::encode(community_id.0),
                            error = %e,
                            "dfrost-beacon-ready emit failed (inbound)",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        community_id = %hex::encode(community_id.0),
                        error = %e,
                        "dfrost inbound: VrfBeacon payload decode failed post-apply",
                    );
                }
            }
        }
        DfrostEventKind::ProactiveRefresh => {
            match ciborium::de::from_reader::<RefreshRoundPayload, _>(&event.payload[..]) {
                Ok(payload) => {
                    let evt = DfrostRefreshProgressPayload {
                        ceremony_id: hex::encode(payload.ceremony_id),
                        round_num: payload.round_num,
                    };
                    if let Err(e) = app_handle.emit("dfrost-refresh-progress", &evt) {
                        tracing::warn!(
                            community_id = %hex::encode(community_id.0),
                            error = %e,
                            "dfrost-refresh-progress emit failed (inbound)",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        community_id = %hex::encode(community_id.0),
                        error = %e,
                        "dfrost inbound: ProactiveRefresh payload decode failed post-apply",
                    );
                }
            }
        }
        // DkgComplete (`dk`), ThresholdSign (`ts`), Close — no event mirror.
        // DkgComplete is the silent finalisation handled by `apply`; ts is
        // per-member share collection that aggregates into the VrfBeacon
        // emit above; Close is not yet defined as a kind.
        _ => {}
    }
}

impl<R: tauri::Runtime> DfrostLogEngine<R> {
    pub fn community_id(&self) -> SpaceId {
        self.community_id
    }

    pub async fn start(params: DfrostLogEngineParams<R>) -> Arc<Self> {
        let tracker = Arc::new(Mutex::new(DfrostReplayTracker::new()));
        let community_id = params.community_id;
        let log_for_loop = params.dfrost_log.clone();
        let tracker_for_loop = tracker.clone();
        let app_for_loop = params.app_handle;
        let self_addr_for_loop = params.self_addr;
        let self_x_priv_for_loop = params.self_x25519_priv;
        let resolver_for_loop = params.identity_resolver.clone();
        let mut rx = params.subscriber_rx;

        let receive_handle = tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                process_inbound(
                    community_id,
                    &log_for_loop,
                    &tracker_for_loop,
                    &app_for_loop,
                    &self_addr_for_loop,
                    &self_x_priv_for_loop,
                    &resolver_for_loop,
                    &packet,
                )
                .await;
            }
        });

        Arc::new(Self {
            community_id,
            dfrost_log: params.dfrost_log,
            tracker,
            publisher_tx: params.publisher_tx,
            _receive_handle: receive_handle,
            _phantom: std::marker::PhantomData,
        })
    }

    /// Publish a locally-signed event onto the Zenoh-bridged publisher
    /// channel. Records the event in the dedup tracker BEFORE sending,
    /// so the inevitable loopback (Zenoh adapter subscribes to its own
    /// published topic) hits the inbound dedup gate and is silently
    /// dropped instead of double-applied.
    pub async fn publish_event(&self, event: SignedCommitteeEvent) -> Result<(), String> {
        // 1. Record in tracker FIRST so self-loopback is dropped at the
        //    `process_inbound` dedup step.
        {
            let mut t = self.tracker.lock().await;
            t.record(&event);
        }
        // 2. CBOR-encode.
        let mut packet = Vec::new();
        ciborium::ser::into_writer(&event, &mut packet)
            .map_err(|e| format!("publish_event encode: {e}"))?;
        // 3. Send.
        self.publisher_tx
            .send(packet)
            .await
            .map_err(|e| format!("publish_event send: {e}"))
    }
}

// ── Registry ────────────────────────────────────────────────────────────────

/// Per-`SpaceId` map of running `DfrostLogEngine<R>` instances. Mirrors
/// `VotingLogRegistry` (see `community_voting_log_engine.rs`). Wired into
/// `NodeState` alongside the existing channel-log and voting-log registries
/// in a follow-up task.
pub struct DfrostLogRegistry<R: tauri::Runtime> {
    engines: Mutex<HashMap<SpaceId, Arc<DfrostLogEngine<R>>>>,
}

impl<R: tauri::Runtime> DfrostLogRegistry<R> {
    pub fn new() -> Self {
        Self {
            engines: Mutex::new(HashMap::new()),
        }
    }

    /// Start an engine for `params.community_id` and stash it in the
    /// registry. If an engine already exists for that community it is
    /// replaced — the caller is responsible for shutting the old one
    /// down by dropping their `Arc` (the receive loop will then exit
    /// when the adapter's publisher sender is dropped). Returns the
    /// fresh `Arc` so callers can immediately `publish_event` without
    /// re-doing the `get`.
    pub async fn register(&self, params: DfrostLogEngineParams<R>) -> Arc<DfrostLogEngine<R>> {
        let cid = params.community_id;
        let engine = DfrostLogEngine::start(params).await;
        let mut engines = self.engines.lock().await;
        engines.insert(cid, Arc::clone(&engine));
        engine
    }

    pub async fn get(&self, community_id: SpaceId) -> Option<Arc<DfrostLogEngine<R>>> {
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

impl<R: tauri::Runtime> Default for DfrostLogRegistry<R> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::community_dfrost_log_engine::{
        DfrostLogEngine, DfrostLogEngineParams, DfrostReplayTracker,
    };
    use crate::community_dfrost_types::{
        DfrostEventKind, SignedCommitteeEvent, ThresholdSignPayload,
    };
    use crate::community_state_sync::IdentityResolver;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Minimal in-test `IdentityResolver` backed by a `HashMap`. Used by
    /// both the startup test (with an empty map — never queried because
    /// the test never drives an inbound event) and the inbound-apply
    /// test (populated with Alice's identity composite).
    struct StaticResolver(HashMap<OwnerAddr, [u8; 64]>);

    #[async_trait::async_trait]
    impl IdentityResolver for StaticResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            self.0.get(addr).copied()
        }
    }

    fn test_event(actor: OwnerAddr, wall_ms: u64, logical: u32) -> SignedCommitteeEvent {
        let payload = ThresholdSignPayload {
            ceremony_id: [0u8; 32],
            message_hash: [0u8; 32],
            commitment_bytes: vec![],
            share_bytes: vec![],
        };
        let mut payload_bytes = Vec::new();
        ciborium::ser::into_writer(&payload, &mut payload_bytes).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms,
                logical,
                device_id: "dev-a".into(),
            },
            actor,
            payload: payload_bytes,
            sig: vec![0u8; 64],
        }
    }

    #[test]
    fn replay_tracker_dedups_repeat_event() {
        let mut t = DfrostReplayTracker::new();
        let addr = OwnerAddr([1u8; 16]);
        let e = test_event(addr, 100, 0);
        assert!(!t.contains(&e), "fresh event not contained");
        t.record(&e);
        assert!(t.contains(&e), "recorded event is contained");
    }

    #[test]
    fn replay_tracker_dedups_per_actor_device() {
        let mut t = DfrostReplayTracker::new();
        let addr_a = OwnerAddr([1u8; 16]);
        let addr_b = OwnerAddr([2u8; 16]);
        t.record(&test_event(addr_a, 100, 0));
        assert!(
            !t.contains(&test_event(addr_b, 100, 0)),
            "different actor not deduped"
        );
    }

    #[test]
    fn replay_tracker_advances_on_higher_hlc() {
        let mut t = DfrostReplayTracker::new();
        let addr = OwnerAddr([1u8; 16]);
        t.record(&test_event(addr, 100, 0));
        let later = test_event(addr, 100, 1);
        assert!(!t.contains(&later), "advancing logical not deduped");
        t.record(&later);
        assert!(t.contains(&later), "advanced event recorded");
        assert!(t.contains(&test_event(addr, 100, 0)));
    }

    #[tokio::test]
    async fn engine_start_returns_handle_and_drops_cleanly() {
        let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let log = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::community_dfrost_log::DfrostLog::new(),
        ));
        let community_id = crate::owner_state_types::SpaceId([0u8; 16]);
        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();

        let resolver: Arc<dyn IdentityResolver + Send + Sync> =
            Arc::new(StaticResolver(HashMap::new()));
        let engine = DfrostLogEngine::start(DfrostLogEngineParams {
            community_id,
            dfrost_log: log.clone(),
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            app_handle,
            self_addr: crate::owner_state_types::OwnerAddr([0u8; 16]),
            self_x25519_priv: [0u8; 32],
            identity_resolver: resolver,
        })
        .await;

        assert_eq!(engine.community_id(), community_id);
        drop(sub_tx); // signal end-of-stream
        drop(engine);
    }

    /// Build an `(SigningKey, OwnerAddr, identity_pub_64)` triple from a
    /// seed where `address_hash` binds correctly — mirrors
    /// `community_channel_log::tests::fixture_identity`. Required for any
    /// test exercising `verify_signed_committee_event`'s binding chain.
    fn fixture_identity(seed: u8) -> (ed25519_dalek::SigningKey, OwnerAddr, [u8; 64]) {
        let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
        let owner = OwnerAddr(priv_id.identity.address_hash);
        let pub_64 = priv_id.identity.to_public_bytes();
        // PrivateIdentity::signing_key is private; round-trip through
        // to_private_bytes (X25519_secret(32) || Ed25519_secret(32)).
        let private_bytes = priv_id.to_private_bytes();
        let mut ed_secret = [0u8; 32];
        ed_secret.copy_from_slice(&private_bytes[32..64]);
        let signing = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
        (signing, owner, pub_64)
    }

    /// Inbound chain end-to-end: a CBOR-encoded `dr rn=1` event signed
    /// by Alice lands on the subscriber, the receive loop verifies +
    /// applies it, and `pending_dkg.round1_packages` gains Alice's
    /// entry. Single-member committee (Alice only) keeps the setup
    /// minimal; the cross-engine multi-member case is covered by the
    /// Task 10 integration test.
    #[tokio::test]
    async fn engine_processes_valid_inbound_event() {
        use crate::community_dfrost_log::{build_signed_dfrost_event, DfrostLog, PendingCeremony};
        use crate::community_dfrost_types::DkgRoundPayload;
        use crate::owner_state_types::SpaceId;

        // 1. Identity setup: Alice with proper address_hash binding.
        let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xA1);
        let alice_x_priv = *crate::dm_signing::ed25519_priv_to_x25519(&alice_sk);

        let mut resolver_map = HashMap::new();
        resolver_map.insert(alice_addr, alice_pub64);
        let resolver: Arc<dyn IdentityResolver + Send + Sync> =
            Arc::new(StaticResolver(resolver_map));

        // 2. Log + pending DKG seeded so the apply path has a ceremony
        //    to attach the rn=1 contribution to.
        let community_id = SpaceId([0xC0; 16]);
        let ceremony_id = [0x42u8; 32];
        let mut initial_log = DfrostLog::new();
        initial_log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id,
            members: vec![alice_addr],
            threshold: 1,
            max_signers: 1,
            proposed_epoch: 1,
            ..Default::default()
        });
        let log = Arc::new(tokio::sync::Mutex::new(initial_log));

        // 3. Build a properly-signed dr rn=1 event.
        let payload = DkgRoundPayload {
            ceremony_id,
            round_num: 1,
            round1_package: Some(vec![0xde, 0xad, 0xbe, 0xef]),
            recipient_ciphertexts: None,
        };
        let event = build_signed_dfrost_event(
            &alice_sk,
            alice_addr,
            DfrostEventKind::DkgRound,
            &payload,
            Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        )
        .expect("build_signed_dfrost_event rn=1");

        let mut packet = Vec::new();
        ciborium::ser::into_writer(&event, &mut packet).expect("ciborium encode");

        // 4. Spin up the engine.
        let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();

        let _engine = DfrostLogEngine::start(DfrostLogEngineParams {
            community_id,
            dfrost_log: log.clone(),
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            app_handle,
            self_addr: alice_addr,
            self_x25519_priv: alice_x_priv,
            identity_resolver: resolver,
        })
        .await;

        // 5. Push the inbound packet + wait for the receive loop to drain.
        sub_tx.send(packet).await.expect("send inbound");
        // Yield + small sleep — the receive loop is an independent tokio
        // task, so we need to surrender the runtime long enough for it
        // to pull the packet, run async resolver + verify, and acquire
        // the log lock. 50ms is well above the actual processing budget
        // (verify is microseconds; lock acquisition is unblocked).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 6. Assert: Alice's rn=1 contribution lands in pending_dkg.
        let log_guard = log.lock().await;
        let pending = log_guard
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending_dkg still present after rn=1 apply");
        assert!(
            pending.round1_packages.contains_key(&alice_addr),
            "Alice's rn=1 package must land in pending_dkg.round1_packages after inbound apply"
        );
    }

    /// Negative: a SignedCommitteeEvent with a corrupted signature MUST
    /// be dropped at the verify gate — no apply, no replay-tracker
    /// advance, no state mutation. Defence against an attacker injecting
    /// a malformed/forged envelope onto the topic.
    #[tokio::test]
    async fn engine_drops_event_with_bad_signature() {
        use crate::community_dfrost_log::{build_signed_dfrost_event, DfrostLog, PendingCeremony};
        use crate::community_dfrost_types::DkgRoundPayload;
        use crate::owner_state_types::SpaceId;

        // Same identity setup as the positive test.
        let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xA1);
        let alice_x_priv = *crate::dm_signing::ed25519_priv_to_x25519(&alice_sk);

        let mut resolver_map = HashMap::new();
        resolver_map.insert(alice_addr, alice_pub64);
        let resolver: Arc<dyn IdentityResolver + Send + Sync> =
            Arc::new(StaticResolver(resolver_map));

        let community_id = SpaceId([0xC0; 16]);
        let ceremony_id = [0x42u8; 32];
        let mut initial_log = DfrostLog::new();
        initial_log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id,
            members: vec![alice_addr],
            threshold: 1,
            max_signers: 1,
            proposed_epoch: 1,
            ..Default::default()
        });
        let log = Arc::new(tokio::sync::Mutex::new(initial_log));

        let payload = DkgRoundPayload {
            ceremony_id,
            round_num: 1,
            round1_package: Some(vec![0xde, 0xad]),
            recipient_ciphertexts: None,
        };
        let mut event = build_signed_dfrost_event(
            &alice_sk,
            alice_addr,
            DfrostEventKind::DkgRound,
            &payload,
            Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        )
        .expect("build_signed_dfrost_event rn=1");

        // Tamper the signature — flipping a single bit is enough for
        // verify_strict to reject.
        event.sig[0] ^= 0x01;

        let mut packet = Vec::new();
        ciborium::ser::into_writer(&event, &mut packet).expect("ciborium encode");

        let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();

        let _engine = DfrostLogEngine::start(DfrostLogEngineParams {
            community_id,
            dfrost_log: log.clone(),
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            app_handle,
            self_addr: alice_addr,
            self_x25519_priv: alice_x_priv,
            identity_resolver: resolver,
        })
        .await;

        sub_tx.send(packet).await.expect("send inbound");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Assert: pending_dkg.round1_packages is still empty — the
        // bad-sig event was dropped at the verify gate.
        let log_guard = log.lock().await;
        let pending = log_guard
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending_dkg still present");
        assert!(
            pending.round1_packages.is_empty(),
            "round1_packages must remain empty when sig verify fails"
        );
    }

    /// Task 4: after a successful inbound DKG rn=1 apply, the engine
    /// MUST emit `dfrost-dkg-progress` on the Tauri event bus with the
    /// same payload shape the IPC local-drive path emits
    /// (`DfrostDkgProgressPayload` with hex ceremony_id, round_num,
    /// participants_so_far). Mirrors the
    /// `publish_emits_channel_message_received_event` pattern in
    /// `community_channel_log_engine`.
    #[tokio::test]
    async fn engine_emits_dkg_progress_on_inbound_apply() {
        use crate::community_dfrost_log::{build_signed_dfrost_event, DfrostLog, PendingCeremony};
        use crate::community_dfrost_types::DkgRoundPayload;
        use crate::owner_state_types::SpaceId;
        use std::sync::Mutex as StdMutex;
        use std::time::Duration;
        use tauri::Listener;

        // Identity + log setup mirrors engine_processes_valid_inbound_event.
        let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xA1);
        let alice_x_priv = *crate::dm_signing::ed25519_priv_to_x25519(&alice_sk);

        let mut resolver_map = HashMap::new();
        resolver_map.insert(alice_addr, alice_pub64);
        let resolver: Arc<dyn IdentityResolver + Send + Sync> =
            Arc::new(StaticResolver(resolver_map));

        let community_id = SpaceId([0xC0; 16]);
        let ceremony_id = [0x42u8; 32];
        let mut initial_log = DfrostLog::new();
        initial_log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id,
            members: vec![alice_addr],
            threshold: 1,
            max_signers: 1,
            proposed_epoch: 1,
            ..Default::default()
        });
        let log = Arc::new(tokio::sync::Mutex::new(initial_log));

        let payload = DkgRoundPayload {
            ceremony_id,
            round_num: 1,
            round1_package: Some(vec![0xde, 0xad, 0xbe, 0xef]),
            recipient_ciphertexts: None,
        };
        let event = build_signed_dfrost_event(
            &alice_sk,
            alice_addr,
            DfrostEventKind::DkgRound,
            &payload,
            Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        )
        .expect("build_signed_dfrost_event rn=1");

        let mut packet = Vec::new();
        ciborium::ser::into_writer(&event, &mut packet).expect("ciborium encode");

        let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();

        // Register listener BEFORE starting the engine so we don't race
        // the inbound apply.
        let captured: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_for_listener = Arc::clone(&captured);
        app_handle.listen("dfrost-dkg-progress", move |evt| {
            captured_for_listener
                .lock()
                .expect("captured lock")
                .push(evt.payload().to_string());
        });

        let _engine = DfrostLogEngine::start(DfrostLogEngineParams {
            community_id,
            dfrost_log: log.clone(),
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            app_handle,
            self_addr: alice_addr,
            self_x25519_priv: alice_x_priv,
            identity_resolver: resolver,
        })
        .await;

        sub_tx.send(packet).await.expect("send inbound");

        // Poll for the listener to fire — receive loop is an independent
        // tokio task and the Tauri event bus has its own dispatch latency.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let captured_payload = loop {
            {
                let v = captured.lock().expect("captured lock");
                if let Some(first) = v.first().cloned() {
                    break first;
                }
            }
            if std::time::Instant::now() >= deadline {
                panic!("dfrost-dkg-progress event not received within 1s");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        let parsed: serde_json::Value =
            serde_json::from_str(&captured_payload).expect("payload is JSON");
        assert_eq!(
            parsed["ceremonyId"].as_str(),
            Some(hex::encode(ceremony_id).as_str()),
            "ceremonyId must be hex of payload.ceremony_id",
        );
        assert_eq!(
            parsed["roundNum"].as_u64(),
            Some(1),
            "roundNum must be 1 for rn=1 inbound",
        );
        assert_eq!(
            parsed["participantsSoFar"].as_u64(),
            Some(1),
            "participantsSoFar reflects pending_dkg.round1_packages.len() after apply",
        );
    }

    /// Task 5: `publish_event` must CBOR-encode the event and forward
    /// the bytes on the publisher channel. The round-trip decode here
    /// pins the on-wire format (CBOR of `SignedCommitteeEvent`) for
    /// the eventual Zenoh adapter.
    #[tokio::test]
    async fn engine_publish_event_sends_cbor_on_publisher_tx() {
        use crate::owner_state_types::SpaceId;

        let (pub_tx, mut pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (_sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let log = Arc::new(tokio::sync::Mutex::new(
            crate::community_dfrost_log::DfrostLog::new(),
        ));
        let community_id = SpaceId([0u8; 16]);
        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();
        let resolver: Arc<dyn IdentityResolver + Send + Sync> =
            Arc::new(StaticResolver(HashMap::new()));

        let engine = DfrostLogEngine::start(DfrostLogEngineParams {
            community_id,
            dfrost_log: log,
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            app_handle,
            self_addr: OwnerAddr([0u8; 16]),
            self_x25519_priv: [0u8; 32],
            identity_resolver: resolver,
        })
        .await;

        let payload = ThresholdSignPayload {
            ceremony_id: [7u8; 32],
            message_hash: [8u8; 32],
            commitment_bytes: vec![],
            share_bytes: vec![],
        };
        let mut payload_bytes = Vec::new();
        ciborium::ser::into_writer(&payload, &mut payload_bytes).unwrap();
        let event = SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "dev-a".into(),
            },
            actor: OwnerAddr([1u8; 16]),
            payload: payload_bytes,
            sig: vec![0u8; 64],
        };

        engine.publish_event(event.clone()).await.expect("publish");
        let bytes = pub_rx.recv().await.expect("packet");
        let decoded: SignedCommitteeEvent = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded.actor, event.actor);
        assert_eq!(decoded.kind as u8, event.kind as u8);
        assert_eq!(decoded.hlc.wall_ms, event.hlc.wall_ms);
        assert_eq!(decoded.hlc.device_id, event.hlc.device_id);
    }

    /// Task 5: `publish_event` MUST record the event in the dedup
    /// tracker BEFORE sending. The Zenoh adapter subscribes to its own
    /// published topic, so the loopback packet comes back through
    /// `subscriber_rx` — the inbound dedup gate must drop it.
    #[tokio::test]
    async fn engine_publish_event_records_in_tracker_before_send() {
        use crate::community_dfrost_log::{build_signed_dfrost_event, DfrostLog, PendingCeremony};
        use crate::community_dfrost_types::DkgRoundPayload;
        use crate::owner_state_types::SpaceId;

        // Identity + log setup so the loopback would otherwise apply.
        let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xA1);
        let alice_x_priv = *crate::dm_signing::ed25519_priv_to_x25519(&alice_sk);

        let mut resolver_map = HashMap::new();
        resolver_map.insert(alice_addr, alice_pub64);
        let resolver: Arc<dyn IdentityResolver + Send + Sync> =
            Arc::new(StaticResolver(resolver_map));

        let community_id = SpaceId([0xC0; 16]);
        let ceremony_id = [0x42u8; 32];
        let mut initial_log = DfrostLog::new();
        initial_log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id,
            members: vec![alice_addr],
            threshold: 1,
            max_signers: 1,
            proposed_epoch: 1,
            ..Default::default()
        });
        let log = Arc::new(tokio::sync::Mutex::new(initial_log));

        let payload = DkgRoundPayload {
            ceremony_id,
            round_num: 1,
            round1_package: Some(vec![0xde, 0xad, 0xbe, 0xef]),
            recipient_ciphertexts: None,
        };
        let event = build_signed_dfrost_event(
            &alice_sk,
            alice_addr,
            DfrostEventKind::DkgRound,
            &payload,
            Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        )
        .expect("build_signed_dfrost_event rn=1");

        let (pub_tx, mut pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();

        let engine = DfrostLogEngine::start(DfrostLogEngineParams {
            community_id,
            dfrost_log: log.clone(),
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            app_handle,
            self_addr: alice_addr,
            self_x25519_priv: alice_x_priv,
            identity_resolver: resolver,
        })
        .await;

        // Publish — this should record-then-send.
        engine.publish_event(event.clone()).await.expect("publish");
        let loopback_bytes = pub_rx.recv().await.expect("packet emitted");

        // Simulate the Zenoh adapter's self-loopback: feed the same
        // bytes back through subscriber_rx.
        sub_tx.send(loopback_bytes).await.expect("loopback inbound");

        // Give the receive loop time to process (verify + dedup-drop).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Assert: pending_dkg.round1_packages is still EMPTY — the
        // loopback was dropped at the dedup gate because publish_event
        // already advanced the tracker.
        let log_guard = log.lock().await;
        let pending = log_guard
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending_dkg still present");
        assert!(
            pending.round1_packages.is_empty(),
            "self-loopback must be dropped at dedup gate (publish_event records first)",
        );
    }

    // ── Registry tests ─────────────────────────────────────────────────────

    use crate::community_dfrost_log_engine::DfrostLogRegistry;
    use crate::owner_state_types::SpaceId;

    /// Build a minimal `DfrostLogEngineParams` for a given `community_id`.
    /// Uses an empty resolver / zero keys; never drives an inbound event so
    /// the resolver is never queried. The discarded peer ends (`_sub_tx` /
    /// `_pub_rx`) drop at end-of-function — the receive loop will exit on
    /// end-of-stream but the engine itself remains valid in the registry,
    /// which is what these tests are exercising.
    fn registry_test_params(
        community_id: SpaceId,
        app_handle: tauri::AppHandle<tauri::test::MockRuntime>,
    ) -> DfrostLogEngineParams<tauri::test::MockRuntime> {
        let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (_sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let log = Arc::new(tokio::sync::Mutex::new(
            crate::community_dfrost_log::DfrostLog::new(),
        ));
        let resolver: Arc<dyn IdentityResolver + Send + Sync> =
            Arc::new(StaticResolver(HashMap::new()));
        DfrostLogEngineParams {
            community_id,
            dfrost_log: log,
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            app_handle,
            self_addr: OwnerAddr([0u8; 16]),
            self_x25519_priv: [0u8; 32],
            identity_resolver: resolver,
        }
    }

    #[tokio::test]
    async fn registry_register_and_get_round_trips() {
        let reg: DfrostLogRegistry<tauri::test::MockRuntime> = DfrostLogRegistry::new();
        let community_id = SpaceId([7u8; 16]);
        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();

        let _engine = reg
            .register(registry_test_params(community_id, app_handle))
            .await;
        assert!(reg.get(community_id).await.is_some());
        assert!(reg.get(SpaceId([99u8; 16])).await.is_none());
    }

    #[tokio::test]
    async fn registry_shutdown_drops_all_engines() {
        let reg: DfrostLogRegistry<tauri::test::MockRuntime> = DfrostLogRegistry::new();
        let community_id_a = SpaceId([1u8; 16]);
        let community_id_b = SpaceId([2u8; 16]);
        let app = tauri::test::mock_app();

        reg.register(registry_test_params(community_id_a, app.handle().clone()))
            .await;
        reg.register(registry_test_params(community_id_b, app.handle().clone()))
            .await;
        assert!(reg.get(community_id_a).await.is_some());
        assert!(reg.get(community_id_b).await.is_some());

        reg.shutdown().await;
        assert!(reg.get(community_id_a).await.is_none());
        assert!(reg.get(community_id_b).await.is_none());
    }
}
