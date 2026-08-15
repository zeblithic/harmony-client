//! ZEB-298+ZEB-312 PR 1: end-to-end test of voting outbound→inbound
//! through TWO real `zenoh::Session` instances (not the mpsc test
//! bridge). Verifies that a peer-delivered voting event arrives via
//! the Zenoh subscriber, passes through `verify_voting_event` on the
//! receiving engine, and applies to its `VotingLog`.
//!
//! Why this test exists: the mpsc-bridged tests prove the engine's
//! pub/sub channels work, but they cannot verify the Zenoh transport
//! layer itself (key_expr parsing, session.put, declare_subscriber,
//! recv_async). This is the missing end-to-end coverage that proves
//! voting events actually cross the wire.

#![cfg(feature = "test-fixtures")]

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use harmony_app::community_membership::ChannelId;
use harmony_app::community_voting_approval::Tier1PollConfig;
use harmony_app::community_voting_core::{
    build_signed_poll_create_tier1, derive_poll_id, Eligibility, MemberAttrs, MembershipSnapshot,
    VotingIdentityResolver,
};
use harmony_app::community_voting_log::{
    MembershipSnapshotResolver, SnapshotResolverError, VotingLog,
};
use harmony_app::community_voting_log_engine::{VotingLogEngine, VotingLogEngineParams};
use harmony_app::event_loop::spawn_voting_log_zenoh_adapter;
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};

/// Build a `(SigningKey, OwnerAddr, [u8; 64])` triple from a single-byte seed.
/// The returned `owner`'s `address_hash` is derived from the public key bytes —
/// the same binding enforced by `verify_voting_event`'s defense-in-depth check.
fn fixture_identity(seed: u8) -> (ed25519_dalek::SigningKey, OwnerAddr, [u8; 64]) {
    let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
    let owner = OwnerAddr(priv_id.identity.address_hash);
    let pub_64 = priv_id.identity.to_public_bytes();
    let private_bytes = priv_id.to_private_bytes();
    let mut ed_secret = [0u8; 32];
    ed_secret.copy_from_slice(&private_bytes[32..64]);
    let signing = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
    (signing, owner, pub_64)
}

/// Test resolvers that satisfy both VotingIdentityResolver and
/// MembershipSnapshotResolver from a single Arc — same pattern used in
/// the production-build process_inbound test.
struct FixedResolvers {
    identity: HashMap<OwnerAddr, [u8; 64]>,
    snapshot: MembershipSnapshot,
}

#[async_trait]
impl VotingIdentityResolver for FixedResolvers {
    async fn resolve(&self, owner: &OwnerAddr) -> Option<[u8; 64]> {
        self.identity.get(owner).copied()
    }
}

#[async_trait]
impl MembershipSnapshotResolver for FixedResolvers {
    async fn snapshot_at(
        &self,
        _community_id: SpaceId,
        _hlc: &Hlc,
    ) -> Result<MembershipSnapshot, SnapshotResolverError> {
        Ok(self.snapshot.clone())
    }
}

/// ZEB-718: no-op backfill closures for tests that only exercise the live
/// pub/sub path. The responder serves nothing; the requester applies nothing.
fn noop_backfill() -> (
    harmony_app::event_loop::VotingBackfillReadFn,
    harmony_app::event_loop::VotingBackfillApplyFn,
) {
    (
        Arc::new(|| Box::pin(async { Vec::new() })),
        Arc::new(|_frame| Box::pin(async { true })),
    )
}

/// A backfill floor long enough that the periodic pull never fires during a
/// test; recovery in the ZEB-718 acceptance tests rides the pull-on-spawn.
const NO_BACKFILL_FLOOR: std::time::Duration = std::time::Duration::from_secs(86_400);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voting_event_flows_through_two_zenoh_sessions() {
    // Open two default-config peer sessions — they discover each other
    // via in-memory gossip.
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A open"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B open"));

    let community_id = SpaceId([0xab; 16]);
    let community_id_hex = hex::encode(community_id.0);

    // ZEB-717: the voting adapters encrypt/decrypt at the wire boundary using
    // the community's live epoch key. A + B share one crdt_state (same key,
    // epoch 0) — the realistic post-enrollment state — so events round-trip.
    let crdt_state = Arc::new(Mutex::new({
        let mut os = harmony_app::owner_state_crdt::OwnerState::default();
        os.spaces.insert(
            community_id,
            harmony_app::community_state_sync::test_community_space(
                community_id,
                0,
                EpochKey::new([0x11; 32]),
            ),
        );
        os
    }));

    // Build the peer event using a bound fixture identity.
    let (keypair, actor, pub_64) = fixture_identity(0xcd);

    let resolvers = Arc::new(FixedResolvers {
        identity: HashMap::from([(actor, pub_64)]),
        snapshot: MembershipSnapshot {
            members: HashMap::from([(
                actor,
                MemberAttrs {
                    power: 1,
                    vouching_depth: 1,
                },
            )]),
        },
    });
    let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
    let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();

    // ── Engine A side (publisher only) ──────────────────────────────────
    // We only need session A to publish to the topic via the production
    // adapter. Engine A doesn't hold a real engine — we just drive
    // a_pub_tx from the test body.
    let closing_a = Arc::new(AtomicBool::new(false));
    let (a_pub_tx, a_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    // Engine A doesn't need to receive inbound; give it a dummy subscriber_tx.
    let (a_sub_tx_unused, _a_sub_rx_unused) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (bf_read_a, bf_apply_a) = noop_backfill();
    let _adapter_a_handle = spawn_voting_log_zenoh_adapter(
        Arc::clone(&session_a),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state),
        a_pub_rx,
        a_sub_tx_unused,
        bf_read_a,
        bf_apply_a,
        NO_BACKFILL_FLOOR,
        None,
        Arc::clone(&closing_a),
    );

    // ── Engine B side (subscriber + engine) ─────────────────────────────
    // The adapter on session_b subscribes to the topic and forwards
    // received packets → b_sub_tx → engine B's inbound receive loop,
    // which calls verify_voting_event then applies on success.
    let closing_b = Arc::new(AtomicBool::new(false));
    let (b_pub_tx, b_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (b_sub_tx, b_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (bf_read_b, bf_apply_b) = noop_backfill();
    let _adapter_b_handle = spawn_voting_log_zenoh_adapter(
        Arc::clone(&session_b),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state),
        b_pub_rx,
        b_sub_tx,
        bf_read_b,
        bf_apply_b,
        NO_BACKFILL_FLOOR,
        None,
        Arc::clone(&closing_b),
    );

    // Warm-up: poll until Zenoh peer-discovery + subscriber declaration are
    // complete before publishing the real event. A fixed sleep is fragile
    // on resource-constrained CI (Greptile P2). Instead, we publish a
    // sentinel packet that the engine will silently reject (malformed CBOR),
    // then retry the real publish in a poll loop. The 3-second poll at the
    // bottom already tolerates apply latency; this warm-up ensures the
    // first real publish doesn't fire into an unwired subscriber.
    //
    // Strategy: publish a "warm-up" byte over a_pub_tx repeatedly
    // (up to 20 × 100ms = 2s total) until Zenoh has had time to wire the
    // peer link. We then publish the real event below. The engine on the
    // B side silently drops the malformed bytes (decode error logged as
    // warn, not fatal) and continues. The real event is published after
    // the warm-up window, before the 3-second poll loop.
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Ignore send errors — the adapter may not be up yet.
        let _ = a_pub_tx.send(b"WARMUP".to_vec()).await;
    }

    // Engine B: real VotingLogEngine that consumes b_sub_rx, calls
    // verify_voting_event, and applies on success.
    let log_b = Arc::new(Mutex::new(VotingLog::default()));
    let _engine_b = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&log_b),
        publisher_tx: b_pub_tx,
        subscriber_rx: b_sub_rx,
        hlc_tracker: None,
        device_id: None,
        app_handle: None,
        identity_resolver: Some(id_resolver),
        membership_resolver: Some(mem_resolver),
    })
    .await;

    // ── Mint a Tier 1 PollCreate event and push it via engine A ─────────
    let cfg_poll = Tier1PollConfig {
        options: vec!["a".into(), "b".into()],
        window_seconds: 600,
        quorum: None,
        threshold_percent: None,
        multi_winner: None,
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        channel_id: ChannelId([0xef; 16]),
    };
    let event_hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "peer-a".into(),
    };
    let event = build_signed_poll_create_tier1(&keypair, actor, &cfg_poll, event_hlc)
        .expect("build poll create");

    let mut packet = Vec::new();
    ciborium::ser::into_writer(&event, &mut packet).expect("encode");

    // Push packet through engine A's outbound channel → adapter A →
    // session_a.put → in-memory peer link → session_b subscriber →
    // adapter B → b_sub_rx → engine B's inbound loop → verify+apply.
    a_pub_tx.send(packet).await.expect("push to a_pub_tx");

    // ── Wait for engine B to apply the event (poll up to 3 seconds) ─────
    let sb = event.signing_bytes().expect("signing_bytes");
    let pid = derive_poll_id(&community_id, &sb);

    let mut applied = false;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let log = log_b.lock().await;
        if log.has_poll(&pid) {
            applied = true;
            break;
        }
    }

    assert!(
        applied,
        "engine B must apply the peer event via Zenoh within 3s"
    );

    // ── Clean shutdown ───────────────────────────────────────────────────
    closing_a.store(true, std::sync::atomic::Ordering::SeqCst);
    closing_b.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(_engine_b);
}

/// Craft an encrypted voting envelope for `event` under `space`'s current epoch
/// (+ voting AAD) and `put` it directly on the topic — models a raw publisher /
/// attacker who controls the wire bytes, with no adapter-side crypto in the way.
async fn craft_and_put_voting(
    session: &zenoh::Session,
    topic: &str,
    space: &harmony_app::owner_state_types::Space,
    event: &harmony_app::community_voting_core::SignedVotingEvent,
) {
    let mut plaintext = Vec::new();
    ciborium::ser::into_writer(event, &mut plaintext).expect("encode event");
    let envelope = harmony_app::community_state_sync::encrypt_for_topic_with_aad(
        space,
        &plaintext,
        harmony_app::community_state_sync::VOTING_TOPIC_AAD,
    )
    .expect("encrypt under space epoch");
    let mut wire = Vec::new();
    ciborium::ser::into_writer(&envelope, &mut wire).expect("encode envelope");
    session.put(topic, wire).await.expect("zenoh put");
}

/// ZEB-717 acceptance criterion: a member kicked at the N→N+1 epoch rotation,
/// holding only the stale epoch-N key, cannot inject a voting event — even
/// though the receiver still RETAINS K(N) in `old_epoch_keys`. The transport's
/// current-epoch-only cut drops the stale-epoch envelope before the engine ever
/// sees it. Verify-layer membership can't distinguish this (the injection is a
/// validly-signed member event with a backdated HLC); the transport is the only
/// gate that can, and it must key on the CURRENT epoch, not merely on whether
/// the receiver holds the key.
///
/// Delivery is proven, not assumed: a **readiness barrier** polls until an
/// epoch-1 event actually applies on B (peer link live + B decrypts the current
/// epoch), and a FIFO **sentinel** epoch-1 event is published immediately AFTER
/// the stale one on the same session + key. Zenoh delivers per-publisher in
/// order, so once B applies the sentinel the stale packet was delivered too —
/// its absence then proves the epoch gate DROPPED it, not that it was lost. The
/// sentinel doubles as the current-epoch control. (Readiness discipline over a
/// fixed warm-up sleep — Qodo #504.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kicked_then_rotated_member_injection_is_dropped() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A open"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B open"));

    let community_id = SpaceId([0xac; 16]);
    let community_id_hex = hex::encode(community_id.0);
    let topic = format!("harmony/community/{}/voting", community_id_hex);

    let k0 = EpochKey::new([0xa0; 32]); // epoch-0 key the kicked member still holds
    let k1 = EpochKey::new([0xb1; 32]); // epoch-1 key installed by the rotation

    // Publisher-side epoch snapshots for crafting envelopes directly: the kicked
    // member is stuck at epoch 0 (K0); an honest member publishes at epoch 1 (K1).
    let space_e0 =
        harmony_app::community_state_sync::test_community_space(community_id, 0, k0.clone());
    let space_e1 =
        harmony_app::community_state_sync::test_community_space(community_id, 1, k1.clone());

    // Receiver B: rotated to epoch 1, but RETAINS K0 in old_epoch_keys — the
    // whole point is that key retention alone must NOT admit the stale injection.
    let crdt_state_b = Arc::new(Mutex::new({
        let mut os = harmony_app::owner_state_crdt::OwnerState::default();
        let mut space =
            harmony_app::community_state_sync::test_community_space(community_id, 1, k1.clone());
        space.old_epoch_keys.insert(0, k0.clone());
        os.spaces.insert(community_id, space);
        os
    }));

    // Actor is a valid member in B's snapshot, so verify_voting_event passes for
    // every event — only the transport epoch differs.
    let (keypair, actor, pub_64) = fixture_identity(0xcd);
    let resolvers = Arc::new(FixedResolvers {
        identity: HashMap::from([(actor, pub_64)]),
        snapshot: MembershipSnapshot {
            members: HashMap::from([(
                actor,
                MemberAttrs {
                    power: 1,
                    vouching_depth: 1,
                },
            )]),
        },
    });
    let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
    let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();

    // Only B runs an adapter (the component under test) + engine. A is a raw
    // publisher that crafts envelopes directly via `craft_and_put_voting`.
    let closing_b = Arc::new(AtomicBool::new(false));
    let (b_pub_tx, b_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (b_sub_tx, b_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (bf_read_b, bf_apply_b) = noop_backfill();
    let _adapter_b = spawn_voting_log_zenoh_adapter(
        Arc::clone(&session_b),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state_b),
        b_pub_rx,
        b_sub_tx,
        bf_read_b,
        bf_apply_b,
        NO_BACKFILL_FLOOR,
        None,
        Arc::clone(&closing_b),
    );

    let log_b = Arc::new(Mutex::new(VotingLog::default()));
    let _engine_b = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&log_b),
        publisher_tx: b_pub_tx,
        subscriber_rx: b_sub_rx,
        hlc_tracker: None,
        device_id: None,
        app_handle: None,
        identity_resolver: Some(id_resolver),
        membership_resolver: Some(mem_resolver),
    })
    .await;

    let make_event = |wall_ms: u64, dev: &str| {
        let cfg_poll = Tier1PollConfig {
            options: vec!["a".into(), "b".into()],
            window_seconds: 600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            channel_id: ChannelId([0xef; 16]),
        };
        let hlc = Hlc {
            wall_ms,
            logical: 0,
            device_id: dev.into(),
        };
        build_signed_poll_create_tier1(&keypair, actor, &cfg_poll, hlc).expect("build poll create")
    };

    // ── Readiness barrier: an epoch-1 event MUST apply on B before we test the
    // drop, so a later non-application can only mean "dropped", never "link not
    // up yet". Re-publish each iteration until it lands (waits exactly as long
    // as discovery needs — no fixed warm-up sleep).
    let ready = make_event(1_700_000_000_000, "ready");
    let ready_pid = derive_poll_id(&community_id, &ready.signing_bytes().expect("sb"));
    let mut link_up = false;
    for _ in 0..100 {
        craft_and_put_voting(&session_a, &topic, &space_e1, &ready).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if log_b.lock().await.has_poll(&ready_pid) {
            link_up = true;
            break;
        }
    }
    assert!(
        link_up,
        "peer link never came up / B never applied the epoch-1 readiness event"
    );

    // ── STALE injection under epoch 0 (the kicked member's retained key) ──
    let stale = make_event(1_700_000_001_000, "kicked");
    let stale_pid = derive_poll_id(&community_id, &stale.signing_bytes().expect("sb"));
    craft_and_put_voting(&session_a, &topic, &space_e0, &stale).await;

    // ── SENTINEL under epoch 1, published AFTER the stale on the same session +
    // key. Per-publisher FIFO ⇒ once B applies the sentinel the stale arrived
    // too; the sentinel also serves as the current-epoch control.
    let sentinel = make_event(1_700_000_002_000, "member");
    let sentinel_pid = derive_poll_id(&community_id, &sentinel.signing_bytes().expect("sb"));
    craft_and_put_voting(&session_a, &topic, &space_e1, &sentinel).await;

    let mut sentinel_applied = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if log_b.lock().await.has_poll(&sentinel_pid) {
            sentinel_applied = true;
            break;
        }
    }
    assert!(
        sentinel_applied,
        "epoch-1 sentinel (sent after the stale) must apply on B — current epoch is accepted"
    );
    assert!(
        !log_b.lock().await.has_poll(&stale_pid),
        "stale epoch-0 injection was delivered before the sentinel but must be DROPPED by the \
         current-epoch-only gate — a retained old key must not admit it"
    );

    closing_b.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(_engine_b);
}

// ══════════════════════════════════════════════════════════════════════════
// ZEB-718: voting backfill / pull-on-rejoin acceptance tests.
// ══════════════════════════════════════════════════════════════════════════

/// Build a Tier-1 PollCreate signed by `keypair` for `actor` on `device` at
/// `wall`, plus its derived PollId.
fn signed_tier1_create(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    community_id: SpaceId,
    device: &str,
    wall: u64,
) -> (
    harmony_app::community_voting_core::SignedVotingEvent,
    harmony_app::community_voting_core::PollId,
) {
    let cfg = Tier1PollConfig {
        options: vec!["a".into(), "b".into()],
        window_seconds: 600,
        quorum: None,
        threshold_percent: None,
        multi_winner: None,
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        channel_id: ChannelId([0xef; 16]),
    };
    let hlc = Hlc {
        wall_ms: wall,
        logical: 0,
        device_id: device.into(),
    };
    let ev = build_signed_poll_create_tier1(keypair, actor, &cfg, hlc).expect("build create");
    let sb = ev.signing_bytes().expect("signing_bytes");
    let pid = derive_poll_id(&community_id, &sb);
    (ev, pid)
}

/// One community `crdt_state` at `epoch` with `key` — the wire-crypto source
/// the adapter reads for encrypt/decrypt.
fn crdt_at_epoch(
    community_id: SpaceId,
    epoch: u64,
    key: EpochKey,
) -> Arc<Mutex<harmony_app::owner_state_crdt::OwnerState>> {
    Arc::new(Mutex::new({
        let mut os = harmony_app::owner_state_crdt::OwnerState::default();
        os.spaces.insert(
            community_id,
            harmony_app::community_state_sync::test_community_space(community_id, epoch, key),
        );
        os
    }))
}

/// Spawn a real `VotingLogEngine` + adapter (with the real backfill closures)
/// on `session`. Returns the engine (hold it to keep the node alive) and its
/// shared `VotingLog` (seed / assert through it).
async fn spawn_real_voting_node(
    session: Arc<zenoh::Session>,
    community_id: SpaceId,
    community_id_hex: &str,
    crdt_state: Arc<Mutex<harmony_app::owner_state_crdt::OwnerState>>,
    resolvers: Arc<FixedResolvers>,
    backfill_interval: std::time::Duration,
    closing: Arc<AtomicBool>,
) -> (
    Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    Arc<Mutex<VotingLog>>,
) {
    let log = Arc::new(Mutex::new(VotingLog::default()));
    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
    let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();
    let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&log),
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        hlc_tracker: None,
        device_id: None,
        app_handle: None,
        identity_resolver: Some(id_resolver),
        membership_resolver: Some(mem_resolver),
    })
    .await;
    let (bf_read, bf_apply) =
        harmony_app::community_voting_log_engine::backfill_closures_for_test(&engine);
    // Detached adapter task (tokio::spawn keeps running after the handle
    // drops); it exits when `closing` flips.
    let _adapter = spawn_voting_log_zenoh_adapter(
        Arc::clone(&session),
        community_id_hex.to_string(),
        community_id,
        crdt_state,
        pub_rx,
        sub_tx,
        bf_read,
        bf_apply,
        backfill_interval,
        None,
        closing,
    );
    (engine, log)
}

fn one_member_resolvers(actor: OwnerAddr, pub_64: [u8; 64]) -> Arc<FixedResolvers> {
    Arc::new(FixedResolvers {
        identity: HashMap::from([(actor, pub_64)]),
        snapshot: MembershipSnapshot {
            members: HashMap::from([(
                actor,
                MemberAttrs {
                    power: 1,
                    vouching_depth: 1,
                },
            )]),
        },
    })
}

/// Criterion 1: a peer that missed voting events while offline recovers them
/// on rejoin via the full-dump backfill pull.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfill_recovers_events_missed_while_offline() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("A open"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("B open"));

    let community_id = SpaceId([0x51; 16]);
    let community_id_hex = hex::encode(community_id.0);
    let key = EpochKey::new([0x11; 32]);

    let (keypair, actor, pub_64) = fixture_identity(0x5a);
    let resolvers = one_member_resolvers(actor, pub_64);

    // Node A (source): seed its log with two polls A "created" while B was
    // offline. A only needs to serve them from `log.events`.
    let closing_a = Arc::new(AtomicBool::new(false));
    let (_engine_a, log_a) = spawn_real_voting_node(
        Arc::clone(&session_a),
        community_id,
        &community_id_hex,
        crdt_at_epoch(community_id, 0, key.clone()),
        resolvers.clone(),
        std::time::Duration::from_secs(86_400),
        Arc::clone(&closing_a),
    )
    .await;

    let (e1, pid1) = signed_tier1_create(&keypair, actor, community_id, "a1", 1_700_000_000_001);
    let (e2, pid2) = signed_tier1_create(&keypair, actor, community_id, "a2", 1_700_000_000_002);
    {
        let mut log = log_a.lock().await;
        log.events.push(e1);
        log.events.push(e2);
    }

    // Node B (rejoins): short backfill floor so it re-pulls until A's
    // responder is discoverable.
    let closing_b = Arc::new(AtomicBool::new(false));
    let (_engine_b, log_b) = spawn_real_voting_node(
        Arc::clone(&session_b),
        community_id,
        &community_id_hex,
        crdt_at_epoch(community_id, 0, key),
        resolvers.clone(),
        std::time::Duration::from_millis(300),
        Arc::clone(&closing_b),
    )
    .await;

    let mut recovered = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let log = log_b.lock().await;
        if log.has_poll(&pid1) && log.has_poll(&pid2) {
            recovered = true;
            break;
        }
    }
    assert!(
        recovered,
        "B must recover both missed polls via the backfill pull"
    );

    closing_a.store(true, std::sync::atomic::Ordering::SeqCst);
    closing_b.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Criterion 2: a legitimate vote dropped across an epoch rotation is recovered
/// under the NEW epoch. C (rotated to epoch 1) holds `e1`; B (also at epoch 1)
/// missed it on the live topic (its current-epoch cut dropped the stale epoch-0
/// packet) and recovers it via backfill — C re-encrypts `e1` under epoch 1 at
/// serve time, so it passes B's cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfill_recovers_cross_rotation_dropped_vote_under_new_epoch() {
    let cfg = zenoh::Config::default();
    let session_b = Arc::new(zenoh::open(cfg.clone()).await.expect("B open"));
    let session_c = Arc::new(zenoh::open(cfg).await.expect("C open"));

    let community_id = SpaceId([0x52; 16]);
    let community_id_hex = hex::encode(community_id.0);
    let k1 = EpochKey::new([0xb1; 32]); // the post-rotation (epoch 1) key both hold

    let (keypair, actor, pub_64) = fixture_identity(0x5c);
    let resolvers = one_member_resolvers(actor, pub_64);
    let (e1, pid1) = signed_tier1_create(&keypair, actor, community_id, "src", 1_700_000_000_100);

    // Node C (responder): rotated to epoch 1, holds e1 (received pre-rotation).
    let closing_c = Arc::new(AtomicBool::new(false));
    let (_engine_c, log_c) = spawn_real_voting_node(
        Arc::clone(&session_c),
        community_id,
        &community_id_hex,
        crdt_at_epoch(community_id, 1, k1.clone()),
        resolvers.clone(),
        std::time::Duration::from_secs(86_400),
        Arc::clone(&closing_c),
    )
    .await;
    log_c.lock().await.events.push(e1);

    // Node B (rotated to epoch 1, missing e1): recovers via backfill.
    let closing_b = Arc::new(AtomicBool::new(false));
    let (_engine_b, log_b) = spawn_real_voting_node(
        Arc::clone(&session_b),
        community_id,
        &community_id_hex,
        crdt_at_epoch(community_id, 1, k1),
        resolvers.clone(),
        std::time::Duration::from_millis(300),
        Arc::clone(&closing_b),
    )
    .await;

    let mut recovered = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if log_b.lock().await.has_poll(&pid1) {
            recovered = true;
            break;
        }
    }
    assert!(
        recovered,
        "B must recover the cross-rotation-dropped vote, re-served under the current epoch"
    );

    closing_b.store(true, std::sync::atomic::Ordering::SeqCst);
    closing_c.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Criterion 3: backfill does not weaken the ZEB-717 cut. A kicked-then-rotated
/// identity K (holds only the epoch-0 key) cannot recover current-epoch backfill
/// replies — it lacks K(1), so its decrypt cut drops them. Non-vacuous: a
/// retained member B (at epoch 1) recovers `e1` from the SAME responder M,
/// proving M is serving and the pull path works; K, on the same M, gets nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfill_does_not_weaken_the_cut_for_a_kicked_rotated_identity() {
    let cfg = zenoh::Config::default();
    let session_m = Arc::new(zenoh::open(cfg.clone()).await.expect("M open"));
    let session_b = Arc::new(zenoh::open(cfg.clone()).await.expect("B open"));
    let session_k = Arc::new(zenoh::open(cfg).await.expect("K open"));

    let community_id = SpaceId([0x53; 16]);
    let community_id_hex = hex::encode(community_id.0);
    let k0 = EpochKey::new([0xa0; 32]); // stale epoch-0 key the kicked member still holds
    let k1 = EpochKey::new([0xb1; 32]); // current epoch-1 key retained members hold

    let (keypair, actor, pub_64) = fixture_identity(0x5e);
    let resolvers = one_member_resolvers(actor, pub_64);
    let (e1, pid1) = signed_tier1_create(&keypair, actor, community_id, "src", 1_700_000_000_200);

    // Responder M: epoch 1, holds e1.
    let closing_m = Arc::new(AtomicBool::new(false));
    let (_engine_m, log_m) = spawn_real_voting_node(
        Arc::clone(&session_m),
        community_id,
        &community_id_hex,
        crdt_at_epoch(community_id, 1, k1.clone()),
        resolvers.clone(),
        std::time::Duration::from_secs(86_400),
        Arc::clone(&closing_m),
    )
    .await;
    log_m.lock().await.events.push(e1);

    // Positive control B: epoch 1 → recovers e1 (readiness barrier).
    let closing_b = Arc::new(AtomicBool::new(false));
    let (_engine_b, log_b) = spawn_real_voting_node(
        Arc::clone(&session_b),
        community_id,
        &community_id_hex,
        crdt_at_epoch(community_id, 1, k1),
        resolvers.clone(),
        std::time::Duration::from_millis(300),
        Arc::clone(&closing_b),
    )
    .await;

    // Kicked K: epoch 0 → must recover nothing from the same responder.
    let closing_k = Arc::new(AtomicBool::new(false));
    let (_engine_k, log_k) = spawn_real_voting_node(
        Arc::clone(&session_k),
        community_id,
        &community_id_hex,
        crdt_at_epoch(community_id, 0, k0),
        resolvers.clone(),
        std::time::Duration::from_millis(300),
        Arc::clone(&closing_k),
    )
    .await;

    // Barrier: wait until B has recovered (proves M serves + pull works).
    let mut b_recovered = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if log_b.lock().await.has_poll(&pid1) {
            b_recovered = true;
            break;
        }
    }
    assert!(
        b_recovered,
        "positive control: retained member B must recover e1"
    );

    // Give K ample additional pull cycles, then assert it recovered nothing —
    // its epoch-0 cut drops M's current-epoch replies.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(
        !log_k.lock().await.has_poll(&pid1),
        "kicked-then-rotated K (epoch 0) must NOT recover current-epoch backfill"
    );

    closing_m.store(true, std::sync::atomic::Ordering::SeqCst);
    closing_b.store(true, std::sync::atomic::Ordering::SeqCst);
    closing_k.store(true, std::sync::atomic::Ordering::SeqCst);
}
