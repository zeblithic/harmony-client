//! ZEB-270 Phase 3 integration test: two ChannelLogEngines on a
//! shared in-memory Zenoh router exercise live broadcast,
//! offline-then-backfill, and replay rejection.
//!
//! Per spec §14.2.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_channel_log::{
    derive_channel_key, encrypt_channel_packet, ChannelLogConfig, CommunityStateAtHlc,
    CommunityStateSnapshot,
};
use harmony_app::community_channel_log_engine::{
    ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig, SpawnOutcome,
};
use harmony_app::community_membership::{ChannelId, ChannelInfo, ChannelKind};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use harmony_identity::PrivateIdentity;
use tempfile::TempDir;
use tokio::sync::{mpsc, Mutex};

/// Small seal threshold so 150 events produce ≥ 18 sealed segments
/// — exercises seal/reload paths in the engine receive loop.
/// Per spec §14.2.
const TEST_SEAL_THRESHOLD: usize = 8;

/// Build a deterministic `(SigningKey, OwnerAddr, identity_pub_64)`
/// triple for a single seed byte. Mirrors the engine-test helper at
/// `community_channel_log_engine.rs::tests::fixture_identity`.
fn fixture_identity(seed: u8) -> (SigningKey, OwnerAddr, [u8; 64]) {
    let priv_id = PrivateIdentity::from_seed(&[seed; 32]);
    let owner = OwnerAddr(priv_id.identity.address_hash);
    let pub_64 = priv_id.identity.to_public_bytes();
    let private_bytes = priv_id.to_private_bytes();
    let mut ed_secret = [0u8; 32];
    ed_secret.copy_from_slice(&private_bytes[32..64]);
    let signing = SigningKey::from_bytes(&ed_secret);
    (signing, owner, pub_64)
}

/// State stub: every author here is a Joined member with full power.
/// Suitable for the two-engine test where the verify chain only needs
/// to admit A's posts at B (and vice-versa) — channel-config materialize
/// is out of scope.
struct BothJoinedState {
    a: OwnerAddr,
    b: OwnerAddr,
    channel_id: ChannelId,
    // ZEB-399: each member's enrolled device verifying key (ed25519).
    // verify_channel_event authenticates posts against these — the
    // membership trust root — not a DM-layer identity resolver.
    enrolled_a: [u8; 32],
    enrolled_b: [u8; 32],
}

#[async_trait::async_trait]
impl CommunityStateAtHlc for BothJoinedState {
    async fn snapshot_at(
        &self,
        channel_id: &ChannelId,
        author: &OwnerAddr,
        _at: &Hlc,
    ) -> CommunityStateSnapshot {
        let channel = if channel_id == &self.channel_id {
            Some(ChannelInfo {
                name: "general".to_string(),
                write_power: 0,
                kind: ChannelKind::Text,
                created_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "test".to_string(),
                },
                deleted_at: None,
            })
        } else {
            None
        };
        let author_power = if author == &self.a || author == &self.b {
            Some(100)
        } else {
            None
        };
        let author_enrolled_keys = if author == &self.a {
            vec![self.enrolled_a]
        } else if author == &self.b {
            vec![self.enrolled_b]
        } else {
            vec![]
        };
        CommunityStateSnapshot {
            channel,
            author_power,
            author_enrolled_keys,
        }
    }
}

/// Drainer task plumbing: stand-in for `event_loop::run`'s
/// adapter-bridge select arm. Each `ChannelLogAdapterRequest` posted
/// through `adapter_request_tx` is re-bound to the supplied Zenoh
/// session via `spawn_channel_log_zenoh_adapter` — exact same
/// call shape as the production event_loop arm.
///
/// Returns a JoinHandle whose lifetime must outlive every spawn under
/// the registry — when the matching `adapter_request_tx` drops, the
/// drainer's `recv()` returns None and the task exits.
fn spawn_adapter_bridge_drainer(
    session: Arc<zenoh::Session>,
    mut adapter_request_rx: mpsc::UnboundedReceiver<
        harmony_app::event_loop::ChannelLogAdapterRequest,
    >,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = adapter_request_rx.recv().await {
            let _handle = harmony_app::event_loop::spawn_channel_log_zenoh_adapter(
                Arc::clone(&session),
                req.community_id_hex,
                req.channel_id_hex,
                req.publisher_rx,
                req.subscriber_tx,
                req.query_request_rx,
                req.read_for_query,
                req.emit_backfill_progress,
                req.backfill_progress_interval,
                req.backfill_default_limit,
                req.closing,
                req.rbsr_hooks,
            );
            // JoinHandle dropped — adapter task is fire-and-forget;
            // closing flag (held by registry) signals shutdown.
        }
    })
}

/// Poll an async predicate until it returns true or timeout elapses.
async fn wait_until<F, Fut>(mut predicate: F, timeout: Duration) -> Result<(), ()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if predicate().await {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll the received-event counter until it stops growing for
/// `stable_for_polls` consecutive 100ms intervals (= ~500ms quiet),
/// or `timeout` elapses. Used by the offline-window checks to confirm
/// B's adapter has drained after `stop()` and stays quiet while A keeps
/// publishing — i.e. a *negative* drain signal (nothing more should
/// arrive). (ZEB-686 retired its former use as the replay-phase
/// readiness barrier — a quiet-window is the wrong shape for a
/// *positive* "backfill done" wait and flaked under load; the replay
/// checkpoint now counts the pinned replayed message-id instead.)
///
/// `Ok(count)` — counter was stable for the requested window.
/// `Err(count)` — timeout fired before stability was observed (the
/// counter is still growing); callers MUST treat this as a loud
/// failure, otherwise the test would silently accept a non-stable
/// snapshot and could mask post-stop leaks under load. (ZEB-288 R1
/// Qodo finding.)
async fn wait_for_stable_count(
    counter: &Arc<std::sync::Mutex<Vec<String>>>,
    stable_for_polls: usize,
    timeout: Duration,
) -> Result<usize, usize> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = counter.lock().expect("count lock").len();
    let mut stable = 0usize;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = counter.lock().expect("count lock").len();
        if now == last {
            stable += 1;
            if stable >= stable_for_polls {
                return Ok(now);
            }
        } else {
            stable = 0;
            last = now;
        }
    }
    Err(counter.lock().expect("count lock").len())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_live_then_offline_backfill_with_replay_rejection() {
    use std::sync::Mutex as StdMutex;

    // ── Set up shared in-memory Zenoh router ──────────────────────────
    // Two `zenoh::open(default_config)` sessions on the same in-memory
    // peer router — they discover each other via gossip.
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    // ── Set up identities A + B ──────────────────────────────────────
    let (signing_a_raw, owner_a, _pub_a) = fixture_identity(0xAA);
    let (signing_b_raw, owner_b, _pub_b) = fixture_identity(0xBB);
    let signing_a = Arc::new(signing_a_raw);
    let signing_b = Arc::new(signing_b_raw);

    // ── Set up shared community + channel ────────────────────────────
    let community_id = SpaceId([0xc0; 16]);
    let channel_id = ChannelId([0xc1; 16]);
    let membership_key = EpochKey::new([0x77; 32]);
    let channel_key = derive_channel_key(&membership_key, &community_id, &channel_id);

    // ── Set up event sinks for each side (ZEB-445: mode-agnostic
    //    NodeEventSink instead of Tauri mock apps). Side A never asserts
    //    on emissions — empty fan-out. Side B records the two event
    //    streams the test asserts on: `channel-message-received`
    //    (messageId per frame) and `channel-backfill-progress` (count).
    struct SideBRecorder {
        received: Arc<StdMutex<Vec<String>>>,
        backfill_progress: Arc<StdMutex<u32>>,
    }
    impl harmony_app::node_event_sink::NodeEventSink for SideBRecorder {
        fn emit(&self, event: &str, payload: serde_json::Value) {
            match event {
                "channel-message-received" => {
                    let msg_id = payload["message"]["messageId"]
                        .as_str()
                        .expect("messageId")
                        .to_string();
                    self.received.lock().expect("received_b lock").push(msg_id);
                }
                "channel-backfill-progress" => {
                    *self
                        .backfill_progress
                        .lock()
                        .expect("backfill progress lock") += 1;
                }
                _ => {}
            }
        }
    }
    let received_b: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let backfill_progress_b: Arc<StdMutex<u32>> = Arc::new(StdMutex::new(0));
    let sink_a: Arc<dyn harmony_app::node_event_sink::NodeEventSink> =
        Arc::new(harmony_app::node_event_sink::FanoutSink(vec![]));
    let sink_b: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = Arc::new(SideBRecorder {
        received: Arc::clone(&received_b),
        backfill_progress: Arc::clone(&backfill_progress_b),
    });

    let dir_a = TempDir::new().expect("tmp A");
    let dir_b = TempDir::new().expect("tmp B");

    // ── Construct stubs (verify-chain shortcuts for the test) ───────
    let enrolled_a = signing_a.verifying_key().to_bytes();
    let enrolled_b = signing_b.verifying_key().to_bytes();
    let state_a: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(BothJoinedState {
        a: owner_a,
        b: owner_b,
        channel_id,
        enrolled_a,
        enrolled_b,
    });
    let state_b: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(BothJoinedState {
        a: owner_a,
        b: owner_b,
        channel_id,
        enrolled_a,
        enrolled_b,
    });

    let tracker_a: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>> =
        Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "device-a".to_string(),
        )));
    let tracker_b: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>> =
        Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "device-a".to_string(),
        )));

    // ── Adapter-request bridges + drainers (mirrors registry test fixture) ──
    let (adapter_tx_a, adapter_rx_a) = mpsc::unbounded_channel();
    let (adapter_tx_b, adapter_rx_b) = mpsc::unbounded_channel();
    let _drainer_a = spawn_adapter_bridge_drainer(Arc::clone(&session_a), adapter_rx_a);
    let _drainer_b = spawn_adapter_bridge_drainer(Arc::clone(&session_b), adapter_rx_b);

    // ── Build registries ─────────────────────────────────────────────
    let registry_a = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        device_cipher: harmony_app::device_dataset_file::test_cipher(),
        adapter_request_tx: adapter_tx_a,
        sink: sink_a,
        identity_dir: dir_a.path().to_path_buf(),
        self_owner: owner_a,
        self_device_id: "device-a".to_string(),
        signing_key: Arc::clone(&signing_a),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        engine_config: ChannelLogEngineConfig {
            log_config: ChannelLogConfig {
                seal_threshold_events: TEST_SEAL_THRESHOLD,
            },
            ..Default::default()
        },
        transport_epoch_rx: None,
        // ZEB-599 Direction 1: no presence watch in this integration harness.
        presence_resync_rx: None,
    });
    let registry_b = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        device_cipher: harmony_app::device_dataset_file::test_cipher(),
        adapter_request_tx: adapter_tx_b,
        sink: sink_b,
        identity_dir: dir_b.path().to_path_buf(),
        self_owner: owner_b,
        self_device_id: "device-b".to_string(),
        signing_key: Arc::clone(&signing_b),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        engine_config: ChannelLogEngineConfig {
            log_config: ChannelLogConfig {
                seal_threshold_events: TEST_SEAL_THRESHOLD,
            },
            ..Default::default()
        },
        transport_epoch_rx: None,
        // ZEB-599 Direction 1: no presence watch in this integration harness.
        presence_resync_rx: None,
    });

    // B's `channel-message-received` + `channel-backfill-progress`
    // emissions (spec §14.2 requires at least one progress event during
    // the backfill phase) are captured by `SideBRecorder` above —
    // `received_b` / `backfill_progress_b` are asserted on below.

    let engine_a = match Arc::clone(&registry_a)
        .spawn(
            community_id,
            channel_id,
            channel_key.clone(),
            None,
            Arc::clone(&state_a),
            Arc::clone(&tracker_a),
        )
        .await
        .expect("spawn A")
    {
        SpawnOutcome::Spawned(e) => e,
        SpawnOutcome::DeferredForCommit => panic!("unexpected deferred spawn"),
    };
    let _engine_b = match Arc::clone(&registry_b)
        .spawn(
            community_id,
            channel_id,
            channel_key.clone(),
            None,
            Arc::clone(&state_b),
            Arc::clone(&tracker_b),
        )
        .await
        .expect("spawn B")
    {
        SpawnOutcome::Spawned(e) => e,
        SpawnOutcome::DeferredForCommit => panic!("unexpected deferred spawn"),
    };

    // Deterministically wait until A's session has discovered and matched B's
    // REMOTE channel subscriber before publishing, rather than a fixed sleep that
    // races CI scheduling. The two engines run on separate loopback zenoh
    // sessions, so matching B's subscriber depends on inter-session discovery —
    // the contention-sensitive step. A fixed 1s wait flaked under full-suite CI
    // load (ZEB-459): A's first burst published before B's subscriber was
    // matched, and those messages were lost on the live path (the live wire does
    // not replay), stalling the first drain checkpoint until its 30s ceiling.
    //
    // `Locality::Remote` is load-bearing: A's own channel-log adapter also
    // declares a subscriber on this key on `session_a` (event_loop.rs), so a
    // default (`Any`) matching check would be satisfied by A's *local* subscriber
    // and pass before B was reachable — reintroducing the race. Restricting to
    // remote subscribers waits for B specifically. `matching_status` reads the
    // session's routing tables (no network round-trip), so an `Err` means a real
    // session failure — surface it loudly rather than masking it as a timeout.
    let readiness_topic = format!(
        "harmony/channels/{}/{}/events",
        hex::encode(community_id.0),
        hex::encode(channel_id.0)
    );
    let readiness_pub = session_a
        .declare_publisher(
            zenoh::key_expr::KeyExpr::try_from(readiness_topic).expect("readiness topic key"),
        )
        .allowed_destination(zenoh::sample::Locality::Remote)
        .await
        .expect("declare readiness publisher");
    let readiness_deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let matched = readiness_pub
            .matching_status()
            .await
            .expect("matching_status query failed")
            .matching();
        if matched {
            break;
        }
        assert!(
            std::time::Instant::now() < readiness_deadline,
            "A never matched B's remote channel subscriber within 30s \
             (inter-session zenoh discovery did not complete — live wire not ready)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Brief settle so the matched route is fully installed before the first put.
    tokio::time::sleep(Duration::from_millis(250)).await;

    // ── Phase 1: A posts 100 messages live ───────────────────────────
    // ZEB-288: publish in small DRAIN-SYNCED batches so the engine→adapter
    // publisher channel (cap-64, `try_send` drop-on-full) never fills.
    // `try_send` only drops on a *full* channel, so by waiting for B to
    // catch up to each checkpoint before bursting the next batch we keep
    // the in-flight count at ≤ BATCH (< 64) — which makes live delivery
    // LOSSLESS and the Phase-1 assertion deterministic. This resolves the
    // tension the earlier attempts hit: a fixed floor of 90 flaked when CI
    // starvation let the publisher outrun the adapter and drop > 10
    // (Qodo + CodeAnt), while a ≥ 1 floor was too weak to catch a genuine
    // live-path regression (Qodo). Bounding the burst gives us BOTH: all
    // 100 must arrive live, and no scheduling can cause a spurious drop.
    const BATCH: usize = 10;
    let mut posted_ids = Vec::new();
    for i in 0..100 {
        let id = Arc::clone(&engine_a)
            .publish(format!("msg-{i}").into_bytes(), None, None, None)
            .await
            .expect("publish");
        posted_ids.push(id);
        if (i + 1) % BATCH == 0 {
            // Drain checkpoint: B must catch up to everything posted so
            // far before we burst more, keeping publisher_tx well under
            // its 64 cap (≤ BATCH in flight at any instant → no drop).
            let target = i + 1;
            wait_until(
                || {
                    let received_b = Arc::clone(&received_b);
                    async move { received_b.lock().expect("received_b lock").len() >= target }
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap_or_else(|()| {
                panic!("B should receive the first {target} live messages (live wire stalled)")
            });
        }
    }

    // All 100 delivered live and losslessly. The final checkpoint
    // (i + 1 == 100) already waited for this; assert explicitly so the
    // Phase-1 contract — strong (full volume) AND deterministic — is
    // unmistakable. The loss-free guarantee for the *offline* gap is
    // Phase 3's ≥ 150 completeness assert + the final exact-count/no-dupes
    // check (151 with the Phase-4 ordering sentinel).
    {
        let got = received_b.lock().expect("received_b lock").len();
        assert!(
            got >= 100,
            "Phase 1 should deliver all 100 messages live (got {got})"
        );
    }

    // ── Phase 2: B disconnect; A posts 50 more ───────────────────────
    // Stop B's engine + adapter. The on-disk segments persist (spec
    // §17.4) so the backfill phase will dedupe against them by
    // message_id when B re-spawns.
    registry_b
        .stop(&community_id, &channel_id)
        .await
        .expect("stop B");

    // ZEB-288: `stop()` shuts the engine down (its receive loop
    // re-checks `closing` every iteration, so it exits within one
    // packet even mid-flow) and flips the adapter's `closing` flag,
    // whose select arms poll on a 1s interval — so a packet already
    // queued between adapter and engine can still deliver briefly
    // after stop returns. A fixed wall-clock sleep here is the wrong
    // shape (200ms << 1s poll; failed under load during ZEB-250
    // baseline). Instead, wait for B's received counter to actually
    // stop growing — 5 quiet polls of 100ms each = 500ms of confirmed
    // silence — which proves the pipeline has drained regardless of
    // host load or future poll-interval changes.
    let received_at_offline_start = wait_for_stable_count(&received_b, 5, Duration::from_secs(5))
        .await
        .unwrap_or_else(|count| {
            panic!(
                "B never stopped receiving within 5s after registry_b.stop(); \
             last observed count = {count}. Adapter teardown is stuck or \
             the poll interval has regressed."
            )
        });

    for i in 100..150 {
        Arc::clone(&engine_a)
            .publish(format!("msg-{i}").into_bytes(), None, None, None)
            .await
            .expect("publish offline");
        if (i + 1) % 16 == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // Re-confirm B stays quiet across a second stable-count window. If
    // any of the 50 publishes leak past the (already-stopped) adapter,
    // we want a loud failure — both via the timeout (counter still
    // growing) and via the slack assertion below (counter grew too much).
    let received_at_offline_end = wait_for_stable_count(&received_b, 5, Duration::from_secs(3))
        .await
        .unwrap_or_else(|count| {
            panic!(
                "B kept receiving past stop+publish window; last observed \
             count = {count}, baseline was {received_at_offline_start}. \
             Adapter is still delivering after stop."
            )
        });
    // Slack of ≤5 covers any in-flight events that were already queued
    // between B's adapter and engine when `closing` was first observed.
    assert!(
        (received_at_offline_start..=received_at_offline_start + 5)
            .contains(&received_at_offline_end),
        "B should be at ~{received_at_offline_start} during offline (got {received_at_offline_end})"
    );

    // ── Phase 3: B reconnects + backfill ─────────────────────────────
    // Re-spawn B's engine. The on-disk tail+segments are reloaded; the
    // existing 100 events are visible locally already.
    let engine_b2 = match Arc::clone(&registry_b)
        .spawn(
            community_id,
            channel_id,
            channel_key.clone(),
            None,
            Arc::clone(&state_b),
            Arc::clone(&tracker_b),
        )
        .await
        .expect("re-spawn B")
    {
        SpawnOutcome::Spawned(e) => e,
        SpawnOutcome::DeferredForCommit => panic!("unexpected deferred spawn"),
    };

    // ZEB-469: deterministically wait until B's session has discovered A's REMOTE
    // backfill queryable before requesting backfill, rather than a fixed sleep that
    // races CI scheduling. Backfill is a zenoh GET against A's queryable (declared
    // on `harmony/channels/{c}/{ch}/since/**`); a query issued before that queryable
    // is discovered finds no responder, returns zero replies, and the backfill
    // silently no-ops — so the 150-event completeness wait below would stall to its
    // 30s ceiling (the flake this removes). This is the query-side analog of the
    // ZEB-459 publisher `matching_status` barrier above.
    //
    // `Locality::Remote` is load-bearing: B's own re-spawned adapter also declares a
    // queryable on this `since/**` key on `session_b`, so a default (`Any`) matching
    // check would be satisfied by B's *local* queryable and pass before A was
    // reachable. Restricting to remote queryables waits for A specifically. A querier
    // on the concrete `since/0/0` key intersects A's `since/**` queryable.
    let backfill_probe_key = format!(
        "harmony/channels/{}/{}/since/0/0",
        hex::encode(community_id.0),
        hex::encode(channel_id.0)
    );
    let backfill_querier = session_b
        .declare_querier(
            zenoh::key_expr::KeyExpr::try_from(backfill_probe_key).expect("backfill probe key"),
        )
        .allowed_destination(zenoh::sample::Locality::Remote)
        .await
        .expect("declare backfill readiness querier");
    let backfill_ready_deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let matched = backfill_querier
            .matching_status()
            .await
            .expect("backfill querier matching_status query failed")
            .matching();
        if matched {
            break;
        }
        assert!(
            std::time::Instant::now() < backfill_ready_deadline,
            "B never matched A's remote backfill queryable within 30s \
             (re-spawned adapter's queryable discovery did not complete)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(backfill_querier);

    Arc::clone(&engine_b2)
        .request_backfill(None)
        .await
        .expect("backfill");

    // Wait for B to receive every event it is missing (deduped against
    // what's already on disk via the rebuilt replay tracker — see
    // ChannelLogEngine::new for the boot-time tracker rebuild). This is
    // the loss-free completeness assert (see the ZEB-288 note in Phase
    // 1): every live delivery was appended to B's disk log, so backfill
    // serves exactly the missing remainder and the counter lands on
    // precisely 150 no matter how many live publishes dropped.
    wait_until(
        || {
            let received_b = Arc::clone(&received_b);
            async move { received_b.lock().expect("received_b lock").len() >= 150 }
        },
        Duration::from_secs(30),
    )
    .await
    .expect("B should receive all 150 after backfill");

    // Spec §14.2 requires at least one channel-backfill-progress event
    // fires during backfill. With 50 events and the default progress
    // interval of 16, this should fire ~3 times.
    let progress_count = *backfill_progress_b.lock().expect("backfill progress lock");
    assert!(
        progress_count >= 1,
        "expected at least one channel-backfill-progress event (got {progress_count})"
    );

    // ── Phase 4: replay attack ───────────────────────────────────────
    // The replayed event is `posted_ids[0]` — the FIRST event A published,
    // so it carries the minimum HLC on A's (sole) author/device lane. B's
    // replay tracker, rebuilt on respawn from the ≥100 events already on
    // disk, sits at a strictly-higher high-water mark, so this event is
    // dropped at the tracker gate on every re-serve. Its emission therefore
    // happens exactly ONCE (the Phase-1 live delivery, which is lossless —
    // see the ZEB-288 batching note above) and can never recur, independent
    // of how the rest of the backfill emission stream drains.
    //
    // ZEB-686: assert on the count of THIS specific message-id, not the
    // total received count. The previous baseline was captured via a
    // wall-clock quiet-window (`wait_for_stable_count`) that, under full-
    // sweep contention, latched a premature "stable" total while legit
    // backfill replies were still draining, then mis-attributed the resumed
    // deliveries to the replay packet (`got 300, expected 184`). The total
    // received count is load-dependent; the replayed id's emission count is
    // pinned at 1 by the high-water-mark argument above, so counting it is
    // deterministic and immune to the lull — retiring the last quiet-window
    // readiness barrier in this test (generalizes the ZEB-469 audit).
    let first_id = posted_ids[0];
    let replayed_id_hex = hex::encode(first_id.0);
    let count_replayed = || {
        received_b
            .lock()
            .expect("received_b lock")
            .iter()
            .filter(|id| id.as_str() == replayed_id_hex.as_str())
            .count()
    };
    // The replayed event must already have been delivered exactly once
    // (Phase-1 live) — this makes the post-replay equality below non-vacuous
    // (a broken tracker that re-emits would push it to 2).
    let pre_replay_emits = count_replayed();
    assert_eq!(
        pre_replay_emits, 1,
        "replayed event should have been delivered exactly once before the replay \
         (got {pre_replay_emits}); the assertion below would be vacuous otherwise"
    );

    // Re-encrypt A's first event and re-publish it via A's session. The
    // packet is wire-identical to the original broadcast (and backfill
    // replies, per spec §17.1). B's replay tracker has already advanced
    // past this event's HLC, so B must drop the duplicate.
    let first_event = engine_a
        .list_messages(None, 200)
        .await
        .expect("list a")
        .into_iter()
        .find(|ev| *ev.id() == first_id)
        .expect("first event");
    let replay_packet = encrypt_channel_packet(&channel_key, &first_event).expect("re-encrypt");
    let topic = format!(
        "harmony/channels/{}/{}/events",
        hex::encode(community_id.0),
        hex::encode(channel_id.0)
    );
    let topic_key = zenoh::key_expr::KeyExpr::try_from(topic).expect("key");
    // ZEB-688: capture the drop counter BEFORE the replay put and wait on the
    // DELTA — earlier live/backfill delivery overlap may already have produced
    // replay drops, so an absolute count would race the setup phases.
    let pre_replay_drops = engine_b2.replay_drop_count();
    session_a
        .put(&topic_key, replay_packet)
        .await
        .expect("replay put");

    // ZEB-688 (closes the ZEB-469 audit deferral): wait for B's engine to
    // OBSERVE a drop instead of sleeping 1s. The emission-count check below
    // is a *negative* assertion; under the old sleep its only failure mode was
    // a spurious pass (a slow packet meant the drop path was never exercised —
    // the count was unchanged for the wrong reason). The counter proves the
    // drop path ran; the SENTINEL below proves it ran for OUR packet.
    wait_until(
        || async { engine_b2.replay_drop_count() > pre_replay_drops },
        Duration::from_secs(30),
    )
    .await
    .expect("B must observe a replay drop after the replayed put");

    // ZEB-688 R1 (CodeRabbit): the drop counter is engine-global, and a
    // straggling duplicate from the Phase-3 backfill overlap could satisfy the
    // delta above before OUR replayed put is processed — reopening the vacuity
    // window. Close it with an ORDERING barrier: publish one fresh event from
    // A on the same session+key the replayed put used. Same-session same-key
    // zenoh puts reach B's subscriber in publication order, and B's receive
    // loop processes packets serially, so once the sentinel lands in B's log
    // the replayed put has been fully processed — whatever its outcome.
    Arc::clone(&engine_a)
        .publish(b"sentinel-after-replay".to_vec(), None, None, None)
        .await
        .expect("publish ordering sentinel");
    wait_until(
        || async {
            engine_b2
                .list_messages(None, 300)
                .await
                .map(|v| v.len() == 151)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await
    .expect("B must receive the post-replay ordering sentinel");

    let post_replay_emits = count_replayed();
    assert_eq!(
        post_replay_emits, pre_replay_emits,
        "B should not re-emit the replayed event (replayed id emitted {post_replay_emits}×, \
         expected {pre_replay_emits}× — the replay tracker must drop the duplicate)"
    );

    // ── Final state check ────────────────────────────────────────────
    // B's log contains exactly 151 events (150 + the ordering sentinel) in
    // HLC order, no duplicates.
    let final_listed = engine_b2
        .list_messages(None, 300)
        .await
        .expect("final list");
    assert_eq!(
        final_listed.len(),
        151,
        "B's log should contain exactly 151 events (150 + ordering sentinel)"
    );

    // Verify HLC ordering across the entire log.
    for window in final_listed.windows(2) {
        let prev_at = window[0].at();
        let next_at = window[1].at();
        assert!(
            next_at.is_strictly_newer_than(prev_at),
            "log out of HLC order: {prev_at:?} → {next_at:?}",
        );
    }

    // Verify no duplicates by message_id.
    let mut seen_ids = std::collections::HashSet::new();
    for ev in &final_listed {
        let id = ev.id();
        assert!(
            seen_ids.insert(*id),
            "duplicate message_id in final log: {id:?}"
        );
    }

    // Clean shutdown — flushes B's tail synchronously.
    registry_a
        .shutdown_all()
        .await
        .expect("shutdown registry A");
    registry_b
        .shutdown_all()
        .await
        .expect("shutdown registry B");

    // Drop tempdirs only after shutdowns (per HARD RULES — TempDir
    // lifetime extends to test end). dir_a + dir_b auto-drop here.
    drop(dir_a);
    drop(dir_b);
}
