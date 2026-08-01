//! ZEB-418 SP2 P3a Task 6: capstone two-registry integration test for
//! automatic channel-history backfill (spec §8 scenarios), END-TO-END
//! over real zenoh.
//!
//! Two `ChannelLogRegistry` instances (A = holder, B = joiner /
//! reconnector) with separate data dirs share ONE in-process zenoh
//! session (the established single-session pattern from
//! `event_loop::channel_log_adapter_tests` — zenoh local routing lets
//! every declared queryable/subscriber on the session answer every
//! GET/put on the session). Real adapters are bound through the same
//! `spawn_channel_log_zenoh_adapter` call shape as the production
//! event_loop bridge arm. NO manual `request_backfill` call appears
//! anywhere in this file: every backfill below is the registry's
//! auto-spawned driver (`ChannelLogRegistry::spawn_inner_now` →
//! `run_backfill_driver`).
//!
//! ## Scenario 3 deviation (zenoh semantics, investigated)
//!
//! Spec §8's "holder appears late" scenario wants the latch RETRY path
//! (no-reply → 30s→600s backoff → eventual convergence when a holder
//! comes online). Over real zenoh that path is unreachable: a GET with
//! ZERO declared matching queryables completes as a CLEAN EMPTY reply
//! stream (zenoh resolves the query against the set of currently
//! matched queryables — none means "done, zero replies", not "no
//! answer"). The qr-driver therefore reports a clean empty page, which
//! correctly SATISFIES the latch per spec D24 ("a served nothing is an
//! answer"). The no-reply/abort path only occurs on adapter shutdown
//! or `session.get` failure — neither can be forced here without
//! faking zenoh, which this test deliberately refuses to do.
//! `eventual_convergence_when_holder_appears_late` therefore proves
//! the alternative end-to-end guarantee: a joiner whose auto-backfill
//! completes empty (holder online but with no history yet) still
//! converges via normal live pub/sub once the holder produces events.
//! Latch retry/backoff coverage lives at the unit level
//! (`channel_backfill::tests::driver_retries_until_holder_appears_then_satisfies`
//! + the backoff-schedule tests).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use harmony_app::community_channel_log::{
    derive_channel_key, encrypt_channel_packet, sign_channel_event, ChannelKey, ChannelLogConfig,
    ChannelPostPayload, CommunityStateAtHlc, CommunityStateSnapshot, MessageId, SignedChannelEvent,
};
use harmony_app::community_channel_log_engine::{
    ChannelLogEngine, ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
    SpawnOutcome,
};
use harmony_app::community_membership::{ChannelId, ChannelInfo, ChannelKind};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use harmony_identity::PrivateIdentity;
use tempfile::TempDir;
use tokio::sync::{mpsc, Mutex};

/// Mirrors the existing two-engine integration test
/// (`community_channel_messages_integration.rs`).
const TEST_SEAL_THRESHOLD: usize = 8;

/// ZEB-418 P3a Task 6 prerequisite knob: fast retry base for tests
/// (production default is 30s, spec D24). Load-bearing for test
/// robustness: if any auto-backfill GET aborts instead of closing
/// cleanly (rare adapter-teardown race), the driver recovers within
/// ~250ms (driver min-wait floor) instead of stalling the test 30s.
const TEST_RETRY_BASE_MS: u64 = 200;

type MockEngine = Arc<ChannelLogEngine>;

/// Build a deterministic `(SigningKey, OwnerAddr, identity_pub_64)`
/// triple for a single seed byte. The in-crate fixture twin
/// (`community_channel_log_engine.rs::tests::fixture_identity`) is
/// `#[cfg(test)]`-gated, so integration tests rebuild it from the PUB
/// `harmony_identity` surface — same as
/// `community_channel_messages_integration.rs` already does.
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

/// State stub mirroring the in-crate `AlwaysJoinedState` (which is
/// `#[cfg(test)]`-gated and unreachable from integration tests):
/// every `(owner, enrolled_key)` in `members` is a Joined member with
/// full power at every HLC; everyone else is unknown.
struct MembersJoinedState {
    channel_id: ChannelId,
    members: Vec<(OwnerAddr, [u8; 32])>,
}

fn channel_info_stub() -> ChannelInfo {
    ChannelInfo {
        name: "general".to_string(),
        write_power: 0,
        kind: ChannelKind::Text,
        created_at: Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "fixture".to_string(),
        },
        deleted_at: None,
    }
}

#[async_trait::async_trait]
impl CommunityStateAtHlc for MembersJoinedState {
    async fn snapshot_at(
        &self,
        channel_id: &ChannelId,
        author: &OwnerAddr,
        _at: &Hlc,
    ) -> CommunityStateSnapshot {
        let channel = (channel_id == &self.channel_id).then(channel_info_stub);
        let member = self.members.iter().find(|(o, _)| o == author);
        CommunityStateSnapshot {
            channel,
            author_power: member.map(|_| 100),
            author_enrolled_keys: member.map(|(_, k)| vec![*k]).unwrap_or_default(),
        }
    }
}

/// Scenario-4 resolver variant: like [`MembersJoinedState`], but
/// reports `rejected` NOT joined (author_power = None) at every HLC,
/// while STILL surfacing the rejected author's enrolled keys. That
/// makes the verify failure specifically the membership-at-HLC gate
/// of `verify_channel_event` (NotJoined), not a missing-key artifact.
struct NotJoinedForAuthor {
    channel_id: ChannelId,
    members: Vec<(OwnerAddr, [u8; 32])>,
    rejected: OwnerAddr,
}

#[async_trait::async_trait]
impl CommunityStateAtHlc for NotJoinedForAuthor {
    async fn snapshot_at(
        &self,
        channel_id: &ChannelId,
        author: &OwnerAddr,
        _at: &Hlc,
    ) -> CommunityStateSnapshot {
        let channel = (channel_id == &self.channel_id).then(channel_info_stub);
        let member = self.members.iter().find(|(o, _)| o == author);
        let author_power = if author == &self.rejected {
            None
        } else {
            member.map(|_| 100)
        };
        CommunityStateSnapshot {
            channel,
            author_power,
            author_enrolled_keys: member.map(|(_, k)| vec![*k]).unwrap_or_default(),
        }
    }
}

/// Stand-in for `event_loop::run`'s adapter-bridge select arm: binds
/// each `ChannelLogAdapterRequest` to the shared zenoh session via the
/// production `spawn_channel_log_zenoh_adapter`. Same shape as the
/// in-crate `RegistryFixture` drainer and the existing two-engine
/// integration test.
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
        }
    })
}

/// One identity's registry + everything needed to (re-)spawn channel
/// engines under it. The `TempDir` is the per-registry data dir — it
/// persists across `stop()`/re-`spawn()` within a test (the reconnect
/// scenario depends on that).
struct RegistryHandle {
    registry: Arc<ChannelLogRegistry>,
    owner: OwnerAddr,
    signing: Arc<SigningKey>,
    tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    _dir: TempDir,
    _drainer: tokio::task::JoinHandle<()>,
}

fn build_registry(session: &Arc<zenoh::Session>, seed: u8, device_id: &str) -> RegistryHandle {
    let (signing_raw, owner, _pub) = fixture_identity(seed);
    let signing = Arc::new(signing_raw);
    let dir = TempDir::new().expect("tempdir");
    let (adapter_tx, adapter_rx) = mpsc::unbounded_channel();
    let drainer = spawn_adapter_bridge_drainer(Arc::clone(session), adapter_rx);
    // ZEB-445: registry takes a mode-agnostic NodeEventSink; this test
    // asserts convergence via engine state, not emissions — empty fan-out.
    let registry = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        adapter_request_tx: adapter_tx,
        sink: Arc::new(harmony_app::node_event_sink::FanoutSink(vec![])),
        identity_dir: dir.path().to_path_buf(),
        self_owner: owner,
        self_device_id: device_id.to_string(),
        signing_key: Arc::clone(&signing),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        engine_config: ChannelLogEngineConfig {
            log_config: ChannelLogConfig {
                seal_threshold_events: TEST_SEAL_THRESHOLD,
            },
            backfill_retry_base_ms: TEST_RETRY_BASE_MS,
            ..Default::default()
        },
        transport_epoch_rx: None,
        // ZEB-599 Direction 1: no presence watch in this integration harness.
        presence_resync_rx: None,
    });
    RegistryHandle {
        registry,
        owner,
        signing,
        tracker: Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            device_id.to_string(),
        ))),
        _dir: dir,
        _drainer: drainer,
    }
}

/// Spawn (or re-spawn) the channel engine under a registry. The
/// backfill driver auto-starts inside `spawn` — this helper never
/// calls `request_backfill`.
async fn spawn_channel(
    h: &RegistryHandle,
    community_id: SpaceId,
    channel_id: ChannelId,
    channel_key: &ChannelKey,
    state: &Arc<dyn CommunityStateAtHlc + Send + Sync>,
) -> MockEngine {
    match Arc::clone(&h.registry)
        .spawn(
            community_id,
            channel_id,
            channel_key.clone(),
            Arc::clone(state),
            Arc::clone(&h.tracker),
        )
        .await
        .expect("spawn")
    {
        SpawnOutcome::Spawned(engine) => engine,
        SpawnOutcome::DeferredForCommit => panic!("no transaction open in these tests"),
    }
}

fn hlc(wall_ms: u64, device: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device.to_string(),
    }
}

/// Craft a signed Post event (mirrors the `#[cfg(test)]`-gated engine
/// helper). `id_byte` keeps MessageIds deterministic AND distinct per
/// crafted event within a test.
fn make_signed_event(
    community_id: SpaceId,
    channel_id: ChannelId,
    author: OwnerAddr,
    at: Hlc,
    body: &str,
    signing_key: &SigningKey,
    id_byte: u8,
) -> SignedChannelEvent {
    let payload = ChannelPostPayload {
        id: MessageId([id_byte; 16]),
        community_id,
        channel_id,
        author,
        at,
        content_kind: 0,
        body,
        reply_to: None,
        mentions: None,
        attachments: None,
    };
    sign_channel_event(&payload, signing_key).expect("sign")
}

/// Bodies in B's verified log, in `list_messages` (HLC) order.
async fn list_bodies(engine: &MockEngine) -> Vec<String> {
    engine
        .list_messages(None, 1000)
        .await
        .expect("list_messages")
        .into_iter()
        .filter_map(|ev| match ev {
            SignedChannelEvent::Post { body, .. } => Some(body),
            _ => None,
        })
        .collect()
}

/// Poll an engine's verified-log size until it reaches EXACTLY
/// `expected`, failing loudly on overshoot (duplicate delivery) or
/// deadline. Condition-based — no fixed sleeps.
async fn wait_for_count(engine: &MockEngine, expected: usize, timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    loop {
        let n = list_bodies(engine).await.len();
        if n == expected {
            return;
        }
        assert!(
            n <= expected,
            "{what}: count overshot — got {n}, expected {expected} (duplicate delivery?)"
        );
        assert!(
            Instant::now() < deadline,
            "{what}: expected {expected} events within {timeout:?}, last saw {n}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Absence/stability check: re-poll across a short observation window
/// and assert the verified-log size never moves off `expected`.
async fn assert_count_stays(engine: &MockEngine, expected: usize, window: Duration, what: &str) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let n = list_bodies(engine).await.len();
        assert_eq!(
            n, expected,
            "{what}: count drifted during observation window"
        );
    }
}

/// Count replies to a raw full-history backfill GET (`since=0`
/// sentinel) against the channel's queryable prefix. Zenoh resolves a
/// GET against the queryables declared AT THAT MOMENT — zero matching
/// queryables yields a clean empty completion — so a joiner's
/// auto-GET can race the holder's async queryable declaration and
/// (correctly, per spec D24) satisfy on an empty page. Tests that need
/// the holder's history actually served therefore prove the holder is
/// online and serving exactly `expected` events BEFORE spawning the
/// joiner, via this probe.
async fn probe_backfill_reply_count(
    session: &Arc<zenoh::Session>,
    community_id: &SpaceId,
    channel_id: &ChannelId,
) -> usize {
    let key = format!(
        "harmony/channels/{}/{}/since/0/1000",
        hex::encode(community_id.0),
        hex::encode(channel_id.0)
    );
    let receiver = session
        .get(&key)
        .consolidation(zenoh::query::ConsolidationMode::None)
        .await
        .expect("probe get");
    let mut n = 0usize;
    while let Ok(reply) = receiver.recv_async().await {
        if reply.into_result().is_ok() {
            n += 1;
        }
    }
    n
}

/// Poll the probe until the channel queryable(s) on the session serve
/// EXACTLY `expected` packets. Equality (not >=) also waits out a
/// just-stopped engine's lingering queryable (adapter teardown polls
/// its closing flag on a ~1s sleep arm), which would otherwise answer
/// with stale extras.
///
/// The probe spacing MUST exceed that ~1s closing-poll: the queryable
/// task's `select!` is `biased` toward incoming queries and re-creates
/// its sleep future every iteration, so rapid-fire probes keep the
/// query arm permanently ready and the stopped queryable never gets a
/// quiet window to observe `closing` (probe-vs-teardown livelock —
/// found red-first: a 50ms probe cadence kept a stopped queryable
/// serving indefinitely).
async fn wait_until_serving(
    session: &Arc<zenoh::Session>,
    community_id: &SpaceId,
    channel_id: &ChannelId,
    expected: usize,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        let n = probe_backfill_reply_count(session, community_id, channel_id).await;
        if n == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "holder queryable never served exactly {expected} events within {timeout:?} \
             (last probe saw {n})"
        );
        tokio::time::sleep(Duration::from_millis(1_100)).await;
    }
}

/// Deterministic live-delivery: re-`put` an encrypted packet on the
/// channel's events topic until every listed engine's VERIFIED log
/// contains `body`. A zenoh `put` that fires before a subscriber is
/// declared is silently dropped, so one-shot publishes can race
/// adapter startup; re-putting is safe because each engine's replay
/// tracker accepts the event exactly once and drops every duplicate.
async fn put_until_in_logs(
    session: &Arc<zenoh::Session>,
    community_id: &SpaceId,
    channel_id: &ChannelId,
    packet: &[u8],
    body: &str,
    engines: &[&MockEngine],
    timeout: Duration,
) {
    let topic = format!(
        "harmony/channels/{}/{}/events",
        hex::encode(community_id.0),
        hex::encode(channel_id.0)
    );
    let key = zenoh::key_expr::KeyExpr::try_from(topic).expect("events key");
    let deadline = Instant::now() + timeout;
    loop {
        session.put(&key, packet.to_vec()).await.expect("put");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut all = true;
        for engine in engines {
            if !list_bodies(engine).await.iter().any(|b| b == body) {
                all = false;
                break;
            }
        }
        if all {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "event {body:?} not delivered to all engines within {timeout:?}"
        );
    }
}

fn assert_unique_message_ids(events: &[SignedChannelEvent]) {
    let mut seen = HashSet::new();
    for ev in events {
        let id = ev.id();
        assert!(seen.insert(*id), "duplicate message_id in log: {id:?}");
    }
}

// ─────────────────────────────────────────────────────────────────────
// Scenario 1 (spec §8 / ZEB-403 repro): pre-join history reaches a
// brand-new member through the auto-backfill alone.
// ─────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_join_history_backfills_to_new_member() {
    let session = Arc::new(
        zenoh::open(zenoh::Config::default())
            .await
            .expect("zenoh open"),
    );

    let community_id = SpaceId([0xA1; 16]);
    let channel_id = ChannelId([0xB1; 16]);
    let membership_key = EpochKey::new([0x77; 32]);
    let channel_key = derive_channel_key(&membership_key, &community_id, &channel_id);

    let a = build_registry(&session, 0xAA, "device-a");
    let b = build_registry(&session, 0xBB, "device-b");

    // Both sides resolve A (the only author) as joined.
    let state: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(MembersJoinedState {
        channel_id,
        members: vec![(a.owner, a.signing.verifying_key().to_bytes())],
    });

    // A's registry spawns the channel and A posts 3 messages BEFORE B
    // has ever seen the channel.
    let engine_a = spawn_channel(&a, community_id, channel_id, &channel_key, &state).await;
    for body in ["pre-1", "pre-2", "pre-3"] {
        Arc::clone(&engine_a)
            .publish(body.as_bytes().to_vec(), None, None, None)
            .await
            .expect("publish");
    }
    wait_for_count(&engine_a, 3, Duration::from_secs(10), "holder local log").await;

    // Prove A's queryable is online and serving all 3 BEFORE B spawns
    // (see probe doc — otherwise B's auto-GET can satisfy clean-empty).
    wait_until_serving(
        &session,
        &community_id,
        &channel_id,
        3,
        Duration::from_secs(15),
    )
    .await;

    // B's registry spawns the same (community, channel) with B's OWN
    // data dir on the SAME session: the spawn-time backfill driver
    // (since = None, empty log) pulls all 3 through the real queryable.
    let engine_b = spawn_channel(&b, community_id, channel_id, &channel_key, &state).await;
    wait_for_count(
        &engine_b,
        3,
        Duration::from_secs(20),
        "joiner pre-join backfill (ZEB-403 repro)",
    )
    .await;

    // Bodies + HLC order match the holder's publish order.
    assert_eq!(
        list_bodies(&engine_b).await,
        vec!["pre-1", "pre-2", "pre-3"],
        "backfilled history must match the holder's bodies in order"
    );

    a.registry.shutdown_all().await.expect("shutdown A");
    b.registry.shutdown_all().await.expect("shutdown B");
}

// ─────────────────────────────────────────────────────────────────────
// Scenario 2 (spec §8): reconnect catch-up fetches exactly the missed
// events — no duplicates even though the holder may re-serve overlap
// (replay-tracker pre-population from disk makes overlap a no-op).
// ─────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_catch_up_fetches_exactly_missed_events() {
    let session = Arc::new(
        zenoh::open(zenoh::Config::default())
            .await
            .expect("zenoh open"),
    );

    let community_id = SpaceId([0xA2; 16]);
    let channel_id = ChannelId([0xB2; 16]);
    let membership_key = EpochKey::new([0x77; 32]);
    let channel_key = derive_channel_key(&membership_key, &community_id, &channel_id);

    let a = build_registry(&session, 0xAA, "device-a");
    let b = build_registry(&session, 0xBB, "device-b");

    let state: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(MembersJoinedState {
        channel_id,
        members: vec![(a.owner, a.signing.verifying_key().to_bytes())],
    });

    let engine_a = spawn_channel(&a, community_id, channel_id, &channel_key, &state).await;
    let engine_b = spawn_channel(&b, community_id, channel_id, &channel_key, &state).await;

    // ── Live phase: B receives 2 posts from A over pub/sub ──────────
    // The posts are crafted A-authored events on a dedicated HLC lane
    // and re-put until BOTH engines hold them: a one-shot engine
    // publish can race B's subscriber declaration (zenoh drops puts
    // with no matching subscriber), and engine publishes mint a fresh
    // MessageId per call so they cannot be retried. Re-puts of the
    // SAME packet are deduped by each engine's replay tracker, so this
    // is both deterministic and duplicate-safe. Low wall_ms values
    // keep these strictly OLDER than A's later wall-clock publishes.
    let live1 = make_signed_event(
        community_id,
        channel_id,
        a.owner,
        hlc(1_000, "live-dev"),
        "live-1",
        &a.signing,
        0x01,
    );
    let live2 = make_signed_event(
        community_id,
        channel_id,
        a.owner,
        hlc(2_000, "live-dev"),
        "live-2",
        &a.signing,
        0x02,
    );
    for (ev, body) in [(&live1, "live-1"), (&live2, "live-2")] {
        let packet = encrypt_channel_packet(&channel_key, ev).expect("encrypt");
        put_until_in_logs(
            &session,
            &community_id,
            &channel_id,
            &packet,
            body,
            &[&engine_a, &engine_b],
            Duration::from_secs(15),
        )
        .await;
    }
    wait_for_count(&engine_b, 2, Duration::from_secs(10), "joiner live phase").await;

    // ── B disconnects ────────────────────────────────────────────────
    // Registry stop(): engine shutdown persists the log tail durably
    // and flips the backfill driver's shutdown watch.
    b.registry
        .stop(&community_id, &channel_id)
        .await
        .expect("stop B");

    // ── A posts 3 more while B is offline ───────────────────────────
    for body in ["miss-1", "miss-2", "miss-3"] {
        Arc::clone(&engine_a)
            .publish(body.as_bytes().to_vec(), None, None, None)
            .await
            .expect("publish offline");
    }
    wait_for_count(
        &engine_a,
        5,
        Duration::from_secs(10),
        "holder offline-phase log",
    )
    .await;

    // Prove A's queryable serves exactly 5. Equality also waits out
    // B's just-stopped queryable (lingers ≤~1s on its closing poll),
    // which would otherwise answer the probe with 2 stale extras.
    wait_until_serving(
        &session,
        &community_id,
        &channel_id,
        5,
        Duration::from_secs(15),
    )
    .await;

    // ── B reconnects: re-spawn on the SAME data dir ──────────────────
    // The reloaded log yields watermark = max HLC ("live-2"), so the
    // auto-driver requests strictly-newer history only; the replay
    // tracker is pre-populated from disk at engine reload, so any
    // re-served overlap is dropped instead of duplicated.
    let engine_b2 = spawn_channel(&b, community_id, channel_id, &channel_key, &state).await;
    wait_for_count(
        &engine_b2,
        5,
        Duration::from_secs(20),
        "reconnect catch-up backfill",
    )
    .await;

    // Exactly 5 — and it STAYS exactly 5 across an observation window
    // (overlap re-serves, if any, must be no-ops).
    assert_count_stays(
        &engine_b2,
        5,
        Duration::from_millis(600),
        "post-catch-up stability",
    )
    .await;

    let events = engine_b2
        .list_messages(None, 1000)
        .await
        .expect("final list");
    assert_eq!(events.len(), 5, "exactly the 2 live + 3 missed events");
    assert_unique_message_ids(&events);
    assert_eq!(
        list_bodies(&engine_b2).await,
        vec!["live-1", "live-2", "miss-1", "miss-2", "miss-3"],
        "catch-up must append exactly the missed events after the watermark"
    );

    a.registry.shutdown_all().await.expect("shutdown A");
    b.registry.shutdown_all().await.expect("shutdown B");
}

/// ZEB-585: a returning member recovers a NEVER-SEEN authoring device's
/// offline-window message whose HLC sorts BELOW the member's global max,
/// via the per-author watermark vector. The pre-ZEB-585 scalar `since`
/// path filters it out forever (`max_hlc` is not a completeness
/// certificate); the periodic full-reconcile floor (~1 h) does not fire
/// inside this window, so delivery here is proof of the vector path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn returning_member_recovers_unseen_device_sub_max_hlc_event() {
    let session = Arc::new(
        zenoh::open(zenoh::Config::default())
            .await
            .expect("zenoh open"),
    );

    let community_id = SpaceId([0xA3; 16]);
    let channel_id = ChannelId([0xB3; 16]);
    let membership_key = EpochKey::new([0x88; 32]);
    let channel_key = derive_channel_key(&membership_key, &community_id, &channel_id);

    let a = build_registry(&session, 0xAA, "device-a");
    let b = build_registry(&session, 0xBB, "device-b");

    // A is the only member; every served event (incl. the skew-device one)
    // is A-authored so it verifies on B. Distinct HLC device_ids model A
    // posting from multiple devices.
    let state: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(MembersJoinedState {
        channel_id,
        members: vec![(a.owner, a.signing.verifying_key().to_bytes())],
    });

    let engine_a = spawn_channel(&a, community_id, channel_id, &channel_key, &state).await;
    let engine_b = spawn_channel(&b, community_id, channel_id, &channel_key, &state).await;

    // ── Live phase: B sees two posts on "live-dev" (global max = 2000) ──
    let live1 = make_signed_event(
        community_id,
        channel_id,
        a.owner,
        hlc(1_000, "live-dev"),
        "live-1",
        &a.signing,
        0x01,
    );
    let live2 = make_signed_event(
        community_id,
        channel_id,
        a.owner,
        hlc(2_000, "live-dev"),
        "live-2",
        &a.signing,
        0x02,
    );
    for (ev, body) in [(&live1, "live-1"), (&live2, "live-2")] {
        let packet = encrypt_channel_packet(&channel_key, ev).expect("encrypt");
        put_until_in_logs(
            &session,
            &community_id,
            &channel_id,
            &packet,
            body,
            &[&engine_a, &engine_b],
            Duration::from_secs(15),
        )
        .await;
    }
    wait_for_count(&engine_b, 2, Duration::from_secs(10), "joiner live phase").await;
    // Pin B's pre-offline baseline: exactly the two live events, nothing
    // else — so the skew-1 recovery below can ONLY come from the reconnect
    // catch-up, never a delayed live delivery masking the test.
    assert_eq!(
        list_bodies(&engine_b).await,
        vec!["live-1", "live-2"],
        "B must hold exactly the live phase before going offline"
    );

    // ── B disconnects ───────────────────────────────────────────────
    b.registry
        .stop(&community_id, &channel_id)
        .await
        .expect("stop B");

    // ── While B is offline, A logs (into A only):
    //    skew-1: a NEVER-SEEN device "skew-dev" at wall 1500 — BELOW B's
    //            global max 2000 (the gap the scalar path loses forever).
    //    new-1:  a normal "live-dev" post at 3000 — above the max (both
    //            paths serve this; included to show the normal catch-up
    //            still works alongside the gap recovery). ───────────────
    let skew1 = make_signed_event(
        community_id,
        channel_id,
        a.owner,
        hlc(1_500, "skew-dev"),
        "skew-1",
        &a.signing,
        0x03,
    );
    let new1 = make_signed_event(
        community_id,
        channel_id,
        a.owner,
        hlc(3_000, "live-dev"),
        "new-1",
        &a.signing,
        0x04,
    );
    for (ev, body) in [(&skew1, "skew-1"), (&new1, "new-1")] {
        let packet = encrypt_channel_packet(&channel_key, ev).expect("encrypt");
        put_until_in_logs(
            &session,
            &community_id,
            &channel_id,
            &packet,
            body,
            &[&engine_a],
            Duration::from_secs(15),
        )
        .await;
    }
    wait_for_count(
        &engine_a,
        4,
        Duration::from_secs(10),
        "holder offline-phase log",
    )
    .await;
    wait_until_serving(
        &session,
        &community_id,
        &channel_id,
        4,
        Duration::from_secs(15),
    )
    .await;

    // ── B reconnects: the reloaded watermark vector is {live-dev:(2000,0)}
    //    (no entry for skew-dev) → the catch-up GET seals it, A serves all
    //    of the unseen skew-dev plus live-dev's tail. ────────────────────
    let engine_b2 = spawn_channel(&b, community_id, channel_id, &channel_key, &state).await;
    wait_for_count(
        &engine_b2,
        4,
        Duration::from_secs(20),
        "vector catch-up backfill",
    )
    .await;
    assert_count_stays(
        &engine_b2,
        4,
        Duration::from_millis(600),
        "post-catch-up stability",
    )
    .await;

    let bodies = list_bodies(&engine_b2).await;
    assert!(
        bodies.contains(&"skew-1".to_string()),
        "the never-seen skew-dev event (HLC 1500, below B's max 2000) must arrive \
         via the watermark vector; the scalar path would lose it. got {bodies:?}"
    );
    assert_eq!(
        bodies,
        vec!["live-1", "live-2", "skew-1", "new-1"],
        "list_messages is in append/arrival order — the live phase first, then \
         the reconnect catch-up batch (A serves skew-1 then new-1 in its stored \
         order); skew-1's low HLC does not re-sort it ahead of live-2"
    );
    let events = engine_b2
        .list_messages(None, 1000)
        .await
        .expect("final list");
    assert_unique_message_ids(&events);

    a.registry.shutdown_all().await.expect("shutdown A");
    b.registry.shutdown_all().await.expect("shutdown B");
}

// ─────────────────────────────────────────────────────────────────────
// Scenario 3 (spec §8, DOCUMENTED DEVIATION — see module doc): the
// latch retry path is unreachable over real zenoh (a GET with no
// declared queryable completes CLEAN EMPTY, which satisfies the latch
// per spec D24). This test ships the provable alternative: the joiner
// spawns FIRST (auto-backfill completes empty — served-nothing), the
// holder appears late with an empty log, and the system still
// converges via normal live pub/sub. Retry/backoff is covered at the
// unit level in `channel_backfill::tests`.
// ─────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eventual_convergence_when_holder_appears_late() {
    let session = Arc::new(
        zenoh::open(zenoh::Config::default())
            .await
            .expect("zenoh open"),
    );

    let community_id = SpaceId([0xA3; 16]);
    let channel_id = ChannelId([0xB3; 16]);
    let membership_key = EpochKey::new([0x77; 32]);
    let channel_key = derive_channel_key(&membership_key, &community_id, &channel_id);

    // Both registries run the fast retry base (200ms) so that IF any
    // auto-GET aborts instead of completing clean-empty, the driver
    // re-requests promptly and the test converges via backfill instead
    // of stalling 30s — either path is a valid convergence proof.
    let a = build_registry(&session, 0xAA, "device-a");
    let b = build_registry(&session, 0xBB, "device-b");

    let state: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(MembersJoinedState {
        channel_id,
        members: vec![(a.owner, a.signing.verifying_key().to_bytes())],
    });

    // B (joiner) spawns FIRST — no holder is serving yet. Its auto-GET
    // resolves against zero (or only its own empty) queryables and
    // completes as a clean empty page → latch satisfied with 0 events.
    let engine_b = spawn_channel(&b, community_id, channel_id, &channel_key, &state).await;
    assert_eq!(
        list_bodies(&engine_b).await.len(),
        0,
        "joiner starts with nothing to backfill"
    );

    // Holder appears late, with NO history yet.
    let engine_a = spawn_channel(&a, community_id, channel_id, &channel_key, &state).await;

    // Deterministic subscriber warm-up: a crafted A-authored event is
    // re-put until BOTH engines hold it, proving both live subscribers
    // are declared before the asserted posts fire (zenoh drops puts
    // with no matching subscriber; replay trackers dedupe the re-puts).
    let warm = make_signed_event(
        community_id,
        channel_id,
        a.owner,
        hlc(1_000, "warm-dev"),
        "warm-up",
        &a.signing,
        0x01,
    );
    let warm_packet = encrypt_channel_packet(&channel_key, &warm).expect("encrypt");
    put_until_in_logs(
        &session,
        &community_id,
        &channel_id,
        &warm_packet,
        "warm-up",
        &[&engine_a, &engine_b],
        Duration::from_secs(15),
    )
    .await;

    // The late holder now produces history; it reaches the
    // already-satisfied joiner via normal live pub/sub.
    for body in ["late-1", "late-2", "late-3"] {
        Arc::clone(&engine_a)
            .publish(body.as_bytes().to_vec(), None, None, None)
            .await
            .expect("publish");
    }
    wait_for_count(
        &engine_b,
        4,
        Duration::from_secs(20),
        "liveness after empty backfill",
    )
    .await;
    assert_eq!(
        list_bodies(&engine_b).await,
        vec!["warm-up", "late-1", "late-2", "late-3"],
        "joiner must converge on the late holder's history"
    );

    a.registry.shutdown_all().await.expect("shutdown A");
    b.registry.shutdown_all().await.expect("shutdown B");
}

// ─────────────────────────────────────────────────────────────────────
// Scenario 4 (spec §8): a backfilled event whose author was NOT a
// member at the event's HLC is rejected by the joiner's verify chain
// (decrypt → replay → snapshot_at membership gate → signature) even
// though the holder accepted and serves it.
// ─────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfilled_event_from_non_member_at_hlc_is_rejected() {
    let session = Arc::new(
        zenoh::open(zenoh::Config::default())
            .await
            .expect("zenoh open"),
    );

    let community_id = SpaceId([0xA4; 16]);
    let channel_id = ChannelId([0xB4; 16]);
    let membership_key = EpochKey::new([0x77; 32]);
    let channel_key = derive_channel_key(&membership_key, &community_id, &channel_id);

    let a = build_registry(&session, 0xAA, "device-a");
    let b = build_registry(&session, 0xBB, "device-b");
    let (mallory_signing, mallory_owner, _mallory_pub) = fixture_identity(0x66);

    let a_key = a.signing.verifying_key().to_bytes();
    let mallory_key = mallory_signing.verifying_key().to_bytes();

    // A's resolver: both A and Mallory joined → A accepts + serves
    // Mallory's event.
    let state_a: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(MembersJoinedState {
        channel_id,
        members: vec![(a.owner, a_key), (mallory_owner, mallory_key)],
    });
    // B's resolver: same member/enrolled-key surface, but reports
    // Mallory NOT joined at the event's HLC — the rejection below is
    // specifically the membership-at-HLC gate.
    let state_b: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(NotJoinedForAuthor {
        channel_id,
        members: vec![(a.owner, a_key), (mallory_owner, mallory_key)],
        rejected: mallory_owner,
    });

    let engine_a = spawn_channel(&a, community_id, channel_id, &channel_key, &state_a).await;
    for body in ["legit-1", "legit-2"] {
        Arc::clone(&engine_a)
            .publish(body.as_bytes().to_vec(), None, None, None)
            .await
            .expect("publish");
    }
    wait_for_count(&engine_a, 2, Duration::from_secs(10), "holder legit posts").await;

    // Inject Mallory's event into A's log over the real wire (A's
    // resolver admits it).
    let evil = make_signed_event(
        community_id,
        channel_id,
        mallory_owner,
        hlc(5_000, "mallory-dev"),
        "from-mallory",
        &mallory_signing,
        0x6D,
    );
    let evil_packet = encrypt_channel_packet(&channel_key, &evil).expect("encrypt");
    put_until_in_logs(
        &session,
        &community_id,
        &channel_id,
        &evil_packet,
        "from-mallory",
        &[&engine_a],
        Duration::from_secs(15),
    )
    .await;
    wait_for_count(&engine_a, 3, Duration::from_secs(10), "holder full log").await;

    // Holder provably serves all 3 (2 legit + 1 Mallory) before the
    // joiner spawns.
    wait_until_serving(
        &session,
        &community_id,
        &channel_id,
        3,
        Duration::from_secs(15),
    )
    .await;

    // B backfills: the 2 legit events land; Mallory's is dropped by
    // verify_channel_event's membership-at-HLC gate.
    let engine_b = spawn_channel(&b, community_id, channel_id, &channel_key, &state_b).await;
    wait_for_count(
        &engine_b,
        2,
        Duration::from_secs(20),
        "joiner backfill minus rejected author",
    )
    .await;

    // ... and STAYS absent across one more observation window.
    assert_count_stays(
        &engine_b,
        2,
        Duration::from_millis(600),
        "rejected-event absence",
    )
    .await;

    let bodies = list_bodies(&engine_b).await;
    assert_eq!(
        bodies,
        vec!["legit-1", "legit-2"],
        "joiner must hold the member-authored events only"
    );
    assert!(
        !bodies.iter().any(|b| b == "from-mallory"),
        "non-member-at-HLC event must be absent from the joiner's log"
    );

    a.registry.shutdown_all().await.expect("shutdown A");
    b.registry.shutdown_all().await.expect("shutdown B");
}

/// Probe the live `rbsr/**` queryable as an EMPTY requester: seal a round-0
/// request over an empty source and count the encrypted Have packets the holder
/// ships back. The sealed reply frame uses the RBSR AAD, so it won't decrypt as
/// a channel packet and is naturally excluded from the count. Polls until the
/// holder serves exactly `expected` (its queryable declaration is async).
async fn probe_rbsr_have_count(
    session: &Arc<zenoh::Session>,
    community_id: &SpaceId,
    channel_id: &ChannelId,
    channel_key: &ChannelKey,
    expected: usize,
    timeout: Duration,
) {
    let empty = harmony_app::channel_rbsr::SliceSource::from_unsorted(Vec::new());
    let sealed = harmony_app::community_channel_log::seal_rbsr_message(
        channel_key,
        &harmony_app::channel_rbsr::initial_request(&empty),
    )
    .expect("seal rbsr initial request");
    let key = format!(
        "harmony/channels/{}/{}/rbsr/0",
        hex::encode(community_id.0),
        hex::encode(channel_id.0)
    );
    let deadline = Instant::now() + timeout;
    loop {
        let receiver = session
            .get(&key)
            .payload(sealed.clone())
            .consolidation(zenoh::query::ConsolidationMode::None)
            .timeout(Duration::from_secs(5))
            .await
            .expect("rbsr get");
        let mut have = 0usize;
        while let Ok(reply) = receiver.recv_async().await {
            if let Ok(sample) = reply.into_result() {
                let bytes = sample.payload().to_bytes().to_vec();
                if harmony_app::community_channel_log::decrypt_channel_packet(channel_key, &bytes)
                    .is_ok()
                {
                    have += 1;
                }
            }
        }
        if have == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "rbsr probe: expected {expected} Have packets within {timeout:?}, last saw {have}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ─────────────────────────────────────────────────────────────────────
// ZEB-593: the live rbsr/** transport reconciles history end-to-end and the
// legacy since/** (watermark-vector) path stays intact alongside it.
// ─────────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rbsr_transport_recovers_history_and_vector_path_intact() {
    // TWO sessions (not the shared-session pattern the other tests use): RBSR
    // excludes the requester's OWN queryable (Locality::Remote), so A must be a
    // genuinely remote responder for the live RBSR path to run. Default-config
    // sessions discover each other over localhost scouting; the polling helpers
    // below absorb the discovery delay.
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    let community_id = SpaceId([0xA9; 16]);
    let channel_id = ChannelId([0xB9; 16]);
    let membership_key = EpochKey::new([0x77; 32]);
    let channel_key = derive_channel_key(&membership_key, &community_id, &channel_id);

    let a = build_registry(&session_a, 0xAA, "device-a");
    let b = build_registry(&session_b, 0xBB, "device-b");

    let state: Arc<dyn CommunityStateAtHlc + Send + Sync> = Arc::new(MembersJoinedState {
        channel_id,
        members: vec![(a.owner, a.signing.verifying_key().to_bytes())],
    });

    // A posts 6 events (≤ LEAF_THRESHOLD → RBSR ships them wholesale, round 0).
    let engine_a = spawn_channel(&a, community_id, channel_id, &channel_key, &state).await;
    for body in ["m1", "m2", "m3", "m4", "m5", "m6"] {
        Arc::clone(&engine_a)
            .publish(body.as_bytes().to_vec(), None, None, None)
            .await
            .expect("publish");
    }
    wait_for_count(&engine_a, 6, Duration::from_secs(10), "holder local log").await;

    // Backward-compat: the legacy since/** queryable still serves all 6.
    // Backward-compat: A's legacy since/** queryable still serves all 6 (probed
    // from B's session, which also proves the two sessions have discovered each
    // other before the RBSR probe below).
    wait_until_serving(
        &session_b,
        &community_id,
        &channel_id,
        6,
        Duration::from_secs(20),
    )
    .await;

    // Live RBSR transport: an empty requester pulls exactly the 6-event diff
    // from the holder's rbsr/** queryable (O(diff), not O(history-of-nothing)).
    probe_rbsr_have_count(
        &session_b,
        &community_id,
        &channel_id,
        &channel_key,
        6,
        Duration::from_secs(20),
    )
    .await;

    // End-to-end: B spawns empty; its auto-backfill driver reconciles via RBSR
    // (drive_rbsr_rounds → rbsr/** → process_inbound_packet) and recovers all 6.
    let engine_b = spawn_channel(&b, community_id, channel_id, &channel_key, &state).await;
    wait_for_count(
        &engine_b,
        6,
        Duration::from_secs(20),
        "joiner RBSR catch-up",
    )
    .await;
    assert_eq!(
        list_bodies(&engine_b).await,
        vec!["m1", "m2", "m3", "m4", "m5", "m6"],
        "RBSR-recovered history must match the holder's bodies in order"
    );

    a.registry.shutdown_all().await.expect("shutdown A");
    b.registry.shutdown_all().await.expect("shutdown B");
}
