//! ZEB-270 Phase 3 integration test: two ChannelLogEngines on a
//! shared in-memory Zenoh router exercise live broadcast,
//! offline-then-backfill, and replay rejection.
//!
//! Per spec §14.2.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_channel_log::{
    derive_channel_key, encrypt_channel_packet, ChannelIdentityResolver, ChannelLogConfig,
    CommunityStateAtHlc, SignedChannelEvent,
};
use harmony_app::community_channel_log_engine::{
    ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
};
use harmony_app::community_membership::{ChannelId, ChannelInfo};
use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
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
}

#[async_trait::async_trait]
impl CommunityStateAtHlc for BothJoinedState {
    async fn channel_at(&self, channel_id: &ChannelId, _at: &Hlc) -> Option<ChannelInfo> {
        if channel_id != &self.channel_id {
            return None;
        }
        Some(ChannelInfo {
            name: "general".to_string(),
            write_power: 0,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "test".to_string(),
            },
            deleted_at: None,
        })
    }

    async fn author_power_at(&self, author: &OwnerAddr, _at: &Hlc) -> Option<u8> {
        if author == &self.a || author == &self.b {
            Some(100)
        } else {
            None
        }
    }
}

/// Resolver stub: maps OwnerAddr → 64-byte identity composite.
struct SharedResolver {
    map: HashMap<OwnerAddr, [u8; 64]>,
}

#[async_trait::async_trait]
impl ChannelIdentityResolver for SharedResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.map.get(addr).copied()
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
                req.closing,
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
/// or `timeout` elapses. Returns the final count regardless. Used
/// before the replay-attack phase so in-flight backfill replies
/// don't race the replay measurement.
async fn wait_for_stable_count(
    counter: &Arc<std::sync::Mutex<Vec<String>>>,
    stable_for_polls: usize,
    timeout: Duration,
) -> usize {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = counter.lock().expect("count lock").len();
    let mut stable = 0usize;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = counter.lock().expect("count lock").len();
        if now == last {
            stable += 1;
            if stable >= stable_for_polls {
                return now;
            }
        } else {
            stable = 0;
            last = now;
        }
    }
    counter.lock().expect("count lock").len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_live_then_offline_backfill_with_replay_rejection() {
    use std::sync::Mutex as StdMutex;
    use tauri::Listener;

    // ── Set up shared in-memory Zenoh router ──────────────────────────
    // Two `zenoh::open(default_config)` sessions on the same in-memory
    // peer router — they discover each other via gossip.
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    // ── Set up identities A + B ──────────────────────────────────────
    let (signing_a_raw, owner_a, pub_a) = fixture_identity(0xAA);
    let (signing_b_raw, owner_b, pub_b) = fixture_identity(0xBB);
    let signing_a = Arc::new(signing_a_raw);
    let signing_b = Arc::new(signing_b_raw);

    // ── Set up shared community + channel ────────────────────────────
    let community_id = SpaceId([0xc0; 16]);
    let channel_id = ChannelId([0xc1; 16]);
    let membership_key = MembershipKey::new([0x77; 32]);
    let channel_key = derive_channel_key(&membership_key, &community_id, &channel_id);

    // ── Set up Tauri mock apps for each side ────────────────────────
    let app_a = tauri::test::mock_app();
    let app_b = tauri::test::mock_app();

    let dir_a = TempDir::new().expect("tmp A");
    let dir_b = TempDir::new().expect("tmp B");

    // ── Construct stubs (verify-chain shortcuts for the test) ───────
    let state_a: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(BothJoinedState {
        a: owner_a,
        b: owner_b,
        channel_id,
    });
    let state_b: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(BothJoinedState {
        a: owner_a,
        b: owner_b,
        channel_id,
    });

    let mut resolver_map = HashMap::new();
    resolver_map.insert(owner_a, pub_a);
    resolver_map.insert(owner_b, pub_b);
    let resolver: Arc<dyn ChannelIdentityResolver + Send + Sync> =
        Arc::new(SharedResolver { map: resolver_map });

    let tracker_a: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let tracker_b: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));

    // ── Adapter-request bridges + drainers (mirrors registry test fixture) ──
    let (adapter_tx_a, adapter_rx_a) = mpsc::unbounded_channel();
    let (adapter_tx_b, adapter_rx_b) = mpsc::unbounded_channel();
    let _drainer_a = spawn_adapter_bridge_drainer(Arc::clone(&session_a), adapter_rx_a);
    let _drainer_b = spawn_adapter_bridge_drainer(Arc::clone(&session_b), adapter_rx_b);

    // ── Build registries ─────────────────────────────────────────────
    let registry_a = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        adapter_request_tx: adapter_tx_a,
        app: app_a.handle().clone(),
        identity_dir: dir_a.path().to_path_buf(),
        self_owner: owner_a,
        self_device_id: "device-a".to_string(),
        signing_key: Arc::clone(&signing_a),
        engine_config: ChannelLogEngineConfig {
            log_config: ChannelLogConfig {
                seal_threshold_events: TEST_SEAL_THRESHOLD,
            },
            ..Default::default()
        },
    });
    let registry_b = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        adapter_request_tx: adapter_tx_b,
        app: app_b.handle().clone(),
        identity_dir: dir_b.path().to_path_buf(),
        self_owner: owner_b,
        self_device_id: "device-b".to_string(),
        signing_key: Arc::clone(&signing_b),
        engine_config: ChannelLogEngineConfig {
            log_config: ChannelLogConfig {
                seal_threshold_events: TEST_SEAL_THRESHOLD,
            },
            ..Default::default()
        },
    });

    // ── Listen for B's channel-message-received events ──────────────
    // Listener callback is SYNC; use std::sync::Mutex (not tokio's
    // Mutex) so the closure is non-Send-async-friendly and we avoid
    // having to spawn an inner task per event.
    let received_b: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let received_b_for_listener = Arc::clone(&received_b);
    let _unlisten_message = app_b
        .handle()
        .listen("channel-message-received", move |event| {
            let payload: serde_json::Value =
                serde_json::from_str(event.payload()).expect("parse payload");
            let msg_id = payload["message"]["messageId"]
                .as_str()
                .expect("messageId")
                .to_string();
            received_b_for_listener
                .lock()
                .expect("received_b lock")
                .push(msg_id);
        });

    // Listen for backfill-progress events too (spec §14.2 requires
    // at least one fires during the backfill phase).
    let backfill_progress_b: Arc<StdMutex<u32>> = Arc::new(StdMutex::new(0));
    let backfill_progress_for_listener = Arc::clone(&backfill_progress_b);
    let _unlisten_progress = app_b
        .handle()
        .listen("channel-backfill-progress", move |_event| {
            *backfill_progress_for_listener
                .lock()
                .expect("backfill progress lock") += 1;
        });

    let engine_a = Arc::clone(&registry_a)
        .spawn(
            community_id,
            channel_id,
            channel_key.clone(),
            Arc::clone(&state_a),
            Arc::clone(&resolver),
            Arc::clone(&tracker_a),
        )
        .await
        .expect("spawn A");
    let _engine_b = Arc::clone(&registry_b)
        .spawn(
            community_id,
            channel_id,
            channel_key.clone(),
            Arc::clone(&state_b),
            Arc::clone(&resolver),
            Arc::clone(&tracker_b),
        )
        .await
        .expect("spawn B");

    // Give Zenoh subscribers + queryables time to declare and peers
    // time to discover. 1s is conservative; the registry-fixture tests
    // use shorter waits but they don't drive messages through the wire.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ── Phase 1: A posts 100 messages live ───────────────────────────
    // The publisher channel between engine and adapter has capacity 64,
    // so a tight burst can drop on the wire (only locally appended).
    // Yield + small sleep every batch so the publisher task drains.
    let mut posted_ids = Vec::new();
    for i in 0..100 {
        let id = Arc::clone(&engine_a)
            .publish(format!("msg-{i}").into_bytes(), None)
            .await
            .expect("publish");
        posted_ids.push(id);
        if (i + 1) % 16 == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // Wait for B to receive all 100.
    wait_until(
        || {
            let received_b = Arc::clone(&received_b);
            async move { received_b.lock().expect("received_b lock").len() >= 100 }
        },
        Duration::from_secs(30),
    )
    .await
    .expect("B should receive 100 live");

    // ── Phase 2: B disconnect; A posts 50 more ───────────────────────
    // Stop B's engine + adapter. The on-disk segments persist (spec
    // §17.4) so the backfill phase will dedupe against them by
    // message_id when B re-spawns.
    registry_b
        .stop(&community_id, &channel_id)
        .await
        .expect("stop B");

    // Briefly let the adapter teardown settle (closing flag is poll-
    // based on a 1s timer for select arms — but we don't need to wait
    // a full second; the publisher task exits as soon as the queue is
    // empty AND closing is true on its next tick).
    tokio::time::sleep(Duration::from_millis(200)).await;

    let received_at_offline_start = received_b.lock().expect("received_b lock").len();

    for i in 100..150 {
        Arc::clone(&engine_a)
            .publish(format!("msg-{i}").into_bytes(), None)
            .await
            .expect("publish offline");
        if (i + 1) % 16 == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // Give a moment for any in-flight loopback packets to settle.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let received_at_offline_end = received_b.lock().expect("received_b lock").len();
    // B may receive a small number of in-flight messages racing with
    // the stop; we allow ≤5 slack on either side of 100.
    assert!(
        (received_at_offline_start..=received_at_offline_start + 5)
            .contains(&received_at_offline_end),
        "B should be at ~{received_at_offline_start} during offline (got {received_at_offline_end})"
    );

    // ── Phase 3: B reconnects + backfill ─────────────────────────────
    // Re-spawn B's engine. The on-disk tail+segments are reloaded; the
    // existing 100 events are visible locally already.
    let engine_b2 = Arc::clone(&registry_b)
        .spawn(
            community_id,
            channel_id,
            channel_key.clone(),
            Arc::clone(&state_b),
            Arc::clone(&resolver),
            Arc::clone(&tracker_b),
        )
        .await
        .expect("re-spawn B");

    // Wait for the new adapter to fully declare its subscriber/queryable.
    tokio::time::sleep(Duration::from_secs(1)).await;

    Arc::clone(&engine_b2)
        .request_backfill(None)
        .await
        .expect("backfill");

    // Wait for B to receive the missing 50 events (deduped against
    // the 100 already on disk via the rebuilt replay tracker — see
    // ChannelLogEngine::new for the boot-time tracker rebuild).
    wait_until(
        || {
            let received_b = Arc::clone(&received_b);
            async move {
                // Live phase delivered ~100 to received_b counter; the
                // backfill phase appends another 50 (the remaining
                // events A posted while B was offline). Total ≈ 150.
                received_b.lock().expect("received_b lock").len() >= 150
            }
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
    // Wait for received_b to stabilize — backfill replies stream in
    // asynchronously and we don't want late-arriving in-flight
    // deliveries to be mis-attributed to the replay.
    let stable_count = wait_for_stable_count(&received_b, 5, Duration::from_secs(5)).await;

    // Re-encrypt one of A's events and re-publish it via A's session.
    // The packet is wire-identical to the original broadcast (and
    // backfill replies, per spec §17.1). B's replay tracker has
    // already advanced past this event's HLC, so B must drop the
    // duplicate.
    let pre_replay_count = stable_count;
    let first_id = posted_ids[0];
    let first_event_opt = engine_a
        .list_messages(None, 200)
        .await
        .expect("list a")
        .into_iter()
        .find(|ev| {
            let SignedChannelEvent::Post { id, .. } = ev;
            *id == first_id
        });
    let first_event = first_event_opt.expect("first event");
    let replay_packet = encrypt_channel_packet(&channel_key, &first_event).expect("re-encrypt");
    let topic = format!(
        "harmony/channels/{}/{}/events",
        hex::encode(community_id.0),
        hex::encode(channel_id.0)
    );
    let topic_key = zenoh::key_expr::KeyExpr::try_from(topic).expect("key");
    session_a
        .put(&topic_key, replay_packet)
        .await
        .expect("replay put");

    // Give the loopback ~1s to arrive at B's subscriber + traverse
    // verify_channel_event (where the replay tracker drops it).
    tokio::time::sleep(Duration::from_secs(1)).await;

    let final_count = received_b.lock().expect("received_b lock").len();
    assert_eq!(
        final_count, pre_replay_count,
        "B should not double-emit replayed event (got {final_count}, expected {pre_replay_count})"
    );

    // ── Final state check ────────────────────────────────────────────
    // B's log contains exactly 150 events in HLC order, no duplicates.
    let final_listed = engine_b2
        .list_messages(None, 300)
        .await
        .expect("final list");
    assert_eq!(
        final_listed.len(),
        150,
        "B's log should contain exactly 150 events"
    );

    // Verify HLC ordering across the entire log.
    for window in final_listed.windows(2) {
        let SignedChannelEvent::Post { at: prev_at, .. } = &window[0];
        let SignedChannelEvent::Post { at: next_at, .. } = &window[1];
        assert!(
            next_at.is_strictly_newer_than(prev_at),
            "log out of HLC order: {prev_at:?} → {next_at:?}",
        );
    }

    // Verify no duplicates by message_id.
    let mut seen_ids = std::collections::HashSet::new();
    for ev in &final_listed {
        let SignedChannelEvent::Post { id, .. } = ev;
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
